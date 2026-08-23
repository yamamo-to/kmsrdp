//! Pure-Rust integration with systemd service notification and watchdog protocol.
//!
//! Uses `$NOTIFY_SOCKET` directly over a UNIX datagram socket without external C libraries.

use std::env;
use std::os::unix::net::UnixDatagram;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Send a raw state string to systemd via `$NOTIFY_SOCKET`.
///
/// Returns `Ok(false)` if `$NOTIFY_SOCKET` is not set (not running under systemd with Type=notify).
pub fn notify(state: &str) -> std::io::Result<bool> {
    let socket_path = match env::var_os("NOTIFY_SOCKET") {
        Some(path) => path,
        None => return Ok(false),
    };

    send_to_socket(state, &socket_path)?;
    Ok(true)
}

fn send_to_socket(state: &str, socket_path: &std::ffi::OsStr) -> std::io::Result<()> {
    let sock = UnixDatagram::unbound()?;
    let path_bytes = socket_path.as_encoded_bytes();

    if let Some(rest) = path_bytes.strip_prefix(b"@") {
        // Abstract socket path: replace leading '@' with NUL byte
        let mut addr_bytes = Vec::with_capacity(rest.len() + 1);
        addr_bytes.push(0u8);
        addr_bytes.extend_from_slice(rest);
        use std::os::unix::ffi::OsStrExt;
        let os_addr = std::ffi::OsStr::from_bytes(&addr_bytes);
        sock.send_to(state.as_bytes(), os_addr)?;
    } else {
        sock.send_to(state.as_bytes(), socket_path)?;
    }
    Ok(())
}

/// Notify systemd that the service has finished initialization and is ready (`READY=1`).
pub fn notify_ready() {
    match notify("READY=1") {
        Ok(true) => info!("Notified systemd: READY=1"),
        Ok(false) => debug!("systemd notification skipped: NOTIFY_SOCKET not set"),
        Err(e) => warn!(error = %e, "Failed to send READY=1 to systemd"),
    }
}

/// Notify systemd that the service is alive (`WATCHDOG=1`).
pub fn notify_watchdog() {
    if let Err(e) = notify("WATCHDOG=1") {
        warn!(error = %e, "Failed to send WATCHDOG=1 to systemd");
    }
}

/// Notify systemd that the service is shutting down (`STOPPING=1`).
pub fn notify_stopping() {
    let _ = notify("STOPPING=1");
}

/// Spawns a background task to periodically pet the systemd watchdog if `$WATCHDOG_USEC` is configured.
///
/// Systemd convention recommends sending notifications at half the watchdog timeout period.
pub fn spawn_watchdog_task() -> Option<tokio::task::JoinHandle<()>> {
    let usec_str = env::var("WATCHDOG_USEC").ok()?;
    let usec: u64 = usec_str.parse().ok()?;
    if usec == 0 {
        return None;
    }

    let interval_ms = (usec / 1000) / 2;
    let interval = Duration::from_millis(interval_ms.max(100));

    info!(
        watchdog_interval_sec = interval.as_secs_f64(),
        "Starting systemd watchdog task"
    );

    let handle = tokio::spawn(async move {
        let mut timer = tokio::time::interval(interval);
        loop {
            timer.tick().await;
            notify_watchdog();
        }
    });

    Some(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_to_socket_delivers_payload() {
        let temp_dir = std::env::temp_dir();
        let sock_path = temp_dir.join(format!("test_systemd_notify_{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);

        let server_sock = UnixDatagram::bind(&sock_path).expect("bind server unix socket");
        send_to_socket("READY=1", sock_path.as_os_str()).expect("send notification");

        let mut buf = [0u8; 64];
        let (len, _) = server_sock
            .recv_from(&mut buf)
            .expect("receive notification");
        assert_eq!(&buf[..len], b"READY=1");

        let _ = std::fs::remove_file(&sock_path);
    }
}
