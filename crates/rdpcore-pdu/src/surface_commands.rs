//! SurfaceCommands fast-path updates (MS-RDPBCGR 2.2.9.2) carrying
//! NSCodec-compressed bitmaps via `SetSurfaceBits`, plus Frame Marker.

use crate::cursor::WriteBuf;

const CMD_SET_SURFACE_BITS: u16 = 0x01;
const CMD_FRAME_MARKER: u16 = 0x0004;

/// `SURFACECMD_FRAMEACTION_BEGIN` (MS-RDPBCGR 2.2.9.2.3).
pub const FRAME_ACTION_BEGIN: u16 = 0x0000;
/// `SURFACECMD_FRAMEACTION_END` (MS-RDPBCGR 2.2.9.2.3).
pub const FRAME_ACTION_END: u16 = 0x0001;

/// Encode one `TS_FRAME_MARKER` command body (without the fast-path header).
pub fn encode_frame_marker(frame_action: u16, frame_id: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.write_u16_le(CMD_FRAME_MARKER);
    out.write_u16_le(frame_action);
    out.write_u32_le(frame_id);
    out
}

/// Exclusive rectangle: `right`/`bottom` are one past the last pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExclusiveRectangle {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

impl ExclusiveRectangle {
    fn encode(&self, out: &mut Vec<u8>) {
        out.write_u16_le(self.left);
        out.write_u16_le(self.top);
        out.write_u16_le(self.right);
        out.write_u16_le(self.bottom);
    }
}

/// Encode one `SetSurfaceBits` command body (without the fast-path header).
pub fn encode_set_surface_bits(
    left: u16,
    top: u16,
    width: u16,
    height: u16,
    codec_id: u8,
    data: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + 8 + 12 + data.len());
    out.write_u16_le(CMD_SET_SURFACE_BITS);
    ExclusiveRectangle {
        left,
        top,
        right: left.saturating_add(width),
        bottom: top.saturating_add(height),
    }
    .encode(&mut out);
    out.write_u8(32); // bpp
    out.write_u8(0); // flags
    out.write_u8(0); // reserved
    out.write_u8(codec_id);
    out.write_u16_le(width);
    out.write_u16_le(height);
    out.write_u32_le(data.len() as u32);
    out.write_slice(data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_marker_encodes_action_and_id() {
        let body = encode_frame_marker(FRAME_ACTION_BEGIN, 42);
        assert_eq!(&body[0..2], &CMD_FRAME_MARKER.to_le_bytes());
        assert_eq!(&body[2..4], &FRAME_ACTION_BEGIN.to_le_bytes());
        assert_eq!(&body[4..8], &42u32.to_le_bytes());
    }

    #[test]
    fn set_surface_bits_carries_exclusive_rect_and_payload() {
        let payload = [1u8, 2, 3];
        let body = encode_set_surface_bits(10, 20, 30, 40, 0x77, &payload);
        assert_eq!(&body[0..2], &CMD_SET_SURFACE_BITS.to_le_bytes());
        assert_eq!(&body[2..4], &10u16.to_le_bytes()); // left
        assert_eq!(&body[4..6], &20u16.to_le_bytes()); // top
        assert_eq!(&body[6..8], &40u16.to_le_bytes()); // right = left+width
        assert_eq!(&body[8..10], &60u16.to_le_bytes()); // bottom = top+height
        assert_eq!(body[13], 0x77); // codec_id
        assert_eq!(body[body.len() - 3..], payload);
    }
}
