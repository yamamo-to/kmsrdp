//! Process-wide settings parsed from `KMSRDP_*` environment variables.
//!
//! Defaults are conservative: loopback listen, NLA required, one
//! authenticated session. Override explicitly to expose the service.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use std::os::unix::fs::PermissionsExt as _;

use crate::clipboard::ClipboardMode;
use crate::tls;

/// Hard ceiling so `KMSRDP_MAX_SESSIONS` cannot accidentally share one
/// desktop/uinput/FUSE mount across a huge client count.
const MAX_SESSIONS_CAP: usize = 32;

/// Capture loop period when neither `KMSRDP_FPS` nor `KMSRDP_FRAME_INTERVAL_MS`
/// is set (20 fps).
pub const DEFAULT_FRAME_INTERVAL: Duration = Duration::from_millis(50);

/// Runtime configuration gathered once at startup.
#[derive(Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub require_nla: bool,
    pub max_sessions: usize,
    pub username: String,
    pub password: String,
    pub password_generated: bool,
    pub clipboard: ClipboardMode,
    /// MS-RDPEGFX AVC420. Off by default — the library reads this via
    /// [`rdpcore_server::RdpServerBuilder::with_gfx`], not `std::env`.
    pub gfx_enabled: bool,
    pub frame_interval: Duration,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("listen", &self.listen)
            .field("require_nla", &self.require_nla)
            .field("max_sessions", &self.max_sessions)
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .field("password_generated", &self.password_generated)
            .field("clipboard", &self.clipboard)
            .field("gfx_enabled", &self.gfx_enabled)
            .field("frame_interval", &self.frame_interval)
            .finish()
    }
}

impl Drop for Config {
    fn drop(&mut self) {
        use zeroize::Zeroize as _;
        self.password.zeroize();
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let listen = listen_addr()?;
        let require_nla = parse_bool_env("KMSRDP_REQUIRE_NLA")
            .or_else(|| parse_bool_env("KMSRDP_FORCE_NLA"))
            .unwrap_or(true);
        let max_sessions = parse_max_sessions()?;
        let (username, password, password_generated) = load_credentials()?;
        let clipboard = parse_clipboard_mode();
        let gfx_enabled = parse_bool_env("KMSRDP_GFX").unwrap_or(false);
        let frame_interval = parse_frame_interval();
        Ok(Self {
            listen,
            require_nla,
            max_sessions,
            username,
            password,
            password_generated,
            clipboard,
            gfx_enabled,
            frame_interval,
        })
    }
}

fn parse_max_sessions() -> Result<usize> {
    match std::env::var("KMSRDP_MAX_SESSIONS") {
        Ok(raw) => {
            let n: usize = raw.trim().parse().map_err(|_| {
                anyhow::anyhow!("KMSRDP_MAX_SESSIONS must be an integer >= 1, got {raw:?}")
            })?;
            if n == 0 {
                anyhow::bail!("KMSRDP_MAX_SESSIONS must be >= 1");
            }
            if n > MAX_SESSIONS_CAP {
                anyhow::bail!(
                    "KMSRDP_MAX_SESSIONS must be <= {MAX_SESSIONS_CAP} (sessions share one desktop, input device, and drive mount), got {n}"
                );
            }
            Ok(n)
        }
        Err(_) => Ok(1),
    }
}

pub(crate) fn parse_bool_env(name: &str) -> Option<bool> {
    let raw = std::env::var(name).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => {
            tracing::warn!("{name}={raw:?} is not a recognized boolean; ignoring");
            None
        }
    }
}

