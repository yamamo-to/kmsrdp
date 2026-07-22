//! RDPSND bridge for the from-scratch `rdpcore-*` stack: shells out to
//! `parec` to capture whatever's playing via the default sink's PipeWire
//! monitor source, and pipes it to the connected client via
//! `rdpcore_rdpsnd`.
//!
//! No stale-chunk-dropping hack here (an earlier, ironrdp-based version
//! of this bridge needed one): that workaround existed because
//! `ironrdp-server`'s shared write path had no real scheduling, so audio
//! was throttled by throwing data away under contention.
//! `rdpcore-transport`'s scheduler (see its crate docs) fixes the actual
//! problem - a latency-priority frame always gets written within one bulk
//! fragment's worth of delay - so this bridge just sends every chunk and
//! trusts the scheduler.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use rdpcore_rdpsnd::pdu::{AudioFormat, NegotiatedFormat};
use rdpcore_rdpsnd::{RdpsndError, RdpsndServerHandler, RdpsndServerMessage, SoundServerFactory};
use tokio::sync::mpsc::UnboundedSender;

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u16 = 2;
const BITS_PER_SAMPLE: u16 = 16;
const BLOCK_ALIGN: u16 = CHANNELS * (BITS_PER_SAMPLE / 8);
// 20ms chunks: small enough to feel live, large enough not to spam the channel.
const CHUNK_MS: u32 = 20;
const CHUNK_BYTES: usize = (SAMPLE_RATE * BLOCK_ALIGN as u32 / 1000 * CHUNK_MS) as usize;

fn pcm_format() -> AudioFormat {
    AudioFormat::pcm(CHANNELS, SAMPLE_RATE, BITS_PER_SAMPLE)
}

/// Kill (if still running) and `wait()` so the child cannot linger as a zombie.
fn reap_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Stateless factory: each connection gets its own backend and sender.
#[derive(Clone, Default)]
pub struct LocalAudioFactory;

impl LocalAudioFactory {
    pub fn new() -> Self {
        Self
    }
}

impl SoundServerFactory for LocalAudioFactory {
    fn build_backend(
        &self,
        sender: UnboundedSender<RdpsndServerMessage>,
    ) -> Box<dyn RdpsndServerHandler> {
        Box::new(LocalAudioHandler {
            sender,
            formats: vec![pcm_format()],
            child: Arc::new(Mutex::new(None)),
            capture: None,
        })
    }
}

struct LocalAudioHandler {
    sender: UnboundedSender<RdpsndServerMessage>,
    formats: Vec<AudioFormat>,
    child: Arc<Mutex<Option<Child>>>,
    /// Joined in [`Self::stop`] so renegotiation / disconnect cannot leave
    /// orphaned capture threads (visible as growing `Threads:` in `/proc`).
    capture: Option<JoinHandle<()>>,
}

impl core::fmt::Debug for LocalAudioHandler {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LocalAudioHandler").finish_non_exhaustive()
    }
}

impl Drop for LocalAudioHandler {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_capture(sender: UnboundedSender<RdpsndServerMessage>, child: Arc<Mutex<Option<Child>>>) {
    let spawned = Command::new("parec")
        .args([
            "-d",
            "@DEFAULT_SINK@.monitor",
            "--format=s16le",
            &format!("--rate={SAMPLE_RATE}"),
            &format!("--channels={CHANNELS}"),
            "--raw",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();

    let mut child_proc = match spawned {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("parec spawn failed: {e}");
            return;
        }
    };
    let Some(mut stdout) = child_proc.stdout.take() else {
        reap_child(&mut child_proc);
        return;
    };
    *child.lock().unwrap() = Some(child_proc);

    let mut buf = [0u8; CHUNK_BYTES];
    let mut timestamp_ms: u32 = 0;
    loop {
        if stdout.read_exact(&mut buf).is_err() {
            break; // EOF/error - `parec` exited (killed by `stop()`, or itself failed).
        }
        if sender
            .send(RdpsndServerMessage::Wave(buf.to_vec(), timestamp_ms))
            .is_err()
        {
            break; // server side gone.
        }
        timestamp_ms = timestamp_ms.wrapping_add(CHUNK_MS);
    }

    // If `stop()` already took the Child, this is a no-op. Otherwise parec
    // exited on its own and we still must wait() to avoid a zombie.
    if let Some(mut child_proc) = child.lock().unwrap().take() {
        reap_child(&mut child_proc);
    }
}

impl RdpsndServerHandler for LocalAudioHandler {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn choose_format(&mut self, common: &[NegotiatedFormat]) -> Option<NegotiatedFormat> {
        common.first().cloned()
    }

    fn start(&mut self, _format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        // Guacamole (and some clients) can renegotiate / restart the wave
        // stream; never leave a previous parec or capture thread running.
        self.stop();
        let sender = self.sender.clone();
        let child = Arc::clone(&self.child);
        self.capture = Some(std::thread::spawn(move || run_capture(sender, child)));
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            reap_child(&mut child);
        }
        if let Some(handle) = self.capture.take() {
            let _ = handle.join();
        }
    }
}
