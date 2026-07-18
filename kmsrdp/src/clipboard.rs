//! CLIPRDR bridge for the from-scratch `rdpcore-*` stack: an
//! `arboard`-based text-only clipboard backend for `rdpcore_cliprdr`.
//! File/bitmap/locking parts of CLIPRDR are unimplemented - the codec
//! doesn't decode those messages at all yet.
//!
//! Session awareness: arboard reads `DISPLAY`/`XAUTHORITY` from the process
//! environment, which [`crate::session_watcher`] keeps up-to-date.  When the
//! active session changes the polling watcher resets its state so the next
//! poll creates a fresh arboard connection to the new session.

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
        return sender
            .send(ClipboardMessage::SendInitiateCopy(vec![
                ClipboardFormat::unicode_text(),
            ]))
            .is_ok();
    }
    true
}

/// Polls the local clipboard for changes (no notify API in `arboard`) and
/// advertises new text content to the remote as it appears.
///
/// Reacts to session changes by resetting state, forcing a re-advertisement
/// with the new session's clipboard once it reconnects. Stops when the
/// connection's sender is dropped.
fn spawn_local_clipboard_watcher(
    sender: UnboundedSender<ClipboardMessage>,
    mut session_rx: watch::Receiver<Option<Session>>,
) {
    tokio::spawn(async move {
        let mut last = local_text();
        loop {
            // Exit as soon as the connection drops its receiver. Previously we
            // only noticed on a failed send (clipboard text change), so after
            // Guacamole disconnect the watcher kept spawning blocking polls.
            if sender.is_closed() {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(750)) => {
                    if sender.is_closed() {
                        break;
                    }
                    let current = tokio::task::spawn_blocking(local_text).await.unwrap_or(None);
                    if current != last && matches!(&current, Some(t) if !t.is_empty())
                        && !advertise_local_text(&sender) {
                            break;
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

/// Stateless factory: each connection gets its own backend, sender, and
/// clipboard watcher.
#[derive(Clone)]
pub struct LocalClipboardFactory {
    session_rx: watch::Receiver<Option<Session>>,
}

impl LocalClipboardFactory {
    pub fn new(session_rx: watch::Receiver<Option<Session>>) -> Self {
        Self { session_rx }
    }
}

impl CliprdrBackendFactory for LocalClipboardFactory {
    fn build_cliprdr_backend(
        &self,
        sender: UnboundedSender<ClipboardMessage>,
    ) -> Box<dyn CliprdrBackend> {
        spawn_local_clipboard_watcher(sender.clone(), self.session_rx.clone());
        Box::new(LocalClipboardBackend {
            sender,
            remote_has_text: false,
        })
    }
}

struct LocalClipboardBackend {
    sender: UnboundedSender<ClipboardMessage>,
    remote_has_text: bool,
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
        if self.remote_has_text {
            let _ = self
                .sender
                .send(ClipboardMessage::SendInitiatePaste(CF_UNICODETEXT));
        }
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