fn parse_frame_interval() -> Duration {
    if let Ok(fps_str) = std::env::var("KMSRDP_FPS") {
        match fps_str.trim().parse::<u32>() {
            Ok(fps) if fps >= 1 => {
                let clamped = fps.min(120);
                return Duration::from_millis(u64::from((1000 / clamped).max(1)));
            }
            _ => {
                tracing::warn!("KMSRDP_FPS={fps_str:?} is not an integer 1-120; ignoring");
            }
        }
    }
    if let Ok(ms_str) = std::env::var("KMSRDP_FRAME_INTERVAL_MS") {
        match ms_str.trim().parse::<u64>() {
            Ok(ms) if ms >= 1 => {
                return Duration::from_millis(ms.clamp(8, 1000));
            }
            _ => {
                tracing::warn!(
                    "KMSRDP_FRAME_INTERVAL_MS={ms_str:?} is not an integer 8-1000; ignoring"
                );
            }
        }
    }
    DEFAULT_FRAME_INTERVAL
}

fn parse_clipboard_mode() -> ClipboardMode {
    let raw = std::env::var("KMSRDP_CLIPBOARD").unwrap_or_else(|_| "bidirectional".to_string());
    match raw.trim().to_ascii_lowercase().as_str() {
        "disabled" | "off" | "0" | "false" => ClipboardMode::Disabled,
        "host-to-client" | "read-only" | "readonly" => ClipboardMode::HostToClient,
        "client-to-host" => ClipboardMode::ClientToHost,
        "bidirectional" | "on" | "1" | "true" | "" => ClipboardMode::Bidirectional,
        other => {
            tracing::warn!(
                "KMSRDP_CLIPBOARD={other:?} is not a recognized mode \
                 (disabled, host-to-client, client-to-host, bidirectional); using bidirectional"
            );
            ClipboardMode::Bidirectional
        }
    }
}

/// Listen address from `KMSRDP_BIND` (default `127.0.0.1`) and `KMSRDP_PORT`
/// (default `3389`). `KMSRDP_BIND` accepts an IPv4/IPv6 address (`127.0.0.1`,
/// `0.0.0.0`, `::`, optional `[::1]` brackets).
pub fn listen_addr() -> Result<SocketAddr> {
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

    let bind = std::env::var("KMSRDP_BIND").unwrap_or_else(|_| "127.0.0.1".to_owned());
    let bind = bind.trim();
    let bind = bind
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(bind);
    let ip: IpAddr = bind.parse().map_err(|_| {
        anyhow::anyhow!(
            "KMSRDP_BIND must be an IP address (e.g. 127.0.0.1, 0.0.0.0, ::), got {bind:?}"
        )
    })?;
    Ok(SocketAddr::new(ip, port))
}

fn load_credentials() -> Result<(String, String, bool)> {
    let username = std::env::var("KMSRDP_USER").unwrap_or_else(|_| "kmsrdp".to_string());
    match std::env::var("KMSRDP_PASSWORD") {
        Ok(password) => Ok((username, password, false)),
        Err(_) => {
            if let Some((path, password)) = read_password_file()? {
                tracing::info!(
                    path = %path.display(),
                    "loaded RDP password from file (not from KMSRDP_PASSWORD)"
                );
                return Ok((username, password, false));
            }
            use rand::RngExt as _;
            let generated: String = rand::rng()
                .sample_iter(&rand::distr::Alphanumeric)
                .take(20)
                .map(char::from)
                .collect();
            let path = persist_oneshot_password(&generated)?;
            tracing::warn!(
                user = %username,
                path = %path.display(),
                "KMSRDP_PASSWORD not set; wrote a one-shot password to a 0600 file"
            );
            if stderr_is_tty() {
                eprintln!(
                    "kmsrdp: one-shot RDP password for user {username}: {generated}\n\
                     kmsrdp: also written to {}",
                    path.display()
                );
            } else {
                eprintln!(
                    "kmsrdp: one-shot RDP password for user {username} written to {}",
                    path.display()
                );
            }
            Ok((username, generated, true))
        }
    }
}

/// Password file path: `KMSRDP_PASSWORD_FILE`, else systemd
/// `$CREDENTIALS_DIRECTORY/kmsrdp.password` when that file exists.
pub fn password_file_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("KMSRDP_PASSWORD_FILE") {
        let path = path.trim();
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    let dir = std::env::var("CREDENTIALS_DIRECTORY").ok()?;
    let path = PathBuf::from(dir).join("kmsrdp.password");
    path.is_file().then_some(path)
}

