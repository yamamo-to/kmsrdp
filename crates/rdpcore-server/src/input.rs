//! Input event traits, shaped after `ironrdp-server`'s so existing
//! `RdpServerInputHandler` impls (like kmsrdp's, driving a `uinput`
//! virtual device) port with only import-path changes.

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
