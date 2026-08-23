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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Cap on text written into the host clipboard from a client. The CLIPRDR
/// reassembly budget is 16 MiB; this is a tighter bound so a client cannot
/// dump that much into X11/Wayland.
const MAX_HOST_CLIPBOARD_BYTES: usize = 1024 * 1024;

/// Startup Format Lists from macOS Windows App arrive in a burst; debounce
/// paste requests so we do not overlap CLIPRDR channel setup. After this
/// window, later Format Lists (real copy events) are forwarded again.
const PASTE_DEBOUNCE: Duration = Duration::from_secs(2);

fn set_local_text(text: String) {
    if text.len() > MAX_HOST_CLIPBOARD_BYTES {
        tracing::warn!(
            len = text.len(),
            max = MAX_HOST_CLIPBOARD_BYTES,
            "dropping client clipboard: exceeds host size cap"
        );
        return;
    }
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

/// Process-wide clipboard watcher. Prefers XFixes SelectionNotify when an
/// X11 display is available; otherwise falls back to a slow poll so
/// Wayland-only sessions still work.
fn spawn_shared_clipboard_watcher(
    subscribers: Arc<Mutex<Vec<UnboundedSender<ClipboardMessage>>>>,
    mut session_rx: watch::Receiver<Option<Session>>,
) {
    tokio::spawn(async move {
        let mut last = local_text();
        let mut xfixes_stop = Arc::new(AtomicBool::new(false));
        let mut xfixes_active =
            start_xfixes_watch(Arc::clone(&subscribers), Arc::clone(&xfixes_stop));
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {
                    let any = {
                        let mut subs = subscribers.lock().unwrap_or_else(|e| e.into_inner());
                        subs.retain(|s| !s.is_closed());
                        !subs.is_empty()
                    };
                    if !any || xfixes_active.load(Ordering::Relaxed) {
                        continue;
                    }
                    let current = tokio::task::spawn_blocking(local_text).await.unwrap_or(None);
                    if current != last && matches!(&current, Some(t) if !t.is_empty()) {
                        let mut subs = subscribers.lock().unwrap_or_else(|e| e.into_inner());
                        subs.retain(advertise_unicode_formats);
                    }
                    last = current;
                }
                changed = session_rx.changed() => {
                    if changed.is_err() {
                        xfixes_stop.store(true, Ordering::SeqCst);
                        break;
                    }
                    last = None;
                    xfixes_stop.store(true, Ordering::SeqCst);
                    xfixes_stop = Arc::new(AtomicBool::new(false));
                    xfixes_active =
                        start_xfixes_watch(Arc::clone(&subscribers), Arc::clone(&xfixes_stop));
                }
            }
        }
    });
}

fn start_xfixes_watch(
    subscribers: Arc<Mutex<Vec<UnboundedSender<ClipboardMessage>>>>,
    stop: Arc<AtomicBool>,
) -> Arc<AtomicBool> {
    let active = Arc::new(AtomicBool::new(false));
    let active_for_thread = Arc::clone(&active);
    let _ = std::thread::Builder::new()
        .name("kmsrdp-clip-xfixes".into())
        .spawn(
            move || match xfixes_selection_loop(&subscribers, &stop, &active_for_thread) {
                Ok(()) => {}
                Err(e) => {
                    active_for_thread.store(false, Ordering::SeqCst);
                    tracing::debug!(error = %e, "XFixes clipboard watch unavailable; using poll");
                }
            },
        );
    active
}

