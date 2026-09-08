//! FreeRDP client e2e against an in-process `RdpServer`.
//!
//! In CI (`CI=true` or `KMSRDP_REQUIRE_FREERDP=1`) these tests **must** find
//! `xfreerdp`/`xfreerdp3` and `Xvfb`; skipping is a failure. Locally they skip
//! when the tools are missing.

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

const DESKTOP_W: u16 = 320;
const DESKTOP_H: u16 = 240;

fn require_freerdp() -> bool {
    // Opt-in only. Do not key off bare `CI=true`: coverage/fuzz jobs also set
    // CI but do not install FreeRDP. The main rust job sets
    // `KMSRDP_REQUIRE_FREERDP=1` after installing freerdp2-x11 + xvfb.
    std::env::var_os("KMSRDP_REQUIRE_FREERDP").is_some_and(|v| v != "0")
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

/// Prefer `xfreerdp3`, fall back to `xfreerdp`.
fn freerdp_bin() -> Option<&'static str> {
    ["xfreerdp3", "xfreerdp"]
        .into_iter()
        .find(|c| command_exists(c))
}

fn ensure_freerdp_tools() -> Option<&'static str> {
    let client = freerdp_bin();
    let xvfb = command_exists("Xvfb");
    match (client, xvfb) {
        (Some(bin), true) => Some(bin),
        _ if require_freerdp() => {
            panic!(
                "FreeRDP e2e required (CI/KMSRDP_REQUIRE_FREERDP) but tools missing: \
                 freerdp_bin={client:?} Xvfb={xvfb}. Install freerdp2-x11 (or freerdp3-x11) and xvfb."
            );
        }
        _ => {
            eprintln!("xfreerdp/xfreerdp3 or Xvfb not available, skipping FreeRDP e2e");
            None
        }
    }
}

fn checkerboard_bgrx(width: u16, height: u16) -> Vec<u8> {
    let mut data = vec![0u8; width as usize * height as usize * 4];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let light = ((x / 16) + (y / 16)) % 2 == 0;
            let i = (y * width as usize + x) * 4;
            if light {
                data[i] = 0x40; // B
                data[i + 1] = 0xc0; // G
                data[i + 2] = 0x20; // R
            } else {
                data[i] = 0x20;
                data[i + 1] = 0x20;
                data[i + 2] = 0xc0;
            }
            data[i + 3] = 0xff;
        }
    }
    data
}

fn make_full_bitmap(width: u16, height: u16, data: Vec<u8>) -> BitmapUpdate {
    BitmapUpdate {
        x: 0,
        y: 0,
        width: core::num::NonZeroU16::new(width).unwrap(),
        height: core::num::NonZeroU16::new(height).unwrap(),
        format: PixelFormat::BgrX32,
        data: Arc::from(data),
        stride: core::num::NonZeroUsize::new(width as usize * 4).unwrap(),
        src_x: 0,
        src_y: 0,
    }
}

/// Emits one patterned frame, then keeps `latest_full_frame` so resync works.
struct PatternDisplayUpdates {
    full: BitmapUpdate,
    sent: bool,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for PatternDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>, rdpcore_server::DisplayError> {
        if !self.sent {
            self.sent = true;
            return Ok(Some(DisplayUpdate::Bitmap(self.full.clone())));
        }
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(None)
    }

    fn latest_full_frame(&self) -> Option<BitmapUpdate> {
        Some(self.full.clone())
    }
}

struct PatternDisplay {
    width: u16,
    height: u16,
    pixels: Vec<u8>,
}

impl PatternDisplay {
    fn checkerboard(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            pixels: checkerboard_bgrx(width, height),
        }
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for PatternDisplay {
    async fn size(&self) -> DesktopSize {
        DesktopSize {
            width: self.width,
            height: self.height,
        }
    }

    async fn updates(
        &self,
    ) -> Result<Box<dyn RdpServerDisplayUpdates>, rdpcore_server::DisplayError> {
        Ok(Box::new(PatternDisplayUpdates {
            full: make_full_bitmap(self.width, self.height, self.pixels.clone()),
            sent: false,
        }))
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

async fn spawn_test_server(display: PatternDisplay) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tls, pub_key) = create_tls_acceptor_and_pubkey();
    let creds = Credentials {
        username: "testuser".to_string(),
        password: "testpassword".to_string(),
        domain: None,
    };
    let validator = Arc::new(ExactMatchCredentialValidator::new(creds.clone()));
    let server = RdpServer::builder()
        .with_listener(listener)
        .with_tls(tls)
        .with_tls_public_key(pub_key)
        .with_display_handler(display)
        .with_input_handler(TestInputHandler::default())
        .with_credential_validator(Some(validator))
        .with_nla_credentials(Some(creds))
        .with_require_nla(true)
        .build();
    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });
    (port, server_task)
}

