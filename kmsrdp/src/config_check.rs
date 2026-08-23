//! Startup configuration and dependency checks.
//!
//! [`validate`] gathers hard errors (refuse to start) and soft warnings
//! (degraded features). Call [`log_report`] then bail if there are errors.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use crate::capture;
use crate::config::Config;
use crate::tls;

/// Linux capability numbers (uapi/linux/capability.h).
const CAP_DAC_OVERRIDE: u8 = 1;
const CAP_NET_BIND_SERVICE: u8 = 10;
const CAP_SYS_ADMIN: u8 = 21;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StartupReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl StartupReport {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Validate env, capabilities, devices, and helper binaries.
///
/// Credential checks use the already-parsed [`Config`] so they still work
/// after `Config::from_env` has cleared `KMSRDP_PASSWORD` from the process
/// environment.
pub fn validate(cfg: &Config) -> StartupReport {
    let mut report = StartupReport::default();

    check_credentials(&mut report, cfg);
    check_display_env(&mut report);
    check_tls_env(&mut report);
    check_capabilities(cfg.listen.port(), &mut report);
    check_devices(&mut report);
    check_pulse_capture(&mut report);
    check_helper_binaries(&mut report);
    check_fuse_conf(&mut report);

    report
}

/// Emit the report via `tracing`. Returns `Err` when hard errors are present.
pub fn log_report(report: &StartupReport) -> io::Result<()> {
    for warning in &report.warnings {
        tracing::warn!(%warning, "startup check");
    }
    for error in &report.errors {
        tracing::error!(%error, "startup check failed");
    }
    if report.ok() {
        if report.warnings.is_empty() {
            tracing::info!("startup checks passed");
        } else {
            tracing::info!(
                warnings = report.warnings.len(),
                "startup checks passed with warnings"
            );
        }
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "startup configuration invalid ({} error(s)): {}",
                report.errors.len(),
                report.errors.join("; ")
            ),
        ))
    }
}

fn check_credentials(report: &mut StartupReport, cfg: &Config) {
    if cfg.username.trim().is_empty() {
        report
            .errors
            .push("KMSRDP_USER is set but empty — set a non-empty RDP username".to_string());
    }
    if cfg.password.is_empty() {
        report.errors.push(
            "RDP password is empty — set KMSRDP_PASSWORD, KMSRDP_PASSWORD_FILE, \
             or unset them to get a generated one-shot password"
                .to_string(),
        );
    } else if cfg.password.len() < 8 {
        report.warnings.push(format!(
            "RDP password is only {} character(s); prefer a longer password",
            cfg.password.len()
        ));
    }
    if cfg.password_generated {
        report.warnings.push(
            "using a generated one-shot password; set KMSRDP_PASSWORD_FILE or KMSRDP_PASSWORD \
             for a stable credential (the secret should not live in the process environment)"
                .to_string(),
        );
    }
    if let Some(path) = crate::config::password_file_path() {
        warn_if_insecure_password_file(&path, report);
    }
}

fn warn_if_insecure_password_file(path: &Path, report: &mut StartupReport) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        report.warnings.push(format!(
            "password file {} is mode {mode:04o}; prefer 0600 so group/other cannot read it",
            path.display()
        ));
    }
}

fn check_display_env(report: &mut StartupReport) {
    if let Err(e) = capture::validate_display_env() {
        report.errors.push(e.to_string());
    }
}

fn check_tls_env(report: &mut StartupReport) {
    if let Err(e) = tls::tls_paths() {
        report.errors.push(format!("TLS path config: {e}"));
    }
}

fn check_capabilities(listen_port: u16, report: &mut StartupReport) {
    let euid = unsafe { libc::geteuid() };
    let caps = effective_caps();

    let has = |cap: u8| euid == 0 || caps.map(|c| capability_set(c, cap)).unwrap_or(false);

    if !has(CAP_SYS_ADMIN) || !has(CAP_DAC_OVERRIDE) {
        report.warnings.push(format!(
            "missing CAP_SYS_ADMIN and/or CAP_DAC_OVERRIDE (euid={euid}) — \
             DRM capture or /dev/uinput may fail; \
             run as root or: setcap cap_sys_admin,cap_dac_override,cap_net_bind_service+ep <binary>"
        ));
    }

    if requires_net_bind_capability(listen_port, euid, caps) {
        report.errors.push(format!(
            "listen port {listen_port} is privileged (<1024) and requires \
             CAP_NET_BIND_SERVICE or root (or set KMSRDP_PORT>=1024)"
        ));
    }
}

/// Privileged TCP ports need root or `CAP_NET_BIND_SERVICE`.
fn requires_net_bind_capability(listen_port: u16, euid: u32, caps: Option<u64>) -> bool {
    if listen_port == 0 || listen_port >= 1024 || euid == 0 {
        return false;
    }
    !caps
        .map(|c| capability_set(c, CAP_NET_BIND_SERVICE))
        .unwrap_or(false)
}

