//! TLS identity for the RDP listener.
//!
//! By default a self-signed certificate and key are created once and reused
//! across restarts (so clients can pin the fingerprint). Override paths with
//! `KMSRDP_TLS_CERT` / `KMSRDP_TLS_KEY` or `KMSRDP_TLS_DIR`, or set
//! `KMSRDP_TLS_EPHEMERAL=1` to regenerate every start (legacy behaviour).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rdpcore_server::tokio_rustls::TlsAcceptor;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const CERT_FILE_NAME: &str = "tls.crt";
const KEY_FILE_NAME: &str = "tls.key";

/// TLS acceptor plus the subjectPublicKey bytes CredSSP needs for `pubKeyAuth`.
pub struct TlsIdentity {
    pub acceptor: TlsAcceptor,
    /// X.509 SubjectPublicKeyInfo *subjectPublicKey* BIT STRING contents
    /// (not the full SPKI wrapper). FreeRDP/Guacamole and Windows CredSSP
    /// hash this blob in `pubKeyAuth` (via `i2d_PublicKey` / equivalent).
    pub public_key: Vec<u8>,
}

pub fn build_acceptor() -> io::Result<TlsIdentity> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let hostnames = tls_hostnames();
    tracing::info!(
        "kmsrdp: TLS certificate hostnames: {}",
        hostnames.join(", ")
    );

    let (cert_der, key_der, public_key) = if ephemeral_requested() {
        tracing::info!("kmsrdp: TLS identity is ephemeral (KMSRDP_TLS_EPHEMERAL)");
        generate_identity(&hostnames)?
    } else {
        load_or_create_identity(&hostnames)?
    };

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .map_err(|e| io::Error::other(format!("TLS config failed: {e}")))?;

    Ok(TlsIdentity {
        acceptor: TlsAcceptor::from(Arc::new(config)),
        public_key,
    })
}

fn ephemeral_requested() -> bool {
    matches!(
        std::env::var("KMSRDP_TLS_EPHEMERAL").as_deref(),
        Ok("1" | "true" | "yes" | "on")
    )
}

fn generate_identity(
    hostnames: &[String],
) -> io::Result<(CertificateDer<'static>, PrivateKeyDer<'static>, Vec<u8>)> {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(hostnames.to_vec())
            .map_err(|e| io::Error::other(format!("certificate generation failed: {e}")))?;

    let public_key = signing_key.public_key_raw().to_vec();
    let cert_der: CertificateDer<'static> = cert.der().clone();
    let key_der: PrivateKeyDer<'static> =
        PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into();
    Ok((cert_der, key_der, public_key))
}

fn load_or_create_identity(
    hostnames: &[String],
) -> io::Result<(CertificateDer<'static>, PrivateKeyDer<'static>, Vec<u8>)> {
    let (cert_path, key_path) = tls_paths()?;

    if cert_path.is_file() && key_path.is_file() {
        match load_identity_files(&cert_path, &key_path) {
            Ok(identity) => {
                tracing::info!(
                    "kmsrdp: loaded TLS identity from {} and {}",
                    cert_path.display(),
                    key_path.display()
                );
                return Ok(identity);
            }
            Err(e) => {
                tracing::warn!(
                    "kmsrdp: failed to load TLS identity from {} / {} ({e}); regenerating",
                    cert_path.display(),
                    key_path.display()
                );
            }
        }
    } else if cert_path.exists() || key_path.exists() {
        tracing::info!(
            "kmsrdp: incomplete TLS identity (need both {} and {}); regenerating",
            cert_path.display(),
            key_path.display()
        );
    }

    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(hostnames.to_vec())
            .map_err(|e| io::Error::other(format!("certificate generation failed: {e}")))?;

    if let Err(e) = persist_identity(
        &cert_path,
        &key_path,
        &cert.pem(),
        &signing_key.serialize_pem(),
    ) {
        tracing::info!(
            "kmsrdp: warning: could not persist TLS identity to {} / {} ({e}); continuing with in-memory cert",
            cert_path.display(),
            key_path.display()
        );
    } else {
        tracing::info!(
            "kmsrdp: persisted TLS identity to {} and {}",
            cert_path.display(),
            key_path.display()
        )
    }

    let public_key = signing_key.public_key_raw().to_vec();
    let cert_der: CertificateDer<'static> = cert.der().clone();
    let key_der: PrivateKeyDer<'static> =
        PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into();
    Ok((cert_der, key_der, public_key))
}

fn load_identity_files(
    cert_path: &Path,
    key_path: &Path,
) -> io::Result<(CertificateDer<'static>, PrivateKeyDer<'static>, Vec<u8>)> {
    let cert_pem = fs::read(cert_path)?;
    let key_pem = fs::read_to_string(key_path)?;

    let cert_der = CertificateDer::from_pem_slice(&cert_pem)
        .map_err(|e| io::Error::other(format!("parse TLS certificate PEM: {e}")))?;
    let key_der = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .map_err(|e| io::Error::other(format!("parse TLS private key PEM: {e}")))?;
    let key_pair = rcgen::KeyPair::from_pem(&key_pem)
        .map_err(|e| io::Error::other(format!("parse TLS key for CredSSP pubkey: {e}")))?;

    Ok((cert_der, key_der, key_pair.public_key_raw().to_vec()))
}

