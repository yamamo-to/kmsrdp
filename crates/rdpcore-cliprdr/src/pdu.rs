//! MS-RDPECLIP wire format: text formats only (`CF_UNICODETEXT`). Every
//! message shares an 8-byte header (`msgType`, `msgFlags`, `dataLen`, all
//! little-endian) riding as the whole static-channel payload (after SVC
//! chunking, see `rdpcore_pdu::svc`) over `"cliprdr"`.
//!
//! Scope: Clipboard Capabilities (`CB_CLIP_CAPS`, General capability set
//! only), Monitor Ready (`CB_MONITOR_READY`), Format List/Response
//! (`CB_FORMAT_LIST`/`CB_FORMAT_LIST_RESPONSE`, long-format-name variant -
//! this server always advertises `CB_USE_LONG_FORMAT_NAMES`, and every
//! format list this crate builds carries exactly one entry: `CF_UNICODETEXT`
//! with an empty name, the correct minimal encoding for a standard format),
//! and Format Data Request/Response (`CB_FORMAT_DATA_REQUEST`/
//! `CB_FORMAT_DATA_RESPONSE`). File contents and locking messages are
//! neither encoded nor decoded - unrecognized incoming message types are
//! decoded far enough to be safely skipped, never treated as an error.

use rdpcore_pdu::DecodeError;
use rdpcore_pdu::cursor::{ReadCursor, WriteBuf};
use rdpcore_pdu::utf16;

pub const CB_MONITOR_READY: u16 = 0x0001;
pub const CB_FORMAT_LIST: u16 = 0x0002;
pub const CB_FORMAT_LIST_RESPONSE: u16 = 0x0003;
pub const CB_FORMAT_DATA_REQUEST: u16 = 0x0004;
pub const CB_FORMAT_DATA_RESPONSE: u16 = 0x0005;
pub const CB_CLIP_CAPS: u16 = 0x0007;

pub const CB_RESPONSE_OK: u16 = 0x0001;
pub const CB_RESPONSE_FAIL: u16 = 0x0002;

const CAPSTYPE_GENERAL: u16 = 0x0001;
const CB_CAPS_VERSION_2: u32 = 0x0000_0002;
const CB_USE_LONG_FORMAT_NAMES: u32 = 0x0000_0002;

pub const CF_UNICODETEXT: u32 = 13;

pub const CHANNEL_NAME: &str = "cliprdr";

fn write_header(out: &mut Vec<u8>, msg_type: u16, msg_flags: u16, body_len: usize) {
    out.write_u16_le(msg_type);
    out.write_u16_le(msg_flags);
    out.write_u32_le(body_len as u32);
}

pub fn encode_capabilities() -> Vec<u8> {
    let mut body = Vec::new();
    body.write_u16_le(1); // cCapabilitiesSets
    body.write_u16_le(0); // pad1
    body.write_u16_le(CAPSTYPE_GENERAL);
    body.write_u16_le(12); // lengthCapability, wrapper(4) + version(4) + flags(4)
    body.write_u32_le(CB_CAPS_VERSION_2);
    body.write_u32_le(CB_USE_LONG_FORMAT_NAMES);

    let mut out = Vec::with_capacity(body.len() + 8);
    write_header(&mut out, CB_CLIP_CAPS, 0, body.len());
    out.write_slice(&body);
    out
}

pub fn encode_monitor_ready() -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    write_header(&mut out, CB_MONITOR_READY, 0, 0);
    out
}

/// A format list containing just `CF_UNICODETEXT` - the minimal correct
/// encoding for a standard predefined format is `formatId(4) + "\0"(2
/// bytes)`, no name string.
pub fn encode_format_list_unicode_text() -> Vec<u8> {
    let mut body = Vec::with_capacity(6);
    body.write_u32_le(CF_UNICODETEXT);
    body.write_u16_le(0); // empty name: just the UTF-16 NUL terminator

    let mut out = Vec::with_capacity(body.len() + 8);
    write_header(&mut out, CB_FORMAT_LIST, 0, body.len());
    out.write_slice(&body);
    out
}

pub fn encode_format_list_response_ok() -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    write_header(&mut out, CB_FORMAT_LIST_RESPONSE, CB_RESPONSE_OK, 0);
    out
}

pub fn encode_format_data_request(format_id: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    write_header(&mut out, CB_FORMAT_DATA_REQUEST, 0, 4);
    out.write_u32_le(format_id);
    out
}

/// `CF_UNICODETEXT` data is UTF-16LE, terminated with a NUL code unit (Win32
/// convention: "a null character signals the end of the data").
pub fn encode_format_data_response_text(text: &str) -> Vec<u8> {
    let mut body = utf16::encode_units(text);
    body.write_u16_le(0); // trailing NUL terminator

    let mut out = Vec::with_capacity(body.len() + 8);
    write_header(
        &mut out,
        CB_FORMAT_DATA_RESPONSE,
        CB_RESPONSE_OK,
        body.len(),
    );
    out.write_slice(&body);
    out
}

