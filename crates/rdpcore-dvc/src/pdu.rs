//! MS-RDPEDYC wire format: the subset needed to negotiate capabilities,
//! create/close a dynamic channel, and exchange data on it (with this
//! layer's own Data/Data-First fragmentation, separate from and nested
//! inside the static-channel-level SVC chunking every `"drdynvc"` message
//! also goes through - see `rdpcore_pdu::svc`). Compressed variants and
//! SoftSync are out of scope.
//!
//! Every message starts with one header byte packing `cmd` (high nibble),
//! `sp` (bits 3-2, meaning depends on `cmd`), and `cb_id` (low 2 bits - a
//! length-of-length selector, `0`/`1`/`2` meaning the channel ID that
//! follows is 1/2/4 bytes, little-endian). Capability Request/Response are
//! the one exception: they carry no channel ID at all (`cb_id` is unused,
//! always encoded as 0).

use rdpcore_pdu::cursor::{ReadCursor, WriteBuf};
use rdpcore_pdu::DecodeError;

const CMD_CREATE: u8 = 0x01;
const CMD_DATA_FIRST: u8 = 0x02;
const CMD_DATA: u8 = 0x03;
const CMD_CLOSE: u8 = 0x04;
const CMD_CAPABILITY: u8 = 0x05;

pub const CHANNEL_NAME: &str = "drdynvc";

/// Above this, a payload must be split into a `DataFirst` PDU plus one or
/// more `Data` continuation PDUs - matches a real implementation's own
/// per-PDU cap, chosen to comfortably fit inside one SVC chunk
/// (`rdpcore_pdu::svc::DEFAULT_CHUNK_LENGTH`) alongside this layer's own
/// header bytes.
pub const MAX_DATA_SIZE: usize = 1590;

/// Picks the smallest `FieldType` selector (`0`=u8, `1`=u16, `2`=u32) that
/// can represent `value`, and returns it alongside the byte width.
fn field_size_for(value: u32) -> (u8, usize) {
    if value <= 0xFF {
        (0, 1)
    } else if value <= 0xFFFF {
        (1, 2)
    } else {
        (2, 4)
    }
}

fn write_variable(out: &mut Vec<u8>, value: u32, size: usize) {
    match size {
        1 => out.write_u8(value as u8),
        2 => out.write_u16_le(value as u16),
        4 => out.write_u32_le(value),
        _ => unreachable!("field_size_for only ever returns 1, 2, or 4"),
    }
}

fn read_variable(cursor: &mut ReadCursor<'_>, selector: u8) -> Result<u32, DecodeError> {
    match selector {
        0 => Ok(u32::from(cursor.read_u8()?)),
        1 => Ok(u32::from(cursor.read_u16_le()?)),
        2 => Ok(cursor.read_u32_le()?),
        _ => Err(DecodeError::InvalidValue {
            field: "dvc.field_selector",
            reason: "selector value 3 is reserved/unused",
        }),
    }
}

pub fn encode_capability_request() -> Vec<u8> {
    let mut out = Vec::with_capacity(11);
    out.write_u8(CMD_CAPABILITY << 4); // sp=0, cb_id=0: no channel ID on this PDU
    out.write_u8(0); // pad
    out.write_u16_le(2); // CapsVersion 2
    for _ in 0..4 {
        out.write_u16_le(0); // priority charges, all zero
    }
    out
}

pub fn encode_create_request(channel_id: u32, channel_name: &str) -> Vec<u8> {
    let (cb_sel, cb_size) = field_size_for(channel_id);
    let mut out = Vec::with_capacity(1 + cb_size + channel_name.len() + 1);
    out.write_u8((CMD_CREATE << 4) | cb_sel);
    write_variable(&mut out, channel_id, cb_size);
    out.write_slice(channel_name.as_bytes());
    out.write_u8(0); // NUL terminator
    out
}

