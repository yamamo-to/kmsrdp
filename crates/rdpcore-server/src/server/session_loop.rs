use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use rdpcore_cliprdr::{CliprdrBackendFactory, CliprdrChannel};
use rdpcore_connector::{AcceptedConnection, Acceptor, AcceptorEvent, ConnectorError};
use rdpcore_dvc::DvcMux;
use rdpcore_pdu::fastpath::{self, FastPathInput};
use rdpcore_pdu::finalization::{
    DataPdu, MonitorDef, STREAM_UNDEFINED, ShareDataPduType, encode_monitor_layout,
};
use rdpcore_rdpdr::{DriveConsumerFactory, RdpdrChannel};
use rdpcore_rdpeai::{AudioInputBackendFactory, AudioInputHandler};
#[cfg(feature = "gfx")]
use rdpcore_rdpegfx::{GfxSession, select_h264_encoder};
use rdpcore_rdpsnd::{RdpsndChannel, RdpsndServerMessage, SoundServerFactory, wave_channel};
use rdpcore_transport::{ChannelKey, ConnectionWriter, Frame, Priority};
use tokio::io::{AsyncRead, AsyncWrite};
#[cfg(any(feature = "gfx", feature = "dvc-echo"))]
use tracing::info;
use tracing::{debug, warn};

use crate::display::{BitmapUpdate, DesktopSize, DisplayUpdate, RdpServerDisplay};
use crate::encode::{
    bitmap_encode_policy, client_needs_compat_workarounds, retain_bitmap_during_resize,
};
use crate::error::{SessionError, finish_session};
use crate::input::{ConnectionScopedInput, RdpServerInputHandler};
use crate::transport::{SteadyStateFrame, read_steady_state_frame};

#[cfg(feature = "gfx")]
use super::frame_pump::{
    apply_gfx_encode_outcome, build_gfx_frames, send_gfx_frames, try_encode_gfx_frame,
};
use super::frame_pump::{
    encode_and_queue_bitmap, flush_pending_resize_bitmap, send_all_or_timeout,
};
use super::input_handler::dispatch_input_event;
use super::metrics::SessionBitmapMetrics;
use super::slow_path::handle_slow_path_frame;

/// Per-connection dependencies and feature toggles for [`run_steady_state`].
/// One instance is built per accepted connection from the server's shared
/// factories/handles.
pub struct SteadyStateParams {
    /// Display backend this connection reads captured frames from.
    pub display: Arc<dyn RdpServerDisplay>,
    /// Shared input handler; wrapped per-connection so this session's
    /// `reset()` only releases keys/buttons it itself is holding.
    pub input: Arc<Mutex<dyn RdpServerInputHandler>>,
    /// `None` disables RDPSND for this connection (no audio channel).
    pub sound_factory: Option<Arc<dyn SoundServerFactory>>,
    /// `None` disables CLIPRDR (clipboard) for this connection.
    pub cliprdr_factory: Option<Arc<dyn CliprdrBackendFactory>>,
    /// `None` disables RDPEAI (microphone redirection) for this connection.
    pub audio_input_factory: Option<Arc<dyn AudioInputBackendFactory>>,
    /// `None` disables RDPDR drive redirection for this connection.
    pub drive_factory: Option<Arc<dyn DriveConsumerFactory>>,
    #[cfg(feature = "gfx")]
    /// Whether to attempt MS-RDPEGFX/AVC420 for this connection; falls
    /// back to Planar/NSCodec if the client doesn't negotiate GFX.
    pub gfx_enabled: bool,
    #[cfg(feature = "dvc-echo")]
    /// Test-only: exercises the DVC echo channel at connect time.
    pub echo_smoke_test: bool,
}

