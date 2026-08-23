use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use rdpcore_cliprdr::{CliprdrBackendFactory, CliprdrChannel};
use rdpcore_connector::{AcceptedConnection, Acceptor, AcceptorEvent, ConnectorError};
use rdpcore_dvc::DvcMux;
use rdpcore_pdu::fastpath::{self, FastPathInput};
use rdpcore_pdu::finalization::{
    DataPdu, MonitorDef, STREAM_UNDEFINED, ShareDataPduType, decode_refresh_rect,
    decode_suppress_output, encode_monitor_layout,
};
use rdpcore_rdpdr::{DriveConsumerFactory, RdpdrChannel};
use rdpcore_rdpeai::{AudioInputBackendFactory, AudioInputHandler};
#[cfg(feature = "gfx")]
use rdpcore_rdpegfx::{GfxSession, select_h264_encoder};
use rdpcore_rdpsnd::{RdpsndChannel, RdpsndServerMessage, SoundServerFactory, wave_channel};
use rdpcore_transport::{ChannelKey, ConnectionWriter, Frame, Priority};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, warn};
#[cfg(any(feature = "gfx", feature = "dvc-echo"))]
use tracing::info;

use crate::display::{BitmapUpdate, DesktopSize, DisplayUpdate, RdpServerDisplay};
use crate::encode::{
    BitmapEncodePolicy, bitmap_encode_policy, client_needs_compat_workarounds,
    retain_bitmap_during_resize,
};
use crate::error::{SessionError, finish_session};
use crate::input::{ConnectionScopedInput, RdpServerInputHandler};
use crate::transport::{SteadyStateFrame, read_steady_state_frame};

use super::frame_pump::{
    flush_pending_resize_bitmap, send_outbound_bitmap, send_outbound_frame,
};
#[cfg(feature = "gfx")]
use super::frame_pump::{apply_gfx_encode_outcome, send_gfx_payloads, try_encode_gfx_frame};
use super::input_handler::dispatch_input_event;
use super::metrics::SessionBitmapMetrics;