pub fn encode_close(channel_id: u32) -> Vec<u8> {
    let (cb_sel, cb_size) = field_size_for(channel_id);
    let mut out = Vec::with_capacity(1 + cb_size);
    out.write_u8((CMD_CLOSE << 4) | cb_sel);
    write_variable(&mut out, channel_id, cb_size);
    out
}

pub fn encode_data(channel_id: u32, data: &[u8]) -> Vec<u8> {
    let (cb_sel, cb_size) = field_size_for(channel_id);
    let mut out = Vec::with_capacity(1 + cb_size + data.len());
    out.write_u8((CMD_DATA << 4) | cb_sel);
    write_variable(&mut out, channel_id, cb_size);
    out.write_slice(data);
    out
}

pub fn encode_data_first(channel_id: u32, total_length: u32, first_chunk: &[u8]) -> Vec<u8> {
    let (cb_sel, cb_size) = field_size_for(channel_id);
    let (len_sel, len_size) = field_size_for(total_length);
    let mut out = Vec::with_capacity(1 + cb_size + len_size + first_chunk.len());
    out.write_u8((CMD_DATA_FIRST << 4) | (len_sel << 2) | cb_sel);
    write_variable(&mut out, channel_id, cb_size);
    write_variable(&mut out, total_length, len_size);
    out.write_slice(first_chunk);
    out
}

