//! MS-RDPEGFX wire format: the subset needed for AVC420 full-frame streaming.
//!
//! Server-to-client messages are wrapped in [`encode_segmented_single`]
//! (`RDP_SEGMENTED_DATA` with uncompressed `RDP8_BULK_ENCODED_DATA`).
//! Client-to-server messages arrive as raw GFX PDUs (no segmented wrapper).

use rdpcore_pdu::DecodeError;
use rdpcore_pdu::cursor::{ReadCursor, WriteBuf};

pub const CHANNEL_NAME: &str = "Microsoft::Windows::RDS::Graphics";

const CMD_WIRE_TO_SURFACE_1: u16 = 0x0001;
const CMD_CREATE_SURFACE: u16 = 0x0009;
const CMD_DELETE_SURFACE: u16 = 0x000a;
const CMD_START_FRAME: u16 = 0x000b;
const CMD_END_FRAME: u16 = 0x000c;
const CMD_FRAME_ACKNOWLEDGE: u16 = 0x000d;
const CMD_RESET_GRAPHICS: u16 = 0x000e;
const CMD_MAP_SURFACE_TO_OUTPUT: u16 = 0x000f;
const CMD_CACHE_IMPORT_OFFER: u16 = 0x0010;
const CMD_CAPS_ADVERTISE: u16 = 0x0012;
const CMD_CAPS_CONFIRM: u16 = 0x0013;

pub const CODEC_AVC420: u16 = 0x000b;
pub const PIXEL_FORMAT_XRGB: u8 = 0x20;

pub const CAP_VERSION_8: u32 = 0x0008_0004;
pub const CAP_VERSION_81: u32 = 0x0008_0005;
pub const CAP_VERSION_10: u32 = 0x000a_0002;
pub const CAP_VERSION_101: u32 = 0x000a_0003;
pub const CAP_VERSION_102: u32 = 0x000a_0004;
pub const CAP_VERSION_103: u32 = 0x000a_0005;
pub const CAP_VERSION_104: u32 = 0x000a_0006;
pub const CAP_VERSION_105: u32 = 0x000a_0007;
pub const CAP_VERSION_106: u32 = 0x000a_0600;
pub const CAP_VERSION_106_ERR: u32 = 0x000a_0601;
pub const CAP_VERSION_107: u32 = 0x000a_0701;

pub const CAPS_FLAG_SMALL_CACHE: u32 = 0x02;
pub const CAPS_FLAG_AVC420_ENABLED: u32 = 0x10;
pub const CAPS_FLAG_AVC_DISABLED: u32 = 0x20;

const SEGMENTED_SINGLE: u8 = 0xe0;
const PACKET_COMPR_TYPE_RDP8: u8 = 0x04;