fn xfixes_selection_loop(
    subscribers: &Mutex<Vec<UnboundedSender<ClipboardMessage>>>,
    stop: &AtomicBool,
    active: &AtomicBool,
) -> std::io::Result<()> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::Event;
    use x11rb::protocol::xfixes::{self, SelectionEventMask};
    use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, WindowClass};

    let (conn, screen_num) =
        x11rb::connect(None).map_err(|e| std::io::Error::other(format!("X11 connect: {e}")))?;
    let screen = &conn.setup().roots[screen_num];
    xfixes::query_version(&conn, 5, 0)
        .map_err(|e| std::io::Error::other(format!("XFixes query: {e}")))?
        .reply()
        .map_err(|e| std::io::Error::other(format!("XFixes query reply: {e}")))?;
    let clipboard = conn
        .intern_atom(false, b"CLIPBOARD")
        .map_err(|e| std::io::Error::other(format!("intern CLIPBOARD: {e}")))?
        .reply()
        .map_err(|e| std::io::Error::other(format!("intern CLIPBOARD reply: {e}")))?
        .atom;
    let win = conn.generate_id().map_err(std::io::Error::other)?;
    conn.create_window(
        0,
        win,
        screen.root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )
    .map_err(std::io::Error::other)?;
    xfixes::select_selection_input(
        &conn,
        win,
        clipboard,
        SelectionEventMask::SET_SELECTION_OWNER
            | SelectionEventMask::SELECTION_WINDOW_DESTROY
            | SelectionEventMask::SELECTION_CLIENT_CLOSE,
    )
    .map_err(std::io::Error::other)?;
    conn.flush().map_err(std::io::Error::other)?;
    active.store(true, Ordering::SeqCst);

    use std::os::unix::io::AsRawFd as _;

    while !stop.load(Ordering::Relaxed) {
        match conn.poll_for_event() {
            Ok(Some(Event::XfixesSelectionNotify(_))) => {
                let mut subs = subscribers.lock().unwrap_or_else(|e| e.into_inner());
                subs.retain(|s| !s.is_closed());
                if !subs.is_empty() {
                    let current = local_text();
                    if matches!(&current, Some(t) if !t.is_empty()) {
                        subs.retain(advertise_unicode_formats);
                    }
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                // Wait on the X11 connection fd with a timeout so we don't busy-loop/wake every 50ms,
                // while still periodically checking the `stop` flag.
                let mut pfd = libc::pollfd {
                    fd: conn.stream().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                unsafe {
                    libc::poll(&mut pfd, 1, 500);
                }
            }
            Err(e) => {
                active.store(false, Ordering::SeqCst);
                return Err(std::io::Error::other(e));
            }
        }
    }
    active.store(false, Ordering::SeqCst);
    Ok(())
}

/// Clipboard synchronization mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClipboardMode {
    /// Bidirectional: host clipboard is shared to client, client clipboard is written to host.
    #[default]
    Bidirectional,
    /// Host to client only (read-only): client receives host clipboard, but cannot write to host.
    HostToClient,
    /// Client to host only: client can write to host clipboard, but host clipboard is not advertised.
    ClientToHost,
    /// CLIPRDR is not offered to the client.
    Disabled,
}

impl ClipboardMode {
    pub fn allows_host_to_client(self) -> bool {
        matches!(self, Self::Bidirectional | Self::HostToClient)
    }

    pub fn allows_client_to_host(self) -> bool {
        matches!(self, Self::Bidirectional | Self::ClientToHost)
    }

    pub fn is_disabled(self) -> bool {
        matches!(self, Self::Disabled)
    }
}

/// Factory for creating local CLIPRDR clipboard backends.
#[derive(Clone)]
pub struct LocalClipboardFactory {
    subscribers: Arc<Mutex<Vec<UnboundedSender<ClipboardMessage>>>>,
    mode: ClipboardMode,
}

impl LocalClipboardFactory {
    pub fn new(session_rx: watch::Receiver<Option<Session>>, mode: ClipboardMode) -> Self {
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        if mode.allows_host_to_client() {
            spawn_shared_clipboard_watcher(subscribers.clone(), session_rx);
        }
        Self { subscribers, mode }
    }
}

impl CliprdrBackendFactory for LocalClipboardFactory {
    fn build_cliprdr_backend(
        &self,
        sender: UnboundedSender<ClipboardMessage>,
    ) -> Box<dyn CliprdrBackend> {
        if self.mode.allows_host_to_client() {
            self.subscribers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(sender.clone());
        }
        Box::new(LocalClipboardBackend {
            sender,
            mode: self.mode,
            remote_has_text: false,
            delay_first_paste: true,
            last_paste_at: None,
        })
    }
}

struct LocalClipboardBackend {
    sender: UnboundedSender<ClipboardMessage>,
    mode: ClipboardMode,
    remote_has_text: bool,
    /// First paste is delayed (see [`PASTE_DEBOUNCE`]); later ones go out
    /// immediately after the debounce window.
    delay_first_paste: bool,
    last_paste_at: Option<Instant>,
}

impl LocalClipboardBackend {
    /// `Some(delay)` when a Format Data Request should be sent; `None` when
    /// this advertisement is inside the debounce window or has no unicode.
    fn paste_delay(&mut self) -> Option<Duration> {
        if !self.remote_has_text {
            return None;
        }
        if let Some(at) = self.last_paste_at
            && at.elapsed() < PASTE_DEBOUNCE
        {
            return None;
        }
        self.last_paste_at = Some(Instant::now());
        if self.delay_first_paste {
            self.delay_first_paste = false;
            Some(PASTE_DEBOUNCE)
        } else {
            Some(Duration::ZERO)
        }
    }
}

impl core::fmt::Debug for LocalClipboardBackend {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LocalClipboardBackend")
            .field("mode", &self.mode)
            .field("remote_has_text", &self.remote_has_text)
            .finish()
    }
}

