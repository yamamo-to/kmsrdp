//! Process-wide settings parsed from `KMSRDP_*` environment variables.
//!
//! Defaults are conservative: loopback listen, NLA required, one
//! authenticated session. Override explicitly to expose the service.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context, Result};

use std::os::unix::fs::PermissionsExt as _;

use crate::clipboard::ClipboardMode;
use crate::tls;

/// Runtime configuration gathered once at startup.
#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub require_nla: bool,
    pub max_sessions: usize,
    pub username: String,
    pub password: String,
    pub password_generated: bool,
    pub clipboard: ClipboardMode,
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
        Ok(Self {
            listen,
            require_nla,
            max_sessions,
            username,
            password,
            password_generated,
            clipboard,
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
            Ok(n)
        }
        Err(_) => Ok(1),
    }
}

fn parse_bool_env(name: &str) -> Option<bool> {
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

fn parse_clipboard_mode() -> ClipboardMode {
    match std::env::var("KMSRDP_CLIPBOARD")
        .unwrap_or_else(|_| "bidirectional".to_string())
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "disabled" | "off" | "0" | "false" => ClipboardMode::Disabled,
        "host-to-client" | "read-only" | "readonly" => ClipboardMode::HostToClient,
        "client-to-host" => ClipboardMode::ClientToHost,
        _ => ClipboardMode::Bidirectional,
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
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

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
}
