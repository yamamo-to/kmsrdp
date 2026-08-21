//! MS-RDPEDYC dynamic virtual channel multiplexer: PDU codec (`pdu`) plus
//! [`DvcMux`], which negotiates capabilities, opens/closes dynamic
//! channels on demand, and reassembles each channel's Data/DataFirst
//! fragments before handing complete payloads to a [`DvcHandler`].
//!
//! Production handlers (e.g. AUDIO_INPUT via `rdpcore-rdpeai`) register
//! through [`DvcMux::register_channel`]. The optional `echo` feature
//! provides a loopback handler for transport smoke tests only.

#[cfg(feature = "echo")]
pub mod echo;
pub mod pdu;

use rdpcore_pdu::svc::wrap_indication;
use rdpcore_pdu::{DecodeError, svc};

#[cfg(test)]
use rdpcore_pdu::mcs::SendData;
#[cfg(test)]
use rdpcore_pdu::x224;

/// One dynamic channel's behavior. `on_open`/`on_data` return payloads to
/// send in response, if any - [`DvcMux`] takes care of DVC-layer framing
/// (Data/DataFirst fragmentation) and everything below it.
pub trait DvcHandler: core::fmt::Debug + Send {
    fn channel_name(&self) -> &str;
    fn on_open(&mut self) -> Vec<Vec<u8>>;
    fn on_data(&mut self, data: &[u8]) -> Vec<Vec<u8>>;
}

struct ChannelSlot {
    id: u32,
    handler: Box<dyn DvcHandler>,
    /// `Some((total_length, accumulated))` while a `DataFirst` has been
    /// seen but not yet fully reassembled.
    reassembly: Option<(u32, Vec<u8>)>,
}

/// One connection's `"drdynvc"` static channel, multiplexing zero-or-more
/// dynamic channels over it.
pub struct DvcMux {
    channel_id: u16,
    user_channel_id: u16,
    /// Reassembly buffer for the *static*-channel-level SVC chunking
    /// (separate from, and one layer below, each dynamic channel's own
    /// Data/DataFirst reassembly).
    svc_incoming: Vec<u8>,
    next_dynamic_id: u32,
    capability_negotiated: bool,
    /// Registered before capability negotiation completed - their Create
    /// Request is deferred until it does.
    pending: Vec<Box<dyn DvcHandler>>,
    channels: Vec<ChannelSlot>,
}

/// Maximum reassembled payload size for a dynamic virtual channel (16 MB).
pub const MAX_DVC_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

impl DvcMux {
    /// Returns the mux plus the initial Capability Request frame(s) the
    /// caller should send immediately - the server always speaks first on
    /// this channel, before waiting for anything from the client.
    pub fn new(channel_id: u16, user_channel_id: u16) -> (Self, Vec<Vec<u8>>) {
        let initial = wrap_indication(
            user_channel_id,
            channel_id,
            pdu::encode_capability_request(),
        );
        (
            Self {
                channel_id,
                user_channel_id,
                svc_incoming: Vec::new(),
                next_dynamic_id: 1,
                capability_negotiated: false,
                pending: Vec::new(),
                channels: Vec::new(),
            },
            initial,
        )
    }

    pub fn channel_id(&self) -> u16 {
        self.channel_id
    }

    /// Registers a new dynamic channel to open. Returns the Create
    /// Request frame(s) to send immediately if capabilities have already
    /// been negotiated; otherwise the request is queued and sent as soon
    /// as negotiation completes.
    pub fn register_channel(&mut self, handler: Box<dyn DvcHandler>) -> Vec<Vec<u8>> {
        if self.capability_negotiated {
            self.open_channel(handler)
        } else {
            self.pending.push(handler);
            Vec::new()
        }
    }

    fn open_channel(&mut self, handler: Box<dyn DvcHandler>) -> Vec<Vec<u8>> {
        let id = self.next_dynamic_id;
        self.next_dynamic_id += 1;
        let name = handler.channel_name().to_owned();
        self.channels.push(ChannelSlot {
            id,
            handler,
            reassembly: None,
        });
        wrap_indication(
            self.user_channel_id,
            self.channel_id,
            pdu::encode_create_request(id, &name),
        )
    }

