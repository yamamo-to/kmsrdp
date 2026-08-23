use rdpcore_pdu::fastpath::{FastPathInputEvent, keyboard_flags};

use crate::input::{KeyboardEvent, MouseEvent, RdpServerInputHandler};

pub fn dispatch_input_event(input: &mut dyn RdpServerInputHandler, event: FastPathInputEvent) {
    match event {
        FastPathInputEvent::Scancode { flags, code } => {
            let extended = flags & (keyboard_flags::EXTENDED | keyboard_flags::EXTENDED1) != 0;
            input.keyboard(if flags & keyboard_flags::RELEASE != 0 {
                KeyboardEvent::Released { code, extended }
            } else {
                KeyboardEvent::Pressed { code, extended }
            });
        }
        FastPathInputEvent::Mouse {
            pointer_flags,
            x,
            y,
        } => {
            input.mouse(translate_mouse(pointer_flags, x, y));
        }
        FastPathInputEvent::Sync { .. } => {}
        FastPathInputEvent::Unicode { flags, code } => {
            if flags & keyboard_flags::RELEASE == 0 {
                input.keyboard(KeyboardEvent::UnicodePressed(code));
            }
        }
    }
}

pub fn translate_mouse(pointer_flags: u16, x: u16, y: u16) -> MouseEvent {
    const WHEEL_NEGATIVE: u16 = 0x0100;
    const VERTICAL_WHEEL: u16 = 0x0200;
    const HORIZONTAL_WHEEL: u16 = 0x0400;
    const LEFT_BUTTON: u16 = 0x1000;
    const RIGHT_BUTTON: u16 = 0x2000;
    const MIDDLE_BUTTON: u16 = 0x4000;
    const DOWN: u16 = 0x8000;

    if pointer_flags & VERTICAL_WHEEL != 0 {
        let raw = i32::from(pointer_flags & 0xFF);
        let value = if pointer_flags & WHEEL_NEGATIVE != 0 {
            raw - 256
        } else {
            raw
        };
        return MouseEvent::VerticalScroll { value };
    }
    if pointer_flags & HORIZONTAL_WHEEL != 0 {
        let raw = i32::from(pointer_flags & 0xFF);
        let value = if pointer_flags & WHEEL_NEGATIVE != 0 {
            raw - 256
        } else {
            raw
        };
        return MouseEvent::HorizontalScroll { value };
    }
    let down = pointer_flags & DOWN != 0;
    if pointer_flags & LEFT_BUTTON != 0 {
        return if down {
            MouseEvent::LeftPressed
        } else {
            MouseEvent::LeftReleased
        };
    }
    if pointer_flags & RIGHT_BUTTON != 0 {
        return if down {
            MouseEvent::RightPressed
        } else {
            MouseEvent::RightReleased
        };
    }
    if pointer_flags & MIDDLE_BUTTON != 0 {
        return if down {
            MouseEvent::MiddlePressed
        } else {
            MouseEvent::MiddleReleased
        };
    }
    MouseEvent::Move { x, y }
}
