//! A fresh self-signed TLS identity, generated once per process start.
//!
//! Clients will show a "can't verify this certificate" warning (there's no
//! CA and nothing pins the key across restarts) - accept it once, e.g.
//! `xfreerdp ... /cert:ignore`. Good enough to stop sending the whole
//! session in plaintext; a real deployment would load a persisted
//! cert/key pair (or one from a real CA) instead of generating one here.

use std::io;
use std::sync::Arc;

use rdpcore_server::tokio_rustls::TlsAcceptor;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

pub fn build_acceptor() -> io::Result<TlsAcceptor> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let hostnames = tls_hostnames();
    eprintln!("kmsrdp: TLS certificate hostnames: {}", hostnames.join(", "));

    let rcgen::CertifiedKey { cert, signing_key } = rcgen::generate_simple_self_signed(hostnames)
        .map_err(|e| io::Error::other(format!("certificate generation failed: {e}")))?;

    let cert_der: CertificateDer<'static> = cert.der().clone();
    let key_der: PrivateKeyDer<'static> = PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into();

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .map_err(|e| io::Error::other(format!("TLS config failed: {e}")))?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Subject Alternative Names placed in the per-run self-signed certificate.
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
