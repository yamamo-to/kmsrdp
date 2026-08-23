//! RDPSND bridge for the from-scratch `rdpcore-*` stack: captures the
//! default sink monitor via the PulseAudio/PipeWire client library
//! (`libpulse-simple`) and pipes PCM to the connected client through
//! `rdpcore_rdpsnd`.
//!
//! Capture publishes into a latest-wins slot ([`WavePublisher`]). When the
//! session loop stalls (e.g. synchronous GFX encode during video), older
//! PCM is overwritten instead of queued. Publish rate is also capped to
//! wall-clock realtime: a 20 ms chunk that arrives early is dropped so
//! the RDP client's playout FIFO cannot grow without bound. Dropouts
//! under load are the intended trade for live remote-desktop audio.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use libpulse_binding as pulse;
use libpulse_simple_binding as psimple;
use pulse::def::BufferAttr;
use pulse::sample::{Format, Spec};
use pulse::stream::Direction;
use rdpcore_rdpsnd::pdu::{AudioFormat, NegotiatedFormat};
use rdpcore_rdpsnd::{RdpsndServerHandler, RdpsndServerMessage, SoundServerFactory, WavePublisher};

const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u16 = 2;
const BITS_PER_SAMPLE: u16 = 16;
const BLOCK_ALIGN: u16 = CHANNELS * (BITS_PER_SAMPLE / 8);
const CHUNK_MS: u32 = 20;
const CHUNK_BYTES: usize = (SAMPLE_RATE * BLOCK_ALIGN as u32 / 1000 * CHUNK_MS) as usize;
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
/// `fragsize` is one 20 ms chunk. `maxlength` is capped at a few chunks so
/// PipeWire/Pulse cannot accumulate seconds of monitor data (their default
/// `maxlength` is large enough for ~10–20 s at 48 kHz stereo).
fn capture_buffer_attr() -> BufferAttr {
    BufferAttr {
        maxlength: (CHUNK_BYTES * 4) as u32,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: CHUNK_BYTES as u32,
    }
}

/// A `pa_simple` read that returns this quickly still had buffered data.
const DRAIN_FAST_READ: Duration = Duration::from_millis(4);
/// Cap drain iterations per cycle so a huge backlog cannot starve `stop`.
const MAX_DRAIN_CHUNKS: usize = 15;

/// After one `read()`, decide whether `buf` is live enough to publish.
///
/// Immediate reads mean Pulse still has a backlog. Keep discarding until a
/// read blocks (~one fragment) so we publish the live edge, not 10 s of
/// queued monitor PCM. Hitting [`MAX_DRAIN_CHUNKS`] without blocking means
/// we are still behind — skip publish and drain again next cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureAction {
    DrainMore,
    SkipPublish,
    Publish,
}

fn after_capture_read(waited: Duration, drained: usize) -> CaptureAction {
    if waited < DRAIN_FAST_READ {
        if drained < MAX_DRAIN_CHUNKS {
            CaptureAction::DrainMore
        } else {
            CaptureAction::SkipPublish
        }
    } else {
        CaptureAction::Publish
    }
}

/// How far sent PCM may lead wall time before we drop a chunk.
const MAX_AHEAD_MS: u64 = 40;
/// If capture stalled this long, snap the send budget to now (no burst catch-up).
const SNAP_BEHIND_MS: u64 = 80;

/// Decide whether one [`CHUNK_MS`] of PCM may go to the client.
///
/// Returns the updated `sent_ms` after sending, or `None` to drop. Clients
/// such as mstsc play every sample they receive and never skip, so sending
/// even slightly faster than realtime makes delay grow for the whole
/// session. After a stall, snap to `elapsed_ms` instead of bursting the
/// missed duration into the client's FIFO.
fn pcm_send_budget(sent_ms: u64, elapsed_ms: u64) -> Option<u64> {
    let sent_ms = if elapsed_ms > sent_ms.saturating_add(SNAP_BEHIND_MS) {
        elapsed_ms
    } else {
        sent_ms
    };
    if sent_ms.saturating_add(u64::from(CHUNK_MS)) <= elapsed_ms.saturating_add(MAX_AHEAD_MS) {
        Some(sent_ms.saturating_add(u64::from(CHUNK_MS)))
    } else {
        None
    }
}

