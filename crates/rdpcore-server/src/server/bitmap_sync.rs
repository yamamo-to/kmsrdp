//! Per-connection bookkeeping for what a session is confirmed to have sent
//! the client in full, and the resync-on-catch-up policy built on top of
//! [`resync_bitmap`] that keeps a display update this session couldn't act
//! on right away (startup gate, busy send, kernel backlog) from leaving
//! stale pixels on screen indefinitely.
//!
//! This exists as its own module - separate from `session_loop`'s giant
//! `select!` - so the resync *decision* (when to arm/disarm/advance) can be
//! unit-tested directly instead of only indirectly through a live
//! connection. [`resync_bitmap`] itself (the frame-wide diff) already has
//! its own tests in `encode.rs`; the tests here are about the state
//! transitions layered on top of it.

use tracing::debug;

use crate::display::BitmapUpdate;
use crate::encode::resync_bitmap;
use crate::error::SessionError;

/// Tracks what this connection is confirmed to have sent in full, and
/// whether a resync against that baseline is currently owed.
#[derive(Default)]
pub(crate) struct BitmapSyncState {
    /// Set whenever this session might be missing pixels the client
    /// should have - a capture dirty-rect notification arrived, or a send
    /// attempt was deferred. Cleared once a resync computed from
    /// `last_synced_full` is confirmed queued for sending.
    pending_resync: bool,
    /// The last full-desktop frame this session is confirmed to have sent
    /// in its entirety (either directly, or as the baseline a resync
    /// diffed against and then fully covered).
    last_synced_full: Option<BitmapUpdate>,
    /// Set alongside a resync's output bitmap by [`Self::take_pending`], so
    /// `last_synced_full` only advances to it once [`Self::confirm_send`]
    /// reports that pump actually got handed off to be sent - not before,
    /// since a resync only reflects reality once it's queued.
    resync_target: Option<BitmapUpdate>,
}