fn check_devices(report: &mut StartupReport) {
    let uinput = ["/dev/uinput", "/dev/input/uinput"]
        .into_iter()
        .map(Path::new)
        .find(|p| p.exists());
    match uinput {
        None => report.errors.push(
            "neither /dev/uinput nor /dev/input/uinput exists — load the uinput kernel module"
                .to_string(),
        ),
        Some(path) if !is_writable(path) => report.errors.push(format!(
            "{} is not writable by this process — need CAP_SYS_ADMIN / root or udev rules",
            path.display()
        )),
        Some(_) => {}
    }

    match fs::read_dir("/dev/dri") {
        Err(e) => report.warnings.push(format!(
            "/dev/dri unreadable ({e}) — DRM/KMS capture will fail unless NvFBC works"
        )),
        Ok(entries) => {
            let any_card = entries.flatten().any(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.starts_with("card"))
                    .unwrap_or(false)
            });
            if !any_card {
                report.warnings.push(
                    "/dev/dri has no card* nodes — DRM/KMS capture likely unavailable".to_string(),
                );
            }
        }
    }
}

fn pulse_socket_path() -> Option<PathBuf> {
    if let Ok(server) = std::env::var("PULSE_SERVER")
        && let Some(path) = server.strip_prefix("unix:")
    {
        return Some(PathBuf::from(path));
    }
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(|runtime| PathBuf::from(runtime).join("pulse/native"))
}

fn check_pulse_capture(report: &mut StartupReport) {
    match pulse_socket_path() {
        Some(path) if path.exists() => {}
        Some(path) => {
            report.warnings.push(format!(
                "PulseAudio/PipeWire socket not found at {} — RDPSND/RDPEAI audio will not work",
                path.display()
            ));
        }
        None => report.warnings.push(
            "PULSE_SERVER / XDG_RUNTIME_DIR unset — RDPSND output and RDPEAI microphone \
             may not work until a graphical session is active"
                .to_string(),
        ),
    }
}

fn check_helper_binaries(report: &mut StartupReport) {
    if !command_on_path("fusermount3") && !command_on_path("fusermount") {
        report.warnings.push(
            "`fusermount3`/`fusermount` not found on PATH — client drive redirection \
             (RDPDR FUSE) may fail"
                .to_string(),
        );
    }
}

fn check_fuse_conf(report: &mut StartupReport) {
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let conf = Path::new("/etc/fuse.conf");
    let Ok(text) = fs::read_to_string(conf) else {
        report.warnings.push(
            "/etc/fuse.conf missing — root FUSE mounts for drive redirection may need \
             `user_allow_other`"
                .to_string(),
        );
        return;
    };
    let enabled = fuse_user_allow_other_enabled(&text);
    if !enabled {
        report.warnings.push(
            "/etc/fuse.conf has no active `user_allow_other` — root FUSE drive mounts \
             often fail without it"
                .to_string(),
        );
    }
}

/// Whether `/etc/fuse.conf` enables `user_allow_other` (comments ignored).
fn fuse_user_allow_other_enabled(conf_text: &str) -> bool {
    conf_text.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == "user_allow_other" || trimmed.starts_with("user_allow_other ")
    })
}

fn is_writable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    let Ok(cstr) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(cstr.as_ptr(), libc::W_OK) == 0 }
}

fn command_on_path(name: &str) -> bool {
    let Ok(path_var) = std::env::var("PATH") else {
        return PathBuf::from("/usr/bin").join(name).is_file()
            || PathBuf::from("/bin").join(name).is_file();
    };
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            let mode = fs::metadata(&candidate)
                .map(|m| m.permissions().mode())
                .unwrap_or(0);
            // Owner/group/other execute bit.
            if mode & 0o111 != 0 {
                return true;
            }
        }
    }
    false
}

fn effective_caps() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("CapEff:") else {
            continue;
        };
        return u64::from_str_radix(rest.trim(), 16).ok();
    }
    None
}

