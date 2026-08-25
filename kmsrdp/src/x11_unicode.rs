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

/// The topmost keycodes are conventionally left spare by keyboard layouts
/// for exactly this kind of remap trick.
///
/// Number of usable spare keycodes below `max`, down to `min`, is
/// typically far more than the pool size - but on a layout with fewer
/// free keycodes than `SCRATCH_KEYCODE_POOL_SIZE`, wrap around that
/// smaller range instead of letting every slot past it collapse onto
/// `min` - that would defeat the pool's whole point (see its doc comment)
/// far more than necessary given how many keycodes are actually available.
fn scratch_keycode_pool(min: u8, max: u8) -> [u8; SCRATCH_KEYCODE_POOL_SIZE as usize] {
    let span = max.saturating_sub(min).saturating_add(1).max(1);
    let mut pool = [0u8; SCRATCH_KEYCODE_POOL_SIZE as usize];
    for (i, slot) in pool.iter_mut().enumerate() {
        *slot = max.saturating_sub((i as u8) % span);
    }
    pool
}

pub fn unicode_to_keysym(codepoint: u32) -> u32 {
    // Standard X11 keysym conversion: Latin-1 (0x20..=0xFF) map 1:1,
    // while higher Unicode codepoints use the 0x01000000 + codepoint offset.
    if codepoint <= 0x00ff {
        codepoint
    } else {
        0x0100_0000 + codepoint
    }
}

impl X11Connection {
    fn open(display: &str) -> io::Result<Self> {
        let (conn, screen_num) = x11rb::connect(Some(display))
            .map_err(|e| io::Error::other(format!("X11 connect failed on {display}: {e}")))?;
        let setup = conn.setup();
        let root = setup.roots[screen_num].root;
        let scratch_keycodes = scratch_keycode_pool(setup.min_keycode, setup.max_keycode);
        Ok(Self {
            conn,
            scratch_keycodes,
            next_slot: 0,
            root,
        })
    }

