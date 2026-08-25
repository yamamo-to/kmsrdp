//! Sans-io RDP server-side connection-sequence state machine: everything
//! from X.224 Connection Request through Finalization ("Accepted" steady
//! state), tying together the PDU codecs in `rdpcore-pdu`. No sockets, no
//! async, no TLS - the caller (`rdpcore-server`) is responsible for
//! reading/writing bytes and for driving the TLS handshake at the right
//! point (see [`Acceptor::step`]'s docs on [`AcceptorEvent::TlsUpgrade`]).
//!
//! Usage: repeatedly call [`Acceptor::step`] with one already-framed input
//! PDU until it reports [`AcceptorEvent::Accepted`].

mod error;

pub use error::ConnectorError;
pub use rdpcore_pdu::x224::SecurityProtocol;

use rdpcore_pdu::capability_sets::{
    BitmapCapability, BitmapCodecsCapability, ConfirmActive, DeactivateAllPdu, DemandActive,
    GeneralCapability, InputCapability, MultiFragmentUpdateCapability, NsCodecNegotiated,
    OrderCapability, PointerCapability, ServerCapabilities, ShareControlHeader,
    ShareControlPduType, SurfaceCommandsCapability, VirtualChannelCapability,
    parse_client_max_request_size, parse_client_nscodec,
};
use rdpcore_pdu::client_info::ClientInfoPdu;
use rdpcore_pdu::cursor::ReadCursor;
use rdpcore_pdu::finalization::{
    ControlPdu, DataPdu, FontPdu, STREAM_UNDEFINED, ShareDataPduType, SynchronizePdu,
    encode_save_session_info_plain_notify,
};
use rdpcore_pdu::gcc::{
    ClientGccBlocks, ConferenceCreateRequest, ConferenceCreateResponse, ServerCoreData,
    ServerGccBlocks, ServerMessageChannelData, ServerNetworkData, ServerSecurityData,
};
use rdpcore_pdu::licensing;
use rdpcore_pdu::mcs::{
    AttachUserConfirm, AttachUserRequest, BASE_CHANNEL_ID, ChannelJoinConfirm, ChannelJoinRequest,
    ConnectInitial, ConnectResponse, DomainMcsPdu, DomainParameters, ErectDomainRequest, SendData,
};
use rdpcore_pdu::x224::{self, ConnectionConfirm, ConnectionRequest, FailureCode, ResponseFlags};

/// MCS user (initiator) channel id - fixed since this server design is one
/// connection (one `Acceptor`) per TCP/TLS stream, so there's no risk of
/// collision that would require dynamic allocation.
pub const USER_CHANNEL_ID: u16 = BASE_CHANNEL_ID + 1; // 1002
/// Main I/O (graphics + input) channel id.
pub const IO_CHANNEL_ID: u16 = USER_CHANNEL_ID + 1; // 1003

const SHARE_ID: u32 = 0x0001_0000;

