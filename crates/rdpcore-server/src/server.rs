use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use rdpcore_cliprdr::{CliprdrBackendFactory, CliprdrChannel};
use rdpcore_connector::{AcceptedConnection, Acceptor, AcceptorEvent, ConnectorError};
use rdpcore_dvc::DvcMux;
use rdpcore_pdu::capability_sets::NsCodecNegotiated;
use rdpcore_pdu::fastpath::{
    self, FastPathInputEvent, UPDATE_CODE_BITMAP, UPDATE_CODE_SURFACE_COMMANDS, keyboard_flags,
};
use rdpcore_pdu::finalization::{
    DataPdu, MonitorDef, STREAM_UNDEFINED, ShareDataPduType, decode_refresh_rect,
    decode_suppress_output, encode_monitor_layout,
};
use rdpcore_pdu::surface_commands::{FRAME_ACTION_BEGIN, FRAME_ACTION_END, encode_frame_marker};
use rdpcore_rdpdr::{DriveConsumerFactory, RdpdrChannel};
use rdpcore_rdpeai::{AudioInputBackendFactory, AudioInputHandler};
#[cfg(feature = "gfx")]
use rdpcore_rdpegfx::{GfxSession, select_h264_encoder};
use rdpcore_rdpsnd::{RdpsndChannel, RdpsndServerMessage, SoundServerFactory};
use rdpcore_transport::{ChannelKey, ConnectionWriter, Frame, FrameSender, Priority};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tracing::Instrument as _;
use tracing::{debug, info, info_span, warn};

use crate::credentials::{CredentialValidator, Credentials};
use crate::credssp;
use crate::display::{BitmapUpdate, DesktopSize, DisplayUpdate, RdpServerDisplay};
use crate::input::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
use crate::transport::{SteadyStateFrame, read_steady_state_frame, read_tpkt_frame};

static NEXT_CONN_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub struct RdpServerBuilder {
    addr: Option<SocketAddr>,
    listener: Option<TcpListener>,
    tls: Option<TlsAcceptor>,
    tls_public_key: Option<Vec<u8>>,
    display: Option<Arc<dyn RdpServerDisplay>>,
    input: Option<Arc<Mutex<dyn RdpServerInputHandler>>>,
    credential_validator: Option<Arc<dyn CredentialValidator>>,
    /// Account used for CredSSP/NTLMv2 when the client negotiates NLA.
    nla_credentials: Option<Credentials>,
    sound_factory: Option<Arc<dyn SoundServerFactory>>,
    cliprdr_factory: Option<Arc<dyn CliprdrBackendFactory>>,
    audio_input_factory: Option<Arc<dyn AudioInputBackendFactory>>,
    drive_factory: Option<Arc<dyn DriveConsumerFactory>>,
    #[cfg(feature = "dvc-echo")]
    echo_smoke_test: bool,
}

impl RdpServerBuilder {
    fn new() -> Self {
        Self {
            addr: None,
            listener: None,
            tls: None,
            tls_public_key: None,
            display: None,
            input: None,
            credential_validator: None,
            nla_credentials: None,
            sound_factory: None,
            cliprdr_factory: None,
            audio_input_factory: None,
            drive_factory: None,
            #[cfg(feature = "dvc-echo")]
            echo_smoke_test: false,
        }
    }

    pub fn with_addr(mut self, addr: SocketAddr) -> Self {
        self.addr = Some(addr);
        self
    }

    /// Use an already-bound listener (e.g. so the caller can fail bind
    /// before allocating other resources such as a uinput device).
    pub fn with_listener(mut self, listener: TcpListener) -> Self {
        self.listener = Some(listener);
        self
    }

    pub fn with_tls(mut self, tls: TlsAcceptor) -> Self {
        self.tls = Some(tls);
        self
    }

    /// SubjectPublicKeyInfo DER of the certificate presented during TLS.
    /// Required for CredSSP `pubKeyAuth` when a client negotiates NLA.
    pub fn with_tls_public_key(mut self, public_key: Vec<u8>) -> Self {
        self.tls_public_key = Some(public_key);
        self
    }

    pub fn with_display_handler(mut self, display: impl RdpServerDisplay + 'static) -> Self {
        self.display = Some(Arc::new(display));
        self
    }

    pub fn with_input_handler(mut self, input: impl RdpServerInputHandler + 'static) -> Self {
        self.input = Some(Arc::new(Mutex::new(input)));
        self
    }

    pub fn with_credential_validator(
        mut self,
        validator: Option<Arc<dyn CredentialValidator>>,
    ) -> Self {
        self.credential_validator = validator;
        self
    }

    /// Credentials CredSSP/NTLMv2 uses to verify the client's challenge
    /// response. Typically the same account as `with_credential_validator`.
    pub fn with_nla_credentials(mut self, credentials: Option<Credentials>) -> Self {
        self.nla_credentials = credentials;
        self
    }

    pub fn with_sound_factory(mut self, factory: Option<Box<dyn SoundServerFactory>>) -> Self {
        self.sound_factory = factory.map(Arc::from);
        self
    }

    pub fn with_cliprdr_factory(mut self, factory: Option<Box<dyn CliprdrBackendFactory>>) -> Self {
        self.cliprdr_factory = factory.map(Arc::from);
        self
    }

    pub fn with_audio_input_factory(
        mut self,
        factory: Option<Box<dyn AudioInputBackendFactory>>,
    ) -> Self {
        self.audio_input_factory = factory.map(Arc::from);
        self
    }

    pub fn with_drive_factory(mut self, factory: Option<Box<dyn DriveConsumerFactory>>) -> Self {
        self.drive_factory = factory.map(Arc::from);
        self
    }

    /// Opens a trivial MS-RDPEECO Echo dynamic channel on every connection
    /// and logs whether the client echoed the payload back correctly -
    /// purely a diagnostic to confirm the DVC transport itself is healthy.
    /// Requires the `dvc-echo` cargo feature.
    #[cfg(feature = "dvc-echo")]
    pub fn with_echo_smoke_test(mut self, enabled: bool) -> Self {
        self.echo_smoke_test = enabled;
        self
    }

    pub fn build(self) -> RdpServer {
        assert!(
            self.addr.is_some() || self.listener.is_some(),
            "with_addr or with_listener is required"
        );
        RdpServer {
            addr: self.addr,
            listener: self.listener,
            tls: self.tls.expect("with_tls is required"),
            tls_public_key: self.tls_public_key.unwrap_or_default(),
            display: self.display.expect("with_display_handler is required"),
            input: self.input.expect("with_input_handler is required"),
            credential_validator: self.credential_validator,
            nla_credentials: self.nla_credentials,
            sound_factory: self.sound_factory,
            cliprdr_factory: self.cliprdr_factory,
            audio_input_factory: self.audio_input_factory,
            drive_factory: self.drive_factory,
            #[cfg(feature = "dvc-echo")]
            echo_smoke_test: self.echo_smoke_test,
        }
    }
}

pub struct RdpServer {
    addr: Option<SocketAddr>,
    listener: Option<TcpListener>,
    tls: TlsAcceptor,
    tls_public_key: Vec<u8>,
    display: Arc<dyn RdpServerDisplay>,
    input: Arc<Mutex<dyn RdpServerInputHandler>>,
    credential_validator: Option<Arc<dyn CredentialValidator>>,
    nla_credentials: Option<Credentials>,
    sound_factory: Option<Arc<dyn SoundServerFactory>>,
    cliprdr_factory: Option<Arc<dyn CliprdrBackendFactory>>,
    audio_input_factory: Option<Arc<dyn AudioInputBackendFactory>>,
    drive_factory: Option<Arc<dyn DriveConsumerFactory>>,
    #[cfg(feature = "dvc-echo")]
    echo_smoke_test: bool,
}