fn persist_identity(
    cert_path: &Path,
    key_path: &Path,
    cert_pem: &str,
    key_pem: &str,
) -> io::Result<()> {
    if let Some(parent) = cert_path.parent() {
        fs::create_dir_all(parent)?;
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    if let Some(parent) = key_path.parent() {
        fs::create_dir_all(parent)?;
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }

    write_private_file(cert_path, cert_pem.as_bytes(), 0o644)?;
    write_private_file(key_path, key_pem.as_bytes(), 0o600)?;
    Ok(())
}

fn write_private_file(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let tmp = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("kmsrdp-tls")
    ));

    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Resolve certificate and key paths.
///
/// Precedence:
/// 1. `KMSRDP_TLS_CERT` + `KMSRDP_TLS_KEY` (both required if either is set)
/// 2. `KMSRDP_TLS_DIR` / `{tls.crt,tls.key}`
/// 3. `$STATE_DIRECTORY` (systemd `StateDirectory=`) / `{tls.crt,tls.key}`
/// 4. root → `/var/lib/kmsrdp`; else `$XDG_STATE_HOME/kmsrdp` or `~/.local/state/kmsrdp`
pub fn tls_paths() -> io::Result<(PathBuf, PathBuf)> {
    let cert_env = std::env::var_os("KMSRDP_TLS_CERT");
    let key_env = std::env::var_os("KMSRDP_TLS_KEY");
    match (cert_env, key_env) {
        (Some(cert), Some(key)) => return Ok((PathBuf::from(cert), PathBuf::from(key))),
        (None, None) => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "set both KMSRDP_TLS_CERT and KMSRDP_TLS_KEY, or neither",
            ));
        }
    }

    let dir = if let Ok(dir) = std::env::var("KMSRDP_TLS_DIR") {
        PathBuf::from(dir)
    } else if let Ok(state) = std::env::var("STATE_DIRECTORY") {
        // systemd may pass multiple dirs separated by `:`; use the first.
        PathBuf::from(state.split(':').next().unwrap_or(&state))
    } else {
        default_tls_dir()?
    };

    Ok((dir.join(CERT_FILE_NAME), dir.join(KEY_FILE_NAME)))
}

fn default_tls_dir() -> io::Result<PathBuf> {
    if unsafe { libc::geteuid() } == 0 {
        return Ok(PathBuf::from("/var/lib/kmsrdp"));
    }
    if let Ok(state) = std::env::var("XDG_STATE_HOME")
        && !state.is_empty()
    {
        return Ok(PathBuf::from(state).join("kmsrdp"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "HOME unset; set KMSRDP_TLS_DIR or KMSRDP_TLS_CERT/KEY",
        )
    })?;
    Ok(PathBuf::from(home).join(".local/state/kmsrdp"))
}

/// Subject Alternative Names placed in a newly generated self-signed certificate.
/// `localhost` is always included; set `KMSRDP_TLS_HOSTS` to a comma-separated
/// list of extra hostnames or IP literals the RDP client will connect to
/// (e.g. `192.168.101.10`) so mstsc's hostname check can pass.
fn tls_hostnames() -> Vec<String> {
    let mut hosts = vec!["localhost".to_owned()];
    if let Ok(extra) = std::env::var("KMSRDP_TLS_HOSTS") {
        for host in extra.split(',') {
            let host = host.trim();
            if host.is_empty() || hosts.iter().any(|h| h == host) {
                continue;
            }
            hosts.push(host.to_owned());
        }
    }
    hosts
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
    fn persist_and_reload_keeps_same_public_key() {
        let _guard = env_lock();
        let dir = tempfile_dir();
        let cert_path = dir.join(CERT_FILE_NAME);
        let key_path = dir.join(KEY_FILE_NAME);

        unsafe {
            std::env::remove_var("KMSRDP_TLS_EPHEMERAL");
            std::env::set_var("KMSRDP_TLS_CERT", &cert_path);
            std::env::set_var("KMSRDP_TLS_KEY", &key_path);
        }

        let hosts = vec!["localhost".to_owned()];
        let (_, _, pk1) = load_or_create_identity(&hosts).expect("create");
        assert!(cert_path.is_file());
        assert!(key_path.is_file());
        let mode = fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let (_, _, pk2) = load_or_create_identity(&hosts).expect("reload");
        assert_eq!(pk1, pk2);

        unsafe {
            std::env::remove_var("KMSRDP_TLS_CERT");
            std::env::remove_var("KMSRDP_TLS_KEY");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tls_paths_require_both_explicit_env_vars() {
        let _guard = env_lock();
        unsafe {
            std::env::set_var("KMSRDP_TLS_CERT", "/tmp/a.crt");
            std::env::remove_var("KMSRDP_TLS_KEY");
        }
        let err = tls_paths().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        unsafe {
            std::env::remove_var("KMSRDP_TLS_CERT");
        }
    }

    fn tempfile_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "kmsrdp-tls-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
