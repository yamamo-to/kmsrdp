#[cfg(feature = "gfx")]
use rdpcore_dvc::DvcMux;
use rdpcore_pdu::fastpath::UPDATE_CODE_SURFACE_COMMANDS;
use rdpcore_pdu::surface_commands::{FRAME_ACTION_BEGIN, FRAME_ACTION_END, encode_frame_marker};
#[cfg(feature = "gfx")]
use rdpcore_rdpegfx::GfxSession;
use rdpcore_transport::{ChannelKey, Frame, FrameSender, Priority};

use super::metrics::SessionBitmapMetrics;
use crate::display::BitmapUpdate;
use crate::encode::{
    BitmapEncodePolicy, EncodeScratch, encode_bitmap_update, encode_nscodec_update,
    encode_update_to_wire_frames,
};
use crate::error::SessionError;

/// How long a single frame is allowed to wait for bulk-queue space before
/// the connection is treated as dead. `FrameSender::send_all` waits
/// indefinitely on its own (that's the fix for truncating updates under
/// brief backpressure) - this bounds that wait so a client that's genuinely
/// stopped reading (hung, not just momentarily slow) still gets reaped
/// instead of wedging this connection's whole steady-state loop (input
/// included) forever.
const BULK_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Waits for bulk-queue space like `FrameSender::send_all`, but gives up
/// (as if the writer had closed) after [`BULK_SEND_TIMEOUT`] instead of
/// waiting forever.
pub async fn send_all_or_timeout(
    frame_sender: &FrameSender,
    frame: Frame,
) -> Result<(), SessionError> {
    match tokio::time::timeout(BULK_SEND_TIMEOUT, frame_sender.send_all(frame)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) | Err(_) => Err(SessionError::WriterClosed),
    }
}

/// Encodes one Planar/NSCodec update to wire frames without sending.
///
/// Send via [`send_frames`] / [`spawn_send_frames`] so the session loop can
/// keep reading WaveConfirms while a full-screen bulk burst waits for
/// queue space.
pub async fn encode_outbound_bitmap(
    bitmap: &BitmapUpdate,
    policy: &BitmapEncodePolicy,
    frame_id: &mut u32,
    metrics: &mut SessionBitmapMetrics,
) -> Result<Vec<Frame>, SessionError> {
    // Planar/NSCodec encoding is CPU-bound and was previously run inline on
    // this connection's steady-state select! task, stalling input dispatch
    // and every other channel for the full encode duration on every frame
    // (the same class of bug the RDPSND wave path was pulled off this loop
    // for - see the comment above `_audio_task`). Run it on the blocking
    // pool instead.
    let bitmap = bitmap.clone();
    let policy = *policy;
    let (batches, stats) = tokio::task::spawn_blocking(move || {
        if let Some((codec_id, cll)) = policy.nscodec {
            encode_nscodec_update(&bitmap, codec_id, cll, policy.max_request_size)
        } else {
            let mut scratch = EncodeScratch::default();
            let stats = encode_bitmap_update(&bitmap, &policy, &mut scratch);
            (scratch.batches, stats)
        }
    })
    .await
    .map_err(|_| SessionError::EncodeJoin)?;
    tracing::debug!(
        tiles = stats.tiles,
        compressed_tiles = stats.compressed_tiles,
        raw_tiles = stats.raw_tiles,
        encoded_bytes = stats.encoded_bytes,
        update_batches = stats.update_batches,
        max_request_size = policy.max_request_size,
        "kmsrdp: bitmap update encoded"
    );
    metrics.record(stats);

    let id = *frame_id;
    *frame_id = frame_id.wrapping_add(1).max(1);
    // Frame Marker is formally a Surface Commands concept (MS-RDPEGDI) with
    // no defined role for the classic `TS_BITMAP_UPDATE_DATA` path the
    // Planar branch above uses - a prior change here restricted it to
    // NSCodec on that reading. In practice, real clients use it as a
    // generic "a logical frame just completed" signal regardless of which
    // update carried the pixels: Guacamole's guacd (`guac_rdp_gdi_*_frame_marker`
    // in guacamole-server) drives its render thread's frame-boundary
    // detection off exactly this PDU, falling back to a lossy ~10-100ms
    // timing heuristic without it. Restricting it to NSCodec-only made that
    // fallback guacd's *only* option for Planar (the path every non-NSCodec
    // client, including guacd, actually uses) and didn't fix the stuck/stale
    // region it was meant to address - so it goes back to wrapping every
    // bitmap update, not just NSCodec's.
    let begin = encode_update_to_wire_frames(
        UPDATE_CODE_SURFACE_COMMANDS,
        &encode_frame_marker(FRAME_ACTION_BEGIN, id),
        policy.max_request_size,
    );
    let end = encode_update_to_wire_frames(
        UPDATE_CODE_SURFACE_COMMANDS,
        &encode_frame_marker(FRAME_ACTION_END, id),
        policy.max_request_size,
    );

    // NOTE: each of `begin`, one entry of `batches`, and `end` is the
    // *complete* Fast-Path fragment sequence for one logical
    // `TS_BITMAP_UPDATE_DATA`/Frame-Marker PDU. Fragments of a single PDU are
    // concatenated into one `Frame` so the transport scheduler cannot
    // interleave another channel's Latency frame mid-reassembly (clients
    // keep one global Fast-Path reassembly buffer; an interruption drops
    // or corrupts the update and leaves residual tiles). Preemption is
    // still allowed between whole PDUs (BEGIN / each bitmap batch / END).
    Ok(std::iter::once(concat_wire_pdu(begin))
        .chain(batches.into_iter().map(concat_wire_pdu))
        .chain(std::iter::once(concat_wire_pdu(end)))
        .filter(|bytes| !bytes.is_empty())
        .map(|bytes| Frame {
            channel: ChannelKey::Io,
            priority: Priority::Bulk,
            bytes,
        })
        .collect())
}

