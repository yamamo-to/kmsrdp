//! Serve the DRM/KMS-captured live screen over RDP with the from-scratch
//! `rdpcore-*` stack, and forward RDP input back through the uinput
//! virtual device. TLS (self-signed, regenerated per run - see `tls.rs`)
//! plus username/password auth. Connect with e.g. `xfreerdp
//! /v:<host> /u:<user> /p:<password> /cert:ignore`.
//!
//! Credentials come from `KMSRDP_USER`/`KMSRDP_PASSWORD`; if unset, a
//! random one-shot password is generated and printed on startup so the
//! server is never reachable with a guessable default.
//!
//! Session management: at startup the server connects to systemd-logind
//! via D-Bus and watches for session changes.  When a user logs in or
//! out the server automatically switches `DISPLAY`/`XAUTHORITY`/
//! `XDG_RUNTIME_DIR` and the X11 Unicode typer reconnects to the new
//! session.  Existing RDP connections are not dropped.
//!
//! Concurrent clients share one DRM capture loop ([`DisplayHub`]) and one
//! uinput device ([`SharedInput`]); audio and clipboard are per-connection.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use kmsrdp::audio::LocalAudioFactory;
use kmsrdp::audio_input::VirtualMicFactory;
use kmsrdp::capture;
use kmsrdp::clipboard::LocalClipboardFactory;
use kmsrdp::display_hub::{Display, DisplayHub, MouseScale};
use kmsrdp::tls;
use kmsrdp::uinput::{self, VirtualInput};
use kmsrdp::x11_unicode::X11UnicodeTyper;
use rdpcore_server::{
    Credentials, ExactMatchCredentialValidator, KeyboardEvent, MouseEvent, RdpServer,
    RdpServerInputHandler,
};

struct Input {
    device: VirtualInput,
    mouse_scale: MouseScale,
    x11_typer: X11UnicodeTyper,
}

/// Cloneable handle around the singleton uinput / X11 typer. All RDP
/// sessions inject through the same device, serialized by the mutex.
#[derive(Clone)]
struct SharedInput {
    inner: Arc<Mutex<Input>>,
}

impl SharedInput {
    fn new(input: Input) -> Self {
        Self {
            inner: Arc::new(Mutex::new(input)),
        }
    }
}

impl RdpServerInputHandler for SharedInput {
    fn keyboard(&mut self, event: KeyboardEvent) {
        self.inner.lock().unwrap().keyboard(event);
    }

    fn mouse(&mut self, event: MouseEvent) {
        self.inner.lock().unwrap().mouse(event);
    }
}

impl RdpServerInputHandler for Input {
    fn keyboard(&mut self, event: KeyboardEvent) {
        let scancode = match event {
            KeyboardEvent::Pressed { code, extended } => Some((code, extended, true)),
            KeyboardEvent::Released { code, extended } => Some((code, extended, false)),
            // IME-composed text (e.g. CJK input) has no scancode at all;
            // inject via X11 keymap-remap trick. Only act on press.
            KeyboardEvent::UnicodePressed(codepoint) => {
                self.x11_typer.type_char(codepoint.into());
                None
            }
        };

        let Some((code, extended, down)) = scancode else { return };
        match uinput::linux_keycode_from_rdp_scancode(code, extended) {
            Some(keycode) => {
                if let Err(e) = self.device.key(keycode, down) {
                    eprintln!("key injection failed: {e}");
                }
            }
            None => eprintln!("no keycode mapping for scancode {code:#x} (extended={extended})"),
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        let result = match event {
            MouseEvent::Move { x, y } => {
                let (width, height) = *self.mouse_scale.lock().unwrap();
                self.device.move_abs(f64::from(x) / width, f64::from(y) / height)
            }
            MouseEvent::LeftPressed => self.device.button(uinput::BTN_LEFT, true),
            MouseEvent::LeftReleased => self.device.button(uinput::BTN_LEFT, false),
            MouseEvent::RightPressed => self.device.button(uinput::BTN_RIGHT, true),
            MouseEvent::RightReleased => self.device.button(uinput::BTN_RIGHT, false),
            MouseEvent::MiddlePressed => self.device.button(uinput::BTN_MIDDLE, true),
            MouseEvent::MiddleReleased => self.device.button(uinput::BTN_MIDDLE, false),
            MouseEvent::VerticalScroll { value } => self.device.scroll(value.into()),
        };
        if let Err(e) = result {
            eprintln!("mouse injection failed: {e}");
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Session watcher must start first: it sets DISPLAY/XAUTHORITY/
    // XDG_RUNTIME_DIR in the process environment so that all subsequent
    // component initializations (arboard, pactl) see the right session.
    let session_rx = kmsrdp::session_watcher::start().await?;

    let initial = capture::capture_raw_bgrx()?;
    let width = initial.width as u16;
    let height = initial.height as u16;
    println!("desktop size: {width}x{height}");

    let device = VirtualInput::create()?;
    println!("virtual input device created");

    let mouse_scale: MouseScale = Arc::new(Mutex::new((f64::from(width), f64::from(height))));
    let hub = DisplayHub::start(width, height, mouse_scale.clone());
    let display = Display::new(hub);

    let input = SharedInput::new(Input {
        device,
        mouse_scale,
        x11_typer: X11UnicodeTyper::new(session_rx.clone()),
    });

    let username = std::env::var("KMSRDP_USER").unwrap_or_else(|_| "kmsrdp".to_string());
    let password = match std::env::var("KMSRDP_PASSWORD") {
        Ok(password) => password,
        Err(_) => {
            use rand::RngExt as _;
            let generated: String = rand::rng()
                .sample_iter(&rand::distr::Alphanumeric)
                .take(20)
                .map(char::from)
                .collect();
            println!(
                "KMSRDP_PASSWORD not set; generated a one-shot password for this run:\n  \
                 user: {username}\n  password: {generated}"
            );
            generated
        }
    };
    let validator = ExactMatchCredentialValidator::new(Credentials {
        username,
        password,
        domain: None,
    });

    let acceptor = tls::build_acceptor()?;

    let addr: SocketAddr = "0.0.0.0:3389".parse()?;
    let server: RdpServer = RdpServer::builder()
        .with_addr(addr)
        .with_tls(acceptor)
        .with_input_handler(input)
        .with_display_handler(display)
        .with_cliprdr_factory(Some(Box::new(LocalClipboardFactory::new(session_rx.clone()))))
        .with_sound_factory(Some(Box::new(LocalAudioFactory::new())))
        .with_audio_input_factory(Some(Box::new(VirtualMicFactory::new())))
        .with_credential_validator(Some(Arc::new(validator)))
        .build();

    println!(
        "RDP server listening on {addr} (TLS, self-signed - use e.g. \
         `xfreerdp /sec:tls /cert:ignore /u:<user> /p:<password>`)"
    );
    server.run().await
}