/// Total encoded size of `RDPGFX_RESET_GRAPHICS_PDU` including header (MS-RDPEGFX).
const RESET_GRAPHICS_TOTAL_SIZE: usize = 340;
const GFX_HEADER_SIZE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect16 {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

impl Rect16 {
    fn encode(&self, out: &mut Vec<u8>) {
        out.write_u16_le(self.left);
        out.write_u16_le(self.top);
        out.write_u16_le(self.right);
        out.write_u16_le(self.bottom);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorDef {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub primary: bool,
}

impl MonitorDef {
    fn encode(&self, out: &mut Vec<u8>) {
        out.write_i32_le(self.left);
        out.write_i32_le(self.top);
        out.write_i32_le(self.right);
        out.write_i32_le(self.bottom);
        out.write_u32_le(if self.primary { 1 } else { 0 });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawCapabilitySet {
    pub version: u32,
    pub data: Vec<u8>,
}

impl RawCapabilitySet {
    pub fn flags_only(version: u32, flags: u32) -> Self {
        Self {
            version,
            data: flags.to_le_bytes().to_vec(),
        }
    }

    pub fn flags(&self) -> u32 {
        if self.data.len() >= 4 {
            u32::from_le_bytes([self.data[0], self.data[1], self.data[2], self.data[3]])
        } else {
            0
        }
    }

    fn encode_body(&self, out: &mut Vec<u8>) {
        out.write_u32_le(self.version);
        out.write_u32_le(self.data.len() as u32);
        out.write_slice(&self.data);
    }

    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        let version = cursor.read_u32_le()?;
        let len = cursor.read_u32_le()? as usize;
        let data = cursor.read_slice(len)?.to_vec();
        Ok(Self { version, data })
    }

    /// True when this capability set permits AVC420 encoding.
    pub fn supports_avc420(&self) -> bool {
        match self.version {
            CAP_VERSION_81 => self.flags() & CAPS_FLAG_AVC420_ENABLED != 0,
            CAP_VERSION_10 | CAP_VERSION_101 | CAP_VERSION_102 | CAP_VERSION_103
            | CAP_VERSION_104 | CAP_VERSION_105 | CAP_VERSION_106 | CAP_VERSION_106_ERR
            | CAP_VERSION_107 => self.flags() & CAPS_FLAG_AVC_DISABLED == 0,
            _ => false,
        }
    }
}

/// Preference order when selecting among client-advertised sets (highest first).
fn cap_rank(version: u32) -> u8 {
    match version {
        CAP_VERSION_107 => 11,
        CAP_VERSION_106 | CAP_VERSION_106_ERR => 10,
        CAP_VERSION_105 => 9,
        CAP_VERSION_104 => 8,
        CAP_VERSION_103 => 7,
        CAP_VERSION_102 => 6,
        CAP_VERSION_101 => 5,
        CAP_VERSION_10 => 4,
        CAP_VERSION_81 => 3,
        CAP_VERSION_8 => 1,
        _ => 0,
    }
}

/// Pick the best client capability set that supports AVC420.
pub fn select_avc420_capability(sets: &[RawCapabilitySet]) -> Option<RawCapabilitySet> {
    sets.iter()
        .filter(|s| s.supports_avc420())
        .max_by_key(|s| cap_rank(s.version))
        .cloned()
}

fn write_header(out: &mut Vec<u8>, cmd_id: u16, body_len: usize) {
    let pdu_length = (GFX_HEADER_SIZE + body_len) as u32;
    out.write_u16_le(cmd_id);
    out.write_u16_le(0); // flags
    out.write_u32_le(pdu_length);
}

/// Wrap one or more GFX PDUs in an uncompressed `RDP_SEGMENTED_DATA` SINGLE.
pub fn encode_segmented_single(gfx_pdus: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + gfx_pdus.len());
    out.write_u8(SEGMENTED_SINGLE);
    out.write_u8(PACKET_COMPR_TYPE_RDP8);
    out.write_slice(gfx_pdus);
    out
}

pub fn encode_caps_confirm(cap: &RawCapabilitySet) -> Vec<u8> {
    let mut body = Vec::with_capacity(8 + cap.data.len());
    cap.encode_body(&mut body);
    let mut out = Vec::with_capacity(GFX_HEADER_SIZE + body.len());
    write_header(&mut out, CMD_CAPS_CONFIRM, body.len());
    out.extend_from_slice(&body);
    out
}

pub fn encode_reset_graphics(width: u32, height: u32, monitors: &[MonitorDef]) -> Vec<u8> {
    let body_target = RESET_GRAPHICS_TOTAL_SIZE - GFX_HEADER_SIZE;
    let mut body = Vec::with_capacity(body_target);
    body.write_u32_le(width);
    body.write_u32_le(height);
    body.write_u32_le(monitors.len() as u32);
    for m in monitors {
        m.encode(&mut body);
    }
    while body.len() < body_target {
        body.push(0);
    }
    body.truncate(body_target);

    let mut out = Vec::with_capacity(RESET_GRAPHICS_TOTAL_SIZE);
    write_header(&mut out, CMD_RESET_GRAPHICS, body.len());
    out.extend_from_slice(&body);
    debug_assert_eq!(out.len(), RESET_GRAPHICS_TOTAL_SIZE);
    out
}

pub fn encode_create_surface(surface_id: u16, width: u16, height: u16) -> Vec<u8> {
    let mut body = Vec::with_capacity(7);
    body.write_u16_le(surface_id);
    body.write_u16_le(width);
    body.write_u16_le(height);
    body.write_u8(PIXEL_FORMAT_XRGB);
    let mut out = Vec::with_capacity(GFX_HEADER_SIZE + body.len());
    write_header(&mut out, CMD_CREATE_SURFACE, body.len());
    out.extend_from_slice(&body);
    out
}

pub fn encode_delete_surface(surface_id: u16) -> Vec<u8> {
    let mut body = Vec::with_capacity(2);
    body.write_u16_le(surface_id);
    let mut out = Vec::with_capacity(GFX_HEADER_SIZE + body.len());
    write_header(&mut out, CMD_DELETE_SURFACE, body.len());
    out.extend_from_slice(&body);
    out
}

pub fn encode_map_surface_to_output(
    surface_id: u16,
    output_origin_x: u32,
    output_origin_y: u32,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(12);
    body.write_u16_le(surface_id);
    body.write_u16_le(0); // reserved
    body.write_u32_le(output_origin_x);
    body.write_u32_le(output_origin_y);
    let mut out = Vec::with_capacity(GFX_HEADER_SIZE + body.len());
    write_header(&mut out, CMD_MAP_SURFACE_TO_OUTPUT, body.len());
    out.extend_from_slice(&body);
    out
}

pub fn encode_start_frame(timestamp_ms: u32, frame_id: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(8);
    body.write_u32_le(timestamp_ms);
    body.write_u32_le(frame_id);
    let mut out = Vec::with_capacity(GFX_HEADER_SIZE + body.len());
    write_header(&mut out, CMD_START_FRAME, body.len());
    out.extend_from_slice(&body);
    out
}

pub fn encode_end_frame(frame_id: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(4);
    body.write_u32_le(frame_id);
    let mut out = Vec::with_capacity(GFX_HEADER_SIZE + body.len());
    write_header(&mut out, CMD_END_FRAME, body.len());
    out.extend_from_slice(&body);
    out
}

/// Exclusive crop rectangle (MS-RDPEGFX `RDPGFX_RECT16`).
/// `qpVal` low 6 bits = QP; bit 7 = progressive (desktop is progressive).
pub fn encode_avc420_bitmap_stream(
    visible_width: u16,
    visible_height: u16,
    qp: u8,
    quality: u8,
    h264_bitstream: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 8 + 2 + h264_bitstream.len());
    out.write_u32_le(1); // numRegionRects
    Rect16 {
        left: 0,
        top: 0,
        right: visible_width,
        bottom: visible_height,
    }
    .encode(&mut out);
    out.write_u8((qp & 0x3f) | 0x80);
    out.write_u8(quality);
    out.write_slice(h264_bitstream);
    out
}

/// Exclusive destination rectangle on the surface (MS-RDPEGFX RDPGFX_RECT16).
pub fn encode_wire_to_surface_1_avc420(
    surface_id: u16,
    dest_width: u16,
    dest_height: u16,
    bitmap_data: &[u8],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(17 + bitmap_data.len());
    body.write_u16_le(surface_id);
    body.write_u16_le(CODEC_AVC420);
    body.write_u8(PIXEL_FORMAT_XRGB);
    Rect16 {
        left: 0,
        top: 0,
        right: dest_width,
        bottom: dest_height,
    }
    .encode(&mut body);
    body.write_u32_le(bitmap_data.len() as u32);
    body.write_slice(bitmap_data);

    let mut out = Vec::with_capacity(GFX_HEADER_SIZE + body.len());
    write_header(&mut out, CMD_WIRE_TO_SURFACE_1, body.len());
    out.extend_from_slice(&body);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    CapsAdvertise {
        sets: Vec<RawCapabilitySet>,
    },
    FrameAcknowledge {
        queue_depth: u32,
        frame_id: u32,
        total_frames_decoded: u32,
    },
    CacheImportOffer,
    Other {
        cmd_id: u16,
    },
}

pub fn decode_client_message(input: &[u8]) -> Result<ClientMessage, DecodeError> {
    let mut cursor = ReadCursor::new(input);
    let cmd_id = cursor.read_u16_le()?;
    let _flags = cursor.read_u16_le()?;
    let pdu_length = cursor.read_u32_le()? as usize;
    if pdu_length < GFX_HEADER_SIZE {
        return Err(DecodeError::InvalidValue {
            field: "rdpegfx.pduLength",
            reason: "smaller than header",
        });
    }
    let body_len = pdu_length - GFX_HEADER_SIZE;
    if cursor.remaining() < body_len {
        return Err(rdpcore_pdu::cursor::NotEnoughBytes {
            needed: body_len,
            remaining: cursor.remaining(),
        }
        .into());
    }

    match cmd_id {
        CMD_CAPS_ADVERTISE => {
            let count = cursor.read_u16_le()? as usize;
            let mut sets = Vec::with_capacity(count);
            for _ in 0..count {
                sets.push(RawCapabilitySet::decode(&mut cursor)?);
            }
            Ok(ClientMessage::CapsAdvertise { sets })
        }
        CMD_FRAME_ACKNOWLEDGE => Ok(ClientMessage::FrameAcknowledge {
            queue_depth: cursor.read_u32_le()?,
            frame_id: cursor.read_u32_le()?,
            total_frames_decoded: cursor.read_u32_le()?,
        }),
        CMD_CACHE_IMPORT_OFFER => Ok(ClientMessage::CacheImportOffer),
        other => Ok(ClientMessage::Other { cmd_id: other }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segmented_single_wraps_payload() {
        let wrapped = encode_segmented_single(&[0xab, 0xcd]);
        assert_eq!(wrapped, vec![0xe0, 0x04, 0xab, 0xcd]);
    }

    #[test]
    fn caps_confirm_roundtrip_header() {
        let cap = RawCapabilitySet::flags_only(CAP_VERSION_81, CAPS_FLAG_AVC420_ENABLED);
        let encoded = encode_caps_confirm(&cap);
        assert_eq!(&encoded[0..2], &CMD_CAPS_CONFIRM.to_le_bytes());
        let len = u32::from_le_bytes(encoded[4..8].try_into().unwrap()) as usize;
        assert_eq!(encoded.len(), len);
    }

    #[test]
    fn reset_graphics_is_340_bytes() {
        let pdu = encode_reset_graphics(
            1920,
            1080,
            &[MonitorDef {
                left: 0,
                top: 0,
                right: 1919,
                bottom: 1079,
                primary: true,
            }],
        );
        assert_eq!(pdu.len(), 340);
    }

    #[test]
    fn avc420_metablock_then_bitstream() {
        let stream = encode_avc420_bitmap_stream(100, 50, 22, 100, &[0x00, 0x00, 0x00, 0x01, 0x67]);
        assert_eq!(&stream[0..4], &1u32.to_le_bytes());
        // exclusive right/bottom (MS-RDPEGFX RDPGFX_RECT16)
        assert_eq!(
            &stream[4..12],
            &{
                let mut r = Vec::new();
                Rect16 {
                    left: 0,
                    top: 0,
                    right: 100,
                    bottom: 50,
                }
                .encode(&mut r);
                r
            }[..]
        );
        assert_eq!(stream[12], 22 | 0x80); // progressive bit set
        assert_eq!(stream[13], 100);
        assert_eq!(&stream[14..], &[0x00, 0x00, 0x00, 0x01, 0x67]);
    }

    #[test]
    fn decode_caps_advertise() {
        let mut body = Vec::new();
        body.write_u16_le(1);
        RawCapabilitySet::flags_only(CAP_VERSION_81, CAPS_FLAG_AVC420_ENABLED)
            .encode_body(&mut body);
        let mut pdu = Vec::new();
        write_header(&mut pdu, CMD_CAPS_ADVERTISE, body.len());
        pdu.extend_from_slice(&body);

        match decode_client_message(&pdu).unwrap() {
            ClientMessage::CapsAdvertise { sets } => {
                assert_eq!(sets.len(), 1);
                assert!(sets[0].supports_avc420());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn select_prefers_higher_version() {
        let sets = vec![
            RawCapabilitySet::flags_only(CAP_VERSION_81, CAPS_FLAG_AVC420_ENABLED),
            RawCapabilitySet::flags_only(CAP_VERSION_10, CAPS_FLAG_SMALL_CACHE),
            RawCapabilitySet::flags_only(CAP_VERSION_8, CAPS_FLAG_SMALL_CACHE),
        ];
        let selected = select_avc420_capability(&sets).unwrap();
        assert_eq!(selected.version, CAP_VERSION_10);
    }

    #[test]
    fn select_rejects_avc_disabled() {
        let sets = vec![RawCapabilitySet::flags_only(
            CAP_VERSION_10,
            CAPS_FLAG_AVC_DISABLED,
        )];
        assert!(select_avc420_capability(&sets).is_none());
    }

    #[test]
    fn wire_to_surface_header_and_codec() {
        let data = encode_avc420_bitmap_stream(16, 16, 22, 100, &[1, 2, 3]);
        let pdu = encode_wire_to_surface_1_avc420(1, 16, 16, &data);
        assert_eq!(&pdu[0..2], &CMD_WIRE_TO_SURFACE_1.to_le_bytes());
        assert_eq!(&pdu[8..10], &1u16.to_le_bytes()); // surfaceId
        assert_eq!(&pdu[10..12], &CODEC_AVC420.to_le_bytes());
    }

    #[test]
    fn avc420_preserves_annex_b_start_codes_on_wire() {
        let annex_b = [
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x00, 0x00, 0x01, 0x65,
        ];
        let stream = encode_avc420_bitmap_stream(32, 24, 22, 100, &annex_b);
        // metablock = 4 + 8 + 2; bitstream follows verbatim (Annex B, not AVCC).
        assert_eq!(&stream[14..], &annex_b);
        assert_eq!(&stream[14..18], &[0, 0, 0, 1]);
    }

    #[test]
    fn create_delete_map_start_end_layouts() {
        let create = encode_create_surface(7, 1280, 800);
        assert_eq!(&create[0..2], &CMD_CREATE_SURFACE.to_le_bytes());
        assert_eq!(&create[8..10], &7u16.to_le_bytes());
        assert_eq!(&create[10..12], &1280u16.to_le_bytes());
        assert_eq!(&create[12..14], &800u16.to_le_bytes());
        assert_eq!(create[14], PIXEL_FORMAT_XRGB);

        let delete = encode_delete_surface(7);
        assert_eq!(&delete[0..2], &CMD_DELETE_SURFACE.to_le_bytes());
        assert_eq!(&delete[8..10], &7u16.to_le_bytes());

        let map = encode_map_surface_to_output(7, 10, 20);
        assert_eq!(&map[0..2], &CMD_MAP_SURFACE_TO_OUTPUT.to_le_bytes());
        assert_eq!(&map[8..10], &7u16.to_le_bytes());
        assert_eq!(&map[12..16], &10u32.to_le_bytes());
        assert_eq!(&map[16..20], &20u32.to_le_bytes());

        let start = encode_start_frame(1_000, 42);
        assert_eq!(&start[0..2], &CMD_START_FRAME.to_le_bytes());
        assert_eq!(&start[8..12], &1_000u32.to_le_bytes());
        assert_eq!(&start[12..16], &42u32.to_le_bytes());

        let end = encode_end_frame(42);
        assert_eq!(&end[0..2], &CMD_END_FRAME.to_le_bytes());
        assert_eq!(&end[8..12], &42u32.to_le_bytes());
    }

    #[test]
    fn reset_graphics_monitor_coords_are_inclusive() {
        let pdu = encode_reset_graphics(
            100,
            50,
            &[MonitorDef {
                left: 0,
                top: 0,
                right: 99,
                bottom: 49,
                primary: true,
            }],
        );
        assert_eq!(pdu.len(), 340);
        // width/height then monitorCount then first monitor left/top/right/bottom
        assert_eq!(&pdu[8..12], &100u32.to_le_bytes());
        assert_eq!(&pdu[12..16], &50u32.to_le_bytes());
        assert_eq!(&pdu[16..20], &1u32.to_le_bytes());
        assert_eq!(&pdu[20..24], &0i32.to_le_bytes());
        assert_eq!(&pdu[28..32], &99i32.to_le_bytes());
        assert_eq!(&pdu[32..36], &49i32.to_le_bytes());
    }

    #[test]
    fn decode_frame_acknowledge() {
        let mut body = Vec::new();
        body.write_u32_le(3);
        body.write_u32_le(9);
        body.write_u32_le(100);
        let mut pdu = Vec::new();
        write_header(&mut pdu, CMD_FRAME_ACKNOWLEDGE, body.len());
        pdu.extend_from_slice(&body);
        match decode_client_message(&pdu).unwrap() {
            ClientMessage::FrameAcknowledge {
                queue_depth,
                frame_id,
                total_frames_decoded,
            } => {
                assert_eq!(queue_depth, 3);
                assert_eq!(frame_id, 9);
                assert_eq!(total_frames_decoded, 100);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_truncated_and_undersized_length() {
        assert!(decode_client_message(&[0x12, 0x00, 0, 0, 4, 0, 0, 0]).is_err());
        let mut pdu = Vec::new();
        write_header(&mut pdu, CMD_FRAME_ACKNOWLEDGE, 12);
        // claim 12 body bytes but provide none
        assert!(decode_client_message(&pdu).is_err());
    }

    #[test]
    fn decode_cache_import_offer_and_unknown() {
        let mut offer = Vec::new();
        write_header(&mut offer, CMD_CACHE_IMPORT_OFFER, 0);
        assert!(matches!(
            decode_client_message(&offer).unwrap(),
            ClientMessage::CacheImportOffer
        ));
        let mut other = Vec::new();
        write_header(&mut other, 0x00ff, 0);
        assert!(matches!(
            decode_client_message(&other).unwrap(),
            ClientMessage::Other { cmd_id: 0x00ff }
        ));
    }

    #[test]
    fn select_rejects_version8_and_prefers_107() {
        assert!(
            !RawCapabilitySet::flags_only(CAP_VERSION_8, CAPS_FLAG_SMALL_CACHE).supports_avc420()
        );
        let sets = vec![
            RawCapabilitySet::flags_only(CAP_VERSION_81, CAPS_FLAG_AVC420_ENABLED),
            RawCapabilitySet::flags_only(CAP_VERSION_107, 0),
        ];
        assert_eq!(
            select_avc420_capability(&sets).unwrap().version,
            CAP_VERSION_107
        );
    }

    #[test]
    fn caps_advertise_rejects_oversize_capability_blob() {
        let mut body = Vec::new();
        body.write_u16_le(1);
        body.write_u32_le(CAP_VERSION_81);
        body.write_u32_le(1_000_000); // claims huge data
        body.extend_from_slice(&0u32.to_le_bytes()); // only 4 bytes present
        let mut pdu = Vec::new();
        write_header(&mut pdu, CMD_CAPS_ADVERTISE, body.len());
        pdu.extend_from_slice(&body);
        assert!(decode_client_message(&pdu).is_err());
    }
}