/// Concatenate the Fast-Path packets that make up one fragmented PDU into
/// a single scheduler `Frame` so nothing else is written between them.
fn concat_wire_pdu(parts: Vec<Vec<u8>>) -> Vec<u8> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    let mut out = Vec::with_capacity(len);
    for part in parts {
        out.extend(part);
    }
    out
}

pub async fn send_frames(
    frame_sender: &FrameSender,
    frames: Vec<Frame>,
) -> Result<(), SessionError> {
    for frame in frames {
        // A momentarily full bulk queue must not truncate this update
        // partway through (e.g. dropping the tail rows of a full-screen
        // refresh) or tear down the session - wait for space instead
        // (bounded, so a genuinely hung client still gets reaped).
        send_all_or_timeout(frame_sender, frame).await?;
    }
    Ok(())
}

/// Sends already-encoded graphics off the session loop. Only one of these
/// should be in flight per connection so BEGIN/tiles/END (or GFX) sequences
/// stay ordered. The loop keeps polling `read_steady_state_frame` while
/// this waits on [`send_all_or_timeout`].
pub fn spawn_send_frames(
    frame_sender: FrameSender,
    frames: Vec<Frame>,
) -> tokio::task::JoinHandle<Result<(), SessionError>> {
    tokio::spawn(async move { send_frames(&frame_sender, frames).await })
}

/// Encodes Planar/NSCodec if GFX did not handle the frame, then starts at
/// most one in-flight bulk send. If a send is already running, `bitmap` is
/// kept as latest-wins instead of queueing a second burst. (The caller in
/// `session_loop.rs` only ever invokes this once `bulk_send` is confirmed
/// `None`, so this branch is a defensive fallback, not a live path -
/// catch-up policy lives in the caller now.)
#[allow(clippy::too_many_arguments)]
pub async fn encode_and_queue_bitmap(
    bitmap: BitmapUpdate,
    frame_sender: &FrameSender,
    policy: &BitmapEncodePolicy,
    frame_id: &mut u32,
    metrics: &mut SessionBitmapMetrics,
    bulk_send: &mut Option<tokio::task::JoinHandle<Result<(), SessionError>>>,
    deferred_bitmap: &mut Option<BitmapUpdate>,
    gfx_frames: Vec<Frame>,
    gfx_handled: bool,
) -> Result<(), SessionError> {
    if bulk_send.is_some() {
        *deferred_bitmap = Some(bitmap);
        return Ok(());
    }
    let frames = if gfx_handled {
        let gfx_bytes: usize = gfx_frames.iter().map(|f| f.bytes.len()).sum();
        metrics.record_gfx(gfx_bytes);
        gfx_frames
    } else {
        let mut frames = gfx_frames;
        frames.extend(encode_outbound_bitmap(&bitmap, policy, frame_id, metrics).await?);
        frames
    };
    if frames.is_empty() {
        return Ok(());
    }
    *bulk_send = Some(spawn_send_frames(frame_sender.clone(), frames));
    Ok(())
}

