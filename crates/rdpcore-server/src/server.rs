use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use rdpcore_cliprdr::{CliprdrBackendFactory, CliprdrChannel};
use rdpcore_connector::{AcceptedConnection, Acceptor, AcceptorEvent, ConnectorError};
use rdpcore_dvc::DvcMux;
use rdpcore_pdu::capability_sets::NsCodecNegotiated;
use rdpcore_pdu::fastpath::{
    self, FastPathInputEvent, UPDATE_CODE_BITMAP, UPDATE_CODE_SURFACE_COMMANDS, keyboard_flags,
};
use rdpcore_rdpdr::{DriveConsumerFactory, RdpdrChannel};
use rdpcore_rdpeai::{AudioInputBackendFactory, AudioInputHandler};
use rdpcore_rdpsnd::{RdpsndChannel, RdpsndServerMessage, SoundServerFactory};
use rdpcore_transport::{ChannelKey, ConnectionWriter, Frame, FrameSender, Priority};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use crate::credentials::{CredentialValidator, Credentials};
use crate::credssp;
use crate::display::{BitmapUpdate, DesktopSize, DisplayUpdate, RdpServerDisplay};
use crate::input::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
use crate::transport::{SteadyStateFrame, read_steady_state_frame, read_tpkt_frame};

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
            tokio::spawn(async move {
                if let Err(e) = session.handle_connection(tcp).await {
                    eprintln!("connection from {peer} ended: {e}");
                }
            });
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
            eprintln!("rdp[{peer}]: cleartext negotiation PDU error: {e}");
            e
        })?;
        tcp.write_all(&result.response).await?;
        tcp.flush().await?;
        match result.event {
            AcceptorEvent::TlsUpgrade => {
                if acceptor.requires_credssp() {
                    eprintln!("rdp[{peer}]: negotiation ok (NLA/HYBRID), starting TLS");
                } else {
                    eprintln!("rdp[{peer}]: negotiation ok (TLS), starting TLS");
                }
            }
            AcceptorEvent::Rejected => {
                eprintln!(
                    "rdp[{peer}]: rejected at negotiation - client offered neither \
                     PROTOCOL_HYBRID nor PROTOCOL_SSL"
                );
                return Ok(());
            }
            other => anyhow::bail!("unexpected acceptor event before TLS upgrade: {other:?}"),
        }

        let mut tls = match self.tls.accept(tcp).await {
            Ok(stream) => {
                eprintln!("rdp[{peer}]: TLS established");
                stream
            }
            Err(e) => {
                eprintln!("rdp[{peer}]: TLS handshake failed: {e}");
                return Err(e.into());
            }
        };

        // NLA: CredSSP runs on the TLS stream before MCS Connect Initial.
        let mut nla_authenticated = false;
        if acceptor.requires_credssp() {
            let Some(credentials) = self.nla_credentials.clone() else {
                eprintln!(
                    "rdp[{peer}]: client requested NLA but server has no NLA credentials configured"
                );
                return Ok(());
            };
            if self.tls_public_key.is_empty() {
                eprintln!("rdp[{peer}]: client requested NLA but server TLS public key is missing");
                return Ok(());
            }
            eprintln!("rdp[{peer}]: starting CredSSP (NTLMv2)");
            match credssp::run_credssp_nla(
                &mut tls,
                self.tls_public_key.clone(),
                credentials,
                "kmsrdp",
            )
            .await
            {
                Ok(user) => {
                    eprintln!("rdp[{peer}]: CredSSP succeeded for user {user:?}");
                    nla_authenticated = true;
                }
                Err(e) => {
                    eprintln!("rdp[{peer}]: CredSSP failed: {e}");
                    return Ok(());
                }
            }
        }

        let accepted = loop {
            let frame = read_tpkt_frame(&mut tls).await.map_err(|e| {
                eprintln!(
                    "rdp[{peer}]: read failed during handshake (waiting for {}): {e}",
                    acceptor.handshake_phase()
                );
                e
            })?;
            if frame.first() != Some(&0x03) {
                eprintln!(
                    "rdp[{peer}]: first byte during RDP handshake is 0x{:02x}, not TPKT 0x03",
                    frame.first().copied().unwrap_or(0)
                );
            }
            let result = acceptor.step(&frame).map_err(|e| {
                eprintln!(
                    "rdp[{peer}]: handshake PDU error while waiting for {}: {e}",
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
                    eprintln!(
                        "rdp[{peer}]: client info user={:?} domain={:?} (nla={nla_authenticated})",
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
                        eprintln!(
                            "rdp[{peer}]: rejecting invalid credentials for user {:?} domain {:?} ({password_hint})",
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
                    eprintln!("rdp[{peer}]: credentials accepted, sent Demand Active");
                }
                AcceptorEvent::Accepted(accepted) => {
                    if !result.response.is_empty() {
                        tls.write_all(&result.response).await?;
                        tls.flush().await?;
                    }
                    eprintln!("rdp[{peer}]: handshake complete");
                    break accepted;
                }
                AcceptorEvent::Rejected => {
                    eprintln!("rdp[{peer}]: rejected during handshake");
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
            (Some(channel_id), None) => None,
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
            (Some(channel_id), None) => None,
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
                                println!(
                                    "DVC echo smoke test: OK, payload round-tripped correctly"
                                );
                            } else {
                                eprintln!(
                                    "DVC echo smoke test: FAILED, echoed payload did not match"
                                );
                            }
                        },
                    )));
                eprintln!(
                    "rdp[{peer}]: DVC echo smoke test: queued {} follow-up frame(s)",
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
            (Some(channel_id), None) => None,
            _ => None,
        };

        let client_label = trim_client_name(&accepted.client_name);
        let bitmap_policy = bitmap_encode_policy(client_label, accepted.nscodec);
        let defer_ms = initial_bitmap_defer_ms(client_label);
        let mut bitmap_gate_open = defer_ms == 0;
        let mut bitmap_gate = Box::pin(tokio::time::sleep(std::time::Duration::from_millis(
            defer_ms,
        )));
        let mut deferred_bitmap: Option<BitmapUpdate> = None;

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

        loop {
            tokio::select! {
                _ = &mut bitmap_gate, if !bitmap_gate_open => {
                    bitmap_gate_open = true;
                    if let Some(bitmap) = deferred_bitmap.take()
                        && send_outbound_bitmap(&bitmap, &frame_sender, &bitmap_policy)
                            .await
                            .is_err()
                    {
                        return Ok(());
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
                                Err(e) => eprintln!("dropping malformed fast-path input frame: {e}"),
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
                                )
                                .await
                                .is_err()
                                {
                                    return Ok(());
                                }
                                if let Err(e) = handle_slow_path_frame(
                                    &bytes,
                                    rdpsnd.as_mut(),
                                    cliprdr.as_mut(),
                                    dvc.as_mut(),
                                    rdpdr.as_mut(),
                                    &frame_sender,
                                ) {
                                    eprintln!("dropping malformed slow-path frame after resize: {e}");
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
                                        )
                                        .await
                                        .is_err()
                                        {
                                            return Ok(());
                                        }
                                        if let Err(err) = handle_slow_path_frame(
                                            &bytes,
                                            rdpsnd.as_mut(),
                                            cliprdr.as_mut(),
                                            dvc.as_mut(),
                                            rdpdr.as_mut(),
                                            &frame_sender,
                                        ) {
                                            eprintln!(
                                                "dropping malformed slow-path frame after resize: {err}"
                                            );
                                        }
                                    } else {
                                        eprintln!("dropping malformed frame during resize: {e}");
                                    }
                                }
                            }
                        }
                        Ok(SteadyStateFrame::SlowPath(bytes)) => {
                            if let Err(e) =
                                handle_slow_path_frame(&bytes, rdpsnd.as_mut(), cliprdr.as_mut(), dvc.as_mut(), rdpdr.as_mut(), &frame_sender)
                            {
                                eprintln!("dropping malformed slow-path frame: {e}");
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
                        Ok(Some(DisplayUpdate::Bitmap(bitmap))) => {
                            if send_outbound_bitmap(
                                &bitmap,
                                &frame_sender,
                                &bitmap_policy,
                            )
                            .await
                            .is_err()
                            {
                                return Ok(());
                            }
                        }
                        Ok(Some(DisplayUpdate::Resized(size))) if resizing => {
                            eprintln!("dropping resize to {}x{}: a previous resize is still in flight", size.width, size.height);
                        }
                        Ok(Some(DisplayUpdate::Resized(size))) => {
                            match acceptor.begin_resize(size.width, size.height) {
                                Ok(response) => {
                                    resizing = true;
                                    resize_desktop = size;
                                    pending_after_resize = None;
                                    if frame_sender.send(Frame { channel: ChannelKey::Io, priority: Priority::Latency, bytes: response }).is_err() {
                                        return Ok(());
                                    }
                                }
                                Err(e) => eprintln!("failed to start resize to {}x{}: {e}", size.width, size.height),
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
) -> Result<(), ()> {
    let batches = if let Some((codec_id, cll)) = policy.nscodec {
        encode_nscodec_update(bitmap, codec_id, cll).0
    } else {
        encode_bitmap_update(bitmap, policy).0
    };
    for batch in &batches {
        for wire_frame in batch {
            if frame_sender
                .send(Frame {
                    channel: ChannelKey::Io,
                    priority: Priority::Bulk,
                    bytes: wire_frame.clone(),
                })
                .is_err()
            {
                return Err(());
            }
        }
    }
    Ok(())
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

/// Slow-path (TPKT/X.224/MCS) traffic at steady state: today this is
/// rdpsnd control messages (format negotiation replies, WaveConfirm) and
/// cliprdr messages, dispatched by MCS channel ID. Unrecognized channel
/// IDs are silently ignored rather than treated as an error - a real
/// client may have other static channels open that this server doesn't
/// implement.
fn handle_slow_path_frame(
    bytes: &[u8],
    rdpsnd: Option<&mut RdpsndChannel>,
    cliprdr: Option<&mut CliprdrChannel>,
    dvc: Option<&mut DvcMux>,
    rdpdr: Option<&mut RdpdrChannel>,
    frame_sender: &rdpcore_transport::FrameSender,
) -> anyhow::Result<()> {
    let payload = rdpcore_pdu::x224::unwrap_data(bytes)?;
    let send_data = rdpcore_pdu::mcs::SendData::decode_request(payload)?;

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
        FastPathInputEvent::Sync { .. } => {} // lock-key state sync, not acted on
        FastPathInputEvent::Unicode { flags, code } => {
            // `KeyboardEvent::UnicodePressed` is a fire-once event (see its
            // doc comment) - the paired release carries no useful
            // information here and is dropped, same as a real
            // ironrdp-server-backed handler would see.
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
) -> Result<(), ()> {
    let Some(bitmap) = pending.take() else {
        return Ok(());
    };
    send_outbound_bitmap(&bitmap, frame_sender, policy).await
}

#[derive(Debug, Clone, Copy)]
struct BitmapEncodePolicy {
    use_rdp6_planar: bool,
    max_rects_per_update: usize,
    nscodec: Option<(u8, u8)>,
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
) -> BitmapEncodePolicy {
    let compat_mode = client_needs_compat_workarounds(client_name);
    // macOS Windows App: prefer NSCodec SurfaceCommands (IronRDP path). Raw
    // fast-path bitmaps work but are ~9MB/frame; RDP6 planar disconnects.
    let nscodec = if compat_mode {
        nscodec.map(|n| (n.codec_id, n.color_loss_level))
    } else {
        None
    };
    BitmapEncodePolicy {
        use_rdp6_planar: !compat_mode,
        max_rects_per_update: if compat_mode {
            COMPAT_MAX_RECTS_PER_UPDATE
        } else {
            usize::MAX
        },
        nscodec,
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
        let wire = encode_rectangles_to_wire_frames(chunk);
        stats.encoded_bytes += chunk.iter().map(|r| r.data.len()).sum::<usize>();
        stats.update_batches += 1;
        batches.push(wire);
    }
    (batches, stats)
}

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
            (compressed, Some(tile_width))
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
    let wire = encode_update_to_wire_frames(UPDATE_CODE_SURFACE_COMMANDS, &body);
    let stats = BitmapWireStats {
        tiles: 1,
        compressed_tiles: 1,
        raw_tiles: 0,
        encoded_bytes: data.len(),
        update_batches: 1,
    };
    (vec![wire], stats)
}

fn encode_update_to_wire_frames(update_code: u8, body: &[u8]) -> Vec<Vec<u8>> {
    let chunks: Vec<&[u8]> = body
        .chunks(fastpath::MAX_FASTPATH_CHUNK_SIZE)
        .collect();
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

fn encode_rectangles_to_wire_frames(rectangles: &[fastpath::BitmapRect]) -> Vec<Vec<u8>> {
    let bitmap_bytes = fastpath::BitmapUpdateData {
        rectangles: rectangles.to_vec(),
    }
    .encode();
    encode_update_to_wire_frames(UPDATE_CODE_BITMAP, &bitmap_bytes)
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
        let policy = bitmap_encode_policy("m1-mac-mini", None);
        assert!(!policy.use_rdp6_planar);
        assert_eq!(policy.max_rects_per_update, 32);

        let frame = bitmap(0, 0, 1920, 1200, 0);
        let (_wire, stats) = encode_bitmap_update(&frame, &policy);
        assert_eq!(stats.tiles, 150); // 1200 / 8 scanline strips
        assert_eq!(stats.update_batches, 5); // ceil(150 / 32)
        assert_eq!(stats.raw_tiles, 150);
    }
}