/// Per-connection clone of the shared server handles. Accepting a new
/// client clones this and runs it on a dedicated task so sessions proceed
/// concurrently.
struct Session {
    tls: TlsAcceptor,
    tls_public_key: Vec<u8>,
    display: Arc<dyn RdpServerDisplay>,
    input: Arc<Mutex<dyn RdpServerInputHandler>>,
    credential_validator: Option<Arc<dyn CredentialValidator>>,
    nla_credentials: Option<Credentials>,
    sound_factory: Option<Arc<dyn SoundServerFactory>>,
    cliprdr_factory: Option<Arc<dyn CliprdrBackendFactory>>,
    audio_input_factory: Option<Arc<dyn AudioInputBackendFactory>>,
    drive_factory: Option<Arc<dyn DriveConsumerFactory>>,
    #[cfg(feature = "dvc-echo")]
    echo_smoke_test: bool,
}

impl RdpServer {
    pub fn builder() -> RdpServerBuilder {
        RdpServerBuilder::new()
    }

    fn session(&self) -> Session {
        Session {
            tls: self.tls.clone(),
            tls_public_key: self.tls_public_key.clone(),
            display: Arc::clone(&self.display),
            input: Arc::clone(&self.input),
            credential_validator: self.credential_validator.clone(),
            nla_credentials: self.nla_credentials.clone(),
            sound_factory: self.sound_factory.clone(),
            cliprdr_factory: self.cliprdr_factory.clone(),
            audio_input_factory: self.audio_input_factory.clone(),
            drive_factory: self.drive_factory.clone(),
            #[cfg(feature = "dvc-echo")]
            echo_smoke_test: self.echo_smoke_test,
        }
    }

    /// Accepts connections and runs each on its own task. Display capture
    /// and input injection are shared across sessions (see kmsrdp's
    /// `DisplayHub` / `SharedInput`); audio backends are per-connection,
    /// clipboard backends share one process-wide local poller.
    pub async fn run(mut self) -> anyhow::Result<()> {
        let listener = match self.listener.take() {
            Some(listener) => listener,
            None => {
                let addr = self.addr.expect("with_addr or with_listener is required");
                TcpListener::bind(addr).await?
            }
        };
        let server = Arc::new(self);
        loop {
            let (tcp, peer) = listener.accept().await?;
            let session = server.session();
            let conn_id = NEXT_CONN_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tokio::spawn(
                async move {
                    if let Err(e) = session.handle_connection(tcp).await {
                        warn!(error = %e, "connection ended");
                    }
                }
                .instrument(info_span!("rdp", conn_id, %peer)),
            );
        }
    }
}

impl Session {
    async fn handle_connection(&self, mut tcp: TcpStream) -> anyhow::Result<()> {
        let peer = tcp.peer_addr()?;
        let desktop = self.display.size().await;
        let mut acceptor = Acceptor::new(desktop.width, desktop.height);

        // Connection Request/Confirm is always cleartext, even under
        // PROTOCOL_SSL / PROTOCOL_HYBRID - the TLS handshake only starts
        // after this.
        let frame = read_tpkt_frame(&mut tcp).await?;
        let result = acceptor.step(&frame).map_err(|e| {
            warn!("cleartext negotiation PDU error: {e}");
            e
        })?;
        tcp.write_all(&result.response).await?;
        tcp.flush().await?;
        match result.event {
            AcceptorEvent::TlsUpgrade => {
                if acceptor.requires_credssp() {
                    info!("negotiation ok (NLA/HYBRID), starting TLS");
                } else {
                    info!("negotiation ok (TLS), starting TLS");
                }
            }
            AcceptorEvent::Rejected => {
                warn!(
                    "rejected at negotiation - client offered neither \
                     PROTOCOL_HYBRID nor PROTOCOL_SSL"
                );
                return Ok(());
            }
            other => anyhow::bail!("unexpected acceptor event before TLS upgrade: {other:?}"),
        }

        let mut tls = match self.tls.accept(tcp).await {
            Ok(stream) => {
                info!("TLS established");
                stream
            }
            Err(e) => {
                warn!("TLS handshake failed: {e}");
                return Err(e.into());
            }
        };

        // NLA: CredSSP runs on the TLS stream before MCS Connect Initial.
        let mut nla_authenticated = false;
        if acceptor.requires_credssp() {
            let Some(credentials) = self.nla_credentials.clone() else {
                info!("client requested NLA but server has no NLA credentials configured");
                return Ok(());
            };
            if self.tls_public_key.is_empty() {
                warn!("client requested NLA but server TLS public key is missing");
                return Ok(());
            }
            info!("starting CredSSP (NTLMv2)");
            match credssp::run_credssp_nla(
                &mut tls,
                self.tls_public_key.clone(),
                credentials,
                "kmsrdp",
            )
            .await
            {
                Ok(user) => {
                    info!("CredSSP succeeded for user {user:?}");
                    nla_authenticated = true;
                }
                Err(e) => {
                    warn!("CredSSP failed: {e}");
                    return Ok(());
                }
            }
        }

        let accepted = loop {
            let frame = read_tpkt_frame(&mut tls).await.map_err(|e| {
                warn!(
                    "read failed during handshake (waiting for {}): {e}",
                    acceptor.handshake_phase()
                );
                e
            })?;
            if frame.first() != Some(&0x03) {
                debug!(
                    "first byte during RDP handshake is 0x{:02x}, not TPKT 0x03",
                    frame.first().copied().unwrap_or(0)
                );
            }
            let result = acceptor.step(&frame).map_err(|e| {
                warn!(
                    "handshake PDU error while waiting for {}: {e}",
                    acceptor.handshake_phase()
                );
                e
            })?;
            match result.event {
                AcceptorEvent::None | AcceptorEvent::TlsUpgrade => {
                    if !result.response.is_empty() {
                        tls.write_all(&result.response).await?;
                        tls.flush().await?;
                    }
                }
                AcceptorEvent::ClientInfoReceived(credentials) => {
                    info!(
                        "client info user={:?} domain={:?} (nla={nla_authenticated})",
                        credentials.username, credentials.domain
                    );
                    let valid = if nla_authenticated {
                        // mstsc often sends an empty password after NLA; the
                        // CredSSP exchange already proved the account.
                        true
                    } else {
                        match &self.credential_validator {
                            Some(validator) => validator.validate(
                                &credentials.username,
                                &credentials.password,
                                &credentials.domain,
                            ),
                            None => true,
                        }
                    };
                    if !valid {
                        let password_hint = if credentials.password.is_empty() {
                            "password empty (mstsc did not send one - enter the KMSRDP_PASSWORD in the client, or enable NLA)"
                        } else {
                            "password non-empty but does not match KMSRDP_PASSWORD"
                        };
                        warn!(
                            "rejecting invalid credentials for user {:?} domain {:?} ({password_hint})",
                            credentials.username, credentials.domain
                        );
                        acceptor.reject_client_info();
                        return Ok(());
                    }
                    if !result.response.is_empty() {
                        tls.write_all(&result.response).await?;
                    }
                    tls.write_all(&acceptor.approve_client_info()?).await?;
                    tls.flush().await?;
                    info!("credentials accepted, sent Demand Active");
                }
                AcceptorEvent::Accepted(accepted) => {
                    if !result.response.is_empty() {
                        tls.write_all(&result.response).await?;
                        tls.flush().await?;
                    }
                    info!("handshake complete");
                    break accepted;
                }
                AcceptorEvent::Rejected => {
                    warn!("rejected during handshake");
                    return Ok(());
                }
            }
        };

        self.run_steady_state(peer, tls, acceptor, accepted).await
    }

