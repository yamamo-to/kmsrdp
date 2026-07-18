//! Sans-io RDP server-side connection-sequence state machine: everything
//! from X.224 Connection Request through Finalization ("Accepted" steady
//! state), tying together the PDU codecs in `rdpcore-pdu`. No sockets, no
//! async, no TLS - the caller (`rdpcore-server`) is responsible for
//! reading/writing bytes and for driving the TLS handshake at the right
//! point (see [`Acceptor::step`]'s docs on [`AcceptorEvent::TlsUpgrade`]).
//!
//! Usage: repeatedly call [`Acceptor::step`] with one already-framed input
//! PDU (see [`Acceptor::next_read_hint`]) until it reports
//! [`AcceptorEvent::Accepted`].

mod error;

pub use error::ConnectorError;
pub use rdpcore_pdu::x224::SecurityProtocol;

use rdpcore_pdu::capability_sets::{
    BitmapCapability, BitmapCodecsCapability, ConfirmActive, DeactivateAllPdu, DemandActive,
    GeneralCapability, InputCapability, MultiFragmentUpdateCapability, OrderCapability,
    PointerCapability, ServerCapabilities, ShareControlHeader, ShareControlPduType,
    VirtualChannelCapability,
};
use rdpcore_pdu::client_info::ClientInfoPdu;
use rdpcore_pdu::cursor::ReadCursor;
use rdpcore_pdu::finalization::{
    ControlPdu, DataPdu, FontPdu, STREAM_UNDEFINED, ShareDataPduType, SynchronizePdu,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCredentials {
    pub domain: String,
    pub username: String,
    pub password: String,
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
    /// Echo of the client's RDP Negotiation Request `requestedProtocols`
    /// (MS-RDPBCGR: SC_CORE.clientRequestedProtocols MUST match this).
    client_requested_protocols: u32,
    /// Protocol selected in the Connection Confirm (`PROTOCOL_HYBRID` or
    /// `PROTOCOL_SSL`). Callers use this after TLS to decide whether CredSSP
    /// (NLA) must run before MCS Connect Initial.
    selected_protocol: SecurityProtocol,
    /// MCS user-data reassembly for Confirm Active when mstsc fragments it.
    confirm_active_buf: Vec<u8>,
}

impl Acceptor {
    pub fn new(desktop_width: u16, desktop_height: u16) -> Self {
        Self {
            state: State::WaitConnectionRequest,
            desktop_width,
            desktop_height,
            static_channel_names: Vec::new(),
            message_channel_id: None,
            client_requested_protocols: SecurityProtocol::SSL.0,
            selected_protocol: SecurityProtocol::SSL,
            confirm_active_buf: Vec::new(),
        }
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
        // otherwise fall back to TLS-only (PROTOCOL_SSL) + Client Info auth.
        // Either path still needs SSL as the transport underneath CredSSP.
        let selected = if request.protocol.contains(SecurityProtocol::HYBRID) {
            SecurityProtocol::HYBRID
        } else if request.protocol.contains(SecurityProtocol::SSL) {
            SecurityProtocol::SSL
        } else {
            self.state = State::Rejected;
            let response = ConnectionConfirm::Failure {
                code: FailureCode::SSL_REQUIRED_BY_SERVER,
            }
            .encode();
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
            bitmap_codecs: BitmapCodecsCapability,
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

        self.confirm_active_buf.extend_from_slice(&send_data.data);
        if !send_data.complete {
            self.state = State::WaitConfirmActive;
            return Ok(StepResult::just(Vec::new()));
        }

        let data = std::mem::take(&mut self.confirm_active_buf);
        let confirm = try_decode_confirm_active(&data)?;
        let _ = confirm;

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
                    }),
                ));
            }
            // A real client may send other Data PDUs interleaved here (e.g.
            // stray input); tolerate and ignore them rather than error.
            ShareDataPduType::FontMap => {}
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
mod tests {
    use super::*;
    use rdpcore_pdu::cursor::WriteBuf;
    use rdpcore_pdu::gcc::{
        CS_MCS_MSGCHANNEL, ChannelDef, ClientCoreData, ClientNetworkData, ClientSecurityData,
    };