pub fn encode_format_data_response_error() -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    write_header(&mut out, CB_FORMAT_DATA_RESPONSE, CB_RESPONSE_FAIL, 0);
    out
}

/// The format IDs present in an incoming Format List PDU (names are
/// decoded only far enough to skip past them - this server only cares
/// whether `CF_UNICODETEXT` is present).
fn decode_format_list(body: &[u8]) -> Result<Vec<u32>, DecodeError> {
    let mut cursor = ReadCursor::new(body);
    let mut ids = Vec::new();
    while cursor.remaining() >= 6 {
        let id = cursor.read_u32_le()?;
        loop {
            cursor.ensure(2)?;
            if cursor.read_u16_le()? == 0 {
                break; // NUL-terminated UTF-16 name, one code unit at a time
            }
        }
        ids.push(id);
    }
    Ok(ids)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    FormatList(Vec<u32>),
    FormatListResponse,
    FormatDataRequest(u32),
    /// `Ok(text)` for `CB_RESPONSE_OK`, `Err(())` for `CB_RESPONSE_FAIL`.
    FormatDataResponse(Result<String, ()>),
    Other,
}

pub fn decode_client_message(input: &[u8]) -> Result<ClientMessage, DecodeError> {
    let mut cursor = ReadCursor::new(input);
    let msg_type = cursor.read_u16_le()?;
    let msg_flags = cursor.read_u16_le()?;
    let data_len = cursor.read_u32_le()? as usize;
    let body = cursor.read_slice(data_len)?;

    match msg_type {
        CB_FORMAT_LIST => Ok(ClientMessage::FormatList(decode_format_list(body)?)),
        CB_FORMAT_LIST_RESPONSE => Ok(ClientMessage::FormatListResponse),
        CB_FORMAT_DATA_REQUEST => {
            let mut cursor = ReadCursor::new(body);
            Ok(ClientMessage::FormatDataRequest(cursor.read_u32_le()?))
        }
        CB_FORMAT_DATA_RESPONSE => {
            if msg_flags & CB_RESPONSE_FAIL != 0 {
                Ok(ClientMessage::FormatDataResponse(Err(())))
            } else {
                Ok(ClientMessage::FormatDataResponse(Ok(utf16::read_fixed(
                    body,
                ))))
            }
        }
        _ => Ok(ClientMessage::Other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_list_round_trip_minimal_standard_entry() {
        let encoded = encode_format_list_unicode_text();
        assert_eq!(encoded.len(), 8 + 6);
        let decoded = decode_client_message(&encoded).unwrap();
        assert_eq!(decoded, ClientMessage::FormatList(vec![CF_UNICODETEXT]));
    }

    #[test]
    fn format_data_response_text_round_trip() {
        let encoded = encode_format_data_response_text("hello");
        let decoded = decode_client_message(&encoded).unwrap();
        assert_eq!(
            decoded,
            ClientMessage::FormatDataResponse(Ok("hello".to_owned()))
        );
    }

    #[test]
    fn format_data_response_error_round_trip() {
        let encoded = encode_format_data_response_error();
        let decoded = decode_client_message(&encoded).unwrap();
        assert_eq!(decoded, ClientMessage::FormatDataResponse(Err(())));
    }

    #[test]
    fn format_data_request_round_trip() {
        let encoded = encode_format_data_request(CF_UNICODETEXT);
        let decoded = decode_client_message(&encoded).unwrap();
        assert_eq!(decoded, ClientMessage::FormatDataRequest(CF_UNICODETEXT));
    }

    #[test]
    fn format_list_response_is_recognized() {
        let mut out = Vec::new();
        write_header(&mut out, CB_FORMAT_LIST_RESPONSE, CB_RESPONSE_OK, 0);
        assert_eq!(
            decode_client_message(&out).unwrap(),
            ClientMessage::FormatListResponse
        );
    }

    #[test]
    fn unknown_message_types_decode_as_other_without_erroring() {
        let mut out = Vec::new();
        write_header(&mut out, 0x0006, 0, 4); // CB_TEMP_DIRECTORY, not implemented
        out.write_slice(&[0u8; 4]);
        assert_eq!(decode_client_message(&out).unwrap(), ClientMessage::Other);
    }

    #[test]
    fn multi_format_list_finds_unicodetext_among_others() {
        // CF_TEXT(1, no name) then CF_UNICODETEXT(13, no name).
        let mut body = Vec::new();
        body.write_u32_le(1);
        body.write_u16_le(0);
        body.write_u32_le(CF_UNICODETEXT);
        body.write_u16_le(0);
        let mut out = Vec::new();
        write_header(&mut out, CB_FORMAT_LIST, 0, body.len());
        out.extend(body);

        let decoded = decode_client_message(&out).unwrap();
        assert_eq!(decoded, ClientMessage::FormatList(vec![1, CF_UNICODETEXT]));
    }
}
