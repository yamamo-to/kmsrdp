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
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
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
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units)
}