fn read_password_file() -> Result<Option<(PathBuf, String)>> {
    let Some(path) = password_file_path() else {
        return Ok(None);
    };
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read password file {}", path.display()))?;
    let password = trim_password_file(&raw);
    if password.is_empty() {
        anyhow::bail!(
            "password file {} is empty — set a password or unset KMSRDP_PASSWORD_FILE",
            path.display()
        );
    }
    Ok(Some((path, password)))
}

fn trim_password_file(raw: &str) -> String {
    raw.trim_end_matches(['\n', '\r']).to_string()
}

pub fn password_file_has_content(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .is_some_and(|raw| !trim_password_file(&raw).is_empty())
}

fn persist_oneshot_password(password: &str) -> Result<PathBuf> {
    let dir = oneshot_password_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    let path = dir.join("rdp-password");
    tls::write_secret_file(&path, password.as_bytes(), 0o600)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

fn oneshot_password_dir() -> Result<PathBuf> {
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR")
        && !runtime.is_empty()
    {
        return Ok(PathBuf::from(runtime).join("kmsrdp"));
    }
    if let Ok((cert, _)) = tls::tls_paths()
        && let Some(parent) = cert.parent()
    {
        return Ok(parent.to_path_buf());
    }
    Ok(std::env::temp_dir().join(format!("kmsrdp-{}", std::process::id())))
}

fn stderr_is_tty() -> bool {
    unsafe { libc::isatty(libc::STDERR_FILENO) == 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::env_lock;

    #[test]
    fn listen_addr_defaults_to_loopback() {
        let _guard = env_lock();
        unsafe {
            std::env::remove_var("KMSRDP_BIND");
            std::env::remove_var("KMSRDP_PORT");
        }
        let addr = listen_addr().unwrap();
        assert_eq!(addr, "127.0.0.1:3389".parse().unwrap());
    }

    #[test]
    fn listen_addr_accepts_explicit_any() {
        let _guard = env_lock();
        unsafe {
            std::env::set_var("KMSRDP_BIND", "0.0.0.0");
            std::env::set_var("KMSRDP_PORT", "3390");
        }
        let addr = listen_addr().unwrap();
        assert_eq!(addr, "0.0.0.0:3390".parse().unwrap());
        unsafe {
            std::env::remove_var("KMSRDP_BIND");
            std::env::remove_var("KMSRDP_PORT");
        }
    }

    #[test]
    fn max_sessions_defaults_to_one() {
        let _guard = env_lock();
        unsafe {
            std::env::remove_var("KMSRDP_MAX_SESSIONS");
        }
        assert_eq!(parse_max_sessions().unwrap(), 1);
    }

    #[test]
    fn max_sessions_rejects_zero() {
        let _guard = env_lock();
        unsafe {
            std::env::set_var("KMSRDP_MAX_SESSIONS", "0");
        }
        assert!(parse_max_sessions().is_err());
        unsafe {
            std::env::remove_var("KMSRDP_MAX_SESSIONS");
        }
    }

    #[test]
    fn clipboard_mode_aliases() {
        let _guard = env_lock();
        unsafe {
            std::env::set_var("KMSRDP_CLIPBOARD", "disabled");
        }
        assert_eq!(parse_clipboard_mode(), ClipboardMode::Disabled);
        unsafe {
            std::env::set_var("KMSRDP_CLIPBOARD", "read-only");
        }
        assert_eq!(parse_clipboard_mode(), ClipboardMode::HostToClient);
        unsafe {
            std::env::remove_var("KMSRDP_CLIPBOARD");
        }
        assert_eq!(parse_clipboard_mode(), ClipboardMode::Bidirectional);
    }

    #[test]
    fn require_nla_defaults_true_and_can_be_disabled() {
        let _guard = env_lock();
        unsafe {
            std::env::remove_var("KMSRDP_REQUIRE_NLA");
            std::env::remove_var("KMSRDP_FORCE_NLA");
        }
        assert_eq!(parse_bool_env("KMSRDP_REQUIRE_NLA"), None);
        unsafe {
            std::env::set_var("KMSRDP_REQUIRE_NLA", "0");
        }
        assert_eq!(parse_bool_env("KMSRDP_REQUIRE_NLA"), Some(false));
        unsafe {
            std::env::remove_var("KMSRDP_REQUIRE_NLA");
        }
    }

    #[test]
    fn max_sessions_rejects_above_cap() {
        let _guard = env_lock();
        unsafe {
            std::env::set_var("KMSRDP_MAX_SESSIONS", "33");
        }
        assert!(parse_max_sessions().is_err());
        unsafe {
            std::env::remove_var("KMSRDP_MAX_SESSIONS");
        }
    }

    #[test]
    fn config_debug_redacts_password() {
        let cfg = Config {
            listen: "127.0.0.1:3389".parse().unwrap(),
            require_nla: true,
            max_sessions: 1,
            username: "admin".to_string(),
            password: "super_secret_password".to_string(),
            password_generated: false,
            clipboard: ClipboardMode::Bidirectional,
            gfx_enabled: false,
            frame_interval: DEFAULT_FRAME_INTERVAL,
        };
        let formatted = format!("{cfg:?}");
        assert!(!formatted.contains("super_secret_password"));
        assert!(formatted.contains("[REDACTED]"));
    }

    #[test]
    fn trim_password_file_strips_trailing_newlines_only() {
        assert_eq!(trim_password_file("secret\n"), "secret");
        assert_eq!(trim_password_file("secret\r\n"), "secret");
        assert_eq!(trim_password_file(" leading space"), " leading space");
    }

    #[test]
    fn gfx_defaults_off_and_accepts_truthy() {
        let _guard = env_lock();
        unsafe {
            std::env::remove_var("KMSRDP_GFX");
        }
        assert_eq!(parse_bool_env("KMSRDP_GFX"), None);
        unsafe {
            std::env::set_var("KMSRDP_GFX", "1");
        }
        assert_eq!(parse_bool_env("KMSRDP_GFX"), Some(true));
        unsafe {
            std::env::remove_var("KMSRDP_GFX");
        }
    }

    #[test]
    fn frame_interval_prefers_fps_then_ms() {
        let _guard = env_lock();
        unsafe {
            std::env::remove_var("KMSRDP_FPS");
            std::env::remove_var("KMSRDP_FRAME_INTERVAL_MS");
        }
        assert_eq!(parse_frame_interval(), DEFAULT_FRAME_INTERVAL);
        unsafe {
            std::env::set_var("KMSRDP_FRAME_INTERVAL_MS", "40");
        }
        assert_eq!(parse_frame_interval(), Duration::from_millis(40));
        unsafe {
            std::env::set_var("KMSRDP_FPS", "20");
        }
        assert_eq!(parse_frame_interval(), Duration::from_millis(50));
        unsafe {
            std::env::remove_var("KMSRDP_FPS");
            std::env::remove_var("KMSRDP_FRAME_INTERVAL_MS");
        }
    }

    #[test]
    fn password_file_path_prefers_explicit_env() {
        let _guard = env_lock();
        unsafe {
            std::env::set_var("KMSRDP_PASSWORD_FILE", "/tmp/kmsrdp-test-password");
            std::env::set_var("CREDENTIALS_DIRECTORY", "/run/credentials/kmsrdp");
        }
        assert_eq!(
            password_file_path().as_deref(),
            Some(Path::new("/tmp/kmsrdp-test-password"))
        );
        unsafe {
            std::env::remove_var("KMSRDP_PASSWORD_FILE");
            std::env::remove_var("CREDENTIALS_DIRECTORY");
        }
    }
}
