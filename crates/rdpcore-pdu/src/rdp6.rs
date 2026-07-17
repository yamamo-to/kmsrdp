//! RDP 6.0 "Planar" bitmap compression (MS-RDPEGDI 3.1.9): per-plane
//! vertical-delta filtering followed by a byte-oriented run-length
//! encoding, used to compress classic `TS_BITMAP_DATA` bitmap updates -
//! carried through the existing `BITMAP_COMPRESSION` flag path (see
//! `fastpath::BitmapRect`), not a separately-negotiated codec/capability.
//!
//! Scope: ARGB mode only (no YCoCg color transform, no chroma
//! subsampling, no alpha plane) - `FormatHeader = 0x30` (RLE enabled,
//! no-alpha, color-loss-level 0). This is the simplest configuration and
//! requires no capability beyond the classic compression flag every
//! RDP6-capable client already accepts unconditionally.
//!
//! This crate only ever needs to *encode* (this server never receives a
//! compressed bitmap from a client) - `decode` exists purely so the
//! encoder can be round-trip tested without a live client.

use crate::cursor::ReadCursor;
use crate::DecodeError;

pub const FORMAT_HEADER_RLE_NO_ALPHA_ARGB: u8 = 0x30;

/// Encodes one BGRX32 tile (4 bytes/pixel, X ignored) into an RDP6 Planar
/// bitmap stream: `[FormatHeader][R-plane][G-plane][B-plane]`. `bgrx` must
/// already be in the same row order the caller intends the decoder to
/// reconstruct (this codec has no notion of "bottom-up" itself - that's
/// purely a `TS_BITMAP_DATA`-level convention the caller is responsible
/// for, see `rdpcore_server::encode_bitmap_update`).
pub fn encode(bgrx: &[u8], width: usize, height: usize) -> Vec<u8> {
    let pixel_count = width * height;
    let mut r = vec![0u8; pixel_count];
    let mut g = vec![0u8; pixel_count];
    let mut b = vec![0u8; pixel_count];
    for i in 0..pixel_count {
        b[i] = bgrx[i * 4];
        g[i] = bgrx[i * 4 + 1];
        r[i] = bgrx[i * 4 + 2];
    }

    let mut out = Vec::with_capacity(1 + pixel_count * 3 / 2);
    out.push(FORMAT_HEADER_RLE_NO_ALPHA_ARGB);
    encode_plane(&r, width, height, &mut out);
    encode_plane(&g, width, height, &mut out);
    encode_plane(&b, width, height, &mut out);
    out
}

/// Inverse of [`encode`] - reconstructs a BGRX32 tile (X always 0).
pub fn decode(data: &[u8], width: usize, height: usize) -> Result<Vec<u8>, DecodeError> {
    let mut cursor = ReadCursor::new(data);
    let header = cursor.read_u8()?;
    if header != FORMAT_HEADER_RLE_NO_ALPHA_ARGB {
        return Err(DecodeError::InvalidValue {
            field: "rdp6.format_header",
            reason: "only RLE + no-alpha + ARGB (0x30) is supported",
        });
    }
    let r = decode_plane(&mut cursor, width, height)?;
    let g = decode_plane(&mut cursor, width, height)?;
    let b = decode_plane(&mut cursor, width, height)?;

    let pixel_count = width * height;
    let mut bgrx = vec![0u8; pixel_count * 4];
    for i in 0..pixel_count {
        bgrx[i * 4] = b[i];
        bgrx[i * 4 + 1] = g[i];
        bgrx[i * 4 + 2] = r[i];
    }
    Ok(bgrx)
}

// ---------------------------------------------------------------------
// Per-plane vertical delta filter (zigzag-packed)
// ---------------------------------------------------------------------

fn zigzag_encode(raw_delta: u8) -> u8 {
    if raw_delta < 128 {
        raw_delta << 1
    } else {
        ((255 - raw_delta) << 1) + 1
    }
}

fn zigzag_decode(encoded: u8) -> u8 {
    if encoded & 1 != 0 {
        255 - ((encoded - 1) >> 1)
    } else {
        encoded >> 1
    }
}

fn encode_plane(plane: &[u8], width: usize, height: usize, out: &mut Vec<u8>) {
    let mut delta = vec![0u8; width * height];
    delta[..width].copy_from_slice(&plane[..width]); // row 0: literal, unfiltered
    for row in 1..height {
        for col in 0..width {
            let idx = row * width + col;
            let above = plane[idx - width];
            delta[idx] = zigzag_encode(plane[idx].wrapping_sub(above));
        }
    }
    for row in 0..height {
        encode_scanline_rle(&delta[row * width..(row + 1) * width], out);
    }
}

