//! ASN.1 BER primitives, just the subset MCS Connect-Initial/Connect-Response
//! actually use (definite-length encoding only): SEQUENCE/INTEGER/BOOLEAN/
//! ENUMERATED/OCTET STRING tags, and the two-byte "Application" tag form
//! MCS's `Connect-Initial`/`Connect-Response` application tags need.
//!
//! Everything below Connect-Initial/Response (the domain PDUs, and GCC's own
//! ConnectData wrapper) uses a different, PER-based encoding - see `per.rs`.

use crate::cursor::{ReadCursor, WriteBuf};
use crate::DecodeError;

pub const TAG_BOOLEAN: u8 = 0x01;
pub const TAG_INTEGER: u8 = 0x02;
pub const TAG_ENUMERATED: u8 = 0x0A;
pub const TAG_OCTET_STRING: u8 = 0x04;
/// Universal(0x00) | Construct(0x20) | tag-number 0x10 (SEQUENCE)
pub const TAG_SEQUENCE: u8 = 0x30;

/// Definite-length BER length: short form (`<=0x7F`, 1 byte) or long form
/// (`0x81`/`0x82` prefix + 1 or 2 big-endian bytes). We never need a
/// 3-byte-length-of-length form since nothing we encode exceeds 65535 bytes.
pub fn write_length(out: &mut Vec<u8>, length: usize) {
    if length <= 0x7F {
        out.write_u8(length as u8);
    } else if length <= 0xFF {
        out.write_u8(0x81);
        out.write_u8(length as u8);
    } else {
        out.write_u8(0x82);
        out.write_u16_be(length as u16);
    }
}

pub fn read_length(cursor: &mut ReadCursor<'_>) -> Result<usize, DecodeError> {
    let first = cursor.read_u8()?;
    match first {
        0..=0x7F => Ok(usize::from(first)),
        0x81 => Ok(usize::from(cursor.read_u8()?)),
        0x82 => Ok(usize::from(cursor.read_u16_be()?)),
        _ => Err(DecodeError::InvalidValue {
            field: "ber.length",
            reason: "unsupported BER length-of-length form",
        }),
    }
}

/// The 2-byte `[APPLICATION n]` tag form MCS's Connect-Initial (101) /
/// Connect-Response (102) use, since both tag numbers exceed the 1-byte
/// form's `0x1E` cutoff: `0x7F`, then the raw tag number.
pub fn write_application_tag(out: &mut Vec<u8>, tag_number: u8, body_length: usize) {
    out.write_u8(0x7F);
    out.write_u8(tag_number);
    write_length(out, body_length);
}

pub fn read_application_tag(cursor: &mut ReadCursor<'_>, expected_tag_number: u8) -> Result<usize, DecodeError> {
    let marker = cursor.read_u8()?;
    if marker != 0x7F {
        return Err(DecodeError::InvalidValue {
            field: "ber.application_tag",
            reason: "expected the 2-byte [APPLICATION n] tag form",
        });
    }
    let tag_number = cursor.read_u8()?;
    if tag_number != expected_tag_number {
        return Err(DecodeError::InvalidValue {
            field: "ber.application_tag",
            reason: "unexpected application tag number",
        });
    }
    read_length(cursor)
}

pub fn write_sequence_tag(out: &mut Vec<u8>, body_length: usize) {
    out.write_u8(TAG_SEQUENCE);
    write_length(out, body_length);
}

pub fn read_sequence_tag(cursor: &mut ReadCursor<'_>) -> Result<usize, DecodeError> {
    expect_tag(cursor, TAG_SEQUENCE, "ber.sequence")?;
    read_length(cursor)
}

/// Minimal-length big-endian encoding used by BER INTEGER: 1/2/3/4 bytes
/// depending on magnitude (see module docs on the crate for the exact
/// thresholds, cross-checked against a real implementation).
pub fn write_integer(out: &mut Vec<u8>, value: u32) {
    out.write_u8(TAG_INTEGER);
    if value < 0x80 {
        write_length(out, 1);
        out.write_u8(value as u8);
    } else if value < 0x8000 {
        write_length(out, 2);
        out.write_u16_be(value as u16);
    } else if value < 0x0080_0000 {
        write_length(out, 3);
        out.write_u8((value >> 16) as u8);
        out.write_u16_be(value as u16);
    } else {
        write_length(out, 4);
        out.write_u16_be((value >> 16) as u16);
        out.write_u16_be(value as u16);
    }
}