/// Runs one accepted connection's steady state to completion: dispatches
/// Fast-Path input and virtual-channel PDUs, pumps display updates and
/// audio, and handles resize, until the client disconnects or a fatal
/// error occurs. Returns `Ok(())` for a normal or writer-closed
/// disconnect (see [`crate::error::finish_session`]).
pub async fn run_steady_state<S>(
    params: SteadyStateParams,
    _peer: SocketAddr,
    stream: S,
    mut acceptor: Acceptor,
    accepted: AcceptedConnection,
) -> Result<(), crate::error::ServerError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Per-connection wrapper so reset() releases only this session's
    // holds. Guarantees reset runs on every exit path.
    let connection_input: Arc<Mutex<dyn RdpServerInputHandler>> = Arc::new(Mutex::new(
        ConnectionScopedInput::new(Arc::clone(&params.input)),
    ));
    let _reset_input_on_drop = ResetInputOnDrop(Arc::clone(&connection_input));

    let (mut read_half, write_half) = tokio::io::split(stream);
    let (writer, frame_sender) = ConnectionWriter::new(write_half);
    // Detached: it keeps running/flushing until every `FrameSender`
    // clone for this connection is dropped, which happens naturally
    // when this function returns.
    tokio::spawn(writer.run());

    let mut updates = params.display.updates().await?;

    let rdpsnd_channel_id = accepted
        .static_channels
        .iter()
        .find(|(name, _)| name == rdpcore_rdpsnd::pdu::CHANNEL_NAME)
        .map(|(_, id)| *id);
    let mut rdpsnd_audio_rx = None;
    let rdpsnd = match (rdpsnd_channel_id, &params.sound_factory) {
        (Some(channel_id), Some(factory)) => {
            let (tx, rx) = wave_channel();
            let (channel, initial) = RdpsndChannel::new(
                channel_id,
                accepted.user_channel_id,
                factory.build_backend(tx),
            );
            for bytes in initial {
                let _ = frame_sender.send(Frame {
                    channel: ChannelKey::Static(channel_id),
                    priority: Priority::Latency,
                    bytes,
                });
            }
            rdpsnd_audio_rx = Some(rx);
            Some(Arc::new(tokio::sync::Mutex::new(channel)))
        }
        (Some(_channel_id), None) => None,
        _ => None,
    };

    // Wave chunks are pumped by a dedicated task rather than the
    // steady-state loop below: GFX/bitmap *encode* there is necessarily
    // awaited on this task (see `try_encode_gfx_frame`'s doc comment),
    // but the subsequent `send_all` of a full-screen burst is spawned so
    // WaveConfirm can still be read. Locking `rdpsnd` from here never
    // contends with the loop below - the loop only ever touches it for
    // `on_channel_data`, briefly and never across an await.
    let _audio_task = match (rdpsnd.clone(), rdpsnd_audio_rx.take()) {
        (Some(channel), Some(mut audio_rx)) => {
            let sender = frame_sender.clone();
            Some(AbortOnDrop(tokio::spawn(async move {
                let mut diag = PlayQueueDiag::default();
                while let Some(RdpsndServerMessage::Wave(pcm, timestamp_ms)) = audio_rx.recv().await
                {
                    let mut channel = channel.lock().await;
                    // Catch rather than let a panic here silently kill
                    // this task - nothing awaits its JoinHandle (only
                    // AbortOnDrop's Drop, which just aborts), so audio
                    // would otherwise stop dead with no log line at all.
                    if let Err(panic) =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            send_wave_frames(&mut channel, &sender, pcm, timestamp_ms, &mut diag);
                        }))
                    {
                        warn!("rdpsnd: audio task panicked in send_wave_frames: {panic:?}");
                    }
                }
                debug!("rdpsnd: audio task ending (wave sender dropped)");
            })))
        }
        _ => None,
    };

    let cliprdr_channel_id = accepted
        .static_channels
        .iter()
        .find(|(name, _)| name == rdpcore_cliprdr::pdu::CHANNEL_NAME)
        .map(|(_, id)| *id);
    let mut cliprdr_event_rx = None;
    let mut cliprdr = match (cliprdr_channel_id, &params.cliprdr_factory) {
        (Some(channel_id), Some(factory)) => {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let (channel, initial) = CliprdrChannel::new(
                channel_id,
                accepted.user_channel_id,
                factory.build_cliprdr_backend(tx),
            );
            for bytes in initial {
                let _ = frame_sender.send(Frame {
                    channel: ChannelKey::Static(channel_id),
                    priority: Priority::Bulk,
                    bytes,
                });
            }
            cliprdr_event_rx = Some(rx);
            Some(channel)
        }
        (Some(_channel_id), None) => None,
        _ => None,
    };

    let drdynvc_channel_id = accepted
        .static_channels
        .iter()
        .find(|(name, _)| name == rdpcore_dvc::pdu::CHANNEL_NAME)
        .map(|(_, id)| *id);
    let mut dvc = drdynvc_channel_id.map(|channel_id| {
        let (mut mux, initial) = DvcMux::new(channel_id, accepted.user_channel_id);
        for bytes in initial {
            let _ = frame_sender.send(Frame {
                channel: ChannelKey::Static(channel_id),
                priority: Priority::Latency,
                bytes,
            });
        }
        #[cfg(feature = "dvc-echo")]
        if params.echo_smoke_test {
            let echo_frames = mux.register_channel(Box::new(rdpcore_dvc::echo::EchoHandler::new(
                b"kmsrdp-dvc-smoketest".to_vec(),
                |matched| {
                    if matched {
                        info!("DVC echo smoke test: OK, payload round-tripped correctly");
                    } else {
                        warn!("DVC echo smoke test: FAILED, echoed payload did not match");
                    }
                },
            )));
            info!(
                "DVC echo smoke test: queued {} follow-up frame(s)",
                echo_frames.len()
            );
            for bytes in echo_frames {
                let _ = frame_sender.send(Frame {
                    channel: ChannelKey::Static(channel_id),
                    priority: Priority::Latency,
                    bytes,
                });
            }
        }
        if let Some(factory) = &params.audio_input_factory {
            let audio_input_frames =
                mux.register_channel(Box::new(AudioInputHandler::new(factory.build_backend())));
            for bytes in audio_input_frames {
                let _ = frame_sender.send(Frame {
                    channel: ChannelKey::Static(channel_id),
                    priority: Priority::Latency,
                    bytes,
                });
            }
        }
        mux
    });

    #[cfg(feature = "gfx")]
    let gfx_session = if params.gfx_enabled {
        match select_h264_encoder() {
            Ok(selected) => {
                let session = GfxSession::new(
                    selected.encoder,
                    accepted.desktop_width,
                    accepted.desktop_height,
                );
                if let Some(mux) = dvc.as_mut() {
                    let frames = mux.register_channel(Box::new(session.dvc_handler()));
                    for bytes in frames {
                        let _ = frame_sender.send(Frame {
                            channel: ChannelKey::Static(mux.channel_id()),
                            priority: Priority::Bulk,
                            bytes,
                        });
                    }
                    info!(encoder = selected.name, "GFX AVC420 channel registered");
                }
                Some(session)
            }
            Err(e) => {
                warn!("GFX encoder unavailable ({e}); using Planar/NSCodec");
                None
            }
        }
    } else {
        info!("GFX disabled; using Planar/NSCodec");
        None
    };

    let rdpdr_channel_id = accepted
        .static_channels
        .iter()
        .find(|(name, _)| name == rdpcore_rdpdr::pdu::CHANNEL_NAME)
        .map(|(_, id)| *id);
    let mut rdpdr_wake_rx = None;
    let mut rdpdr = match (rdpdr_channel_id, &params.drive_factory) {
        (Some(channel_id), Some(factory)) => {
            let (wake_tx, wake_rx) = tokio::sync::mpsc::unbounded_channel();
            let (channel, initial) = RdpdrChannel::new(
                channel_id,
                accepted.user_channel_id,
                factory.supported_device_types(),
                factory.build_drive_consumer(wake_tx),
            );
            for bytes in initial {
                let _ = frame_sender.send(Frame {
                    channel: ChannelKey::Static(channel_id),
                    priority: Priority::Latency,
                    bytes,
                });
            }
            rdpdr_wake_rx = Some(wake_rx);
            Some(channel)
        }
        (Some(_channel_id), None) => None,
        _ => None,
    };

    let client_label = trim_client_name(&accepted.client_name);
    let server_mfu = 8 * 1024 * 1024u32;
    let max_request_size = accepted
        .max_request_size
        .unwrap_or(server_mfu)
        .min(server_mfu)
        .max(fastpath::MAX_FASTPATH_CHUNK_SIZE as u32);
    let bitmap_policy =
        bitmap_encode_policy(client_label, accepted.nscodec, max_request_size as usize);
    let defer_ms = initial_bitmap_defer_ms(client_label, bitmap_policy.nscodec.is_some());
    let mut metrics = SessionBitmapMetrics::default();
    let mut bitmap_gate_open = defer_ms == 0;
    let mut bitmap_gate = Box::pin(tokio::time::sleep(std::time::Duration::from_millis(
        defer_ms,
    )));
    let mut deferred_bitmap: Option<BitmapUpdate> = None;
    let mut display_updates_allowed = true;
    let mut frame_id = 1u32;
    let io_channel_id = accepted.io_channel_id;

    // Advertise host monitor rectangles when the virtual desktop spans
    // more than one CRTC (clients may ignore this).
    let monitors = params.display.monitor_layout();
    if monitors.len() > 1 {
        let defs: Vec<MonitorDef> = monitors
            .iter()
            .map(|m| MonitorDef {
                left: m.left,
                top: m.top,
                right: m.right,
                bottom: m.bottom,
                primary: m.primary,
            })
            .collect();
        let body = DataPdu {
            share_id: accepted.share_id,
            pdu_source: io_channel_id,
            stream_id: STREAM_UNDEFINED,
            pdu_type2: ShareDataPduType::MonitorLayout,
            body: encode_monitor_layout(&defs),
        }
        .encode();
        let bytes = rdpcore_pdu::x224::wrap_data(
            &rdpcore_pdu::mcs::SendData {
                initiator: accepted.user_channel_id,
                channel_id: io_channel_id,
                data: body,
                complete: true,
            }
            .encode_indication(),
        );
        let _ = frame_sender.send(Frame {
            channel: ChannelKey::Io,
            priority: Priority::Latency,
            bytes,
        });
    }

    // Ensure client cursor is synchronized to default pointer on initial connect.
    let default_ptr = rdpcore_pdu::pointer::encode_ptr_default();
    let _ = frame_sender.send(Frame {
        channel: ChannelKey::Io,
        priority: Priority::Latency,
        bytes: default_ptr,
    });

    let mut resizing = false;
    let mut resize_desktop = DesktopSize {
        width: accepted.desktop_width,
        height: accepted.desktop_height,
    };
    let mut pending_after_resize: Option<BitmapUpdate> = None;
    #[cfg(feature = "gfx")]
    let mut last_gfx_data: Option<std::sync::Arc<[u8]>> = None;
    // In-flight bulk graphics `send_all`. Kept off this select! so
    // WaveConfirm is still read during the first full-screen refresh
    // (otherwise the 80 ms unacked window fills and mstsc's FIFO holds
    // those samples until confirms arrive, which looks like ~1 s of
    // A/V offset from connect). Abort on session end so the spawned
    // task cannot keep the writer alive after we return.
    let mut bulk_send = AbortHandleOnDrop::default();

    loop {
        let mut bitmap_to_pump: Option<BitmapUpdate> = None;
        tokio::select! {
            biased;
            frame = read_steady_state_frame(&mut read_half) => {
                match frame {
                    Err(e) => return Err(e.into()),
                    Ok(SteadyStateFrame::FastPathInput(bytes)) => {
                        match FastPathInput::decode(&bytes) {
                            Ok(input_pdu) => {
                                let mut input =
                                    connection_input.lock().unwrap_or_else(|e| e.into_inner());
                                for event in input_pdu.events {
                                    dispatch_input_event(&mut *input, event);
                                }
                            }
                            Err(e) => debug!("dropping malformed fast-path input frame: {e}"),
                        }
                    }
                    Ok(SteadyStateFrame::SlowPath(bytes)) if resizing => {
                        if acceptor.is_finished() {
                            resizing = false;
                            if let Err(e) = flush_pending_resize_bitmap(
                                &mut pending_after_resize,
                                &frame_sender,
                                &bitmap_policy,
                                &mut frame_id,
                                display_updates_allowed,
                                &mut metrics,
                            )
                            .await
                            {
                                return finish_session(Err(e));
                            }
                            if let Err(e) = handle_slow_path_frame(
                                &bytes,
                                io_channel_id,
                                &mut display_updates_allowed,
                                updates.as_mut(),
                                &rdpsnd,
                                cliprdr.as_mut(),
                                dvc.as_mut(),
                                rdpdr.as_mut(),
                                &frame_sender,
                                &bitmap_policy,
                                &mut frame_id,
                                &mut metrics,
                            )
                            .await
                            {
                                debug!("dropping malformed slow-path frame after resize: {e}");
                            }
                            continue;
                        }
                        match acceptor.step(&bytes) {
                            Ok(result) => {
                                if !result.response.is_empty()
                                    && send_all_or_timeout(
                                        &frame_sender,
                                        Frame { channel: ChannelKey::Io, priority: Priority::Latency, bytes: result.response },
                                    )
                                    .await
                                    .is_err()
                                {
                                    return finish_session(Err(SessionError::WriterClosed));
                                }
                                if acceptor.is_finished()
                                    || matches!(result.event, AcceptorEvent::Accepted(_))
                                {
                                    resizing = false;
                                    if let Err(e) = flush_pending_resize_bitmap(
                                        &mut pending_after_resize,
                                        &frame_sender,
                                        &bitmap_policy,
                                        &mut frame_id,
                                        display_updates_allowed,
                                        &mut metrics,
                                    )
                                    .await
                                    {
                                        return finish_session(Err(e));
                                    }
                                }
                            }
                            Err(e) => {
                                if acceptor.is_finished()
                                    || matches!(e, ConnectorError::AlreadyFinished)
                                {
                                    resizing = false;
                                    if let Err(e) = flush_pending_resize_bitmap(
                                        &mut pending_after_resize,
                                        &frame_sender,
                                        &bitmap_policy,
                                        &mut frame_id,
                                        display_updates_allowed,
                                        &mut metrics,
                                    )
                                    .await
                                    {
                                        return finish_session(Err(e));
                                    }
                                    if let Err(err) = handle_slow_path_frame(
                                        &bytes,
                                        io_channel_id,
                                        &mut display_updates_allowed,
                                        updates.as_mut(),
                                        &rdpsnd,
                                        cliprdr.as_mut(),
                                        dvc.as_mut(),
                                        rdpdr.as_mut(),
                                        &frame_sender,
                                        &bitmap_policy,
                                        &mut frame_id,
                                        &mut metrics,
                                    )
                                    .await
                                    {
                                        debug!(
                                            "dropping malformed slow-path frame after resize: {err}"
                                        );
                                    }
                                } else {
                                    debug!("dropping malformed frame during resize: {e}");
                                }
                            }
                        }
                    }
                    Ok(SteadyStateFrame::SlowPath(bytes)) => {
                        if let Err(e) = handle_slow_path_frame(
                            &bytes,
                            io_channel_id,
                            &mut display_updates_allowed,
                            updates.as_mut(),
                            &rdpsnd,
                            cliprdr.as_mut(),
                            dvc.as_mut(),
                            rdpdr.as_mut(),
                            &frame_sender,
                            &bitmap_policy,
                            &mut frame_id,
                            &mut metrics,
                        )
                        .await
                        {
                            debug!("dropping malformed slow-path frame: {e}");
                        }
                    }
                }
            }
            join = async { bulk_send.0.as_mut().unwrap().await }, if bulk_send.0.is_some() => {
                bulk_send.0 = None;
                match join {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => return finish_session(Err(e)),
                    Err(_) => return finish_session(Err(SessionError::EncodeJoin)),
                }
                if display_updates_allowed && bitmap_gate_open {
                    bitmap_to_pump = deferred_bitmap.take();
                }
            }
            _ = &mut bitmap_gate, if !bitmap_gate_open => {
                bitmap_gate_open = true;
                if display_updates_allowed {
                    bitmap_to_pump = deferred_bitmap.take();
                }
            }
            update = updates.next_update() => {
                match update {
                    Err(e) => return Err(crate::error::ServerError::Display(e)),
                    Ok(Some(DisplayUpdate::Bitmap(bitmap))) if resizing => {
                        retain_bitmap_during_resize(
                            &mut pending_after_resize,
                            bitmap,
                            resize_desktop.width,
                            resize_desktop.height,
                        );
                    }
                    Ok(Some(DisplayUpdate::Bitmap(bitmap))) if !bitmap_gate_open => {
                        deferred_bitmap = Some(bitmap);
                    }
                    Ok(Some(DisplayUpdate::Bitmap(_))) if !display_updates_allowed => {}
                    Ok(Some(DisplayUpdate::Bitmap(bitmap))) => {
                        bitmap_to_pump = Some(bitmap);
                    }
                    Ok(Some(DisplayUpdate::Resized(size))) if resizing => {
                        debug!("dropping resize to {}x{}: a previous resize is still in flight", size.width, size.height);
                    }
                    Ok(Some(DisplayUpdate::Resized(size))) => {
                        #[cfg(feature = "gfx")]
                        let resize_gfx_frames = if let (Some(gfx), Some(mux)) =
                            (gfx_session.as_ref(), dvc.as_ref())
                            && let Some(payloads) = gfx.resize(size.width, size.height)
                        {
                            last_gfx_data = None;
                            build_gfx_frames(mux, payloads).ok()
                        } else {
                            None
                        };
                        #[cfg(feature = "gfx")]
                        if let Some(frames) = resize_gfx_frames {
                            let _ = send_gfx_frames(&frame_sender, frames).await;
                        }
                        match acceptor.begin_resize(size.width, size.height) {
                            Ok(response) => {
                                resizing = true;
                                resize_desktop = size;
                                pending_after_resize = None;
                                deferred_bitmap = None;
                                if frame_sender.send(Frame { channel: ChannelKey::Io, priority: Priority::Latency, bytes: response }).is_err() {
                                    return finish_session(Err(SessionError::WriterClosed));
                                }
                            }
                            Err(e) => warn!("failed to start resize to {}x{}: {e}", size.width, size.height),
                        }
                    }
                    Ok(None) => {
                        metrics.log("display_ended");
                        return Ok(());
                    }
                }
            }
            clipboard_event = recv_optional(&mut cliprdr_event_rx) => {
                let Some(event) = clipboard_event else { continue };
                if let Some(channel) = cliprdr.as_mut() {
                    let channel_id = channel.channel_id();
                    let outgoing = channel.encode_message(event);
                    for bytes in outgoing {
                        let _ = send_all_or_timeout(
                            &frame_sender,
                            Frame { channel: ChannelKey::Static(channel_id), priority: Priority::Bulk, bytes },
                        )
                        .await;
                    }
                }
            }
            _ = recv_optional(&mut rdpdr_wake_rx) => {
                if let Some(channel) = rdpdr.as_mut() {
                    let channel_id = channel.channel_id();
                    let outgoing = channel.flush_pending_commands();
                    for bytes in outgoing {
                        if send_all_or_timeout(
                            &frame_sender,
                            Frame {
                                channel: ChannelKey::Static(channel_id),
                                priority: Priority::Latency,
                                bytes,
                            },
                        )
                        .await
                        .is_err()
                        {
                            return finish_session(Err(SessionError::WriterClosed));
                        }
                    }
                }
            }
        }

        if let Some(bitmap) = bitmap_to_pump {
            if bulk_send.0.is_some() {
                deferred_bitmap = Some(bitmap);
                continue;
            }
            let full = updates.latest_full_frame();
            #[cfg(feature = "gfx")]
            let (gfx_handled, gfx_frames) = match try_encode_gfx_frame(
                gfx_session.as_ref(),
                &mut last_gfx_data,
                full.as_ref(),
                &bitmap,
            )
            .await
            {
                Ok(outcome) => match apply_gfx_encode_outcome(outcome, dvc.as_ref()) {
                    Ok(pair) => pair,
                    Err(e) => return finish_session(Err(e)),
                },
                Err(e) => return finish_session(Err(e)),
            };
            #[cfg(not(feature = "gfx"))]
            let (gfx_handled, gfx_frames) = {
                let _ = full;
                (false, Vec::new())
            };
            if let Err(e) = encode_and_queue_bitmap(
                bitmap,
                &frame_sender,
                &bitmap_policy,
                &mut frame_id,
                &mut metrics,
                &mut bulk_send.0,
                &mut deferred_bitmap,
                gfx_frames,
                gfx_handled,
            )
            .await
            {
                return finish_session(Err(e));
            }
        }
    }
}

