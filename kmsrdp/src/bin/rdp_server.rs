//! Serve the DRM/KMS-captured live screen over RDP with the from-scratch
//! `rdpcore-*` stack, and forward RDP input back through the uinput
//! virtual device. TLS uses a persisted self-signed certificate by default
//! (see `tls.rs`) plus username/password auth. Connect with e.g. `xfreerdp
//! /v:127.0.0.1 /u:<user> /p:<password> /cert:ignore`.
//!
//! Credentials come from `KMSRDP_USER`/`KMSRDP_PASSWORD`; if unset, a
//! random one-shot password is written to a 0600 file (and printed only
//! when stderr is a TTY).
//!
//! Defaults: listen on `127.0.0.1:3389`, require NLA, one authenticated
//! session. Set `KMSRDP_BIND=0.0.0.0` and `KMSRDP_REQUIRE_NLA=0` only on
//! a trusted network.
//!
//! Session management: at startup the server connects to systemd-logind
//! via D-Bus and watches for session changes.  When a user logs in or
//! out the server automatically switches `DISPLAY`/`XAUTHORITY`/
//! `XDG_RUNTIME_DIR` and the X11 Unicode typer reconnects to the new
//! session.  Existing RDP connections are not dropped.
//!
//! Concurrent clients share one DRM capture loop ([`DisplayHub`]) and one
//! uinput device ([`SharedInput`]); by default a second authenticated
//! session is rejected. Audio is per-connection. Clipboard backends are
//! per-connection but share one process-wide local watcher.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
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
    /// Linux keycodes currently reported as held down, so `reset` can
    /// release exactly those on disconnect - a client that disconnects
    /// mid-keypress (before the matching `Released` arrives) would
    /// otherwise leave the shared uinput device reporting that key held
    /// forever, which e.g. X11's key-repeat then turns into the key
    /// retyping itself indefinitely.
    pressed_keys: HashSet<i32>,
    /// Same idea for mouse buttons.
    pressed_buttons: HashSet<i32>,
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

    fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.reset();
        inner.device.destroy();
    }
}

impl RdpServerInputHandler for SharedInput {
    fn keyboard(&mut self, event: KeyboardEvent) {
        // Poison-tolerant: this Mutex is one process-wide singleton shared
        // by every connected RDP session (see this struct's doc comment).
        // A panic while injecting one session's event must not
        // permanently break input for every other session too.
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keyboard(event);
    }

