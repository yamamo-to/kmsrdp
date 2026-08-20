//! RDPSND bridge for the from-scratch `rdpcore-*` stack: captures the
//! default sink monitor via the PulseAudio/PipeWire client library
//! (`libpulse-simple`) and pipes PCM to the connected client through
//! `rdpcore_rdpsnd`.
//!
//! Capture publishes into a latest-wins slot ([`WavePublisher`]). When the
//! session loop stalls (e.g. synchronous GFX encode during video), older
//! PCM is overwritten instead of queued, so A/V lag cannot accumulate.
//! Pulse/PipeWire `fragsize`/`maxlength` are still kept small (~20–80 ms)
//! so the monitor source itself does not add multi-second buffering.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use libpulse_binding as pulse;
use libpulse_simple_binding as psimple;
use pulse::def::BufferAttr;
use pulse::sample::{Format, Spec};
use pulse::stream::Direction;
use rdpcore_rdpsnd::pdu::{AudioFormat, NegotiatedFormat};
use rdpcore_rdpsnd::{
    RdpsndError, RdpsndServerHandler, RdpsndServerMessage, SoundServerFactory, WavePublisher,
};

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u16 = 2;
const BITS_PER_SAMPLE: u16 = 16;
const BLOCK_ALIGN: u16 = CHANNELS * (BITS_PER_SAMPLE / 8);
// 20ms chunks: small enough to feel live, large enough not to spam the channel.
const CHUNK_MS: u32 = 20;
const CHUNK_BYTES: usize = (SAMPLE_RATE * BLOCK_ALIGN as u32 / 1000 * CHUNK_MS) as usize;
/// Cap Pulse monitor buffering to a few chunks (matches former queue depth).
const PULSE_MAX_CHUNKS: usize = 4;
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

/// Low-latency record attributes for monitor capture.
///
/// Passing `None` to `Simple::new` leaves PulseAudio/PipeWire defaults, and
/// `fragsize` defaults to roughly **2 seconds** of audio — which shows up as
/// multi-second A/V lag on clients such as macOS Windows App. Match the
/// fragment size to our 20 ms wave chunks and cap the buffer to a few chunks.
fn capture_buffer_attr() -> BufferAttr {
    BufferAttr {
        maxlength: (CHUNK_BYTES * PULSE_MAX_CHUNKS) as u32,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: CHUNK_BYTES as u32,
    }
}

/// Stateless factory: each connection gets its own backend and publisher.
#[derive(Clone, Default)]
pub struct LocalAudioFactory;

impl LocalAudioFactory {
    pub fn new() -> Self {
        Self
    }
}

impl SoundServerFactory for LocalAudioFactory {
    fn build_backend(&self, publisher: WavePublisher) -> Box<dyn RdpsndServerHandler> {
        Box::new(LocalAudioHandler {
            publisher,
            formats: vec![pcm_format()],
            stop: Arc::new(AtomicBool::new(false)),
            capture: None,
        })
    }
}

struct LocalAudioHandler {
    publisher: WavePublisher,
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

fn run_capture(publisher: WavePublisher, stop: Arc<AtomicBool>) {
    let spec = capture_spec();
    if !spec.is_valid() {
        tracing::warn!("kmsrdp: invalid PulseAudio capture spec");
        return;
    }

    let attr = capture_buffer_attr();
    let simple = match psimple::Simple::new(
        None,
        "kmsrdp",
        Direction::Record,
        Some(MONITOR_SOURCE),
        "RDP audio capture",
        &spec,
        None,
        Some(&attr),
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
                // Overwrite any unread Wave so the session always sees the
                // newest PCM after a GFX stall (never a FIFO of late audio).
                if !publisher.publish(RdpsndServerMessage::Wave(buf.to_vec(), timestamp_ms)) {
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
        let publisher = self.publisher.clone();
        let stop = Arc::clone(&self.stop);
        self.capture = Some(std::thread::spawn(move || run_capture(publisher, stop)));
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
    use rdpcore_rdpsnd::wave_channel;

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
    fn capture_buffer_attr_targets_twenty_ms_fragments() {
        let attr = capture_buffer_attr();
        assert_eq!(attr.fragsize, CHUNK_BYTES as u32);
        assert_eq!(attr.maxlength, (CHUNK_BYTES * PULSE_MAX_CHUNKS) as u32);
    }

    #[test]
    fn handler_advertises_stereo_pcm_48khz() {
        let factory = LocalAudioFactory::new();
        let (tx, _rx) = wave_channel();
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
        let (tx, _rx) = wave_channel();
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
        let (tx, _rx) = wave_channel();
        let handler = factory.build_backend(tx);
        drop(handler);
    }

    #[test]
    fn publisher_keeps_only_newest_wave() {
        let (tx, rx) = wave_channel();
        assert!(tx.publish(RdpsndServerMessage::Wave(vec![1], 0)));
        assert!(tx.publish(RdpsndServerMessage::Wave(vec![2], 20)));
        assert!(tx.publish(RdpsndServerMessage::Wave(vec![3], 40)));
        match rx.take_latest() {
            Some(RdpsndServerMessage::Wave(pcm, ts)) => {
                assert_eq!(pcm, vec![3]);
                assert_eq!(ts, 40);
            }
            other => panic!("expected newest wave, got {other:?}"),
        }
        assert!(rx.take_latest().is_none());
    }
}
