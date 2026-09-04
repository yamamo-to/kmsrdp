//! UTF-16LE helpers shared by GCC core data (fixed-width, NUL-padded
//! fields) and Client Info (length-prefixed, explicitly NUL-terminated
//! fields).

use crate::cursor::WriteBuf;

/// Encodes `s` truncated/padded to exactly `byte_len` bytes, NUL-padded,
/// with at least one trailing NUL guaranteed (matches GCC's
/// clientName/imeFileName convention).
pub fn write_fixed(out: &mut Vec<u8>, s: &str, byte_len: usize) {
    let max_units = byte_len / 2;
    let mut units: Vec<u16> = s.encode_utf16().collect();
    units.truncate(max_units.saturating_sub(1));
    for &u in &units {
        out.write_u16_le(u);
    }
    for _ in units.len()..max_units {
        out.write_u16_le(0);
    }
}

/// Decodes up to the first NUL code unit found in `bytes` (or all of it, if
/// none) - works both for NUL-padded fixed fields and for
/// length-includes-terminator fields (Client Info's clientAddress/clientDir).
pub fn read_fixed(bytes: &[u8]) -> String {
    // Feeds `char::decode_utf16` straight off a `take_while` over the raw
    // code units instead of collecting into an intermediate `Vec<u16>`
    // first (as `String::from_utf16_lossy` would need) - one fewer heap
    // buffer per decoded string field.
    // `as_chunks` (clippy's suggested replacement) stabilized after this
    // workspace's MSRV (1.87, see the workspace Cargo.toml comment) - keep
    // `chunks_exact` rather than bump it for a lint.
    #[allow(clippy::chunks_exact_to_as_chunks)]
    let units = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0);
    char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Raw UTF-16LE bytes, no terminator - used where the length prefix
/// excludes the terminator (Client Info's domain/username/password/...).
pub fn encode_units(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 2);
    for u in s.encode_utf16() {
        out.write_u16_le(u);
    }
    out
}

/// Inverse of [`encode_units`]: decodes exactly `bytes` with no NUL search.
pub fn decode_units(bytes: &[u8]) -> String {
    // See `read_fixed`'s `chunks_exact_to_as_chunks` allow - same MSRV reason.
    #[allow(clippy::chunks_exact_to_as_chunks)]
    let units = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]));
    char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_fixed_pads_to_exact_byte_length() {
        let mut out = Vec::new();
        write_fixed(&mut out, "ab", 8);
        assert_eq!(out.len(), 8);
        assert_eq!(read_fixed(&out), "ab");
    }

    #[test]
    fn read_fixed_stops_at_nul() {
        let mut out = Vec::new();
        write_fixed(&mut out, "hello", 12);
        assert_eq!(read_fixed(&out), "hello");
    }

    #[test]
    fn encode_decode_units_round_trip() {
        let s = "naïve";
        let bytes = encode_units(s);
        assert_eq!(decode_units(&bytes), s);
    }

    #[test]
    fn encode_units_is_utf16le() {
        let bytes = encode_units("A");
        assert_eq!(bytes, [b'A', 0]);
    }
}
