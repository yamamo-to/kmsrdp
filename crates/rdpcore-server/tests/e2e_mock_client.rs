use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rdpcore_cliprdr::pdu as cliprdr_pdu;
use rdpcore_cliprdr::{
    ClipboardFormat, ClipboardMessage, CliprdrBackend, CliprdrBackendFactory, FormatDataRequest,
    FormatDataResponse,
};
use rdpcore_connector::{IO_CHANNEL_ID, USER_CHANNEL_ID};
use rdpcore_pdu::client_info::{ClientInfo, ClientInfoFlags, ClientInfoPdu};
use rdpcore_pdu::fastpath::{
    self, FastPathInput, FastPathInputEvent, FastPathOutput, UPDATE_CODE_BITMAP,
};
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
use rdpcore_pdu::rdp6;
use rdpcore_pdu::svc;
use rdpcore_pdu::x224::{self, ConnectionConfirm, ConnectionRequest, SecurityProtocol};
use rdpcore_rdpsnd::pdu as rdpsnd_pdu;
use rdpcore_rdpsnd::{RdpsndServerHandler, RdpsndServerMessage, SoundServerFactory, WavePublisher};
use rdpcore_server::tokio_rustls::TlsAcceptor;
use rdpcore_server::tokio_rustls::client::TlsStream as ClientTlsStream;
use rdpcore_server::tokio_rustls::rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName,
};
use rdpcore_server::tokio_rustls::rustls::{
    self, DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
};
use rdpcore_server::{
    BitmapUpdate, Credentials, DesktopSize, DisplayUpdate, ExactMatchCredentialValidator,
    KeyboardEvent, MouseEvent, PixelFormat, RdpServer, RdpServerDisplay, RdpServerDisplayUpdates,
    RdpServerInputHandler,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SHARE_ID: u32 = 0x0001_0000;

// =====================================================================
// TLS & Certificate Helpers
// =====================================================================

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
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
        ]
    }
}

fn create_tls_acceptor_and_pubkey() -> (TlsAcceptor, Vec<u8>) {
    install_crypto();
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

// =====================================================================
// Display, Input, Sound & Clipboard Test Backends
// =====================================================================

struct SingleFrameDisplayUpdates {
    update: Option<BitmapUpdate>,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for SingleFrameDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>, rdpcore_server::DisplayError> {
        if let Some(update) = self.update.take() {
            Ok(Some(DisplayUpdate::Bitmap(update)))
        } else {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(None)
        }
    }

    fn latest_full_frame(&self) -> Option<BitmapUpdate> {
        self.update.clone()
    }
}

struct SingleFrameDisplay {
    width: u16,
    height: u16,
    data: Arc<[u8]>,
}

impl SingleFrameDisplay {
    fn new(width: u16, height: u16, data: Vec<u8>) -> Self {
        Self {
            width,
            height,
            data: Arc::from(data),
        }
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for SingleFrameDisplay {
    async fn size(&self) -> DesktopSize {
        DesktopSize {
            width: self.width,
            height: self.height,
        }
    }

    async fn updates(
        &self,
    ) -> Result<Box<dyn RdpServerDisplayUpdates>, rdpcore_server::DisplayError> {
        let width = core::num::NonZeroU16::new(self.width).unwrap();
        let height = core::num::NonZeroU16::new(self.height).unwrap();
        let stride = core::num::NonZeroUsize::new(usize::from(self.width) * 4).unwrap();
        Ok(Box::new(SingleFrameDisplayUpdates {
            update: Some(BitmapUpdate {
                x: 0,
                y: 0,
                width,
                height,
                format: PixelFormat::BgrX32,
                data: Arc::clone(&self.data),
                stride,
                src_x: 0,
                src_y: 0,
            }),
        }))
    }
}

fn make_bitmap_update(width: u16, height: u16, data: Vec<u8>) -> BitmapUpdate {
    let w = core::num::NonZeroU16::new(width).unwrap();
    let h = core::num::NonZeroU16::new(height).unwrap();
    let stride = core::num::NonZeroUsize::new(usize::from(width) * 4).unwrap();
    BitmapUpdate {
        x: 0,
        y: 0,
        width: w,
        height: h,
        format: PixelFormat::BgrX32,
        data: Arc::from(data),
        stride,
        src_x: 0,
        src_y: 0,
    }
}

struct ChannelDisplayUpdates {
    rx: tokio::sync::mpsc::UnboundedReceiver<BitmapUpdate>,
    latest: Option<BitmapUpdate>,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for ChannelDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>, rdpcore_server::DisplayError> {
        if let Some(update) = self.rx.recv().await {
            self.latest = Some(update.clone());
            Ok(Some(DisplayUpdate::Bitmap(update)))
        } else {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(None)
        }
    }

    fn latest_full_frame(&self) -> Option<BitmapUpdate> {
        self.latest.clone()
    }
}

struct ChannelDisplay {
    initial_width: u16,
    initial_height: u16,
    rx_slot: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<BitmapUpdate>>>>,
}

impl ChannelDisplay {
    fn new(width: u16, height: u16) -> (Self, tokio::sync::mpsc::UnboundedSender<BitmapUpdate>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                initial_width: width,
                initial_height: height,
                rx_slot: Arc::new(Mutex::new(Some(rx))),
            },
            tx,
        )
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for ChannelDisplay {
    async fn size(&self) -> DesktopSize {
        DesktopSize {
            width: self.initial_width,
            height: self.initial_height,
        }
    }

    async fn updates(
        &self,
    ) -> Result<Box<dyn RdpServerDisplayUpdates>, rdpcore_server::DisplayError> {
        let rx = self
            .rx_slot
            .lock()
            .unwrap()
            .take()
            .expect("updates called once");
        Ok(Box::new(ChannelDisplayUpdates { rx, latest: None }))
    }
}

#[derive(Default)]
struct RecordingInputHandler {
    keyboard_events: Arc<Mutex<Vec<KeyboardEvent>>>,
    mouse_events: Arc<Mutex<Vec<MouseEvent>>>,
}

impl RdpServerInputHandler for RecordingInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        self.keyboard_events.lock().unwrap().push(event);
    }
    fn mouse(&mut self, event: MouseEvent) {
        self.mouse_events.lock().unwrap().push(event);
    }
    fn reset(&mut self) {}
}

struct NoopInput;

impl RdpServerInputHandler for NoopInput {
    fn keyboard(&mut self, _event: KeyboardEvent) {}
    fn mouse(&mut self, _event: MouseEvent) {}
    fn reset(&mut self) {}
}

#[derive(Debug)]
struct MockSoundHandler {
    formats: Vec<rdpsnd_pdu::AudioFormat>,
}

impl RdpsndServerHandler for MockSoundHandler {
    fn get_formats(&self) -> &[rdpsnd_pdu::AudioFormat] {
        &self.formats
    }
    fn choose_format(
        &mut self,
        common: &[rdpsnd_pdu::NegotiatedFormat],
    ) -> Option<rdpsnd_pdu::NegotiatedFormat> {
        common.first().cloned()
    }
    fn start(
        &mut self,
        _format: &rdpsnd_pdu::NegotiatedFormat,
    ) -> Result<(), Box<dyn std::error::Error + Send>> {
        Ok(())
    }
    fn stop(&mut self) {}
}

struct MockSoundFactory {
    publisher_slot: Arc<Mutex<Option<WavePublisher>>>,
}

impl SoundServerFactory for MockSoundFactory {
    fn build_backend(&self, publisher: WavePublisher) -> Box<dyn RdpsndServerHandler> {
        *self.publisher_slot.lock().unwrap() = Some(publisher);
        Box::new(MockSoundHandler {
            formats: vec![rdpsnd_pdu::AudioFormat::pcm(2, 44100, 16)],
        })
    }
}

#[derive(Debug)]
struct MockCliprdrBackend {
    text: String,
    sender: tokio::sync::mpsc::UnboundedSender<ClipboardMessage>,
}

impl CliprdrBackend for MockCliprdrBackend {
    fn on_ready(&mut self) {
        let _ = self.sender.send(ClipboardMessage::SendInitiateCopy(vec![
            ClipboardFormat::unicode_text(),
        ]));
    }
    fn on_remote_copy(&mut self, _formats: &[ClipboardFormat]) {}
    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        if request.format == cliprdr_pdu::CF_UNICODETEXT {
            let _ = self.sender.send(ClipboardMessage::SendFormatData(
                FormatDataResponse::new_unicode_string(&self.text),
            ));
        } else {
            let _ = self.sender.send(ClipboardMessage::SendFormatData(
                FormatDataResponse::new_error(),
            ));
        }
    }
    fn on_format_data_response(&mut self, _response: FormatDataResponse) {}
}

