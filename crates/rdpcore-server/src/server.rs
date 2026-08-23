use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rdpcore_cliprdr::{CliprdrBackendFactory, CliprdrChannel};
use rdpcore_connector::{AcceptedConnection, Acceptor, AcceptorEvent, ConnectorError};
use rdpcore_dvc::DvcMux;
use rdpcore_pdu::fastpath::{
    self, FastPathInputEvent, UPDATE_CODE_SURFACE_COMMANDS, keyboard_flags,
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
use rdpcore_rdpsnd::{RdpsndChannel, RdpsndServerMessage, SoundServerFactory, wave_channel};
use rdpcore_transport::{ChannelKey, ConnectionWriter, Frame, FrameSender, Priority};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tracing::Instrument as _;
use tracing::{debug, info, info_span, warn};

use crate::auth_limit::AuthLimiter;
use crate::credentials::{CredentialValidator, Credentials};
use crate::credssp;
use crate::display::{BitmapUpdate, DesktopSize, DisplayUpdate, RdpServerDisplay};
use crate::encode::{
    BitmapEncodePolicy, BitmapWireStats, bitmap_encode_policy, client_needs_compat_workarounds,
    encode_bitmap_update, encode_nscodec_update, encode_update_to_wire_frames,
    retain_bitmap_during_resize,
};
use crate::error::{SessionError, finish_session};
use crate::input::{ConnectionScopedInput, KeyboardEvent, MouseEvent, RdpServerInputHandler};
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
    require_nla: bool,
    max_sessions: usize,
    #[cfg(feature = "gfx")]
    gfx_enabled: bool,
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
            require_nla: false,
            max_sessions: 1,
            #[cfg(feature = "gfx")]
            gfx_enabled: false,
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

    /// Whether to require NLA (CredSSP / PROTOCOL_HYBRID), rejecting plain TLS.
    pub fn with_require_nla(mut self, require_nla: bool) -> Self {
        self.require_nla = require_nla;
        self
    }

    /// Maximum number of connections that may be in the authenticated
    /// steady-state at once. Further clients are disconnected after
    /// handshake. Defaults to 1 so concurrent sessions cannot share
    /// input/clipboard/drives.
    pub fn with_max_sessions(mut self, max_sessions: usize) -> Self {
        self.max_sessions = max_sessions.max(1);
        self
    }

    /// Enable MS-RDPEGFX AVC420 when the `gfx` cargo feature is compiled in.
    /// Callers pass a parsed flag (e.g. from `KMSRDP_GFX`); this crate does
    /// not read process environment itself.
    pub fn with_gfx(self, enabled: bool) -> Self {
        #[cfg(feature = "gfx")]
        {
            let mut this = self;
            this.gfx_enabled = enabled;
            this
        }
        #[cfg(not(feature = "gfx"))]
        {
            let _ = enabled;
            self
        }
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
            require_nla: self.require_nla,
            #[cfg(feature = "gfx")]
            gfx_enabled: self.gfx_enabled,
            #[cfg(feature = "dvc-echo")]
            echo_smoke_test: self.echo_smoke_test,
            handshake_permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_HANDSHAKES)),
            session_slots: Arc::new(tokio::sync::Semaphore::new(self.max_sessions)),
            auth_limiter: Arc::new(AuthLimiter::new()),
        }
    }
}