impl BitmapSyncState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A capture-side dirty-rect notification arrived. Dirty rects are only
    /// ever a wake-up here: every send instead diffs against
    /// `last_synced_full` via [`resync_bitmap`], so a rect this session
    /// never got to act on can't leave residual glyphs on the client (see
    /// `resync_bitmap`'s doc comment for why a running union isn't enough).
    pub(crate) fn mark_dirty(&mut self) {
        self.pending_resync = true;
    }

    /// Whether a resync is currently owed.
    pub(crate) fn has_pending(&self) -> bool {
        self.pending_resync
    }

    /// Consumes a pending resync flag and turns it into a bitmap to send,
    /// arming [`Self::resync_target`] until [`Self::confirm_send`] reports
    /// whether it actually went out. Returns `None` if nothing was
    /// pending, the display has no full frame yet, or the resync found
    /// nothing actually different.
    ///
    /// Runs the frame-wide diff on the blocking pool, not inline on the
    /// caller's task - same reason `encode_outbound_bitmap` does: this
    /// walks the whole frame (same cost class as the capture-side
    /// dirty-diff it mirrors) and would otherwise stall Fast-Path input
    /// dispatch for its duration on every catch-up cycle.
    pub(crate) async fn take_pending(
        &mut self,
        latest_full: Option<BitmapUpdate>,
    ) -> Result<Option<BitmapUpdate>, SessionError> {
        if !std::mem::take(&mut self.pending_resync) {
            return Ok(None);
        }
        let Some(full) = latest_full else {
            debug!("kmsrdp: resync pending but no latest_full frame yet");
            return Ok(None);
        };
        let had_baseline = self.last_synced_full.is_some();
        let last_synced_full = self.last_synced_full.clone();
        let full_for_diff = full.clone();
        let merged = tokio::task::spawn_blocking(move || {
            resync_bitmap(last_synced_full.as_ref(), &full_for_diff)
        })
        .await
        .map_err(|_| SessionError::EncodeJoin)?;
        match &merged {
            Some(m) => debug!(
                x = m.x,
                y = m.y,
                w = m.width.get(),
                h = m.height.get(),
                had_baseline,
                "kmsrdp: resync found changed region"
            ),
            None => debug!(had_baseline, "kmsrdp: resync found no difference"),
        }
        let Some(merged) = merged else {
            return Ok(None);
        };
        self.resync_target = Some(full);
        Ok(Some(merged))
    }

    /// A computed pump couldn't be sent right now (busy `bulk_send` or high
    /// kernel backlog). Re-arms the pending resync so a later catch-up
    /// opportunity retries it, and drops any not-yet-confirmed resync
    /// target - letting it stand would let the next unrelated successful
    /// send advance `last_synced_full` to a frame the client never actually
    /// received, and a later resync would then find "nothing different"
    /// while stale glyphs remain on screen.
    ///
    /// Returns whether a resync target had actually been armed, purely so
    /// the caller can pick the right log message.
    pub(crate) fn defer(&mut self) -> bool {
        let had_target = self.resync_target.take().is_some();
        self.pending_resync = true;
        had_target
    }

    /// Takes the resync target armed by the most recent [`Self::take_pending`]
    /// call, if any. Split out from [`Self::confirm_send`] because the
    /// caller needs this *before* it consumes the bitmap being sent (to
    /// decide whether that send also happens to cover the whole desktop on
    /// its own, via `covers_desktop`).
    pub(crate) fn take_resync_target(&mut self) -> Option<BitmapUpdate> {
        self.resync_target.take()
    }

    /// Records the outcome of a send attempt.
    ///
    /// `queued` is whether bytes were actually handed to the writer - an
    /// empty encode (every tile skipped/SoftSkip) must not claim the
    /// client has pixels it never received. `advance_synced` is whatever
    /// [`Self::take_resync_target`] returned for this send; `advance_full`
    /// is `Some(bitmap.clone())` when the sent bitmap covered the whole
    /// desktop by itself (outside of a resync).
    pub(crate) fn confirm_send(
        &mut self,
        queued: bool,
        advance_synced: Option<BitmapUpdate>,
        advance_full: Option<BitmapUpdate>,
    ) {
        if !queued {
            if advance_synced.is_some() {
                self.pending_resync = true;
            }
            return;
        }
        if let Some(synced) = advance_synced {
            self.last_synced_full = Some(synced);
            self.pending_resync = false;
        } else if let Some(full) = advance_full {
            self.last_synced_full = Some(full);
            self.pending_resync = false;
        } else {
            // Partial non-resync send: heal any remaining gap next.
            self.pending_resync = true;
        }
    }

    /// Clears all state - e.g. on a resize, where the old baseline's
    /// geometry no longer applies.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Flushes whatever catch-up is owed: a pending resync (preferring
    /// [`Self::take_pending`] over any stale [`deferred`] Arc), or else the
    /// cheap unioned `deferred` bitmap alone.
    pub(crate) async fn take_catchup(
        &mut self,
        deferred: &mut Option<BitmapUpdate>,
        latest_full: Option<BitmapUpdate>,
    ) -> Result<Option<BitmapUpdate>, SessionError> {
        if self.has_pending() {
            // Anything still sitting in `deferred` is an older Arc that
            // would paint stale pixels after the resync; drop it.
            *deferred = None;
            self.take_pending(latest_full).await
        } else {
            Ok(deferred.take())
        }
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU16;
    use core::num::NonZeroUsize;
    use std::sync::Arc;

    use super::*;
    use crate::display::PixelFormat;

    fn bitmap(x: u16, y: u16, width: u16, height: u16, fill: u8) -> BitmapUpdate {
        let stride = NonZeroUsize::new(usize::from(width) * 4).unwrap();
        BitmapUpdate {
            x,
            y,
            width: NonZeroU16::new(width).unwrap(),
            height: NonZeroU16::new(height).unwrap(),
            format: PixelFormat::BgrX32,
            data: Arc::from(vec![fill; stride.get() * usize::from(height)]),
            stride,
            src_x: 0,
            src_y: 0,
        }
    }

    #[tokio::test]
    async fn dirty_with_no_full_frame_yet_is_dropped_not_retried() {
        // Matches `resync_bitmap`'s caller contract: a resync request that
        // finds no `latest_full` at all is not re-armed automatically. The
        // next capture tick's dirty notification (`mark_dirty`) is what
        // actually retries it in practice.
        let mut state = BitmapSyncState::new();
        state.mark_dirty();
        let result = state.take_pending(None).await.unwrap();
        assert!(result.is_none());
        assert!(!state.has_pending());
    }

    #[tokio::test]
    async fn first_resync_with_no_baseline_returns_whole_frame_and_arms_target() {
        let mut state = BitmapSyncState::new();
        state.mark_dirty();
        let full = bitmap(0, 0, 64, 64, 7);
        let result = state.take_pending(Some(full.clone())).await.unwrap();
        assert!(result.is_some());
        let target = state.take_resync_target().unwrap();
        assert_eq!((target.width.get(), target.height.get()), (64, 64));
    }

    #[tokio::test]
    async fn confirmed_send_advances_baseline_so_next_resync_finds_nothing_new() {
        let mut state = BitmapSyncState::new();
        state.mark_dirty();
        let full = bitmap(0, 0, 64, 64, 3);
        let sent = state.take_pending(Some(full.clone())).await.unwrap();
        assert!(sent.is_some());
        let advance_synced = state.take_resync_target();
        state.confirm_send(true, advance_synced, None);
        assert!(!state.has_pending());

        // Same frame comes back unchanged - nothing left to resend.
        state.mark_dirty();
        let result = state.take_pending(Some(full)).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn defer_before_confirm_drops_target_and_stays_pending() {
        let mut state = BitmapSyncState::new();
        state.mark_dirty();
        let full = bitmap(0, 0, 64, 64, 1);
        let sent = state.take_pending(Some(full)).await.unwrap();
        assert!(sent.is_some());

        let had_target = state.defer();
        assert!(had_target);
        assert!(state.has_pending());
        // The target must not survive a defer - a later unrelated send
        // must not be able to advance the baseline to a frame that was
        // never actually queued.
        assert!(state.take_resync_target().is_none());
    }

    #[tokio::test]
    async fn empty_encode_does_not_advance_baseline() {
        let mut state = BitmapSyncState::new();
        state.mark_dirty();
        let full = bitmap(0, 0, 64, 64, 9);
        let sent = state.take_pending(Some(full)).await.unwrap();
        assert!(sent.is_some());
        let advance_synced = state.take_resync_target();

        // `queued = false`: every tile was skipped, nothing was actually
        // handed to the writer.
        state.confirm_send(false, advance_synced, None);
        assert!(state.has_pending(), "must retry rather than claim synced");
    }

    #[test]
    fn reset_clears_pending_and_baseline() {
        let mut state = BitmapSyncState::new();
        state.mark_dirty();
        state.reset();
        assert!(!state.has_pending());
    }
}