struct MockCliprdrFactory {
    text: String,
}

impl CliprdrBackendFactory for MockCliprdrFactory {
    fn build_cliprdr_backend(
        &self,
        sender: tokio::sync::mpsc::UnboundedSender<ClipboardMessage>,
    ) -> Box<dyn CliprdrBackend> {
        Box::new(MockCliprdrBackend {
            text: self.text.clone(),
            sender,
        })
    }
}

// =====================================================================
// PDU Framing & Mock Client Protocol Machine
// =====================================================================

async fn read_tpkt_or_fastpath_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Vec<u8>, std::io::Error> {
    let mut header = [0u8; 2];
    reader.read_exact(&mut header).await?;
    if header[0] == 0x03 {
        // TPKT packet: [0x03, 0x00, len_hi, len_lo]
        let mut len_bytes = [0u8; 2];
        reader.read_exact(&mut len_bytes).await?;
        let total_len = u16::from_be_bytes(len_bytes) as usize;
        if total_len < 4 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "TPKT length too short",
            ));
        }
        let mut payload = vec![0u8; total_len - 4];
        reader.read_exact(&mut payload).await?;
        let mut full = Vec::with_capacity(total_len);
        full.extend_from_slice(&header);
        full.extend_from_slice(&len_bytes);
        full.extend_from_slice(&payload);
        Ok(full)
    } else {
        // Fast-Path Output packet
        let b2 = header[1];
        let (total_len, consumed_len_bytes) = if (b2 & 0x80) != 0 {
            let mut b3 = [0u8; 1];
            reader.read_exact(&mut b3).await?;
            let len = (((b2 & 0x7F) as usize) << 8) | (b3[0] as usize);
            (len, 3)
        } else {
            (b2 as usize, 2)
        };
        if total_len < consumed_len_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "FastPath length too short",
            ));
        }
        let mut rest = vec![0u8; total_len - consumed_len_bytes];
        reader.read_exact(&mut rest).await?;
        let mut packet = Vec::with_capacity(total_len);
        packet.push(header[0]);
        packet.push(header[1]);
        if consumed_len_bytes == 3 {
            packet.push((total_len & 0xFF) as u8);
        }
        packet.extend_from_slice(&rest);
        Ok(packet)
    }
}

async fn read_n_tpkts<R: AsyncRead + Unpin>(reader: &mut R, n: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let frame = read_tpkt_or_fastpath_frame(reader)
            .await
            .expect("read frame");
        out.push(frame);
    }
    out
}