/// Bounds how many connections may be in the (unauthenticated, pre-steady-
/// state) handshake phase at once. Combined with `HANDSHAKE_TIMEOUT` (see
/// `handle_connection`), this caps the resources a flood of slow/idle
/// sockets can tie up before either completing the handshake or timing
/// out - accept() itself is never blocked, connections beyond this limit
/// just queue for a permit.
const MAX_CONCURRENT_HANDSHAKES: usize = 16;

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
    require_nla: bool,
    #[cfg(feature = "gfx")]
    gfx_enabled: bool,
    #[cfg(feature = "dvc-echo")]
    echo_smoke_test: bool,
    handshake_permits: Arc<tokio::sync::Semaphore>,
    session_slots: Arc<tokio::sync::Semaphore>,
    auth_limiter: Arc<AuthLimiter>,
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
    require_nla: bool,
    #[cfg(feature = "gfx")]
    gfx_enabled: bool,
    #[cfg(feature = "dvc-echo")]
    echo_smoke_test: bool,
    session_slots: Arc<tokio::sync::Semaphore>,
    auth_limiter: Arc<AuthLimiter>,
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
            require_nla: self.require_nla,
            #[cfg(feature = "gfx")]
            gfx_enabled: self.gfx_enabled,
            #[cfg(feature = "dvc-echo")]
            echo_smoke_test: self.echo_smoke_test,
            session_slots: Arc::clone(&self.session_slots),
            auth_limiter: Arc::clone(&self.auth_limiter),
        }
    }

    /// Accepts connections and runs each on its own task. Display capture
    /// is shared; by default only one authenticated session may be in
    /// steady-state at a time (`with_max_sessions`).
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
            let (tcp, peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    // A single bad accept() (e.g. EMFILE under fd
                    // pressure, or a client that reset the connection
                    // between select() and accept()) must not take the
                    // whole listener down permanently - log and keep
                    // accepting.
                    warn!(error = %e, "accept() failed, continuing to listen");
                    continue;
                }
            };
            // Without this, Nagle's algorithm can hold a Priority::Latency
            // frame (audio wave chunk, input ack, control PDU) in the
            // kernel send buffer for up to ~40ms waiting for
            // coalescing/ACK - directly undermining the write scheduler's
            // whole purpose of draining latency-sensitive frames first.
            if let Err(e) = tcp.set_nodelay(true) {
                warn!(error = %e, "failed to set TCP_NODELAY");
            }
            if server.auth_limiter.is_blocked(peer.ip()) {
                warn!(%peer, "dropping connection: too many failed authentication attempts");
                continue;
            }
            // Acquired here, before spawning - not inside the connection
            // task - so a flood of TCP connections that never send a byte
            // backpressures accept() itself once MAX_CONCURRENT_HANDSHAKES
            // is in flight, instead of piling up unboundedly as spawned
            // tasks and open fds all waiting on a permit. Locked-out
            // addresses (above) never take a permit.
            let Ok(permit) = Arc::clone(&server.handshake_permits).acquire_owned().await else {
                unreachable!("handshake_permits semaphore is never closed");
            };
            let session = server.session();
            let conn_id = NEXT_CONN_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tokio::spawn(
                async move {
                    if let Err(e) = session.handle_connection(tcp, permit).await {
                        warn!(error = %e, "connection ended");
                    }
                }
                .instrument(info_span!("rdp", conn_id, %peer)),
            );
        }
    }
}

/// Bounds everything before the steady-state loop: cleartext negotiation,
/// TLS handshake, CredSSP, and the MCS/finalization handshake. None of
/// that involves waiting on the user, so a client that opens a socket and
/// then trickles bytes (or sends none at all) would otherwise tie up a
/// connection task - and its slot in the per-connection resources below -
/// indefinitely. Once `run_steady_state` starts, no timeout applies: an
/// idle-but-connected client is expected and fine.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

