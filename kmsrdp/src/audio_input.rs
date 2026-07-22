//! Bridges MS-RDPEAI-captured audio (the RDP client's microphone) into a
//! virtual PipeWire/PulseAudio microphone source other local applications
//! can select as their input device - the mirror image of `audio.rs`'s
//! in-process monitor capture bridge for playback.
//!
//! A null sink ([`crate::pulse_util::VIRTUAL_MIC_SINK`]) is loaded once per
//! user session via libpulse; its `.monitor` is what shows up as a selectable
//! microphone. Each connection's backend pipes negotiated-format PCM into
//! that sink through an in-process `libpulse-simple` playback stream.
//!
//! Session awareness: `XDG_RUNTIME_DIR` / `PULSE_SERVER` are kept up-to-date
//! in the process environment by [`crate::session_watcher`]. The null sink is
//! initialized per session UID so each user's PulseAudio instance gets its own
//! virtual microphone.

use std::collections::HashSet;
use std::sync::mpsc::Sender;
use std::sync::{LazyLock, Mutex};

use libpulse_binding as pulse;
use libpulse_simple_binding as psimple;
use pulse::sample::{Format, Spec};
use pulse::stream::Direction;
use rdpcore_rdpeai::pdu::AudioFormat;
use rdpcore_rdpeai::{AudioInputBackend, AudioInputBackendFactory};

use crate::pulse_util::{self, VIRTUAL_MIC_SINK};

static INITIALIZED_UIDS: LazyLock<Mutex<HashSet<u32>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Ensure the kmsrdp null sink exists in the current session's PulseAudio.
///
/// Tracks which UIDs have been initialized so subsequent calls are cheap.
fn ensure_null_sink_loaded(uid: u32) {
    ensure_null_sink_loaded_with(uid, pulse_util::ensure_virtual_mic_sink);
}

fn ensure_null_sink_loaded_with(uid: u32, ensure: impl FnOnce() -> bool) {
    if INITIALIZED_UIDS.lock().unwrap().contains(&uid) {
        return;
    }
    let _ = ensure();
    INITIALIZED_UIDS.lock().unwrap().insert(uid);
}

pub struct VirtualMicFactory;

impl VirtualMicFactory {
    pub fn new() -> Self {
        Self
    }
}

impl Default for VirtualMicFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioInputBackendFactory for VirtualMicFactory {
    fn build_backend(&self) -> Box<dyn AudioInputBackend> {
        Box::new(VirtualMicBackend { tx: None })
    }
}

struct VirtualMicBackend {
    tx: Option<Sender<Vec<u8>>>,
}

fn current_session_uid() -> Option<u32> {
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .and_then(|p| p.strip_prefix("/run/user/").and_then(|s| s.parse().ok()))
}

fn playback_spec(format: &AudioFormat) -> Option<Spec> {
    let spec = Spec {
        format: Format::S16NE,
        channels: format.n_channels as u8,
        rate: format.n_samples_per_sec,
    };
    spec.is_valid().then_some(spec)
}

fn spawn_writer(format: &AudioFormat) -> Option<Sender<Vec<u8>>> {
    if let Some(uid) = current_session_uid() {
        ensure_null_sink_loaded(uid);
    }

    let spec = playback_spec(format)?;
    let simple = psimple::Simple::new(
        None,
        "kmsrdp",
        Direction::Playback,
        Some(VIRTUAL_MIC_SINK),
        "RDP microphone",
        &spec,
        None,
        None,
    )
    .inspect_err(|e| {
        tracing::warn!("kmsrdp: PulseAudio playback connect failed for virtual mic: {e}");
    })
    .ok()?;

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        for chunk in rx {
            if simple.write(&chunk).is_err() {
                break;
            }
        }
    });
    Some(tx)
}

impl AudioInputBackend for VirtualMicBackend {
    fn on_audio_data(&mut self, format: &AudioFormat, data: &[u8]) {
        if self.tx.is_none() {
            self.tx = spawn_writer(format);
        }
        if let Some(tx) = &self.tx
            && tx.send(data.to_vec()).is_err()
        {
            self.tx = None; // writer thread died - respawn on next chunk
        }
    }
}

impl Drop for VirtualMicBackend {
    fn drop(&mut self) {
        self.tx = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn current_session_uid_parses_xdg_runtime_dir() {
        let _guard = env_lock();
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", "/run/user/4242");
        }
        assert_eq!(current_session_uid(), Some(4242));
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
        assert_eq!(current_session_uid(), None);
    }

    #[test]
    fn current_session_uid_rejects_non_user_runtime_paths() {
        let _guard = env_lock();
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", "/tmp/not-a-user-dir");
        }
        assert_eq!(current_session_uid(), None);
    }

    #[test]
    fn playback_spec_requires_valid_pcm() {
        let format = AudioFormat::pcm(2, 48_000, 16);
        let spec = playback_spec(&format).expect("valid pcm");
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.rate, 48_000);
    }

    #[test]
    fn playback_spec_rejects_zero_channels() {
        let format = AudioFormat::pcm(0, 48_000, 16);
        assert!(playback_spec(&format).is_none());
    }

    #[test]
    fn playback_spec_rejects_zero_rate() {
        let format = AudioFormat::pcm(1, 0, 16);
        assert!(playback_spec(&format).is_none());
    }

    #[test]
    fn backend_accepts_pcm_without_panicking() {
        let _guard = env_lock();
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
            std::env::remove_var("PULSE_SERVER");
        }
        let factory = VirtualMicFactory::new();
        let mut backend = factory.build_backend();
        let format = AudioFormat::pcm(1, 48_000, 16);
        backend.on_audio_data(&format, &[0u8; 4]);
        backend.on_audio_data(&format, &[0u8; 8]);
    }

    #[test]
    fn spawn_writer_returns_none_for_invalid_format() {
        let _guard = env_lock();
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
            std::env::remove_var("PULSE_SERVER");
        }
        let format = AudioFormat::pcm(0, 48_000, 16);
        assert!(spawn_writer(&format).is_none());
    }

    #[test]
    fn ensure_null_sink_skips_ensure_after_first_uid() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let uid = 9_999_998u32;
        INITIALIZED_UIDS.lock().unwrap().remove(&uid);

        let calls = AtomicUsize::new(0);
        ensure_null_sink_loaded_with(uid, || {
            calls.fetch_add(1, Ordering::Relaxed);
            false
        });
        ensure_null_sink_loaded_with(uid, || {
            calls.fetch_add(1, Ordering::Relaxed);
            true
        });
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(INITIALIZED_UIDS.lock().unwrap().contains(&uid));

        INITIALIZED_UIDS.lock().unwrap().remove(&uid);
    }
}
