//! Watches systemd-logind via D-Bus and maintains a [`tokio::sync::watch`]
//! channel reflecting the currently active graphical session.
//!
//! When the active session changes (login, logout, VT switch) the watcher
//! also updates the process-level `DISPLAY`, `XAUTHORITY`,
//! `XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS`, and `PULSE_SERVER`
//! environment variables so that child processes (parec, pactl, paplay) and
//! arboard automatically inherit the new session without needing to be
//! passed explicit paths.

use std::path::PathBuf;

use anyhow::{Context, Result};
use futures_util::StreamExt as _;
use tokio::sync::watch;
use zbus::Connection;
use zbus::proxy;

use crate::session::{Session, find_display_fallback, find_xauthority};

type LogindSession = (String, u32, String, String, zbus::zvariant::OwnedObjectPath);

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait LoginManager {
    fn list_sessions(&self) -> zbus::Result<Vec<LogindSession>>;

    #[zbus(signal)]
    fn session_new(
        &self,
        session_id: &str,
        object_path: zbus::zvariant::ObjectPath<'_>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn session_removed(
        &self,
        session_id: &str,
        object_path: zbus::zvariant::ObjectPath<'_>,
    ) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1/session/auto"
)]
trait LoginSession {
    #[zbus(property)]
    fn active(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "Type")]
    fn session_type(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn display(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn uid(&self) -> zbus::Result<u32>;

    #[zbus(property)]
    fn leader(&self) -> zbus::Result<u32>;
}

async fn find_active_session(conn: &Connection) -> Option<Session> {
    let manager = LoginManagerProxy::new(conn).await.ok()?;
    let sessions = manager.list_sessions().await.ok()?;

    for (_, uid, username, _, path) in sessions {
        let proxy = LoginSessionProxy::builder(conn)
            .path(path)
            .ok()?
            .build()
            .await
            .ok()?;

        let session_type = proxy.session_type().await.unwrap_or_default();
        if !matches!(session_type.as_str(), "x11" | "wayland" | "mir") {
            continue;
        }
        if !proxy.active().await.unwrap_or(false) {
            continue;
        }

        let display_str = proxy.display().await.unwrap_or_default();
        let display = if display_str.is_empty() {
            // logind frequently never learns the X11 display number for a
            // session it otherwise correctly tracks as `Type=x11`/`Active`
            // (observed on a plain GDM-started session) - fall back to
            // scanning for the X server socket ourselves.
            find_display_fallback()
        } else {
            Some(display_str)
        };
        let leader = proxy.leader().await.unwrap_or(0);
        let xdg_runtime_dir = PathBuf::from(format!("/run/user/{uid}"));
        let xauthority = find_xauthority(&username, &xdg_runtime_dir, leader);

        return Some(Session {
            uid,
            username,
            display,
            xauthority,
            xdg_runtime_dir,
        });
    }

    None
}

/// Update process-level environment variables to reflect `session`.
///
/// Child processes (parec, pactl, paplay) and arboard inherit these so they
/// automatically connect to the right PulseAudio/X11 instance.
///
/// `PULSE_SERVER` matters even though `XDG_RUNTIME_DIR` is also set: when
/// this process runs as root (the system-service deployment), PulseAudio's
/// client library ignores `XDG_RUNTIME_DIR` for uid 0 and looks for a
/// system-wide socket at `/var/run/pulse` instead, which doesn't exist -
/// only an explicit `PULSE_SERVER` reaches the target user's PipeWire/Pulse
/// instance in that case.
///
/// # Safety
/// `set_var`/`remove_var` are unsafe in Rust edition 2024 due to potential
/// races in multi-threaded programs. We accept this here because we are the
/// only caller that writes these variables and we do so from a single task
/// before any child processes that read them are spawned.
fn apply_session_env(session: &Option<Session>) {
    unsafe {
        match session {
            Some(s) => {
                match &s.display {
                    Some(d) => std::env::set_var("DISPLAY", d),
                    None => std::env::remove_var("DISPLAY"),
                }
                match &s.xauthority {
                    Some(xa) => std::env::set_var("XAUTHORITY", xa),
                    None => std::env::remove_var("XAUTHORITY"),
                }
                std::env::set_var("XDG_RUNTIME_DIR", &s.xdg_runtime_dir);
                std::env::set_var(
                    "DBUS_SESSION_BUS_ADDRESS",
                    format!("unix:path={}/bus", s.xdg_runtime_dir.display()),
                );
                std::env::set_var("PULSE_SERVER", s.pulse_server());
            }
            None => {
                std::env::remove_var("DISPLAY");
                std::env::remove_var("XAUTHORITY");
                std::env::remove_var("XDG_RUNTIME_DIR");
                std::env::remove_var("DBUS_SESSION_BUS_ADDRESS");
                std::env::remove_var("PULSE_SERVER");
            }
        }
    }
}

/// Connect to systemd-logind on the system D-Bus, detect the current active
/// graphical session, and return a [`watch::Receiver`] that is updated
/// whenever the active session changes.
///
/// Falls back gracefully if D-Bus is unavailable: returns a receiver whose
/// initial value is `None` and no background watcher is started.  In that
/// case env vars set in the unit's `EnvironmentFile` are used as-is.
pub async fn start() -> Result<watch::Receiver<Option<Session>>> {
    let conn = match Connection::system().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("kmsrdp: system D-Bus unavailable ({e}), session auto-detect disabled");
            let (_, rx) = watch::channel(None);
            return Ok(rx);
        }
    };

    let initial = find_active_session(&conn).await;
    apply_session_env(&initial);

    if let Some(ref s) = initial {
        eprintln!(
            "kmsrdp: active session: user={} uid={} display={} xdg_runtime_dir={}",
            s.username,
            s.uid,
            s.display.as_deref().unwrap_or("(wayland)"),
            s.xdg_runtime_dir.display(),
        );
    } else {
        eprintln!("kmsrdp: no active graphical session found at startup");
    }

    let (tx, rx) = watch::channel(initial);

    tokio::spawn(async move {
        if let Err(e) = run_watcher(conn, tx).await {
            eprintln!("kmsrdp: session watcher stopped: {e}");
        }
    });

    Ok(rx)
}

async fn run_watcher(conn: Connection, tx: watch::Sender<Option<Session>>) -> Result<()> {
    let manager = LoginManagerProxy::new(&conn)
        .await
        .context("LoginManagerProxy::new")?;

    let mut new_stream = manager.receive_session_new().await?;
    let mut removed_stream = manager.receive_session_removed().await?;

    loop {
        tokio::select! {
            msg = new_stream.next() => { if msg.is_none() { break; } }
            msg = removed_stream.next() => { if msg.is_none() { break; } }
        }

        let session = find_active_session(&conn).await;
        apply_session_env(&session);

        if let Some(ref s) = session {
            eprintln!(
                "kmsrdp: session switched to user={} uid={} display={}",
                s.username,
                s.uid,
                s.display.as_deref().unwrap_or("(wayland)"),
            );
        } else {
            eprintln!("kmsrdp: no active graphical session");
        }

        let _ = tx.send(session);
    }

    Ok(())
}
