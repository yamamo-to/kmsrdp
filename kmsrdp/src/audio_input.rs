//! Bridges MS-RDPEAI-captured audio (the RDP client's microphone) into a
//! virtual PipeWire/PulseAudio microphone source other local applications
//! can select as their input device - the mirror image of `audio.rs`'s
//! `parec`-from-monitor-source bridge for playback.
//!
//! Approach: create a null sink (`pactl load-module module-null-sink`)
//! once per user session; its `.monitor` is what shows up as a selectable
//! microphone. Each connection's backend pipes its negotiated-format PCM
//! into that sink via `paplay`, spawned lazily once the format is known.
//! Writing to the child's stdin happens on a dedicated OS thread (not the
//! async connection task) since it's blocking I/O, mirroring how
//! `audio.rs` keeps its own blocking `parec` read off the async path.
//!
//! Session awareness: `XDG_RUNTIME_DIR` is kept up-to-date in the process
//! environment by [`crate::session_watcher`].  The null sink is initialized
//! per session UID so that each user's PulseAudio instance gets its own
//! virtual microphone.

use std::collections::HashSet;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{LazyLock, Mutex};

use rdpcore_rdpeai::pdu::AudioFormat;
use rdpcore_rdpeai::{AudioInputBackend, AudioInputBackendFactory};

const SINK_NAME: &str = "kmsrdp_mic";

static INITIALIZED_UIDS: LazyLock<Mutex<HashSet<u32>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Ensure the kmsrdp null sink exists in the current session's PulseAudio.
///
/// Tracks which UIDs have been initialized so subsequent calls are cheap.
/// Uses the process environment for `XDG_RUNTIME_DIR` (kept current by
/// `session_watcher::apply_session_env`).
fn ensure_null_sink_loaded(uid: u32) {
    {
        if INITIALIZED_UIDS.lock().unwrap().contains(&uid) {
            return;
        }
    }

    let already_loaded = Command::new("pactl")
        .args(["list", "short", "sinks"])
        .output()
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.contains(SINK_NAME))
        })
        .unwrap_or(false);

    if !already_loaded {
        let result = Command::new("pactl")
            .args([
                "load-module",
                "module-null-sink",
                &format!("sink_name={SINK_NAME}"),
                &format!("sink_properties=device.description={SINK_NAME}"),
            ])
            .output();
        match result {
            Ok(output) if output.status.success() => {
                println!(
                    "kmsrdp: virtual microphone ready (uid={uid}) - \
                     select '{SINK_NAME}.monitor' as a microphone input"
                );
            }
            Ok(output) => eprintln!(
                "kmsrdp: failed to create virtual microphone sink: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(e) => eprintln!("kmsrdp: pactl unavailable, virtual microphone won't work: {e}"),
        }
    }

    INITIALIZED_UIDS.lock().unwrap().insert(uid);
}

pub struct VirtualMicFactory;

impl VirtualMicFactory {
    pub fn new() -> Self {
        if let Some(uid) = current_session_uid() {
            ensure_null_sink_loaded(uid);
        }
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

fn spawn_writer(format: &AudioFormat) -> Option<Sender<Vec<u8>>> {
    // Ensure null sink exists for the current session before spawning paplay.
    if let Some(uid) = current_session_uid() {
        ensure_null_sink_loaded(uid);
    }

    let spawned = Command::new("paplay")
        .args([
            "--raw",
            &format!("--rate={}", format.n_samples_per_sec),
            &format!("--channels={}", format.n_channels),
            "--format=s16le",
            &format!("--device={SINK_NAME}"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    let mut child: Child = match spawned {
        Ok(child) => child,
        Err(e) => {
            eprintln!("kmsrdp: failed to start paplay for virtual microphone: {e}");
            return None;
        }
    };
    let mut stdin = child.stdin.take()?;

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        for chunk in rx {
            if stdin.write_all(&chunk).is_err() {
                break;
            }
        }
        let _ = child.kill();
    });
    Some(tx)
}

impl AudioInputBackend for VirtualMicBackend {
    fn on_audio_data(&mut self, format: &AudioFormat, data: &[u8]) {
        if self.tx.is_none() {
            self.tx = spawn_writer(format);
        }
        if let Some(tx) = &self.tx {
            if tx.send(data.to_vec()).is_err() {
                self.tx = None; // writer thread/child died - respawn on next chunk
            }
        }
    }
}