    fn mouse(&mut self, event: MouseEvent) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mouse(event);
    }

    fn reset(&mut self) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).reset();
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
                    tracing::warn!(error = %e, "key injection failed");
                }
                if down {
                    self.pressed_keys.insert(keycode);
                } else {
                    self.pressed_keys.remove(&keycode);
                }
            }
            None => tracing::debug!(scancode = code, extended, "no keycode mapping for scancode"),
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        let button = match event {
            MouseEvent::LeftPressed | MouseEvent::LeftReleased => Some(uinput::BTN_LEFT),
            MouseEvent::RightPressed | MouseEvent::RightReleased => Some(uinput::BTN_RIGHT),
            MouseEvent::MiddlePressed | MouseEvent::MiddleReleased => Some(uinput::BTN_MIDDLE),
            MouseEvent::Move { .. }
            | MouseEvent::VerticalScroll { .. }
            | MouseEvent::HorizontalScroll { .. } => None,
        };
        let down = matches!(
            event,
            MouseEvent::LeftPressed | MouseEvent::RightPressed | MouseEvent::MiddlePressed
        );

        let result = match event {
            MouseEvent::Move { x, y } => {
                let (width, height) = *self.mouse_scale.lock().unwrap_or_else(|e| e.into_inner());
                self.device
                    .move_abs(f64::from(x) / width, f64::from(y) / height)
            }
            MouseEvent::LeftPressed | MouseEvent::LeftReleased => {
                self.device.button(uinput::BTN_LEFT, down)
            }
            MouseEvent::RightPressed | MouseEvent::RightReleased => {
                self.device.button(uinput::BTN_RIGHT, down)
            }
            MouseEvent::MiddlePressed | MouseEvent::MiddleReleased => {
                self.device.button(uinput::BTN_MIDDLE, down)
            }
            MouseEvent::VerticalScroll { value } => self.device.scroll(value),
            MouseEvent::HorizontalScroll { value } => self.device.hscroll(value),
        };
        if let Err(e) = result {
            tracing::warn!(error = %e, "mouse injection failed");
        }
        if let Some(button) = button {
            if down {
                self.pressed_buttons.insert(button);
            } else {
                self.pressed_buttons.remove(&button);
            }
        }
    }

    fn reset(&mut self) {
        for keycode in self.pressed_keys.drain() {
            if let Err(e) = self.device.key(keycode, false) {
                tracing::warn!(error = %e, keycode, "failed to release stuck key on disconnect");
            }
        }
        for button in self.pressed_buttons.drain() {
            if let Err(e) = self.device.button(button, false) {
                tracing::warn!(error = %e, button, "failed to release stuck mouse button on disconnect");
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    kmsrdp::logging::init();
    // Before any Pulse/PipeWire client is created (session watcher, capture, …).
    kmsrdp::audio::hint_low_latency_audio();

    // Fail fast on bad env / missing privileges before touching DRM or uinput.
    let cfg = kmsrdp::config::Config::from_env()?;
    kmsrdp::config_check::log_report(&kmsrdp::config_check::validate(cfg.listen.port()))
        .context("startup configuration check failed")?;

    // Session watcher must start first: it sets DISPLAY/XAUTHORITY/
    // XDG_RUNTIME_DIR in the process environment so that all subsequent
    // component initializations (arboard, libpulse) see the right session.
    let session_rx = kmsrdp::session_watcher::start().await?;

    let mut capturer = capture::Capturer::new().context(
        "failed to open screen capturer (DRM/KMS). \
         Check that a CRTC is active, KMSRDP_DISPLAY matches a connector, \
         and the process has CAP_SYS_ADMIN/CAP_DAC_OVERRIDE",
    )?;
    let initial = capturer.capture().context(
        "failed to capture the first frame. \
         DRM found no usable CRTC/FB and NvFBC (if available) also failed — \
         clients would see a black screen",
    )?;
    let width = initial.width as u16;
    let height = initial.height as u16;
    if width == 0 || height == 0 {
        anyhow::bail!(
            "initial capture returned {}x{} — refusing to start with a blank desktop",
            width,
            height
        );
    }
    tracing::info!(width, height, "desktop size");
    if initial.monitors.len() > 1 {
        let monitors = initial
            .monitors
            .iter()
            .map(|m| {
                format!(
                    "{}x{}@{},{}{}",
                    m.right - m.left + 1,
                    m.bottom - m.top + 1,
                    m.left,
                    m.top,
                    if m.primary { " (primary)" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        tracing::info!(%monitors, "composite monitors");
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
    let hub = DisplayHub::start(
        width,
        height,
        mouse_scale.clone(),
        capturer,
        monitors,
        cfg.frame_interval,
    );
    let display = Display::new(hub);

    let credentials = Credentials {
        username: cfg.username.clone(),
        password: cfg.password.clone(),
        domain: None,
    };
    let validator = ExactMatchCredentialValidator::new(credentials.clone());

    let tls_identity = tls::build_acceptor()?;

    // Bind before creating the uinput device so a busy port fails without
    // spamming `input: kmsrdp as ...` on every systemd restart.
    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind {}: {e}", cfg.listen))?;

    let device = tokio::task::spawn_blocking(VirtualInput::create)
        .await
        .context("uinput create task panicked")??;
    tracing::info!("virtual input device created");

    let input = SharedInput::new(Input {
        device,
        mouse_scale,
        x11_typer: X11UnicodeTyper::spawn(session_rx.clone()),
        pressed_keys: HashSet::new(),
        pressed_buttons: HashSet::new(),
    });

    let (drive_factory, fuse_shutdown): (
        Box<dyn rdpcore_rdpdr::DriveConsumerFactory>,
        Option<FuseDriveFactory>,
    ) = {
        #[cfg(feature = "rdpdr-diagnostic")]
        {
            if std::env::var_os("KMSRDP_RDPDR_DIAGNOSTIC").is_some() {
                tracing::info!("RDPDR diagnostic self-test enabled (KMSRDP_RDPDR_DIAGNOSTIC)");
                (
                    Box::new(kmsrdp::rdpdr_diagnostic::DiagnosticDriveFactory),
                    None,
                )
            } else {
                let fuse = FuseDriveFactory::new(session_rx.clone());
                (Box::new(fuse.clone()), Some(fuse))
            }
        }
        #[cfg(not(feature = "rdpdr-diagnostic"))]
        {
            if std::env::var_os("KMSRDP_RDPDR_DIAGNOSTIC").is_some() {
                tracing::warn!(
                    "KMSRDP_RDPDR_DIAGNOSTIC is set but this binary was built without \
                     the rdpdr-diagnostic feature; using FUSE drives"
                );
            }
            let fuse = FuseDriveFactory::new(session_rx.clone());
            (Box::new(fuse.clone()), Some(fuse))
        }
    };

    if cfg.password_generated {
        tracing::warn!(
            "using a generated one-shot password; set KMSRDP_PASSWORD_FILE or KMSRDP_PASSWORD \
             for a stable credential (the secret should not live in the process environment)"
        );
    }
    if cfg.require_nla {
        tracing::info!("NLA (Network Level Authentication) is required for all connections");
    } else {
        tracing::warn!(
            "NLA is optional (KMSRDP_REQUIRE_NLA=0); clients may authenticate with TLS + Client Info only"
        );
    }
    if cfg.gfx_enabled {
        tracing::info!("GFX AVC420 requested (KMSRDP_GFX); Planar/NSCodec remains the fallback");
    }
    tracing::info!(max_sessions = cfg.max_sessions, "concurrent session limit");
    if cfg.max_sessions > 1 {
        tracing::warn!(
            max_sessions = cfg.max_sessions,
            "multiple authenticated sessions share one desktop, one uinput device, \
             and one FUSE drive mount per DosName — I/O goes through the mount owner"
        );
    }

    let cliprdr_factory: Option<Box<dyn rdpcore_cliprdr::CliprdrBackendFactory>> =
        if cfg.clipboard.is_disabled() {
            tracing::info!("clipboard redirection disabled (KMSRDP_CLIPBOARD=disabled)");
            None
        } else {
            match cfg.clipboard {
                kmsrdp::clipboard::ClipboardMode::HostToClient => {
                    tracing::info!("clipboard redirection set to host-to-client (read-only)");
                }
                kmsrdp::clipboard::ClipboardMode::ClientToHost => {
                    tracing::info!("clipboard redirection set to client-to-host");
                }
                _ => {}
            }
            Some(Box::new(LocalClipboardFactory::new(
                session_rx.clone(),
                cfg.clipboard,
            )))
        };

    let input_for_shutdown = input.clone();
    let server: RdpServer = RdpServer::builder()
        .with_listener(listener)
        .with_tls(tls_identity.acceptor)
        .with_tls_public_key(tls_identity.public_key)
        .with_input_handler(input)
        .with_display_handler(display)
        .with_cliprdr_factory(cliprdr_factory)
        .with_sound_factory(Some(Box::new(LocalAudioFactory::new())))
        .with_audio_input_factory(Some(Box::new(VirtualMicFactory::new())))
        .with_drive_factory(Some(drive_factory))
        .with_credential_validator(Some(Arc::new(validator)))
        .with_nla_credentials(Some(credentials))
        .with_require_nla(cfg.require_nla)
        .with_max_sessions(cfg.max_sessions)
        .with_gfx(cfg.gfx_enabled)
        .build();

    let nla_desc = if cfg.require_nla {
        "TLS + required NLA"
    } else {
        "TLS + optional NLA"
    };
    tracing::info!(addr = %cfg.listen, "RDP server listening ({nla_desc})");
    // Clean up FUSE/uinput first, then exit. Tokio graceful shutdown would
    // wait for DRM `spawn_blocking` / FUSE threads and can hang host
    // shutdown; `process::exit` after a short cleanup is still the backstop.
    tokio::select! {
        result = server.run() => result,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("SIGINT, shutting down");
            graceful_shutdown(&input_for_shutdown, fuse_shutdown.as_ref());
            std::process::exit(0);
        }
        _ = sigterm() => {
            tracing::info!("SIGTERM, shutting down");
            graceful_shutdown(&input_for_shutdown, fuse_shutdown.as_ref());
            std::process::exit(0);
        }
    }
}

fn graceful_shutdown(input: &SharedInput, fuse: Option<&FuseDriveFactory>) {
    if let Some(fuse) = fuse {
        fuse.unmount_all();
    }
    input.shutdown();
}

async fn sigterm() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut stream) => {
            stream.recv().await;
        }
        Err(e) => {
            tracing::warn!(error = %e, "cannot install SIGTERM handler; waiting forever");
            std::future::pending::<()>().await;
        }
    }
}
