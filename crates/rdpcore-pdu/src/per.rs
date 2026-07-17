//! T.125/T.124's compact PER-derived encoding - hand-rolled helpers, not a
//! general ASN.1 PER codec. Used for: GCC's `ConnectData`/`ConnectGCCPDU`
//! wrapper (around the Client/Server GCC blocks, see `gcc.rs`) and every MCS
//! domain PDU after Connect-Response (`mcs.rs`'s `DomainMcsPdu`s). Distinct
//! from `ber.rs`, which only covers Connect-Initial/Connect-Response's outer
//! framing.

use crate::DecodeError;
use crate::cursor::{ReadCursor, WriteBuf};

/// 1 byte if `<=0x7F`; otherwise 2 bytes, big-endian, with the top bit of
/// the 16-bit value forced set to mark the long form. This is *not* the
/// same long-form scheme as BER's `0x81`/`0x82` prefix bytes.
pub fn write_length(out: &mut Vec<u8>, length: usize) {
    if length <= 0x7F {
        out.write_u8(length as u8);
    } else {
        out.write_u16_be(length as u16 | 0x8000);
    }
}

pub fn read_length(cursor: &mut ReadCursor<'_>) -> Result<usize, DecodeError> {
    let first = cursor.peek_slice(1)?[0];
    if first & 0x80 == 0 {
        Ok(usize::from(cursor.read_u8()?))
    } else {
        Ok(usize::from(cursor.read_u16_be()? & 0x7FFF))
    }
}

pub fn write_choice(out: &mut Vec<u8>, choice: u8) {
    out.write_u8(choice);
}

pub fn read_choice(cursor: &mut ReadCursor<'_>) -> Result<u8, DecodeError> {
    Ok(cursor.read_u8()?)
}

pub fn write_selection(out: &mut Vec<u8>, selection: u8) {
    out.write_u8(selection);
}

pub fn read_selection(cursor: &mut ReadCursor<'_>) -> Result<u8, DecodeError> {
    Ok(cursor.read_u8()?)
}

pub fn write_number_of_sets(out: &mut Vec<u8>, count: u8) {
    out.write_u8(count);
}

pub fn read_number_of_sets(cursor: &mut ReadCursor<'_>) -> Result<u8, DecodeError> {
    Ok(cursor.read_u8()?)
}

pub fn write_enum(out: &mut Vec<u8>, value: u8) {
    out.write_u8(value);
}

pub fn read_enum(cursor: &mut ReadCursor<'_>) -> Result<u8, DecodeError> {
    Ok(cursor.read_u8()?)
}

/// Length-prefixed, minimal-width big-endian unsigned integer (1/2/4 bytes).
pub fn write_u32(out: &mut Vec<u8>, value: u32) {
    if value <= 0xFF {
        write_length(out, 1);
        out.write_u8(value as u8);
    } else if value <= 0xFFFF {
        write_length(out, 2);
        out.write_u16_be(value as u16);
    } else {
        write_length(out, 4);
        out.write_u16_be((value >> 16) as u16);
        out.write_u16_be(value as u16);
    }
}

pub fn read_u32(cursor: &mut ReadCursor<'_>) -> Result<u32, DecodeError> {
    let length = read_length(cursor)?;
    let bytes = cursor.read_slice(length)?;
    let mut value = 0u32;
    for &b in bytes {
        value = (value << 8) | u32::from(b);
    }
    Ok(value)
}

/// Fixed 2-byte big-endian "constrained INTEGER (min..max)" collapsed to a
/// subtract-and-store-as-u16 pattern - no separate length prefix.
pub fn write_u16(out: &mut Vec<u8>, value: u16, min: u16) {
    out.write_u16_be(value - min);
}

pub fn read_u16(cursor: &mut ReadCursor<'_>, min: u16) -> Result<u16, DecodeError> {
    Ok(cursor.read_u16_be()?.wrapping_add(min))
}

/// A 6-element GCC object identifier: 1 fixed length byte (`5`), then
/// `id[0]*40 + id[1]` packed into one byte, then the remaining 4 elements
/// as raw bytes.
pub fn write_object_id(out: &mut Vec<u8>, id: [u8; 6]) {
    out.write_u8(5);
    out.write_u8(id[0] * 40 + id[1]);
    out.write_slice(&id[2..6]);
}

pub fn read_object_id(cursor: &mut ReadCursor<'_>) -> Result<[u8; 6], DecodeError> {
    let length = cursor.read_u8()?;
    if length != 5 {
        return Err(DecodeError::InvalidValue {
            field: "per.object_id",
            reason: "expected a 5-byte packed OID body",
        });
    }
    let packed = cursor.read_u8()?;
    let rest = cursor.read_slice(4)?;
    Ok([packed / 40, packed % 40, rest[0], rest[1], rest[2], rest[3]])
}

