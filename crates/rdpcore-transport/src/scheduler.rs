//! The scheduling policy, kept pure/synchronous on purpose: no tokio, no
//! I/O, no timing - so it can be unit-tested deterministically instead of
//! relying on flaky async timing assertions. [`ConnectionWriter`] (in
//! `lib.rs`) is the thin async wrapper that actually touches a socket,
//! re-checking this scheduler between every single frame write.

use std::collections::VecDeque;

/// Identifies a channel purely for the scheduler's bookkeeping -
/// independent of the wire-level MCS channel ID numbering, so dynamic
/// virtual channels (multiplexed under `"drdynvc"`) and static channels
/// can share one enum here without the scheduler needing DVC details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelKey {
    /// The main graphics + input channel.
    Io,
    /// A static virtual channel, keyed by its negotiated MCS channel ID
    /// (e.g. rdpsnd, cliprdr, rdpdr).
    Static(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Small, real-time-sensitive data — audio wave chunks, resize /
    /// control PDUs. Always fully drained before any `Bulk` channel gets a
    /// turn.
    Latency,
    /// Large, throughput-oriented data — graphics tiles, cliprdr paste of
    /// a big clipboard blob, and (today) RDPDR I/O when marked Latency by
    /// the server loop. Serviced one frame at a time, round-robin across
    /// channels, only when no `Latency` channel has anything pending.
    Bulk,
}

/// One already-fully-framed (TPKT/X.224/MCS/SVC headers all applied),
/// wire-ready unit, small enough that writing it wholesale is an
/// acceptable unit of scheduling granularity - callers are responsible for
/// pre-chunking large payloads (e.g. bitmap update fragments) before
/// wrapping them as `Frame`s, since the scheduler's fairness guarantee is
/// only as fine-grained as "between frames," not within one.
#[derive(Debug, Clone)]
pub struct Frame {
    pub channel: ChannelKey,
    pub priority: Priority,
    pub bytes: Vec<u8>,
}

struct ChannelQueue {
    key: ChannelKey,
    priority: Priority,
    pending: VecDeque<Vec<u8>>,
}

/// The pure scheduling policy: latency channels always drain first; bulk
/// channels are serviced one frame at a time, round-robin, only when every
/// latency channel is empty.
#[derive(Default)]
pub struct Scheduler {
    queues: Vec<ChannelQueue>,
    /// Indices into `queues` of every `Bulk`-priority queue, in creation
    /// order - `queues` only ever grows and a queue's priority is fixed
    /// at creation (see `enqueue`), so this can be maintained
    /// incrementally instead of re-filtering `queues` on every `pop_next`
    /// call, which the doc comment on `pop_next` notes runs between every
    /// single frame write.
    bulk_order: Vec<usize>,
    next_bulk_channel: usize,
}