pub async fn send_outbound_bitmap(
    bitmap: &BitmapUpdate,
    frame_sender: &FrameSender,
    policy: &BitmapEncodePolicy,
    frame_id: &mut u32,
    metrics: &mut SessionBitmapMetrics,
) -> Result<(), SessionError> {
    let frames = encode_outbound_bitmap(bitmap, policy, frame_id, metrics).await?;
    send_frames(frame_sender, frames).await
}

/// Outcome of attempting the GFX path for one frame - kept separate from
/// actually sending, so the caller only touches `&DvcMux` after the encode
/// `.await` completes (see `try_encode_gfx_frame`'s doc comment for why).
#[cfg(feature = "gfx")]
pub enum GfxEncodeOutcome {
    /// No GFX session, or not ready yet: fall back to Planar/NSCodec.
    Fallback,
    /// GFX handled the frame; encode succeeded and the caller should send
    /// these wire payloads (updating `last_gfx_data` as a side effect
    /// already happened before this was returned).
    Send(Vec<Vec<u8>>),
    /// GFX handled the frame with an intentional soft skip (e.g. transient
    /// OpenH264 RC): keep the GFX path so we do not paint Planar over a
    /// black H.264 surface, but there is nothing to send this tick.
    SoftSkip,
    /// GFX is abandoned for this connection: send optional teardown PDUs, then
    /// fall through to Planar/NSCodec without dropping the session.
    Disable { teardown: Vec<Vec<u8>> },
}

/// True when `source` is the same framebuffer Arc already encoded this tick.
#[cfg(any(test, feature = "gfx"))]
pub fn gfx_already_sent_frame(
    last: &Option<std::sync::Arc<[u8]>>,
    source: &std::sync::Arc<[u8]>,
) -> bool {
    last.as_ref()
        .is_some_and(|prev| std::sync::Arc::ptr_eq(prev, source))
}

/// Runs the GFX H.264 encode for one frame, if applicable.
///
/// The encode is CPU/GPU-bound and runs on the blocking pool (see
/// `send_outbound_bitmap`'s comment for why this moved off the
/// steady-state task). Deliberately takes no `&DvcMux`: that type isn't
/// `Sync`, so a reference to it can't be held across the `.await` below
/// without making the whole per-connection task non-`Send` (and thus
/// unspawnable) - callers apply the result (which does need `&DvcMux`, via
/// [`send_gfx_payloads`]) only after this returns.
#[cfg(feature = "gfx")]
pub async fn try_encode_gfx_frame(
    gfx: Option<&GfxSession>,
    last_gfx_data: &mut Option<std::sync::Arc<[u8]>>,
    latest_full: Option<&BitmapUpdate>,
    bitmap: &BitmapUpdate,
) -> Result<GfxEncodeOutcome, SessionError> {
    let Some(gfx) = gfx else {
        return Ok(GfxEncodeOutcome::Fallback);
    };
    if !gfx.is_ready() {
        return Ok(GfxEncodeOutcome::Fallback);
    }
    let source = latest_full.unwrap_or(bitmap).clone();
    let source_data = std::sync::Arc::clone(&source.data);
    // Capture allocates a new framebuffer Arc every tick, so pointer
    // equality only dedups the N dirty-rect notifications of one frame
    // (each would otherwise re-encode the whole desktop). A later tick
    // always has a new Arc, so periodic IDR still runs.
    if gfx_already_sent_frame(last_gfx_data, &source_data) {
        return Ok(GfxEncodeOutcome::SoftSkip);
    }
    let gfx_for_encode = gfx.clone();
    let payloads = tokio::task::spawn_blocking(move || {
        gfx_for_encode.encode_frame(
            source.width.get(),
            source.height.get(),
            source.stride.get(),
            source.data.as_ref(),
        )
    })
    .await
    .map_err(|_| SessionError::GfxEncodeJoin)?;
    match payloads {
        rdpcore_rdpegfx::GfxFrameResult::Frames(payloads) => {
            *last_gfx_data = Some(source_data);
            Ok(GfxEncodeOutcome::Send(payloads))
        }
        rdpcore_rdpegfx::GfxFrameResult::Skip => Ok(GfxEncodeOutcome::SoftSkip),
        rdpcore_rdpegfx::GfxFrameResult::Fallback { teardown } => {
            Ok(GfxEncodeOutcome::Disable { teardown })
        }
    }
}