/// Maximum allowed size for the reassembled Confirm Active PDU. Without a
/// cap, a client sending fragments with `complete = false` indefinitely
/// could grow `confirm_active_buf` without bound - mirrors the
/// `MAX_TS_REQUEST_LEN` guard `credssp.rs` already applies to CredSSP's
/// own fragment reassembly.
const MAX_CONFIRM_ACTIVE_LEN: usize = 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub struct ClientCredentials {
    pub domain: String,
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for ClientCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCredentials")
            .field("domain", &self.domain)
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

/// Everything the caller needs once the connection reaches steady state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedConnection {
    pub io_channel_id: u16,
    pub user_channel_id: u16,
    /// Static channel name -> MCS channel id, in the order the client
    /// listed them in its Client Network Data.
    pub static_channels: Vec<(String, u16)>,
    pub share_id: u32,
    pub desktop_width: u16,
    pub desktop_height: u16,
    /// CS_CORE `clientName` from the client's Connect Initial GCC block.
    pub client_name: String,
    /// NSCodec negotiated in Confirm Active (macOS Windows App).
    pub nscodec: Option<NsCodecNegotiated>,
    /// Client MultiFragmentUpdate MaxRequestSize (reassembly budget).
    /// `None` if the client omitted the capability; callers should fall back
    /// to the server's advertised value.
    pub max_request_size: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptorEvent {
    /// Nothing notable happened this step - just send `response` (if
    /// non-empty) and keep calling `step`.
    None,
    /// The client's Connection Request didn't include `PROTOCOL_SSL`;
    /// `response` carries an `RDP_NEG_FAILURE` and the caller should close
    /// the connection after writing it (no further `step` calls).
    Rejected,
    /// Send `response` (the Connection Confirm choosing `PROTOCOL_SSL`),
    /// then perform the TLS handshake on the raw socket before reading
    /// anything else, then resume calling `step` with the plaintext bytes
    /// TLS decrypts.
    TlsUpgrade,
    /// Validates the client's username/password/domain, for the caller to
    /// check (e.g. against `KMSRDP_USER`/`KMSRDP_PASSWORD`). Licensing has
    /// already been sent; call [`Acceptor::approve_client_info`] on success
    /// or [`Acceptor::reject_client_info`] on failure before resuming
    /// `step`.
    ClientInfoReceived(ClientCredentials),
    /// The connection sequence is complete; steady-state fast-path
    /// input/output can begin.
    Accepted(AcceptedConnection),
}

pub struct StepResult {
    /// Bytes to write to the (by-then TLS-wrapped, except for the very
    /// first step) stream, verbatim. May be empty.
    pub response: Vec<u8>,
    pub event: AcceptorEvent,
}

impl StepResult {
    fn just(response: Vec<u8>) -> Self {
        Self {
            response,
            event: AcceptorEvent::None,
        }
    }

    fn with_event(response: Vec<u8>, event: AcceptorEvent) -> Self {
        Self { response, event }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct FinalizationProgress {
    granted_control_sent: bool,
}

#[derive(Clone)]
enum State {
    WaitConnectionRequest,
    WaitConnectInitial,
    WaitErectDomainRequest,
    WaitAttachUserRequest,
    WaitChannelJoinRequest { remaining: u32 },
    WaitClientInfo,
    WaitAuthApproval,
    WaitConfirmActive,
    WaitFinalization(FinalizationProgress),
    Accepted,
    Rejected,
}

pub struct Acceptor {
    state: State,
    desktop_width: u16,
    desktop_height: u16,
    static_channel_names: Vec<String>,
    message_channel_id: Option<u16>,
    client_name: String,
    /// Echo of the client's RDP Negotiation Request `requestedProtocols`
    /// (MS-RDPBCGR: SC_CORE.clientRequestedProtocols MUST match this).
    client_requested_protocols: u32,
    /// Protocol selected in the Connection Confirm (`PROTOCOL_HYBRID` or
    /// `PROTOCOL_SSL`). Callers use this after TLS to decide whether CredSSP
    /// (NLA) must run before MCS Connect Initial.
    selected_protocol: SecurityProtocol,
    /// Whether NLA (PROTOCOL_HYBRID) is strictly required.
    require_nla: bool,
    /// MCS user-data reassembly for Confirm Active when mstsc fragments it.
    confirm_active_buf: Vec<u8>,
    nscodec: Option<NsCodecNegotiated>,
    max_request_size: Option<u32>,
}

impl Acceptor {
    pub fn new(desktop_width: u16, desktop_height: u16) -> Self {
        Self {
            state: State::WaitConnectionRequest,
            desktop_width,
            desktop_height,
            static_channel_names: Vec::new(),
            message_channel_id: None,
            client_name: String::new(),
            client_requested_protocols: SecurityProtocol::SSL.0,
            selected_protocol: SecurityProtocol::SSL,
            require_nla: false,
            confirm_active_buf: Vec::new(),
            nscodec: None,
            max_request_size: None,
        }
    }

    /// Enforce NLA (PROTOCOL_HYBRID) and reject non-NLA TLS-only connections.
    pub fn with_require_nla(mut self, require_nla: bool) -> Self {
        self.require_nla = require_nla;
        self
    }

    /// Protocol chosen during X.224 negotiation (`PROTOCOL_HYBRID` when the
    /// client offered NLA, otherwise `PROTOCOL_SSL`).
    pub fn selected_protocol(&self) -> SecurityProtocol {
        self.selected_protocol
    }

    /// Whether the selected protocol requires a CredSSP exchange after TLS
    /// and before MCS Connect Initial.
    pub fn requires_credssp(&self) -> bool {
        self.selected_protocol.contains(SecurityProtocol::HYBRID)
    }

    /// Whether the connection has reached (or been rejected before
    /// reaching) steady state - once true, stop calling `step`.
    pub fn is_finished(&self) -> bool {
        matches!(self.state, State::Accepted | State::Rejected)
    }

    /// Human-readable handshake phase for diagnostics.
    pub fn handshake_phase(&self) -> &'static str {
        match &self.state {
            State::WaitConnectionRequest => "connection-request",
            State::WaitConnectInitial => "connect-initial",
            State::WaitErectDomainRequest => "erect-domain",
            State::WaitAttachUserRequest => "attach-user",
            State::WaitChannelJoinRequest { .. } => "channel-join",
            State::WaitClientInfo => "client-info",
            State::WaitAuthApproval => "auth-approval",
            State::WaitConfirmActive => "confirm-active",
            State::WaitFinalization(_) => "finalization",
            State::Accepted => "accepted",
            State::Rejected => "rejected",
        }
    }

    /// After [`AcceptorEvent::ClientInfoReceived`] and successful credential
    /// validation, sends Demand Active and resumes the connection sequence.
    pub fn approve_client_info(&mut self) -> Result<Vec<u8>, ConnectorError> {
        if !matches!(self.state, State::WaitAuthApproval) {
            return Err(ConnectorError::NotReady);
        }
        self.state = State::WaitConfirmActive;
        Ok(send_io_indication(self.demand_active()))
    }

    /// After failed credential validation: marks the acceptor finished so no
    /// further `step` calls are made.
    pub fn reject_client_info(&mut self) {
        if matches!(self.state, State::WaitAuthApproval) {
            self.state = State::Rejected;
        }
    }

    /// Starts a server-initiated resolution change: sends a Deactivate-All
    /// PDU followed by a new Demand Active advertising `desktop_width`/
    /// `desktop_height`, and reopens the connection sequence at
    /// `WaitConfirmActive` (MS-RDPBCGR's classic "deactivate, then
    /// reactivate with different capabilities" resize mechanism - the only
    /// spec-correct way for the *server* to change the desktop size mid-
    /// session; MS-RDPEDISP's Display Control channel only carries
    /// layout changes in the other direction, client to server).
    ///
    /// Only valid once the connection has reached `Accepted`; the caller
    /// must keep calling [`Acceptor::step`] with the client's subsequent
    /// bytes (Confirm Active, then Synchronize/Control/FontList, exactly
    /// like the initial handshake) until it reports `Accepted` again.
    pub fn begin_resize(
        &mut self,
        desktop_width: u16,
        desktop_height: u16,
    ) -> Result<Vec<u8>, ConnectorError> {
        if !matches!(self.state, State::Accepted) {
            return Err(ConnectorError::NotReady);
        }
        self.desktop_width = desktop_width;
        self.desktop_height = desktop_height;

        let mut response = Vec::new();
        response.extend(x224::wrap_data(
            &SendData {
                initiator: USER_CHANNEL_ID,
                channel_id: IO_CHANNEL_ID,
                data: DeactivateAllPdu {
                    share_id: SHARE_ID,
                    pdu_source: IO_CHANNEL_ID,
                }
                .encode(),
                complete: true,
            }
            .encode_indication(),
        ));
        response.extend(x224::wrap_data(
            &SendData {
                initiator: USER_CHANNEL_ID,
                channel_id: IO_CHANNEL_ID,
                data: self.demand_active(),
                complete: true,
            }
            .encode_indication(),
        ));

        // A previous Confirm Active fragment must not leak into the re-activation.
        self.confirm_active_buf.clear();
        self.state = State::WaitConfirmActive;
        Ok(response)
    }

    pub fn step(&mut self, input: &[u8]) -> Result<StepResult, ConnectorError> {
        if matches!(
            self.state,
            State::WaitErectDomainRequest
                | State::WaitAttachUserRequest
                | State::WaitChannelJoinRequest { .. }
        ) {
            return self.step_mcs_domain_pdus(input);
        }

        // Don't `mem::replace` into Rejected before returning AlreadyFinished:
        // a single stray step after Accepted used to permanently poison the
        // acceptor (Rejected), after which mid-session resize could never
        // complete and the server kept feeding frames into step().
        if matches!(self.state, State::Accepted | State::Rejected) {
            return Err(ConnectorError::AlreadyFinished);
        }

        if matches!(
            self.state,
            State::WaitConfirmActive | State::WaitFinalization(_)
        ) {
            return self.step_reactivation(input);
        }

        let previous = core::mem::replace(&mut self.state, State::Rejected);
        let result = match previous.clone() {
            State::WaitConnectionRequest => self.on_connection_request(input),
            State::WaitConnectInitial => self.on_connect_initial(input),
            State::WaitClientInfo => self.on_client_info(input),
            State::WaitAuthApproval => Err(ConnectorError::NotReady),
            State::WaitConfirmActive
            | State::WaitFinalization(_)
            | State::Accepted
            | State::Rejected
            | State::WaitErectDomainRequest
            | State::WaitAttachUserRequest
            | State::WaitChannelJoinRequest { .. } => {
                unreachable!("handled above")
            }
        };
        // Transient decode errors must not leave the acceptor Rejected mid-resize;
        // restore the phase we were in so the client can still finish the handshake.
        if result.is_err() && matches!(self.state, State::Rejected) {
            self.state = previous;
        }
        result
    }

    /// Drive Confirm Active + finalization, peeling every MCS Send Data
    /// Request packed into one X.224 payload. mstsc often batches Synchronize /
    /// Control / FontList together; processing only the first PDU used to
    /// drop FontList, leave the acceptor unfinished, and then spam
    /// `AlreadyFinished` once a later frame finally completed (or left the
    /// server calling `step` after Accept).
    fn step_reactivation(&mut self, input: &[u8]) -> Result<StepResult, ConnectorError> {
        let payload = x224::unwrap_data(input)?;
        let mut cursor = ReadCursor::new(payload);
        let mut response = Vec::new();
        let mut event = AcceptorEvent::None;

        while cursor.remaining() > 0 {
            if matches!(self.state, State::Accepted | State::Rejected) {
                break;
            }
            let send_data = match SendData::decode_request_from_cursor(&mut cursor) {
                Ok(sd) => sd,
                Err(e) => {
                    // Trailing noise after a successful Accept is harmless.
                    if matches!(event, AcceptorEvent::Accepted(_)) {
                        break;
                    }
                    return Err(ConnectorError::Decode(e));
                }
            };

            let step = match self.state.clone() {
                State::WaitConfirmActive => self.on_confirm_active_send_data(send_data)?,
                State::WaitFinalization(progress) => {
                    self.on_finalization_send_data(send_data, progress)?
                }
                State::Accepted | State::Rejected => break,
                other => {
                    self.state = other;
                    return Err(ConnectorError::NotReady);
                }
            };
            response.extend(step.response);
            if matches!(step.event, AcceptorEvent::Accepted(_)) {
                event = step.event;
                break;
            }
        }

        Ok(StepResult { response, event })
    }

    /// mstsc may pack several MCS domain PDUs (Erect Domain, Attach User,
    /// Channel Join, ...) into a single X.224 Data TPDU; peel and answer
    /// each one before waiting for the next socket read.
    fn step_mcs_domain_pdus(&mut self, input: &[u8]) -> Result<StepResult, ConnectorError> {
        let payload = x224::unwrap_data(input)?;
        let mut cursor = ReadCursor::new(payload);
        let mut response = Vec::new();
        let mut event = AcceptorEvent::None;

        while cursor.remaining() > 0 {
            if let Some(pdu) = Self::peek_domain_pdu(&cursor)
                && pdu == DomainMcsPdu::DisconnectProviderUltimatum
            {
                self.state = State::Rejected;
                return Err(ConnectorError::Decode(
                    rdpcore_pdu::DecodeError::InvalidValue {
                        field: "mcs.domain_pdu",
                        reason: "client disconnected during MCS domain setup",
                    },
                ));
            }

            let result = match core::mem::replace(&mut self.state, State::Rejected) {
                State::WaitErectDomainRequest => self.handle_erect_domain_request(&mut cursor)?,
                State::WaitAttachUserRequest => self.handle_attach_user_request(&mut cursor)?,
                State::WaitChannelJoinRequest { remaining } => {
                    self.handle_channel_join_request(&mut cursor, remaining)?
                }
                other => {
                    self.state = other;
                    return Err(ConnectorError::NotReady);
                }
            };
            response.extend(result.response);
            if !matches!(result.event, AcceptorEvent::None) {
                event = result.event;
                return Ok(StepResult { response, event });
            }
            if matches!(self.state, State::WaitClientInfo) {
                break;
            }
        }

        Ok(StepResult { response, event })
    }

    fn peek_domain_pdu(cursor: &ReadCursor<'_>) -> Option<DomainMcsPdu> {
        if cursor.remaining() == 0 {
            return None;
        }
        let byte = cursor.peek_slice(1).ok()?[0];
        DomainMcsPdu::from_u8(byte >> 2)
    }

    fn on_connection_request(&mut self, input: &[u8]) -> Result<StepResult, ConnectorError> {
        let request = ConnectionRequest::decode(input)?;
        // Prefer NLA (CredSSP / PROTOCOL_HYBRID) when the client offers it;
        // otherwise fall back to TLS-only (PROTOCOL_SSL) + Client Info auth
        // unless require_nla is set.
        let selected = if request.protocol.contains(SecurityProtocol::HYBRID) {
            SecurityProtocol::HYBRID
        } else if !self.require_nla && request.protocol.contains(SecurityProtocol::SSL) {
            SecurityProtocol::SSL
        } else {
            self.state = State::Rejected;
            let code = if self.require_nla {
                FailureCode::HYBRID_REQUIRED_BY_SERVER
            } else {
                FailureCode::SSL_REQUIRED_BY_SERVER
            };
            let response = ConnectionConfirm::Failure { code }.encode();
            return Ok(StepResult::with_event(response, AcceptorEvent::Rejected));
        };

        self.client_requested_protocols = request.protocol.0;
        self.selected_protocol = selected;
        let response = ConnectionConfirm::Response {
            flags: ResponseFlags(0),
            protocol: selected,
        }
        .encode();
        self.state = State::WaitConnectInitial;
        Ok(StepResult::with_event(response, AcceptorEvent::TlsUpgrade))
    }

    fn on_connect_initial(&mut self, input: &[u8]) -> Result<StepResult, ConnectorError> {
        let payload = x224::unwrap_data(input)?;
        let connect_initial = ConnectInitial::decode(payload)?;
        let request = ConferenceCreateRequest::decode(&connect_initial.user_data)?;
        let client_blocks = ClientGccBlocks::decode(&request.client_gcc_blocks)?;
        self.client_name = client_blocks.core.client_name.clone();

        self.static_channel_names = client_blocks
            .network
            .map(|network| network.channels.into_iter().map(|c| c.name).collect())
            .unwrap_or_default();

        let static_channel_ids: Vec<u16> = (0..self.static_channel_names.len())
            .map(|i| IO_CHANNEL_ID + 1 + i as u16)
            .collect();

        let message_channel_id = client_blocks
            .message_channel
            .as_ref()
            .map(|_| IO_CHANNEL_ID + 1 + self.static_channel_names.len() as u16);
        self.message_channel_id = message_channel_id;

        let server_blocks = ServerGccBlocks {
            core: ServerCoreData {
                version: 0x0008_0004,
                client_requested_protocols: Some(self.client_requested_protocols),
                early_capability_flags: None,
            },
            network: ServerNetworkData {
                io_channel_id: IO_CHANNEL_ID,
                channel_ids: static_channel_ids,
            },
            security: ServerSecurityData,
            message_channel: message_channel_id
                .map(|mcs_channel_id| ServerMessageChannelData { mcs_channel_id }),
        };
        let response = ConferenceCreateResponse {
            node_id: USER_CHANNEL_ID,
            server_gcc_blocks: server_blocks.encode(),
        };
        let connect_response = ConnectResponse {
            called_connect_id: 0,
            domain_parameters: DomainParameters::target(),
            user_data: response.encode(),
        };

        self.state = State::WaitErectDomainRequest;
        Ok(StepResult::just(x224::wrap_data(
            &connect_response.encode(),
        )))
    }

    fn handle_erect_domain_request(
        &mut self,
        cursor: &mut ReadCursor<'_>,
    ) -> Result<StepResult, ConnectorError> {
        let _request =
            ErectDomainRequest::decode_from_cursor(cursor).map_err(ConnectorError::from)?;
        self.state = State::WaitAttachUserRequest;
        Ok(StepResult::just(Vec::new()))
    }

    fn handle_attach_user_request(
        &mut self,
        cursor: &mut ReadCursor<'_>,
    ) -> Result<StepResult, ConnectorError> {
        let _request =
            AttachUserRequest::decode_from_cursor(cursor).map_err(ConnectorError::from)?;
        let confirm = AttachUserConfirm {
            result: 0,
            initiator: USER_CHANNEL_ID,
        };
        let mut remaining = 2 + self.static_channel_names.len() as u32;
        if self.message_channel_id.is_some() {
            remaining += 1;
        }
        self.state = State::WaitChannelJoinRequest { remaining };
        Ok(StepResult::just(x224::wrap_data(&confirm.encode())))
    }

    fn handle_channel_join_request(
        &mut self,
        cursor: &mut ReadCursor<'_>,
        remaining: u32,
    ) -> Result<StepResult, ConnectorError> {
        let request =
            ChannelJoinRequest::decode_from_cursor(cursor).map_err(ConnectorError::from)?;
        let confirm = ChannelJoinConfirm {
            result: 0,
            initiator: request.initiator,
            requested_channel_id: request.channel_id,
            channel_id: request.channel_id,
        };
        let remaining = remaining.saturating_sub(1);
        self.state = if remaining == 0 {
            State::WaitClientInfo
        } else {
            State::WaitChannelJoinRequest { remaining }
        };
        Ok(StepResult::just(x224::wrap_data(&confirm.encode())))
    }

    fn on_client_info(&mut self, input: &[u8]) -> Result<StepResult, ConnectorError> {
        let payload = x224::unwrap_data(input)?;
        let send_data = SendData::decode_request(payload)?;
        let client_info = ClientInfoPdu::decode(&send_data.data)?;

        let credentials = ClientCredentials {
            domain: client_info.info.domain,
            username: client_info.info.username,
            password: client_info.info.password,
        };

        let mut response = Vec::new();
        response.extend(x224::wrap_data(
            &SendData {
                initiator: USER_CHANNEL_ID,
                channel_id: IO_CHANNEL_ID,
                data: licensing::encode_valid_client(),
                complete: true,
            }
            .encode_indication(),
        ));

        self.state = State::WaitAuthApproval;
        Ok(StepResult::with_event(
            response,
            AcceptorEvent::ClientInfoReceived(credentials),
        ))
    }

    fn demand_active(&self) -> Vec<u8> {
        let capabilities = ServerCapabilities {
            general: GeneralCapability {
                extra_flags: GeneralCapability::FASTPATH_OUTPUT_SUPPORTED,
                refresh_rect_support: true,
                suppress_output_support: true,
            },
            bitmap: BitmapCapability {
                preferred_bits_per_pixel: 32,
                desktop_width: self.desktop_width,
                desktop_height: self.desktop_height,
                // Tells the client a later Deactivate-All + reactivation
                // may carry different desktop dimensions (see
                // Acceptor::begin_resize) - without this, a well-behaved
                // client has no reason to expect one and may not
                // reallocate/clear its surface on a shrink, leaving stale
                // content in the area outside the new smaller bounds.
                desktop_resize_flag: true,
            },
            order: OrderCapability,
            pointer: PointerCapability {
                color_pointer_cache_size: 2048,
                pointer_cache_size: 2048,
            },
            input: InputCapability {
                // Deliberately not MOUSEX/MOUSE_RELATIVE/QOE_TIMESTAMPS -
                // see rdpcore_pdu::fastpath's module docs on why the input
                // decoder only handles Scancode/Mouse/Sync/Unicode.
                input_flags: InputCapability::SCANCODES
                    | InputCapability::FASTPATH_INPUT
                    | InputCapability::FASTPATH_INPUT_2
                    | InputCapability::UNICODE,
                keyboard_layout: 0,
                keyboard_type: 0,
                keyboard_subtype: 0,
                keyboard_function_key: 0,
            },
            virtual_channel: VirtualChannelCapability { flags: 0 },
            multifragment_update: MultiFragmentUpdateCapability {
                max_request_size: 8 * 1024 * 1024,
            },
            surface_commands: SurfaceCommandsCapability,
            bitmap_codecs: BitmapCodecsCapability::default(),
        };
        DemandActive {
            share_id: SHARE_ID,
            pdu_source: IO_CHANNEL_ID,
            capabilities: &capabilities,
        }
        .encode()
    }

    fn on_confirm_active_send_data(
        &mut self,
        send_data: SendData,
    ) -> Result<StepResult, ConnectorError> {
        // Static-channel traffic (drdynvc caps, etc.) can arrive before
        // Confirm Active; only the I/O channel carries it.
        if send_data.channel_id != IO_CHANNEL_ID {
            self.state = State::WaitConfirmActive;
            return Ok(StepResult::just(Vec::new()));
        }

        // Mid-session resize: mstsc may still emit Share Data (or retransmit
        // finalization) on the I/O channel. Only Confirm Active advances this
        // phase — anything else must be ignored, not appended into the buffer.
        if let Ok((header, _)) = ShareControlHeader::decode(&mut ReadCursor::new(&send_data.data))
            && header.pdu_type != ShareControlPduType::ConfirmActive
        {
            self.state = State::WaitConfirmActive;
            return Ok(StepResult::just(Vec::new()));
        }

        if self
            .confirm_active_buf
            .len()
            .saturating_add(send_data.data.len())
            > MAX_CONFIRM_ACTIVE_LEN
        {
            self.confirm_active_buf.clear();
            return Err(ConnectorError::Decode(
                rdpcore_pdu::DecodeError::InvalidValue {
                    field: "confirm_active.reassembly",
                    reason: "exceeded maximum allowed reassembled size",
                },
            ));
        }
        self.confirm_active_buf.extend_from_slice(&send_data.data);
        if !send_data.complete {
            self.state = State::WaitConfirmActive;
            return Ok(StepResult::just(Vec::new()));
        }

        let data = std::mem::take(&mut self.confirm_active_buf);
        let confirm = try_decode_confirm_active(&data)?;
        self.nscodec = parse_client_nscodec(&confirm.capabilities, 3);
        self.max_request_size = parse_client_max_request_size(&confirm.capabilities);

        // MS-RDPBCGR 1.3.1.1: Server Synchronize is sent in response to
        // Confirm Active; Server Cooperate follows immediately. mstsc waits
        // for these before sending its own finalization PDUs - deferring all
        // server finalization replies until every client PDU has arrived
        // deadlocks with mstsc ("configuring remote session" forever) while
        // batch-oriented clients like xfreerdp happen to work anyway.
        let mut response = Vec::new();
        response.extend(send_io_indication(server_synchronize_pdu()));
        response.extend(send_io_indication(server_cooperate_pdu()));

        self.state = State::WaitFinalization(FinalizationProgress::default());
        Ok(StepResult::just(response))
    }

    fn on_finalization_send_data(
        &mut self,
        send_data: SendData,
        mut progress: FinalizationProgress,
    ) -> Result<StepResult, ConnectorError> {
        // Mid-session resize keeps cliprdr/rdpdr/drdynvc alive. Their PDUs
        // arrive interleaved with Synchronize/Control/FontList; parsing them
        // as Share Data aborts the handshake (Rejected) and leaves the RDP
        // client on a blank canvas after Deactivate-All.
        if send_data.channel_id != IO_CHANNEL_ID {
            self.state = State::WaitFinalization(progress);
            return Ok(StepResult::just(Vec::new()));
        }

        if let Ok((header, _)) = ShareControlHeader::decode(&mut ReadCursor::new(&send_data.data))
            && header.pdu_type != ShareControlPduType::Data
        {
            self.state = State::WaitFinalization(progress);
            return Ok(StepResult::just(Vec::new()));
        }

        let Ok(data_pdu) = DataPdu::decode(&send_data.data) else {
            self.state = State::WaitFinalization(progress);
            return Ok(StepResult::just(Vec::new()));
        };

        let mut response = Vec::new();

        match data_pdu.pdu_type2 {
            ShareDataPduType::Synchronize => {}
            ShareDataPduType::Control => {
                if let Ok(control) = ControlPdu::decode_body(&data_pdu.body)
                    && control.action == ControlPdu::REQUEST_CONTROL
                    && !progress.granted_control_sent
                {
                    response.extend(send_io_indication(server_granted_control_pdu()));
                    progress.granted_control_sent = true;
                }
            }
            ShareDataPduType::FontList => {
                response.extend(send_io_indication(server_font_map_pdu()));
                response.extend(send_io_indication(server_save_session_info_plain_notify()));

                let static_channels = self
                    .static_channel_names
                    .iter()
                    .enumerate()
                    .map(|(i, name)| (name.clone(), IO_CHANNEL_ID + 1 + i as u16))
                    .collect();

                self.state = State::Accepted;
                return Ok(StepResult::with_event(
                    response,
                    AcceptorEvent::Accepted(AcceptedConnection {
                        io_channel_id: IO_CHANNEL_ID,
                        user_channel_id: USER_CHANNEL_ID,
                        static_channels,
                        share_id: SHARE_ID,
                        desktop_width: self.desktop_width,
                        desktop_height: self.desktop_height,
                        client_name: self.client_name.clone(),
                        nscodec: self.nscodec,
                        max_request_size: self.max_request_size,
                    }),
                ));
            }
            // A real client may send other Data PDUs interleaved here (e.g.
            // stray input / suppress); tolerate and ignore them rather than error.
            ShareDataPduType::FontMap
            | ShareDataPduType::RefreshRect
            | ShareDataPduType::SuppressOutput
            | ShareDataPduType::SaveSessionInfo
            | ShareDataPduType::MonitorLayout
            | ShareDataPduType::Input => {}
        }

        self.state = State::WaitFinalization(progress);
        Ok(StepResult::just(response))
    }
}

fn send_io_indication(body: Vec<u8>) -> Vec<u8> {
    x224::wrap_data(
        &SendData {
            initiator: USER_CHANNEL_ID,
            channel_id: IO_CHANNEL_ID,
            data: body,
            complete: true,
        }
        .encode_indication(),
    )
}

fn try_decode_confirm_active(data: &[u8]) -> Result<ConfirmActive, rdpcore_pdu::DecodeError> {
    match ConfirmActive::decode(data) {
        Ok(c) => Ok(c),
        Err(first) => {
            if data.len() >= 14
                && let Ok(c) = ConfirmActive::decode(&data[4..])
            {
                return Ok(c);
            }
            Err(first)
        }
    }
}

fn server_synchronize_pdu() -> Vec<u8> {
    data_pdu_bytes(
        ShareDataPduType::Synchronize,
        SynchronizePdu {
            target_user: USER_CHANNEL_ID,
        }
        .encode_body(),
    )
}

fn server_cooperate_pdu() -> Vec<u8> {
    data_pdu_bytes(
        ShareDataPduType::Control,
        ControlPdu {
            action: ControlPdu::COOPERATE,
            grant_id: 0,
            control_id: 0,
        }
        .encode_body(),
    )
}

fn server_granted_control_pdu() -> Vec<u8> {
    data_pdu_bytes(
        ShareDataPduType::Control,
        ControlPdu {
            action: ControlPdu::GRANTED_CONTROL,
            grant_id: USER_CHANNEL_ID,
            control_id: u32::from(USER_CHANNEL_ID),
        }
        .encode_body(),
    )
}

fn server_font_map_pdu() -> Vec<u8> {
    data_pdu_bytes(
        ShareDataPduType::FontMap,
        FontPdu::font_map_default().encode_body(),
    )
}

fn server_save_session_info_plain_notify() -> Vec<u8> {
    data_pdu_bytes(
        ShareDataPduType::SaveSessionInfo,
        encode_save_session_info_plain_notify(),
    )
}

fn data_pdu_bytes(pdu_type2: ShareDataPduType, body: Vec<u8>) -> Vec<u8> {
    DataPdu {
        share_id: SHARE_ID,
        pdu_source: IO_CHANNEL_ID,
        stream_id: STREAM_UNDEFINED,
        pdu_type2,
        body,
    }
    .encode()
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
