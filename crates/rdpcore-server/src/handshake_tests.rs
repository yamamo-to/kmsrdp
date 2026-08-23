//! Loopback tests for [`super::Session::negotiate`]: TLS + Client Info auth
//! over a real TCP pair, without CredSSP.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use rdpcore_connector::{IO_CHANNEL_ID, USER_CHANNEL_ID};
use rdpcore_pdu::client_info::{ClientInfo, ClientInfoFlags, ClientInfoPdu};
use rdpcore_pdu::finalization::{
    ControlPdu, DataPdu, FontPdu, STREAM_UNDEFINED, ShareDataPduType, SynchronizePdu,
};
use rdpcore_pdu::gcc::{
    ClientCoreData, ClientGccBlocks, ClientNetworkData, ClientSecurityData, ConferenceCreateRequest,
};
use rdpcore_pdu::mcs::{
    AttachUserRequest, ChannelJoinRequest, ConnectInitial, DomainParameters, ErectDomainRequest,
    SendData,
};
use rdpcore_pdu::x224::{self, ConnectionConfirm, ConnectionRequest, SecurityProtocol};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::client::TlsStream as ClientTlsStream;
use tokio_rustls::rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName,
};
use tokio_rustls::rustls::{
    self, DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
};

use rdpcore_pdu::fastpath::{FastPathInput, FastPathInputEvent};

use super::Session;
use crate::auth_limit::AuthLimiter;
use crate::credentials::{CredentialValidator, Credentials, ExactMatchCredentialValidator};
use crate::display::{DesktopSize, DisplayUpdate, RdpServerDisplay, RdpServerDisplayUpdates};
use crate::input::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
use crate::transport::read_tpkt_frame;

const SHARE_ID: u32 = 0x0001_0000;

struct StaticDisplay;

#[async_trait::async_trait]
impl RdpServerDisplay for StaticDisplay {
    async fn size(&self) -> DesktopSize {
        DesktopSize {
            width: 1024,
            height: 768,
        }
    }

    async fn updates(&self) -> anyhow::Result<Box<dyn RdpServerDisplayUpdates>> {
        anyhow::bail!("unused during negotiate")
    }
}

struct NoopInput;

impl RdpServerInputHandler for NoopInput {
    fn keyboard(&mut self, _event: KeyboardEvent) {}
    fn mouse(&mut self, _event: MouseEvent) {}
    fn reset(&mut self) {}
}

fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

#[derive(Debug)]
struct AcceptAllCerts;