impl Scheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// A single `ChannelKey` (e.g. the multiplexed drdynvc mux channel)
    /// can legitimately carry both `Latency` and `Bulk` frames - so
    /// channels are split into a separate scheduler queue per
    /// `(ChannelKey, Priority)` pair rather than one queue per key. That
    /// keeps FIFO order within each priority class while still letting a
    /// `Latency` frame on a channel preempt `Bulk` frames already queued
    /// for that same channel, instead of getting stuck behind them in one
    /// shared FIFO.
    pub fn enqueue(&mut self, frame: Frame) {
        let idx = self
            .queues
            .iter()
            .position(|q| q.key == frame.channel && q.priority == frame.priority)
            .unwrap_or_else(|| {
                self.queues.push(ChannelQueue {
                    key: frame.channel,
                    priority: frame.priority,
                    pending: VecDeque::new(),
                });
                let idx = self.queues.len() - 1;
                if frame.priority == Priority::Bulk {
                    self.bulk_order.push(idx);
                }
                idx
            });
        self.queues[idx].pending.push_back(frame.bytes);
    }

    pub fn has_pending(&self) -> bool {
        self.queues.iter().any(|q| !q.pending.is_empty())
    }

    /// Pops the single next frame to write, or `None` if there's nothing
    /// queued anywhere right now. Callers (the async writer) must call
    /// this again - re-checking latency channels - after writing whatever
    /// this returns, before writing anything else; that's what actually
    /// bounds how long a burst of bulk frames can delay a latency frame
    /// that arrives mid-burst.
    pub fn pop_next(&mut self) -> Option<Vec<u8>> {
        for q in self
            .queues
            .iter_mut()
            .filter(|q| q.priority == Priority::Latency)
        {
            if let Some(bytes) = q.pending.pop_front() {
                return Some(bytes);
            }
        }

        if self.bulk_order.is_empty() {
            return None;
        }

        for offset in 0..self.bulk_order.len() {
            let idx = self.bulk_order[(self.next_bulk_channel + offset) % self.bulk_order.len()];
            if let Some(bytes) = self.queues[idx].pending.pop_front() {
                self.next_bulk_channel =
                    (self.next_bulk_channel + offset + 1) % self.bulk_order.len();
                return Some(bytes);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(channel: ChannelKey, priority: Priority, tag: u8) -> Frame {
        Frame {
            channel,
            priority,
            bytes: vec![tag],
        }
    }

    #[test]
    fn latency_always_pops_before_bulk_regardless_of_enqueue_order() {
        let mut s = Scheduler::new();
        s.enqueue(frame(ChannelKey::Io, Priority::Bulk, 1));
        s.enqueue(frame(ChannelKey::Io, Priority::Bulk, 2));
        s.enqueue(frame(ChannelKey::Static(1004), Priority::Latency, 0xAA));

        // Latency frame was enqueued last but must still come out first.
        assert_eq!(s.pop_next(), Some(vec![0xAA]));
        assert_eq!(s.pop_next(), Some(vec![1]));
        assert_eq!(s.pop_next(), Some(vec![2]));
        assert_eq!(s.pop_next(), None);
    }

    #[test]
    fn a_latency_frame_arriving_between_pops_preempts_the_next_bulk_frame() {
        // Models the real bug this crate exists to fix: a bulk burst is
        // mid-flight (several frames already queued) when a new latency
        // frame shows up - it must be sent before the *next* bulk frame,
        // not queued behind the rest of the burst.
        let mut s = Scheduler::new();
        s.enqueue(frame(ChannelKey::Io, Priority::Bulk, 1));
        s.enqueue(frame(ChannelKey::Io, Priority::Bulk, 2));
        s.enqueue(frame(ChannelKey::Io, Priority::Bulk, 3));

        assert_eq!(s.pop_next(), Some(vec![1])); // first bulk frame goes out

        // Now, "mid-burst," an audio frame arrives.
        s.enqueue(frame(ChannelKey::Static(1004), Priority::Latency, 0xAA));

        assert_eq!(s.pop_next(), Some(vec![0xAA])); // preempts frame 2
        assert_eq!(s.pop_next(), Some(vec![2]));
        assert_eq!(s.pop_next(), Some(vec![3]));
    }

    #[test]
    fn bulk_channels_are_serviced_fifo_within_a_channel() {
        let mut s = Scheduler::new();
        for tag in 0..5 {
            s.enqueue(frame(ChannelKey::Io, Priority::Bulk, tag));
        }
        for tag in 0..5 {
            assert_eq!(s.pop_next(), Some(vec![tag]));
        }
        assert_eq!(s.pop_next(), None);
    }

    #[test]
    fn multiple_bulk_channels_round_robin() {
        let mut s = Scheduler::new();
        s.enqueue(frame(ChannelKey::Io, Priority::Bulk, 1));
        s.enqueue(frame(ChannelKey::Io, Priority::Bulk, 2));
        s.enqueue(frame(ChannelKey::Static(2000), Priority::Bulk, 10));
        s.enqueue(frame(ChannelKey::Static(2000), Priority::Bulk, 20));

        // Alternates between the two bulk channels rather than draining
        // one fully before touching the other.
        assert_eq!(s.pop_next(), Some(vec![1]));
        assert_eq!(s.pop_next(), Some(vec![10]));
        assert_eq!(s.pop_next(), Some(vec![2]));
        assert_eq!(s.pop_next(), Some(vec![20]));
        assert_eq!(s.pop_next(), None);
    }

    #[test]
    fn priority_is_tracked_per_frame_not_per_channel() {
        // Models drdynvc: GFX (Bulk) and DVC control (Latency) frames both
        // travel over the same multiplexed ChannelKey.
        let mut s = Scheduler::new();
        let dvc = ChannelKey::Static(1004);
        s.enqueue(frame(dvc, Priority::Bulk, 1));
        s.enqueue(frame(dvc, Priority::Bulk, 2));
        s.enqueue(frame(dvc, Priority::Latency, 0xAA));

        // The channel's first-ever frames were Bulk, but a Latency frame
        // on the SAME channel key must still preempt Bulk frames already
        // queued ahead of it for that key, not get stuck behind them.
        assert_eq!(s.pop_next(), Some(vec![0xAA]));
        assert_eq!(s.pop_next(), Some(vec![1]));
        assert_eq!(s.pop_next(), Some(vec![2]));
        assert_eq!(s.pop_next(), None);
    }

    #[test]
    fn has_pending_reflects_queue_state() {
        let mut s = Scheduler::new();
        assert!(!s.has_pending());
        s.enqueue(frame(ChannelKey::Io, Priority::Bulk, 1));
        assert!(s.has_pending());
        s.pop_next();
        assert!(!s.has_pending());
    }
}
