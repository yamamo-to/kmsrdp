//! MS-RDPEFS (RDPDR) core header and connection-sequence PDUs for
//! filesystem ("drive") and printer devices. Every message shares a
//! 4-byte `RDPDR_HEADER` (Component, PacketId, both u16 LE) riding as the
//! whole static-channel payload (after SVC chunking, see
//! `rdpcore_pdu::svc`) over `"rdpdr"`.
//!
//! Byte layouts and the exact connection-sequence order below were
//! extracted from FreeRDP's own server-side `channels/rdpdr/server/rdpdr_main.c`
//! (read-only reference, no code reuse) - the sequence has two surprises
//! relative to a naive spec skim: the server's "Client ID Confirm" reply is
//! sent *after* the Server Capability Request (both triggered by Client
//! Name Request, not by Client Announce Reply), and a conditional
//! "Server User Logged On" message follows the Client Capability Response.

use rdpcore_pdu::DecodeError;
use rdpcore_pdu::cursor::{ReadCursor, WriteBuf};
use rdpcore_pdu::utf16;

pub const CHANNEL_NAME: &str = "rdpdr";

pub const RDPDR_CTYP_CORE: u16 = 0x4472;
/// The printer-specific control messages (cache data, XPS mode) live
/// under this component instead of `RDPDR_CTYP_CORE` - both are advisory
/// only (the reference server itself just logs and no-ops them), so this
/// crate doesn't need to special-case dispatch on component, only ignore
/// their `PacketId`s safely (which `decode_client_message` already does
/// by falling through to `ClientMessage::Other`).
pub const RDPDR_CTYP_PRN: u16 = 0x5052;

pub const PAKID_CORE_SERVER_ANNOUNCE: u16 = 0x496E;
pub const PAKID_CORE_CLIENTID_CONFIRM: u16 = 0x4343;
pub const PAKID_CORE_CLIENT_NAME: u16 = 0x434E;
pub const PAKID_CORE_DEVICELIST_ANNOUNCE: u16 = 0x4441;
pub const PAKID_CORE_DEVICE_REPLY: u16 = 0x6472;
pub const PAKID_CORE_DEVICE_IOREQUEST: u16 = 0x4952;
pub const PAKID_CORE_DEVICE_IOCOMPLETION: u16 = 0x4943;
pub const PAKID_CORE_SERVER_CAPABILITY: u16 = 0x5350;
pub const PAKID_CORE_CLIENT_CAPABILITY: u16 = 0x4350;
pub const PAKID_CORE_USER_LOGGEDON: u16 = 0x554C;

/// Advisory only (printer font/driver cache, XPS-mode negotiation) - safe
/// to ignore entirely; listed here for documentation, not dispatched on
/// specially (see [`RDPDR_CTYP_PRN`]'s doc comment).
pub const PAKID_PRN_CACHE_DATA: u16 = 0x5043;
pub const PAKID_PRN_USING_XPS: u16 = 0x5543;

const VERSION_MAJOR: u16 = 0x0001;
const VERSION_MINOR: u16 = 0x000C;

const CAP_GENERAL_TYPE: u16 = 0x0001;
const CAP_PRINTER_TYPE: u16 = 0x0002;
const CAP_DRIVE_TYPE: u16 = 0x0004;
const GENERAL_CAPABILITY_VERSION_02: u32 = 0x0000_0002;
const PRINT_CAPABILITY_VERSION_01: u32 = 0x0000_0001;
const DRIVE_CAPABILITY_VERSION_02: u32 = 0x0000_0002;
const CAPABILITY_HEADER_LENGTH: u16 = 8;

const RDPDR_DEVICE_REMOVE_PDUS: u32 = 0x0000_0001;
const RDPDR_CLIENT_DISPLAY_NAME_PDU: u32 = 0x0000_0002;
const RDPDR_USER_LOGGEDON_PDU: u32 = 0x0000_0004;
const ENABLE_ASYNCIO: u32 = 0x0000_0001;