/// Builds the wire-ready GFX channel frames for `payloads`.
#[cfg(feature = "gfx")]
pub(crate) fn build_gfx_frames(
    mux: &DvcMux,
    payloads: Vec<Vec<u8>>,
) -> Result<Vec<Frame>, SessionError> {
    let Some(dyn_id) = mux.channel_id_for_name(rdpcore_rdpegfx::CHANNEL_NAME) else {
        return Err(SessionError::GfxChannelMissing);
    };
    let channel = ChannelKey::Static(mux.channel_id());
    Ok(mux
        .wrap_channel_payloads(dyn_id, payloads)
        .into_iter()
        .map(|bytes| Frame {
            channel,
            priority: Priority::Bulk,
            bytes,
        })
        .collect())
}

/// Applies a [`GfxEncodeOutcome`], building any wire frames it needs sent.
/// Deliberately synchronous (no `.await`, unlike the rest of this module):
/// `&DvcMux` isn't `Sync`, so merely being a parameter of an `async fn`
/// that awaits anything - even if unused past that point - makes the
/// generated future `!Send` and unspawnable. Keeping this function fully
/// sync means `&DvcMux` never needs to cross an await point; the caller
/// sends the returned frames via [`send_gfx_frames`] afterward.
#[cfg(feature = "gfx")]
pub fn apply_gfx_encode_outcome(
    outcome: GfxEncodeOutcome,
    dvc: Option<&DvcMux>,
) -> Result<(bool, Vec<Frame>), SessionError> {
    match outcome {
        GfxEncodeOutcome::Fallback => Ok((false, Vec::new())),
        GfxEncodeOutcome::SoftSkip => Ok((true, Vec::new())),
        GfxEncodeOutcome::Send(payloads) => {
            let mux = dvc.ok_or(SessionError::GfxChannelMissing)?;
            Ok((true, build_gfx_frames(mux, payloads)?))
        }
        GfxEncodeOutcome::Disable { teardown } => {
            let frames = match (teardown.is_empty(), dvc) {
                (false, Some(mux)) => build_gfx_frames(mux, teardown).unwrap_or_default(),
                _ => Vec::new(),
            };
            Ok((false, frames))
        }
    }
}

/// Sends already-built GFX wire frames, waiting for bulk-queue space
/// instead of dropping a frame mid-update or tearing down the session (see
/// `send_outbound_bitmap`). Takes no `&DvcMux` - see
/// [`apply_gfx_encode_outcome`]'s doc comment for why that matters here.
#[cfg(feature = "gfx")]
pub async fn send_gfx_frames(
    frame_sender: &FrameSender,
    frames: Vec<Frame>,
) -> Result<(), SessionError> {
    send_frames(frame_sender, frames).await
}