    /// `payload` is one SVC chunk (Channel PDU Header included) of
    /// `"drdynvc"`-channel data from an incoming MCS Send Data Request.
    pub fn on_channel_data(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>, DecodeError> {
        let Some(message) = svc::reassemble(
            &mut self.svc_incoming,
            payload,
            MAX_DVC_MESSAGE_SIZE,
            "dvc.svc_incoming",
        )?
        else {
            return Ok(Vec::new()); // wait for the rest of this drdynvc-level message
        };

        match pdu::decode_client_message(&message)? {
            pdu::ClientMessage::CapabilityResponse { .. } => {
                self.capability_negotiated = true;
                let pending = core::mem::take(&mut self.pending);
                let mut frames = Vec::new();
                for handler in pending {
                    frames.extend(self.open_channel(handler));
                }
                Ok(frames)
            }
            pdu::ClientMessage::CreateResponse(response) => {
                if !response.is_ok() {
                    self.channels.retain(|slot| slot.id != response.channel_id);
                    return Ok(Vec::new());
                }
                let payloads = {
                    let Some(slot) = self
                        .channels
                        .iter_mut()
                        .find(|slot| slot.id == response.channel_id)
                    else {
                        return Ok(Vec::new());
                    };
                    slot.handler.on_open()
                };
                Ok(self.encode_payloads(response.channel_id, payloads))
            }
            pdu::ClientMessage::Data { channel_id, data } => {
                self.handle_incoming_data(channel_id, data, None)
            }
            pdu::ClientMessage::DataFirst {
                channel_id,
                total_length,
                data,
            } => self.handle_incoming_data(channel_id, data, Some(total_length)),
            pdu::ClientMessage::Close { channel_id } => {
                self.channels.retain(|slot| slot.id != channel_id);
                Ok(Vec::new())
            }
            pdu::ClientMessage::Other => Ok(Vec::new()),
        }
    }

    fn handle_incoming_data(
        &mut self,
        channel_id: u32,
        mut data: Vec<u8>,
        starts_new: Option<u32>,
    ) -> Result<Vec<Vec<u8>>, DecodeError> {
        let payloads = {
            let Some(slot) = self.channels.iter_mut().find(|slot| slot.id == channel_id) else {
                return Ok(Vec::new());
            };
            let complete = match starts_new {
                Some(total_length) => {
                    if total_length as usize > MAX_DVC_MESSAGE_SIZE {
                        return Err(DecodeError::InvalidValue {
                            field: "dvc.data_first.total_length",
                            reason: "DataFirst declared total length exceeds maximum allowed size",
                        });
                    }
                    if data.len() as u32 >= total_length {
                        // A client declaring `total_length` but then
                        // sending more bytes than that in the very same
                        // PDU is malformed - truncate to what it actually
                        // promised rather than handing the handler extra,
                        // unaccounted-for trailing bytes as if they were
                        // part of the message.
                        data.truncate(total_length as usize);
                        Some(data)
                    } else {
                        slot.reassembly = Some((total_length, data));
                        None
                    }
                }
                None => match slot.reassembly.take() {
                    Some((total_length, mut buffered)) => {
                        if buffered.len().saturating_add(data.len()) > MAX_DVC_MESSAGE_SIZE {
                            return Err(DecodeError::InvalidValue {
                                field: "dvc.data.buffered",
                                reason: "accumulated dynamic channel data exceeded maximum allowed size",
                            });
                        }
                        buffered.extend(data);
                        if buffered.len() as u32 >= total_length {
                            // Same reasoning as above: don't let a
                            // continuation overrun the declared total
                            // silently pass extra bytes through.
                            buffered.truncate(total_length as usize);
                            Some(buffered)
                        } else {
                            slot.reassembly = Some((total_length, buffered));
                            None
                        }
                    }
                    // A standalone Data PDU with no preceding DataFirst is
                    // already the complete payload.
                    None => Some(data),
                },
            };
            match complete {
                Some(complete) => slot.handler.on_data(&complete),
                None => return Ok(Vec::new()),
            }
        };
        Ok(self.encode_payloads(channel_id, payloads))
    }

    fn encode_payloads(&self, channel_id: u32, payloads: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        payloads
            .into_iter()
            .flat_map(|payload| pdu::encode_channel_payload(channel_id, &payload))
            .flat_map(|dvc_pdu| wrap_indication(self.user_channel_id, self.channel_id, dvc_pdu))
            .collect()
    }

    /// Look up a dynamic channel id by its ANSI name (e.g. GFX / AUDIO_INPUT).
    pub fn channel_id_for_name(&self, name: &str) -> Option<u32> {
        self.channels
            .iter()
            .find(|slot| slot.handler.channel_name() == name)
            .map(|slot| slot.id)
    }

    /// Wrap already-assembled dynamic-channel payloads for a known channel id
    /// into MCS Send Data Indication frames on the `"drdynvc"` static channel.
    /// Used by producers that push unsolicited data (GFX frames) outside of
    /// [`DvcHandler::on_data`].
    pub fn wrap_channel_payloads(&self, channel_id: u32, payloads: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        self.encode_payloads(channel_id, payloads)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdpcore_pdu::cursor::WriteBuf;

    #[derive(Debug, Default)]
    struct RecordingHandler {
        name: &'static str,
        opened: bool,
        received: Vec<Vec<u8>>,
        reply_with: Vec<Vec<u8>>,
    }

    impl DvcHandler for RecordingHandler {
        fn channel_name(&self) -> &str {
            self.name
        }
        fn on_open(&mut self) -> Vec<Vec<u8>> {
            self.opened = true;
            self.reply_with.clone()
        }
        fn on_data(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
            self.received.push(data.to_vec());
            Vec::new()
        }
    }

    #[test]
    fn full_lifecycle_open_data_close() {
        let (mut mux, initial) = DvcMux::new(1005, 1002);
        assert_eq!(initial.len(), 1); // Capability Request

        // Client responds to capability negotiation.
        let cap_response_wire = svc::chunkify(&{
            let mut raw = Vec::new();
            raw.write_u8(0x05 << 4);
            raw.write_u8(0);
            raw.write_u16_le(2);
            raw
        });
        assert_eq!(cap_response_wire.len(), 1);

        let handler = Box::new(RecordingHandler {
            name: "TEST",
            ..Default::default()
        });
        // Registered before negotiation completes - should be queued.
        assert!(mux.register_channel(handler).is_empty());

        let create_requests = mux.on_channel_data(&cap_response_wire[0]).unwrap();
        assert_eq!(create_requests.len(), 1); // Create Request for the queued channel

        // Client confirms creation with channel_id=1 (server's first allocation).
        let create_response_wire = svc::chunkify(&{
            let mut raw = Vec::new();
            raw.write_u8(0x01 << 4);
            raw.write_u8(1);
            raw.write_u32_le(0);
            raw
        });
        let opened_frames = mux.on_channel_data(&create_response_wire[0]).unwrap();
        assert!(opened_frames.is_empty()); // RecordingHandler's on_open reply_with is empty by default

        // Client sends data on the now-open channel.
        let data_wire = svc::chunkify(&pdu::encode_data(1, b"hello"));
        assert!(mux.on_channel_data(&data_wire[0]).unwrap().is_empty());

        // Client closes the channel.
        let close_wire = svc::chunkify(&pdu::encode_close(1));
        assert!(mux.on_channel_data(&close_wire[0]).unwrap().is_empty());
    }

    #[test]
    fn on_open_reply_payload_reaches_the_wire_as_a_data_pdu() {
        let (mut mux, _initial) = DvcMux::new(1005, 1002);
        mux.capability_negotiated = true; // skip straight to negotiated for this test

        let handler = Box::new(RecordingHandler {
            name: "TEST",
            reply_with: vec![b"greetings".to_vec()],
            ..Default::default()
        });
        let create_request = mux.register_channel(handler);
        assert_eq!(create_request.len(), 1);

        let create_response_wire = svc::chunkify(&{
            let mut raw = Vec::new();
            raw.write_u8(0x01 << 4);
            raw.write_u8(1);
            raw.write_u32_le(0);
            raw
        });
        let reply_frames = mux.on_channel_data(&create_response_wire[0]).unwrap();
        assert_eq!(reply_frames.len(), 1);

        // Unwrap the wire frame back down to the DVC Data PDU to confirm
        // the handler's payload made it through encode_channel_payload.
        let x224_payload = x224::unwrap_data(&reply_frames[0]).unwrap();
        let send_data = SendData::decode_indication(x224_payload).unwrap();
        let (_, _, dvc_pdu) = svc::dechunkify(&send_data.data).unwrap();
        assert_eq!(
            pdu::decode_client_message(dvc_pdu).unwrap(),
            pdu::ClientMessage::Data {
                channel_id: 1,
                data: b"greetings".to_vec()
            }
        );
    }

    #[test]
    fn rejects_oversized_data_first_total_length() {
        let (mut mux, _) = DvcMux::new(1005, 1002);
        mux.capability_negotiated = true;
        let handler = Box::new(RecordingHandler {
            name: "TEST",
            ..Default::default()
        });
        mux.register_channel(handler);

        // Client sends DataFirst with total_length = 32 MB (> MAX_DVC_MESSAGE_SIZE 16 MB)
        let data_first_pdu = pdu::encode_data_first(1, 32 * 1024 * 1024, b"head");
        let wire = svc::chunkify(&data_first_pdu);
        let result = mux.on_channel_data(&wire[0]);
        assert!(
            matches!(result, Err(DecodeError::InvalidValue { field, .. }) if field == "dvc.data_first.total_length")
        );
    }

    #[derive(Debug)]
    struct EchoBackHandler;
    impl DvcHandler for EchoBackHandler {
        fn channel_name(&self) -> &str {
            "TEST"
        }
        fn on_open(&mut self) -> Vec<Vec<u8>> {
            Vec::new()
        }
        fn on_data(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
            vec![data.to_vec()]
        }
    }

    #[test]
    fn data_first_overrunning_its_declared_total_length_is_truncated() {
        let (mut mux, _) = DvcMux::new(1005, 1002);
        mux.capability_negotiated = true;
        mux.register_channel(Box::new(EchoBackHandler));

        // DataFirst declares total_length=4 but the PDU actually carries
        // 8 bytes ("overrun!" truncated to "over") - the extra 4 bytes
        // must not silently reach the handler as part of the message.
        let data_first_pdu = pdu::encode_data_first(1, 4, b"overrun!");
        let wire = svc::chunkify(&data_first_pdu);
        let replies = mux.on_channel_data(&wire[0]).unwrap();
        assert_eq!(replies.len(), 1);

        let x224_payload = x224::unwrap_data(&replies[0]).unwrap();
        let send_data = SendData::decode_indication(x224_payload).unwrap();
        let (_, _, dvc_pdu) = svc::dechunkify(&send_data.data).unwrap();
        let pdu::ClientMessage::Data { data, .. } = pdu::decode_client_message(dvc_pdu).unwrap()
        else {
            panic!("expected a Data PDU echoed back");
        };
        assert_eq!(data, b"over");
    }
}