fn trim_client_name(name: &str) -> &str {
    name.trim_end_matches('\0').trim()
}

fn initial_bitmap_defer_ms(client_name: &str, using_nscodec: bool) -> u64 {
    if using_nscodec || client_needs_compat_workarounds(client_name) {
        400
    } else {
        0
    }
}

async fn recv_optional<T>(rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match rx {
        Some(r) => {
            let msg = r.recv().await;
            if msg.is_none() {
                *rx = None;
            }
            msg
        }
        None => std::future::pending().await,
    }
}

/// Aborts the wrapped task when dropped, instead of letting it run
/// detached forever after this connection ends.
pub struct AbortOnDrop(pub tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Same as [`AbortOnDrop`], but for a bulk-graphics send that the session
/// loop also awaits. Dropping the handle without abort would detach the
/// task and keep the connection writer alive after the session returns.
#[derive(Default)]
struct AbortHandleOnDrop(Option<tokio::task::JoinHandle<Result<(), SessionError>>>);

impl Drop for AbortHandleOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

/// Calls `reset()` on the wrapped input handler when dropped, so a
/// connection that ends (normally, on error, or via panic) always
/// releases whatever keys/buttons it was holding.
pub struct ResetInputOnDrop(pub Arc<Mutex<dyn RdpServerInputHandler>>);

impl Drop for ResetInputOnDrop {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).reset();
    }
}

