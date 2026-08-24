//! Per-connection write scheduler: a single task owns the socket
//! exclusively (no shared mutex, no producer ever touches the socket
//! directly), draining a priority-aware [`Scheduler`] one frame at a time.
//! Re-checking the scheduler between every single frame write (not just
//! between batches) is what actually bounds how long a burst of bulk
//! frames (e.g. a full-screen graphics update during video playback) can
//! delay a latency-sensitive frame (e.g. an audio wave chunk) that arrives
//! mid-burst - see `scheduler.rs`'s tests for the exact property being
//! fixed.
//!
//! Incoming frames are split across two bounded inboxes so a flood of
//! graphics cannot grow memory without bound or starve audio: bulk frames
//! are dropped when their queue is full; latency frames have a dedicated
//! smaller queue. Live audio uses a latest-wins slot so a slow socket
//! overwrites unread waves instead of queueing seconds of PCM.

mod scheduler;

pub use scheduler::{ChannelKey, Frame, Priority, Scheduler};

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

const LATENCY_QUEUE_CAP: usize = 64;
const BULK_QUEUE_CAP: usize = 256;

/// Cheap to clone - every producer (display updates, rdpsnd, ...) gets its
/// own handle into the same connection's writer task.
#[derive(Clone)]
pub struct FrameSender {
    latency: mpsc::Sender<Frame>,
    bulk: mpsc::Sender<Frame>,
    live: Arc<Mutex<Option<Vec<Frame>>>>,
    live_notify: mpsc::Sender<()>,
}

impl FrameSender {
    /// Enqueues a frame. Fails if the writer has shut down, or if the
    /// corresponding bounded queue is full (callers typically drop bulk
    /// graphics in that case).
    pub fn send(&self, frame: Frame) -> Result<(), Frame> {
        let tx = match frame.priority {
            Priority::Latency => &self.latency,
            Priority::Bulk => &self.bulk,
        };
        tx.try_send(frame).map_err(|e| match e {
            mpsc::error::TrySendError::Full(frame) | mpsc::error::TrySendError::Closed(frame) => {
                frame
            }
        })
    }

    /// Enqueues a frame, waiting for space instead of dropping it if the
    /// corresponding bounded queue is momentarily full. Use this for frames
    /// that must all arrive together as one logical update (e.g. a
    /// BEGIN/tiles/END bitmap sequence) - unlike `send`, a transiently full
    /// queue never causes a frame in the middle of such a sequence to be
    /// silently dropped. Returns `Err` only once the writer has actually
    /// shut down.
    pub async fn send_all(&self, frame: Frame) -> Result<(), Frame> {
        let tx = match frame.priority {
            Priority::Latency => &self.latency,
            Priority::Bulk => &self.bulk,
        };
        tx.send(frame).await.map_err(|e| e.0)
    }

