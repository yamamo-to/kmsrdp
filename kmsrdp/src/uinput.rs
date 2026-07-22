//! Virtual keyboard/mouse via `/dev/uinput`, mirroring the device that
//! `reframe-streamer/main.c` creates upstream: absolute pointer axes scaled
//! to `POINTER_MAX` (so the compositor maps the whole axis range onto the
//! virtual desktop, regardless of actual pixel resolution), a handful of
//! mouse buttons, a wheel, and the full keyboard keybit range.

use std::fs::{File, OpenOptions};
use std::io;
use std::mem;
use std::os::unix::io::AsRawFd;
use std::time::Duration;

use uinput_sys as sys;

/// Same convention as upstream `RF_POINTER_MAX` (`INT16_MAX`): callers send
/// fractional (0.0..=1.0) positions, we scale them to this fixed axis range.
pub const POINTER_MAX: i32 = i16::MAX as i32;

/// Same as upstream `RF_KEYBOARD_MAX`: register every keycode in this range
/// as a key this device can send, covering the whole keyboard.
const KEYBOARD_MAX: i32 = 256;

pub const BTN_LEFT: i32 = sys::BTN_LEFT;
pub const BTN_RIGHT: i32 = sys::BTN_RIGHT;
pub const BTN_MIDDLE: i32 = sys::BTN_MIDDLE;

pub struct VirtualInput {
    file: File,
}

fn ioctl_check(ret: libc::c_int, what: &str) -> io::Result<()> {
    if ret < 0 {
        return Err(io::Error::new(
            io::Error::last_os_error().kind(),
            format!("{what} failed: {}", io::Error::last_os_error()),
        ));
    }
    Ok(())
}

impl VirtualInput {
    pub fn create() -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .or_else(|_| OpenOptions::new().write(true).open("/dev/input/uinput"))?;
        let fd = file.as_raw_fd();

        unsafe {
            ioctl_check(sys::ui_set_evbit(fd, sys::EV_SYN), "UI_SET_EVBIT(EV_SYN)")?;
            ioctl_check(sys::ui_set_evbit(fd, sys::EV_KEY), "UI_SET_EVBIT(EV_KEY)")?;
            ioctl_check(sys::ui_set_evbit(fd, sys::EV_ABS), "UI_SET_EVBIT(EV_ABS)")?;
            ioctl_check(sys::ui_set_evbit(fd, sys::EV_REL), "UI_SET_EVBIT(EV_REL)")?;

            for key in 0..KEYBOARD_MAX {
                ioctl_check(sys::ui_set_keybit(fd, key), "UI_SET_KEYBIT(key)")?;
            }
            for &btn in &[
                sys::BTN_LEFT,
                sys::BTN_MIDDLE,
                sys::BTN_RIGHT,
                sys::BTN_SIDE,
                sys::BTN_EXTRA,
            ] {
                ioctl_check(sys::ui_set_keybit(fd, btn), "UI_SET_KEYBIT(button)")?;
            }

            ioctl_check(sys::ui_set_absbit(fd, sys::ABS_X), "UI_SET_ABSBIT(ABS_X)")?;
            ioctl_check(sys::ui_set_absbit(fd, sys::ABS_Y), "UI_SET_ABSBIT(ABS_Y)")?;

            ioctl_check(
                sys::ui_set_relbit(fd, sys::REL_WHEEL),
                "UI_SET_RELBIT(REL_WHEEL)",
            )?;
            ioctl_check(
                sys::ui_set_relbit(fd, sys::REL_HWHEEL),
                "UI_SET_RELBIT(REL_HWHEEL)",
            )?;
        }

        // Legacy `uinput_user_dev` device-creation path (pre Linux-4.5
        // UI_DEV_SETUP/UI_ABS_SETUP), still fully supported today and all
        // this crate's ioctl bindings give us.
        let mut dev: sys::uinput_user_dev = unsafe { mem::zeroed() };
        let name = b"kmsrdp\0";
        dev.name[..name.len()].copy_from_slice(unsafe {
            std::slice::from_raw_parts(name.as_ptr() as *const i8, name.len())
        });
        dev.id.bustype = 0x03; // BUS_USB
        dev.id.vendor = 0xa3a7;
        dev.id.product = 0x0003;
        dev.id.version = 1;
        dev.absmin[sys::ABS_X as usize] = 0;
        dev.absmax[sys::ABS_X as usize] = POINTER_MAX;
        dev.absmin[sys::ABS_Y as usize] = 0;
        dev.absmax[sys::ABS_Y as usize] = POINTER_MAX;

        let dev_bytes = unsafe {
            std::slice::from_raw_parts(
                &dev as *const _ as *const u8,
                mem::size_of::<sys::uinput_user_dev>(),
            )
        };
        let written = unsafe {
            libc::write(
                fd,
                dev_bytes.as_ptr() as *const libc::c_void,
                dev_bytes.len(),
            )
        };
        if written != dev_bytes.len() as isize {
            return Err(io::Error::last_os_error());
        }

        ioctl_check(unsafe { sys::ui_dev_create(fd) }, "UI_DEV_CREATE")?;

