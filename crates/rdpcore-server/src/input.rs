//! Input event traits, shaped after `ironrdp-server`'s so existing
//! `RdpServerInputHandler` impls (like kmsrdp's, driving a `uinput`
//! virtual device) port with only import-path changes.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardEvent {
    Pressed {
        code: u8,
        extended: bool,
    },
    Released {
        code: u8,
        extended: bool,
    },
    /// A `TS_UNICODE_KEYBOARD_EVENT` key-down (`rdpcore_pdu::fastpath`
    /// drops the paired key-up, which carries no useful information for
    /// this event type) - a single UTF-16 code unit, for CJK/IME text
    /// input. Fire-once by design: handlers should treat this as "type
    /// this character now", not track it as a held key the way
    /// `Pressed`/`Released` are.
    UnicodePressed(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    Move { x: u16, y: u16 },
    LeftPressed,
    LeftReleased,
    RightPressed,
    RightReleased,
    MiddlePressed,
    MiddleReleased,
    VerticalScroll { value: i32 },
    HorizontalScroll { value: i32 },
}

pub trait RdpServerInputHandler: Send {
    fn keyboard(&mut self, event: KeyboardEvent);
    fn mouse(&mut self, event: MouseEvent);

    /// Called when an RDP connection ends, for any reason (clean
    /// disconnect, network drop, client crash). Implementations that
    /// inject input into a persistent device (e.g. `uinput`) should
    /// release any key/button this connection left physically "down" -
    /// a client can disconnect mid-keypress (before the matching
    /// `Released` arrives), and without this the device would otherwise
    /// report that key held forever, which e.g. X11's key-repeat then
    /// turns into the key retyping itself indefinitely.
    ///
    /// Default no-op so existing implementations still compile.
    fn reset(&mut self) {}
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum HeldButton {
    Left,
    Right,
    Middle,
}

/// Per-connection wrapper around a shared input handler. Tracks keys and
/// buttons this connection pressed so [`RdpServerInputHandler::reset`]
/// releases only those, not holds belonging to another session.
pub(crate) struct ConnectionScopedInput {
    inner: Arc<Mutex<dyn RdpServerInputHandler>>,
    pressed_keys: HashSet<(u8, bool)>,
    pressed_buttons: HashSet<HeldButton>,
}

impl ConnectionScopedInput {
    pub(crate) fn new(inner: Arc<Mutex<dyn RdpServerInputHandler>>) -> Self {
        Self {
            inner,
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
        }
    }
}

impl RdpServerInputHandler for ConnectionScopedInput {
    fn keyboard(&mut self, event: KeyboardEvent) {
        match event {
            KeyboardEvent::Pressed { code, extended } => {
                self.pressed_keys.insert((code, extended));
            }
            KeyboardEvent::Released { code, extended } => {
                self.pressed_keys.remove(&(code, extended));
            }
            KeyboardEvent::UnicodePressed(_) => {}
        }
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keyboard(event);
    }

    fn mouse(&mut self, event: MouseEvent) {
        match event {
            MouseEvent::LeftPressed => {
                self.pressed_buttons.insert(HeldButton::Left);
            }
            MouseEvent::LeftReleased => {
                self.pressed_buttons.remove(&HeldButton::Left);
            }
            MouseEvent::RightPressed => {
                self.pressed_buttons.insert(HeldButton::Right);
            }
            MouseEvent::RightReleased => {
                self.pressed_buttons.remove(&HeldButton::Right);
            }
            MouseEvent::MiddlePressed => {
                self.pressed_buttons.insert(HeldButton::Middle);
            }
            MouseEvent::MiddleReleased => {
                self.pressed_buttons.remove(&HeldButton::Middle);
            }
            MouseEvent::Move { .. }
            | MouseEvent::VerticalScroll { .. }
            | MouseEvent::HorizontalScroll { .. } => {}
        }
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mouse(event);
    }

    fn reset(&mut self) {
        let keys: Vec<(u8, bool)> = self.pressed_keys.drain().collect();
        let buttons: Vec<HeldButton> = self.pressed_buttons.drain().collect();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for (code, extended) in keys {
            inner.keyboard(KeyboardEvent::Released { code, extended });
        }
        for button in buttons {
            inner.mouse(match button {
                HeldButton::Left => MouseEvent::LeftReleased,
                HeldButton::Right => MouseEvent::RightReleased,
                HeldButton::Middle => MouseEvent::MiddleReleased,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingHandler {
        keys: Vec<KeyboardEvent>,
        mice: Vec<MouseEvent>,
        reset_calls: usize,
    }

    impl RdpServerInputHandler for RecordingHandler {
        fn keyboard(&mut self, event: KeyboardEvent) {
            self.keys.push(event);
        }
        fn mouse(&mut self, event: MouseEvent) {
            self.mice.push(event);
        }
        fn reset(&mut self) {
            self.reset_calls += 1;
        }
    }

    #[test]
    fn reset_releases_only_this_connection_holds() {
        let shared = Arc::new(Mutex::new(RecordingHandler::default()));
        let dyn_shared: Arc<Mutex<dyn RdpServerInputHandler>> = shared.clone();
        let mut a = ConnectionScopedInput::new(Arc::clone(&dyn_shared));
        let mut b = ConnectionScopedInput::new(dyn_shared);

        a.keyboard(KeyboardEvent::Pressed {
            code: 0x1E,
            extended: false,
        });
        b.keyboard(KeyboardEvent::Pressed {
            code: 0x1F,
            extended: false,
        });
        a.mouse(MouseEvent::LeftPressed);
        b.mouse(MouseEvent::RightPressed);

        a.reset();

        let rec = shared.lock().unwrap();
        assert_eq!(rec.reset_calls, 0, "must not reset the shared handler");
        assert!(rec.keys.contains(&KeyboardEvent::Released {
            code: 0x1E,
            extended: false,
        }));
        assert!(!rec.keys.contains(&KeyboardEvent::Released {
            code: 0x1F,
            extended: false,
        }));
        assert!(rec.mice.contains(&MouseEvent::LeftReleased));
        assert!(!rec.mice.contains(&MouseEvent::RightReleased));
    }
}