/// Splits `payload` into one-or-more DVC-layer PDUs: a single `Data` PDU
/// if it fits under [`MAX_DATA_SIZE`], else a `DataFirst` PDU followed by
/// `Data` continuation PDUs.
pub fn encode_channel_payload(channel_id: u32, payload: &[u8]) -> Vec<Vec<u8>> {
    if payload.len() <= MAX_DATA_SIZE {
        return vec![encode_data(channel_id, payload)];
    }
    let mut chunks = payload.chunks(MAX_DATA_SIZE);
    let first = chunks.next().expect("payload is non-empty since it exceeds MAX_DATA_SIZE");
    let mut frames = vec![encode_data_first(channel_id, payload.len() as u32, first)];
    frames.extend(chunks.map(|chunk| encode_data(channel_id, chunk)));
    frames
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateResponse {
    pub channel_id: u32,
    pub creation_status: u32,
}

impl CreateResponse {
    pub fn is_ok(&self) -> bool {
        self.creation_status == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    CapabilityResponse { version: u16 },
    CreateResponse(CreateResponse),
    Data { channel_id: u32, data: Vec<u8> },
    DataFirst { channel_id: u32, total_length: u32, data: Vec<u8> },
    Close { channel_id: u32 },
    Other,
}

pub fn decode_client_message(input: &[u8]) -> Result<ClientMessage, DecodeError> {
    let mut cursor = ReadCursor::new(input);
    let header = cursor.read_u8()?;
    let cmd = header >> 4;
    let sp = (header >> 2) & 0x03;
    let cb_id = header & 0x03;

    match cmd {
        CMD_CAPABILITY => {
            let _pad = cursor.read_u8()?;
            let version = cursor.read_u16_le()?;
            Ok(ClientMessage::CapabilityResponse { version })
        }
        CMD_CREATE => {
            let channel_id = read_variable(&mut cursor, cb_id)?;
            let creation_status = cursor.read_u32_le()?;
            Ok(ClientMessage::CreateResponse(CreateResponse {
                channel_id,
                creation_status,
            }))
        }
        CMD_DATA => {
            let channel_id = read_variable(&mut cursor, cb_id)?;
            Ok(ClientMessage::Data {
                channel_id,
                data: cursor.read_rest().to_vec(),
            })
        }
        CMD_DATA_FIRST => {
            let channel_id = read_variable(&mut cursor, cb_id)?;
            let total_length = read_variable(&mut cursor, sp)?;
            Ok(ClientMessage::DataFirst {
                channel_id,
                total_length,
                data: cursor.read_rest().to_vec(),
            })
        }
        CMD_CLOSE => {
            let channel_id = read_variable(&mut cursor, cb_id)?;
            Ok(ClientMessage::Close { channel_id })
        }
        _ => Ok(ClientMessage::Other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_request_has_no_channel_id_and_v2_charges() {
        let encoded = encode_capability_request();
        // header(1) + pad(1) + version(2) + 4 charges(2 each) = 12
        assert_eq!(encoded.len(), 12);
        assert_eq!(encoded[0], 0x05 << 4); // cmd=Capability, sp=0, cb_id=0
    }

    #[test]
    fn create_request_round_trips_as_create_response_shape() {
        let encoded = encode_create_request(7, "ECHO");
        assert_eq!(encoded[0], (0x01 << 4)); // cb_id=0 since 7 fits in a u8
        assert_eq!(&encoded[2..6], b"ECHO");
        assert_eq!(encoded[6], 0); // NUL terminator
    }

    #[test]
    fn create_response_decode() {
        let mut raw = Vec::new();
        raw.write_u8(0x01 << 4);
        raw.write_u8(7);
        raw.write_u32_le(0);
        let decoded = decode_client_message(&raw).unwrap();
        assert_eq!(
            decoded,
            ClientMessage::CreateResponse(CreateResponse {
                channel_id: 7,
                creation_status: 0
            })
        );
        let ClientMessage::CreateResponse(cr) = decoded else { unreachable!() };
        assert!(cr.is_ok());
    }

    #[test]
    fn capability_response_decode() {
        let mut raw = Vec::new();
        raw.write_u8(0x05 << 4);
        raw.write_u8(0);
        raw.write_u16_le(2);
        assert_eq!(decode_client_message(&raw).unwrap(), ClientMessage::CapabilityResponse { version: 2 });
    }

    #[test]
    fn close_round_trip() {
        let encoded = encode_close(300); // needs 2-byte channel id
        assert_eq!(encoded[0] & 0x03, 1); // cb_id selector = u16
        let decoded = decode_client_message(&encoded).unwrap();
        assert_eq!(decoded, ClientMessage::Close { channel_id: 300 });
    }

    #[test]
    fn small_payload_encodes_as_single_data_pdu() {
        let frames = encode_channel_payload(1, b"hello");
        assert_eq!(frames.len(), 1);
        let decoded = decode_client_message(&frames[0]).unwrap();
        assert_eq!(
            decoded,
            ClientMessage::Data {
                channel_id: 1,
                data: b"hello".to_vec()
            }
        );
    }

    #[test]
    fn large_payload_splits_into_data_first_plus_continuations() {
        let payload = vec![0xAB; MAX_DATA_SIZE * 2 + 10];
        let frames = encode_channel_payload(1, &payload);
        assert_eq!(frames.len(), 3);

        let ClientMessage::DataFirst { channel_id, total_length, data } = decode_client_message(&frames[0]).unwrap() else {
            panic!("expected DataFirst");
        };
        assert_eq!(channel_id, 1);
        assert_eq!(total_length as usize, payload.len());
        assert_eq!(data.len(), MAX_DATA_SIZE);

        let mut reassembled = data;
        for frame in &frames[1..] {
            let ClientMessage::Data { data, .. } = decode_client_message(frame).unwrap() else {
                panic!("expected Data continuation");
            };
            reassembled.extend(data);
        }
        assert_eq!(reassembled, payload);
    }

    #[test]
    fn data_pdu_channel_id_sizes_selected_correctly() {
        for (channel_id, expected_selector) in [(5u32, 0u8), (1000, 1), (100_000, 2)] {
            let encoded = encode_data(channel_id, b"x");
            assert_eq!(encoded[0] & 0x03, expected_selector, "channel_id {channel_id}");
            let decoded = decode_client_message(&encoded).unwrap();
            assert_eq!(decoded, ClientMessage::Data { channel_id, data: b"x".to_vec() });
        }
    }
}