fn decode_plane(cursor: &mut ReadCursor<'_>, width: usize, height: usize) -> Result<Vec<u8>, DecodeError> {
    let mut delta = vec![0u8; width * height];
    for row in 0..height {
        decode_scanline_rle(cursor, &mut delta[row * width..(row + 1) * width])?;
    }
    let mut plane = vec![0u8; width * height];
    plane[..width].copy_from_slice(&delta[..width]);
    for row in 1..height {
        for col in 0..width {
            let idx = row * width + col;
            let transformed = zigzag_decode(delta[idx]);
            plane[idx] = plane[idx - width].wrapping_add(transformed);
        }
    }
    Ok(plane)
}

// ---------------------------------------------------------------------
// Per-scanline RLE: one control byte per segment,
// `control = (literal_count << 4) | run_field`.
//   run_field == 0        -> pure literal, no repeat (literal_count 0..15)
//   run_field in 3..=15   -> literal_count literal bytes (0..15, 0 means
//                            "continue the previous segment's last byte"),
//                            then repeat the last of those bytes run_field
//                            more times
//   run_field in {1, 2}   -> reserved "extended run" forms (16+extra /
//                            32+extra repeats, no literal bytes) - this
//                            encoder never emits them (any run needing
//                            them is instead expressed as a longer chain
//                            of run_field-3..=15 segments plus a final
//                            1-2-byte literal tail), but decode supports
//                            them for completeness/spec-compliance.
// A segment must never straddle a scanline boundary, and the "last byte"
// used for repeats/continuation resets to 0 at the start of every
// scanline.
// ---------------------------------------------------------------------

fn encode_scanline_rle(scanline: &[u8], out: &mut Vec<u8>) {
    let mut pending: Vec<u8> = Vec::with_capacity(15);
    let mut i = 0;
    while i < scanline.len() {
        let byte = scanline[i];
        let mut count = 1;
        while i + count < scanline.len() && scanline[i + count] == byte {
            count += 1;
        }
        if count < 4 {
            for _ in 0..count {
                pending.push(byte);
                if pending.len() == 15 {
                    flush_literals(&mut pending, out);
                }
            }
        } else {
            flush_literals(&mut pending, out);
            emit_run(byte, count, out);
        }
        i += count;
    }
    flush_literals(&mut pending, out);
}

fn flush_literals(pending: &mut Vec<u8>, out: &mut Vec<u8>) {
    if pending.is_empty() {
        return;
    }
    out.push((pending.len() as u8) << 4); // run_field = 0
    out.extend_from_slice(pending);
    pending.clear();
}

/// `count` is always >= 4 here (smaller runs are handled as plain
/// literals by the caller) - see the module doc comment for why
/// `run_field` 1/2 are never used.
fn emit_run(byte: u8, count: usize, out: &mut Vec<u8>) {
    let mut remaining = count - 1;
    let first_run = remaining.min(15); // >= 3, since count >= 4
    out.push((1u8 << 4) | (first_run as u8)); // literal_count=1 (the byte itself), run_field=first_run
    out.push(byte);
    remaining -= first_run;

    while remaining > 0 {
        if remaining >= 3 {
            let chunk = remaining.min(15);
            out.push(chunk as u8); // literal_count=0 (continuation), run_field=chunk
            remaining -= chunk;
        } else {
            // 1 or 2 bytes left - run_field can't express that (reserved
            // for the extended form), so emit them as literals instead.
            out.push((remaining as u8) << 4); // run_field = 0
            out.extend(std::iter::repeat_n(byte, remaining));
            remaining = 0;
        }
    }
}

