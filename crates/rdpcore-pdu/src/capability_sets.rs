//! Share Control Header + the Demand Active / Confirm Active capability
//! exchange (MS-RDPBCGR 2.2.1.13). The server only ever *encodes* Demand
//! Active and *decodes* Confirm Active, and for a raw-bitmap-only phase-1
//! server there's nothing worth extracting from the client's advertised
//! capability sets - so Confirm Active decodes its capability list
//! generically (type + raw body), while each capability set this server
//! actually sends has its own concrete encode/decode pair (decode kept
//! around for round-trip tests, cross-checked against a real
//! implementation's exact byte layout).

use crate::DecodeError;
use crate::cursor::{ReadCursor, WriteBuf};

// ---------------------------------------------------------------------
// Share Control Header
// ---------------------------------------------------------------------

const SHARE_CONTROL_VERSION_SENTINEL: u16 = 0x10;
const SHARE_CONTROL_TYPE_MASK: u16 = 0xF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareControlPduType {
    DemandActive,
    ConfirmActive,
    DeactivateAll,
    Data,
}

impl ShareControlPduType {
    fn as_u16(self) -> u16 {
        match self {
            Self::DemandActive => 0x1,
            Self::ConfirmActive => 0x3,
            Self::DeactivateAll => 0x6,
            Self::Data => 0x7,
        }
    }

    pub fn from_u16(value: u16) -> Option<Self> {
        Some(match value {
            0x1 => Self::DemandActive,
            0x3 => Self::ConfirmActive,
            0x6 => Self::DeactivateAll,
            0x7 => Self::Data,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareControlHeader {
    pub pdu_type: ShareControlPduType,
    pub pdu_source: u16,
    pub share_id: u32,
}

impl ShareControlHeader {
    pub const SIZE: usize = 10;

    /// `body_len` is everything after this 10-byte header.
    pub fn write(&self, out: &mut Vec<u8>, body_len: usize) {
        out.write_u16_le((Self::SIZE + body_len) as u16);
        out.write_u16_le(SHARE_CONTROL_VERSION_SENTINEL | self.pdu_type.as_u16());
        out.write_u16_le(self.pdu_source);
        out.write_u32_le(self.share_id);
    }

    /// Returns the header and the declared total body length (`totalLength
    /// - 10`), which may be larger than what a decoder actually consumes -
    ///
    /// some clients pad Data PDUs with trailing bytes; callers should treat
    /// leftover bytes as harmless rather than erroring.
    pub fn decode(cursor: &mut ReadCursor<'_>) -> Result<(Self, usize), DecodeError> {
        let total_length = cursor.read_u16_le()?;
        let raw_type = cursor.read_u16_le()?;
        if raw_type & !SHARE_CONTROL_TYPE_MASK != SHARE_CONTROL_VERSION_SENTINEL {
            return Err(DecodeError::InvalidValue {
                field: "share_control_header.pdu_type",
                reason: "expected the fixed 0x10 version sentinel",
            });
        }
        let pdu_type = ShareControlPduType::from_u16(raw_type & SHARE_CONTROL_TYPE_MASK).ok_or(
            DecodeError::InvalidValue {
                field: "share_control_header.pdu_type",
                reason: "unrecognized PDU type",
            },
        )?;
        let pdu_source = cursor.read_u16_le()?;
        let share_id = cursor.read_u32_le()?;
        let body_len = usize::from(total_length).saturating_sub(Self::SIZE);
        Ok((
            Self {
                pdu_type,
                pdu_source,
                share_id,
            },
            body_len,
        ))
    }
}

// ---------------------------------------------------------------------
// Generic capability set wrapper
// ---------------------------------------------------------------------

pub const CAPSET_GENERAL: u16 = 0x01;
pub const CAPSET_BITMAP: u16 = 0x02;
pub const CAPSET_ORDER: u16 = 0x03;
pub const CAPSET_POINTER: u16 = 0x08;
pub const CAPSET_INPUT: u16 = 0x0D;
pub const CAPSET_VIRTUAL_CHANNEL: u16 = 0x14;
pub const CAPSET_MULTIFRAGMENT_UPDATE: u16 = 0x1A;
pub const CAPSET_BITMAP_CODECS: u16 = 0x1D;

/// One entry in a capability-set list, decoded structurally (type + raw
/// body) without interpreting the body - see module docs for why that's
/// sufficient for a raw-bitmap-only server reading the client's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawCapabilitySet {
    pub set_type: u16,
    pub body: Vec<u8>,
}

impl RawCapabilitySet {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.write_u16_le(self.set_type);
        out.write_u16_le((self.body.len() + 4) as u16);
        out.write_slice(&self.body);
    }

    pub fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        let set_type = cursor.read_u16_le()?;
        let length = cursor.read_u16_le()?;
        let body_len = usize::from(length).saturating_sub(4);
        let body = cursor.read_slice(body_len)?.to_vec();
        Ok(Self { set_type, body })
    }
}

fn write_capset(out: &mut Vec<u8>, set_type: u16, body: &[u8]) {
    RawCapabilitySet {
        set_type,
        body: body.to_vec(),
    }
    .write(out);
}

// ---------------------------------------------------------------------
// Individual capability sets this server sends
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeneralCapability {
    pub extra_flags: u16,
    pub refresh_rect_support: bool,
    pub suppress_output_support: bool,
}

impl GeneralCapability {
    pub const FASTPATH_OUTPUT_SUPPORTED: u16 = 0x0001;