    /// Replaces any unread live-audio wave with `frames` (one RDPSND Wave
    /// split into SVC chunks). Returns `false` only when the writer is gone.
    /// Unread older waves are dropped — the intended trade for live A/V.
    pub fn send_live(&self, frames: Vec<Frame>) -> bool {
        *self.live.lock().unwrap_or_else(|e| e.into_inner()) = Some(frames);
        match self.live_notify.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

pub struct ConnectionWriter<W> {
    sink: W,
    scheduler: Scheduler,
    latency: mpsc::Receiver<Frame>,
    bulk: mpsc::Receiver<Frame>,
    live: Arc<Mutex<Option<Vec<Frame>>>>,
    live_notify: mpsc::Receiver<()>,
    latency_open: bool,
    bulk_open: bool,
    live_open: bool,
}

impl<W: AsyncWrite + Unpin> ConnectionWriter<W> {
    pub fn new(sink: W) -> (Self, FrameSender) {
        let (latency_tx, latency_rx) = mpsc::channel(LATENCY_QUEUE_CAP);
        let (bulk_tx, bulk_rx) = mpsc::channel(BULK_QUEUE_CAP);
        let live = Arc::new(Mutex::new(None));
        let (live_notify_tx, live_notify_rx) = mpsc::channel(1);
        (
            Self {
                sink,
                scheduler: Scheduler::new(),
                latency: latency_rx,
                bulk: bulk_rx,
                live: Arc::clone(&live),
                live_notify: live_notify_rx,
                latency_open: true,
                bulk_open: true,
                live_open: true,
            },
            FrameSender {
                latency: latency_tx,
                bulk: bulk_tx,
                live,
                live_notify: live_notify_tx,
            },
        )
    }

    fn take_live(&self) -> Option<Vec<Frame>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Runs until every [`FrameSender`] for this connection is dropped
    /// (i.e. for the lifetime of the connection) or a write fails.
    pub async fn run(mut self) -> std::io::Result<()> {
        loop {
            if let Some(frames) = self.take_live() {
                for frame in frames {
                    self.sink.write_all(&frame.bytes).await?;
                }
                continue;
            }

            while let Ok(frame) = self.latency.try_recv() {
                self.scheduler.enqueue(frame);
            }
            while let Ok(frame) = self.bulk.try_recv() {
                self.scheduler.enqueue(frame);
            }

            match self.scheduler.pop_next() {
                Some(bytes) => self.sink.write_all(&bytes).await?,
                None => {
                    if !self.latency_open && !self.bulk_open && !self.live_open {
                        break;
                    }
                    tokio::select! {
                        biased;
                        n = self.live_notify.recv(), if self.live_open => {
                            if n.is_none() {
                                self.live_open = false;
                            }
                        }
                        frame = self.latency.recv(), if self.latency_open => {
                            match frame {
                                Some(frame) => self.scheduler.enqueue(frame),
                                None => self.latency_open = false,
                            }
                        }
                        frame = self.bulk.recv(), if self.bulk_open => {
                            match frame {
                                Some(frame) => self.scheduler.enqueue(frame),
                                None => self.bulk_open = false,
                            }
                        }
                    }
                }
            }
        }
        self.sink.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn writes_frames_in_scheduling_order_and_closes_when_senders_drop() {
        let (client_side, server_side) = tokio::io::duplex(4096);
        let (writer, sender) = ConnectionWriter::new(server_side);
        let run_handle = tokio::spawn(writer.run());

        sender
            .send(Frame {
                channel: ChannelKey::Io,
                priority: Priority::Bulk,
                bytes: b"graphics".to_vec(),
            })
            .unwrap();
        sender
            .send(Frame {
                channel: ChannelKey::Static(1004),
                priority: Priority::Latency,
                bytes: b"audio".to_vec(),
            })
            .unwrap();

        drop(sender); // no more frames coming - writer should finish and return.

        let mut received = Vec::new();
        let mut client_side = client_side;
        client_side.read_to_end(&mut received).await.unwrap();

        // Latency frame was enqueued second but must still be written first.
        assert_eq!(received, b"audiographics");
        run_handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn bulk_send_drops_when_queue_is_full() {
        let (_client_side, server_side) = tokio::io::duplex(4096);
        let (writer, sender) = ConnectionWriter::new(server_side);
        // Do not run the writer, so the bulk queue fills.
        let mut accepted = 0usize;
        let mut dropped = 0usize;
        for _ in 0..(BULK_QUEUE_CAP + 8) {
            let frame = Frame {
                channel: ChannelKey::Io,
                priority: Priority::Bulk,
                bytes: vec![0],
            };
            if sender.send(frame).is_ok() {
                accepted += 1;
            } else {
                dropped += 1;
            }
        }
        assert_eq!(accepted, BULK_QUEUE_CAP);
        assert!(dropped >= 8);
        drop(writer);
    }

    #[tokio::test]
    async fn send_all_waits_for_space_instead_of_dropping() {
        // A tiny duplex buffer forces the writer to apply real backpressure,
        // so pushing more than BULK_QUEUE_CAP frames via send_all only
        // succeeds if it actually waits for space rather than dropping
        // (unlike plain `send`, exercised by `bulk_send_drops_when_queue_is_full`).
        let (client_side, server_side) = tokio::io::duplex(64);
        let (writer, sender) = ConnectionWriter::new(server_side);
        let run_handle = tokio::spawn(writer.run());

        let total = BULK_QUEUE_CAP + 50;
        let send_task = tokio::spawn(async move {
            for _ in 0..total {
                sender
                    .send_all(Frame {
                        channel: ChannelKey::Io,
                        priority: Priority::Bulk,
                        bytes: vec![0u8],
                    })
                    .await
                    .unwrap();
            }
        });

        let mut received = Vec::new();
        let mut client_side = client_side;
        client_side.read_to_end(&mut received).await.unwrap();

        send_task.await.unwrap();
        run_handle.await.unwrap().unwrap();
        assert_eq!(received.len(), total);
    }

    #[tokio::test]
    async fn send_live_overwrites_unread_waves_and_preempts_bulk() {
        let (client_side, server_side) = tokio::io::duplex(4096);
        let (writer, sender) = ConnectionWriter::new(server_side);

        sender
            .send(Frame {
                channel: ChannelKey::Io,
                priority: Priority::Bulk,
                bytes: b"graphics".to_vec(),
            })
            .unwrap();
        assert!(sender.send_live(vec![Frame {
            channel: ChannelKey::Static(1004),
            priority: Priority::Latency,
            bytes: b"old".to_vec(),
        }]));
        assert!(sender.send_live(vec![Frame {
            channel: ChannelKey::Static(1004),
            priority: Priority::Latency,
            bytes: b"new".to_vec(),
        }]));
        // Start the writer only after both waves are posted so the unread
        // older wave is already overwritten — no race with the first write.
        let run_handle = tokio::spawn(writer.run());
        drop(sender);

        let mut received = Vec::new();
        let mut client_side = client_side;
        client_side.read_to_end(&mut received).await.unwrap();
        assert_eq!(received, b"newgraphics");
        run_handle.await.unwrap().unwrap();
    }
}