impl CliprdrBackend for LocalClipboardBackend {
    fn on_ready(&mut self) {
        if self.mode.allows_host_to_client() {
            let _ = advertise_local_text(&self.sender);
        }
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        if !self.mode.allows_client_to_host() {
            return;
        }
        self.remote_has_text = available_formats.iter().any(|f| f.id == CF_UNICODETEXT);
        let Some(delay) = self.paste_delay() else {
            return;
        };
        let sender = self.sender.clone();
        if delay.is_zero() {
            let _ = sender.send(ClipboardMessage::SendInitiatePaste(CF_UNICODETEXT));
        } else {
            // Pulling the remote clipboard immediately during CLIPRDR startup
            // overlaps channel setup on macOS Windows App and has been observed
            // to coincide with abrupt disconnects. Delay the first paste request.
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = sender.send(ClipboardMessage::SendInitiatePaste(CF_UNICODETEXT));
            });
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        if !self.mode.allows_host_to_client() || request.format != CF_UNICODETEXT {
            let _ = self.sender.send(ClipboardMessage::SendFormatData(
                FormatDataResponse::new_error(),
            ));
            return;
        }
        let sender = self.sender.clone();
        let execute = move || {
            let response = match local_text() {
                Some(text) => FormatDataResponse::new_unicode_string(&text),
                None => FormatDataResponse::new_error(),
            };
            let _ = sender.send(ClipboardMessage::SendFormatData(response));
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(execute);
        } else {
            std::thread::spawn(execute);
        }
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse) {
        if !self.mode.allows_client_to_host() || response.is_error() {
            return;
        }
        if let Some(text) = response.to_unicode_string() {
            let execute = move || {
                set_local_text(text);
            };
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn_blocking(execute);
            } else {
                std::thread::spawn(execute);
            }
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
        let factory = LocalClipboardFactory::new(session_rx(), ClipboardMode::default());
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, _rx2) = mpsc::unbounded_channel();
        let mut b1 = factory.build_cliprdr_backend(tx1);
        let mut b2 = factory.build_cliprdr_backend(tx2);
        b1.on_ready();
        b2.on_ready();
    }

    #[tokio::test]
    async fn unknown_format_request_returns_error_response() {
        let factory = LocalClipboardFactory::new(session_rx(), ClipboardMode::default());
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
        let factory = LocalClipboardFactory::new(session_rx(), ClipboardMode::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut backend = factory.build_cliprdr_backend(tx);
        backend.on_format_data_response(FormatDataResponse::new_error());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn remote_copy_without_unicode_skips_paste() {
        let factory = LocalClipboardFactory::new(session_rx(), ClipboardMode::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut backend = factory.build_cliprdr_backend(tx);
        backend.on_remote_copy(&[ClipboardFormat { id: 1 }]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn host_to_client_mode_ignores_remote_copy() {
        let factory = LocalClipboardFactory::new(session_rx(), ClipboardMode::HostToClient);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut backend = factory.build_cliprdr_backend(tx);
        backend.on_remote_copy(&[ClipboardFormat::unicode_text()]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn client_to_host_mode_rejects_format_data_request() {
        let factory = LocalClipboardFactory::new(session_rx(), ClipboardMode::ClientToHost);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut backend = factory.build_cliprdr_backend(tx);
        backend.on_format_data_request(FormatDataRequest {
            format: CF_UNICODETEXT,
        });
        match rx.try_recv().expect("expected response") {
            ClipboardMessage::SendFormatData(resp) => assert!(resp.is_error()),
            _ => panic!("expected SendFormatData"),
        }
    }

    fn backend(
        mode: ClipboardMode,
    ) -> (
        LocalClipboardBackend,
        mpsc::UnboundedReceiver<ClipboardMessage>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            LocalClipboardBackend {
                sender: tx,
                mode,
                remote_has_text: false,
                delay_first_paste: true,
                last_paste_at: None,
            },
            rx,
        )
    }

    #[test]
    fn paste_delay_debounces_then_allows_later_copies() {
        let (mut backend, _rx) = backend(ClipboardMode::Bidirectional);
        backend.remote_has_text = true;
        assert_eq!(backend.paste_delay(), Some(PASTE_DEBOUNCE));
        assert_eq!(backend.paste_delay(), None);
        backend.last_paste_at = Some(Instant::now() - PASTE_DEBOUNCE);
        assert_eq!(backend.paste_delay(), Some(Duration::ZERO));
    }

    #[test]
    fn paste_delay_skips_when_no_unicode() {
        let (mut backend, _rx) = backend(ClipboardMode::Bidirectional);
        assert_eq!(backend.paste_delay(), None);
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