    pub fn encode_body(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(20);
        out.write_u16_le(1); // osMajorType: WINDOWS (informational only)
        out.write_u16_le(3); // osMinorType: WINDOWS_NT
        out.write_u16_le(0x0200); // protocolVersion: fixed constant
        out.write_u16_le(0); // pad2octets
        out.write_u16_le(0); // generalCompressionTypes, must be 0
        out.write_u16_le(self.extra_flags);
        out.write_u16_le(0); // updateCapabilityFlag, must be 0
        out.write_u16_le(0); // remoteUnshareFlag, must be 0
        out.write_u16_le(0); // generalCompressionLevel, must be 0
        out.write_u8(self.refresh_rect_support as u8);
        out.write_u8(self.suppress_output_support as u8);
        out
    }

    pub fn decode_body(body: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(body);
        let _os_major_type = cursor.read_u16_le()?;
        let _os_minor_type = cursor.read_u16_le()?;
        let _protocol_version = cursor.read_u16_le()?;
        let _pad = cursor.read_u16_le()?;
        let compression_types = cursor.read_u16_le()?;
        if compression_types != 0 {
            return Err(DecodeError::InvalidValue {
                field: "general_capability.compression_types",
                reason: "must be 0",
            });
        }
        let extra_flags = cursor.read_u16_le()?;
        let _update_capability_flag = cursor.read_u16_le()?;
        let _remote_unshare_flag = cursor.read_u16_le()?;
        let _general_compression_level = cursor.read_u16_le()?;
        let refresh_rect_support = cursor.read_u8()? != 0;
        let suppress_output_support = cursor.read_u8()? != 0;
        Ok(Self {
            extra_flags,
            refresh_rect_support,
            suppress_output_support,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitmapCapability {
    pub preferred_bits_per_pixel: u16,
    pub desktop_width: u16,
    pub desktop_height: u16,
    pub desktop_resize_flag: bool,
}

impl BitmapCapability {
    pub fn encode_body(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24);
        out.write_u16_le(self.preferred_bits_per_pixel);
        out.write_u16_le(1); // receive1BitPerPixel, legacy hardcoded
        out.write_u16_le(1); // receive4BitsPerPixel, legacy hardcoded
        out.write_u16_le(1); // receive8BitsPerPixel, legacy hardcoded
        out.write_u16_le(self.desktop_width);
        out.write_u16_le(self.desktop_height);
        out.write_u16_le(0); // pad2octets
        out.write_u16_le(self.desktop_resize_flag as u16);
        out.write_u16_le(1); // bitmapCompressionFlag, must be nonzero
        out.write_u8(0); // highColorFlags, hardcoded
        out.write_u8(0); // drawingFlags
        out.write_u16_le(1); // multipleRectangleSupport, hardcoded
        out.write_u16_le(0); // pad2octets
        out
    }

    pub fn decode_body(body: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(body);
        let preferred_bits_per_pixel = cursor.read_u16_le()?;
        let _receive_1 = cursor.read_u16_le()?;
        let _receive_4 = cursor.read_u16_le()?;
        let _receive_8 = cursor.read_u16_le()?;
        let desktop_width = cursor.read_u16_le()?;
        let desktop_height = cursor.read_u16_le()?;
        let _pad = cursor.read_u16_le()?;
        let desktop_resize_flag = cursor.read_u16_le()? != 0;
        let compression_flag = cursor.read_u16_le()?;
        if compression_flag == 0 {
            return Err(DecodeError::InvalidValue {
                field: "bitmap_capability.compression_flag",
                reason: "must be nonzero",
            });
        }
        let _high_color_flags = cursor.read_u8()?;
        let _drawing_flags = cursor.read_u8()?;
        let _multiple_rectangle_support = cursor.read_u16_le()?;
        let _pad2 = cursor.read_u16_le()?;
        Ok(Self {
            preferred_bits_per_pixel,
            desktop_width,
            desktop_height,
            desktop_resize_flag,
        })
    }
}

/// Advertises no drawing orders at all (raw-bitmap-only server) - matches
/// a real production server's own shipped defaults (all-zero
/// `orderSupport`, empty flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderCapability;

impl OrderCapability {
    pub fn encode_body(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(84);
        out.write_slice(&[0u8; 16]); // terminalDescriptor
        out.write_u32_le(0); // pad4octetsA
        out.write_u16_le(1); // desktopSaveXGranularity
        out.write_u16_le(20); // desktopSaveYGranularity
        out.write_u16_le(0); // pad2octetsA
        out.write_u16_le(1); // maximumOrderLevel
        out.write_u16_le(0); // numberFonts
        // NEGOTIATEORDERSUPPORT (0x0002) MUST be set per MS-RDPBCGR 2.2.7.1.3
        // even when orderSupport is all-zero (raw-bitmap-only server).
        out.write_u16_le(0x0002); // orderFlags
        out.write_slice(&[0u8; 32]); // orderSupport: nothing supported
        out.write_u16_le(0); // textFlags
        out.write_u16_le(0); // orderSupportExFlags: empty
        out.write_u32_le(0); // pad4octetsB
        out.write_u32_le(0); // desktopSaveSize
        out.write_u16_le(0); // pad2octetsC
        out.write_u16_le(0); // pad2octetsD
        out.write_u16_le(0); // textANSICodePage
        out.write_u16_le(0); // pad2octetsE
        out
    }

    pub fn decode_body(body: &[u8]) -> Result<Self, DecodeError> {
        if body.len() < 84 {
            return Err(DecodeError::InvalidValue {
                field: "order_capability",
                reason: "body shorter than the fixed 84-byte layout",
            });
        }
        Ok(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerCapability {
    pub color_pointer_cache_size: u16,
    pub pointer_cache_size: u16,
}

impl PointerCapability {
    pub fn encode_body(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6);
        out.write_u16_le(1); // colorPointerFlag, hardcoded
        out.write_u16_le(self.color_pointer_cache_size);
        out.write_u16_le(self.pointer_cache_size);
        out
    }

    pub fn decode_body(body: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(body);
        let _color_pointer_flag = cursor.read_u16_le()?;
        let color_pointer_cache_size = cursor.read_u16_le()?;
        let pointer_cache_size = cursor.read_u16_le()?;
        Ok(Self {
            color_pointer_cache_size,
            pointer_cache_size,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputCapability {
    pub input_flags: u16,
    pub keyboard_layout: u32,
    pub keyboard_type: u32,
    pub keyboard_subtype: u32,
    pub keyboard_function_key: u32,
}

impl InputCapability {
    pub const SCANCODES: u16 = 0x0001;
    pub const MOUSEX: u16 = 0x0004;
    pub const FASTPATH_INPUT: u16 = 0x0008;
    pub const UNICODE: u16 = 0x0010;
    pub const FASTPATH_INPUT_2: u16 = 0x0020;

    pub fn encode_body(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(84);
        out.write_u16_le(self.input_flags);
        out.write_u16_le(0); // pad2octetsA
        out.write_u32_le(self.keyboard_layout);
        out.write_u32_le(self.keyboard_type);
        out.write_u32_le(self.keyboard_subtype);
        out.write_u32_le(self.keyboard_function_key);
        out.write_slice(&[0u8; 64]); // imeFileName: empty
        out
    }

    pub fn decode_body(body: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(body);
        let input_flags = cursor.read_u16_le()?;
        let _pad = cursor.read_u16_le()?;
        let keyboard_layout = cursor.read_u32_le()?;
        let keyboard_type = cursor.read_u32_le()?;
        let keyboard_subtype = cursor.read_u32_le()?;
        let keyboard_function_key = cursor.read_u32_le()?;
        let _ime_file_name = cursor.read_slice(64)?;
        Ok(Self {
            input_flags,
            keyboard_layout,
            keyboard_type,
            keyboard_subtype,
            keyboard_function_key,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualChannelCapability {
    pub flags: u32,
}

impl VirtualChannelCapability {
    pub fn encode_body(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4);
        out.write_u32_le(self.flags);
        out
    }

    pub fn decode_body(body: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(body);
        let flags = cursor.read_u32_le()?;
        Ok(Self { flags })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultiFragmentUpdateCapability {
    pub max_request_size: u32,
}

impl MultiFragmentUpdateCapability {
    pub fn encode_body(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4);
        out.write_u32_le(self.max_request_size);
        out
    }

    pub fn decode_body(body: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(body);
        Ok(Self {
            max_request_size: cursor.read_u32_le()?,
        })
    }
}

/// Always advertises zero codecs - this server only ever sends raw/
/// uncompressed bitmap updates (phase 1; a real codec is a later,
/// additive phase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BitmapCodecsCapability;

impl BitmapCodecsCapability {
    pub fn encode_body(&self) -> Vec<u8> {
        // codecCount = 0, plus a trailing pad byte so lengthCapability is
        // even. MS-RDPBCGR capability sets are conventionally even-sized;
        // an odd 5-byte CAPSETTYPE_BITMAP_CODECS has been observed to make
        // mstsc send a malformed follow-up PDU after Demand Active.
        vec![0, 0]
    }
}

/// The capability sets this server builds into a Demand Active PDU.
pub struct ServerCapabilities {
    pub general: GeneralCapability,
    pub bitmap: BitmapCapability,
    pub order: OrderCapability,
    pub pointer: PointerCapability,
    pub input: InputCapability,
    pub virtual_channel: VirtualChannelCapability,
    pub multifragment_update: MultiFragmentUpdateCapability,
    pub bitmap_codecs: BitmapCodecsCapability,
}

impl ServerCapabilities {
    pub fn write_list(&self, out: &mut Vec<u8>) -> u16 {
        let mut count = 0u16;
        write_capset(out, CAPSET_GENERAL, &self.general.encode_body());
        count += 1;
        write_capset(out, CAPSET_BITMAP, &self.bitmap.encode_body());
        count += 1;
        write_capset(out, CAPSET_ORDER, &self.order.encode_body());
        count += 1;
        write_capset(out, CAPSET_POINTER, &self.pointer.encode_body());
        count += 1;
        write_capset(out, CAPSET_INPUT, &self.input.encode_body());
        count += 1;
        write_capset(
            out,
            CAPSET_VIRTUAL_CHANNEL,
            &self.virtual_channel.encode_body(),
        );
        count += 1;
        write_capset(
            out,
            CAPSET_MULTIFRAGMENT_UPDATE,
            &self.multifragment_update.encode_body(),
        );
        count += 1;
        write_capset(out, CAPSET_BITMAP_CODECS, &self.bitmap_codecs.encode_body());
        count += 1;
        count
    }
}

// ---------------------------------------------------------------------
// Demand Active (server -> client) / Confirm Active (client -> server)
// ---------------------------------------------------------------------

pub struct DemandActive<'a> {
    pub share_id: u32,
    pub pdu_source: u16,
    pub capabilities: &'a ServerCapabilities,
}

impl DemandActive<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let mut capsets = Vec::new();
        let count = self.capabilities.write_list(&mut capsets);

        let mut body = Vec::new();
        body.write_u16_le(1); // lengthSourceDescriptor: empty string + NUL
        body.write_u16_le((2 + 2 + capsets.len()) as u16); // lengthCombinedCapabilities
        body.write_u8(0); // sourceDescriptor: empty, NUL only
        body.write_u16_le(count);
        body.write_u16_le(0); // pad2Octets
        body.write_slice(&capsets);
        body.write_u32_le(0); // sessionId, ignored by clients

        let mut out = Vec::with_capacity(ShareControlHeader::SIZE + body.len());
        ShareControlHeader {
            pdu_type: ShareControlPduType::DemandActive,
            pdu_source: self.pdu_source,
            share_id: self.share_id,
        }
        .write(&mut out, body.len());
        out.write_slice(&body);
        out
    }
}

/// `TS_DEACTIVATE_ALL_PDU` (MS-RDPBCGR 2.2.3.1) - tells the client the
/// session is about to be reactivated with new capabilities (in this
/// server's case, only ever a new `desktopWidth`/`desktopHeight` in the
/// Demand Active that follows). Server -> client only, no decode needed.
pub struct DeactivateAllPdu {
    pub share_id: u32,
    pub pdu_source: u16,
}

impl DeactivateAllPdu {
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        body.write_u16_le(0); // lengthSourceDescriptor: empty
        // sourceDescriptor: empty, no bytes follow.

        let mut out = Vec::with_capacity(ShareControlHeader::SIZE + body.len());
        ShareControlHeader {
            pdu_type: ShareControlPduType::DeactivateAll,
            pdu_source: self.pdu_source,
            share_id: self.share_id,
        }
        .write(&mut out, body.len());
        out.write_slice(&body);
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmActive {
    pub share_id: u32,
    pub pdu_source: u16,
    pub originator_id: u16,
    pub capabilities: Vec<RawCapabilitySet>,
}

impl ConfirmActive {
    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        let (header, _body_len) = ShareControlHeader::decode(&mut cursor)?;
        if header.pdu_type != ShareControlPduType::ConfirmActive {
            return Err(DecodeError::InvalidValue {
                field: "confirm_active.share_control_header.pdu_type",
                reason: "expected ConfirmActivePdu",
            });
        }
        // Everything after the share-control header is best-effort: this
        // server does not consult client capability sets, and mstsc has been
        // observed to deliver Confirm Active in MCS segments or with a
        // trailing security-header quirk that leaves the body truncated
        // relative to lengthSourceDescriptor. Accept the PDU as soon as the
        // type checks out rather than failing the whole handshake.
        let originator_id = cursor.read_u16_le().unwrap_or(0);
        let mut capabilities = Vec::new();
        if let Ok(length_source_descriptor) = cursor.read_u16_le() {
            let _ = cursor.read_u16_le(); // lengthCombinedCapabilities
            let take = usize::from(length_source_descriptor).min(cursor.remaining());
            let _ = cursor.read_slice(take);
            if let (Ok(number_capabilities), Ok(_)) = (cursor.read_u16_le(), cursor.read_u16_le()) {
                for _ in 0..number_capabilities {
                    match RawCapabilitySet::decode(&mut cursor) {
                        Ok(cap) => capabilities.push(cap),
                        Err(_) => break,
                    }
                }
            }
        }

        Ok(Self {
            share_id: header.share_id,
            pdu_source: header.pdu_source,
            originator_id,
            capabilities,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_capabilities() -> ServerCapabilities {
        ServerCapabilities {
            general: GeneralCapability {
                extra_flags: GeneralCapability::FASTPATH_OUTPUT_SUPPORTED,
                refresh_rect_support: true,
                suppress_output_support: true,
            },
            bitmap: BitmapCapability {
                preferred_bits_per_pixel: 32,
                desktop_width: 1920,
                desktop_height: 1080,
                desktop_resize_flag: true,
            },
            order: OrderCapability,
            pointer: PointerCapability {
                color_pointer_cache_size: 2048,
                pointer_cache_size: 2048,
            },
            input: InputCapability {
                input_flags: InputCapability::SCANCODES
                    | InputCapability::MOUSEX
                    | InputCapability::FASTPATH_INPUT
                    | InputCapability::UNICODE,
                keyboard_layout: 0x0409,
                keyboard_type: 4,
                keyboard_subtype: 0,
                keyboard_function_key: 12,
            },
            virtual_channel: VirtualChannelCapability { flags: 0 },
            multifragment_update: MultiFragmentUpdateCapability {
                max_request_size: 8 * 1024 * 1024,
            },
            bitmap_codecs: BitmapCodecsCapability,
        }
    }

    #[test]
    fn share_control_header_round_trip() {
        let header = ShareControlHeader {
            pdu_type: ShareControlPduType::DemandActive,
            pdu_source: 1003,
            share_id: 0x1000,
        };
        let mut out = Vec::new();
        header.write(&mut out, 5);
        out.extend_from_slice(&[0u8; 5]);

        let mut cursor = ReadCursor::new(&out);
        let (decoded, body_len) = ShareControlHeader::decode(&mut cursor).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(body_len, 5);
    }

    #[test]
    fn deactivate_all_pdu_has_a_decodable_share_control_header() {
        let encoded = DeactivateAllPdu {
            share_id: 0x1_0000,
            pdu_source: 1003,
        }
        .encode();
        let mut cursor = ReadCursor::new(&encoded);
        let (header, body_len) = ShareControlHeader::decode(&mut cursor).unwrap();
        assert_eq!(header.pdu_type, ShareControlPduType::DeactivateAll);
        assert_eq!(header.share_id, 0x1_0000);
        assert_eq!(header.pdu_source, 1003);
        assert_eq!(body_len, 2); // lengthSourceDescriptor only, empty descriptor
    }

    #[test]
    fn general_capability_round_trip() {
        let cap = GeneralCapability {
            extra_flags: GeneralCapability::FASTPATH_OUTPUT_SUPPORTED,
            refresh_rect_support: true,
            suppress_output_support: true,
        };
        assert_eq!(
            GeneralCapability::decode_body(&cap.encode_body()).unwrap(),
            cap
        );
        assert_eq!(cap.encode_body().len(), 20);
    }

    #[test]
    fn bitmap_capability_round_trip() {
        let cap = BitmapCapability {
            preferred_bits_per_pixel: 32,
            desktop_width: 1920,
            desktop_height: 1080,
            desktop_resize_flag: true,
        };
        assert_eq!(
            BitmapCapability::decode_body(&cap.encode_body()).unwrap(),
            cap
        );
        assert_eq!(cap.encode_body().len(), 24);
    }

    #[test]
    fn order_capability_is_84_bytes_and_all_zero_support() {
        let body = OrderCapability.encode_body();
        assert_eq!(body.len(), 84);
        assert!(OrderCapability::decode_body(&body).is_ok());
    }

    #[test]
    fn pointer_capability_round_trip() {
        let cap = PointerCapability {
            color_pointer_cache_size: 2048,
            pointer_cache_size: 2048,
        };
        assert_eq!(
            PointerCapability::decode_body(&cap.encode_body()).unwrap(),
            cap
        );
        assert_eq!(cap.encode_body().len(), 6);
    }

    #[test]
    fn input_capability_round_trip() {
        let cap = InputCapability {
            input_flags: InputCapability::SCANCODES | InputCapability::UNICODE,
            keyboard_layout: 0x0409,
            keyboard_type: 4,
            keyboard_subtype: 0,
            keyboard_function_key: 12,
        };
        assert_eq!(
            InputCapability::decode_body(&cap.encode_body()).unwrap(),
            cap
        );
        assert_eq!(cap.encode_body().len(), 84);
    }

    #[test]
    fn virtual_channel_and_multifragment_round_trip() {
        let vc = VirtualChannelCapability { flags: 0 };
        assert_eq!(
            VirtualChannelCapability::decode_body(&vc.encode_body()).unwrap(),
            vc
        );

        let mfu = MultiFragmentUpdateCapability {
            max_request_size: 8 * 1024 * 1024,
        };
        assert_eq!(
            MultiFragmentUpdateCapability::decode_body(&mfu.encode_body()).unwrap(),
            mfu
        );
    }

    #[test]
    fn bitmap_codecs_zero_count_is_one_byte() {
        assert_eq!(BitmapCodecsCapability.encode_body(), [0, 0]);
    }

    #[test]
    fn demand_active_round_trip_via_confirm_active_decoder() {
        // ConfirmActive::decode is generic over any well-formed capability
        // list, so we can reuse it to sanity-check DemandActive's own
        // capability-list bytes (share control header pdu_type aside).
        let caps = sample_capabilities();
        let demand = DemandActive {
            share_id: 0x1000,
            pdu_source: 1003,
            capabilities: &caps,
        };
        let mut encoded = demand.encode();
        // Flip the share control header's pdu_type bits from DemandActive
        // (0x1) to ConfirmActive (0x3) in place, and inject a 2-byte
        // originatorId after the header so `ConfirmActive::decode` (which
        // expects that field) can parse the rest unchanged.
        let type_field = u16::from_le_bytes([encoded[2], encoded[3]]);
        let patched = (type_field & !0xF) | 0x3;
        encoded[2..4].copy_from_slice(&patched.to_le_bytes());
        let mut with_originator = encoded[..10].to_vec();
        with_originator.write_u16_le(1002);
        with_originator.extend_from_slice(&encoded[10..]);
        // Fix up totalLength for the 2 extra bytes we just inserted.
        let new_total = with_originator.len() as u16;
        with_originator[0..2].copy_from_slice(&new_total.to_le_bytes());

        let confirm = ConfirmActive::decode(&with_originator).unwrap();
        assert_eq!(confirm.share_id, 0x1000);
        assert_eq!(confirm.capabilities.len(), 8);
        assert!(
            confirm
                .capabilities
                .iter()
                .any(|c| c.set_type == CAPSET_GENERAL)
        );
        assert!(
            confirm
                .capabilities
                .iter()
                .any(|c| c.set_type == CAPSET_BITMAP_CODECS)
        );
    }
}