fn send_wave_frames(
    channel: &mut RdpsndChannel,
    frame_sender: &rdpcore_transport::FrameSender,
    pcm: Vec<u8>,
    timestamp_ms: u32,
    diag: &mut PlayQueueDiag,
) {
    let channel_id = channel.channel_id();
    let encoded = channel.encode_wave(pcm, timestamp_ms);
    let stats = channel.play_queue_stats();
    if encoded.is_empty() {
        diag.note_skip(&stats);
        return;
    }
    let frames = encoded
        .into_iter()
        .map(|bytes| Frame {
            channel: ChannelKey::Static(channel_id),
            priority: Priority::Latency,
            bytes,
        })
        .collect();
    let _ = frame_sender.send_live(frames);
    diag.note_send(&stats);
}

struct PlayQueueDiag {
    first_send: Option<std::time::Instant>,
    logged_first_confirm: bool,
    last_log: std::time::Instant,
    sent: u32,
    skipped: u32,
}

impl Default for PlayQueueDiag {
    fn default() -> Self {
        Self {
            first_send: None,
            logged_first_confirm: false,
            last_log: std::time::Instant::now(),
            sent: 0,
            skipped: 0,
        }
    }
}

impl PlayQueueDiag {
    fn note_skip(&mut self, stats: &rdpcore_rdpsnd::PlayQueueStats) {
        self.skipped = self.skipped.saturating_add(1);
        self.maybe_log(stats);
    }

