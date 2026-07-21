//! Fast-path pointer updates (MS-RDPBCGR 2.2.9.1.2.1.4–2.2.9.1.2.1.8).

use crate::cursor::WriteBuf;
use crate::fastpath::{FastPathOutput, FastPathUpdatePdu, Fragmentation};

pub const UPDATE_CODE_PTR_NULL: u8 = 0x5;
pub const UPDATE_CODE_PTR_DEFAULT: u8 = 0x6;
pub const UPDATE_CODE_PTR_POSITION: u8 = 0x8;
pub const UPDATE_CODE_COLOR: u8 = 0x9;
pub const UPDATE_CODE_CACHED: u8 = 0xA;

fn single_update(update_code: u8, data: Vec<u8>) -> Vec<u8> {
    FastPathOutput {
        updates: vec![FastPathUpdatePdu {
            update_code,
            fragmentation: Fragmentation::Single,
            data,
        }],
    }
    .encode()
}

/// System default pointer (empty payload).
#[allow(dead_code)]
pub fn encode_ptr_default() -> Vec<u8> {
    single_update(UPDATE_CODE_PTR_DEFAULT, Vec::new())
}

/// Hide the pointer (empty payload).
#[allow(dead_code)]
pub fn encode_ptr_null() -> Vec<u8> {
    single_update(UPDATE_CODE_PTR_NULL, Vec::new())
}

/// Absolute pointer position (server-driven warp). Not used for mouse-move echo.
#[allow(dead_code)]
pub fn encode_ptr_position(x: u16, y: u16) -> Vec<u8> {
    let mut data = Vec::with_capacity(4);
    data.write_u16_le(x);
    data.write_u16_le(y);
    single_update(UPDATE_CODE_PTR_POSITION, data)
}

/// Instruct the client to use a previously cached color pointer.
#[allow(dead_code)]
pub fn encode_cached_pointer(cache_index: u16) -> Vec<u8> {
    let mut data = Vec::with_capacity(2);
    data.write_u16_le(cache_index);
    single_update(UPDATE_CODE_CACHED, data)
}

/// Encode a 24 bpp Color Pointer Update (xor + 1 bpp and-mask).
pub fn encode_color_pointer(
    cache_index: u16,
    hot_x: u16,
    hot_y: u16,
    width: u16,
    height: u16,
    xor_bgr_bottom_up: &[u8],
    and_mask: &[u8],
) -> Vec<u8> {
    let mut data = Vec::with_capacity(12 + xor_bgr_bottom_up.len() + and_mask.len());
    data.write_u16_le(cache_index);
    data.write_u16_le(hot_x);
    data.write_u16_le(hot_y);
    data.write_u16_le(width);
    data.write_u16_le(height);
    data.write_u16_le(and_mask.len() as u16);
    data.write_u16_le(xor_bgr_bottom_up.len() as u16);
    data.write_slice(xor_bgr_bottom_up);
    data.write_slice(and_mask);
    single_update(UPDATE_CODE_COLOR, data)
}

/// A simple 16×16 black arrow with a white outline (hotspot at tip).
#[allow(dead_code)]
pub fn encode_default_arrow_pointer(cache_index: u16) -> Vec<u8> {
    const W: usize = 16;
    const H: usize = 16;
    // Tip at (0,0); filled body toward bottom-right.
    let shape = [
        0b1000_0000_0000_0000u16,
        0b1100_0000_0000_0000,
        0b1110_0000_0000_0000,
        0b1111_0000_0000_0000,
        0b1111_1000_0000_0000,
        0b1111_1100_0000_0000,
        0b1111_1110_0000_0000,
        0b1111_1111_0000_0000,
        0b1111_1111_1000_0000,
        0b1111_1100_0000_0000,
        0b1101_1000_0000_0000,
        0b1000_1100_0000_0000,
        0b0000_0110_0000_0000,
        0b0000_0110_0000_0000,
        0b0000_0011_0000_0000,
        0b0000_0000_0000_0000,
    ];

    let mut xor = vec![0u8; W * H * 3];
    for (y, bits) in shape.iter().enumerate() {
        for x in 0..W {
            let row = H - 1 - y;
            let i = (row * W + x) * 3;
            if bits & (1u16 << (15 - x)) != 0 {
                xor[i] = 0x00;
                xor[i + 1] = 0x00;
                xor[i + 2] = 0x00;
            } else {
                xor[i] = 0xFF;
                xor[i + 1] = 0xFF;
                xor[i + 2] = 0xFF;
            }
        }
    }

    // 1 bpp AND mask, each row padded to a multiple of 2 bytes.
    let stride = W.div_ceil(16) * 2;
    let mut and_mask = vec![0xFFu8; stride * H];
    for (y, bits) in shape.iter().enumerate() {
        let row = H - 1 - y;
        for x in 0..W {
            if bits & (1u16 << (15 - x)) != 0 {
                let byte = row * stride + x / 8;
                let bit = 7 - (x % 8);
                and_mask[byte] &= !(1 << bit);
            }
        }
    }

    encode_color_pointer(cache_index, 0, 0, W as u16, H as u16, &xor, &and_mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fastpath::FastPathOutput;

    #[test]
    fn default_and_position_round_trip_headers() {
        let def = encode_ptr_default();
        let decoded = FastPathOutput::decode(&def).unwrap();
        assert_eq!(decoded.updates[0].update_code, UPDATE_CODE_PTR_DEFAULT);
        assert!(decoded.updates[0].data.is_empty());

        let pos = encode_ptr_position(100, 200);
        let decoded = FastPathOutput::decode(&pos).unwrap();
        assert_eq!(decoded.updates[0].update_code, UPDATE_CODE_PTR_POSITION);
        assert_eq!(decoded.updates[0].data, [100, 0, 200, 0]);
    }

    #[test]
    fn arrow_pointer_encodes_nonzero_masks() {
        let wire = encode_default_arrow_pointer(0);
        let decoded = FastPathOutput::decode(&wire).unwrap();
        assert_eq!(decoded.updates[0].update_code, UPDATE_CODE_COLOR);
        assert!(decoded.updates[0].data.len() > 12);
    }
}
