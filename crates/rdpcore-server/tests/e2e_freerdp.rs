use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rdpcore_server::tokio_rustls::TlsAcceptor;
use rdpcore_server::tokio_rustls::rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer,
};
use rdpcore_server::tokio_rustls::rustls::{self};
use rdpcore_server::{
    BitmapUpdate, Credentials, DesktopSize, DisplayUpdate, ExactMatchCredentialValidator,
    KeyboardEvent, MouseEvent, PixelFormat, RdpServer, RdpServerDisplay, RdpServerDisplayUpdates,
    RdpServerInputHandler,
};
use tokio::net::TcpListener;
use tokio::process::Command;

struct DummyDisplayUpdates {
    sent: bool,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for DummyDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>, rdpcore_server::DisplayError> {
        if !self.sent {
            self.sent = true;
            let width = core::num::NonZeroU16::new(1024).unwrap();
            let height = core::num::NonZeroU16::new(768).unwrap();
            let stride = core::num::NonZeroUsize::new(1024 * 4).unwrap();
            let data = vec![0u8; 1024 * 768 * 4];
            Ok(Some(DisplayUpdate::Bitmap(BitmapUpdate {
                x: 0,
                y: 0,
                width,
                height,
                format: PixelFormat::BgrX32,
                data: Arc::from(data),
                stride,
                src_x: 0,
                src_y: 0,
            })))
        } else {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(None)
        }
    }

    fn latest_full_frame(&self) -> Option<BitmapUpdate> {
        None
    }
}

struct DummyDisplay;

#[async_trait::async_trait]
impl RdpServerDisplay for DummyDisplay {
    async fn size(&self) -> DesktopSize {
        DesktopSize {
            width: 1024,
            height: 768,
        }
    }

    async fn updates(
        &self,
    ) -> Result<Box<dyn RdpServerDisplayUpdates>, rdpcore_server::DisplayError> {
        Ok(Box::new(DummyDisplayUpdates { sent: false }))
    }
}

#[derive(Default)]
struct TestInputHandler {
    keyboard_events: AtomicUsize,
    mouse_events: AtomicUsize,
}

impl RdpServerInputHandler for TestInputHandler {
    fn keyboard(&mut self, _event: KeyboardEvent) {
        self.keyboard_events.fetch_add(1, Ordering::SeqCst);
    }
    fn mouse(&mut self, _event: MouseEvent) {
        self.mouse_events.fetch_add(1, Ordering::SeqCst);
    }
    fn reset(&mut self) {}
}

fn create_tls_acceptor_and_pubkey() -> (TlsAcceptor, Vec<u8>) {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
            .expect("self-signed cert");
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let public_key = signing_key.public_key_raw().to_vec();
    let key_der: PrivateKeyDer<'static> =
        PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into();

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("tls server config");
    (TlsAcceptor::from(Arc::new(config)), public_key)
}

fn command_exists(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn test_freerdp_e2e_connection() {
    if !command_exists("xfreerdp") || !command_exists("Xvfb") {
        eprintln!("xfreerdp or Xvfb not available, skipping test");
        return;
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let (tls, pub_key) = create_tls_acceptor_and_pubkey();
    let creds = Credentials {
        username: "testuser".to_string(),
        password: "testpassword".to_string(),
        domain: None,
    };
    let validator = Arc::new(ExactMatchCredentialValidator::new(creds.clone()));
    let input = TestInputHandler::default();

    let server = RdpServer::builder()
        .with_listener(listener)
        .with_tls(tls)
        .with_tls_public_key(pub_key)
        .with_display_handler(DummyDisplay)
        .with_input_handler(input)
        .with_credential_validator(Some(validator))
        .with_nla_credentials(Some(creds))
        .with_require_nla(true)
        .build();

    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    let display_num = 99 + (port % 500) as i32;
    let display_str = format!(":{display_num}");
    let mut xvfb = Command::new("Xvfb")
        .arg(&display_str)
        .arg("-screen")
        .arg("0")
        .arg("1024x768x24")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start Xvfb");

    tokio::time::sleep(Duration::from_millis(500)).await;

    let client_output = Command::new("xfreerdp")
        .env("DISPLAY", &display_str)
        .arg(format!("/v:127.0.0.1:{port}"))
        .arg("/u:testuser")
        .arg("/p:testpassword")
        .arg("/cert:ignore")
        .arg("+auth-only")
        .output();

    let result = tokio::time::timeout(Duration::from_secs(10), client_output).await;

    let _ = xvfb.kill().await;
    server_task.abort();

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            println!("xfreerdp stdout:\n{stdout}");
            println!("xfreerdp stderr:\n{stderr}");
            assert!(
                output.status.success(),
                "xfreerdp failed with exit status {:?}\nstderr:\n{}",
                output.status,
                stderr
            );
        }
        Ok(Err(e)) => panic!("xfreerdp failed to spawn/run: {e}"),
        Err(_) => panic!("xfreerdp timed out (NLA auth-only took > 10s)"),
    }
}

#[tokio::test]
async fn test_freerdp_e2e_streaming() {
    if !command_exists("xfreerdp") || !command_exists("Xvfb") {
        eprintln!("xfreerdp or Xvfb not available, skipping test");
        return;
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let (tls, pub_key) = create_tls_acceptor_and_pubkey();
    let creds = Credentials {
        username: "testuser".to_string(),
        password: "testpassword".to_string(),
        domain: None,
    };
    let validator = Arc::new(ExactMatchCredentialValidator::new(creds.clone()));
    let input = TestInputHandler::default();

    let server = RdpServer::builder()
        .with_listener(listener)
        .with_tls(tls)
        .with_tls_public_key(pub_key)
        .with_display_handler(DummyDisplay)
        .with_input_handler(input)
        .with_credential_validator(Some(validator))
        .with_nla_credentials(Some(creds))
        .with_require_nla(true)
        .build();

    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    let display_num = 99 + (port % 500) as i32;
    let display_str = format!(":{display_num}");
    let mut xvfb = Command::new("Xvfb")
        .arg(&display_str)
        .arg("-screen")
        .arg("0")
        .arg("1024x768x24")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start Xvfb");

    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut client = Command::new("xfreerdp")
        .env("DISPLAY", &display_str)
        .arg(format!("/v:127.0.0.1:{port}"))
        .arg("/u:testuser")
        .arg("/p:testpassword")
        .arg("/cert:ignore")
        .arg("/bpp:32")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn xfreerdp");

    // Let it stream frames for 2 seconds
    tokio::time::sleep(Duration::from_secs(2)).await;

    let _ = client.kill().await;
    let _ = xvfb.kill().await;
    server_task.abort();
}