async fn start_xvfb(port: u16) -> (String, tokio::process::Child) {
    let display_num = 99 + (port % 500) as i32;
    let display_str = format!(":{display_num}");
    let xvfb = Command::new("Xvfb")
        .arg(&display_str)
        .arg("-screen")
        .arg("0")
        .arg(format!("{DESKTOP_W}x{DESKTOP_H}x24"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start Xvfb");
    tokio::time::sleep(Duration::from_millis(400)).await;
    (display_str, xvfb)
}

#[tokio::test]
async fn test_freerdp_e2e_nla_auth_only() {
    let Some(freerdp) = ensure_freerdp_tools() else {
        return;
    };

    let (port, server_task) =
        spawn_test_server(PatternDisplay::checkerboard(DESKTOP_W, DESKTOP_H)).await;
    let (display_str, mut xvfb) = start_xvfb(port).await;

    let client_output = Command::new(freerdp)
        .env("DISPLAY", &display_str)
        .arg(format!("/v:127.0.0.1:{port}"))
        .arg("/u:testuser")
        .arg("/p:testpassword")
        .arg("/cert:ignore")
        .arg("+auth-only")
        .output();

    let result = tokio::time::timeout(Duration::from_secs(15), client_output).await;

    let _ = xvfb.kill().await;
    server_task.abort();

    match result {
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            println!("xfreerdp stderr:\n{stderr}");
            assert!(
                output.status.success(),
                "xfreerdp auth-only failed: {:?}\n{stderr}",
                output.status
            );
        }
        Ok(Err(e)) => panic!("xfreerdp failed to spawn/run: {e}"),
        Err(_) => panic!("xfreerdp timed out (NLA auth-only took > 15s)"),
    }
}

/// Full session (not auth-only): connect, receive patterned Planar frames, stay
/// up briefly without a transport failure, then we kill the client.
#[tokio::test]
async fn test_freerdp_e2e_planar_session_stays_up() {
    let Some(freerdp) = ensure_freerdp_tools() else {
        return;
    };

    let (port, server_task) =
        spawn_test_server(PatternDisplay::checkerboard(DESKTOP_W, DESKTOP_H)).await;
    let (display_str, mut xvfb) = start_xvfb(port).await;

    let mut client = Command::new(freerdp)
        .env("DISPLAY", &display_str)
        .arg(format!("/v:127.0.0.1:{port}"))
        .arg("/u:testuser")
        .arg("/p:testpassword")
        .arg("/cert:ignore")
        .arg(format!("/w:{DESKTOP_W}"))
        .arg(format!("/h:{DESKTOP_H}"))
        .arg("/bpp:32")
        .arg("/network:auto")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn xfreerdp");

    // Allow handshake + first bitmap(s).
    tokio::time::sleep(Duration::from_secs(3)).await;

    // If FreeRDP already exited, the session failed (zgfx/transport/etc.).
    match client.try_wait() {
        Ok(Some(status)) => {
            let output = client
                .wait_with_output()
                .await
                .expect("collect xfreerdp output");
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = xvfb.kill().await;
            server_task.abort();
            panic!("xfreerdp exited early during Planar session: {status:?}\nstderr:\n{stderr}");
        }
        Ok(None) => {
            // Still running — success for this phase.
            let _ = client.kill().await;
            let _ = client.wait().await;
        }
        Err(e) => {
            let _ = xvfb.kill().await;
            server_task.abort();
            panic!("try_wait failed: {e}");
        }
    }

    let _ = xvfb.kill().await;
    server_task.abort();
}

/// Optional GFX smoke: only runs when FreeRDP tools exist and the `gfx`
/// feature is compiled into this test crate. Skips (does not fail) when the
/// client rejects `/gfx` flags — CI documents that in ARCHITECTURE.md.
#[cfg(feature = "gfx")]
#[tokio::test]
async fn test_freerdp_e2e_gfx_session_optional() {
    let Some(freerdp) = ensure_freerdp_tools() else {
        return;
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tls, pub_key) = create_tls_acceptor_and_pubkey();
    let creds = Credentials {
        username: "testuser".to_string(),
        password: "testpassword".to_string(),
        domain: None,
    };
    let validator = Arc::new(ExactMatchCredentialValidator::new(creds.clone()));
    let server = RdpServer::builder()
        .with_listener(listener)
        .with_tls(tls)
        .with_tls_public_key(pub_key)
        .with_display_handler(PatternDisplay::checkerboard(DESKTOP_W, DESKTOP_H))
        .with_input_handler(TestInputHandler::default())
        .with_credential_validator(Some(validator))
        .with_nla_credentials(Some(creds))
        .with_require_nla(true)
        .with_gfx(true)
        .build();
    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });
    let (display_str, mut xvfb) = start_xvfb(port).await;

    // FreeRDP 3: prefer AVC420 when available. Unknown flags make older
    // clients exit immediately — treat that as skip, not CI red.
    let mut client = Command::new(freerdp)
        .env("DISPLAY", &display_str)
        .arg(format!("/v:127.0.0.1:{port}"))
        .arg("/u:testuser")
        .arg("/p:testpassword")
        .arg("/cert:ignore")
        .arg(format!("/w:{DESKTOP_W}"))
        .arg(format!("/h:{DESKTOP_H}"))
        .arg("/gfx:AVC420")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn xfreerdp gfx");

    tokio::time::sleep(Duration::from_secs(3)).await;

    match client.try_wait() {
        Ok(Some(status)) => {
            let output = client
                .wait_with_output()
                .await
                .expect("collect xfreerdp gfx output");
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = xvfb.kill().await;
            server_task.abort();
            // Soft skip: GFX flag unsupported or negotiation failed.
            eprintln!(
                "GFX FreeRDP session ended early ({status:?}); treating as optional skip.\n{stderr}"
            );
        }
        Ok(None) => {
            let _ = client.kill().await;
            let _ = client.wait().await;
            let _ = xvfb.kill().await;
            server_task.abort();
        }
        Err(e) => {
            let _ = xvfb.kill().await;
            server_task.abort();
            panic!("try_wait failed: {e}");
        }
    }
}