pub async fn flush_pending_resize_bitmap(
    pending: &mut Option<BitmapUpdate>,
    frame_sender: &FrameSender,
    policy: &BitmapEncodePolicy,
    frame_id: &mut u32,
    display_updates_allowed: bool,
    metrics: &mut SessionBitmapMetrics,
) -> Result<(), SessionError> {
    if !display_updates_allowed {
        *pending = None;
        return Ok(());
    }
    let Some(bitmap) = pending.take() else {
        return Ok(());
    };
    send_outbound_bitmap(&bitmap, frame_sender, policy, frame_id, metrics).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{BitmapUpdate, PixelFormat};
    use crate::encode::bitmap_encode_policy;
    use std::num::{NonZeroU16, NonZeroUsize};

    fn tiny_bitmap() -> BitmapUpdate {
        let width = NonZeroU16::new(1).unwrap();
        let height = NonZeroU16::new(1).unwrap();
        let stride = NonZeroUsize::new(4).unwrap();
        BitmapUpdate {
            x: 0,
            y: 0,
            width,
            height,
            format: PixelFormat::BgrX32,
            data: std::sync::Arc::from(vec![0u8; 4]),
            stride,
            src_x: 0,
            src_y: 0,
        }
    }

    #[tokio::test]
    async fn encode_and_queue_keeps_latest_bitmap_when_a_send_is_in_flight() {
        let (_writer, sender) = rdpcore_transport::ConnectionWriter::new(tokio::io::sink());
        let mut bulk_send = Some(tokio::spawn(async {
            std::future::pending::<Result<(), SessionError>>().await
        }));
        let mut deferred = None;
        let mut frame_id = 1u32;
        let mut metrics = SessionBitmapMetrics::default();
        let policy = bitmap_encode_policy("test", None, 8192);

        encode_and_queue_bitmap(
            tiny_bitmap(),
            &sender,
            &policy,
            &mut frame_id,
            &mut metrics,
            &mut bulk_send,
            &mut deferred,
            Vec::new(),
            false,
        )
        .await
        .unwrap();

        assert!(deferred.is_some(), "in-flight send must latest-wins defer");
        assert_eq!(frame_id, 1, "deferred path must not consume a frame id");
        if let Some(handle) = bulk_send.take() {
            handle.abort();
        }
    }

    /// A classic `TS_BITMAP_UPDATE_DATA` (Planar/raw) update is still
    /// bracketed in a Surface Commands Frame Marker even though it has no
    /// frame-boundary concept of its own on paper - real clients (e.g.
    /// Guacamole's guacd) treat it as a generic "frame complete" signal
    /// regardless of which update carried the pixels, and fall back to a
    /// lossy timing heuristic without it (see `encode_outbound_bitmap`'s
    /// doc comment).
    #[tokio::test]
    async fn planar_path_keeps_frame_marker_wrapping() {
        let policy = bitmap_encode_policy("test", None, 8192);
        let bitmap = tiny_bitmap();

        let mut scratch = EncodeScratch::default();
        encode_bitmap_update(&bitmap, &policy, &mut scratch);
        assert!(!scratch.batches.is_empty());

        let mut frame_id = 1u32;
        let mut metrics = SessionBitmapMetrics::default();
        let frames = encode_outbound_bitmap(&bitmap, &policy, &mut frame_id, &mut metrics)
            .await
            .unwrap();

        assert!(
            frames.len() >= 3,
            "Planar update must still be bracketed by begin/end frame markers (got {} frames)",
            frames.len()
        );
        // Fragments of each PDU are concatenated, so the outbound frame
        // count is 2 markers + number of bitmap batches — not one Frame
        // per wire fragment.
        assert_eq!(frames.len(), 2 + scratch.batches.len());
    }

    /// NSCodec genuinely goes over Surface Commands (`SetSurfaceBits`), so
    /// it keeps the Frame Marker wrapping.
    #[tokio::test]
    async fn nscodec_path_keeps_frame_marker_wrapping() {
        let nscodec = rdpcore_pdu::capability_sets::NsCodecNegotiated {
            codec_id: 1,
            color_loss_level: 3,
        };
        let policy = bitmap_encode_policy("test", Some(nscodec), 8192);
        let bitmap = tiny_bitmap();

        let (nscodec_batches, _) =
            crate::encode::encode_nscodec_update(&bitmap, 1, 3, policy.max_request_size);
        assert!(!nscodec_batches.is_empty());

        let mut frame_id = 1u32;
        let mut metrics = SessionBitmapMetrics::default();
        let frames = encode_outbound_bitmap(&bitmap, &policy, &mut frame_id, &mut metrics)
            .await
            .unwrap();

        assert!(
            frames.len() >= 3,
            "NSCodec update must still be bracketed by begin/end frame markers (got {} frames)",
            frames.len()
        );
        assert_eq!(frames.len(), 2 + nscodec_batches.len());
    }
}