/// MS-RDPEFS device-type values (not present in FreeRDP's own
/// `rdpdr_main.c`/`rdpdr.h` - those files use the symbol without defining
/// it, presumably from a public header not in that checkout; these are
/// the standard MS-RDPEFS 2.2.1.3 values, stable across every RDP
/// implementation).
pub const RDPDR_DTYP_SERIAL: u32 = 0x0000_0001;
pub const RDPDR_DTYP_PARALLEL: u32 = 0x0000_0002;
pub const RDPDR_DTYP_PRINT: u32 = 0x0000_0004;
pub const RDPDR_DTYP_FILESYSTEM: u32 = 0x0000_0008;
pub const RDPDR_DTYP_SMARTCARD: u32 = 0x0000_0020;

pub const STATUS_SUCCESS: u32 = 0x0000_0000;
pub const STATUS_UNSUCCESSFUL: u32 = 0xC000_0001;

fn write_header(out: &mut Vec<u8>, packet_id: u16) {
    out.write_u16_le(RDPDR_CTYP_CORE);
    out.write_u16_le(packet_id);
}

/// First message on the channel, sent unconditionally as soon as it opens
/// - no waiting for anything from the client first.
pub fn encode_server_announce_request(client_id: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    write_header(&mut out, PAKID_CORE_SERVER_ANNOUNCE);
    out.write_u16_le(VERSION_MAJOR);
    out.write_u16_le(VERSION_MINOR);
    out.write_u32_le(client_id);
    out
}

/// Sent by the server after Client Name Request, immediately following the
/// Server Capability Request (not right after Client Announce Reply -
/// that would be the naive-but-wrong assumption).
pub fn encode_client_id_confirm(client_id: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    write_header(&mut out, PAKID_CORE_CLIENTID_CONFIRM);
    out.write_u16_le(VERSION_MAJOR);
    out.write_u16_le(VERSION_MINOR);
    out.write_u32_le(client_id);
    out
}

/// Header-only PDU, sent after the Client Capability Response, gated on
/// having advertised `RDPDR_USER_LOGGEDON_PDU` in `extendedPdu` (this
/// server always does, see [`encode_server_capability_request`]).
pub fn encode_user_logged_on() -> Vec<u8> {
    let mut out = Vec::with_capacity(4);
    write_header(&mut out, PAKID_CORE_USER_LOGGEDON);
    out
}

/// General capability set (36-byte VERSION_02 body, always sent) plus a
/// body-less Drive and/or Printer capability set for whichever device
/// types `supported` (an OR of `RDPDR_DTYP_*`) includes - matches
/// FreeRDP's own server, which gates each capability set's presence on
/// its own `context->supported` bitmask the same way (Port/Smartcard
/// omitted: out of scope for this crate).
pub fn encode_server_capability_request(supported: u32) -> Vec<u8> {
    let mut num_capabilities = 1u16; // General
    if supported & RDPDR_DTYP_FILESYSTEM != 0 {
        num_capabilities += 1;
    }
    if supported & RDPDR_DTYP_PRINT != 0 {
        num_capabilities += 1;
    }

    let mut out = Vec::new();
    write_header(&mut out, PAKID_CORE_SERVER_CAPABILITY);
    out.write_u16_le(num_capabilities);
    out.write_u16_le(0); // padding

    out.write_u16_le(CAP_GENERAL_TYPE);
    out.write_u16_le(CAPABILITY_HEADER_LENGTH + 36);
    out.write_u32_le(GENERAL_CAPABILITY_VERSION_02);
    out.write_u32_le(0); // osType
    out.write_u32_le(0); // osVersion
    out.write_u16_le(VERSION_MAJOR);
    out.write_u16_le(VERSION_MINOR);
    out.write_u32_le(0xFFFF_FFFF); // ioCode1: advertise every IRP_MJ_* op
    out.write_u32_le(0); // ioCode2 (reserved)
    out.write_u32_le(
        RDPDR_DEVICE_REMOVE_PDUS | RDPDR_CLIENT_DISPLAY_NAME_PDU | RDPDR_USER_LOGGEDON_PDU,
    );
    out.write_u32_le(ENABLE_ASYNCIO); // extraFlags1
    out.write_u32_le(0); // extraFlags2
    out.write_u32_le(0); // SpecialTypeDeviceCap

    if supported & RDPDR_DTYP_FILESYSTEM != 0 {
        out.write_u16_le(CAP_DRIVE_TYPE);
        out.write_u16_le(CAPABILITY_HEADER_LENGTH);
        out.write_u32_le(DRIVE_CAPABILITY_VERSION_02);
    }
    if supported & RDPDR_DTYP_PRINT != 0 {
        out.write_u16_le(CAP_PRINTER_TYPE);
        out.write_u16_le(CAPABILITY_HEADER_LENGTH);
        out.write_u32_le(PRINT_CAPABILITY_VERSION_01);
    }

    out
}

