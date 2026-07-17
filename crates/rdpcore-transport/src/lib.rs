//! Per-connection write scheduler: a single task owns the socket
//! exclusively (no shared mutex, no producer ever touches the socket
//! directly), draining a priority-aware [`Scheduler`] one frame at a time.
//! Re-checking the scheduler between every single frame write (not just
//! between batches) is what actually bounds how long a burst of bulk
//! frames (e.g. a full-screen graphics update during video playback) can
//! delay a latency-sensitive frame (e.g. an audio wave chunk) that arrives
//! mid-burst - see `scheduler.rs`'s tests for the exact property being
//! fixed.

mod scheduler;

pub use scheduler::{ChannelKey, Frame, Priority, Scheduler};

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

/// Cheap to clone - every producer (display updates, rdpsnd, ...) gets its
/// own handle into the same connection's writer task.
#[derive(Clone)]
pub struct FrameSender {
    tx: mpsc::UnboundedSender<Frame>,
}

impl FrameSender {
    /// Fails only once the writer task has shut down (connection closed).
    pub fn send(&self, frame: Frame) -> Result<(), Frame> {
        self.tx.send(frame).map_err(|e| e.0)
    }
}

pub struct ConnectionWriter<W> {
    sink: W,
    scheduler: Scheduler,
    inbox: mpsc::UnboundedReceiver<Frame>,
}

impl<W: AsyncWrite + Unpin> ConnectionWriter<W> {
    pub fn new(sink: W) -> (Self, FrameSender) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                sink,
                scheduler: Scheduler::new(),
                inbox: rx,
            },
            FrameSender { tx },
        )
    }

    /// Runs until every [`FrameSender`] for this connection is dropped
    /// (i.e. for the lifetime of the connection) or a write fails.
    pub async fn run(mut self) -> std::io::Result<()> {
        loop {
            while let Ok(frame) = self.inbox.try_recv() {
                self.scheduler.enqueue(frame);
            }

            match self.scheduler.pop_next() {
                Some(bytes) => self.sink.write_all(&bytes).await?,
                None => match self.inbox.recv().await {
                    Some(frame) => self.scheduler.enqueue(frame),
                    None => break,
                },
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
}