    fn type_char(&mut self, codepoint: u32) -> io::Result<()> {
        let keysym = unicode_to_keysym(codepoint);
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

        // Immediately blank this keycode back to NoSymbol. The race
        // documented above assumes a lost race renders nothing because the
        // stale mapping is empty - true only if every prior use of this
        // slot cleaned up after itself. Left mapped to the last real
        // character instead, a *future* race loss on this same slot (they
        // rotate through only SCRATCH_KEYCODE_POOL_SIZE keycodes) doesn't
        // silently drop a character - it retypes whatever was last typed
        // through this slot, e.g. a previous 'x' reappearing on unrelated
        // IME input. Best-effort: this keycode already did its job either
        // way, so a failure here doesn't invalidate the character just sent.
        self.conn
            .change_keyboard_mapping(1, keycode, 1, &[0])
            .map_err(|e| io::Error::other(format!("ChangeKeyboardMapping (clear) failed: {e}")))?;
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
///
/// `type_char` does several synchronous X11 round-trips plus a fixed 30ms
/// sleep (see its comment) - too slow to run inline on the shared input
/// path, which every connected RDP session's keyboard input funnels
/// through via a single `Mutex<Input>` (see `bin/rdp_server.rs`'s
/// `SharedInput`). A burst of IME-composed characters would otherwise
/// stall every other session's mouse/keyboard input for the sleep
/// duration. [`X11UnicodeTyper`] below runs this on a dedicated thread
/// instead, the same pattern `nvfbc.rs` uses for its OpenGL-context-bound
/// capture calls.
struct X11UnicodeWorker {
    session_rx: watch::Receiver<Option<Session>>,
    conn: Option<X11Connection>,
}

impl X11UnicodeWorker {
    fn new(session_rx: watch::Receiver<Option<Session>>) -> Self {
        Self {
            session_rx,
            conn: None,
        }
    }

    /// Inject `codepoint` into the current X11 session.
    ///
    /// Silently does nothing if there is no X11 session (Wayland-only or no
    /// active session). Reconnects automatically when the session changes.
    fn type_char(&mut self, codepoint: u32) {
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

/// Handle to a dedicated background thread that owns the actual X11
/// connection and performs the (slow, blocking) character injection.
/// `type_char` here is a cheap, non-blocking channel send, safe to call
/// from the shared input-handling path.
pub struct X11UnicodeTyper {
    tx: std::sync::mpsc::Sender<u32>,
}

impl X11UnicodeTyper {
    pub fn spawn(session_rx: watch::Receiver<Option<Session>>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<u32>();
        std::thread::spawn(move || {
            let mut worker = X11UnicodeWorker::new(session_rx);
            for codepoint in rx {
                worker.type_char(codepoint);
            }
        });
        Self { tx }
    }

    /// Enqueues `codepoint` for injection on the dedicated X11 worker
    /// thread. Never blocks the caller; silently drops the character if
    /// the worker thread has terminated.
    pub fn type_char(&self, codepoint: u32) {
        let _ = self.tx.send(codepoint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::sync::watch;

    #[test]
    fn scratch_keycode_pool_is_all_distinct_when_span_covers_it() {
        // Plenty of spare keycodes (typical real keyboards): every slot
        // gets its own keycode, same as before this had a wraparound.
        let pool = scratch_keycode_pool(8, 255);
        let mut sorted = pool.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), pool.len(), "expected all-distinct slots");
        assert_eq!(pool[0], 255);
    }

    #[test]
    fn scratch_keycode_pool_wraps_instead_of_collapsing_on_a_tiny_layout() {
        // Only 3 free keycodes (253, 254, 255) - fewer than the pool size.
        // Every one of them must still show up, rather than 5 of 8 slots
        // collapsing onto a single keycode.
        let pool = scratch_keycode_pool(253, 255);
        for keycode in [253u8, 254, 255] {
            assert!(
                pool.contains(&keycode),
                "expected {keycode} to appear somewhere in the pool"
            );
        }
    }

    #[test]
    fn scratch_keycode_pool_handles_a_single_free_keycode() {
        let pool = scratch_keycode_pool(255, 255);
        assert!(pool.iter().all(|&k| k == 255));
    }

    #[test]
    fn unicode_to_keysym_latin1_and_extended() {
        assert_eq!(unicode_to_keysym(0x41), 0x41); // 'A'
        assert_eq!(unicode_to_keysym(0xE9), 0xE9); // 'é' (Latin-1)
        assert_eq!(unicode_to_keysym(0x20AC), 0x0100_0000 + 0x20AC); // '€' (beyond Latin-1)
    }

    fn session_rx(session: Option<Session>) -> watch::Receiver<Option<Session>> {
        let (_, rx) = watch::channel(session);
        rx
    }

    fn sample_session(display: Option<&str>) -> Session {
        Session {
            uid: 1000,
            username: "alice".to_string(),
            display: display.map(str::to_owned),
            xauthority: None,
            xdg_runtime_dir: PathBuf::from("/run/user/1000"),
        }
    }

    #[test]
    fn type_char_without_session_is_noop() {
        let mut typer = X11UnicodeWorker::new(session_rx(None));
        typer.type_char('A' as u32);
    }

    #[test]
    fn type_char_wayland_session_without_display_is_noop() {
        let mut typer = X11UnicodeWorker::new(session_rx(Some(sample_session(None))));
        typer.type_char('A' as u32);
    }

    #[test]
    fn type_char_invalid_display_does_not_panic() {
        let mut typer = X11UnicodeWorker::new(session_rx(Some(sample_session(Some(":254")))));
        typer.type_char('A' as u32);
    }

    #[test]
    fn session_change_clears_cached_connection() {
        // Use non-existent display numbers (:254, :255) so the test never connects
        // to a live X11 desktop and never injects keystrokes into the user's screen.
        let (tx, rx) = watch::channel(Some(sample_session(Some(":254"))));
        let mut typer = X11UnicodeWorker::new(rx);
        typer.type_char('x' as u32);
        tx.send(Some(sample_session(Some(":255"))))
            .expect("session switch");
        typer.type_char('y' as u32);
    }

    #[test]
    fn spawned_typer_type_char_does_not_block_caller() {
        // No X11 session at all, so the worker thread's type_char is an
        // immediate no-op - this just exercises that `type_char` on the
        // public handle is a plain channel send, not the worker itself.
        let typer = X11UnicodeTyper::spawn(session_rx(None));
        typer.type_char('z' as u32);
    }
}
