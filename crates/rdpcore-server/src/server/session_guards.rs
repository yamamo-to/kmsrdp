//! Drop guards used by the steady-state session loop so cleanup cannot be
//! skipped on early returns, errors, or panics.

use std::sync::{Arc, Mutex};

use crate::error::SessionError;
use crate::input::RdpServerInputHandler;

/// Aborts the wrapped task when dropped, instead of letting it run
/// detached forever after this connection ends.
pub struct AbortOnDrop(pub tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Same as [`AbortOnDrop`], but for a bulk-graphics send that the session
/// loop also awaits. Dropping the handle without abort would detach the
/// task and keep the connection writer alive after the session returns.
#[derive(Default)]
pub(crate) struct AbortHandleOnDrop(
    pub(crate) Option<tokio::task::JoinHandle<Result<(), SessionError>>>,
);

impl Drop for AbortHandleOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

/// Calls `reset()` on the wrapped input handler when dropped, so a
/// connection that ends (normally, on error, or via panic) always
/// releases whatever keys/buttons it was holding.
pub struct ResetInputOnDrop(pub Arc<Mutex<dyn RdpServerInputHandler>>);

impl Drop for ResetInputOnDrop {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).reset();
    }
}
