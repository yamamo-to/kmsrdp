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
//! uinput device ([`SharedInput`]); audio is per-connection. Clipboard
//! backends are per-connection but share one process-wide local poller.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use kmsrdp::audio::LocalAudioFactory;
use kmsrdp::audio_input::VirtualMicFactory;
use kmsrdp::capture;
use kmsrdp::clipboard::LocalClipboardFactory;
use kmsrdp::display_hub::{Display, DisplayHub, MouseScale};
use kmsrdp::rdpdr_fuse::FuseDriveFactory;
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

        let Some((code, extended, down)) = scancode else {
            return;
        };
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
                self.device
                    .move_abs(f64::from(x) / width, f64::from(y) / height)
            }
            MouseEvent::LeftPressed => self.device.button(uinput::BTN_LEFT, true),
            MouseEvent::LeftReleased => self.device.button(uinput::BTN_LEFT, false),
            MouseEvent::RightPressed => self.device.button(uinput::BTN_RIGHT, true),
            MouseEvent::RightReleased => self.device.button(uinput::BTN_RIGHT, false),
            MouseEvent::MiddlePressed => self.device.button(uinput::BTN_MIDDLE, true),
            MouseEvent::MiddleReleased => self.device.button(uinput::BTN_MIDDLE, false),
            MouseEvent::VerticalScroll { value } => self.device.scroll(value),
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

    let mut capturer = capture::Capturer::new()?;
    let initial = capturer.capture()?;
    let width = initial.width as u16;
    let height = initial.height as u16;
    println!("desktop size: {width}x{height}");
    if initial.monitors.len() > 1 {
        println!(
            "composite monitors: {}",
            initial
                .monitors
                .iter()
                .map(|m| format!(
                    "{}x{}@{},{}{}",
                    m.right - m.left + 1,
                    m.bottom - m.top + 1,
                    m.left,
                    m.top,
                    if m.primary { " (primary)" } else { "" }
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let mouse_scale: MouseScale = Arc::new(Mutex::new((f64::from(width), f64::from(height))));
    let monitors = initial
        .monitors
        .iter()
        .map(|m| rdpcore_server::MonitorLayoutEntry {
            left: m.left,
            top: m.top,
            right: m.right,
            bottom: m.bottom,
            primary: m.primary,
        })
        .collect();
    let hub = DisplayHub::start(width, height, mouse_scale.clone(), capturer, monitors);
    let display = Display::new(hub);

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
    let credentials = Credentials {
        username: username.clone(),
        password: password.clone(),
        domain: None,
    };
    let validator = ExactMatchCredentialValidator::new(credentials.clone());

    let tls_identity = tls::build_acceptor()?;

    // Bind before creating the uinput device so a missing CAP_NET_BIND_SERVICE
    // (or a busy port) fails without spamming `input: kmsrdp as ...` on every
    // systemd restart. Override with KMSRDP_BIND / KMSRDP_PORT.
    let addr = listen_addr()?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {addr}: {e}"))?;

    let device = VirtualInput::create()?;
    println!("virtual input device created");

    let input = SharedInput::new(Input {
        device,
        mouse_scale,
        x11_typer: X11UnicodeTyper::new(session_rx.clone()),
    });

    let drive_factory: Box<dyn rdpcore_rdpdr::DriveConsumerFactory> = {
        #[cfg(feature = "rdpdr-diagnostic")]
        {
            if std::env::var_os("KMSRDP_RDPDR_DIAGNOSTIC").is_some() {
                println!("kmsrdp: RDPDR diagnostic self-test enabled (KMSRDP_RDPDR_DIAGNOSTIC)");
                Box::new(kmsrdp::rdpdr_diagnostic::DiagnosticDriveFactory)
            } else {
                Box::new(FuseDriveFactory::new(session_rx.clone()))
            }
        }
        #[cfg(not(feature = "rdpdr-diagnostic"))]
        {
            if std::env::var_os("KMSRDP_RDPDR_DIAGNOSTIC").is_some() {
                eprintln!(
                    "kmsrdp: KMSRDP_RDPDR_DIAGNOSTIC is set but this binary was built without \
                     the rdpdr-diagnostic feature; using FUSE drives"
                );
            }
            Box::new(FuseDriveFactory::new(session_rx.clone()))
        }
    };

    let server: RdpServer = RdpServer::builder()
        .with_listener(listener)
        .with_tls(tls_identity.acceptor)
        .with_tls_public_key(tls_identity.public_key)
        .with_input_handler(input)
        .with_display_handler(display)
        .with_cliprdr_factory(Some(Box::new(LocalClipboardFactory::new(
            session_rx.clone(),
        ))))
        .with_sound_factory(Some(Box::new(LocalAudioFactory::new())))
        .with_audio_input_factory(Some(Box::new(VirtualMicFactory::new())))
        .with_drive_factory(Some(drive_factory))
        .with_credential_validator(Some(Arc::new(validator)))
        .with_nla_credentials(Some(credentials))
        .build();

    println!(
        "RDP server listening on {addr} (TLS + optional NLA - use e.g. \
         `xfreerdp /cert:ignore /u:<user> /p:<password>` or mstsc)"
    );
    server.run().await
}

/// Listen address from `KMSRDP_BIND` (default `0.0.0.0`) and `KMSRDP_PORT`
/// (default `3389`). `KMSRDP_BIND` accepts an IPv4/IPv6 address (`127.0.0.1`,
/// `::`, optional `[::1]` brackets).
fn listen_addr() -> Result<SocketAddr> {
    let port: u16 = match std::env::var("KMSRDP_PORT") {
        Ok(raw) => {
            let trimmed = raw.trim();
            trimmed.parse().map_err(|_| {
                anyhow::anyhow!("KMSRDP_PORT must be an integer port 1-65535, got {raw:?}")
            })?
        }
        Err(_) => 3389,
    };
    if port == 0 {
        anyhow::bail!("KMSRDP_PORT must be non-zero");
    }

    let bind = std::env::var("KMSRDP_BIND").unwrap_or_else(|_| "0.0.0.0".to_owned());
    let bind = bind.trim();
    let bind = bind
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(bind);
    let ip: std::net::IpAddr = bind.parse().map_err(|_| {
        anyhow::anyhow!(
            "KMSRDP_BIND must be an IP address (e.g. 0.0.0.0, 127.0.0.1, ::), got {bind:?}"
        )
    })?;
    Ok(SocketAddr::new(ip, port))
}