    async fn run_steady_state<S>(
        &self,
        _peer: SocketAddr,
        stream: S,
        mut acceptor: Acceptor,
        accepted: AcceptedConnection,
    ) -> anyhow::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut read_half, write_half) = tokio::io::split(stream);
        let (writer, frame_sender) = ConnectionWriter::new(write_half);
        // Detached: it keeps running/flushing until every `FrameSender`
        // clone for this connection is dropped, which happens naturally
        // when this function returns.
        tokio::spawn(writer.run());

        let mut updates = self.display.updates().await?;

        let rdpsnd_channel_id = accepted
            .static_channels
            .iter()
            .find(|(name, _)| name == rdpcore_rdpsnd::pdu::CHANNEL_NAME)
            .map(|(_, id)| *id);
        let mut rdpsnd_audio_rx = None;
        let mut rdpsnd = match (rdpsnd_channel_id, &self.sound_factory) {
            (Some(channel_id), Some(factory)) => {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let (channel, initial) = RdpsndChannel::new(
                    channel_id,
                    accepted.user_channel_id,
                    factory.build_backend(tx),
                );
                for bytes in initial {
                    let _ = frame_sender.send(Frame {
                        channel: ChannelKey::Static(channel_id),
                        priority: Priority::Latency,
                        bytes,
                    });
                }
                rdpsnd_audio_rx = Some(rx);
                Some(channel)
            }
            (Some(_channel_id), None) => None,
            _ => None,
        };

        let cliprdr_channel_id = accepted
            .static_channels
            .iter()
            .find(|(name, _)| name == rdpcore_cliprdr::pdu::CHANNEL_NAME)
            .map(|(_, id)| *id);
        let mut cliprdr_event_rx = None;
        let mut cliprdr = match (cliprdr_channel_id, &self.cliprdr_factory) {
            (Some(channel_id), Some(factory)) => {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let (channel, initial) = CliprdrChannel::new(
                    channel_id,
                    accepted.user_channel_id,
                    factory.build_cliprdr_backend(tx),
                );
                for bytes in initial {
                    let _ = frame_sender.send(Frame {
                        channel: ChannelKey::Static(channel_id),
                        priority: Priority::Bulk,
                        bytes,
                    });
                }
                cliprdr_event_rx = Some(rx);
                Some(channel)
            }
            (Some(_channel_id), None) => None,
            _ => None,
        };

        let drdynvc_channel_id = accepted
            .static_channels
            .iter()
            .find(|(name, _)| name == rdpcore_dvc::pdu::CHANNEL_NAME)
            .map(|(_, id)| *id);
        let mut dvc = drdynvc_channel_id.map(|channel_id| {
            let (mut mux, initial) = DvcMux::new(channel_id, accepted.user_channel_id);
            for bytes in initial {
                let _ = frame_sender.send(Frame {
                    channel: ChannelKey::Static(channel_id),
                    priority: Priority::Latency,
                    bytes,
                });
            }
            #[cfg(feature = "dvc-echo")]
            if self.echo_smoke_test {
                let echo_frames =
                    mux.register_channel(Box::new(rdpcore_dvc::echo::EchoHandler::new(
                        b"kmsrdp-dvc-smoketest".to_vec(),
                        |matched| {
                            if matched {
                                info!("DVC echo smoke test: OK, payload round-tripped correctly");
                            } else {
                                warn!("DVC echo smoke test: FAILED, echoed payload did not match");
                            }
                        },
                    )));
                info!(
                    "DVC echo smoke test: queued {} follow-up frame(s)",
                    echo_frames.len()
                );
                for bytes in echo_frames {
                    let _ = frame_sender.send(Frame {
                        channel: ChannelKey::Static(channel_id),
                        priority: Priority::Latency,
                        bytes,
                    });
                }
            }
            if let Some(factory) = &self.audio_input_factory {
                let audio_input_frames =
                    mux.register_channel(Box::new(AudioInputHandler::new(factory.build_backend())));
                for bytes in audio_input_frames {
                    let _ = frame_sender.send(Frame {
                        channel: ChannelKey::Static(channel_id),
                        priority: Priority::Latency,
                        bytes,
                    });
                }
            }
            mux
        });

        #[cfg(feature = "gfx")]
        let gfx_session = if gfx_env_enabled() {
            match select_h264_encoder() {
                Ok(selected) => {
                    let session = GfxSession::new(
                        selected.encoder,
                        accepted.desktop_width,
                        accepted.desktop_height,
                    );
                    if let Some(mux) = dvc.as_mut() {
                        let frames = mux.register_channel(Box::new(session.dvc_handler()));
                        for bytes in frames {
                            let _ = frame_sender.send(Frame {
                                channel: ChannelKey::Static(mux.channel_id()),
                                priority: Priority::Bulk,
                                bytes,
                            });
                        }
                        info!(encoder = selected.name, "GFX AVC420 channel registered");
                    }
                    Some(session)
                }
                Err(e) => {
                    warn!("GFX encoder unavailable ({e}); using Planar/NSCodec");
                    None
                }
            }
        } else {
            info!("GFX disabled (KMSRDP_GFX=0); using Planar/NSCodec");
            None
        };

        let rdpdr_channel_id = accepted
            .static_channels
            .iter()
            .find(|(name, _)| name == rdpcore_rdpdr::pdu::CHANNEL_NAME)
            .map(|(_, id)| *id);
        let mut rdpdr_wake_rx = None;
        let mut rdpdr = match (rdpdr_channel_id, &self.drive_factory) {
            (Some(channel_id), Some(factory)) => {
                let (wake_tx, wake_rx) = tokio::sync::mpsc::unbounded_channel();
                let (channel, initial) = RdpdrChannel::new(
                    channel_id,
                    accepted.user_channel_id,
                    factory.supported_device_types(),
                    factory.build_drive_consumer(wake_tx),
                );
                for bytes in initial {
                    let _ = frame_sender.send(Frame {
                        channel: ChannelKey::Static(channel_id),
                        priority: Priority::Latency,
                        bytes,
                    });
                }
                rdpdr_wake_rx = Some(wake_rx);
                Some(channel)
            }
            (Some(_channel_id), None) => None,
            _ => None,
        };

        let client_label = trim_client_name(&accepted.client_name);
        let server_mfu = 8 * 1024 * 1024u32;
        let max_request_size = accepted
            .max_request_size
            .unwrap_or(server_mfu)
            .min(server_mfu)
            .max(fastpath::MAX_FASTPATH_CHUNK_SIZE as u32);
        let bitmap_policy =
            bitmap_encode_policy(client_label, accepted.nscodec, max_request_size as usize);
        let defer_ms = initial_bitmap_defer_ms(client_label);
        let mut bitmap_gate_open = defer_ms == 0;
        let mut bitmap_gate = Box::pin(tokio::time::sleep(std::time::Duration::from_millis(
            defer_ms,
        )));
        let mut deferred_bitmap: Option<BitmapUpdate> = None;
        let mut display_updates_allowed = true;
        let mut frame_id = 1u32;
        let io_channel_id = accepted.io_channel_id;

        // No soft-cursor PDUs: we do not track the host cursor shape, and a
        // DIY Color Pointer showed up as a square block beside the client's
        // local cursor. Leave pointer drawing to the client.

        // Advertise host monitor rectangles when the virtual desktop spans
        // more than one CRTC (clients may ignore this).
        let monitors = self.display.monitor_layout();
        if monitors.len() > 1 {
            let defs: Vec<MonitorDef> = monitors
                .iter()
                .map(|m| MonitorDef {
                    left: m.left,
                    top: m.top,
                    right: m.right,
                    bottom: m.bottom,
                    primary: m.primary,
                })
                .collect();
            let body = DataPdu {
                share_id: accepted.share_id,
                pdu_source: io_channel_id,
                stream_id: STREAM_UNDEFINED,
                pdu_type2: ShareDataPduType::MonitorLayout,
                body: encode_monitor_layout(&defs),
            }
            .encode();
            let bytes = rdpcore_pdu::x224::wrap_data(
                &rdpcore_pdu::mcs::SendData {
                    initiator: accepted.user_channel_id,
                    channel_id: io_channel_id,
                    data: body,
                    complete: true,
                }
                .encode_indication(),
            );
            let _ = frame_sender.send(Frame {
                channel: ChannelKey::Io,
                priority: Priority::Latency,
                bytes,
            });
        }

        // Set while a server-initiated resize (Deactivate-All + new Demand
        // Active, see `Acceptor::begin_resize`) is in flight: slow-path
        // frames on the IO channel go to the acceptor instead of the usual
        // channel dispatch, and bitmap updates are held back until the
        // client confirms the new dimensions, since a frame sized for the
        // old (or new, ahead of confirmation) desktop would desync the
        // client's canvas otherwise.
        //
        // mstsc clears its canvas on Deactivate-All and is often slower than
        // Guacamole to finish Confirm Active + finalization. Capture usually
        // emits the post-resize full frame during that window; dropping it
        // leaves mstsc black forever on a static desktop. Retain the best
        // frame and flush it once the resize is confirmed.
        let mut resizing = false;
        let mut resize_desktop = DesktopSize {
            width: accepted.desktop_width,
            height: accepted.desktop_height,
        };
        let mut pending_after_resize: Option<BitmapUpdate> = None;
        #[cfg(feature = "gfx")]
        let mut last_gfx_data: Option<std::sync::Arc<[u8]>> = None;

        loop {
            tokio::select! {
                _ = &mut bitmap_gate, if !bitmap_gate_open => {
                    bitmap_gate_open = true;
                    if display_updates_allowed
                        && let Some(bitmap) = deferred_bitmap.take()
                    {
                        let full = updates.latest_full_frame();
                        #[cfg(feature = "gfx")]
                        let gfx_attempt = Some(try_send_gfx_frame(
                            gfx_session.as_ref(),
                            dvc.as_ref(),
                            &mut last_gfx_data,
                            full.as_ref(),
                            &bitmap,
                            &frame_sender,
                        ));
                        if send_outbound_frame(
                            &bitmap,
                            &frame_sender,
                            &bitmap_policy,
                            &mut frame_id,
                            full.as_ref(),
                            #[cfg(feature = "gfx")]
                            gfx_attempt,
                        )
                        .await
                        .is_err()
                        {
                            return Ok(());
                        }
                    }
                }
                frame = read_steady_state_frame(&mut read_half) => {
                    match frame {
                        Err(e) => return Err(e.into()),
                        Ok(SteadyStateFrame::FastPathInput(bytes)) => {
                            match fastpath::FastPathInput::decode(&bytes) {
                                Ok(input_pdu) => {
                                    let mut input = self.input.lock().unwrap();
                                    for event in input_pdu.events {
                                        dispatch_input_event(&mut *input, event);
                                    }
                                }
                                Err(e) => debug!("dropping malformed fast-path input frame: {e}"),
                            }
                        }
                        Ok(SteadyStateFrame::SlowPath(bytes)) if resizing => {
                            // Handshake may already be done (batched FontList in a
                            // prior frame, or a missed Accepted event). Never call
                            // step() on a finished acceptor — that only spams
                            // AlreadyFinished and keeps the client black.
                            if acceptor.is_finished() {
                                resizing = false;
                                if flush_pending_resize_bitmap(
                                    &mut pending_after_resize,
                                    &frame_sender,
                                    &bitmap_policy,
                                    &mut frame_id,
                                    display_updates_allowed,
                                )
                                .await
                                .is_err()
                                {
                                    return Ok(());
                                }
                                if let Err(e) = handle_slow_path_frame(
                                    &bytes,
                                    io_channel_id,
                                    &mut display_updates_allowed,
                                    updates.as_mut(),
                                    rdpsnd.as_mut(),
                                    cliprdr.as_mut(),
                                    dvc.as_mut(),
                                    rdpdr.as_mut(),
                                    &frame_sender,
                                    &bitmap_policy,
                                    &mut frame_id,
                                )
                                .await
                                {
                                    debug!("dropping malformed slow-path frame after resize: {e}");
                                }
                                continue;
                            }
                            match acceptor.step(&bytes) {
                                Ok(result) => {
                                    if !result.response.is_empty()
                                        && frame_sender
                                            .send(Frame { channel: ChannelKey::Io, priority: Priority::Latency, bytes: result.response })
                                            .is_err()
                                    {
                                        return Ok(());
                                    }
                                    if acceptor.is_finished()
                                        || matches!(result.event, AcceptorEvent::Accepted(_))
                                    {
                                        resizing = false;
                                        if flush_pending_resize_bitmap(
                                            &mut pending_after_resize,
                                            &frame_sender,
                                            &bitmap_policy,
                                            &mut frame_id,
                                            display_updates_allowed,
                                        )
                                        .await
                                        .is_err()
                                        {
                                            return Ok(());
                                        }
                                    }
                                }
                                Err(e) => {
                                    if acceptor.is_finished()
                                        || matches!(e, ConnectorError::AlreadyFinished)
                                    {
                                        resizing = false;
                                        if flush_pending_resize_bitmap(
                                            &mut pending_after_resize,
                                            &frame_sender,
                                            &bitmap_policy,
                                            &mut frame_id,
                                            display_updates_allowed,
                                        )
                                        .await
                                        .is_err()
                                        {
                                            return Ok(());
                                        }
                                        if let Err(err) = handle_slow_path_frame(
                                            &bytes,
                                            io_channel_id,
                                            &mut display_updates_allowed,
                                            updates.as_mut(),
                                            rdpsnd.as_mut(),
                                            cliprdr.as_mut(),
                                            dvc.as_mut(),
                                            rdpdr.as_mut(),
                                            &frame_sender,
                                            &bitmap_policy,
                                            &mut frame_id,
                                        )
                                        .await
                                        {
                                            debug!(
                                                "dropping malformed slow-path frame after resize: {err}"
                                            );
                                        }
                                    } else {
                                        debug!("dropping malformed frame during resize: {e}");
                                    }
                                }
                            }
                        }
                        Ok(SteadyStateFrame::SlowPath(bytes)) => {
                            if let Err(e) = handle_slow_path_frame(
                                &bytes,
                                io_channel_id,
                                &mut display_updates_allowed,
                                updates.as_mut(),
                                rdpsnd.as_mut(),
                                cliprdr.as_mut(),
                                dvc.as_mut(),
                                rdpdr.as_mut(),
                                &frame_sender,
                                &bitmap_policy,
                                &mut frame_id,
                            )
                            .await
                            {
                                debug!("dropping malformed slow-path frame: {e}");
                            }
                        }
                    }
                }
                update = updates.next_update() => {
                    match update {
                        Err(e) => return Err(e),
                        Ok(Some(DisplayUpdate::Bitmap(bitmap))) if resizing => {
                            retain_bitmap_during_resize(
                                &mut pending_after_resize,
                                bitmap,
                                resize_desktop.width,
                                resize_desktop.height,
                            );
                        }
                        Ok(Some(DisplayUpdate::Bitmap(bitmap))) if !bitmap_gate_open => {
                            deferred_bitmap = Some(bitmap);
                        }
                        Ok(Some(DisplayUpdate::Bitmap(_))) if !display_updates_allowed => {}
                        Ok(Some(DisplayUpdate::Bitmap(bitmap))) => {
                            let full = updates.latest_full_frame();
                            #[cfg(feature = "gfx")]
                            let gfx_attempt = Some(try_send_gfx_frame(
                                gfx_session.as_ref(),
                                dvc.as_ref(),
                                &mut last_gfx_data,
                                full.as_ref(),
                                &bitmap,
                                &frame_sender,
                            ));
                            if send_outbound_frame(
                                &bitmap,
                                &frame_sender,
                                &bitmap_policy,
                                &mut frame_id,
                                full.as_ref(),
                                #[cfg(feature = "gfx")]
                                gfx_attempt,
                            )
                            .await
                            .is_err()
                            {
                                return Ok(());
                            }
                        }
                        Ok(Some(DisplayUpdate::Resized(size))) if resizing => {
                            debug!("dropping resize to {}x{}: a previous resize is still in flight", size.width, size.height);
                        }
                        Ok(Some(DisplayUpdate::Resized(size))) => {
                            #[cfg(feature = "gfx")]
                            if let (Some(gfx), Some(mux)) = (gfx_session.as_ref(), dvc.as_ref())
                                && let Some(payloads) = gfx.resize(size.width, size.height)
                            {
                                let _ = send_gfx_payloads(mux, &frame_sender, payloads);
                                last_gfx_data = None;
                            }
                            match acceptor.begin_resize(size.width, size.height) {
                                Ok(response) => {
                                    resizing = true;
                                    resize_desktop = size;
                                    pending_after_resize = None;
                                    if frame_sender.send(Frame { channel: ChannelKey::Io, priority: Priority::Latency, bytes: response }).is_err() {
                                        return Ok(());
                                    }
                                }
                                Err(e) => warn!("failed to start resize to {}x{}: {e}", size.width, size.height),
                            }
                        }
                        Ok(None) => return Ok(()),
                    }
                }
                wave = recv_optional(&mut rdpsnd_audio_rx) => {
                    let Some(RdpsndServerMessage::Wave(pcm, timestamp_ms)) = wave else { continue };
                    if let Some(channel) = rdpsnd.as_mut() {
                        let channel_id = channel.channel_id();
                        for bytes in channel.encode_wave(pcm, timestamp_ms) {
                            let _ = frame_sender.send(Frame { channel: ChannelKey::Static(channel_id), priority: Priority::Latency, bytes });
                        }
                    }
                }
                clipboard_event = recv_optional(&mut cliprdr_event_rx) => {
                    let Some(event) = clipboard_event else { continue };
                    if let Some(channel) = cliprdr.as_mut() {
                        let channel_id = channel.channel_id();
                        for bytes in channel.encode_message(event) {
                            let _ = frame_sender.send(Frame { channel: ChannelKey::Static(channel_id), priority: Priority::Bulk, bytes });
                        }
                    }
                }
                _ = recv_optional(&mut rdpdr_wake_rx) => {
                    if let Some(channel) = rdpdr.as_mut() {
                        let channel_id = channel.channel_id();
                        for bytes in channel.flush_pending_commands() {
                            if frame_sender
                                .send(Frame {
                                    channel: ChannelKey::Static(channel_id),
                                    priority: Priority::Latency,
                                    bytes,
                                })
                                .is_err()
                            {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
    }
}

fn trim_client_name(name: &str) -> &str {
    name.trim_end_matches('\0').trim()
}

fn client_needs_compat_workarounds(client_name: &str) -> bool {
    let n = client_name.to_ascii_lowercase();
    n.contains("mac") || n.contains("darwin") || n.contains("iphone") || n.contains("ipad")
}

fn initial_bitmap_defer_ms(client_name: &str) -> u64 {
    if client_needs_compat_workarounds(client_name) {
        400
    } else {
        0
    }
}

async fn send_outbound_bitmap(
    bitmap: &BitmapUpdate,
    frame_sender: &FrameSender,
    policy: &BitmapEncodePolicy,
    frame_id: &mut u32,
) -> Result<(), ()> {
    let batches = if let Some((codec_id, cll)) = policy.nscodec {
        encode_nscodec_update(bitmap, codec_id, cll, policy.max_request_size).0
    } else {
        encode_bitmap_update(bitmap, policy).0
    };

    let id = *frame_id;
    *frame_id = frame_id.wrapping_add(1).max(1);
    let begin = encode_update_to_wire_frames(
        UPDATE_CODE_SURFACE_COMMANDS,
        &encode_frame_marker(FRAME_ACTION_BEGIN, id),
        policy.max_request_size,
    );
    let end = encode_update_to_wire_frames(
        UPDATE_CODE_SURFACE_COMMANDS,
        &encode_frame_marker(FRAME_ACTION_END, id),
        policy.max_request_size,
    );

    for wire_frame in begin
        .into_iter()
        .chain(batches.into_iter().flatten())
        .chain(end)
    {
        if frame_sender
            .send(Frame {
                channel: ChannelKey::Io,
                priority: Priority::Bulk,
                bytes: wire_frame,
            })
            .is_err()
        {
            return Err(());
        }
    }
    Ok(())
}

/// Prefer GFX AVC420 when negotiated; otherwise Planar/NSCodec Fast-Path.
/// GFX work is synchronous so `&DvcMux` is never held across an await.
#[allow(clippy::too_many_arguments)]
async fn send_outbound_frame(
    bitmap: &BitmapUpdate,
    frame_sender: &FrameSender,
    policy: &BitmapEncodePolicy,
    frame_id: &mut u32,
    latest_full: Option<&BitmapUpdate>,
    #[cfg(feature = "gfx")] gfx_attempt: Option<Result<bool, ()>>,
) -> Result<(), ()> {
    #[cfg(feature = "gfx")]
    if let Some(result) = gfx_attempt {
        match result {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(()) => return Err(()),
        }
    }
    let _ = latest_full;
    send_outbound_bitmap(bitmap, frame_sender, policy, frame_id).await
}

/// Returns `Ok(true)` when the GFX path handled (or intentionally skipped) the
/// frame; `Ok(false)` to fall back to Planar/NSCodec.
#[cfg(feature = "gfx")]
fn try_send_gfx_frame(
    gfx: Option<&GfxSession>,
    dvc: Option<&DvcMux>,
    last_gfx_data: &mut Option<std::sync::Arc<[u8]>>,
    latest_full: Option<&BitmapUpdate>,
    bitmap: &BitmapUpdate,
    frame_sender: &FrameSender,
) -> Result<bool, ()> {
    let (Some(gfx), Some(mux)) = (gfx, dvc) else {
        return Ok(false);
    };
    if !gfx.is_ready() {
        return Ok(false);
    }
    let source = latest_full.unwrap_or(bitmap);
    // Do not skip on Arc ptr equality: a frozen/black client needs periodic
    // IDR refresh even when the captured buffer object is reused.
    if let Some(payloads) = gfx.encode_frame(
        source.width.get(),
        source.height.get(),
        source.stride.get(),
        source.data.as_ref(),
    ) {
        *last_gfx_data = Some(std::sync::Arc::clone(&source.data));
        send_gfx_payloads(mux, frame_sender, payloads)?;
        return Ok(true);
    }
    // Soft encode skip (e.g. transient OpenH264 RC): keep the GFX path so we
    // do not paint Planar over a black H.264 surface. Hard encoder init
    // failures never register the GFX channel in the first place.
    Ok(true)
}

#[cfg(feature = "gfx")]
fn send_gfx_payloads(
    mux: &DvcMux,
    frame_sender: &FrameSender,
    payloads: Vec<Vec<u8>>,
) -> Result<(), ()> {
    let Some(dyn_id) = mux.channel_id_for_name(rdpcore_rdpegfx::CHANNEL_NAME) else {
        return Err(());
    };
    for bytes in mux.wrap_channel_payloads(dyn_id, payloads) {
        if frame_sender
            .send(Frame {
                channel: ChannelKey::Static(mux.channel_id()),
                priority: Priority::Bulk,
                bytes,
            })
            .is_err()
        {
            return Err(());
        }
    }
    Ok(())
}

#[cfg(feature = "gfx")]
fn gfx_env_enabled() -> bool {
    // Opt-in until the AVC420 path is stable with mstsc. A GFX protocol
    // error otherwise drops the session and some clients refuse to reconnect
    // until the server process is restarted. Set KMSRDP_GFX=1 to enable.
    match std::env::var("KMSRDP_GFX") {
        Ok(v) => {
            let v = v.trim();
            v == "1"
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("on")
                || v.eq_ignore_ascii_case("yes")
        }
        Err(_) => false,
    }
}

/// Awaits the next message from an optional channel - never resolves if
/// there isn't one (no rdpsnd negotiated for this connection), which is
/// exactly the right behavior for a `tokio::select!` branch that should
/// simply never fire in that case.
async fn recv_optional<T>(rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Slow-path traffic at steady state: static channels plus IO-channel
/// Suppress Output / Refresh Rect (MS-RDPBCGR 2.2.11).
#[allow(clippy::too_many_arguments)]
async fn handle_slow_path_frame(
    bytes: &[u8],
    io_channel_id: u16,
    display_updates_allowed: &mut bool,
    updates: &mut dyn crate::display::RdpServerDisplayUpdates,
    rdpsnd: Option<&mut RdpsndChannel>,
    cliprdr: Option<&mut CliprdrChannel>,
    dvc: Option<&mut DvcMux>,
    rdpdr: Option<&mut RdpdrChannel>,
    frame_sender: &rdpcore_transport::FrameSender,
    policy: &BitmapEncodePolicy,
    frame_id: &mut u32,
) -> anyhow::Result<()> {
    let payload = rdpcore_pdu::x224::unwrap_data(bytes)?;
    let send_data = rdpcore_pdu::mcs::SendData::decode_request(payload)?;

    if send_data.channel_id == io_channel_id {
        if let Ok(data_pdu) = DataPdu::decode(&send_data.data) {
            match data_pdu.pdu_type2 {
                ShareDataPduType::SuppressOutput => {
                    if let Ok(allow) = decode_suppress_output(&data_pdu.body) {
                        let was = *display_updates_allowed;
                        *display_updates_allowed = allow;
                        if allow
                            && !was
                            && let Some(full) = updates.latest_full_frame()
                        {
                            let _ =
                                send_outbound_bitmap(&full, frame_sender, policy, frame_id).await;
                        }
                    }
                }
                ShareDataPduType::RefreshRect => {
                    if let Ok(rects) = decode_refresh_rect(&data_pdu.body)
                        && let Some(full) = updates.latest_full_frame()
                    {
                        if rects.is_empty() {
                            let _ =
                                send_outbound_bitmap(&full, frame_sender, policy, frame_id).await;
                        } else {
                            for rect in rects {
                                let w = rect.right.saturating_sub(rect.left).saturating_add(1);
                                let h = rect.bottom.saturating_sub(rect.top).saturating_add(1);
                                let (Some(nw), Some(nh)) =
                                    (core::num::NonZeroU16::new(w), core::num::NonZeroU16::new(h))
                                else {
                                    continue;
                                };
                                if let Some(sub) = full.sub(rect.left, rect.top, nw, nh) {
                                    let _ =
                                        send_outbound_bitmap(&sub, frame_sender, policy, frame_id)
                                            .await;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        return Ok(());
    }

    if let Some(channel) = rdpsnd
        && send_data.channel_id == channel.channel_id()
    {
        let channel_id = channel.channel_id();
        for response in channel.on_channel_data(&send_data.data)? {
            let _ = frame_sender.send(Frame {
                channel: ChannelKey::Static(channel_id),
                priority: Priority::Latency,
                bytes: response,
            });
        }
        return Ok(());
    }
    if let Some(channel) = cliprdr
        && send_data.channel_id == channel.channel_id()
    {
        let channel_id = channel.channel_id();
        for response in channel.on_channel_data(&send_data.data)? {
            let _ = frame_sender.send(Frame {
                channel: ChannelKey::Static(channel_id),
                priority: Priority::Bulk,
                bytes: response,
            });
        }
        return Ok(());
    }
    if let Some(mux) = dvc
        && send_data.channel_id == mux.channel_id()
    {
        let channel_id = mux.channel_id();
        for response in mux.on_channel_data(&send_data.data)? {
            let _ = frame_sender.send(Frame {
                channel: ChannelKey::Static(channel_id),
                priority: Priority::Latency,
                bytes: response,
            });
        }
        return Ok(());
    }
    if let Some(channel) = rdpdr
        && send_data.channel_id == channel.channel_id()
    {
        let channel_id = channel.channel_id();
        for response in channel.on_channel_data(&send_data.data)? {
            let _ = frame_sender.send(Frame {
                channel: ChannelKey::Static(channel_id),
                priority: Priority::Latency,
                bytes: response,
            });
        }
    }
    Ok(())
}

fn dispatch_input_event(input: &mut dyn RdpServerInputHandler, event: FastPathInputEvent) {
    match event {
        FastPathInputEvent::Scancode { flags, code } => {
            let extended = flags & (keyboard_flags::EXTENDED | keyboard_flags::EXTENDED1) != 0;
            input.keyboard(if flags & keyboard_flags::RELEASE != 0 {
                KeyboardEvent::Released { code, extended }
            } else {
                KeyboardEvent::Pressed { code, extended }
            });
        }
        FastPathInputEvent::Mouse {
            pointer_flags,
            x,
            y,
        } => {
            input.mouse(translate_mouse(pointer_flags, x, y));
        }
        FastPathInputEvent::Sync { .. } => {}
        FastPathInputEvent::Unicode { flags, code } => {
            if flags & keyboard_flags::RELEASE == 0 {
                input.keyboard(KeyboardEvent::UnicodePressed(code));
            }
        }
    }
}

fn translate_mouse(pointer_flags: u16, x: u16, y: u16) -> MouseEvent {
    const WHEEL_NEGATIVE: u16 = 0x0100;
    const VERTICAL_WHEEL: u16 = 0x0200;
    const LEFT_BUTTON: u16 = 0x1000;
    const RIGHT_BUTTON: u16 = 0x2000;
    const MIDDLE_BUTTON: u16 = 0x4000;
    const DOWN: u16 = 0x8000;

    if pointer_flags & VERTICAL_WHEEL != 0 {
        let raw = i32::from(pointer_flags & 0xFF);
        let value = if pointer_flags & WHEEL_NEGATIVE != 0 {
            raw - 256
        } else {
            raw
        };
        return MouseEvent::VerticalScroll { value };
    }
    let down = pointer_flags & DOWN != 0;
    if pointer_flags & LEFT_BUTTON != 0 {
        return if down {
            MouseEvent::LeftPressed
        } else {
            MouseEvent::LeftReleased
        };
    }
    if pointer_flags & RIGHT_BUTTON != 0 {
        return if down {
            MouseEvent::RightPressed
        } else {
            MouseEvent::RightReleased
        };
    }
    if pointer_flags & MIDDLE_BUTTON != 0 {
        return if down {
            MouseEvent::MiddlePressed
        } else {
            MouseEvent::MiddleReleased
        };
    }
    MouseEvent::Move { x, y }
}

/// `TS_BITMAP_DATA.bitmapLength` is a 16-bit field, so a single rectangle
/// can carry at most ~65535 bytes of raw pixel data (about 128x128 at
/// 32bpp) - a whole-frame update must be tiled into rectangles this small
/// or smaller before encoding, not just fragmented at the wire level
/// afterward (fragmentation splits already-encoded bytes; it can't fix a
/// `bitmapLength` field that overflowed before fragmentation even runs).
const TILE_SIZE: u16 = 64;

async fn flush_pending_resize_bitmap(
    pending: &mut Option<BitmapUpdate>,
    frame_sender: &FrameSender,
    policy: &BitmapEncodePolicy,
    frame_id: &mut u32,
    display_updates_allowed: bool,
) -> Result<(), ()> {
    if !display_updates_allowed {
        *pending = None;
        return Ok(());
    }
    let Some(bitmap) = pending.take() else {
        return Ok(());
    };
    send_outbound_bitmap(&bitmap, frame_sender, policy, frame_id).await
}

#[derive(Debug, Clone, Copy)]
struct BitmapEncodePolicy {
    use_rdp6_planar: bool,
    max_rects_per_update: usize,
    nscodec: Option<(u8, u8)>,
    max_request_size: usize,
}

const COMPAT_MAX_RECTS_PER_UPDATE: usize = 32;

fn max_raw_strip_height(width: u16) -> u16 {
    let row_bytes = usize::from(width).saturating_mul(4);
    if row_bytes == 0 {
        return 1;
    }
    (65535usize / row_bytes).max(1) as u16
}

fn bitmap_encode_policy(
    client_name: &str,
    nscodec: Option<NsCodecNegotiated>,
    max_request_size: usize,
) -> BitmapEncodePolicy {
    let compat_mode = client_needs_compat_workarounds(client_name);
    // macOS Windows App: prefer NSCodec SurfaceCommands (IronRDP path). Raw
    // fast-path bitmaps work but are ~9MB/frame; RDP6 planar disconnects.
    let nscodec = if compat_mode {
        nscodec.map(|n| (n.codec_id, n.color_loss_level))
    } else {
        None
    };
    // Keep each reassembled Fast-Path Update under MaxRequestSize. A 64x64
    // compressed tile is typically a few KB; use a conservative per-rect budget.
    let size_limited_rects = (max_request_size / 8192).max(1);
    let max_rects_per_update = if compat_mode {
        COMPAT_MAX_RECTS_PER_UPDATE.min(size_limited_rects)
    } else {
        size_limited_rects
    };
    BitmapEncodePolicy {
        use_rdp6_planar: !compat_mode,
        max_rects_per_update,
        nscodec,
        max_request_size,
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct BitmapWireStats {
    tiles: u32,
    compressed_tiles: u32,
    raw_tiles: u32,
    encoded_bytes: usize,
    update_batches: u32,
}

/// Splits one `BitmapUpdate` into wire-ready `FastPathOutput` byte buffers,
/// batched for strict clients (macOS Windows App).
fn encode_bitmap_update(
    bitmap: &BitmapUpdate,
    policy: &BitmapEncodePolicy,
) -> (Vec<Vec<Vec<u8>>>, BitmapWireStats) {
    let width = bitmap.width.get();
    let height = bitmap.height.get();
    let row_bytes = usize::from(width) * 4;

    let mut rectangles = Vec::new();
    let mut stats = BitmapWireStats::default();

    if policy.use_rdp6_planar {
        let mut tile_y = 0u16;
        while tile_y < height {
            let tile_height = TILE_SIZE.min(height - tile_y);
            let mut tile_x = 0u16;
            while tile_x < width {
                let tile_width = TILE_SIZE.min(width - tile_x);
                push_bitmap_rect(
                    bitmap,
                    row_bytes,
                    tile_x,
                    tile_y,
                    tile_width,
                    tile_height,
                    policy,
                    &mut rectangles,
                    &mut stats,
                );
                tile_x += TILE_SIZE;
            }
            tile_y += TILE_SIZE;
        }
    } else {
        // Raw: tile into full-width strips so each rect carries as many scanlines
        // as the 16-bit `bitmapLength` field allows (IronRDP-style chunking).
        let strip_height = max_raw_strip_height(width);
        let mut tile_y = 0u16;
        while tile_y < height {
            let th = strip_height.min(height - tile_y);
            push_bitmap_rect(
                bitmap,
                row_bytes,
                0,
                tile_y,
                width,
                th,
                policy,
                &mut rectangles,
                &mut stats,
            );
            tile_y += th;
        }
    }

    let max_rects = policy.max_rects_per_update.min(rectangles.len().max(1));
    let mut batches = Vec::new();
    for chunk in rectangles.chunks(max_rects) {
        let wire = encode_rectangles_to_wire_frames(chunk, policy.max_request_size);
        stats.encoded_bytes += chunk.iter().map(|r| r.data.len()).sum::<usize>();
        stats.update_batches += 1;
        batches.push(wire);
    }
    (batches, stats)
}

#[allow(clippy::too_many_arguments)]
fn push_bitmap_rect(
    bitmap: &BitmapUpdate,
    row_bytes: usize,
    tile_x: u16,
    tile_y: u16,
    tile_width: u16,
    tile_height: u16,
    policy: &BitmapEncodePolicy,
    rectangles: &mut Vec<fastpath::BitmapRect>,
    stats: &mut BitmapWireStats,
) {
    let tile_row_bytes = usize::from(tile_width) * 4;

    let mut tile_data = Vec::with_capacity(tile_row_bytes * usize::from(tile_height));
    for row in (0..tile_height).rev() {
        let src_row = usize::from(tile_y + row);
        let src_start = src_row * row_bytes + usize::from(tile_x) * 4;
        tile_data.extend_from_slice(&bitmap.data[src_start..src_start + tile_row_bytes]);
    }

    let planar_ok = policy.use_rdp6_planar && tile_width.is_multiple_of(4);
    let (data, compressed_scan_width) = if planar_ok {
        let compressed = rdpcore_pdu::rdp6::encode(
            &tile_data,
            usize::from(tile_width),
            usize::from(tile_height),
        );
        if compressed.len() < tile_data.len() {
            // Bytes, not pixels — see BitmapRect docs (MS-RDPBCGR vs mstsc).
            (compressed, Some(tile_width * 4))
        } else {
            (tile_data, None)
        }
    } else {
        (tile_data, None)
    };

    stats.tiles += 1;
    if compressed_scan_width.is_some() {
        stats.compressed_tiles += 1;
    } else {
        stats.raw_tiles += 1;
    }

    rectangles.push(fastpath::BitmapRect {
        dest_left: bitmap.x + tile_x,
        dest_top: bitmap.y + tile_y,
        dest_right: bitmap.x + tile_x + tile_width - 1,
        dest_bottom: bitmap.y + tile_y + tile_height - 1,
        width: tile_width,
        height: tile_height,
        bits_per_pixel: 32,
        data,
        compressed_scan_width,
    });
}

fn encode_nscodec_update(
    bitmap: &BitmapUpdate,
    codec_id: u8,
    color_loss_level: u8,
    max_request_size: usize,
) -> (Vec<Vec<Vec<u8>>>, BitmapWireStats) {
    let data = rdpcore_pdu::nscodec::encode(
        &bitmap.data,
        bitmap.width.get(),
        bitmap.height.get(),
        bitmap.stride.get(),
        color_loss_level,
    );
    let body = rdpcore_pdu::surface_commands::encode_set_surface_bits(
        bitmap.x,
        bitmap.y,
        bitmap.width.get(),
        bitmap.height.get(),
        codec_id,
        &data,
    );
    let wire = encode_update_to_wire_frames(UPDATE_CODE_SURFACE_COMMANDS, &body, max_request_size);
    let stats = BitmapWireStats {
        tiles: 1,
        compressed_tiles: 1,
        raw_tiles: 0,
        encoded_bytes: data.len(),
        update_batches: 1,
    };
    (vec![wire], stats)
}

fn encode_update_to_wire_frames(
    update_code: u8,
    body: &[u8],
    max_request_size: usize,
) -> Vec<Vec<u8>> {
    // Cap per-fragment payload so reassembly cannot exceed MaxRequestSize.
    let chunk = fastpath::MAX_FASTPATH_CHUNK_SIZE.min(max_request_size.max(1));
    let chunks: Vec<&[u8]> = body.chunks(chunk).collect();
    let count = chunks.len().max(1);
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let fragmentation = if count == 1 {
                fastpath::Fragmentation::Single
            } else if i == 0 {
                fastpath::Fragmentation::First
            } else if i == count - 1 {
                fastpath::Fragmentation::Last
            } else {
                fastpath::Fragmentation::Next
            };
            fastpath::FastPathOutput {
                updates: vec![fastpath::FastPathUpdatePdu {
                    update_code,
                    fragmentation,
                    data: chunk.to_vec(),
                }],
            }
            .encode()
        })
        .collect()
}

fn encode_rectangles_to_wire_frames(
    rectangles: &[fastpath::BitmapRect],
    max_request_size: usize,
) -> Vec<Vec<u8>> {
    let bitmap_bytes = fastpath::BitmapUpdateData {
        rectangles: rectangles.to_vec(),
    }
    .encode();
    encode_update_to_wire_frames(UPDATE_CODE_BITMAP, &bitmap_bytes, max_request_size)
}

fn covers_desktop(bitmap: &BitmapUpdate, width: u16, height: u16) -> bool {
    bitmap.x == 0 && bitmap.y == 0 && bitmap.width.get() == width && bitmap.height.get() == height
}

/// Keep the best frame seen while a resize handshake is in flight.
/// Prefer a full-desktop bitmap (mstsc's canvas is blank after Deactivate-All);
/// only replace an existing full frame with a newer full frame.
fn retain_bitmap_during_resize(
    pending: &mut Option<BitmapUpdate>,
    bitmap: BitmapUpdate,
    desktop_width: u16,
    desktop_height: u16,
) {
    let incoming_full = covers_desktop(&bitmap, desktop_width, desktop_height);
    let have_full = pending
        .as_ref()
        .is_some_and(|p| covers_desktop(p, desktop_width, desktop_height));
    if incoming_full || !have_full {
        *pending = Some(bitmap);
    }
}

#[cfg(test)]
mod tests {
    use super::{covers_desktop, retain_bitmap_during_resize};
    use crate::display::{BitmapUpdate, PixelFormat};
    use core::num::{NonZeroU16, NonZeroUsize};

    use super::{bitmap_encode_policy, encode_bitmap_update, max_raw_strip_height};

    fn bitmap(x: u16, y: u16, width: u16, height: u16, fill: u8) -> BitmapUpdate {
        let w = NonZeroU16::new(width).unwrap();
        let h = NonZeroU16::new(height).unwrap();
        let stride = NonZeroUsize::new(usize::from(width) * 4).unwrap();
        BitmapUpdate {
            x,
            y,
            width: w,
            height: h,
            format: PixelFormat::BgrX32,
            data: std::sync::Arc::from(vec![fill; stride.get() * usize::from(height)]),
            stride,
        }
    }

    #[test]
    fn covers_desktop_requires_origin_and_exact_size() {
        assert!(covers_desktop(&bitmap(0, 0, 100, 50, 1), 100, 50));
        assert!(!covers_desktop(&bitmap(1, 0, 100, 50, 1), 100, 50));
        assert!(!covers_desktop(&bitmap(0, 0, 64, 50, 1), 100, 50));
    }

    #[test]
    fn resize_pending_prefers_full_frame_over_later_tile() {
        let mut pending = None;
        retain_bitmap_during_resize(&mut pending, bitmap(0, 0, 100, 50, 1), 100, 50);
        retain_bitmap_during_resize(&mut pending, bitmap(0, 0, 64, 64, 2), 100, 50);
        let kept = pending.unwrap();
        assert!(covers_desktop(&kept, 100, 50));
        assert_eq!(kept.data[0], 1);
    }

    #[test]
    fn resize_pending_upgrades_tile_to_full_frame() {
        let mut pending = None;
        retain_bitmap_during_resize(&mut pending, bitmap(0, 0, 64, 64, 2), 100, 50);
        retain_bitmap_during_resize(&mut pending, bitmap(0, 0, 100, 50, 3), 100, 50);
        let kept = pending.unwrap();
        assert!(covers_desktop(&kept, 100, 50));
        assert_eq!(kept.data[0], 3);
    }

    #[test]
    fn raw_strip_height_fits_bitmap_length_field() {
        assert_eq!(max_raw_strip_height(1920), 8);
        assert!(1920usize * 4 * usize::from(max_raw_strip_height(1920)) <= 65535usize);
    }

    #[test]
    fn mac_compat_full_frame_uses_few_strip_tiles() {
        let policy = bitmap_encode_policy("m1-mac-mini", None, 8 * 1024 * 1024);
        assert!(!policy.use_rdp6_planar);
        assert_eq!(policy.max_rects_per_update, 32);

        let frame = bitmap(0, 0, 1920, 1200, 0);
        let (_wire, stats) = encode_bitmap_update(&frame, &policy);
        assert_eq!(stats.tiles, 150); // 1200 / 8 scanline strips
        assert_eq!(stats.update_batches, 5); // ceil(150 / 32)
        assert_eq!(stats.raw_tiles, 150);
    }
}