/// Length-prefixed octet string where the length field stores `len - min`,
/// but the full string (all `min`-or-more bytes) still follows on the wire.
pub fn write_octet_string(out: &mut Vec<u8>, bytes: &[u8], min: usize) {
    write_length(out, bytes.len() - min);
    out.write_slice(bytes);
}

pub fn read_octet_string<'a>(
    cursor: &mut ReadCursor<'a>,
    min: usize,
) -> Result<&'a [u8], DecodeError> {
    let excess = read_length(cursor)?;
    Ok(cursor.read_slice(excess + min)?)
}

pub fn write_padding(out: &mut Vec<u8>, n: usize) {
    for _ in 0..n {
        out.write_u8(0);
    }
}

pub fn read_padding(cursor: &mut ReadCursor<'_>, n: usize) -> Result<(), DecodeError> {
    cursor.ensure(n)?;
    cursor.advance(n);
    Ok(())
}

/// BCD-packed numeric string (GCC `ConferenceName`, always `"1"` in
/// practice): length-prefixed (`len - min`, PER length), 2 digits per byte
/// via `(c - '0') % 10`, odd count padded with a `'0'` mate.
pub fn write_numeric_string(out: &mut Vec<u8>, value: &str, min: usize) {
    write_length(out, value.len() - min);
    let digits: Vec<u8> = value.bytes().map(|c| (c.wrapping_sub(b'0')) % 10).collect();
    let mut chunks = digits.chunks(2);
    for pair in &mut chunks {
        let hi = pair[0];
        let lo = pair.get(1).copied().unwrap_or(0);
        out.write_u8((hi << 4) | lo);
    }
}

/// Inverse of [`write_numeric_string`]: unpacks BCD nibbles back into ASCII
/// digit characters.
pub fn read_numeric_string(cursor: &mut ReadCursor<'_>, min: usize) -> Result<String, DecodeError> {
    let excess = read_length(cursor)?;
    let digit_count = excess + min;
    let byte_count = digit_count.div_ceil(2);
    let bytes = cursor.read_slice(byte_count)?;
    let mut s = String::with_capacity(digit_count);
    for (i, &b) in bytes.iter().enumerate() {
        if i * 2 < digit_count {
            s.push((b'0' + (b >> 4)) as char);
        }
        if i * 2 + 1 < digit_count {
            s.push((b'0' + (b & 0x0F)) as char);
        }
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_round_trip_short_and_long_form() {
        for len in [0usize, 0x7F, 0x80, 0x1234] {
            let mut buf = Vec::new();
            write_length(&mut buf, len);
            let mut cursor = ReadCursor::new(&buf);
            assert_eq!(read_length(&mut cursor).unwrap(), len);
        }
    }

    #[test]
    fn u16_constrained_round_trip() {
        let mut buf = Vec::new();
        write_u16(&mut buf, 1003, 1001);
        assert_eq!(buf, [0x00, 0x02]);
        let mut cursor = ReadCursor::new(&buf);
        assert_eq!(read_u16(&mut cursor, 1001).unwrap(), 1003);
    }

    #[test]
    fn u32_round_trip_all_widths() {
        for value in [0u32, 0xFF, 0x100, 0xFFFF, 0x10000, 0xFFFF_FFFF] {
            let mut buf = Vec::new();
            write_u32(&mut buf, value);
            let mut cursor = ReadCursor::new(&buf);
            assert_eq!(read_u32(&mut cursor).unwrap(), value);
        }
    }

    #[test]
    fn object_id_round_trip() {
        let id = [0, 0, 20, 124, 0, 1];
        let mut buf = Vec::new();
        write_object_id(&mut buf, id);
        assert_eq!(buf, [0x05, 0x00, 0x14, 0x7C, 0x00, 0x01]);
        let mut cursor = ReadCursor::new(&buf);
        assert_eq!(read_object_id(&mut cursor).unwrap(), id);
    }

    #[test]
    fn numeric_string_round_trip_matches_known_example() {
        // GCC ConferenceName "1", min=1 -> `00 10` (confirmed against a real
        // implementation's byte-for-byte output).
        let mut buf = Vec::new();
        write_numeric_string(&mut buf, "1", 1);
        assert_eq!(buf, [0x00, 0x10]);

        let mut cursor = ReadCursor::new(&buf);
        assert_eq!(read_numeric_string(&mut cursor, 1).unwrap(), "1");
    }

    #[test]
    fn octet_string_round_trip_with_min() {
        let mut buf = Vec::new();
        write_octet_string(&mut buf, b"Duca", 4);
        assert_eq!(buf, [0x00, b'D', b'u', b'c', b'a']);
        let mut cursor = ReadCursor::new(&buf);
        assert_eq!(read_octet_string(&mut cursor, 4).unwrap(), b"Duca");
    }
}