        // Give userspace (libinput etc.) a moment to notice the new device
        // before we start sending it events, same as reframe-streamer does.
        std::thread::sleep(Duration::from_secs(1));

        Ok(Self { file })
    }

    fn emit(&self, events: &[(i32, i32, i32)]) -> io::Result<()> {
        let fd = self.file.as_raw_fd();
        for &(kind, code, value) in events {
            let ev = sys::input_event {
                time: unsafe { mem::zeroed() },
                kind: kind as u16,
                code: code as u16,
                value,
            };
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    &ev as *const _ as *const u8,
                    mem::size_of::<sys::input_event>(),
                )
            };
            let written =
                unsafe { libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len()) };
            if written != bytes.len() as isize {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Moves the pointer to a fractional (0.0..=1.0, 0.0..=1.0) position
    /// within the current CRTC/screen.
    pub fn move_abs(&self, fx: f64, fy: f64) -> io::Result<()> {
        self.emit(&[
            (sys::EV_ABS, sys::ABS_X, (POINTER_MAX as f64 * fx) as i32),
            (sys::EV_ABS, sys::ABS_Y, (POINTER_MAX as f64 * fy) as i32),
            (sys::EV_SYN, sys::SYN_REPORT, 0),
        ])
    }

    pub fn click_left(&self) -> io::Result<()> {
        self.button(sys::BTN_LEFT, true)?;
        std::thread::sleep(Duration::from_millis(50));
        self.button(sys::BTN_LEFT, false)
    }

    /// Presses or releases a mouse button (`sys::BTN_LEFT`/`BTN_RIGHT`/...).
    pub fn button(&self, code: i32, down: bool) -> io::Result<()> {
        self.emit(&[
            (sys::EV_KEY, code, down as i32),
            (sys::EV_SYN, sys::SYN_REPORT, 0),
        ])
    }

    /// Presses or releases a raw Linux keycode (`KEY_*` from
    /// `linux/input-event-codes.h`).
    pub fn key(&self, code: i32, down: bool) -> io::Result<()> {
        self.emit(&[
            (sys::EV_KEY, code, down as i32),
            (sys::EV_SYN, sys::SYN_REPORT, 0),
        ])
    }

    /// Vertical wheel: positive scrolls up, negative scrolls down.
    pub fn scroll(&self, delta: i32) -> io::Result<()> {
        self.emit(&[
            (sys::EV_REL, sys::REL_WHEEL, delta.signum()),
            (sys::EV_SYN, sys::SYN_REPORT, 0),
        ])
    }
}

/// Translates an RDP scan_code (`code`/`extended`, as delivered by
/// `ironrdp_server::KeyboardEvent`) to a Linux `KEY_*` code.
///
/// Non-extended codes are a direct pass-through: Linux keycodes were
/// historically defined from the PC/AT scancode set 1 "make" codes, so the
/// base row (letters, digits, F-keys, enter, space, ...) lines up 1:1.
/// Extended (E0-prefixed) keys don't share that property, so this covers
/// only the common navigation/modifier keys; anything else returns `None`.
pub fn linux_keycode_from_rdp_scancode(code: u8, extended: bool) -> Option<i32> {
    if !extended {
        return Some(code as i32);
    }
    Some(match code {
        0x1d => 97,  // Right Ctrl
        0x38 => 100, // Right Alt (AltGr)
        0x1c => 96,  // Numpad Enter
        0x35 => 98,  // Numpad /
        0x48 => 103, // Up
        0x4b => 105, // Left
        0x4d => 106, // Right
        0x50 => 108, // Down
        0x47 => 102, // Home
        0x4f => 107, // End
        0x49 => 104, // Page Up
        0x51 => 109, // Page Down
        0x52 => 110, // Insert
        0x53 => 111, // Delete
        0x5b => 125, // Left Meta/GUI
        0x5c => 126, // Right Meta/GUI
        0x5d => 127, // Menu
        _ => return None,
    })
}

impl Drop for VirtualInput {
    fn drop(&mut self) {
        unsafe {
            sys::ui_dev_destroy(self.file.as_raw_fd());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_scancode_passes_through_unchanged() {
        assert_eq!(linux_keycode_from_rdp_scancode(0x1e, false), Some(0x1e));
    }

    #[test]
    fn extended_arrow_keys_map_to_linux_codes() {
        assert_eq!(linux_keycode_from_rdp_scancode(0x48, true), Some(103)); // Up
        assert_eq!(linux_keycode_from_rdp_scancode(0x50, true), Some(108)); // Down
    }

    #[test]
    fn unknown_extended_scancode_returns_none() {
        assert_eq!(linux_keycode_from_rdp_scancode(0x99, true), None);
    }

    #[test]
    fn fractional_pointer_maps_to_axis_range() {
        let mid = (POINTER_MAX as f64 * 0.5) as i32;
        assert_eq!(mid, POINTER_MAX / 2);
        assert_eq!((POINTER_MAX as f64 * 1.0) as i32, POINTER_MAX);
    }
}
