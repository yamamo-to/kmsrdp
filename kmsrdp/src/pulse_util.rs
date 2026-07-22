//! Shared PulseAudio helpers for in-process RDPSND capture and RDPEAI playback.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context;
use pulse::mainloop::threaded::Mainloop;

/// Null-sink name whose `.monitor` source appears as a selectable microphone.
pub const VIRTUAL_MIC_SINK: &str = "kmsrdp_mic";

const MAX_MAINLOOP_WAITS: u32 = 1_000;

/// Ensure `module-null-sink` named [`VIRTUAL_MIC_SINK`] is loaded.
pub fn ensure_virtual_mic_sink() -> bool {
    let Some(mut mainloop) = Mainloop::new() else {
        return false;
    };
    mainloop.lock();
    if mainloop.start().is_err() {
        mainloop.unlock();
        return false;
    }

    let Some(mut context) = context::Context::new(&mainloop, "kmsrdp") else {
        mainloop.unlock();
        mainloop.stop();
        return false;
    };
    if context
        .connect(None, context::FlagSet::NOFLAGS, None)
        .is_err()
    {
        mainloop.unlock();
        mainloop.stop();
        return false;
    }

    let mut waits = MAX_MAINLOOP_WAITS;
    if !wait_context_ready(&mut mainloop, &context, &mut waits) {
        mainloop.unlock();
        mainloop.stop();
        tracing::warn!("kmsrdp: PulseAudio context not ready for virtual mic sink");
        return false;
    }

    let sink_exists = Arc::new(AtomicBool::new(false));
    let sink_exists_cb = Arc::clone(&sink_exists);
    let lookup_op = {
        let introspector = context.introspect();
        introspector.get_sink_info_by_name(VIRTUAL_MIC_SINK, move |result| {
            if matches!(result, ListResult::Item(_)) {
                sink_exists_cb.store(true, Ordering::Relaxed);
            }
        })
    };
    while !matches!(lookup_op.get_state(), pulse::operation::State::Done) {
        if !mainloop_wait(&mut mainloop, &mut waits) {
            mainloop.unlock();
            mainloop.stop();
            tracing::warn!("kmsrdp: timed out checking for virtual mic sink");
            return false;
        }
    }

    if sink_exists.load(Ordering::Relaxed) {
        mainloop.unlock();
        mainloop.stop();
        return true;
    }

    let loaded = Arc::new(AtomicBool::new(false));
    let loaded_cb = Arc::clone(&loaded);
    let module_args = format!(
        "sink_name={VIRTUAL_MIC_SINK} sink_properties=device.description={VIRTUAL_MIC_SINK}"
    );
    let load_op = {
        let mut introspector = context.introspect();
        introspector.load_module("module-null-sink", &module_args, move |index| {
            if index != u32::MAX {
                loaded_cb.store(true, Ordering::Relaxed);
            }
        })
    };
    while !matches!(load_op.get_state(), pulse::operation::State::Done) {
        if !mainloop_wait(&mut mainloop, &mut waits) {
            mainloop.unlock();
            mainloop.stop();
            tracing::warn!("kmsrdp: timed out loading module-null-sink");
            return false;
        }
    }
    mainloop.unlock();
    mainloop.stop();

    if loaded.load(Ordering::Relaxed) {
        tracing::info!(
            "kmsrdp: virtual microphone ready — select '{VIRTUAL_MIC_SINK}.monitor' as input"
        );
        return true;
    }

    tracing::warn!("kmsrdp: failed to load module-null-sink for virtual microphone");
    false
}

fn wait_context_ready(
    mainloop: &mut Mainloop,
    context: &context::Context,
    waits: &mut u32,
) -> bool {
    loop {
        match context.get_state() {
            context::State::Ready => return true,
            context::State::Failed | context::State::Terminated => return false,
            _ if !mainloop_wait(mainloop, waits) => return false,
            _ => {}
        }
    }
}

/// Decrement the wait budget. Returns `false` when exhausted (timeout).
fn take_wait_budget(waits: &mut u32) -> bool {
    if *waits == 0 {
        return false;
    }
    *waits -= 1;
    true
}

fn mainloop_wait(mainloop: &mut Mainloop, waits: &mut u32) -> bool {
    if !take_wait_budget(waits) {
        return false;
    }
    mainloop.wait();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn take_wait_budget_exhausts() {
        let mut waits = 2;
        assert!(take_wait_budget(&mut waits));
        assert!(take_wait_budget(&mut waits));
        assert!(!take_wait_budget(&mut waits));
        assert_eq!(waits, 0);
    }

    #[test]
    fn ensure_virtual_mic_sink_fails_without_pulse() {
        let _guard = env_lock();
        // Point at a guaranteed-absent socket so context never becomes Ready.
        unsafe {
            std::env::set_var(
                "PULSE_SERVER",
                "unix:/tmp/kmsrdp-pulse-util-test-missing-socket",
            );
            std::env::remove_var("XDG_RUNTIME_DIR");
        }

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(ensure_virtual_mic_sink());
        });
        let ok = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("ensure_virtual_mic_sink must not hang without Pulse");
        assert!(!ok);
    }

    #[test]
    #[ignore = "requires a live PulseAudio/PipeWire session"]
    fn ensure_virtual_mic_sink_loads_when_pulse_available() {
        assert!(
            ensure_virtual_mic_sink(),
            "expected module-null-sink '{VIRTUAL_MIC_SINK}' to load"
        );
        // Second call should find the existing sink and still succeed.
        assert!(ensure_virtual_mic_sink());
    }
}