pub fn read_integer(cursor: &mut ReadCursor<'_>) -> Result<u32, DecodeError> {
    expect_tag(cursor, TAG_INTEGER, "ber.integer")?;
    let length = read_length(cursor)?;
    let bytes = cursor.read_slice(length)?;
    if bytes.is_empty() || bytes.len() > 4 {
        return Err(DecodeError::InvalidValue {
            field: "ber.integer",
            reason: "unsupported INTEGER length (expected 1-4 bytes)",
        });
    }
    let mut value = 0u32;
    for &b in bytes {
        value = (value << 8) | u32::from(b);
    }
    Ok(value)
}

pub fn write_boolean(out: &mut Vec<u8>, value: bool) {
    out.write_u8(TAG_BOOLEAN);
    write_length(out, 1);
    out.write_u8(if value { 0xFF } else { 0x00 });
}

pub fn read_boolean(cursor: &mut ReadCursor<'_>) -> Result<bool, DecodeError> {
    expect_tag(cursor, TAG_BOOLEAN, "ber.boolean")?;
    let length = read_length(cursor)?;
    let bytes = cursor.read_slice(length)?;
    Ok(bytes.first().is_some_and(|&b| b != 0))
}

pub fn write_enumerated(out: &mut Vec<u8>, value: u8) {
    out.write_u8(TAG_ENUMERATED);
    write_length(out, 1);
    out.write_u8(value);
}

pub fn read_enumerated(cursor: &mut ReadCursor<'_>) -> Result<u8, DecodeError> {
    expect_tag(cursor, TAG_ENUMERATED, "ber.enumerated")?;
    let length = read_length(cursor)?;
    let bytes = cursor.read_slice(length)?;
    bytes.first().copied().ok_or(DecodeError::InvalidValue {
        field: "ber.enumerated",
        reason: "empty ENUMERATED value",
    })
}

pub fn write_octet_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.write_u8(TAG_OCTET_STRING);
    write_length(out, bytes.len());
    out.write_slice(bytes);
}

pub fn read_octet_string<'a>(cursor: &mut ReadCursor<'a>) -> Result<&'a [u8], DecodeError> {
    expect_tag(cursor, TAG_OCTET_STRING, "ber.octet_string")?;
    let length = read_length(cursor)?;
    Ok(cursor.read_slice(length)?)
}

fn expect_tag(cursor: &mut ReadCursor<'_>, expected: u8, field: &'static str) -> Result<(), DecodeError> {
    let tag = cursor.read_u8()?;
    if tag != expected {
        return Err(DecodeError::InvalidValue {
            field,
            reason: "unexpected BER tag byte",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_round_trip_all_size_classes() {
        for value in [0u32, 0x7F, 0x80, 0x7FFF, 0x8000, 0x007F_FFFF, 0x0080_0000, 0xFFFF_FFFF] {
            let mut buf = Vec::new();
            write_integer(&mut buf, value);
            let mut cursor = ReadCursor::new(&buf);
            assert_eq!(read_integer(&mut cursor).unwrap(), value, "value {value:#x}");
        }
    }

    #[test]
    fn boolean_round_trip() {
        for value in [true, false] {
            let mut buf = Vec::new();
            write_boolean(&mut buf, value);
            let mut cursor = ReadCursor::new(&buf);
            assert_eq!(read_boolean(&mut cursor).unwrap(), value);
        }
    }

    #[test]
    fn octet_string_round_trip() {
        let mut buf = Vec::new();
        write_octet_string(&mut buf, b"hello");
        let mut cursor = ReadCursor::new(&buf);
        assert_eq!(read_octet_string(&mut cursor).unwrap(), b"hello");
    }

    #[test]
    fn long_form_length() {
        let mut buf = Vec::new();
        write_length(&mut buf, 200);
        assert_eq!(buf, [0x81, 200]);

        let mut buf = Vec::new();
        write_length(&mut buf, 300);
        assert_eq!(buf, [0x82, 0x01, 0x2C]);
    }

    #[test]
    fn application_tag_round_trip() {
        let mut buf = Vec::new();
        write_application_tag(&mut buf, 0x65, 10);
        assert_eq!(&buf[..2], [0x7F, 0x65]);

        let mut cursor = ReadCursor::new(&buf);
        assert_eq!(read_application_tag(&mut cursor, 0x65).unwrap(), 10);
    }
}