fn client_connect_initial(width: u16, height: u16, channel_names: Vec<String>) -> Vec<u8> {
    let channels = channel_names
        .into_iter()
        .map(|name| rdpcore_pdu::gcc::ChannelDef { name, options: 0 })
        .collect();
    let client_blocks = ClientGccBlocks {
        core: ClientCoreData {
            version: 0x0008_0004,
            desktop_width: width,
            desktop_height: height,
            color_depth: 0xCA01, // 32bpp
            sas_sequence: 0xAA03,
            keyboard_layout: 0x0409,
            client_build: 2600,
            client_name: "MockClient".to_owned(),
            keyboard_type: 4,
            keyboard_subtype: 0,
            keyboard_function_key: 12,
            ime_file_name: String::new(),
        },
        security: ClientSecurityData::default(),
        network: Some(ClientNetworkData { channels }),
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

fn wrap_channel_send_data(channel_id: u16, data: &[u8]) -> Vec<u8> {
    let chunks = svc::chunkify(data);
    let mut payload = Vec::new();
    for chunk in chunks {
        payload.extend_from_slice(
            &SendData {
                initiator: USER_CHANNEL_ID,
                channel_id,
                data: chunk,
                complete: true,
            }
            .encode_request(),
        );
    }
    x224::wrap_data(&payload)
}

struct MockRdpClient {
    tls: ClientTlsStream<TcpStream>,
}

impl MockRdpClient {
    async fn connect(
        addr: SocketAddr,
        width: u16,
        height: u16,
        user: &str,
        password: &str,
        channels: Vec<String>,
    ) -> Self {
        let tcp = TcpStream::connect(addr).await.expect("tcp connect");
        tcp.set_nodelay(true).ok();

        // 1. X.224 Connection Request
        let cr = ConnectionRequest {
            cookie: Some("kmsrdp".to_owned()),
            flags: x224::RequestFlags(0),
            protocol: SecurityProtocol::SSL,
        };
        let mut tcp = tcp;
        tcp.write_all(&cr.encode()).await.expect("write cr");
        tcp.flush().await.expect("flush cr");
        let cc = read_tpkt_or_fastpath_frame(&mut tcp)
            .await
            .expect("read cc");
        assert!(matches!(
            ConnectionConfirm::decode(&cc).unwrap(),
            ConnectionConfirm::Response { .. }
        ));

        // 2. TLS Handshake
        let mut tls = tls_connect(tcp).await;

        // 3. MCS Connect Initial
        let num_channels = channels.len();
        tls.write_all(&client_connect_initial(width, height, channels))
            .await
            .expect("connect initial");
        tls.flush().await.expect("flush ci");
        let _ = read_tpkt_or_fastpath_frame(&mut tls)
            .await
            .expect("connect response");

        // 4. MCS ErectDomain, AttachUser, ChannelJoins
        let mut mcs_pdus = vec![
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
        ];
        for i in 0..num_channels {
            mcs_pdus.push(
                ChannelJoinRequest {
                    initiator: USER_CHANNEL_ID,
                    channel_id: 1004 + i as u16,
                }
                .encode(),
            );
        }
        tls.write_all(&wrap_mcs(&mcs_pdus)).await.expect("mcs join");
        tls.flush().await.expect("flush mcs");

        // Attach confirm + channel join confirms (user, IO, + static channels)
        let _ = read_n_tpkts(&mut tls, 3 + num_channels).await;

        // 5. Client Info
        let info = ClientInfoPdu {
            info: ClientInfo {
                username: user.to_owned(),
                password: password.to_owned(),
                flags: ClientInfoFlags::UNICODE,
                ..Default::default()
            },
        };
        tls.write_all(&wrap_send_data_requests(&[info.encode()]))
            .await
            .expect("client info");
        tls.flush().await.expect("flush info");

        // 6. Licensing + Demand Active
        let _ = read_n_tpkts(&mut tls, 2).await;

        // 7. Confirm Active
        tls.write_all(&wrap_send_data_requests(&[confirm_active_fixture()]))
            .await
            .expect("confirm active");
        tls.flush().await.expect("flush confirm");

        // 8. Server Synchronize + Cooperate
        let _ = read_n_tpkts(&mut tls, 2).await;

        // 9. Client Synchronize + Cooperate + Request Control + Font List
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

        // 10. Server Granted Control + Font Map + Save Session Info
        let _ = read_n_tpkts(&mut tls, 3).await;

        Self { tls }
    }

    async fn send_fastpath_input(&mut self, events: Vec<FastPathInputEvent>) {
        let input = FastPathInput { events };
        self.tls
            .write_all(&input.encode())
            .await
            .expect("write fastpath input");
        self.tls.flush().await.expect("flush fastpath input");
    }

    async fn send_channel_data(&mut self, channel_id: u16, data: &[u8]) {
        let packet = wrap_channel_send_data(channel_id, data);
        self.tls
            .write_all(&packet)
            .await
            .expect("write channel data");
        self.tls.flush().await.expect("flush channel data");
    }

    async fn read_frame(&mut self) -> Vec<u8> {
        read_tpkt_or_fastpath_frame(&mut self.tls)
            .await
            .expect("read frame")
    }
}

// =====================================================================
// Integration Tests
// =====================================================================

#[tokio::test]
async fn test_e2e_bitmap_rendering_pixel_match() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (tls, pub_key) = create_tls_acceptor_and_pubkey();
    let creds = Credentials {
        username: "testuser".to_string(),
        password: "testpassword".to_string(),
        domain: None,
    };
    let validator = Arc::new(ExactMatchCredentialValidator::new(creds.clone()));

    // Create a 128x128 BGRX image with 4 distinct 64x64 quadrants
    let width = 128u16;
    let height = 128u16;
    let mut expected_image = vec![0u8; usize::from(width) * usize::from(height) * 4];
    for y in 0..usize::from(height) {
        for x in 0..usize::from(width) {
            let idx = (y * usize::from(width) + x) * 4;
            if x < 64 && y < 64 {
                // Top-Left: Red (B=0, G=0, R=255, X=0)
                expected_image[idx + 2] = 255;
            } else if x >= 64 && y < 64 {
                // Top-Right: Green (B=0, G=255, R=0, X=0)
                expected_image[idx + 1] = 255;
            } else if x < 64 && y >= 64 {
                // Bottom-Left: Blue (B=255, G=0, R=0, X=0)
                expected_image[idx] = 255;
            } else {
                // Bottom-Right: Gradient
                expected_image[idx] = (x as u8).wrapping_mul(3);
                expected_image[idx + 1] = (y as u8).wrapping_mul(3);
                expected_image[idx + 2] = 128;
            }
        }
    }

    let display = SingleFrameDisplay::new(width, height, expected_image.clone());
    let server = RdpServer::builder()
        .with_listener(listener)
        .with_tls(tls)
        .with_tls_public_key(pub_key)
        .with_display_handler(display)
        .with_input_handler(NoopInput)
        .with_credential_validator(Some(validator))
        .with_nla_credentials(Some(creds))
        .build();

    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    let mut client =
        MockRdpClient::connect(addr, width, height, "testuser", "testpassword", Vec::new()).await;

    // Client canvas to reconstruct the received frame
    let mut reconstructed = vec![0u8; usize::from(width) * usize::from(height) * 4];
    let mut total_tiles_decoded = 0;

    let timeout_result = tokio::time::timeout(Duration::from_secs(5), async {
        while total_tiles_decoded < 4 {
            let frame = client.read_frame().await;
            if frame.is_empty() || (frame[0] & 0x03 != 0) {
                continue;
            }
            let Ok(fastpath) = FastPathOutput::decode(&frame) else {
                continue;
            };
            for update in fastpath.updates {
                if update.update_code != UPDATE_CODE_BITMAP {
                    continue;
                }
                let Ok(bitmap_data) = fastpath::BitmapUpdateData::decode(&update.data) else {
                    continue;
                };
                for rect in bitmap_data.rectangles {
                    let rect_w = usize::from(rect.width);
                    let rect_h = usize::from(rect.height);
                    let bgrx_pixels = if rect.compressed_scan_width.is_some() {
                        rdp6::decode(&rect.data, rect_w, rect_h).expect("decode rdp6 planar tile")
                    } else {
                        rect.data.clone()
                    };

                    // Paint tile into reconstructed canvas (bottom-up encoded scanlines to top-down canvas)
                    for ry in 0..rect_h {
                        let cy = usize::from(rect.dest_top) + (rect_h - 1 - ry);
                        let cx = usize::from(rect.dest_left);
                        let dst_start = (cy * usize::from(width) + cx) * 4;
                        let src_start = (ry * rect_w) * 4;
                        reconstructed[dst_start..dst_start + rect_w * 4]
                            .copy_from_slice(&bgrx_pixels[src_start..src_start + rect_w * 4]);
                    }
                    total_tiles_decoded += 1;
                }
            }
        }
    })
    .await;

    server_task.abort();
    timeout_result.expect("timeout waiting for all bitmap tiles");

    // Pixel-perfect verification
    assert_eq!(
        reconstructed, expected_image,
        "Reconstructed bitmap from FastPath stream must match server's source image byte-for-byte"
    );
}

#[tokio::test]
async fn test_e2e_input_injection_keyboard_and_mouse() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (tls, pub_key) = create_tls_acceptor_and_pubkey();
    let creds = Credentials {
        username: "testuser".to_string(),
        password: "testpassword".to_string(),
        domain: None,
    };
    let validator = Arc::new(ExactMatchCredentialValidator::new(creds.clone()));

    let input_handler = RecordingInputHandler::default();
    let keys = Arc::clone(&input_handler.keyboard_events);
    let mouse = Arc::clone(&input_handler.mouse_events);

    let display = SingleFrameDisplay::new(64, 64, vec![0u8; 64 * 64 * 4]);
    let server = RdpServer::builder()
        .with_listener(listener)
        .with_tls(tls)
        .with_tls_public_key(pub_key)
        .with_display_handler(display)
        .with_input_handler(input_handler)
        .with_credential_validator(Some(validator))
        .with_nla_credentials(Some(creds))
        .build();

    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    let mut client =
        MockRdpClient::connect(addr, 64, 64, "testuser", "testpassword", Vec::new()).await;

    // Send input events: Scancodes (A press & release), Mouse move, Left Click, Wheels
    client
        .send_fastpath_input(vec![
            FastPathInputEvent::Scancode {
                flags: 0,
                code: 0x1E, // Key 'A' pressed
            },
            FastPathInputEvent::Scancode {
                flags: fastpath::keyboard_flags::RELEASE,
                code: 0x1E, // Key 'A' released
            },
            FastPathInputEvent::Mouse {
                pointer_flags: 0x0800, // PTRFLAGS_MOVE
                x: 42,
                y: 84,
            },
            FastPathInputEvent::Mouse {
                pointer_flags: 0x1000 | 0x8000, // PTRFLAGS_BUTTON1 | PTRFLAGS_DOWN
                x: 42,
                y: 84,
            },
            FastPathInputEvent::Mouse {
                pointer_flags: 0x1000, // PTRFLAGS_BUTTON1 (UP)
                x: 42,
                y: 84,
            },
            FastPathInputEvent::Mouse {
                pointer_flags: 0x0200 | 120, // PTRFLAGS_WHEEL (delta +120)
                x: 42,
                y: 84,
            },
            FastPathInputEvent::Mouse {
                pointer_flags: 0x0400 | ((-120i16 as u16) & 0x01FF), // PTRFLAGS_HWHEEL (delta -120)
                x: 42,
                y: 84,
            },
        ])
        .await;

    // Wait and verify input recorded
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let k_len = keys.lock().unwrap().len();
            let m_len = mouse.lock().unwrap().len();
            if k_len >= 2 && m_len >= 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timeout waiting for input events");

    server_task.abort();

    let recorded_keys = keys.lock().unwrap().clone();
    assert_eq!(
        recorded_keys,
        vec![
            KeyboardEvent::Pressed {
                code: 0x1E,
                extended: false,
            },
            KeyboardEvent::Released {
                code: 0x1E,
                extended: false,
            },
        ]
    );

    let recorded_mouse = mouse.lock().unwrap().clone();
    assert_eq!(
        recorded_mouse,
        vec![
            MouseEvent::Move { x: 42, y: 84 },
            MouseEvent::LeftPressed,
            MouseEvent::LeftReleased,
            MouseEvent::VerticalScroll { value: 120 },
            MouseEvent::HorizontalScroll { value: -120 },
        ]
    );
}