impl Session {
    async fn handle_connection(
        &self,
        tcp: TcpStream,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> anyhow::Result<()> {
        let peer = tcp.peer_addr()?;
        if self.auth_limiter.is_blocked(peer.ip()) {
            warn!(%peer, "dropping connection: too many failed authentication attempts");
            return Ok(());
        }
        // Held only for the handshake itself; acquired by the accept loop
        // before this task was even spawned (see `run`).
        let authenticated = AtomicBool::new(false);
        let negotiated = match tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            self.negotiate(tcp, peer.ip(), &authenticated),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                warn!("handshake did not complete within {HANDSHAKE_TIMEOUT:?}");
                if !authenticated.load(Ordering::Relaxed) {
                    self.auth_limiter.record_failure(peer.ip());
                }
                return Ok(());
            }
        };
        drop(permit);
        let Some((tls, acceptor, accepted)) = negotiated else {
            return Ok(());
        };
        let Ok(_session_slot) = Arc::clone(&self.session_slots).try_acquire_owned() else {
            warn!(
                %peer,
                "rejecting connection: maximum concurrent sessions already connected"
            );
            return Ok(());
        };
        self.run_steady_state(peer, tls, acceptor, accepted).await
    }

    /// Runs cleartext negotiation through MCS finalization. `Ok(None)`
    /// means the connection ended for an expected reason (rejected
    /// negotiation, failed auth, ...) and the caller should just return
    /// `Ok(())`; errors are unexpected I/O/protocol failures.
    async fn negotiate(
        &self,
        mut tcp: TcpStream,
        peer_ip: IpAddr,
        authenticated: &AtomicBool,
    ) -> anyhow::Result<
        Option<(
            tokio_rustls::server::TlsStream<TcpStream>,
            Acceptor,
            AcceptedConnection,
        )>,
    > {
        let desktop = self.display.size().await;
        let mut acceptor =
            Acceptor::new(desktop.width, desktop.height).with_require_nla(self.require_nla);

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
                return Ok(None);
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
        let mut nla_authenticated_user: Option<String> = None;
        if acceptor.requires_credssp() {
            let Some(credentials) = self.nla_credentials.clone() else {
                info!("client requested NLA but server has no NLA credentials configured");
                return Ok(None);
            };
            if self.tls_public_key.is_empty() {
                warn!("client requested NLA but server TLS public key is missing");
                return Ok(None);
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
                    nla_authenticated_user = Some(user);
                }
                Err(e) => {
                    warn!("CredSSP failed: {e}");
                    self.auth_limiter.record_failure(peer_ip);
                    return Ok(None);
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
                        "client info user={:?} domain={:?} (nla={})",
                        credentials.username,
                        credentials.domain,
                        nla_authenticated_user.is_some()
                    );
                    let valid = if let Some(nla_user) = &nla_authenticated_user {
                        // CredSSP already proved the account. Bind the
                        // session to that identity: empty ClientInfo
                        // usernames (mstsc after NLA) are filled in as the
                        // NLA account; a non-empty mismatch is rejected.
                        crate::credentials::client_info_is_authorized(
                            Some(nla_user),
                            self.credential_validator.as_deref(),
                            &credentials.username,
                            &credentials.password,
                            &credentials.domain,
                        )
                    } else {
                        crate::credentials::client_info_is_authorized(
                            None,
                            self.credential_validator.as_deref(),
                            &credentials.username,
                            &credentials.password,
                            &credentials.domain,
                        )
                    };
                    if !valid {
                        let password_hint = if credentials.password.is_empty() {
                            "password empty (client did not send one — enter the password in the client, or enable NLA)"
                        } else {
                            "password non-empty but does not match"
                        };
                        warn!(
                            "rejecting invalid credentials for user {:?} domain {:?} ({password_hint})",
                            credentials.username, credentials.domain
                        );
                        self.auth_limiter.record_failure(peer_ip);
                        acceptor.reject_client_info();
                        return Ok(None);
                    }
                    self.auth_limiter.record_success(peer_ip);
                    authenticated.store(true, Ordering::Relaxed);
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
                    return Ok(None);
                }
            }
        };

        Ok(Some((tls, acceptor, accepted)))
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
        // Per-connection wrapper so reset() releases only this session's
        // holds. Guarantees reset runs on every exit path.
        let connection_input: Arc<Mutex<dyn RdpServerInputHandler>> = Arc::new(Mutex::new(
            ConnectionScopedInput::new(Arc::clone(&self.input)),
        ));
        let _reset_input_on_drop = ResetInputOnDrop(Arc::clone(&connection_input));

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
        let rdpsnd = match (rdpsnd_channel_id, &self.sound_factory) {
            (Some(channel_id), Some(factory)) => {
                let (tx, rx) = wave_channel();
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
                Some(Arc::new(tokio::sync::Mutex::new(channel)))
            }
            (Some(_channel_id), None) => None,
            _ => None,
        };

        // Wave chunks are pumped by a dedicated task rather than the
        // steady-state loop below: GFX/bitmap encode there is necessarily
        // synchronous (see `try_encode_gfx_frame`'s doc comment), and a
        // select!-branch-only audio path would stall for the full encode
        // duration every time one runs. Locking `rdpsnd` from here never
        // contends with the loop below - the loop only ever touches it for
        // `on_channel_data`, briefly and never across an await.
        let _audio_task = match (rdpsnd.clone(), rdpsnd_audio_rx.take()) {
            (Some(channel), Some(mut audio_rx)) => {
                let sender = frame_sender.clone();
                Some(AbortOnDrop(tokio::spawn(async move {
                    while let Some(RdpsndServerMessage::Wave(pcm, timestamp_ms)) =
                        audio_rx.recv().await
                    {
                        let mut channel = channel.lock().await;
                        // Catch rather than let a panic here silently kill
                        // this task - nothing awaits its JoinHandle (only
                        // AbortOnDrop's Drop, which just aborts), so audio
                        // would otherwise stop dead with no log line at all.
                        if let Err(panic) =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                send_wave_frames(&mut channel, &sender, pcm, timestamp_ms);
                            }))
                        {
                            warn!("rdpsnd: audio task panicked in send_wave_frames: {panic:?}");
                        }
                    }
                    debug!("rdpsnd: audio task ending (wave sender dropped)");
                })))
            }
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
        let gfx_session = if self.gfx_enabled {
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
            info!("GFX disabled; using Planar/NSCodec");
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
        let defer_ms = initial_bitmap_defer_ms(client_label, bitmap_policy.nscodec.is_some());
        let mut metrics = SessionBitmapMetrics::default();
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

        // Ensure client cursor is synchronized to default pointer on initial connect.
        let default_ptr = rdpcore_pdu::pointer::encode_ptr_default();
        let _ = frame_sender.send(Frame {
            channel: ChannelKey::Io,
            priority: Priority::Latency,
            bytes: default_ptr,
        });

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
                biased;
                frame = read_steady_state_frame(&mut read_half) => {
                    match frame {
                        Err(e) => return Err(e.into()),
                        Ok(SteadyStateFrame::FastPathInput(bytes)) => {
                            match fastpath::FastPathInput::decode(&bytes) {
                                Ok(input_pdu) => {
                                    let mut input =
                                        connection_input.lock().unwrap_or_else(|e| e.into_inner());
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
                                if let Err(e) = flush_pending_resize_bitmap(
                                    &mut pending_after_resize,
                                    &frame_sender,
                                    &bitmap_policy,
                                    &mut frame_id,
                                    display_updates_allowed,
                                    &mut metrics,
                                )
                                .await
                                {
                                    return finish_session(Err(e));
                                }
                                if let Err(e) = handle_slow_path_frame(
                                    &bytes,
                                    io_channel_id,
                                    &mut display_updates_allowed,
                                    updates.as_mut(),
                                    &rdpsnd,
                                    cliprdr.as_mut(),
                                    dvc.as_mut(),
                                    rdpdr.as_mut(),
                                    &frame_sender,
                                    &bitmap_policy,
                                    &mut frame_id,
                                    &mut metrics,
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
                                        return finish_session(Err(SessionError::WriterClosed));
                                    }
                                    if acceptor.is_finished()
                                        || matches!(result.event, AcceptorEvent::Accepted(_))
                                    {
                                        resizing = false;
                                        if let Err(e) = flush_pending_resize_bitmap(
                                            &mut pending_after_resize,
                                            &frame_sender,
                                            &bitmap_policy,
                                            &mut frame_id,
                                            display_updates_allowed,
                                            &mut metrics,
                                        )
                                        .await
                                        {
                                            return finish_session(Err(e));
                                        }
                                    }
                                }
                                Err(e) => {
                                    if acceptor.is_finished()
                                        || matches!(e, ConnectorError::AlreadyFinished)
                                    {
                                        resizing = false;
                                        if let Err(e) = flush_pending_resize_bitmap(
                                            &mut pending_after_resize,
                                            &frame_sender,
                                            &bitmap_policy,
                                            &mut frame_id,
                                            display_updates_allowed,
                                            &mut metrics,
                                        )
                                        .await
                                        {
                                            return finish_session(Err(e));
                                        }
                                        if let Err(err) = handle_slow_path_frame(
                                            &bytes,
                                            io_channel_id,
                                            &mut display_updates_allowed,
                                            updates.as_mut(),
                                            &rdpsnd,
                                            cliprdr.as_mut(),
                                            dvc.as_mut(),
                                            rdpdr.as_mut(),
                                            &frame_sender,
                                            &bitmap_policy,
                                            &mut frame_id,
                                            &mut metrics,
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
                                &rdpsnd,
                                cliprdr.as_mut(),
                                dvc.as_mut(),
                                rdpdr.as_mut(),
                                &frame_sender,
                                &bitmap_policy,
                                &mut frame_id,
                                &mut metrics,
                            )
                            .await
                            {
                                debug!("dropping malformed slow-path frame: {e}");
                            }
                        }
                    }
                }
                _ = &mut bitmap_gate, if !bitmap_gate_open => {
                    bitmap_gate_open = true;
                    if display_updates_allowed
                        && let Some(bitmap) = deferred_bitmap.take()
                    {
                        let full = updates.latest_full_frame();
                        #[cfg(feature = "gfx")]
                        let gfx_attempt = Some(
                            match try_encode_gfx_frame(
                                gfx_session.as_ref(),
                                &mut last_gfx_data,
                                full.as_ref(),
                                &bitmap,
                            )
                            .await
                            {
                                Ok(outcome) => {
                                    apply_gfx_encode_outcome(outcome, dvc.as_ref(), &frame_sender)
                                }
                                Err(e) => Err(e),
                            },
                        );
                        if let Err(e) = send_outbound_frame(
                            &bitmap,
                            &frame_sender,
                            &bitmap_policy,
                            &mut frame_id,
                            full.as_ref(),
                            &mut metrics,
                            #[cfg(feature = "gfx")]
                            gfx_attempt,
                        )
                        .await
                        {
                            return finish_session(Err(e));
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
                            let gfx_attempt = Some(
                                match try_encode_gfx_frame(
                                    gfx_session.as_ref(),
                                    &mut last_gfx_data,
                                    full.as_ref(),
                                    &bitmap,
                                )
                                .await
                                {
                                    Ok(outcome) => apply_gfx_encode_outcome(
                                        outcome,
                                        dvc.as_ref(),
                                        &frame_sender,
                                    ),
                                    Err(e) => Err(e),
                                },
                            );
                            if let Err(e) = send_outbound_frame(
                                &bitmap,
                                &frame_sender,
                                &bitmap_policy,
                                &mut frame_id,
                                full.as_ref(),
                                &mut metrics,
                                #[cfg(feature = "gfx")]
                                gfx_attempt,
                            )
                            .await
                            {
                                return finish_session(Err(e));
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
                                        return finish_session(Err(SessionError::WriterClosed));
                                    }
                                }
                                Err(e) => warn!("failed to start resize to {}x{}: {e}", size.width, size.height),
                            }
                        }
                        Ok(None) => {
                            metrics.log("display_ended");
                            return Ok(());
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
                                return finish_session(Err(SessionError::WriterClosed));
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

fn initial_bitmap_defer_ms(client_name: &str, using_nscodec: bool) -> u64 {
    if using_nscodec || client_needs_compat_workarounds(client_name) {
        400
    } else {
        0
    }
}

#[derive(Debug, Default)]
struct SessionBitmapMetrics {
    frames: u64,
    tiles: u64,
    compressed_tiles: u64,
    raw_tiles: u64,
    encoded_bytes: u64,
    update_batches: u64,
}

impl SessionBitmapMetrics {
    fn record(&mut self, stats: BitmapWireStats) {
        self.frames += 1;
        self.tiles += u64::from(stats.tiles);
        self.compressed_tiles += u64::from(stats.compressed_tiles);
        self.raw_tiles += u64::from(stats.raw_tiles);
        self.encoded_bytes += stats.encoded_bytes as u64;
        self.update_batches += u64::from(stats.update_batches);
        if self.frames.is_multiple_of(30) {
            self.log("periodic");
        }
    }

    fn log(&self, reason: &'static str) {
        if self.frames == 0 {
            return;
        }
        info!(
            reason,
            frames = self.frames,
            tiles = self.tiles,
            compressed_tiles = self.compressed_tiles,
            raw_tiles = self.raw_tiles,
            encoded_bytes = self.encoded_bytes,
            update_batches = self.update_batches,
            "session bitmap metrics"
        );
    }
}

async fn send_outbound_bitmap(
    bitmap: &BitmapUpdate,
    frame_sender: &FrameSender,
    policy: &BitmapEncodePolicy,
    frame_id: &mut u32,
    metrics: &mut SessionBitmapMetrics,
) -> Result<(), SessionError> {
    // Planar/NSCodec encoding is CPU-bound and was previously run inline on
    // this connection's steady-state select! task, stalling input dispatch
    // and every other channel for the full encode duration on every frame
    // (the same class of bug the RDPSND wave path was pulled off this loop
    // for - see the comment above `_audio_task`). Run it on the blocking
    // pool instead.
    let bitmap = bitmap.clone();
    let policy = *policy;
    let (batches, stats) = tokio::task::spawn_blocking(move || {
        if let Some((codec_id, cll)) = policy.nscodec {
            encode_nscodec_update(&bitmap, codec_id, cll, policy.max_request_size)
        } else {
            encode_bitmap_update(&bitmap, &policy)
        }
    })
    .await
    .map_err(|_| SessionError::EncodeJoin)?;
    metrics.record(stats);

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
            return Err(SessionError::WriterClosed);
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
    metrics: &mut SessionBitmapMetrics,
    #[cfg(feature = "gfx")] gfx_attempt: Option<Result<bool, SessionError>>,
) -> Result<(), SessionError> {
    #[cfg(feature = "gfx")]
    if let Some(result) = gfx_attempt {
        match result {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => return Err(e),
        }
    }
    let _ = latest_full;
    send_outbound_bitmap(bitmap, frame_sender, policy, frame_id, metrics).await
}

/// Outcome of attempting the GFX path for one frame - kept separate from
/// actually sending, so the caller only touches `&DvcMux` after the encode
/// `.await` completes (see `try_encode_gfx_frame`'s doc comment for why).
#[cfg(feature = "gfx")]
enum GfxEncodeOutcome {
    /// No GFX session, or not ready yet: fall back to Planar/NSCodec.
    Fallback,
    /// GFX handled the frame; encode succeeded and the caller should send
    /// these wire payloads (updating `last_gfx_data` as a side effect
    /// already happened before this was returned).
    Send(Vec<Vec<u8>>),
    /// GFX handled the frame with an intentional soft skip (e.g. transient
    /// OpenH264 RC): keep the GFX path so we do not paint Planar over a
    /// black H.264 surface, but there is nothing to send this tick.
    SoftSkip,
    /// GFX is abandoned for this connection: send optional teardown PDUs, then
    /// fall through to Planar/NSCodec without dropping the session.
    Disable { teardown: Vec<Vec<u8>> },
}

/// True when `source` is the same framebuffer Arc already encoded this tick.
#[cfg(any(test, feature = "gfx"))]
fn gfx_already_sent_frame(
    last: &Option<std::sync::Arc<[u8]>>,
    source: &std::sync::Arc<[u8]>,
) -> bool {
    last.as_ref()
        .is_some_and(|prev| std::sync::Arc::ptr_eq(prev, source))
}

/// Runs the GFX H.264 encode for one frame, if applicable.
///
/// The encode is CPU/GPU-bound and runs on the blocking pool (see
/// `send_outbound_bitmap`'s comment for why this moved off the
/// steady-state task). Deliberately takes no `&DvcMux`: that type isn't
/// `Sync`, so a reference to it can't be held across the `.await` below
/// without making the whole per-connection task non-`Send` (and thus
/// unspawnable) - callers apply the result (which does need `&DvcMux`, via
/// [`send_gfx_payloads`]) only after this returns.
#[cfg(feature = "gfx")]
async fn try_encode_gfx_frame(
    gfx: Option<&GfxSession>,
    last_gfx_data: &mut Option<std::sync::Arc<[u8]>>,
    latest_full: Option<&BitmapUpdate>,
    bitmap: &BitmapUpdate,
) -> Result<GfxEncodeOutcome, SessionError> {
    let Some(gfx) = gfx else {
        return Ok(GfxEncodeOutcome::Fallback);
    };
    if !gfx.is_ready() {
        return Ok(GfxEncodeOutcome::Fallback);
    }
    let source = latest_full.unwrap_or(bitmap).clone();
    let source_data = std::sync::Arc::clone(&source.data);
    // Capture allocates a new framebuffer Arc every tick, so pointer
    // equality only dedups the N dirty-rect notifications of one frame
    // (each would otherwise re-encode the whole desktop). A later tick
    // always has a new Arc, so periodic IDR still runs.
    if gfx_already_sent_frame(last_gfx_data, &source_data) {
        return Ok(GfxEncodeOutcome::SoftSkip);
    }
    let gfx_for_encode = gfx.clone();
    let payloads = tokio::task::spawn_blocking(move || {
        gfx_for_encode.encode_frame(
            source.width.get(),
            source.height.get(),
            source.stride.get(),
            source.data.as_ref(),
        )
    })
    .await
    .map_err(|_| SessionError::GfxEncodeJoin)?;
    match payloads {
        rdpcore_rdpegfx::GfxFrameResult::Frames(payloads) => {
            *last_gfx_data = Some(source_data);
            Ok(GfxEncodeOutcome::Send(payloads))
        }
        rdpcore_rdpegfx::GfxFrameResult::Skip => Ok(GfxEncodeOutcome::SoftSkip),
        rdpcore_rdpegfx::GfxFrameResult::Fallback { teardown } => {
            Ok(GfxEncodeOutcome::Disable { teardown })
        }
    }
}

/// Applies a [`GfxEncodeOutcome`] - the only place that still needs
/// `&DvcMux`, called synchronously (no `.await`) after the encode above.
#[cfg(feature = "gfx")]
fn apply_gfx_encode_outcome(
    outcome: GfxEncodeOutcome,
    dvc: Option<&DvcMux>,
    frame_sender: &FrameSender,
) -> Result<bool, SessionError> {
    match outcome {
        GfxEncodeOutcome::Fallback => Ok(false),
        GfxEncodeOutcome::SoftSkip => Ok(true),
        GfxEncodeOutcome::Send(payloads) => {
            let mux = dvc.ok_or(SessionError::GfxChannelMissing)?;
            send_gfx_payloads(mux, frame_sender, payloads)?;
            Ok(true)
        }
        GfxEncodeOutcome::Disable { teardown } => {
            if !teardown.is_empty()
                && let Some(mux) = dvc
            {
                let _ = send_gfx_payloads(mux, frame_sender, teardown);
            }
            Ok(false)
        }
    }
}

#[cfg(feature = "gfx")]
fn send_gfx_payloads(
    mux: &DvcMux,
    frame_sender: &FrameSender,
    payloads: Vec<Vec<u8>>,
) -> Result<(), SessionError> {
    let Some(dyn_id) = mux.channel_id_for_name(rdpcore_rdpegfx::CHANNEL_NAME) else {
        return Err(SessionError::GfxChannelMissing);
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
            return Err(SessionError::WriterClosed);
        }
    }
    Ok(())
}

/// Awaits the next message from an optional channel - never resolves if
/// there isn't one (no rdpsnd negotiated for this connection), which is
/// exactly the right behavior for a `tokio::select!` branch that should
/// simply never fire in that case.
///
/// If the sender side is ever dropped, `rx` is cleared to `None` so later
/// calls take that never-resolving path instead of `UnboundedReceiver::
/// recv` on a closed channel, which returns `Ready(None)` immediately on
/// every poll - left as `Some`, this select! branch would busy-loop the
/// connection's task at 100% CPU instead of parking.
async fn recv_optional<T>(rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match rx {
        Some(r) => {
            let msg = r.recv().await;
            if msg.is_none() {
                *rx = None;
            }
            msg
        }
        None => std::future::pending().await,
    }
}

/// Aborts the wrapped task when dropped - used so the per-connection audio
/// task (which otherwise loops forever on `WaveSubscriber::recv`) always
/// stops when `run_steady_state` returns, on every exit path.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Calls `RdpServerInputHandler::reset` when this connection's steady
/// state ends, on every exit path (clean return, `?` propagation, even a
/// panic unwind) - so a key/button this connection left physically "down"
/// (client disconnected mid-keypress, before the matching `Released`
/// arrived) doesn't stay stuck on a shared input device forever.
struct ResetInputOnDrop(Arc<Mutex<dyn RdpServerInputHandler>>);

impl Drop for ResetInputOnDrop {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).reset();
    }
}

fn send_wave_frames(
    channel: &mut RdpsndChannel,
    frame_sender: &rdpcore_transport::FrameSender,
    pcm: Vec<u8>,
    timestamp_ms: u32,
) {
    let channel_id = channel.channel_id();
    for bytes in channel.encode_wave(pcm, timestamp_ms) {
        let _ = frame_sender.send(Frame {
            channel: ChannelKey::Static(channel_id),
            priority: Priority::Latency,
            bytes,
        });
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
    rdpsnd: &Option<Arc<tokio::sync::Mutex<RdpsndChannel>>>,
    cliprdr: Option<&mut CliprdrChannel>,
    dvc: Option<&mut DvcMux>,
    rdpdr: Option<&mut RdpdrChannel>,
    frame_sender: &rdpcore_transport::FrameSender,
    policy: &BitmapEncodePolicy,
    frame_id: &mut u32,
    metrics: &mut SessionBitmapMetrics,
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
                            let _ = send_outbound_bitmap(
                                &full,
                                frame_sender,
                                policy,
                                frame_id,
                                metrics,
                            )
                            .await;
                        }
                    }
                }
                ShareDataPduType::RefreshRect => {
                    if let Ok(rects) = decode_refresh_rect(&data_pdu.body)
                        && let Some(full) = updates.latest_full_frame()
                    {
                        if rects.is_empty() {
                            let _ = send_outbound_bitmap(
                                &full,
                                frame_sender,
                                policy,
                                frame_id,
                                metrics,
                            )
                            .await;
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
                                    let _ = send_outbound_bitmap(
                                        &sub,
                                        frame_sender,
                                        policy,
                                        frame_id,
                                        metrics,
                                    )
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

    if let Some(channel) = rdpsnd {
        // Locked only right here, for the single synchronous
        // on_channel_data call - never across the IO-channel branch's
        // send_outbound_bitmap().await above, which would otherwise stall
        // the audio task for the encode's duration (the exact stall
        // `_audio_task`'s own doc comment says this lock is meant to
        // avoid contending with).
        let mut channel = channel.lock().await;
        if send_data.channel_id == channel.channel_id() {
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
    const HORIZONTAL_WHEEL: u16 = 0x0400;
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
    if pointer_flags & HORIZONTAL_WHEEL != 0 {
        let raw = i32::from(pointer_flags & 0xFF);
        let value = if pointer_flags & WHEEL_NEGATIVE != 0 {
            raw - 256
        } else {
            raw
        };
        return MouseEvent::HorizontalScroll { value };
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

async fn flush_pending_resize_bitmap(
    pending: &mut Option<BitmapUpdate>,
    frame_sender: &FrameSender,
    policy: &BitmapEncodePolicy,
    frame_id: &mut u32,
    display_updates_allowed: bool,
    metrics: &mut SessionBitmapMetrics,
) -> Result<(), SessionError> {
    if !display_updates_allowed {
        *pending = None;
        return Ok(());
    }
    let Some(bitmap) = pending.take() else {
        return Ok(());
    };
    send_outbound_bitmap(&bitmap, frame_sender, policy, frame_id, metrics).await
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn abort_on_drop_cancels_the_wrapped_task() {
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        struct SetOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let spy = SetOnDrop(std::sync::Arc::clone(&dropped));
        let guard = super::AbortOnDrop(tokio::spawn(async move {
            let _spy = spy;
            std::future::pending::<()>().await;
        }));

        drop(guard);

        // Cancellation is cooperative on the runtime's side - give it a few
        // scheduling turns to actually drop the aborted task's future.
        for _ in 0..100 {
            if dropped.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "AbortOnDrop must cancel (and drop) the task it wraps"
        );
    }

    #[test]
    fn reset_input_on_drop_calls_reset() {
        use crate::input::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct RecordingHandler {
            reset_calls: usize,
        }
        impl RdpServerInputHandler for RecordingHandler {
            fn keyboard(&mut self, _event: KeyboardEvent) {}
            fn mouse(&mut self, _event: MouseEvent) {}
            fn reset(&mut self) {
                self.reset_calls += 1;
            }
        }

        let handler = Arc::new(Mutex::new(RecordingHandler::default()));
        let dyn_handler: Arc<Mutex<dyn RdpServerInputHandler>> = handler.clone();
        let guard = super::ResetInputOnDrop(dyn_handler);
        assert_eq!(handler.lock().unwrap().reset_calls, 0);
        drop(guard);
        assert_eq!(handler.lock().unwrap().reset_calls, 1);
    }

    #[test]
    fn gfx_already_sent_frame_is_pointer_equality() {
        use std::sync::Arc;

        let frame: Arc<[u8]> = Arc::from(vec![1u8, 2, 3]);
        let same = Arc::clone(&frame);
        let other: Arc<[u8]> = Arc::from(vec![1u8, 2, 3]);

        assert!(!super::gfx_already_sent_frame(&None, &frame));
        assert!(super::gfx_already_sent_frame(&Some(same), &frame));
        assert!(!super::gfx_already_sent_frame(&Some(other), &frame));
    }

    #[test]
    fn translate_mouse_horizontal_wheel() {
        match super::translate_mouse(0x0400 | 120, 0, 0) {
            crate::input::MouseEvent::HorizontalScroll { value } => assert_eq!(value, 120),
            other => panic!("expected HorizontalScroll, got {other:?}"),
        }
        match super::translate_mouse(0x0400 | 0x0100 | 1, 0, 0) {
            crate::input::MouseEvent::HorizontalScroll { value } => assert_eq!(value, -255),
            other => panic!("expected negative HorizontalScroll, got {other:?}"),
        }
    }
}

#[cfg(test)]
#[path = "handshake_tests.rs"]
mod handshake_tests;
