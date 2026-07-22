//! RDPSND bridge for the from-scratch `rdpcore-*` stack: captures the
//! default sink monitor via the PulseAudio/PipeWire client library
//! (`libpulse-simple`) and pipes PCM to the connected client through
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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use libpulse_binding as pulse;
use libpulse_simple_binding as psimple;
use pulse::sample::{Format, Spec};
use pulse::stream::Direction;
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
/// PulseAudio monitor source for the default playback sink.
const MONITOR_SOURCE: &str = "@DEFAULT_MONITOR@";

fn pcm_format() -> AudioFormat {
    AudioFormat::pcm(CHANNELS, SAMPLE_RATE, BITS_PER_SAMPLE)
}

fn capture_spec() -> Spec {
    Spec {
        format: Format::S16NE,
        channels: CHANNELS as u8,
        rate: SAMPLE_RATE,
    }
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
            stop: Arc::new(AtomicBool::new(false)),
            capture: None,
        })
    }
}

struct LocalAudioHandler {
    sender: UnboundedSender<RdpsndServerMessage>,
    formats: Vec<AudioFormat>,
    stop: Arc<AtomicBool>,
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

fn run_capture(sender: UnboundedSender<RdpsndServerMessage>, stop: Arc<AtomicBool>) {
    let spec = capture_spec();
    if !spec.is_valid() {
        tracing::warn!("kmsrdp: invalid PulseAudio capture spec");
        return;
    }

    let simple = match psimple::Simple::new(
        None,
        "kmsrdp",
        Direction::Record,
        Some(MONITOR_SOURCE),
        "RDP audio capture",
        &spec,
        None,
        None,
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("kmsrdp: PulseAudio capture connect failed: {e}");
            return;
        }
    };

    let mut buf = [0u8; CHUNK_BYTES];
    let mut timestamp_ms: u32 = 0;
    while !stop.load(Ordering::Acquire) {
        match simple.read(&mut buf) {
            Ok(()) => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                if sender
                    .send(RdpsndServerMessage::Wave(buf.to_vec(), timestamp_ms))
                    .is_err()
                {
                    break;
                }
                timestamp_ms = timestamp_ms.wrapping_add(CHUNK_MS);
            }
            Err(e) => {
                tracing::warn!("kmsrdp: PulseAudio capture read failed: {e}");
                break;
            }
        }
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
        // stream; never leave a previous capture thread running.
        self.stop();
        self.stop.store(false, Ordering::Release);
        let sender = self.sender.clone();
        let stop = Arc::clone(&self.stop);
        self.capture = Some(std::thread::spawn(move || run_capture(sender, stop)));
        Ok(())
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.capture.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn chunk_byte_size_matches_twenty_ms_pcm() {
        // 48000 Hz * 4 bytes/frame / 1000 ms * 20 ms
        assert_eq!(CHUNK_BYTES, 3840);
    }

    #[test]
    fn capture_spec_is_valid_pcm() {
        let spec = capture_spec();
        assert!(spec.is_valid());
        assert_eq!(spec.format, Format::S16NE);
        assert_eq!(spec.channels, CHANNELS as u8);
        assert_eq!(spec.rate, SAMPLE_RATE);
    }

    #[test]
    fn handler_advertises_stereo_pcm_48khz() {
        let factory = LocalAudioFactory::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let handler = factory.build_backend(tx);
        let formats = handler.get_formats();
        assert_eq!(formats.len(), 1);
        assert_eq!(formats[0].n_samples_per_sec, SAMPLE_RATE);
        assert_eq!(formats[0].n_channels, CHANNELS);
        assert_eq!(formats[0].bits_per_sample, BITS_PER_SAMPLE);
    }

    #[test]
    fn choose_format_prefers_first_common_format() {
        let factory = LocalAudioFactory::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut handler = factory.build_backend(tx);
        let common = vec![
            NegotiatedFormat {
                format: AudioFormat::pcm(1, 44100, 16),
                format_no: 0,
            },
            NegotiatedFormat {
                format: pcm_format(),
                format_no: 1,
            },
        ];
        let chosen = handler.choose_format(&common).expect("format");
        assert_eq!(chosen.format_no, 0);
    }

    #[test]
    fn drop_without_capture_does_not_panic() {
        let factory = LocalAudioFactory::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let handler = factory.build_backend(tx);
        drop(handler);
    }
}