impl ServerCertVerifier for AcceptAllCerts {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn test_tls_acceptor() -> TlsAcceptor {
    install_crypto();
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("self-signed cert");
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der: PrivateKeyDer<'static> =
        PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("tls server config");
    TlsAcceptor::from(Arc::new(config))
}

fn test_session(require_nla: bool, validator: Option<Arc<dyn CredentialValidator>>) -> Session {
    Session {
        tls: test_tls_acceptor(),
        tls_public_key: Vec::new(),
        display: Arc::new(StaticDisplay),
        input: Arc::new(Mutex::new(NoopInput)),
        credential_validator: validator,
        nla_credentials: None,
        sound_factory: None,
        cliprdr_factory: None,
        audio_input_factory: None,
        drive_factory: None,
        require_nla,
        #[cfg(feature = "gfx")]
        gfx_enabled: false,
        #[cfg(feature = "dvc-echo")]
        echo_smoke_test: false,
        session_slots: Arc::new(tokio::sync::Semaphore::new(1)),
        auth_limiter: Arc::new(AuthLimiter::new()),
    }
}

fn matching_validator() -> Arc<dyn CredentialValidator> {
    Arc::new(ExactMatchCredentialValidator::new(Credentials {
        username: "kmsrdp".to_owned(),
        password: "hunter2".to_owned(),
        domain: None,
    }))
}

fn client_connect_initial() -> Vec<u8> {
    let client_blocks = ClientGccBlocks {
        core: ClientCoreData {
            version: 0x0008_0004,
            desktop_width: 1024,
            desktop_height: 768,
            color_depth: 0xCA01,
            sas_sequence: 0xAA03,
            keyboard_layout: 0x0409,
            client_build: 2600,
            client_name: "test-client".to_owned(),
            keyboard_type: 4,
            keyboard_subtype: 0,
            keyboard_function_key: 12,
            ime_file_name: String::new(),
        },
        security: ClientSecurityData::default(),
        network: Some(ClientNetworkData {
            channels: Vec::new(),
        }),
        early_capability_flags: None,
        cluster: None,
        message_channel: None,
    };
    let connect_initial = ConnectInitial {
        target_parameters: DomainParameters::target(),
        min_parameters: DomainParameters::min(),
        max_parameters: DomainParameters::max(),
        user_data: ConferenceCreateRequest {
            client_gcc_blocks: client_blocks.encode(),
        }
        .encode(),
    };
    x224::wrap_data(&connect_initial.encode())
}

fn confirm_active_fixture() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&USER_CHANNEL_ID.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&4u16.to_le_bytes());
    body.push(0);
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());

    let mut out = Vec::new();
    out.extend_from_slice(&(10u16 + body.len() as u16).to_le_bytes());
    out.extend_from_slice(&(0x10u16 | 0x3).to_le_bytes());
    out.extend_from_slice(&USER_CHANNEL_ID.to_le_bytes());
    out.extend_from_slice(&SHARE_ID.to_le_bytes());
    out.extend(body);
    out
}

fn data_pdu(pdu_type2: ShareDataPduType, body: Vec<u8>) -> Vec<u8> {
    DataPdu {
        share_id: SHARE_ID,
        pdu_source: IO_CHANNEL_ID,
        stream_id: STREAM_UNDEFINED,
        pdu_type2,
        body,
    }
    .encode()
}

fn wrap_mcs(pdus: &[Vec<u8>]) -> Vec<u8> {
    let mut payload = Vec::new();
    for pdu in pdus {
        payload.extend_from_slice(pdu);
    }
    x224::wrap_data(&payload)
}

fn wrap_send_data_requests(bodies: &[Vec<u8>]) -> Vec<u8> {
    let mut payload = Vec::new();
    for data in bodies {
        payload.extend_from_slice(
            &SendData {
                initiator: USER_CHANNEL_ID,
                channel_id: IO_CHANNEL_ID,
                data: data.clone(),
                complete: true,
            }
            .encode_request(),
        );
    }
    x224::wrap_data(&payload)
}

async fn read_n_tpkts<R: AsyncRead + Unpin>(reader: &mut R, n: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(read_tpkt_frame(reader).await.expect("tpkt"));
    }
    out
}

async fn tls_connect(tcp: TcpStream) -> ClientTlsStream<TcpStream> {
    install_crypto();
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAllCerts))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    connector
        .connect(ServerName::try_from("localhost").expect("dns name"), tcp)
        .await
        .expect("tls client handshake")
}

async fn write_client_info<S: AsyncWrite + Unpin>(tls: &mut S, user: &str, password: &str) {
    let pdu = ClientInfoPdu {
        info: ClientInfo {
            username: user.to_owned(),
            password: password.to_owned(),
            flags: ClientInfoFlags::UNICODE,
            ..Default::default()
        },
    };
    tls.write_all(&wrap_send_data_requests(&[pdu.encode()]))
        .await
        .expect("client info");
    tls.flush().await.expect("flush client info");
}