pub struct SteadyStateParams {
    pub display: Arc<dyn RdpServerDisplay>,
    pub input: Arc<Mutex<dyn RdpServerInputHandler>>,
    pub sound_factory: Option<Arc<dyn SoundServerFactory>>,
    pub cliprdr_factory: Option<Arc<dyn CliprdrBackendFactory>>,
    pub audio_input_factory: Option<Arc<dyn AudioInputBackendFactory>>,
    pub drive_factory: Option<Arc<dyn DriveConsumerFactory>>,
    #[cfg(feature = "gfx")]
    pub gfx_enabled: bool,
    #[cfg(feature = "dvc-echo")]
    pub echo_smoke_test: bool,
}

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
    // steady-state loop below: GFX/bitmap encode there is necessarily
    // synchronous (see `try_encode_gfx_frame`'s doc comment), and a
    // select!-branch-only audio path would stall for the full encode
    // duration every time one runs. Locking `rdpsnd` from here never
    // contends with the loop below - the loop only ever touches it for
    // `on_channel_data`, briefly and never across an await.
    let _audio_task = match (rdpsnd.clone(), rdpsnd_audio_rx.take()) {
        (Some(channel), Some(mut audio_rx)) => {
            let sender = frame_sender.clone();
            Some(AbortOnDrop(tokio::spawn(async move {
                while let Some(RdpsndServerMessage::Wave(pcm, timestamp_ms)) =
                    audio_rx.recv().await
                {
                    let mut channel = channel.lock().await;
                    // Catch rather than let a panic here silently kill
                    // this task - nothing awaits its JoinHandle (only
                    // AbortOnDrop's Drop, which just aborts), so audio
                    // would otherwise stop dead with no log line at all.
                    if let Err(panic) =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            send_wave_frames(&mut channel, &sender, pcm, timestamp_ms);
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
            let echo_frames =
                mux.register_channel(Box::new(rdpcore_dvc::echo::EchoHandler::new(
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

    loop {
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
                                    && frame_sender
                                        .send(Frame { channel: ChannelKey::Io, priority: Priority::Latency, bytes: result.response })
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
            _ = &mut bitmap_gate, if !bitmap_gate_open => {
                bitmap_gate_open = true;
                if display_updates_allowed
                    && let Some(bitmap) = deferred_bitmap.take()
                {
                    let full = updates.latest_full_frame();
                    #[cfg(feature = "gfx")]
                    let gfx_attempt = Some(
                        match try_encode_gfx_frame(
                            gfx_session.as_ref(),
                            &mut last_gfx_data,
                            full.as_ref(),
                            &bitmap,
                        )
                        .await
                        {
                            Ok(outcome) => {
                                apply_gfx_encode_outcome(outcome, dvc.as_ref(), &frame_sender)
                            }
                            Err(e) => Err(e),
                        },
                    );
                    if let Err(e) = send_outbound_frame(
                        &bitmap,
                        &frame_sender,
                        &bitmap_policy,
                        &mut frame_id,
                        full.as_ref(),
                        &mut metrics,
                        #[cfg(feature = "gfx")]
                        gfx_attempt,
                    )
                    .await
                    {
                        return finish_session(Err(e));
                    }
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
                        let full = updates.latest_full_frame();
                        #[cfg(feature = "gfx")]
                        let gfx_attempt = Some(
                            match try_encode_gfx_frame(
                                gfx_session.as_ref(),
                                &mut last_gfx_data,
                                full.as_ref(),
                                &bitmap,
                            )
                            .await
                            {
                                Ok(outcome) => apply_gfx_encode_outcome(
                                    outcome,
                                    dvc.as_ref(),
                                    &frame_sender,
                                ),
                                Err(e) => Err(e),
                            },
                        );
                        if let Err(e) = send_outbound_frame(
                            &bitmap,
                            &frame_sender,
                            &bitmap_policy,
                            &mut frame_id,
                            full.as_ref(),
                            &mut metrics,
                            #[cfg(feature = "gfx")]
                            gfx_attempt,
                        )
                        .await
                        {
                            return finish_session(Err(e));
                        }
                    }
                    Ok(Some(DisplayUpdate::Resized(size))) if resizing => {
                        debug!("dropping resize to {}x{}: a previous resize is still in flight", size.width, size.height);
                    }
                    Ok(Some(DisplayUpdate::Resized(size))) => {
                        #[cfg(feature = "gfx")]
                        if let (Some(gfx), Some(mux)) = (gfx_session.as_ref(), dvc.as_ref())
                            && let Some(payloads) = gfx.resize(size.width, size.height)
                        {
                            let _ = send_gfx_payloads(mux, &frame_sender, payloads);
                            last_gfx_data = None;
                        }
                        match acceptor.begin_resize(size.width, size.height) {
                            Ok(response) => {
                                resizing = true;
                                resize_desktop = size;
                                pending_after_resize = None;
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
                    for bytes in channel.encode_message(event) {
                        let _ = frame_sender.send(Frame { channel: ChannelKey::Static(channel_id), priority: Priority::Bulk, bytes });
                    }
                }
            }
            _ = recv_optional(&mut rdpdr_wake_rx) => {
                if let Some(channel) = rdpdr.as_mut() {
                    let channel_id = channel.channel_id();
                    for bytes in channel.flush_pending_commands() {
                        if frame_sender
                            .send(Frame {
                                channel: ChannelKey::Static(channel_id),
                                priority: Priority::Latency,
                                bytes,
                            })
                            .is_err()
                        {
                            return finish_session(Err(SessionError::WriterClosed));
                        }
                    }
                }
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

pub struct AbortOnDrop(pub tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

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
) {
    let channel_id = channel.channel_id();
    for bytes in channel.encode_wave(pcm, timestamp_ms) {
        let _ = frame_sender.send(Frame {
            channel: ChannelKey::Static(channel_id),
            priority: Priority::Latency,
            bytes,
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_slow_path_frame(
    bytes: &[u8],
    io_channel_id: u16,
    display_updates_allowed: &mut bool,
    updates: &mut dyn crate::display::RdpServerDisplayUpdates,
    rdpsnd: &Option<Arc<tokio::sync::Mutex<RdpsndChannel>>>,
    cliprdr: Option<&mut CliprdrChannel>,
    dvc: Option<&mut DvcMux>,
    rdpdr: Option<&mut RdpdrChannel>,
    frame_sender: &rdpcore_transport::FrameSender,
    policy: &BitmapEncodePolicy,
    frame_id: &mut u32,
    metrics: &mut SessionBitmapMetrics,
) -> Result<(), SessionError> {
    let payload = rdpcore_pdu::x224::unwrap_data(bytes)?;
    let send_data = rdpcore_pdu::mcs::SendData::decode_request(payload)?;

    if send_data.channel_id == io_channel_id {
        if let Ok(data_pdu) = DataPdu::decode(&send_data.data) {
            match data_pdu.pdu_type2 {
                ShareDataPduType::SuppressOutput => {
                    if let Ok(allow) = decode_suppress_output(&data_pdu.body) {
                        let was = *display_updates_allowed;
                        *display_updates_allowed = allow;
                        if allow
                            && !was
                            && let Some(full) = updates.latest_full_frame()
                        {
                            let _ = send_outbound_bitmap(
                                &full,
                                frame_sender,
                                policy,
                                frame_id,
                                metrics,
                            )
                            .await;
                        }
                    }
                }
                ShareDataPduType::RefreshRect => {
                    if let Ok(rects) = decode_refresh_rect(&data_pdu.body)
                        && let Some(full) = updates.latest_full_frame()
                    {
                        if rects.is_empty() {
                            let _ = send_outbound_bitmap(
                                &full,
                                frame_sender,
                                policy,
                                frame_id,
                                metrics,
                            )
                            .await;
                        } else {
                            for rect in rects {
                                let w = rect.right.saturating_sub(rect.left).saturating_add(1);
                                let h = rect.bottom.saturating_sub(rect.top).saturating_add(1);
                                let (Some(nw), Some(nh)) =
                                    (core::num::NonZeroU16::new(w), core::num::NonZeroU16::new(h))
                                else {
                                    continue;
                                };
                                if let Some(sub) = full.sub(rect.left, rect.top, nw, nh) {
                                    let _ = send_outbound_bitmap(
                                        &sub,
                                        frame_sender,
                                        policy,
                                        frame_id,
                                        metrics,
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        return Ok(());
    }

    if let Some(channel) = rdpsnd {
        let mut channel = channel.lock().await;
        if send_data.channel_id == channel.channel_id() {
            let channel_id = channel.channel_id();
            for response in channel.on_channel_data(&send_data.data)? {
                let _ = frame_sender.send(Frame {
                    channel: ChannelKey::Static(channel_id),
                    priority: Priority::Latency,
                    bytes: response,
                });
            }
            return Ok(());
        }
    }
    if let Some(channel) = cliprdr
        && send_data.channel_id == channel.channel_id()
    {
        let channel_id = channel.channel_id();
        for response in channel.on_channel_data(&send_data.data)? {
            let _ = frame_sender.send(Frame {
                channel: ChannelKey::Static(channel_id),
                priority: Priority::Bulk,
                bytes: response,
            });
        }
        return Ok(());
    }
    if let Some(mux) = dvc
        && send_data.channel_id == mux.channel_id()
    {
        let channel_id = mux.channel_id();
        for response in mux.on_channel_data(&send_data.data)? {
            let _ = frame_sender.send(Frame {
                channel: ChannelKey::Static(channel_id),
                priority: Priority::Latency,
                bytes: response,
            });
        }
        return Ok(());
    }
    if let Some(channel) = rdpdr
        && send_data.channel_id == channel.channel_id()
    {
        let channel_id = channel.channel_id();
        for response in channel.on_channel_data(&send_data.data)? {
            let _ = frame_sender.send(Frame {
                channel: ChannelKey::Static(channel_id),
                priority: Priority::Latency,
                bytes: response,
            });
        }
    }
    Ok(())
}