    fn client_connect_initial(
        static_channel_names: &[&str],
        with_message_channel: bool,
    ) -> Vec<u8> {
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
                channels: static_channel_names
                    .iter()
                    .map(|name| ChannelDef {
                        name: (*name).to_owned(),
                        options: 0,
                    })
                    .collect(),
            }),
            early_capability_flags: None,
            cluster: None,
            message_channel: None,
        };
        let mut client_gcc_blocks = client_blocks.encode();
        if with_message_channel {
            client_gcc_blocks.write_u16_le(CS_MCS_MSGCHANNEL);
            client_gcc_blocks.write_u16_le(8); // 4-byte header + 4-byte flags body
            client_gcc_blocks.write_u32_le(0xC000_0000);
        }
        let request = ConferenceCreateRequest { client_gcc_blocks };
        let connect_initial = ConnectInitial {
            target_parameters: DomainParameters::target(),
            min_parameters: DomainParameters::min(),
            max_parameters: DomainParameters::max(),
            user_data: request.encode(),
        };
        x224::wrap_data(&connect_initial.encode())
    }

    /// Drives an `Acceptor` all the way to `Accepted`, standing in for a
    /// real client's byte stream so the whole handshake can be tested
    /// without a socket.
    fn run_full_handshake(
        static_channel_names: &[&str],
    ) -> (Acceptor, AcceptedConnection, ClientCredentials) {
        run_full_handshake_inner(static_channel_names, false)
    }

    fn run_full_handshake_inner(
        static_channel_names: &[&str],
        with_message_channel: bool,
    ) -> (Acceptor, AcceptedConnection, ClientCredentials) {
        let mut acceptor = Acceptor::new(1024, 768);

        let request = ConnectionRequest {
            cookie: Some("kmsrdp".to_owned()),
            flags: x224::RequestFlags(0),
            protocol: SecurityProtocol::SSL,
        };
        let result = acceptor.step(&request.encode()).unwrap();
        assert_eq!(result.event, AcceptorEvent::TlsUpgrade);

        let result = acceptor
            .step(&client_connect_initial(
                static_channel_names,
                with_message_channel,
            ))
            .unwrap();
        assert!(matches!(result.event, AcceptorEvent::None));

        let result = acceptor
            .step(&x224::wrap_data(
                &ErectDomainRequest {
                    sub_height: 0,
                    sub_interval: 0,
                }
                .encode(),
            ))
            .unwrap();
        assert!(matches!(result.event, AcceptorEvent::None));

        let result = acceptor
            .step(&x224::wrap_data(&AttachUserRequest.encode()))
            .unwrap();
        assert!(matches!(result.event, AcceptorEvent::None));

        let mut channel_ids = vec![USER_CHANNEL_ID, IO_CHANNEL_ID];
        channel_ids.extend((0..static_channel_names.len()).map(|i| IO_CHANNEL_ID + 1 + i as u16));
        if with_message_channel {
            channel_ids.push(IO_CHANNEL_ID + 1 + static_channel_names.len() as u16);
        }
        for channel_id in channel_ids {
            let join = ChannelJoinRequest {
                initiator: USER_CHANNEL_ID,
                channel_id,
            };
            let result = acceptor.step(&x224::wrap_data(&join.encode())).unwrap();
            assert!(matches!(result.event, AcceptorEvent::None));
        }

        let client_info_pdu = ClientInfoPdu {
            info: rdpcore_pdu::client_info::ClientInfo {
                username: "kmsrdp".to_owned(),
                password: "hunter2".to_owned(),
                flags: rdpcore_pdu::client_info::ClientInfoFlags::UNICODE,
                ..Default::default()
            },
        };
        let send_data = SendData {
            initiator: USER_CHANNEL_ID,
            channel_id: IO_CHANNEL_ID,
            data: client_info_pdu.encode(),
            complete: true,
        };
        let result = acceptor
            .step(&x224::wrap_data(&send_data.encode_request()))
            .unwrap();
        let AcceptorEvent::ClientInfoReceived(credentials) = result.event else {
            panic!("expected ClientInfoReceived, got {:?}", result.event);
        };
        acceptor.approve_client_info().unwrap();

        let accepted = drive_confirm_active_and_finalization(&mut acceptor);
        (acceptor, accepted, credentials)
    }

    /// Drives an `Acceptor` from `WaitConfirmActive` through to `Accepted` -
    /// the tail shared by the initial handshake and every server-initiated
    /// resize (`Acceptor::begin_resize` re-enters at exactly this state).
    fn drive_confirm_active_and_finalization(acceptor: &mut Acceptor) -> AcceptedConnection {
        // Client's Confirm Active - content doesn't matter, just needs to
        // structurally decode (share control header + at least 0 caps).
        let send_data = SendData {
            initiator: USER_CHANNEL_ID,
            channel_id: IO_CHANNEL_ID,
            data: confirm_active_fixture(),
            complete: true,
        };
        let result = acceptor
            .step(&x224::wrap_data(&send_data.encode_request()))
            .unwrap();
        assert!(matches!(result.event, AcceptorEvent::None));
        assert!(
            !result.response.is_empty(),
            "server must answer Confirm Active with Synchronize + Cooperate"
        );

        for (pdu_type2, body) in [
            (
                ShareDataPduType::Synchronize,
                SynchronizePdu {
                    target_user: USER_CHANNEL_ID,
                }
                .encode_body(),
            ),
            (
                ShareDataPduType::Control,
                ControlPdu {
                    action: ControlPdu::COOPERATE,
                    grant_id: 0,
                    control_id: 0,
                }
                .encode_body(),
            ),
            (
                ShareDataPduType::Control,
                ControlPdu {
                    action: ControlPdu::REQUEST_CONTROL,
                    grant_id: 0,
                    control_id: 0,
                }
                .encode_body(),
            ),
            (
                ShareDataPduType::FontList,
                FontPdu::font_map_default().encode_body(),
            ),
        ] {
            let data_pdu = data_pdu_bytes(pdu_type2, body);
            let send_data = SendData {
                initiator: USER_CHANNEL_ID,
                channel_id: IO_CHANNEL_ID,
                data: data_pdu,
                complete: true,
            };
            let result = acceptor
                .step(&x224::wrap_data(&send_data.encode_request()))
                .unwrap();
            if let AcceptorEvent::Accepted(accepted) = result.event {
                assert!(acceptor.is_finished());
                return accepted;
            }
        }
        panic!("acceptor never reached Accepted");
    }

    /// A minimal but structurally valid Confirm Active (Share Control
    /// Header + originatorId + zero capabilities) - the acceptor doesn't
    /// interpret capability content, only needs it to parse.
    fn confirm_active_fixture() -> Vec<u8> {
        use rdpcore_pdu::cursor::WriteBuf;
        let mut body = Vec::new();
        body.write_u16_le(USER_CHANNEL_ID); // originatorId
        body.write_u16_le(1); // lengthSourceDescriptor
        body.write_u16_le(4); // lengthCombinedCapabilities
        body.write_u8(0); // sourceDescriptor: empty
        body.write_u16_le(0); // numberCapabilities
        body.write_u16_le(0); // pad2Octets

        let mut out = Vec::new();
        out.write_u16_le((10 + body.len()) as u16); // totalLength
        out.write_u16_le(0x10 | 0x3); // version sentinel | ConfirmActivePdu
        out.write_u16_le(USER_CHANNEL_ID); // pdu_source
        out.write_u32_le(SHARE_ID);
        out.extend(body);
        out
    }

    #[test]
    fn attach_user_request_with_non_standard_optional_field() {
        let mut payload = vec![(10 << 2) | 2];
        payload.extend_from_slice(&[0x00, 0x00]);
        let mut cursor = ReadCursor::new(&payload);
        assert_eq!(
            AttachUserRequest::decode_from_cursor(&mut cursor).unwrap(),
            AttachUserRequest
        );
        assert_eq!(cursor.remaining(), 0);
    }

    #[test]
    fn batched_mcs_domain_pdus_in_one_x224_payload() {
        let mut acceptor = Acceptor::new(1024, 768);
        let request = ConnectionRequest {
            cookie: Some("kmsrdp".to_owned()),
            flags: x224::RequestFlags(0),
            protocol: SecurityProtocol::SSL,
        };
        acceptor.step(&request.encode()).unwrap();
        acceptor.step(&client_connect_initial(&[], false)).unwrap();

        let mut mcs_payload = ErectDomainRequest {
            sub_height: 0,
            sub_interval: 0,
        }
        .encode();
        mcs_payload.extend(AttachUserRequest.encode());
        mcs_payload.extend(
            ChannelJoinRequest {
                initiator: USER_CHANNEL_ID,
                channel_id: USER_CHANNEL_ID,
            }
            .encode(),
        );

        let result = acceptor.step(&x224::wrap_data(&mcs_payload)).unwrap();
        assert!(
            !result.response.is_empty(),
            "attach user + first join must be answered"
        );
        assert!(matches!(result.event, AcceptorEvent::None));
    }

    #[test]
    fn full_handshake_reaches_accepted_with_no_static_channels() {
        let (_acceptor, accepted, credentials) = run_full_handshake(&[]);
        assert_eq!(accepted.io_channel_id, IO_CHANNEL_ID);
        assert_eq!(accepted.user_channel_id, USER_CHANNEL_ID);
        assert!(accepted.static_channels.is_empty());
        assert_eq!(accepted.desktop_width, 1024);
        assert_eq!(accepted.desktop_height, 768);
        assert_eq!(credentials.username, "kmsrdp");
        assert_eq!(credentials.password, "hunter2");
    }

    #[test]
    fn full_handshake_with_message_channel_reaches_accepted() {
        let (_acceptor, accepted, _credentials) =
            run_full_handshake_inner(&["rdpdr", "rdpsnd", "cliprdr", "drdynvc"], true);
        assert_eq!(accepted.static_channels.len(), 4);
    }

    #[test]
    fn full_handshake_negotiates_static_channels_in_client_order() {
        let (_acceptor, accepted, _credentials) = run_full_handshake(&["cliprdr", "rdpsnd"]);
        assert_eq!(
            accepted.static_channels,
            vec![
                ("cliprdr".to_owned(), IO_CHANNEL_ID + 1),
                ("rdpsnd".to_owned(), IO_CHANNEL_ID + 2)
            ]
        );
    }

    #[test]
    fn rejects_client_that_does_not_offer_ssl() {
        let mut acceptor = Acceptor::new(1024, 768);
        let request = ConnectionRequest {
            cookie: None,
            flags: x224::RequestFlags(0),
            protocol: SecurityProtocol::RDP,
        };
        let result = acceptor.step(&request.encode()).unwrap();
        assert_eq!(result.event, AcceptorEvent::Rejected);
        assert!(acceptor.is_finished());

        let confirm = ConnectionConfirm::decode(&result.response).unwrap();
        assert!(matches!(
            confirm,
            ConnectionConfirm::Failure {
                code: FailureCode::SSL_REQUIRED_BY_SERVER
            }
        ));
    }

    #[test]
    fn prefers_hybrid_when_client_offers_nla() {
        let mut acceptor = Acceptor::new(1024, 768);
        let request = ConnectionRequest {
            cookie: None,
            flags: x224::RequestFlags(0),
            protocol: SecurityProtocol::SSL | SecurityProtocol::HYBRID,
        };
        let result = acceptor.step(&request.encode()).unwrap();
        assert_eq!(result.event, AcceptorEvent::TlsUpgrade);
        assert!(acceptor.requires_credssp());
        assert_eq!(acceptor.selected_protocol(), SecurityProtocol::HYBRID);
        match ConnectionConfirm::decode(&result.response).unwrap() {
            ConnectionConfirm::Response { protocol, .. } => {
                assert_eq!(protocol, SecurityProtocol::HYBRID);
            }
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[test]
    fn falls_back_to_ssl_when_client_omits_hybrid() {
        let mut acceptor = Acceptor::new(1024, 768);
        let request = ConnectionRequest {
            cookie: None,
            flags: x224::RequestFlags(0),
            protocol: SecurityProtocol::SSL,
        };
        let result = acceptor.step(&request.encode()).unwrap();
        assert_eq!(result.event, AcceptorEvent::TlsUpgrade);
        assert!(!acceptor.requires_credssp());
        assert_eq!(acceptor.selected_protocol(), SecurityProtocol::SSL);
        match ConnectionConfirm::decode(&result.response).unwrap() {
            ConnectionConfirm::Response { protocol, .. } => {
                assert_eq!(protocol, SecurityProtocol::SSL);
            }
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[test]
    fn begin_resize_round_trips_to_accepted_with_new_dimensions() {
        let (mut acceptor, first_accepted, _credentials) = run_full_handshake(&[]);
        assert_eq!(
            (first_accepted.desktop_width, first_accepted.desktop_height),
            (1024, 768)
        );

        let response = acceptor.begin_resize(1920, 1080).unwrap();
        assert!(!response.is_empty());
        assert!(
            !acceptor.is_finished(),
            "begin_resize must reopen the connection sequence"
        );

        let resized = drive_confirm_active_and_finalization(&mut acceptor);
        assert_eq!(
            (resized.desktop_width, resized.desktop_height),
            (1920, 1080)
        );
        assert_eq!(
            resized.share_id, first_accepted.share_id,
            "share_id doesn't change across a resize"
        );
        assert!(acceptor.is_finished());
    }

    #[test]
    fn begin_resize_before_accepted_is_rejected() {
        let mut acceptor = Acceptor::new(1024, 768);
        assert!(matches!(
            acceptor.begin_resize(1920, 1080),
            Err(ConnectorError::NotReady)
        ));
    }

    #[test]
    fn step_after_accepted_preserves_accepted_so_resize_still_works() {
        let (mut acceptor, _accepted, _credentials) = run_full_handshake(&[]);
        let bogus = x224::wrap_data(
            &SendData {
                initiator: USER_CHANNEL_ID,
                channel_id: IO_CHANNEL_ID,
                data: vec![0, 1, 2, 3],
                complete: true,
            }
            .encode_request(),
        );
        assert!(matches!(
            acceptor.step(&bogus),
            Err(ConnectorError::AlreadyFinished)
        ));
        assert!(
            acceptor.is_finished(),
            "AlreadyFinished must not poison Accepted into Rejected"
        );
        assert!(
            acceptor.begin_resize(1280, 720).is_ok(),
            "resize must still be possible after a stray post-Accepted step"
        );
    }

    #[test]
    fn resize_finalization_ignores_static_channel_traffic() {
        let (mut acceptor, _accepted, _credentials) = run_full_handshake(&["cliprdr"]);
        acceptor.begin_resize(1920, 1080).unwrap();

        // Confirm Active first.
        let send_data = SendData {
            initiator: USER_CHANNEL_ID,
            channel_id: IO_CHANNEL_ID,
            data: confirm_active_fixture(),
            complete: true,
        };
        assert!(matches!(
            acceptor
                .step(&x224::wrap_data(&send_data.encode_request()))
                .unwrap()
                .event,
            AcceptorEvent::None
        ));

        // Interleaved cliprdr traffic (non-IO) must not abort finalization.
        let cliprdr_noise = SendData {
            initiator: USER_CHANNEL_ID,
            channel_id: IO_CHANNEL_ID + 1,
            data: vec![0xDE, 0xAD, 0xBE, 0xEF],
            complete: true,
        };
        let ignored = acceptor
            .step(&x224::wrap_data(&cliprdr_noise.encode_request()))
            .unwrap();
        assert!(matches!(ignored.event, AcceptorEvent::None));
        assert!(!acceptor.is_finished());

        let resized = drive_confirm_active_and_finalization_from_sync(&mut acceptor);
        assert_eq!(
            (resized.desktop_width, resized.desktop_height),
            (1920, 1080)
        );
    }

    #[test]
    fn resize_finalization_accepts_batched_mcs_send_data() {
        let (mut acceptor, _accepted, _credentials) = run_full_handshake(&[]);
        acceptor.begin_resize(1920, 1080).unwrap();

        let confirm = SendData {
            initiator: USER_CHANNEL_ID,
            channel_id: IO_CHANNEL_ID,
            data: confirm_active_fixture(),
            complete: true,
        };
        assert!(matches!(
            acceptor
                .step(&x224::wrap_data(&confirm.encode_request()))
                .unwrap()
                .event,
            AcceptorEvent::None
        ));

        // mstsc often packs Synchronize + Cooperate + Request Control + FontList
        // into a single X.224 Data TPDU. Losing FontList here used to leave the
        // acceptor unfinished while the client already painted a blank canvas.
        let mut mcs_payload = Vec::new();
        for (pdu_type2, body) in [
            (
                ShareDataPduType::Synchronize,
                SynchronizePdu {
                    target_user: USER_CHANNEL_ID,
                }
                .encode_body(),
            ),
            (
                ShareDataPduType::Control,
                ControlPdu {
                    action: ControlPdu::COOPERATE,
                    grant_id: 0,
                    control_id: 0,
                }
                .encode_body(),
            ),
            (
                ShareDataPduType::Control,
                ControlPdu {
                    action: ControlPdu::REQUEST_CONTROL,
                    grant_id: 0,
                    control_id: 0,
                }
                .encode_body(),
            ),
            (
                ShareDataPduType::FontList,
                FontPdu::font_map_default().encode_body(),
            ),
        ] {
            mcs_payload.extend(
                SendData {
                    initiator: USER_CHANNEL_ID,
                    channel_id: IO_CHANNEL_ID,
                    data: data_pdu_bytes(pdu_type2, body),
                    complete: true,
                }
                .encode_request(),
            );
        }

        let result = acceptor.step(&x224::wrap_data(&mcs_payload)).unwrap();
        assert!(
            matches!(result.event, AcceptorEvent::Accepted(_)),
            "batched finalization must reach Accepted in one step"
        );
        assert!(acceptor.is_finished());
    }

    /// Like `drive_confirm_active_and_finalization`, but skips Confirm Active
    /// (caller already sent it) and starts at Synchronize.
    fn drive_confirm_active_and_finalization_from_sync(
        acceptor: &mut Acceptor,
    ) -> AcceptedConnection {
        for (pdu_type2, body) in [
            (
                ShareDataPduType::Synchronize,
                SynchronizePdu {
                    target_user: USER_CHANNEL_ID,
                }
                .encode_body(),
            ),
            (
                ShareDataPduType::Control,
                ControlPdu {
                    action: ControlPdu::COOPERATE,
                    grant_id: 0,
                    control_id: 0,
                }
                .encode_body(),
            ),
            (
                ShareDataPduType::Control,
                ControlPdu {
                    action: ControlPdu::REQUEST_CONTROL,
                    grant_id: 0,
                    control_id: 0,
                }
                .encode_body(),
            ),
            (
                ShareDataPduType::FontList,
                FontPdu::font_map_default().encode_body(),
            ),
        ] {
            let data_pdu = data_pdu_bytes(pdu_type2, body);
            let send_data = SendData {
                initiator: USER_CHANNEL_ID,
                channel_id: IO_CHANNEL_ID,
                data: data_pdu,
                complete: true,
            };
            let result = acceptor
                .step(&x224::wrap_data(&send_data.encode_request()))
                .unwrap();
            if let AcceptorEvent::Accepted(accepted) = result.event {
                assert!(acceptor.is_finished());
                return accepted;
            }
        }
        panic!("finalization did not reach Accepted");
    }
}