/// Cleartext X.224, TLS, MCS, Client Info, Confirm Active, finalization.
async fn drive_tls_handshake<S>(tls: &mut S, user: &str, password: &str)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tls.write_all(&client_connect_initial())
        .await
        .expect("connect initial");
    tls.flush().await.expect("flush ci");
    let _ = read_tpkt_frame(tls).await.expect("connect response");

    let domain = wrap_mcs(&[
        ErectDomainRequest {
            sub_height: 0,
            sub_interval: 0,
        }
        .encode(),
        AttachUserRequest.encode(),
        ChannelJoinRequest {
            initiator: USER_CHANNEL_ID,
            channel_id: USER_CHANNEL_ID,
        }
        .encode(),
        ChannelJoinRequest {
            initiator: USER_CHANNEL_ID,
            channel_id: IO_CHANNEL_ID,
        }
        .encode(),
    ]);
    tls.write_all(&domain).await.expect("mcs domain");
    tls.flush().await.expect("flush mcs");
    // Attach confirm + two channel-join confirms (Erect has no response).
    let _ = read_n_tpkts(tls, 3).await;

    write_client_info(tls, user, password).await;
}

async fn finish_accepted<S: AsyncRead + AsyncWrite + Unpin>(tls: &mut S) {
    // Licensing then Demand Active.
    let _ = read_n_tpkts(tls, 2).await;

    tls.write_all(&wrap_send_data_requests(&[confirm_active_fixture()]))
        .await
        .expect("confirm active");
    tls.flush().await.expect("flush confirm");
    // Server Synchronize + Cooperate.
    let _ = read_n_tpkts(tls, 2).await;

    tls.write_all(&wrap_send_data_requests(&[
        data_pdu(
            ShareDataPduType::Synchronize,
            SynchronizePdu {
                target_user: USER_CHANNEL_ID,
            }
            .encode_body(),
        ),
        data_pdu(
            ShareDataPduType::Control,
            ControlPdu {
                action: ControlPdu::COOPERATE,
                grant_id: 0,
                control_id: 0,
            }
            .encode_body(),
        ),
        data_pdu(
            ShareDataPduType::Control,
            ControlPdu {
                action: ControlPdu::REQUEST_CONTROL,
                grant_id: 0,
                control_id: 0,
            }
            .encode_body(),
        ),
        data_pdu(
            ShareDataPduType::FontList,
            FontPdu::font_map_default().encode_body(),
        ),
    ]))
    .await
    .expect("finalization");
    tls.flush().await.expect("flush finalization");
    // Granted control + Font Map + Save Session Info.
    let _ = read_n_tpkts(tls, 3).await;
}

#[tokio::test]
async fn negotiate_accepts_matching_tls_client_info() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let session = test_session(false, Some(matching_validator()));
    let server = tokio::spawn(async move {
        let (tcp, peer) = listener.accept().await.unwrap();
        session
            .negotiate(tcp, peer.ip(), &AtomicBool::new(false))
            .await
    });

    let tcp = TcpStream::connect(addr).await.unwrap();
    tcp.set_nodelay(true).ok();
    let cr = ConnectionRequest {
        cookie: Some("kmsrdp".to_owned()),
        flags: x224::RequestFlags(0),
        protocol: SecurityProtocol::SSL,
    };
    let mut tcp = tcp;
    tcp.write_all(&cr.encode()).await.unwrap();
    tcp.flush().await.unwrap();
    let cc = read_tpkt_frame(&mut tcp).await.unwrap();
    assert!(matches!(
        ConnectionConfirm::decode(&cc).unwrap(),
        ConnectionConfirm::Response { .. }
    ));

    let mut tls = tls_connect(tcp).await;
    drive_tls_handshake(&mut tls, "kmsrdp", "hunter2").await;
    finish_accepted(&mut tls).await;

    let negotiated = server.await.unwrap().unwrap();
    assert!(
        negotiated.is_some(),
        "matching credentials must be accepted"
    );
}

