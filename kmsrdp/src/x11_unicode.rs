//! Best-effort Unicode text injection for RDP's `UnicodePressed` events
//! (IME-composed text), which have no evdev/uinput equivalent: Linux
//! keycodes are fundamentally scancode-based, not codepoint-based. On an X11
//! session we can use the same trick `xdotool type` uses - temporarily remap
//! a spare keycode's keysym via `ChangeKeyboardMapping`, then press/release
//! it with XTest.
//!
//! This only works because this desktop session happens to be X11
//! (`XDG_SESSION_TYPE=x11`); Wayland has no equivalent client-side keymap
//! remap API. That's also why upstream ReFrame's own keysym-to-keycode
//! lookup (`rf_vnc_server_handle_keysym_event`) only covers whatever key is
//! statically present in the compiled XKB keymap and silently drops
//! anything else - which in practice means it can't type CJK either.

use std::io;

use tokio::sync::watch;
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::session::Session;

// X11 core protocol event codes (X11/X.h); x11rb doesn't name these.
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;

/// How many spare keycodes to rotate through for consecutive characters.
/// One keycode alone races IME-committed multi-character bursts: character
/// N+1's `ChangeKeyboardMapping` can land before the target app has
/// processed character N's `MappingNotify` and cached keymap refresh, so
/// the app ends up translating N's `KeyPress` against N+1's (or a later)
/// keysym - an entirely unrelated character comes out. Spreading
/// consecutive characters across several keycodes gives each one's mapping
/// room to settle in the target app before its keycode is reused.
const SCRATCH_KEYCODE_POOL_SIZE: u8 = 8;

struct X11Connection {
    conn: RustConnection,
    scratch_keycodes: [u8; SCRATCH_KEYCODE_POOL_SIZE as usize],
    next_slot: usize,
    root: u32,
}

impl X11Connection {
    fn open(display: &str) -> io::Result<Self> {
        let (conn, screen_num) = x11rb::connect(Some(display))
            .map_err(|e| io::Error::other(format!("X11 connect failed on {display}: {e}")))?;
        let setup = conn.setup();
        let root = setup.roots[screen_num].root;
        // The topmost keycodes are conventionally left spare by keyboard
        // layouts for exactly this kind of remap trick.
        let highest = setup.max_keycode;
        let mut scratch_keycodes = [0u8; SCRATCH_KEYCODE_POOL_SIZE as usize];
        for (i, slot) in scratch_keycodes.iter_mut().enumerate() {
            *slot = highest.saturating_sub(i as u8).max(setup.min_keycode);
        }
        Ok(Self {
            conn,
            scratch_keycodes,
            next_slot: 0,
            root,
        })
    }

    fn type_char(&mut self, codepoint: u32) -> io::Result<()> {
        // ICCCM/XKB convention: keysyms for codepoints outside Latin-1 are
        // `0x01000000 + codepoint`.
        let keysym = 0x0100_0000 + codepoint;
        let keycode = self.scratch_keycodes[self.next_slot];
        self.next_slot = (self.next_slot + 1) % self.scratch_keycodes.len();

        self.conn
            .change_keyboard_mapping(1, keycode, 1, &[keysym])
            .map_err(|e| io::Error::other(format!("ChangeKeyboardMapping failed: {e}")))?;
        // `sync()` only guarantees the *server* has applied the new mapping
        // - every other client (the app we're about to "type" into
        // included) gets notified async via MappingNotify and has to
        // refresh its own cached keymap before it'll translate the
        // upcoming keycode correctly. Pressing immediately after `sync()`
        // races that refresh: the X-protocol side succeeds (this is
        // exactly why XTestFakeInput reports success below) but the
        // target app still had its stale mapping cached when the KeyPress
        // arrived, so nothing renders - a well-known gotcha with this
        // exact keymap-remap trick (`xdotool type` works around it the
        // same way). A short, imperceptible-for-one-character delay gives
        // well-behaved clients time to process MappingNotify first.
        self.conn
            .sync()
            .map_err(|e| io::Error::other(format!("sync failed: {e}")))?;
        std::thread::sleep(std::time::Duration::from_millis(30));

        self.conn
            .xtest_fake_input(KEY_PRESS, keycode, 0, self.root, 0, 0, 0)
            .map_err(|e| io::Error::other(format!("XTestFakeInput press failed: {e}")))?;
        self.conn
            .xtest_fake_input(KEY_RELEASE, keycode, 0, self.root, 0, 0, 0)
            .map_err(|e| io::Error::other(format!("XTestFakeInput release failed: {e}")))?;
        self.conn
            .flush()
            .map_err(|e| io::Error::other(format!("flush failed: {e}")))?;
        Ok(())
    }
}

/// Per-input-handler X11 connection manager.
///
/// Maintains a single X11 connection for Unicode character injection and
/// automatically reconnects when the active session changes (new `DISPLAY`).
/// Holds a [`watch::Receiver`] so it can detect session changes
/// synchronously from the input handler (which is called on the async
/// executor without an `await` point).
pub struct X11UnicodeTyper {
    session_rx: watch::Receiver<Option<Session>>,
    conn: Option<X11Connection>,
}

impl X11UnicodeTyper {
    pub fn new(session_rx: watch::Receiver<Option<Session>>) -> Self {
        Self {
            session_rx,
            conn: None,
        }
    }

    /// Inject `codepoint` into the current X11 session.
    ///
    /// Silently does nothing if there is no X11 session (Wayland-only or no
    /// active session). Reconnects automatically when the session changes.
    pub fn type_char(&mut self, codepoint: u32) {
        // Reconnect if the session has changed since last call.
        if self.session_rx.has_changed().unwrap_or(false) {
            self.conn = None;
            // Mark as seen so we don't re-enter this branch until the next change.
            let _ = self.session_rx.borrow_and_update();
        }

        // Lazily open a connection for the current session's DISPLAY.
        if self.conn.is_none() {
            let session = self.session_rx.borrow();
            let display = match session.as_ref().and_then(|s| s.display.as_deref()) {
                Some(d) => d.to_owned(),
                None => return, // Wayland-only or no session
            };
            // XAUTHORITY is already set in process env by session_watcher::apply_session_env.
            match X11Connection::open(&display) {
                Ok(c) => self.conn = Some(c),
                Err(e) => {
                    tracing::warn!("kmsrdp: X11 connect failed: {e}");
                    return;
                }
            }
        }

        if let Some(ref mut conn) = self.conn
            && let Err(e) = conn.type_char(codepoint)
        {
            tracing::warn!("kmsrdp: unicode injection failed for U+{codepoint:04X}: {e}");
            self.conn = None; // Force reconnect on next call.
        }
    }
}