fn capability_set(cap_eff: u64, cap: u8) -> bool {
    cap_eff & (1u64 << cap) != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::ClipboardMode;
    use crate::config::DEFAULT_FRAME_INTERVAL;
    use crate::test_env::env_lock;
    use std::fs;

    fn test_cfg(username: &str, password: &str, password_generated: bool) -> Config {
        Config {
            listen: "127.0.0.1:3389".parse().unwrap(),
            require_nla: true,
            max_sessions: 1,
            username: username.to_owned(),
            password: password.to_owned(),
            password_generated,
            clipboard: ClipboardMode::Bidirectional,
            gfx_enabled: false,
            frame_interval: DEFAULT_FRAME_INTERVAL,
        }
    }

    fn clear_password_env() {
        unsafe {
            std::env::remove_var("KMSRDP_PASSWORD");
            std::env::remove_var("KMSRDP_PASSWORD_FILE");
            std::env::remove_var("CREDENTIALS_DIRECTORY");
            std::env::remove_var("KMSRDP_USER");
            std::env::remove_var("KMSRDP_DISPLAY");
            std::env::remove_var("KMSRDP_TLS_CERT");
            std::env::remove_var("KMSRDP_TLS_KEY");
        }
    }

    #[test]
    fn capability_bit_sys_admin() {
        assert!(capability_set(1u64 << CAP_SYS_ADMIN, CAP_SYS_ADMIN));
        assert!(!capability_set(0, CAP_SYS_ADMIN));
        assert!(capability_set(
            1u64 << CAP_NET_BIND_SERVICE,
            CAP_NET_BIND_SERVICE
        ));
    }

    #[test]
    fn empty_password_is_hard_error() {
        let _guard = env_lock();
        clear_password_env();
        let report = validate(&test_cfg("kmsrdp", "", false));
        assert!(
            report.errors.iter().any(|e| e.contains("password is empty")),
            "{report:?}"
        );
    }

    #[test]
    fn loaded_password_does_not_warn_about_oneshot() {
        let _guard = env_lock();
        clear_password_env();
        let report = validate(&test_cfg("kmsrdp", "a-real-password", false));
        assert!(
            !report
                .warnings
                .iter()
                .any(|w| w.contains("one-shot") || w.contains("generated")),
            "unexpected oneshot warning: {report:?}"
        );
        assert!(
            !report.errors.iter().any(|e| e.contains("password")),
            "{report:?}"
        );
    }

    #[test]
    fn short_loaded_password_is_a_warning() {
        let _guard = env_lock();
        clear_password_env();
        let report = validate(&test_cfg("kmsrdp", "short", false));
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("only 5 character")),
            "{report:?}"
        );
    }

    #[test]
    fn world_readable_password_file_is_a_warning() {
        let _guard = env_lock();
        clear_password_env();
        let dir = std::env::temp_dir().join(format!(
            "kmsrdp-pwfile-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rdp-password");
        fs::write(&path, b"a-real-password").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        unsafe {
            std::env::set_var("KMSRDP_PASSWORD_FILE", &path);
        }
        let report = validate(&test_cfg("kmsrdp", "a-real-password", false));
        unsafe {
            std::env::remove_var("KMSRDP_PASSWORD_FILE");
        }
        let _ = fs::remove_dir_all(&dir);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("prefer 0600") && w.contains("0644")),
            "{report:?}"
        );
    }

    #[test]
    fn privileged_port_requires_net_bind_when_unprivileged() {
        assert!(requires_net_bind_capability(80, 1000, Some(0)));
        assert!(requires_net_bind_capability(443, 1000, None));
        assert!(!requires_net_bind_capability(80, 0, Some(0)));
        assert!(!requires_net_bind_capability(3389, 1000, Some(0)));
        assert!(!requires_net_bind_capability(3390, 1000, Some(0)));
        assert!(!requires_net_bind_capability(
            80,
            1000,
            Some(1u64 << CAP_NET_BIND_SERVICE)
        ));
    }

    #[test]
    fn command_on_path_finds_sh() {
        assert!(command_on_path("sh"));
        assert!(!command_on_path("kmsrdp-definitely-missing-binary-xyz"));
    }

    #[test]
    fn empty_user_is_hard_error() {
        let _guard = env_lock();
        clear_password_env();
        let report = validate(&test_cfg("   ", "a-real-password", false));
        assert!(
            report.errors.iter().any(|e| e.contains("KMSRDP_USER")),
            "{report:?}"
        );
    }

    #[test]
    fn log_report_ok_when_no_errors() {
        let report = StartupReport {
            warnings: vec!["warn".to_string()],
            errors: Vec::new(),
        };
        assert!(log_report(&report).is_ok());
    }

    #[test]
    fn log_report_err_when_errors_present() {
        let report = StartupReport {
            warnings: Vec::new(),
            errors: vec!["bad".to_string()],
        };
        let err = log_report(&report).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("bad"));
    }

    #[test]
    fn fuse_user_allow_other_parses_comments() {
        let conf = "# comment\n#user_allow_other\nuser_allow_other\n";
        assert!(fuse_user_allow_other_enabled(conf));
        assert!(!fuse_user_allow_other_enabled("# user_allow_other\n"));
        assert!(!fuse_user_allow_other_enabled(""));
    }

    #[test]
    fn pulse_socket_path_prefers_pulse_server() {
        let _guard = env_lock();
        unsafe {
            std::env::set_var("PULSE_SERVER", "unix:/run/user/42/pulse/native");
            std::env::set_var("XDG_RUNTIME_DIR", "/run/user/99");
        }
        assert_eq!(
            pulse_socket_path(),
            Some(PathBuf::from("/run/user/42/pulse/native"))
        );
        unsafe {
            std::env::remove_var("PULSE_SERVER");
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
    }

    #[test]
    fn pulse_socket_path_falls_back_to_runtime_dir() {
        let _guard = env_lock();
        unsafe {
            std::env::remove_var("PULSE_SERVER");
            std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        }
        assert_eq!(
            pulse_socket_path(),
            Some(PathBuf::from("/run/user/1000/pulse/native"))
        );
        unsafe {
            std::env::remove_var("XDG_RUNTIME_DIR");
        }
    }
}
