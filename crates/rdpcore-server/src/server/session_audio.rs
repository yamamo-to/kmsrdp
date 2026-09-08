//! RDPSND wave enqueue helpers and play-queue diagnostics for the
//! steady-state audio task.

use rdpcore_rdpsnd::RdpsndChannel;
use rdpcore_transport::{ChannelKey, Frame, Priority};
use tracing::debug;

/// Encode one PCM chunk and enqueue it on the latency path.
pub(crate) fn send_wave_frames(
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

/// Rate-limited RDPSND play-queue logging for the per-connection audio task.
pub(crate) struct PlayQueueDiag {
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