fn decode_scanline_rle(cursor: &mut ReadCursor<'_>, scanline: &mut [u8]) -> Result<(), DecodeError> {
    let mut last_byte = 0u8;
    let mut pos = 0;
    while pos < scanline.len() {
        let control = cursor.read_u8()?;
        if control == 0 {
            return Err(DecodeError::InvalidValue {
                field: "rdp6.rle.control",
                reason: "0x00 control byte is invalid",
            });
        }
        let upper = control >> 4;
        let rle_field = control & 0x0F;
        let (run_length, literal_count): (usize, usize) = match rle_field {
            1 => (16 + usize::from(upper), 0),
            2 => (32 + usize::from(upper), 0),
            n => (usize::from(n), usize::from(upper)),
        };

        for _ in 0..literal_count {
            if pos >= scanline.len() {
                return Err(DecodeError::InvalidValue {
                    field: "rdp6.rle.scanline",
                    reason: "segment overruns the scanline",
                });
            }
            let byte = cursor.read_u8()?;
            scanline[pos] = byte;
            last_byte = byte;
            pos += 1;
        }
        for _ in 0..run_length {
            if pos >= scanline.len() {
                return Err(DecodeError::InvalidValue {
                    field: "rdp6.rle.scanline",
                    reason: "segment overruns the scanline",
                });
            }
            scanline[pos] = last_byte;
            pos += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tile(width: usize, height: usize, mut pixel: impl FnMut(usize, usize) -> (u8, u8, u8)) -> Vec<u8> {
        let mut out = vec![0u8; width * height * 4];
        for row in 0..height {
            for col in 0..width {
                let (b, g, r) = pixel(col, row);
                let idx = (row * width + col) * 4;
                out[idx] = b;
                out[idx + 1] = g;
                out[idx + 2] = r;
                out[idx + 3] = 0;
            }
        }
        out
    }

    #[test]
    fn zigzag_round_trips_every_byte_value() {
        for raw in 0u8..=255 {
            assert_eq!(zigzag_decode(zigzag_encode(raw)), raw, "raw delta {raw}");
        }
    }

    #[test]
    fn solid_color_tile_round_trips_and_compresses_well() {
        let bgrx = make_tile(64, 64, |_, _| (10, 20, 30));
        let compressed = encode(&bgrx, 64, 64);
        assert!(compressed.len() < bgrx.len() / 10, "solid color should compress at least 10x, got {} bytes", compressed.len());
        let decoded = decode(&compressed, 64, 64).unwrap();
        assert_eq!(decoded, bgrx);
    }

    #[test]
    fn gradient_tile_round_trips() {
        let bgrx = make_tile(64, 64, |x, y| ((x * 4) as u8, (y * 4) as u8, ((x + y) * 2) as u8));
        let compressed = encode(&bgrx, 64, 64);
        let decoded = decode(&compressed, 64, 64).unwrap();
        assert_eq!(decoded, bgrx);
    }

    #[test]
    fn noisy_tile_round_trips() {
        // Pseudo-random via a simple LCG - no external `rand` dependency
        // in this crate, and determinism matters more than real entropy.
        let mut state: u32 = 0x12345678;
        let mut next = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        };
        let bgrx = make_tile(37, 29, |_, _| (next(), next(), next()));
        let compressed = encode(&bgrx, 37, 29);
        let decoded = decode(&compressed, 37, 29).unwrap();
        assert_eq!(decoded, bgrx);
    }

    #[test]
    fn runs_crossing_the_extended_length_thresholds_round_trip() {
        // Exercise run lengths right around 16/32 (the extended-form
        // boundaries this encoder deliberately avoids) and past it.
        for &run_len in &[3usize, 4, 15, 16, 17, 31, 32, 33, 60] {
            let width = run_len + 5;
            let bgrx = make_tile(width, 3, |x, _| if x < run_len { (7, 7, 7) } else { (200, 200, 200) });
            let compressed = encode(&bgrx, width, 3);
            let decoded = decode(&compressed, width, 3).unwrap();
            assert_eq!(decoded, bgrx, "run length {run_len}");
        }
    }

    #[test]
    fn single_pixel_tile_round_trips() {
        let bgrx = make_tile(4, 1, |_, _| (1, 2, 3));
        let compressed = encode(&bgrx, 4, 1);
        let decoded = decode(&compressed, 4, 1).unwrap();
        assert_eq!(decoded, bgrx);
    }

    #[test]
    fn format_header_is_rle_no_alpha_argb() {
        let bgrx = make_tile(4, 4, |_, _| (1, 2, 3));
        let compressed = encode(&bgrx, 4, 4);
        assert_eq!(compressed[0], FORMAT_HEADER_RLE_NO_ALPHA_ARGB);
    }

    #[test]
    fn decode_rejects_unsupported_format_header() {
        let err = decode(&[0x00, 0, 0, 0], 4, 1).unwrap_err();
        assert!(matches!(err, DecodeError::InvalidValue { field: "rdp6.format_header", .. }));
    }

    #[test]
    fn decode_rejects_zero_control_byte() {
        // FormatHeader + a lone 0x00 control byte for the R plane.
        let err = decode(&[FORMAT_HEADER_RLE_NO_ALPHA_ARGB, 0x00], 4, 1).unwrap_err();
        assert!(matches!(err, DecodeError::InvalidValue { field: "rdp6.rle.control", .. }));
    }
}