#[tokio::test]
async fn negotiate_rejects_wrong_password() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let session = test_session(false, Some(matching_validator()));
    let server = tokio::spawn(async move {
        let (tcp, peer) = listener.accept().await.unwrap();
        session
            .negotiate(tcp, peer.ip(), &AtomicBool::new(false))
            .await
    });

    let tcp = TcpStream::connect(addr).await.unwrap();
    tcp.set_nodelay(true).ok();
    let cr = ConnectionRequest {
        cookie: Some("kmsrdp".to_owned()),
        flags: x224::RequestFlags(0),
        protocol: SecurityProtocol::SSL,
    };
    let mut tcp = tcp;
    tcp.write_all(&cr.encode()).await.unwrap();
    tcp.flush().await.unwrap();
    let _ = read_tpkt_frame(&mut tcp).await.unwrap();

    let mut tls = tls_connect(tcp).await;
    drive_tls_handshake(&mut tls, "kmsrdp", "wrong").await;

    let negotiated = server.await.unwrap().unwrap();
    assert!(
        negotiated.is_none(),
        "wrong password must end negotiate with Ok(None)"
    );
}

#[tokio::test]
async fn negotiate_require_nla_rejects_tls_only_client() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let session = test_session(true, Some(matching_validator()));
    let server = tokio::spawn(async move {
        let (tcp, peer) = listener.accept().await.unwrap();
        session
            .negotiate(tcp, peer.ip(), &AtomicBool::new(false))
            .await
    });

    let tcp = TcpStream::connect(addr).await.unwrap();
    tcp.set_nodelay(true).ok();
    let cr = ConnectionRequest {
        cookie: Some("kmsrdp".to_owned()),
        flags: x224::RequestFlags(0),
        protocol: SecurityProtocol::SSL,
    };
    let mut tcp = tcp;
    tcp.write_all(&cr.encode()).await.unwrap();
    tcp.flush().await.unwrap();
    let cc = read_tpkt_frame(&mut tcp).await.unwrap();
    assert!(matches!(
        ConnectionConfirm::decode(&cc).unwrap(),
        ConnectionConfirm::Failure { .. }
    ));

    let negotiated = server.await.unwrap().unwrap();
    assert!(negotiated.is_none());
}

struct IdleUpdates;

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for IdleUpdates {
    async fn next_update(&mut self) -> anyhow::Result<Option<DisplayUpdate>> {
        std::future::pending().await
    }
}

struct IdleDisplay;

#[async_trait::async_trait]
impl RdpServerDisplay for IdleDisplay {
    async fn size(&self) -> DesktopSize {
        DesktopSize {
            width: 1024,
            height: 768,
        }
    }

    async fn updates(&self) -> anyhow::Result<Box<dyn RdpServerDisplayUpdates>> {
        Ok(Box::new(IdleUpdates))
    }
}

struct RecordingInput {
    keys: Arc<Mutex<Vec<KeyboardEvent>>>,
}

impl RdpServerInputHandler for RecordingInput {
    fn keyboard(&mut self, event: KeyboardEvent) {
        self.keys.lock().unwrap().push(event);
    }
    fn mouse(&mut self, _event: MouseEvent) {}
    fn reset(&mut self) {}
}

#[tokio::test]
async fn negotiate_hybrid_without_nla_credentials_ends_cleanly() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let session = test_session(false, Some(matching_validator()));
    let server = tokio::spawn(async move {
        let (tcp, peer) = listener.accept().await.unwrap();
        session
            .negotiate(tcp, peer.ip(), &AtomicBool::new(false))
            .await
    });

    let tcp = TcpStream::connect(addr).await.unwrap();
    tcp.set_nodelay(true).ok();
    let cr = ConnectionRequest {
        cookie: Some("kmsrdp".to_owned()),
        flags: x224::RequestFlags(0),
        protocol: SecurityProtocol::SSL | SecurityProtocol::HYBRID,
    };
    let mut tcp = tcp;
    tcp.write_all(&cr.encode()).await.unwrap();
    tcp.flush().await.unwrap();
    let cc = read_tpkt_frame(&mut tcp).await.unwrap();
    assert!(matches!(
        ConnectionConfirm::decode(&cc).unwrap(),
        ConnectionConfirm::Response { protocol, .. }
            if protocol.contains(SecurityProtocol::HYBRID)
    ));

    let _tls = tls_connect(tcp).await;
    let negotiated = tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("NLA-without-credentials must not hang")
        .unwrap()
        .unwrap();
    assert!(
        negotiated.is_none(),
        "HYBRID without NLA credentials must end negotiate with Ok(None)"
    );
}

