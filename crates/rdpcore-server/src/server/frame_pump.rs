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

pub async fn send_outbound_bitmap(
    bitmap: &BitmapUpdate,
    frame_sender: &FrameSender,
    policy: &BitmapEncodePolicy,
    frame_id: &mut u32,
    metrics: &mut SessionBitmapMetrics,
) -> Result<(), SessionError> {
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
    metrics.record(stats);

    let id = *frame_id;
    *frame_id = frame_id.wrapping_add(1).max(1);
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

    for wire_frame in begin
        .into_iter()
        .chain(batches.into_iter().flatten())
        .chain(end)
    {
        // A momentarily full bulk queue must not truncate this update
        // partway through (e.g. dropping the tail rows of a full-screen
        // refresh) or tear down the session - wait for space instead.
        frame_sender
            .send_all(Frame {
                channel: ChannelKey::Io,
                priority: Priority::Bulk,
                bytes: wire_frame,
            })
            .await
            .map_err(|_| SessionError::WriterClosed)?;
    }
    Ok(())
}

/// Prefer GFX AVC420 when negotiated; otherwise Planar/NSCodec Fast-Path.
/// GFX work is synchronous so `&DvcMux` is never held across an await.
#[allow(clippy::too_many_arguments)]
pub async fn send_outbound_frame(
    bitmap: &BitmapUpdate,
    frame_sender: &FrameSender,
    policy: &BitmapEncodePolicy,
    frame_id: &mut u32,
    latest_full: Option<&BitmapUpdate>,
    metrics: &mut SessionBitmapMetrics,
    #[cfg(feature = "gfx")] gfx_attempt: Option<Result<bool, SessionError>>,
) -> Result<(), SessionError> {
    #[cfg(feature = "gfx")]
    if let Some(result) = gfx_attempt {
        match result {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => return Err(e),
        }
    }
    let _ = latest_full;
    send_outbound_bitmap(bitmap, frame_sender, policy, frame_id, metrics).await
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
    for frame in frames {
        frame_sender
            .send_all(frame)
            .await
            .map_err(|_| SessionError::WriterClosed)?;
    }
    Ok(())
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
