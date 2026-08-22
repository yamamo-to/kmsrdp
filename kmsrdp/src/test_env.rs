//! Process-wide lock for tests that call `set_var` / `remove_var`.
//!
//! Each module used to have its own `Mutex`, so parallel `cargo test`
//! threads could race on the process environment. Every test that mutates
//! env vars should take this lock.

use std::sync::{Mutex, MutexGuard, OnceLock};

pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