/// Hint PipeWire/Pulse toward ~20 ms before any client library connects.
/// Safe to call more than once; does not override an explicit user value.
pub fn hint_low_latency_audio() {
    if std::env::var_os("PIPEWIRE_LATENCY").is_none() {
        unsafe {
            std::env::set_var("PIPEWIRE_LATENCY", "960/48000");
        }
    }
    if std::env::var_os("PULSE_LATENCY_MSEC").is_none() {
        unsafe {
            std::env::set_var("PULSE_LATENCY_MSEC", "20");
        }
    }
}

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
        tracing::warn!("kmsrdp: invalid PulseAudio capture spec: {spec:?}");
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

    if let Ok(lat) = simple.get_latency() {
        tracing::info!("kmsrdp: RDPSND pulse record initial latency: {lat:?}");
    }

    let start_instant = std::time::Instant::now();
    let mut sent_ms = 0u64;
    let mut buf = [0u8; CHUNK_BYTES];

    while !stop.load(Ordering::Acquire) {
        let t0 = std::time::Instant::now();
        match simple.read(&mut buf) {
            Ok(()) => {
                if stop.load(Ordering::Acquire) {
                    break;
                }

                let mut waited = t0.elapsed();
                let mut drained = 0;
                let mut action = after_capture_read(waited, drained);
                while action == CaptureAction::DrainMore {
                    let t1 = std::time::Instant::now();
                    if simple.read(&mut buf).is_err() {
                        return;
                    }
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    waited = t1.elapsed();
                    drained += 1;
                    action = after_capture_read(waited, drained);
                }
                if action == CaptureAction::SkipPublish {
                    tracing::debug!(
                        drained,
                        "kmsrdp: RDPSND still catching up; not publishing stale PCM"
                    );
                    sent_ms = start_instant.elapsed().as_millis() as u64;
                    continue;
                }

                let elapsed_ms = start_instant.elapsed().as_millis() as u64;
                let Some(next_sent) = pcm_send_budget(sent_ms, elapsed_ms) else {
                    continue;
                };
                sent_ms = next_sent;

                let timestamp_ms = elapsed_ms as u32;
                if !publisher.publish(RdpsndServerMessage::Wave(buf.to_vec(), timestamp_ms)) {
                    break;
                }
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

    fn start(
        &mut self,
        _format: &NegotiatedFormat,
    ) -> Result<(), Box<dyn std::error::Error + Send>> {
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
        assert_eq!(CHUNK_BYTES, 3840);
        let spec = capture_spec();
        assert!(spec.is_valid());
        assert_eq!(spec.format, Format::S16NE);
        assert_eq!(spec.channels, CHANNELS as u8);
        assert_eq!(spec.rate, SAMPLE_RATE);
    }

    #[test]
    fn capture_buffer_attr_sets_fragsize_to_one_chunk() {
        let attr = capture_buffer_attr();
        assert_eq!(attr.fragsize, CHUNK_BYTES as u32);
        assert_eq!(attr.maxlength, (CHUNK_BYTES * 4) as u32);
    }

    #[test]
    fn after_capture_read_drains_fast_backlog_then_publishes_blocked_read() {
        assert_eq!(
            after_capture_read(Duration::from_millis(1), 0),
            CaptureAction::DrainMore
        );
        assert_eq!(
            after_capture_read(Duration::from_millis(1), MAX_DRAIN_CHUNKS),
            CaptureAction::SkipPublish
        );
        assert_eq!(
            after_capture_read(Duration::from_millis(20), 0),
            CaptureAction::Publish
        );
    }

    #[test]
    fn pcm_send_budget_caps_rate_to_realtime_and_does_not_burst_after_stall() {
        assert_eq!(pcm_send_budget(0, 0), Some(u64::from(CHUNK_MS)));
        assert_eq!(pcm_send_budget(200, 50), None);
        // Far behind: snap to now, then allow one live chunk (not the gap).
        assert_eq!(pcm_send_budget(0, 1_000), Some(1_000 + u64::from(CHUNK_MS)));
    }

    #[test]
    fn handler_advertises_stereo_pcm_formats() {
        let factory = LocalAudioFactory::new();
        let (tx, _rx) = wave_channel();
        let handler = factory.build_backend(tx);
        let formats = handler.get_formats();
        assert_eq!(formats.len(), 1);
        assert_eq!(formats[0].n_samples_per_sec, 48000);
        assert_eq!(formats[0].n_channels, 2);
        assert_eq!(formats[0].bits_per_sample, 16);
    }

    #[test]
    fn choose_format_prefers_first_common_format() {
        let factory = LocalAudioFactory::new();
        let (tx, _rx) = wave_channel();
        let mut handler = factory.build_backend(tx);
        let common = vec![NegotiatedFormat {
            format: AudioFormat::pcm(2, 48000, 16),
            format_no: 0,
        }];
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
