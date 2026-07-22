use std::path::{Path, PathBuf};

/// Active graphical user session detected via systemd-logind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub uid: u32,
    pub username: String,
    /// X11 display string (e.g. ":0"). None for Wayland-native sessions.
    pub display: Option<String>,
    /// Path to the Xauthority cookie file. None if not found.
    pub xauthority: Option<PathBuf>,
    /// /run/user/$uid
    pub xdg_runtime_dir: PathBuf,
}

impl Session {
    /// PulseAudio/PipeWire UNIX domain socket URL for use with `PULSE_SERVER`.
    pub fn pulse_server(&self) -> String {
        format!("unix:{}/pulse/native", self.xdg_runtime_dir.display())
    }
}

/// Find the Xauthority file for a session, trying in order:
///
/// 1. `/proc/$leader_pid/environ` – reliable when running as root.
/// 2. `$xdg_runtime_dir/gdm/Xauthority` – where GDM keeps the cookie for a
///    session it manages; the leader PID is `gdm-session-worker`, a
///    short-lived PAM helper that never exports `XAUTHORITY` in its own
///    environment, so (1) misses this despite the file being right there.
/// 3. `.mutter-Xwaylandauth.*` in `xdg_runtime_dir` – GNOME Wayland+XWayland.
/// 4. `~/.Xauthority` – classic X11 fallback.
pub fn find_xauthority(username: &str, xdg_runtime_dir: &Path, leader_pid: u32) -> Option<PathBuf> {
    if let Ok(environ) = std::fs::read(format!("/proc/{leader_pid}/environ"))
        && let Some(path) = environ
            .split(|&b| b == 0)
            .filter_map(|entry| {
                let s = std::str::from_utf8(entry).ok()?;
                s.strip_prefix("XAUTHORITY=").map(PathBuf::from)
            })
            .find(|p| p.exists())
    {
        return Some(path);
    }

    let gdm_xauthority = xdg_runtime_dir.join("gdm/Xauthority");
    if gdm_xauthority.exists() {
        return Some(gdm_xauthority);
    }

    if let Ok(entries) = std::fs::read_dir(xdg_runtime_dir)
        && let Some(path) = entries
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.starts_with(".mutter-Xwaylandauth"))
                    .unwrap_or(false)
            })
            .map(|e| e.path())
            .next()
    {
        return Some(path);
    }

    let xauth = PathBuf::from(format!("/home/{username}/.Xauthority"));
    xauth.exists().then_some(xauth)
}

/// Returns true when `display` looks like `:N` and `/tmp/.X11-unix/XN` exists.
pub fn display_socket_exists(display: &str) -> bool {
    let Some(n) = display
        .strip_prefix(':')
        .and_then(|s| s.parse::<u32>().ok())
    else {
        return false;
    };
    PathBuf::from(format!("/tmp/.X11-unix/X{n}")).exists()
}

/// Read `DISPLAY` from a process environment when its socket is live.
pub fn display_from_leader_environ(leader_pid: u32) -> Option<String> {
    if leader_pid == 0 {
        return None;
    }
    let environ = std::fs::read(format!("/proc/{leader_pid}/environ")).ok()?;
    for entry in environ.split(|&b| b == 0) {
        let s = std::str::from_utf8(entry).ok()?;
        if let Some(display) = s.strip_prefix("DISPLAY=")
            && display_socket_exists(display)
        {
            return Some(display.to_string());
        }
    }
    None
}

/// Best-effort X11 `DISPLAY` for a logind session.
///
/// Order: logind `Display` property → session leader `DISPLAY` → sole
/// `/tmp/.X11-unix/X*` socket (startx/tty setups where logind still says
/// `Type=tty` and never sets `Display=`).
pub fn resolve_x11_display(logind_display: &str, leader_pid: u32) -> Option<String> {
    if !logind_display.is_empty() {
        let display = logind_display.to_string();
        if display_socket_exists(&display) {
            return Some(display);
        }
    }
    if let Some(display) = display_from_leader_environ(leader_pid) {
        return Some(display);
    }
    find_display_fallback()
}

/// Fallback DISPLAY detection for when logind's own `Display` session
/// property comes back empty - which it does for plenty of real, active
/// X11 sessions (confirmed against a GDM-started `Type=x11` session: it
/// carries no `Display=` property whatsoever even while `Active=yes`).
/// Logind not populating this property is a common, longstanding gap, not
/// something specific to one desktop environment.
///
/// Scans `/tmp/.X11-unix/` for running X server sockets. Only trusts the
/// result when exactly one is present - kmsrdp only ever targets a single
/// active session at a time anyway (see `RdpServer::run`), so ambiguity
/// with more than one candidate isn't worth guessing at.
pub fn find_display_fallback() -> Option<String> {
    let mut displays: Vec<u32> = std::fs::read_dir("/tmp/.X11-unix")
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.strip_prefix('X')?.parse().ok())
        .collect();
    displays.sort_unstable();
    displays.dedup();
    match displays.as_slice() {
        [n] => Some(format!(":{n}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_socket_exists_requires_colon_prefix() {
        assert!(!display_socket_exists("0"));
        assert!(!display_socket_exists(""));
    }

    #[test]
    fn resolve_x11_display_uses_logind_when_socket_live() {
        // :0 is present on most dev machines running Cursor; skip if not.
        if !display_socket_exists(":0") {
            return;
        }
        assert_eq!(resolve_x11_display(":0", 0).as_deref(), Some(":0"));
        // Empty logind Display falls back to the sole /tmp/.X11-unix socket.
        if find_display_fallback().as_deref() == Some(":0") {
            assert_eq!(resolve_x11_display("", 0).as_deref(), Some(":0"));
        }
    }

    #[test]
    fn pulse_server_uses_runtime_dir() {
        let session = Session {
            uid: 1000,
            username: "alice".to_string(),
            display: Some(":0".to_string()),
            xauthority: None,
            xdg_runtime_dir: PathBuf::from("/run/user/1000"),
        };
        assert_eq!(session.pulse_server(), "unix:/run/user/1000/pulse/native");
    }

    #[test]
    fn find_xauthority_falls_back_to_gdm_cookie() {
        let dir = std::env::temp_dir().join(format!("kmsrdp-xauth-test-{}", std::process::id()));
        let gdm_dir = dir.join("gdm");
        std::fs::create_dir_all(&gdm_dir).unwrap();
        let cookie = gdm_dir.join("Xauthority");
        std::fs::write(&cookie, b"").unwrap();
        let found = find_xauthority("alice", &dir, 9_999_999);
        assert_eq!(found, Some(cookie));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
