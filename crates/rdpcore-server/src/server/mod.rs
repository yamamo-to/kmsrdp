use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rdpcore_cliprdr::CliprdrBackendFactory;
use rdpcore_connector::{AcceptedConnection, Acceptor};
use rdpcore_rdpdr::DriveConsumerFactory;
use rdpcore_rdpeai::AudioInputBackendFactory;
use rdpcore_rdpsnd::SoundServerFactory;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tracing::Instrument as _;
use tracing::{info_span, warn};

use crate::auth_limit::AuthLimiter;
use crate::credentials::{CredentialValidator, Credentials};
use crate::display::RdpServerDisplay;
use crate::input::RdpServerInputHandler;

mod frame_pump;
mod handshake;
mod input_handler;
mod metrics;
mod session_loop;
mod slow_path;

#[cfg(test)]
pub(crate) use frame_pump::gfx_already_sent_frame;
#[cfg(test)]
pub(crate) use input_handler::translate_mouse;
#[cfg(test)]
pub(crate) use session_loop::{AbortOnDrop, ResetInputOnDrop};

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
    pub async fn run(mut self) -> Result<(), crate::error::ServerError> {
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
                    warn!(error = %e, "accept() failed, continuing to listen");
                    continue;
                }
            };
            if let Err(e) = tcp.set_nodelay(true) {
                warn!(error = %e, "failed to set TCP_NODELAY");
            }
            if server.auth_limiter.is_blocked(peer.ip()) {
                warn!(%peer, "dropping connection: too many failed authentication attempts");
                continue;
            }
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
/// TLS handshake, CredSSP, and the MCS/finalization handshake.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

impl Session {
    async fn negotiate(
        &self,
        tcp: TcpStream,
        peer_ip: IpAddr,
        authenticated: &AtomicBool,
    ) -> Result<
        Option<(
            tokio_rustls::server::TlsStream<TcpStream>,
            Acceptor,
            AcceptedConnection,
        )>,
        crate::error::ServerError,
    > {
        let desktop = self.display.size().await;
        let params = handshake::HandshakeParams {
            desktop,
            require_nla: self.require_nla,
            tls_acceptor: &self.tls,
            tls_public_key: &self.tls_public_key,
            nla_credentials: self.nla_credentials.as_ref(),
            credential_validator: self.credential_validator.as_deref(),
            auth_limiter: &self.auth_limiter,
        };
        handshake::negotiate(tcp, peer_ip, params, authenticated).await
    }

    async fn run_steady_state<S>(
        &self,
        peer: SocketAddr,
        stream: S,
        acceptor: Acceptor,
        accepted: AcceptedConnection,
    ) -> Result<(), crate::error::ServerError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let params = session_loop::SteadyStateParams {
            display: Arc::clone(&self.display),
            input: Arc::clone(&self.input),
            sound_factory: self.sound_factory.clone(),
            cliprdr_factory: self.cliprdr_factory.clone(),
            audio_input_factory: self.audio_input_factory.clone(),
            drive_factory: self.drive_factory.clone(),
            #[cfg(feature = "gfx")]
            gfx_enabled: self.gfx_enabled,
            #[cfg(feature = "dvc-echo")]
            echo_smoke_test: self.echo_smoke_test,
        };

        session_loop::run_steady_state(params, peer, stream, acceptor, accepted).await
    }

    async fn handle_connection(
        &self,
        tcp: TcpStream,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<(), crate::error::ServerError> {
        let peer = tcp.peer_addr()?;
        if self.auth_limiter.is_blocked(peer.ip()) {
            warn!(%peer, "dropping connection: too many failed authentication attempts");
            return Ok(());
        }
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
#[path = "../handshake_tests.rs"]
mod handshake_tests;