pub fn encode_device_reply(device_id: u32, status: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    write_header(&mut out, PAKID_CORE_DEVICE_REPLY);
    out.write_u32_le(device_id);
    out.write_u32_le(status);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAnnounce {
    pub device_type: u32,
    pub device_id: u32,
    /// Raw 8-byte ANSI field, trimmed of trailing NULs - FreeRDP reads it
    /// as a fixed 8-byte block, not necessarily NUL-terminated.
    pub preferred_dos_name: String,
    /// Opaque for filesystem devices (unused); for `RDPDR_DTYP_PRINT` this
    /// is a `DR_PRN_DEVICE_ANNOUNCE` blob - see
    /// [`decode_printer_device_data`]. FreeRDP's own reference server
    /// doesn't parse this either (it forwards the raw bytes to an
    /// application callback), so there's no tested wire layout to check
    /// this crate's best-effort parse against - failures there are
    /// swallowed, never fatal to accepting the device.
    pub device_data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrinterDeviceData {
    pub driver_name: String,
    pub print_name: String,
    pub is_default: bool,
}

const RDPDR_PRINTER_ANNOUNCE_FLAG_ASCII: u32 = 0x0000_0001;
const RDPDR_PRINTER_ANNOUNCE_FLAG_DEFAULTPRINTER: u32 = 0x0000_0002;

/// Best-effort `DR_PRN_DEVICE_ANNOUNCE` parse (MS-RDPEFS 2.2.1.3, printer
/// variant): `Flags(4) + CodePage(4) + PnPNameLen(4) + DriverNameLen(4) +
/// PrintNameLen(4) + CachedFieldsLen(4)` followed by the three name
/// strings (encoding depends on `RDPDR_PRINTER_ANNOUNCE_FLAG_ASCII`).
/// Returns `None` on any malformed input rather than an error - the
/// printer's friendly name is cosmetic, never required to accept the
/// device (see [`DeviceAnnounce::device_data`]'s doc comment for why
/// there's no tested reference to check this layout against).
pub fn decode_printer_device_data(data: &[u8]) -> Option<PrinterDeviceData> {
    let mut cursor = ReadCursor::new(data);
    let flags = cursor.read_u32_le().ok()?;
    let _code_page = cursor.read_u32_le().ok()?;
    let _pnp_name_len = cursor.read_u32_le().ok()? as usize;
    let driver_name_len = cursor.read_u32_le().ok()? as usize;
    let print_name_len = cursor.read_u32_le().ok()? as usize;
    let _cached_fields_len = cursor.read_u32_le().ok()?;
    let ascii = flags & RDPDR_PRINTER_ANNOUNCE_FLAG_ASCII != 0;

    let _pnp_name = decode_printer_string(&mut cursor, _pnp_name_len, ascii)?;
    let driver_name = decode_printer_string(&mut cursor, driver_name_len, ascii)?;
    let print_name = decode_printer_string(&mut cursor, print_name_len, ascii)?;
    Some(PrinterDeviceData {
        driver_name,
        print_name,
        is_default: flags & RDPDR_PRINTER_ANNOUNCE_FLAG_DEFAULTPRINTER != 0,
    })
}

fn decode_printer_string(cursor: &mut ReadCursor, len: usize, ascii: bool) -> Option<String> {
    let bytes = cursor.read_slice(len).ok()?;
    let decoded = if ascii {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        utf16::decode_units(bytes)
    };
    Some(decoded.trim_end_matches('\0').to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    AnnounceReply,
    ClientName,
    ClientCapability,
    DeviceListAnnounce(Vec<DeviceAnnounce>),
    UserLoggedOn,
    /// Header stripped, body left raw - the caller (which remembers what
    /// `MajorFunction` this `CompletionId` corresponds to) decodes the
    /// rest via `crate::irp`.
    DeviceIoCompletion {
        device_id: u32,
        completion_id: u32,
        io_status: u32,
        body: Vec<u8>,
    },
    Other,
}

fn decode_device_list_announce(body: &[u8]) -> Result<Vec<DeviceAnnounce>, DecodeError> {
    let mut cursor = ReadCursor::new(body);
    let device_count = cursor.read_u32_le()?;
    let mut devices = Vec::with_capacity(device_count as usize);
    for _ in 0..device_count {
        let device_type = cursor.read_u32_le()?;
        let device_id = cursor.read_u32_le()?;
        let dos_name_raw = cursor.read_slice(8)?;
        let end = dos_name_raw.iter().position(|&b| b == 0).unwrap_or(8);
        let preferred_dos_name = String::from_utf8_lossy(&dos_name_raw[..end]).into_owned();
        let device_data_length = cursor.read_u32_le()?;
        let device_data = cursor.read_slice(device_data_length as usize)?.to_vec();
        devices.push(DeviceAnnounce {
            device_type,
            device_id,
            preferred_dos_name,
            device_data,
        });
    }
    Ok(devices)
}

pub fn decode_client_message(input: &[u8]) -> Result<ClientMessage, DecodeError> {
    let mut cursor = ReadCursor::new(input);
    let _component = cursor.read_u16_le()?;
    let packet_id = cursor.read_u16_le()?;
    let body = cursor.read_rest();

    match packet_id {
        PAKID_CORE_CLIENTID_CONFIRM => Ok(ClientMessage::AnnounceReply),
        PAKID_CORE_CLIENT_NAME => Ok(ClientMessage::ClientName),
        PAKID_CORE_CLIENT_CAPABILITY => Ok(ClientMessage::ClientCapability),
        PAKID_CORE_DEVICELIST_ANNOUNCE => Ok(ClientMessage::DeviceListAnnounce(
            decode_device_list_announce(body)?,
        )),
        PAKID_CORE_USER_LOGGEDON => Ok(ClientMessage::UserLoggedOn),
        PAKID_CORE_DEVICE_IOCOMPLETION => {
            let mut cursor = ReadCursor::new(body);
            let device_id = cursor.read_u32_le()?;
            let completion_id = cursor.read_u32_le()?;
            let io_status = cursor.read_u32_le()?;
            Ok(ClientMessage::DeviceIoCompletion {
                device_id,
                completion_id,
                io_status,
                body: cursor.read_rest().to_vec(),
            })
        }
        _ => Ok(ClientMessage::Other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdpcore_pdu::cursor::WriteBuf;

    #[test]
    fn server_announce_request_layout() {
        let encoded = encode_server_announce_request(0x1234_5678);
        assert_eq!(encoded.len(), 12);
        assert_eq!(&encoded[0..2], &RDPDR_CTYP_CORE.to_le_bytes());
        assert_eq!(&encoded[2..4], &PAKID_CORE_SERVER_ANNOUNCE.to_le_bytes());
        assert_eq!(&encoded[4..6], &VERSION_MAJOR.to_le_bytes());
        assert_eq!(&encoded[6..8], &VERSION_MINOR.to_le_bytes());
        assert_eq!(&encoded[8..12], &0x1234_5678u32.to_le_bytes());
    }

    #[test]
    fn client_id_confirm_and_user_logged_on_round_trip_component() {
        let confirm = encode_client_id_confirm(42);
        assert_eq!(confirm.len(), 12);
        let logged_on = encode_user_logged_on();
        assert_eq!(logged_on.len(), 4);
        assert_eq!(&logged_on[2..4], &PAKID_CORE_USER_LOGGEDON.to_le_bytes());
    }

    #[test]
    fn server_capability_request_advertises_general_and_drive() {
        let encoded = encode_server_capability_request(RDPDR_DTYP_FILESYSTEM);
        // header(4) + numCapabilities/padding(4) + general(8+36) + drive(8)
        assert_eq!(encoded.len(), 4 + 4 + 44 + 8);
        assert_eq!(&encoded[4..6], &2u16.to_le_bytes()); // numCapabilities
        let general_type = u16::from_le_bytes([encoded[8], encoded[9]]);
        assert_eq!(general_type, CAP_GENERAL_TYPE);
        let drive_offset = 8 + 44;
        let drive_type = u16::from_le_bytes([encoded[drive_offset], encoded[drive_offset + 1]]);
        assert_eq!(drive_type, CAP_DRIVE_TYPE);
    }

    #[test]
    fn server_capability_request_adds_printer_when_supported() {
        let encoded = encode_server_capability_request(RDPDR_DTYP_FILESYSTEM | RDPDR_DTYP_PRINT);
        assert_eq!(encoded.len(), 4 + 4 + 44 + 8 + 8); // + printer capset (header-only, like drive)
        assert_eq!(&encoded[4..6], &3u16.to_le_bytes()); // numCapabilities
        let printer_offset = 8 + 44 + 8;
        let printer_type =
            u16::from_le_bytes([encoded[printer_offset], encoded[printer_offset + 1]]);
        assert_eq!(printer_type, CAP_PRINTER_TYPE);
    }

    #[test]
    fn server_capability_request_general_only_when_nothing_supported() {
        let encoded = encode_server_capability_request(0);
        assert_eq!(encoded.len(), 4 + 4 + 44);
        assert_eq!(&encoded[4..6], &1u16.to_le_bytes());
    }

    #[test]
    fn decodes_printer_device_data_unicode() {
        let mut body = Vec::new();
        body.write_u32_le(RDPDR_PRINTER_ANNOUNCE_FLAG_DEFAULTPRINTER); // Flags (not ASCII)
        body.write_u32_le(0); // CodePage
        let pnp_name = utf16::encode_units("PNPNAME");
        let driver_name = utf16::encode_units("MyDriver");
        let print_name = utf16::encode_units("MyPrinter");
        body.write_u32_le(pnp_name.len() as u32);
        body.write_u32_le(driver_name.len() as u32);
        body.write_u32_le(print_name.len() as u32);
        body.write_u32_le(0); // CachedFieldsLen
        body.write_slice(&pnp_name);
        body.write_slice(&driver_name);
        body.write_slice(&print_name);

        let decoded = decode_printer_device_data(&body).unwrap();
        assert_eq!(decoded.driver_name, "MyDriver");
        assert_eq!(decoded.print_name, "MyPrinter");
        assert!(decoded.is_default);
    }

    #[test]
    fn decodes_printer_device_data_ascii() {
        let mut body = Vec::new();
        body.write_u32_le(RDPDR_PRINTER_ANNOUNCE_FLAG_ASCII);
        body.write_u32_le(0);
        body.write_u32_le(0); // empty PnPName
        body.write_u32_le(9); // "MyDriver\0".len()
        body.write_u32_le(10); // "MyPrinter\0".len()
        body.write_u32_le(0);
        body.write_slice(b"MyDriver\0");
        body.write_slice(b"MyPrinter\0");

        let decoded = decode_printer_device_data(&body).unwrap();
        assert_eq!(decoded.driver_name, "MyDriver");
        assert_eq!(decoded.print_name, "MyPrinter");
        assert!(!decoded.is_default);
    }

    #[test]
    fn printer_device_data_parse_failure_returns_none_not_panic() {
        assert_eq!(decode_printer_device_data(&[0, 1, 2]), None);
    }

    #[test]
    fn device_reply_round_trip_fields() {
        let encoded = encode_device_reply(7, STATUS_SUCCESS);
        assert_eq!(encoded.len(), 12);
        assert_eq!(&encoded[4..8], &7u32.to_le_bytes());
        assert_eq!(&encoded[8..12], &STATUS_SUCCESS.to_le_bytes());
    }

    fn build_device_list_announce(devices: &[(u32, u32, &str)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.write_u32_le(devices.len() as u32);
        for (device_type, device_id, dos_name) in devices {
            body.write_u32_le(*device_type);
            body.write_u32_le(*device_id);
            let mut name_bytes = dos_name.as_bytes().to_vec();
            name_bytes.resize(8, 0);
            body.write_slice(&name_bytes);
            body.write_u32_le(0); // DeviceDataLength
        }
        let mut out = Vec::new();
        write_header(&mut out, PAKID_CORE_DEVICELIST_ANNOUNCE);
        out.write_slice(&body);
        out
    }

    #[test]
    fn decodes_device_list_announce_with_filesystem_device() {
        let wire = build_device_list_announce(&[(RDPDR_DTYP_FILESYSTEM, 1, "share")]);
        let decoded = decode_client_message(&wire).unwrap();
        assert_eq!(
            decoded,
            ClientMessage::DeviceListAnnounce(vec![DeviceAnnounce {
                device_type: RDPDR_DTYP_FILESYSTEM,
                device_id: 1,
                preferred_dos_name: "share".to_owned(),
                device_data: Vec::new(),
            }])
        );
    }

    #[test]
    fn decodes_device_list_announce_with_full_length_dos_name_no_nul() {
        // Exactly 8 ASCII chars, no room for a NUL terminator - the real
        // wire format doesn't guarantee one (fixed 8-byte read).
        let wire = build_device_list_announce(&[(RDPDR_DTYP_FILESYSTEM, 2, "12345678")]);
        let decoded = decode_client_message(&wire).unwrap();
        assert_eq!(
            decoded,
            ClientMessage::DeviceListAnnounce(vec![DeviceAnnounce {
                device_type: RDPDR_DTYP_FILESYSTEM,
                device_id: 2,
                preferred_dos_name: "12345678".to_owned(),
                device_data: Vec::new(),
            }])
        );
    }

    #[test]
    fn decodes_device_io_completion_header_and_leaves_body_raw() {
        let mut out = Vec::new();
        write_header(&mut out, PAKID_CORE_DEVICE_IOCOMPLETION);
        out.write_u32_le(1); // DeviceId
        out.write_u32_le(99); // CompletionId
        out.write_u32_le(STATUS_SUCCESS); // IoStatus
        out.write_slice(&[0xAA, 0xBB]);

        let decoded = decode_client_message(&out).unwrap();
        assert_eq!(
            decoded,
            ClientMessage::DeviceIoCompletion {
                device_id: 1,
                completion_id: 99,
                io_status: STATUS_SUCCESS,
                body: vec![0xAA, 0xBB],
            }
        );
    }

    #[test]
    fn simple_header_only_messages_are_recognized() {
        let mut announce_reply = Vec::new();
        write_header(&mut announce_reply, PAKID_CORE_CLIENTID_CONFIRM);
        assert_eq!(
            decode_client_message(&announce_reply).unwrap(),
            ClientMessage::AnnounceReply
        );

        let mut client_name = Vec::new();
        write_header(&mut client_name, PAKID_CORE_CLIENT_NAME);
        assert_eq!(
            decode_client_message(&client_name).unwrap(),
            ClientMessage::ClientName
        );

        let mut client_cap = Vec::new();
        write_header(&mut client_cap, PAKID_CORE_CLIENT_CAPABILITY);
        assert_eq!(
            decode_client_message(&client_cap).unwrap(),
            ClientMessage::ClientCapability
        );

        let mut user_logged_on = Vec::new();
        write_header(&mut user_logged_on, PAKID_CORE_USER_LOGGEDON);
        assert_eq!(
            decode_client_message(&user_logged_on).unwrap(),
            ClientMessage::UserLoggedOn
        );
    }

    #[test]
    fn unknown_packet_id_decodes_as_other() {
        let mut out = Vec::new();
        write_header(&mut out, 0xFFFF);
        assert_eq!(decode_client_message(&out).unwrap(), ClientMessage::Other);
    }
}