#[tokio::test]
async fn test_e2e_cliprdr_text_exchange() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (tls, pub_key) = create_tls_acceptor_and_pubkey();
    let creds = Credentials {
        username: "testuser".to_string(),
        password: "testpassword".to_string(),
        domain: None,
    };
    let validator = Arc::new(ExactMatchCredentialValidator::new(creds.clone()));

    let cliprdr_factory = Box::new(MockCliprdrFactory {
        text: "Hello KMSRDP Cliprdr E2E".to_string(),
    });

    let display = SingleFrameDisplay::new(64, 64, vec![0u8; 64 * 64 * 4]);
    let server = RdpServer::builder()
        .with_listener(listener)
        .with_tls(tls)
        .with_tls_public_key(pub_key)
        .with_display_handler(display)
        .with_input_handler(NoopInput)
        .with_credential_validator(Some(validator))
        .with_nla_credentials(Some(creds))
        .with_cliprdr_factory(Some(cliprdr_factory))
        .build();

    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    let mut client = MockRdpClient::connect(
        addr,
        64,
        64,
        "testuser",
        "testpassword",
        vec!["cliprdr".to_string()],
    )
    .await;

    // Client static channel ID for cliprdr is 1004
    let cliprdr_channel_id = 1004;

    // Send Client Capabilities
    let client_caps = cliprdr_pdu::encode_capabilities();
    client
        .send_channel_data(cliprdr_channel_id, &client_caps)
        .await;

    // Send Client Format List (plain text)
    let client_formats = cliprdr_pdu::encode_format_list_unicode_text();
    client
        .send_channel_data(cliprdr_channel_id, &client_formats)
        .await;

    // Wait for Server Format List, reply with OK, then request format data
    let received_text = tokio::time::timeout(Duration::from_secs(4), async {
        let mut requested = false;
        loop {
            let frame = client.read_frame().await;
            if frame.len() <= 10 || frame[0] != 0x03 {
                continue;
            }
            let Ok(send_data) = SendData::decode_indication(&frame[7..]) else {
                continue;
            };
            if send_data.channel_id != cliprdr_channel_id {
                continue;
            }
            let Ok((_, _, payload)) = svc::dechunkify(&send_data.data) else {
                continue;
            };
            let Ok(clip_msg) = cliprdr_pdu::decode_client_message(payload) else {
                continue;
            };
            match clip_msg {
                cliprdr_pdu::ClientMessage::FormatList(_) => {
                    let resp = cliprdr_pdu::encode_format_list_response_ok();
                    client.send_channel_data(cliprdr_channel_id, &resp).await;
                    if !requested {
                        requested = true;
                        let req =
                            cliprdr_pdu::encode_format_data_request(cliprdr_pdu::CF_UNICODETEXT);
                        client.send_channel_data(cliprdr_channel_id, &req).await;
                    }
                }
                cliprdr_pdu::ClientMessage::FormatDataResponse(
                    cliprdr_pdu::FormatDataStatus::Ok(text),
                ) => {
                    return text;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("timeout waiting for clipboard data response");

    server_task.abort();
    assert_eq!(received_text, "Hello KMSRDP Cliprdr E2E");
}

fn encode_client_audio_formats(formats: &[rdpsnd_pdu::AudioFormat], version: u16) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&7u32.to_le_bytes()); // flags: ALIVE|VOLUME|PITCH
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u16.to_be_bytes()); // wDGramPort (BE)
    body.extend_from_slice(&(formats.len() as u16).to_le_bytes());
    body.push(0); // cLastBlockConfirmed
    body.extend_from_slice(&version.to_le_bytes());
    body.push(0); // pad
    for f in formats {
        f.encode(&mut body);
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.push(rdpsnd_pdu::SNDC_FORMATS);
    out.push(0);
    out.extend_from_slice(&(body.len() as u16).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

fn encode_quality_mode() -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.push(rdpsnd_pdu::SNDC_QUALITYMODE);
    out.push(0);
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // DYNAMIC
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[tokio::test]
async fn test_e2e_rdpsnd_audio_streaming_and_confirm() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (tls, pub_key) = create_tls_acceptor_and_pubkey();
    let creds = Credentials {
        username: "testuser".to_string(),
        password: "testpassword".to_string(),
        domain: None,
    };
    let validator = Arc::new(ExactMatchCredentialValidator::new(creds.clone()));

    let publisher_slot = Arc::new(Mutex::new(None));
    let sound_factory = Box::new(MockSoundFactory {
        publisher_slot: Arc::clone(&publisher_slot),
    });

    let display = SingleFrameDisplay::new(64, 64, vec![0u8; 64 * 64 * 4]);
    let server = RdpServer::builder()
        .with_listener(listener)
        .with_tls(tls)
        .with_tls_public_key(pub_key)
        .with_display_handler(display)
        .with_input_handler(NoopInput)
        .with_credential_validator(Some(validator))
        .with_nla_credentials(Some(creds))
        .with_sound_factory(Some(sound_factory))
        .build();

    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    let mut client = MockRdpClient::connect(
        addr,
        64,
        64,
        "testuser",
        "testpassword",
        vec!["rdpsnd".to_string()],
    )
    .await;

    let rdpsnd_channel_id = 1004;

    // Send Client Audio Formats (v8 with PCM 44100 16-bit 2ch) + Quality Mode
    let client_formats = encode_client_audio_formats(
        &[rdpsnd_pdu::AudioFormat::pcm(2, 44100, 16)],
        rdpsnd_pdu::VERSION_V8,
    );
    client
        .send_channel_data(rdpsnd_channel_id, &client_formats)
        .await;

    let qmode = encode_quality_mode();
    client.send_channel_data(rdpsnd_channel_id, &qmode).await;

    // Wait to receive SNDC_TRAINING, reply with TrainingConfirm, then publish audio and receive Wave2
    let got_wave = tokio::time::timeout(Duration::from_secs(4), async {
        let mut audio_published = false;
        loop {
            let frame = client.read_frame().await;
            if frame.len() <= 10 || frame[0] != 0x03 {
                continue;
            }
            let Ok(send_data) = SendData::decode_indication(&frame[7..]) else {
                continue;
            };
            if send_data.channel_id != rdpsnd_channel_id {
                continue;
            }
            let Ok((_, _, payload)) = svc::dechunkify(&send_data.data) else {
                continue;
            };
            if payload.is_empty() {
                continue;
            }
            let msg_type = payload[0];
            if msg_type == rdpsnd_pdu::SNDC_TRAINING {
                // Reply with training confirm
                let mut confirm = Vec::new();
                confirm.extend_from_slice(&[rdpsnd_pdu::SNDC_TRAINING, 0, 4, 0, 0, 0, 0, 0]);
                client.send_channel_data(rdpsnd_channel_id, &confirm).await;

                // Yield to let server process TrainingConfirm and transition to Ready
                tokio::time::sleep(Duration::from_millis(50)).await;

                if !audio_published {
                    audio_published = true;
                    let pub_guard = publisher_slot.lock().unwrap();
                    if let Some(ref publisher) = *pub_guard {
                        publisher.publish(RdpsndServerMessage::Wave(vec![0xAA; 1764], 12345));
                    }
                }
            } else if msg_type == rdpsnd_pdu::SNDC_WAVE2 {
                // Reply with WaveConfirm
                let timestamp = if payload.len() >= 6 {
                    u16::from_le_bytes([payload[4], payload[5]])
                } else {
                    0
                };
                let block_no = if payload.len() >= 9 { payload[8] } else { 0 };
                let mut confirm = Vec::new();
                confirm.push(rdpsnd_pdu::SNDC_WAVECONFIRM);
                confirm.push(0);
                confirm.extend_from_slice(&4u16.to_le_bytes());
                confirm.extend_from_slice(&timestamp.to_le_bytes());
                confirm.push(block_no);
                confirm.push(0);
                client.send_channel_data(rdpsnd_channel_id, &confirm).await;
                return true;
            } else if msg_type == rdpsnd_pdu::SNDC_WAVE {
                return true;
            }
        }
    })
    .await;

    server_task.abort();
    assert!(
        got_wave.unwrap_or(false),
        "Client must successfully receive audio Wave packet on rdpsnd channel"
    );
}

// =====================================================================
// MS-RDPEGFX (H.264 AVC420) DVC E2E Tests
// =====================================================================

#[cfg(feature = "gfx")]
#[allow(dead_code)]
#[derive(Debug, Clone)]
enum GfxServerPdu {
    CapsConfirm(rdpcore_rdpegfx::pdu::RawCapabilitySet),
    ResetGraphics {
        width: u32,
        height: u32,
    },
    CreateSurface {
        surface_id: u16,
        width: u16,
        height: u16,
    },
    DeleteSurface {
        surface_id: u16,
    },
    MapSurfaceToOutput {
        surface_id: u16,
    },
    StartFrame {
        timestamp: u32,
        frame_id: u32,
    },
    EndFrame {
        frame_id: u32,
    },
    WireToSurface1Avc420 {
        surface_id: u16,
        width: u16,
        height: u16,
        data: Vec<u8>,
    },
    Other(u16),
}

#[cfg(feature = "gfx")]
fn parse_gfx_pdus(payload: &[u8]) -> Vec<GfxServerPdu> {
    let mut out = Vec::new();
    let mut data = payload;
    if data.len() >= 2 && data[0] == 0xe0 && data[1] == 0x04 {
        data = &data[2..];
    }
    let mut cursor = rdpcore_pdu::cursor::ReadCursor::new(data);
    while cursor.remaining() >= 8 {
        let Ok(cmd_id) = cursor.read_u16_le() else {
            break;
        };
        let Ok(_flags) = cursor.read_u16_le() else {
            break;
        };
        let Ok(pdu_length) = cursor.read_u32_le() else {
            break;
        };
        if pdu_length < 8 {
            break;
        }
        let body_len = (pdu_length - 8) as usize;
        let Ok(body) = cursor.read_slice(body_len) else {
            break;
        };
        let mut body_cursor = rdpcore_pdu::cursor::ReadCursor::new(body);

        match cmd_id {
            0x0013 => {
                if let (Ok(version), Ok(len)) =
                    (body_cursor.read_u32_le(), body_cursor.read_u32_le())
                    && let Ok(cap_data) = body_cursor.read_slice(len as usize)
                {
                    out.push(GfxServerPdu::CapsConfirm(
                        rdpcore_rdpegfx::pdu::RawCapabilitySet {
                            version,
                            data: cap_data.to_vec(),
                        },
                    ));
                }
            }
            0x000e => {
                if let (Ok(w), Ok(h)) = (body_cursor.read_u32_le(), body_cursor.read_u32_le()) {
                    out.push(GfxServerPdu::ResetGraphics {
                        width: w,
                        height: h,
                    });
                }
            }
            0x0009 => {
                if let (Ok(surface_id), Ok(width), Ok(height)) = (
                    body_cursor.read_u16_le(),
                    body_cursor.read_u16_le(),
                    body_cursor.read_u16_le(),
                ) {
                    out.push(GfxServerPdu::CreateSurface {
                        surface_id,
                        width,
                        height,
                    });
                }
            }
            0x000a => {
                if let Ok(surface_id) = body_cursor.read_u16_le() {
                    out.push(GfxServerPdu::DeleteSurface { surface_id });
                }
            }
            0x000f => {
                if let Ok(surface_id) = body_cursor.read_u16_le() {
                    out.push(GfxServerPdu::MapSurfaceToOutput { surface_id });
                }
            }
            0x000b => {
                if let (Ok(timestamp), Ok(frame_id)) =
                    (body_cursor.read_u32_le(), body_cursor.read_u32_le())
                {
                    out.push(GfxServerPdu::StartFrame {
                        timestamp,
                        frame_id,
                    });
                }
            }
            0x000c => {
                if let Ok(frame_id) = body_cursor.read_u32_le() {
                    out.push(GfxServerPdu::EndFrame { frame_id });
                }
            }
            0x0001 => {
                if let (
                    Ok(surface_id),
                    Ok(codec_id),
                    Ok(_pix),
                    Ok(left),
                    Ok(top),
                    Ok(right),
                    Ok(bottom),
                    Ok(len),
                ) = (
                    body_cursor.read_u16_le(),
                    body_cursor.read_u16_le(),
                    body_cursor.read_u8(),
                    body_cursor.read_u16_le(),
                    body_cursor.read_u16_le(),
                    body_cursor.read_u16_le(),
                    body_cursor.read_u16_le(),
                    body_cursor.read_u32_le(),
                ) && let Ok(data) = body_cursor.read_slice(len as usize)
                    && codec_id == 0x000b
                {
                    out.push(GfxServerPdu::WireToSurface1Avc420 {
                        surface_id,
                        width: right.saturating_sub(left),
                        height: bottom.saturating_sub(top),
                        data: data.to_vec(),
                    });
                }
            }
            other => {
                out.push(GfxServerPdu::Other(other));
            }
        }
    }
    out
}

#[cfg(feature = "gfx")]
fn extract_h264_bitstream(avc420_data: &[u8]) -> &[u8] {
    if avc420_data.len() < 14 {
        return &[];
    }
    let mut cursor = rdpcore_pdu::cursor::ReadCursor::new(avc420_data);
    let Ok(rects) = cursor.read_u32_le() else {
        return &[];
    };
    let rect_bytes = (rects as usize) * 8;
    if cursor.remaining() < rect_bytes + 2 {
        return &[];
    }
    let _ = cursor.read_slice(rect_bytes);
    let _ = cursor.read_u8(); // quantQualityVal
    let _ = cursor.read_u8(); // quality
    cursor.read_rest()
}

#[cfg(feature = "gfx")]
fn find_nal_unit_types(bitstream: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 < bitstream.len() {
        if bitstream[i] == 0 && bitstream[i + 1] == 0 && bitstream[i + 2] == 1 {
            let nal_header = bitstream[i + 3];
            types.push(nal_header & 0x1F);
            i += 4;
        } else if i + 4 < bitstream.len()
            && bitstream[i] == 0
            && bitstream[i + 1] == 0
            && bitstream[i + 2] == 0
            && bitstream[i + 3] == 1
        {
            let nal_header = bitstream[i + 4];
            types.push(nal_header & 0x1F);
            i += 5;
        } else {
            i += 1;
        }
    }
    types
}

#[cfg(feature = "gfx")]
fn encode_gfx_caps_advertise(sets: &[rdpcore_rdpegfx::pdu::RawCapabilitySet]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(sets.len() as u16).to_le_bytes());
    for s in sets {
        body.extend_from_slice(&s.version.to_le_bytes());
        body.extend_from_slice(&(s.data.len() as u32).to_le_bytes());
        body.extend_from_slice(&s.data);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&0x0012u16.to_le_bytes()); // CMD_CAPS_ADVERTISE
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.extend_from_slice(&(8u32 + body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

#[cfg(feature = "gfx")]
fn encode_gfx_frame_acknowledge(queue_depth: u32, frame_id: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&queue_depth.to_le_bytes());
    body.extend_from_slice(&frame_id.to_le_bytes());
    body.extend_from_slice(&frame_id.to_le_bytes()); // total_frames_decoded
    let mut out = Vec::new();
    out.extend_from_slice(&0x000Du16.to_le_bytes()); // CMD_FRAME_ACKNOWLEDGE
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.extend_from_slice(&(8u32 + body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

#[cfg(feature = "gfx")]
fn encode_dvc_data_pdu(channel_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + payload.len());
    out.push(0x30); // CMD_DATA << 4 | 0
    out.push(channel_id as u8);
    out.extend_from_slice(payload);
    out
}

#[cfg(feature = "gfx")]
fn read_dvc_var(
    cursor: &mut rdpcore_pdu::cursor::ReadCursor<'_>,
    sel: u8,
) -> Result<u32, rdpcore_pdu::DecodeError> {
    match sel {
        0 => Ok(u32::from(cursor.read_u8()?)),
        1 => Ok(u32::from(cursor.read_u16_le()?)),
        2 => Ok(cursor.read_u32_le()?),
        _ => Err(rdpcore_pdu::DecodeError::InvalidValue {
            field: "dvc.var",
            reason: "invalid sel",
        }),
    }
}

#[cfg(feature = "gfx")]
struct MockDvcClient {
    gfx_channel_id: Option<u32>,
    reassembly: Option<(u32, Vec<u8>)>,
}

#[cfg(feature = "gfx")]
impl MockDvcClient {
    fn new() -> Self {
        Self {
            gfx_channel_id: None,
            reassembly: None,
        }
    }

    fn handle_drdynvc_frame(&mut self, payload: &[u8]) -> (Option<Vec<u8>>, Vec<GfxServerPdu>) {
        let mut responses = Vec::new();
        let mut gfx_pdus = Vec::new();

        let mut cursor = rdpcore_pdu::cursor::ReadCursor::new(payload);
        let Ok(header) = cursor.read_u8() else {
            return (None, gfx_pdus);
        };
        let cmd = header >> 4;
        let sp = (header >> 2) & 0x03;
        let cb_id = header & 0x03;

        match cmd {
            0x05 => {
                // CMD_CAPABILITY
                responses.extend_from_slice(&[0x50, 0x00, 0x02, 0x00]);
            }
            0x01 => {
                // CMD_CREATE
                if let Ok(channel_id) = read_dvc_var(&mut cursor, cb_id) {
                    let rest = cursor.read_rest();
                    let name = String::from_utf8_lossy(rest);
                    if name.starts_with("Microsoft::Windows::RDS::Graphics") {
                        self.gfx_channel_id = Some(channel_id);
                        let mut resp = Vec::new();
                        resp.push(0x10); // CMD_CREATE << 4 | 0
                        resp.push(channel_id as u8);
                        resp.extend_from_slice(&0u32.to_le_bytes());
                        responses.extend_from_slice(&resp);
                    }
                }
            }
            0x03 => {
                // CMD_DATA
                if let Ok(channel_id) = read_dvc_var(&mut cursor, cb_id) {
                    let chunk = cursor.read_rest();
                    if let Some((total_len, ref mut buf)) = self.reassembly {
                        buf.extend_from_slice(chunk);
                        if buf.len() >= total_len as usize {
                            let completed = self.reassembly.take().unwrap().1;
                            gfx_pdus.extend(parse_gfx_pdus(&completed));
                        }
                    } else if Some(channel_id) == self.gfx_channel_id {
                        gfx_pdus.extend(parse_gfx_pdus(chunk));
                    }
                }
            }
            0x02 => {
                // CMD_DATA_FIRST
                if let (Ok(channel_id), Ok(total_len)) = (
                    read_dvc_var(&mut cursor, cb_id),
                    read_dvc_var(&mut cursor, sp),
                ) {
                    let chunk = cursor.read_rest();
                    if Some(channel_id) == self.gfx_channel_id {
                        let mut buf = Vec::with_capacity(total_len as usize);
                        buf.extend_from_slice(chunk);
                        if buf.len() >= total_len as usize {
                            gfx_pdus.extend(parse_gfx_pdus(&buf));
                        } else {
                            self.reassembly = Some((total_len, buf));
                        }
                    }
                }
            }
            _ => {}
        }

        let resp = if responses.is_empty() {
            None
        } else {
            Some(responses)
        };
        (resp, gfx_pdus)
    }
}

#[cfg(feature = "gfx")]
#[tokio::test]
async fn test_e2e_gfx_avc420_streaming_and_ack() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (tls, pub_key) = create_tls_acceptor_and_pubkey();
    let creds = Credentials {
        username: "testuser".to_string(),
        password: "testpassword".to_string(),
        domain: None,
    };
    let validator = Arc::new(ExactMatchCredentialValidator::new(creds.clone()));

    let (display, display_tx) = ChannelDisplay::new(64, 64);
    let server = RdpServer::builder()
        .with_listener(listener)
        .with_tls(tls)
        .with_tls_public_key(pub_key)
        .with_display_handler(display)
        .with_input_handler(NoopInput)
        .with_credential_validator(Some(validator))
        .with_nla_credentials(Some(creds))
        .with_gfx(true)
        .build();

    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    let mut client = MockRdpClient::connect(
        addr,
        64,
        64,
        "testuser",
        "testpassword",
        vec!["drdynvc".to_string()],
    )
    .await;

    let drdynvc_channel_id = 1004;
    let mut dvc_client = MockDvcClient::new();
    let mut caps_advertised = false;
    let mut got_surface_configured = false;
    let mut got_h264_frame = false;
    let mut acknowledged_frame = false;

    let success = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = client.read_frame().await;
            if frame.len() > 10
                && frame[0] == 0x03
                && let Ok(send_data) = SendData::decode_indication(&frame[7..])
                && send_data.channel_id == drdynvc_channel_id
                && let Ok((_, _, payload)) = svc::dechunkify(&send_data.data)
            {
                let (dvc_resp, gfx_pdus) = dvc_client.handle_drdynvc_frame(payload);
                if let Some(resp) = dvc_resp {
                    client.send_channel_data(drdynvc_channel_id, &resp).await;

                    if let Some(gfx_chan) = dvc_client.gfx_channel_id
                        && !caps_advertised
                    {
                        caps_advertised = true;
                        let caps = encode_gfx_caps_advertise(&[
                            rdpcore_rdpegfx::pdu::RawCapabilitySet::flags_only(
                                rdpcore_rdpegfx::pdu::CAP_VERSION_81,
                                rdpcore_rdpegfx::pdu::CAPS_FLAG_AVC420_ENABLED,
                            ),
                        ]);
                        let dvc_caps = encode_dvc_data_pdu(gfx_chan, &caps);
                        client
                            .send_channel_data(drdynvc_channel_id, &dvc_caps)
                            .await;
                    }
                }

                for pdu in gfx_pdus {
                    match pdu {
                        GfxServerPdu::CreateSurface {
                            surface_id: 1,
                            width: 64,
                            height: 64,
                        } => {
                            got_surface_configured = true;
                            // Send first frame update to server
                            let pixels = vec![0x80u8; 64 * 64 * 4];
                            let _ = display_tx.send(make_bitmap_update(64, 64, pixels));
                        }
                        GfxServerPdu::WireToSurface1Avc420 {
                            surface_id: 1,
                            data,
                            ..
                        } => {
                            let bitstream = extract_h264_bitstream(&data);
                            let nal_types = find_nal_unit_types(bitstream);
                            // Must contain SPS (7), PPS (8), and IDR (5)
                            if nal_types.contains(&7)
                                && nal_types.contains(&8)
                                && nal_types.contains(&5)
                            {
                                got_h264_frame = true;
                                if let Some(gfx_chan) = dvc_client.gfx_channel_id
                                    && !acknowledged_frame
                                {
                                    acknowledged_frame = true;
                                    let ack = encode_gfx_frame_acknowledge(0, 1);
                                    let dvc_ack = encode_dvc_data_pdu(gfx_chan, &ack);
                                    client.send_channel_data(drdynvc_channel_id, &dvc_ack).await;
                                    return true;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    })
    .await;

    server_task.abort();
    assert!(got_surface_configured, "GFX surface must be configured");
    assert!(
        got_h264_frame,
        "Must receive valid H.264 AVC420 frame with SPS/PPS/IDR"
    );
    assert!(
        success.unwrap_or(false),
        "GFX AVC420 streaming test timed out"
    );
}

#[cfg(feature = "gfx")]
#[tokio::test]
async fn test_e2e_gfx_dynamic_resize() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (tls, pub_key) = create_tls_acceptor_and_pubkey();
    let creds = Credentials {
        username: "testuser".to_string(),
        password: "testpassword".to_string(),
        domain: None,
    };
    let validator = Arc::new(ExactMatchCredentialValidator::new(creds.clone()));

    let (display, display_tx) = ChannelDisplay::new(64, 64);
    let server = RdpServer::builder()
        .with_listener(listener)
        .with_tls(tls)
        .with_tls_public_key(pub_key)
        .with_display_handler(display)
        .with_input_handler(NoopInput)
        .with_credential_validator(Some(validator))
        .with_nla_credentials(Some(creds))
        .with_gfx(true)
        .build();

    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    let mut client = MockRdpClient::connect(
        addr,
        64,
        64,
        "testuser",
        "testpassword",
        vec!["drdynvc".to_string()],
    )
    .await;

    let drdynvc_channel_id = 1004;
    let mut dvc_client = MockDvcClient::new();
    let mut caps_advertised = false;
    let mut got_surface_1 = false;
    let mut got_surface_2 = false;
    let mut deleted_surface_1 = false;

    let success = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = client.read_frame().await;
            if frame.len() > 10
                && frame[0] == 0x03
                && let Ok(send_data) = SendData::decode_indication(&frame[7..])
                && send_data.channel_id == drdynvc_channel_id
                && let Ok((_, _, payload)) = svc::dechunkify(&send_data.data)
            {
                let (dvc_resp, gfx_pdus) = dvc_client.handle_drdynvc_frame(payload);
                if let Some(resp) = dvc_resp {
                    client.send_channel_data(drdynvc_channel_id, &resp).await;

                    if let Some(gfx_chan) = dvc_client.gfx_channel_id
                        && !caps_advertised
                    {
                        caps_advertised = true;
                        let caps = encode_gfx_caps_advertise(&[
                            rdpcore_rdpegfx::pdu::RawCapabilitySet::flags_only(
                                rdpcore_rdpegfx::pdu::CAP_VERSION_81,
                                rdpcore_rdpegfx::pdu::CAPS_FLAG_AVC420_ENABLED,
                            ),
                        ]);
                        let dvc_caps = encode_dvc_data_pdu(gfx_chan, &caps);
                        client
                            .send_channel_data(drdynvc_channel_id, &dvc_caps)
                            .await;
                    }
                }

                for pdu in gfx_pdus {
                    match pdu {
                        GfxServerPdu::CreateSurface {
                            surface_id: 1,
                            width: 64,
                            height: 64,
                        } => {
                            got_surface_1 = true;
                            // Send initial 64x64 frame
                            let pixels = vec![0x55u8; 64 * 64 * 4];
                            let _ = display_tx.send(make_bitmap_update(64, 64, pixels));
                        }
                        GfxServerPdu::WireToSurface1Avc420 { surface_id: 1, .. } => {
                            // Once first frame arrived, trigger resize to 80x48
                            let pixels2 = vec![0xAAu8; 80 * 48 * 4];
                            let _ = display_tx.send(make_bitmap_update(80, 48, pixels2));
                        }
                        GfxServerPdu::DeleteSurface { surface_id: 1 } => {
                            deleted_surface_1 = true;
                        }
                        GfxServerPdu::CreateSurface {
                            surface_id: 2,
                            width: 80,
                            height: 48,
                        } => {
                            got_surface_2 = true;
                        }
                        GfxServerPdu::WireToSurface1Avc420 {
                            surface_id: 2,
                            width: 80,
                            height: 48,
                            data,
                        } => {
                            let bitstream = extract_h264_bitstream(&data);
                            let nal_types = find_nal_unit_types(bitstream);
                            if nal_types.contains(&7)
                                && nal_types.contains(&8)
                                && nal_types.contains(&5)
                            {
                                return true;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    })
    .await;

    server_task.abort();
    assert!(got_surface_1, "Surface 1 must be created");
    assert!(deleted_surface_1, "Surface 1 must be deleted on resize");
    assert!(
        got_surface_2,
        "Surface 2 must be created with new dimensions 80x48"
    );
    assert!(
        success.unwrap_or(false),
        "GFX dynamic resize test timed out"
    );
}

#[cfg(feature = "gfx")]
#[tokio::test]
async fn test_e2e_gfx_fallback_to_planar_on_unsupported_caps() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (tls, pub_key) = create_tls_acceptor_and_pubkey();
    let creds = Credentials {
        username: "testuser".to_string(),
        password: "testpassword".to_string(),
        domain: None,
    };
    let validator = Arc::new(ExactMatchCredentialValidator::new(creds.clone()));

    let width = 64u16;
    let height = 64u16;
    let mut expected_image = vec![0u8; usize::from(width) * usize::from(height) * 4];
    for y in 0..usize::from(height) {
        for x in 0..usize::from(width) {
            let idx = (y * usize::from(width) + x) * 4;
            expected_image[idx] = 0x12; // B
            expected_image[idx + 1] = 0x34; // G
            expected_image[idx + 2] = 0x56; // R
            expected_image[idx + 3] = 0; // X
        }
    }

    let (display, display_tx) = ChannelDisplay::new(width, height);
    let server = RdpServer::builder()
        .with_listener(listener)
        .with_tls(tls)
        .with_tls_public_key(pub_key)
        .with_display_handler(display)
        .with_input_handler(NoopInput)
        .with_credential_validator(Some(validator))
        .with_nla_credentials(Some(creds))
        .with_gfx(true)
        .build();

    let server_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    let mut client = MockRdpClient::connect(
        addr,
        width,
        height,
        "testuser",
        "testpassword",
        vec!["drdynvc".to_string()],
    )
    .await;

    let drdynvc_channel_id = 1004;
    let mut dvc_client = MockDvcClient::new();
    let mut caps_advertised = false;
    let mut reconstructed = vec![0u8; usize::from(width) * usize::from(height) * 4];
    let mut total_tiles_decoded = 0;

    let success = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = client.read_frame().await;
            if frame.is_empty() {
                continue;
            }

            // Check if it's a FastPath output packet (Planar fallback)
            if frame[0] & 0x03 == 0 {
                if let Ok(fastpath) = FastPathOutput::decode(&frame) {
                    for update in fastpath.updates {
                        if update.update_code != UPDATE_CODE_BITMAP {
                            continue;
                        }
                        if let Ok(bitmap_data) = fastpath::BitmapUpdateData::decode(&update.data) {
                            for rect in bitmap_data.rectangles {
                                let rect_w = usize::from(rect.width);
                                let rect_h = usize::from(rect.height);
                                let bgrx_pixels = if rect.compressed_scan_width.is_some() {
                                    rdp6::decode(&rect.data, rect_w, rect_h)
                                        .expect("decode rdp6 planar tile")
                                } else {
                                    rect.data.clone()
                                };

                                for ry in 0..rect_h {
                                    let cy = usize::from(rect.dest_top) + (rect_h - 1 - ry);
                                    let cx = usize::from(rect.dest_left);
                                    let dst_start = (cy * usize::from(width) + cx) * 4;
                                    let src_start = (ry * rect_w) * 4;
                                    reconstructed[dst_start..dst_start + rect_w * 4]
                                        .copy_from_slice(
                                            &bgrx_pixels[src_start..src_start + rect_w * 4],
                                        );
                                }
                                total_tiles_decoded += 1;
                                if total_tiles_decoded >= 1 {
                                    return true;
                                }
                            }
                        }
                    }
                }
            } else if frame.len() > 10
                && frame[0] == 0x03
                && let Ok(send_data) = SendData::decode_indication(&frame[7..])
                && send_data.channel_id == drdynvc_channel_id
                && let Ok((_, _, payload)) = svc::dechunkify(&send_data.data)
            {
                let (dvc_resp, _) = dvc_client.handle_drdynvc_frame(payload);
                if let Some(resp) = dvc_resp {
                    client.send_channel_data(drdynvc_channel_id, &resp).await;

                    if let Some(gfx_chan) = dvc_client.gfx_channel_id
                        && !caps_advertised
                    {
                        caps_advertised = true;
                        // Send CapsAdvertise with NO AVC420 support (flags = 0)
                        let caps = encode_gfx_caps_advertise(&[
                            rdpcore_rdpegfx::pdu::RawCapabilitySet::flags_only(
                                rdpcore_rdpegfx::pdu::CAP_VERSION_81,
                                0,
                            ),
                        ]);
                        let dvc_caps = encode_dvc_data_pdu(gfx_chan, &caps);
                        client
                            .send_channel_data(drdynvc_channel_id, &dvc_caps)
                            .await;

                        // Yield to let server process unsupported Caps and abandon GFX
                        tokio::time::sleep(Duration::from_millis(50)).await;

                        // Now push a bitmap frame to trigger fallback Planar encoding
                        let _ = display_tx.send(make_bitmap_update(
                            width,
                            height,
                            expected_image.clone(),
                        ));
                    }
                }
            }
        }
    })
    .await;

    server_task.abort();
    assert!(
        success.unwrap_or(false),
        "Must fall back to Planar tiles when AVC420 is unsupported"
    );
    assert_eq!(
        reconstructed, expected_image,
        "Reconstructed planar tiles must match"
    );
}
