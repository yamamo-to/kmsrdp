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
//! smaller queue.

mod scheduler;

pub use scheduler::{ChannelKey, Frame, Priority, Scheduler};

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
}

pub struct ConnectionWriter<W> {
    sink: W,
    scheduler: Scheduler,
    latency: mpsc::Receiver<Frame>,
    bulk: mpsc::Receiver<Frame>,
    latency_open: bool,
    bulk_open: bool,
}

impl<W: AsyncWrite + Unpin> ConnectionWriter<W> {
    pub fn new(sink: W) -> (Self, FrameSender) {
        let (latency_tx, latency_rx) = mpsc::channel(LATENCY_QUEUE_CAP);
        let (bulk_tx, bulk_rx) = mpsc::channel(BULK_QUEUE_CAP);
        (
            Self {
                sink,
                scheduler: Scheduler::new(),
                latency: latency_rx,
                bulk: bulk_rx,
                latency_open: true,
                bulk_open: true,
            },
            FrameSender {
                latency: latency_tx,
                bulk: bulk_tx,
            },
        )
    }

    /// Runs until every [`FrameSender`] for this connection is dropped
    /// (i.e. for the lifetime of the connection) or a write fails.
    pub async fn run(mut self) -> std::io::Result<()> {
        loop {
            while let Ok(frame) = self.latency.try_recv() {
                self.scheduler.enqueue(frame);
            }
            while let Ok(frame) = self.bulk.try_recv() {
                self.scheduler.enqueue(frame);
            }

            match self.scheduler.pop_next() {
                Some(bytes) => self.sink.write_all(&bytes).await?,
                None => {
                    if !self.latency_open && !self.bulk_open {
                        break;
                    }
                    tokio::select! {
                        biased;
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
}
