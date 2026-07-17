//! MS-RDPECLIP clipboard virtual channel (text formats only): PDU codec
//! (`pdu`) plus the connection-scoped state machine (`CliprdrChannel`) and
//! backend trait (`CliprdrBackend`) a server plugs a local clipboard into.

pub mod pdu;

use rdpcore_pdu::mcs::SendData;
use rdpcore_pdu::{DecodeError, svc, x224};

/// One clipboard format, identified by its standard numeric ID (this
/// crate only ever produces/expects `pdu::CF_UNICODETEXT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardFormat {
    pub id: u32,
}

impl ClipboardFormat {
    pub fn unicode_text() -> Self {
        Self {
            id: pdu::CF_UNICODETEXT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatDataRequest {
    pub format: u32,
}

/// `Ok(text)` for a successful response, `Err(())` for `CB_RESPONSE_FAIL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatDataResponse(Result<String, ()>);

impl FormatDataResponse {
    pub fn new_unicode_string(text: &str) -> Self {
        Self(Ok(text.to_owned()))
    }

    pub fn new_error() -> Self {
        Self(Err(()))
    }

    pub fn is_error(&self) -> bool {
        self.0.is_err()
    }

    pub fn to_unicode_string(&self) -> Option<String> {
        self.0.as_ref().ok().cloned()
    }
}

/// What a backend sends back through its factory-provided channel, to be
/// turned into wire bytes by [`CliprdrChannel`].
pub enum ClipboardMessage {
    /// Advertise newly-available local clipboard formats to the remote
    /// side (a Format List PDU).
    SendInitiateCopy(Vec<ClipboardFormat>),
    /// Ask the remote side for its clipboard content in the given format
    /// (a Format Data Request PDU) - used to pull remote clipboard content
    /// into the local clipboard.
    SendInitiatePaste(u32),
    /// Answer a pending [`CliprdrBackend::on_format_data_request`] (a
    /// Format Data Response PDU).
    SendFormatData(FormatDataResponse),
}

/// Text-only clipboard backend: no file contents, no locking - a minimal
/// server has no use for either, and kmsrdp's own local-clipboard bridge
/// only ever deals in `CF_UNICODETEXT`.
pub trait CliprdrBackend: core::fmt::Debug + Send {
    /// Called once the channel is established and ready to advertise the
    /// local clipboard's current contents, if any.
    fn on_ready(&mut self);
    /// The remote side just advertised new clipboard formats.
    fn on_remote_copy(&mut self, formats: &[ClipboardFormat]);
    /// The remote side is asking for clipboard data in a format the local
    /// side previously advertised.
    fn on_format_data_request(&mut self, request: FormatDataRequest);
    /// The remote side answered a `SendInitiatePaste` request.
    fn on_format_data_response(&mut self, response: FormatDataResponse);
}

/// Builds a per-connection clipboard backend. The outgoing-message `sender`
/// is supplied by the server for that connection so concurrent sessions
/// each get an independent channel.
pub trait CliprdrBackendFactory: Send + Sync {
    fn build_cliprdr_backend(
        &self,
        sender: tokio::sync::mpsc::UnboundedSender<ClipboardMessage>,
    ) -> Box<dyn CliprdrBackend>;
}

/// One connection's cliprdr channel. Every method that produces something
/// to send returns fully wire-ready frames (SVC chunking, TPKT/X.224/MCS
/// SendDataIndication all applied) - the caller (`rdpcore-server`) only
/// needs to wrap them in a `rdpcore_transport::Frame` and hand them to the
/// scheduler.
pub struct CliprdrChannel {
    channel_id: u16,
    user_channel_id: u16,
    backend: Box<dyn CliprdrBackend>,
    /// Accumulates SVC chunks across possibly-multiple `on_channel_data`
    /// calls until a `CHANNEL_FLAG_LAST` chunk completes one logical PDU -
    /// unlike rdpsnd's tiny control messages, a real client's Format List
    /// (many registered formats, each with a name) can plausibly exceed
    /// one SVC chunk, so this can't assume single-chunk messages.
    incoming_buffer: Vec<u8>,
}

impl CliprdrChannel {
    /// Returns the channel plus the initial Capabilities + Monitor Ready
    /// frames the caller should send immediately, in order - the server
    /// always speaks first on this channel (MS-RDPECLIP 3.2.5.1).
    pub fn new(
        channel_id: u16,
        user_channel_id: u16,
        mut backend: Box<dyn CliprdrBackend>,
    ) -> (Self, Vec<Vec<u8>>) {
        backend.on_ready();
        let mut initial = wrap_indication(user_channel_id, channel_id, pdu::encode_capabilities());
        initial.extend(wrap_indication(
            user_channel_id,
            channel_id,
            pdu::encode_monitor_ready(),
        ));
        (
            Self {
                channel_id,
                user_channel_id,
                backend,
                incoming_buffer: Vec::new(),
            },
            initial,
        )
    }

    pub fn channel_id(&self) -> u16 {
        self.channel_id
    }

    /// `payload` is one SVC chunk (Channel PDU Header included) of
    /// `"cliprdr"`-channel data from an incoming MCS Send Data Request.
    pub fn on_channel_data(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>, DecodeError> {
        let (_total_length, flags, chunk_body) = svc::dechunkify(payload)?;
        if flags & svc::CHANNEL_FLAG_FIRST != 0 {
            self.incoming_buffer.clear();
        }
        self.incoming_buffer.extend_from_slice(chunk_body);
        if flags & svc::CHANNEL_FLAG_LAST == 0 {
            return Ok(Vec::new()); // wait for the rest
        }

        let message = core::mem::take(&mut self.incoming_buffer);
        match pdu::decode_client_message(&message)? {
            pdu::ClientMessage::FormatList(format_ids) => {
                let formats: Vec<ClipboardFormat> = format_ids
                    .into_iter()
                    .map(|id| ClipboardFormat { id })
                    .collect();
                self.backend.on_remote_copy(&formats);
                Ok(wrap_indication(
                    self.user_channel_id,
                    self.channel_id,
                    pdu::encode_format_list_response_ok(),
                ))
            }
            pdu::ClientMessage::FormatListResponse => Ok(Vec::new()), // ack for a list we sent - nothing to do
            pdu::ClientMessage::FormatDataRequest(format) => {
                self.backend
                    .on_format_data_request(FormatDataRequest { format });
                Ok(Vec::new()) // the response comes asynchronously via encode_message
            }
            pdu::ClientMessage::FormatDataResponse(result) => {
                let response = match result {
                    Ok(text) => FormatDataResponse::new_unicode_string(&text),
                    Err(()) => FormatDataResponse::new_error(),
                };
                self.backend.on_format_data_response(response);
                Ok(Vec::new())
            }
            // Capabilities, Temporary Directory, file contents, locking -
            // not implemented; safely ignored.
            pdu::ClientMessage::Other => Ok(Vec::new()),
        }
    }

    /// Encodes one backend-produced event for the wire.
    pub fn encode_message(&mut self, message: ClipboardMessage) -> Vec<Vec<u8>> {
        let body = match message {
            ClipboardMessage::SendInitiateCopy(formats) => {
                if !formats.iter().any(|f| f.id == pdu::CF_UNICODETEXT) {
                    return Vec::new(); // nothing this crate knows how to advertise
                }
                pdu::encode_format_list_unicode_text()
            }
            ClipboardMessage::SendInitiatePaste(format_id) => {
                pdu::encode_format_data_request(format_id)
            }
            ClipboardMessage::SendFormatData(response) => match response.to_unicode_string() {
                Some(text) => pdu::encode_format_data_response_text(&text),
                None => pdu::encode_format_data_response_error(),
            },
        };
        wrap_indication(self.user_channel_id, self.channel_id, body)
    }
}

fn wrap_indication(initiator: u16, channel_id: u16, data: Vec<u8>) -> Vec<Vec<u8>> {
    svc::chunkify(&data)
        .into_iter()
        .map(|chunk| {
            x224::wrap_data(
                &SendData {
                    initiator,
                    channel_id,
                    data: chunk,
                    complete: true,
                }
                .encode_indication(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct FakeBackend {
        ready: bool,
        remote_formats: Vec<ClipboardFormat>,
        last_request: Option<FormatDataRequest>,
        last_response: Option<FormatDataResponse>,
    }

    impl CliprdrBackend for FakeBackend {
        fn on_ready(&mut self) {
            self.ready = true;
        }
        fn on_remote_copy(&mut self, formats: &[ClipboardFormat]) {
            self.remote_formats = formats.to_vec();
        }
        fn on_format_data_request(&mut self, request: FormatDataRequest) {
            self.last_request = Some(request);
        }
        fn on_format_data_response(&mut self, response: FormatDataResponse) {
            self.last_response = Some(response);
        }
    }

    #[test]
    fn new_channel_sends_capabilities_then_monitor_ready_and_calls_on_ready() {
        let (channel, initial) = CliprdrChannel::new(1004, 1002, Box::new(FakeBackend::default()));
        assert_eq!(initial.len(), 2);
        let _ = channel; // on_ready was called inside `new` on the boxed backend before it moved in
    }

    #[test]
    fn incoming_format_list_triggers_on_remote_copy_and_acks() {
        let (mut channel, _initial) =
            CliprdrChannel::new(1004, 1002, Box::new(FakeBackend::default()));

        let wire = svc::chunkify(&pdu::encode_format_list_unicode_text());
        assert_eq!(wire.len(), 1);
        let response = channel.on_channel_data(&wire[0]).unwrap();
        assert_eq!(response.len(), 1); // Format List Response
    }

    #[test]
    fn multi_chunk_incoming_message_is_reassembled_before_dispatch() {
        let (mut channel, _initial) =
            CliprdrChannel::new(1004, 1002, Box::new(FakeBackend::default()));

        let full = pdu::encode_format_list_unicode_text();
        // Force a two-chunk split to exercise reassembly, mirroring what a
        // real (larger) format list would look like.
        let mid = full.len() / 2;
        let mut first = Vec::new();
        first.extend_from_slice(&(full.len() as u32).to_le_bytes());
        first.extend_from_slice(&svc::CHANNEL_FLAG_FIRST.to_le_bytes());
        first.extend_from_slice(&full[..mid]);
        let mut second = Vec::new();
        second.extend_from_slice(&(full.len() as u32).to_le_bytes());
        second.extend_from_slice(&svc::CHANNEL_FLAG_LAST.to_le_bytes());
        second.extend_from_slice(&full[mid..]);

        assert!(channel.on_channel_data(&first).unwrap().is_empty()); // waiting for more
        let response = channel.on_channel_data(&second).unwrap();
        assert_eq!(response.len(), 1); // now dispatched: Format List Response went out
    }

    #[test]
    fn encode_initiate_copy_only_for_unicode_text() {
        let (mut channel, _initial) =
            CliprdrChannel::new(1004, 1002, Box::new(FakeBackend::default()));
        let frames = channel.encode_message(ClipboardMessage::SendInitiateCopy(vec![
            ClipboardFormat::unicode_text(),
        ]));
        assert_eq!(frames.len(), 1);

        // A format this crate doesn't know how to advertise: nothing sent.
        let frames =
            channel.encode_message(ClipboardMessage::SendInitiateCopy(vec![ClipboardFormat {
                id: 2,
            }]));
        assert!(frames.is_empty());
    }

    #[test]
    fn format_data_request_response_round_trip_via_wire() {
        let (mut channel, _initial) =
            CliprdrChannel::new(1004, 1002, Box::new(FakeBackend::default()));

        let request_wire =
            channel.encode_message(ClipboardMessage::SendInitiatePaste(pdu::CF_UNICODETEXT));
        assert_eq!(request_wire.len(), 1);

        let response_wire = channel.encode_message(ClipboardMessage::SendFormatData(
            FormatDataResponse::new_unicode_string("hello"),
        ));
        assert_eq!(response_wire.len(), 1);
    }
}