#[tokio::test]
async fn negotiate_hybrid_rejects_garbage_tsrequest() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut session = test_session(true, Some(matching_validator()));
    session.nla_credentials = Some(Credentials {
        username: "kmsrdp".to_owned(),
        password: "hunter2".to_owned(),
        domain: None,
    });
    session.tls_public_key = vec![0u8; 64];
    let server = tokio::spawn(async move {
        let (tcp, peer) = listener.accept().await.unwrap();
        session
            .negotiate(tcp, peer.ip(), &AtomicBool::new(false))
            .await
    });

    let tcp = TcpStream::connect(addr).await.unwrap();
    tcp.set_nodelay(true).ok();
    let cr = ConnectionRequest {
        cookie: Some("kmsrdp".to_owned()),
        flags: x224::RequestFlags(0),
        protocol: SecurityProtocol::SSL | SecurityProtocol::HYBRID,
    };
    let mut tcp = tcp;
    tcp.write_all(&cr.encode()).await.unwrap();
    tcp.flush().await.unwrap();
    let _ = read_tpkt_frame(&mut tcp).await.unwrap();

    let mut tls = tls_connect(tcp).await;
    tls.write_all(&[0u8; 16]).await.unwrap();
    tls.flush().await.unwrap();
    // Close the client so CredSSP's length-prefixed read sees EOF instead of
    // parking forever on a truncated TSRequest.
    drop(tls);

    let negotiated = tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("CredSSP failure must not hang")
        .unwrap()
        .unwrap();
    assert!(
        negotiated.is_none(),
        "garbage TSRequest must fail CredSSP and end negotiate with Ok(None)"
    );
}

#[tokio::test]
async fn steady_state_dispatches_fastpath_scancode() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let keys = Arc::new(Mutex::new(Vec::new()));
    let mut session = test_session(false, Some(matching_validator()));
    session.display = Arc::new(IdleDisplay);
    session.input = Arc::new(Mutex::new(RecordingInput {
        keys: Arc::clone(&keys),
    }));

    let server = tokio::spawn(async move {
        let (tcp, peer) = listener.accept().await.unwrap();
        let negotiated = session
            .negotiate(tcp, peer.ip(), &AtomicBool::new(false))
            .await
            .unwrap();
        let (tls, acceptor, accepted) = negotiated.expect("handshake");
        session
            .run_steady_state(peer, tls, acceptor, accepted)
            .await
    });

    let tcp = TcpStream::connect(addr).await.unwrap();
    tcp.set_nodelay(true).ok();
    let cr = ConnectionRequest {
        cookie: Some("kmsrdp".to_owned()),
        flags: x224::RequestFlags(0),
        protocol: SecurityProtocol::SSL,
    };
    let mut tcp = tcp;
    tcp.write_all(&cr.encode()).await.unwrap();
    tcp.flush().await.unwrap();
    let _ = read_tpkt_frame(&mut tcp).await.unwrap();

    let mut tls = tls_connect(tcp).await;
    drive_tls_handshake(&mut tls, "kmsrdp", "hunter2").await;
    finish_accepted(&mut tls).await;

    let input = FastPathInput {
        events: vec![FastPathInputEvent::Scancode {
            flags: 0,
            code: 0x1E,
        }],
    };
    tls.write_all(&input.encode()).await.unwrap();
    tls.flush().await.unwrap();

    let seen = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            {
                let rec = keys.lock().unwrap();
                if rec.iter().any(|e| {
                    matches!(
                        e,
                        KeyboardEvent::Pressed {
                            code: 0x1E,
                            extended: false
                        }
                    )
                }) {
                    return;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    drop(tls);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
    seen.expect("steady-state must dispatch the Fast-Path scancode");
}
