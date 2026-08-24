//! RDPSND bridge: capture the default-sink monitor at the *live edge* and
//! publish 20 ms PCM through `rdpcore_rdpsnd`.
//!
//! Capture is the analogue of latest-frame video:
//! - Pulse stream API (not `pa_simple`), 20 ms `fragsize`, `maxlength` two
//!   chunks. PipeWire often ignores `maxlength`; `flush` drops a daemon
//!   backlog that peek() never shows (it only delivers one fragment).
//! - `get_latency()` on a monitor subtracts `sink_usec` and can read 0
//!   while `write_index - read_index` is seconds. We key off timing_info.
//! - Leftover keeps only the newest 20 ms. Publish is capped to wall-clock
//!   1× with no catch-up burst. Dropouts under load are the live-A/V trade.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use libpulse_binding as pulse;
use pulse::context::{Context, FlagSet as ContextFlagSet};
use pulse::def::BufferAttr;
use pulse::mainloop::standard::{IterateResult, Mainloop};
use pulse::proplist::Proplist;
use pulse::sample::{Format, Spec};
use pulse::stream::{FlagSet as StreamFlagSet, PeekResult, Stream};
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
/// `fragsize` is one 20 ms chunk. `maxlength` is two chunks so PipeWire/Pulse
/// must drop rather than grow a multi-second record buffer (`pulse.default.frag`
/// is 2 s when the client does not pin this).
fn capture_buffer_attr() -> BufferAttr {
    BufferAttr {
        maxlength: (CHUNK_BYTES * 2) as u32,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: CHUNK_BYTES as u32,
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How far sent PCM may lead wall time before we drop a chunk.
const MAX_AHEAD_MS: u64 = 40;
/// If capture stalled this long, snap the send budget to now (no burst catch-up).
const SNAP_BEHIND_MS: u64 = 80;
/// Daemon record queue above this is discarded via `flush` (peek only sees
/// one fragment, so a 4 MB PipeWire buffer would otherwise drain at 1×).
const MAX_PULSE_BUFFER_BYTES: u32 = (CHUNK_BYTES * 2) as u32;

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
    if std::env::var_os("PIPEWIRE_PROPS").is_none() {
        unsafe {
            std::env::set_var("PIPEWIRE_PROPS", "{ node.latency = 960/48000 }");
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

fn iterate_until(
    mainloop: &mut Mainloop,
    stop: &AtomicBool,
    deadline: Instant,
    mut check: impl FnMut() -> Result<bool, ()>,
) -> bool {
    while Instant::now() < deadline {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        match mainloop.iterate(false) {
            IterateResult::Success(0) => std::thread::sleep(Duration::from_millis(10)),
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) | IterateResult::Err(_) => return false,
        }
        match check() {
            Ok(true) => return true,
            Err(()) => return false,
            Ok(false) => {}
        }
    }
    false
}

fn context_is_ready(context: &Context) -> Result<bool, ()> {
    match context.get_state() {
        pulse::context::State::Ready => Ok(true),
        pulse::context::State::Failed | pulse::context::State::Terminated => Err(()),
        _ => Ok(false),
    }
}

fn stream_is_ready(stream: &Stream) -> Result<bool, ()> {
    match stream.get_state() {
        pulse::stream::State::Ready => Ok(true),
        pulse::stream::State::Failed | pulse::stream::State::Terminated => Err(()),
        _ => Ok(false),
    }
}

fn keep_newest_chunk(leftover: &mut Vec<u8>) {
    let align = usize::from(BLOCK_ALIGN);
    let usable = leftover.len() - leftover.len() % align;
    leftover.truncate(usable);
    if leftover.len() > CHUNK_BYTES {
        leftover.drain(..leftover.len() - CHUNK_BYTES);
    }
}

/// Pulls readable fragments and returns the newest complete 20 ms chunk.
fn take_live_chunk(stream: &mut Stream, leftover: &mut Vec<u8>) -> Option<[u8; CHUNK_BYTES]> {
    loop {
        match stream.peek() {
            Ok(PeekResult::Empty) => break,
            Ok(PeekResult::Hole(_)) => {
                let _ = stream.discard();
            }
            Ok(PeekResult::Data(data)) => {
                leftover.extend_from_slice(data);
                let _ = stream.discard();
            }
            Err(_) => return None,
        }
        keep_newest_chunk(leftover);
    }
    if leftover.len() < CHUNK_BYTES {
        return None;
    }
    let mut chunk = [0u8; CHUNK_BYTES];
    chunk.copy_from_slice(&leftover[..CHUNK_BYTES]);
    leftover.clear();
    Some(chunk)
}

fn pulse_queued_bytes(stream: &mut Stream) -> Option<u64> {
    let info = *stream.get_timing_info()?;
    if info.write_index_corrupt != 0 || info.read_index_corrupt != 0 {
        return None;
    }
    Some(info.write_index.abs_diff(info.read_index))
}

fn run_capture(publisher: WavePublisher, stop: Arc<AtomicBool>) {
    let spec = capture_spec();
    if !spec.is_valid() {
        tracing::warn!("kmsrdp: invalid PulseAudio capture spec: {spec:?}");
        return;
    }

    let Some(mut mainloop) = Mainloop::new() else {
        tracing::warn!("kmsrdp: PulseAudio mainloop create failed");
        return;
    };
    let Some(mut context) = Context::new(&mainloop, "kmsrdp-rdpsnd") else {
        tracing::warn!("kmsrdp: PulseAudio context create failed");
        return;
    };
    if context
        .connect(None, ContextFlagSet::NOFLAGS, None)
        .is_err()
    {
        tracing::warn!("kmsrdp: PulseAudio context connect failed");
        return;
    }

    let deadline = Instant::now() + CONNECT_TIMEOUT;
    if !iterate_until(&mut mainloop, &stop, deadline, || {
        context_is_ready(&context)
    }) {
        tracing::warn!("kmsrdp: PulseAudio context not ready for RDPSND capture");
        return;
    }

    let mut stream_props = match Proplist::new() {
        Some(p) => p,
        None => {
            tracing::warn!("kmsrdp: PulseAudio proplist create failed");
            return;
        }
    };
    let _ = stream_props.set_str(pulse::proplist::properties::MEDIA_NAME, "RDP audio capture");
    let _ = stream_props.set_str("node.latency", "960/48000");

    let Some(mut stream) = Stream::new_with_proplist(
        &mut context,
        "RDP audio capture",
        &spec,
        None,
        &mut stream_props,
    ) else {
        tracing::warn!("kmsrdp: PulseAudio stream create failed");
        return;
    };

    let attr = capture_buffer_attr();
    let flags = StreamFlagSet::ADJUST_LATENCY
        | StreamFlagSet::AUTO_TIMING_UPDATE
        | StreamFlagSet::INTERPOLATE_TIMING
        | StreamFlagSet::START_UNMUTED;
    if stream
        .connect_record(Some(MONITOR_SOURCE), Some(&attr), flags)
        .is_err()
    {
        tracing::warn!("kmsrdp: PulseAudio record connect failed");
        return;
    }

    if !iterate_until(
        &mut mainloop,
        &stop,
        Instant::now() + CONNECT_TIMEOUT,
        || stream_is_ready(&stream),
    ) {
        tracing::warn!("kmsrdp: PulseAudio record stream not ready");
        return;
    }

    if let Some(actual) = stream.get_buffer_attr() {
        tracing::debug!(
            maxlength = actual.maxlength,
            fragsize = actual.fragsize,
            requested_maxlength = attr.maxlength,
            requested_fragsize = attr.fragsize,
            "kmsrdp: RDPSND pulse record buffer"
        );
        if actual.maxlength > attr.maxlength.saturating_mul(2)
            || actual.fragsize > attr.fragsize.saturating_mul(2)
        {
            tracing::warn!(
                maxlength = actual.maxlength,
                fragsize = actual.fragsize,
                "kmsrdp: Pulse/PipeWire ignored the 20 ms record buffer; delay may grow"
            );
        }
    }

    // Drop whatever PipeWire already queued (maxlength is often ignored).
    let _flush = stream.flush(None);
    for _ in 0..8 {
        match mainloop.iterate(false) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) | IterateResult::Err(_) => break,
        }
    }

    let start_instant = Instant::now();
    let mut sent_ms = 0u64;
    let mut leftover = Vec::with_capacity(CHUNK_BYTES * 2);
    let mut last_diag = Instant::now();
    let mut published = 0u32;
    let mut budget_drops = 0u32;
    let mut pulse_flushes = 0u32;

    while !stop.load(Ordering::Acquire) {
        match mainloop.iterate(true) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(_) | IterateResult::Err(_) => break,
        }
        if stop.load(Ordering::Acquire) {
            break;
        }

        if pulse_queued_bytes(&mut stream).is_some_and(|n| n > u64::from(MAX_PULSE_BUFFER_BYTES)) {
            leftover.clear();
            let _flush = stream.flush(None);
            pulse_flushes = pulse_flushes.saturating_add(1);
            continue;
        }

        let Some(chunk) = take_live_chunk(&mut stream, &mut leftover) else {
            continue;
        };

        let elapsed_ms = start_instant.elapsed().as_millis() as u64;
        let Some(next_sent) = pcm_send_budget(sent_ms, elapsed_ms) else {
            budget_drops = budget_drops.saturating_add(1);
            continue;
        };
        sent_ms = next_sent;

        let timestamp_ms = elapsed_ms as u32;
        if !publisher.publish(RdpsndServerMessage::Wave(chunk.to_vec(), timestamp_ms)) {
            break;
        }
        published = published.saturating_add(1);

        if last_diag.elapsed() >= Duration::from_secs(1) {
            let pulse_lat_ms = match stream.get_latency() {
                Ok(pulse::stream::Latency::Positive(us)) => Some(us.0 / 1000),
                Ok(pulse::stream::Latency::Negative(_)) => Some(0),
                _ => None,
            };
            let (buffer_ms, source_ms, sink_ms) = match stream.get_timing_info().copied() {
                Some(info) => {
                    let queued = info.write_index.abs_diff(info.read_index);
                    (
                        Some(spec.bytes_to_usec(queued).0 / 1000),
                        Some(info.source_usec.0 / 1000),
                        Some(info.sink_usec.0 / 1000),
                    )
                }
                None => (None, None, None),
            };
            let leftover_ms =
                leftover.len() as u64 * u64::from(CHUNK_MS) / CHUNK_BYTES.max(1) as u64;
            tracing::debug!(
                pulse_lat_ms,
                buffer_ms,
                source_ms,
                sink_ms,
                leftover_ms,
                sent_ms,
                elapsed_ms,
                published,
                budget_drops,
                pulse_flushes,
                "kmsrdp: RDPSND capture 1s (buffer_ms is Pulse record queue; pulse_lat_ms subtracts sink and can hide it)"
            );
            last_diag = Instant::now();
            published = 0;
            budget_drops = 0;
            pulse_flushes = 0;
        }
    }

    let _ = stream.disconnect();
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
        assert_eq!(attr.maxlength, (CHUNK_BYTES * 2) as u32);
    }

    #[test]
    fn leftover_keeps_only_the_newest_chunk_when_over_cap() {
        let mut leftover = vec![0u8; CHUNK_BYTES * 5];
        let last = leftover.len() - 1;
        leftover[last] = 0xAB;
        keep_newest_chunk(&mut leftover);
        assert_eq!(leftover.len(), CHUNK_BYTES);
        assert_eq!(leftover[CHUNK_BYTES - 1], 0xAB);
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
