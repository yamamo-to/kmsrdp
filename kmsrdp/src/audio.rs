//! RDPSND bridge for the from-scratch `rdpcore-*` stack: captures the
//! default sink monitor via the PulseAudio/PipeWire client library
//! (`libpulse-simple`) and pipes PCM to the connected client through
//! `rdpcore_rdpsnd`.
//!
//! Capture publishes into a latest-wins slot ([`WavePublisher`]). When the
//! session loop stalls (e.g. synchronous GFX encode during video), older
//! PCM is overwritten instead of queued, so A/V lag cannot accumulate.
//! Dropouts under load are the intended trade for live remote-desktop audio.

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
/// `fragsize` is the target latency (~20 ms). Other fields stay at
/// `u32::MAX` ("server default") so they do not fight Pulse's
/// `ADJUST_LATENCY`, which `pa_simple` enables on its own.
fn capture_buffer_attr() -> BufferAttr {
    BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: CHUNK_BYTES as u32,
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

    hint_low_latency_audio();

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
        tracing::info!("kmsrdp: RDPSND pulse record latency: {lat:?}");
    }

    let mut buf = [0u8; CHUNK_BYTES];
    let mut timestamp_ms: u32 = 0;

    while !stop.load(Ordering::Acquire) {
        match simple.read(&mut buf) {
            Ok(()) => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
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
        assert_eq!(attr.maxlength, u32::MAX);
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
