//! CLIPRDR bridge for the from-scratch `rdpcore-*` stack: an
//! `arboard`-based text-only clipboard backend for `rdpcore_cliprdr`.
//! File/bitmap/locking parts of CLIPRDR are unimplemented - the codec
//! doesn't decode those messages at all yet.
//!
//! Session awareness: arboard reads `DISPLAY`/`XAUTHORITY` from the process
//! environment, which [`crate::session_watcher`] keeps up-to-date.  When the
//! active session changes the polling watcher resets its state so the next
//! poll creates a fresh arboard connection to the new session.
//!
//! Polling is process-wide: one watcher fans out format advertisements to
//! every live RDP connection, so N sessions do not mean N clipboard polls.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rdpcore_cliprdr::pdu::CF_UNICODETEXT;
use rdpcore_cliprdr::{
    ClipboardFormat, ClipboardMessage, CliprdrBackend, CliprdrBackendFactory, FormatDataRequest,
    FormatDataResponse,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use crate::session::Session;

fn local_text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

fn set_local_text(text: String) {
    if let Ok(mut clipboard) = arboard::Clipboard::new() {
        let _ = clipboard.set_text(text);
    }
}

fn advertise_local_text(sender: &UnboundedSender<ClipboardMessage>) -> bool {
    if matches!(local_text(), Some(t) if !t.is_empty()) {
        return advertise_unicode_formats(sender);
    }
    true
}

fn advertise_unicode_formats(sender: &UnboundedSender<ClipboardMessage>) -> bool {
    sender
        .send(ClipboardMessage::SendInitiateCopy(vec![
            ClipboardFormat::unicode_text(),
        ]))
        .is_ok()
}

/// Process-wide poll of the local clipboard. Subscribers are per-connection
/// CLIPRDR senders; closed ones are pruned each tick. Idle (no subscribers)
/// skips `spawn_blocking` so disconnect leaves almost no clipboard cost.
fn spawn_shared_clipboard_watcher(
    subscribers: Arc<Mutex<Vec<UnboundedSender<ClipboardMessage>>>>,
    mut session_rx: watch::Receiver<Option<Session>>,
) {
    tokio::spawn(async move {
        let mut last = local_text();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(750)) => {
                    let any = {
                        let mut subs = subscribers.lock().unwrap();
                        subs.retain(|s| !s.is_closed());
                        !subs.is_empty()
                    };
                    if !any {
                        continue;
                    }
                    let current = tokio::task::spawn_blocking(local_text).await.unwrap_or(None);
                    if current != last && matches!(&current, Some(t) if !t.is_empty()) {
                        let mut subs = subscribers.lock().unwrap();
                        subs.retain(advertise_unicode_formats);
                    }
                    last = current;
                }
                changed = session_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    // Session changed: reset so next poll opens a fresh
                    // arboard connection to the new session's clipboard.
                    last = None;
                }
            }
        }
    });
}

/// Builds per-connection backends that share one process-wide clipboard poller.
#[derive(Clone)]
pub struct LocalClipboardFactory {
    subscribers: Arc<Mutex<Vec<UnboundedSender<ClipboardMessage>>>>,
}

impl LocalClipboardFactory {
    pub fn new(session_rx: watch::Receiver<Option<Session>>) -> Self {
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        spawn_shared_clipboard_watcher(Arc::clone(&subscribers), session_rx);
        Self { subscribers }
    }
}

impl CliprdrBackendFactory for LocalClipboardFactory {
    fn build_cliprdr_backend(
        &self,
        sender: UnboundedSender<ClipboardMessage>,
    ) -> Box<dyn CliprdrBackend> {
        self.subscribers.lock().unwrap().push(sender.clone());
        Box::new(LocalClipboardBackend {
            sender,
            remote_has_text: false,
            paste_requested: false,
        })
    }
}

struct LocalClipboardBackend {
    sender: UnboundedSender<ClipboardMessage>,
    remote_has_text: bool,
    /// Avoid duplicate remote paste requests when the client sends several
    /// Format List PDUs during startup (common on macOS Windows App).
    paste_requested: bool,
}

impl core::fmt::Debug for LocalClipboardBackend {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LocalClipboardBackend")
            .field("remote_has_text", &self.remote_has_text)
            .finish()
    }
}

impl CliprdrBackend for LocalClipboardBackend {
    fn on_ready(&mut self) {
        let _ = advertise_local_text(&self.sender);
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        self.remote_has_text = available_formats.iter().any(|f| f.id == CF_UNICODETEXT);
        if !self.remote_has_text || self.paste_requested {
            return;
        }
        self.paste_requested = true;
        // Pulling the remote clipboard immediately during CLIPRDR startup
        // overlaps channel setup on macOS Windows App and has been observed
        // to coincide with abrupt disconnects. Delay the first paste request.
        let sender = self.sender.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _ = sender.send(ClipboardMessage::SendInitiatePaste(CF_UNICODETEXT));
        });
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let response = if request.format == CF_UNICODETEXT {
            match local_text() {
                Some(text) => FormatDataResponse::new_unicode_string(&text),
                None => FormatDataResponse::new_error(),
            }
        } else {
            FormatDataResponse::new_error()
        };
        let _ = self.sender.send(ClipboardMessage::SendFormatData(response));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse) {
        if response.is_error() {
            return;
        }
        if let Some(text) = response.to_unicode_string() {
            set_local_text(text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    fn session_rx() -> watch::Receiver<Option<Session>> {
        let (_, rx) = watch::channel(None);
        rx
    }

    fn test_session(display: Option<&str>) -> Session {
        Session {
            uid: 1000,
            username: "alice".to_string(),
            display: display.map(str::to_owned),
            xauthority: None,
            xdg_runtime_dir: PathBuf::from("/run/user/1000"),
        }
    }

    #[tokio::test]
    async fn factory_builds_distinct_backends() {
        let factory = LocalClipboardFactory::new(session_rx());
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, _rx2) = mpsc::unbounded_channel();
        let mut b1 = factory.build_cliprdr_backend(tx1);
        let mut b2 = factory.build_cliprdr_backend(tx2);
        b1.on_ready();
        b2.on_ready();
    }

    #[tokio::test]
    async fn unknown_format_request_returns_error_response() {
        let factory = LocalClipboardFactory::new(session_rx());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut backend = factory.build_cliprdr_backend(tx);
        backend.on_format_data_request(FormatDataRequest {
            format: 0xDEAD_BEEF,
        });
        match rx.try_recv().expect("expected format data response") {
            ClipboardMessage::SendFormatData(resp) => assert!(resp.is_error()),
            _ => panic!("expected SendFormatData"),
        }
    }

    #[tokio::test]
    async fn format_data_error_response_is_ignored() {
        let factory = LocalClipboardFactory::new(session_rx());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut backend = factory.build_cliprdr_backend(tx);
        backend.on_format_data_response(FormatDataResponse::new_error());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn remote_copy_without_unicode_skips_paste() {
        let factory = LocalClipboardFactory::new(session_rx());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut backend = factory.build_cliprdr_backend(tx);
        backend.on_remote_copy(&[ClipboardFormat { id: 1 }]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn session_change_resets_watcher_state() {
        let (session_tx, session_rx) = watch::channel(None);
        let subscribers = Arc::new(Mutex::new(Vec::<UnboundedSender<ClipboardMessage>>::new()));
        spawn_shared_clipboard_watcher(Arc::clone(&subscribers), session_rx);
        session_tx
            .send(Some(test_session(Some(":0"))))
            .expect("send session");
        tokio::time::sleep(Duration::from_millis(20)).await;
        session_tx.send(None).expect("clear session");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