    fn note_send(&mut self, stats: &rdpcore_rdpsnd::PlayQueueStats) {
        if self.first_send.is_none() {
            self.first_send = Some(std::time::Instant::now());
        }
        self.sent = self.sent.saturating_add(1);
        self.maybe_log(stats);
    }

    fn maybe_log(&mut self, stats: &rdpcore_rdpsnd::PlayQueueStats) {
        if !self.logged_first_confirm
            && let Some(rtt_ms) = stats.last_confirm_rtt_ms
        {
            self.logged_first_confirm = true;
            let wait_ms = self
                .first_send
                .map(|t| t.elapsed().as_millis())
                .unwrap_or(0);
            debug!(
                wait_ms,
                rtt_ms,
                pending_blocks = stats.pending_blocks,
                "rdpsnd: first WaveConfirm (wait_ms ≈ client preroll; rtt_ms ≈ play-queue depth)"
            );
        }
        if self.last_log.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        debug!(
            sent = self.sent,
            skipped = self.skipped,
            pending_blocks = stats.pending_blocks,
            last_confirm_rtt_ms = stats.last_confirm_rtt_ms,
            best_confirm_rtt_ms = stats.best_confirm_rtt_ms,
            ready = stats.ready,
            rtt_behind = stats.rtt_behind,
            receive_ack_count = stats.receive_ack_count,
            estimated_hold_ms = stats.estimated_hold_ms,
            "rdpsnd: play-queue (estimated_hold_ms is measured FIFO; last_confirm_rtt_ms is the latest ack and may be a 0 ms receive-ack)"
        );
        self.last_log = std::time::Instant::now();
        self.sent = 0;
        self.skipped = 0;
    }
}
