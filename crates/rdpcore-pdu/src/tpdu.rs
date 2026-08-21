//! X.224 TPDU header (class 0, the "simple class" RDP relies on), follows a
//! TPKT header. MS-RDPBCGR only exercises Connection Request/Confirm and
//! Data TPDUs.
//!
//! ```text
//! byte 1: LI (length indicator - header length in bytes, excluding LI
//!         itself and any user data)
//! byte 2: code (0xE0 = Connection Request, 0xD0 = Connection Confirm,
//!         0xF0 = Data, ...)
//! bytes 3-7 (non-Data): DST-REF(2) + SRC-REF(2) + Class(1), all zero for RDP
//! byte 3 (Data only): EOT (0x80, always end-of-transmission for RDP)
//! ```

use crate::DecodeError;
use crate::cursor::{ReadCursor, WriteBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TpduCode(pub u8);

impl TpduCode {
    pub const CONNECTION_REQUEST: Self = Self(0xE0);
    pub const CONNECTION_CONFIRM: Self = Self(0xD0);
    pub const DATA: Self = Self(0xF0);

    /// Size of the fixed part, LI byte included.
    fn header_fixed_part_size(self) -> usize {
        if self == Self::DATA {
            3 // LI + code + EOT
        } else {
            7 // LI + code + DST-REF(2) + SRC-REF(2) + Class(1)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct TpduHeader {
    pub li: u8,
    pub code: TpduCode,
}

impl TpduHeader {
    /// `variable_part_len` is the length of whatever comes after the fixed
    /// part but before user data (e.g. the RDP Negotiation Request/Response
    /// bytes for Connection Request/Confirm; zero for Data TPDUs).
    pub fn for_pdu(code: TpduCode, variable_part_len: usize) -> Self {
        let li = (code.header_fixed_part_size() - 1 + variable_part_len) as u8;
        Self { li, code }
    }

    pub fn variable_part_size(&self) -> usize {
        // `decode` below already rejects an `li` too small for the fixed
        // part, so this never actually underflows for a `TpduHeader` that
        // came from `decode` - but `saturating_sub` keeps that true by
        // construction rather than by the caller having to remember it.
        (usize::from(self.li) + 1).saturating_sub(self.code.header_fixed_part_size())
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        const EOT: u8 = 0x80;

        out.write_u8(self.li);
        out.write_u8(self.code.0);
        if self.code == TpduCode::DATA {
            out.write_u8(EOT);
        } else {
            out.write_u16_be(0); // DST-REF
            out.write_u16_be(0); // SRC-REF
            out.write_u8(0); // Class 0
        }
    }

    pub fn decode(src: &mut ReadCursor<'_>, expected: TpduCode) -> Result<Self, DecodeError> {
        src.ensure(2)?;
        let li = src.read_u8()?;
        let code = TpduCode(src.read_u8()?);
        if code != expected {
            return Err(DecodeError::InvalidValue {
                field: "tpdu.code",
                reason: "unexpected TPDU code",
            });
        }
        if li == 0xFF {
            return Err(DecodeError::InvalidValue {
                field: "tpdu.li",
                reason: "0xFF is reserved for X.224 extensions, unsupported",
            });
        }
        // `li` is the header length excluding LI itself, so the fixed
        // part (LI included) is `li + 1` bytes - reject anything too
        // small to hold it before `variable_part_size` (or the `- 2`
        // below) can underflow on it.
        if usize::from(li) + 1 < code.header_fixed_part_size() {
            return Err(DecodeError::InvalidValue {
                field: "tpdu.li",
                reason: "too small to hold this TPDU code's fixed header part",
            });
        }

        let header = Self { li, code };
        let remaining_fixed = code.header_fixed_part_size() - 2; // LI + code already consumed
        src.ensure(remaining_fixed)?;
        if code == TpduCode::DATA {
            src.advance(1); // EOT
        } else {
            src.advance(5); // DST-REF, SRC-REF, Class
        }
        Ok(header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_request_round_trip() {
        let header = TpduHeader::for_pdu(TpduCode::CONNECTION_REQUEST, 8);
        assert_eq!(header.li, 6 + 8);

        let mut buf = Vec::new();
        header.write(&mut buf);
        assert_eq!(buf, [6 + 8, 0xE0, 0, 0, 0, 0, 0]);

        let mut cursor = ReadCursor::new(&buf);
        let decoded = TpduHeader::decode(&mut cursor, TpduCode::CONNECTION_REQUEST).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(decoded.variable_part_size(), 8);
    }

    #[test]
    fn data_tpdu_round_trip() {
        let header = TpduHeader::for_pdu(TpduCode::DATA, 0);
        assert_eq!(header.li, 2);

        let mut buf = Vec::new();
        header.write(&mut buf);
        assert_eq!(buf, [2, 0xF0, 0x80]);

        let mut cursor = ReadCursor::new(&buf);
        let decoded = TpduHeader::decode(&mut cursor, TpduCode::DATA).unwrap();
        assert_eq!(decoded, header);
    }

    #[test]
    fn rejects_li_too_small_for_fixed_part_instead_of_underflowing() {
        // Connection Request needs li >= 6 (7-byte fixed part, LI itself
        // excluded from the count); li=3 previously underflowed
        // `variable_part_size`'s subtraction instead of being rejected
        // here in `decode`.
        let buf = [3u8, TpduCode::CONNECTION_REQUEST.0, 0, 0, 0, 0, 0];
        let mut cursor = ReadCursor::new(&buf);
        assert!(matches!(
            TpduHeader::decode(&mut cursor, TpduCode::CONNECTION_REQUEST),
            Err(DecodeError::InvalidValue { field: "tpdu.li", .. })
        ));
    }

    #[test]
    fn rejects_wrong_code() {
        let mut buf = Vec::new();
        TpduHeader::for_pdu(TpduCode::CONNECTION_REQUEST, 8).write(&mut buf);

        let mut cursor = ReadCursor::new(&buf);
        assert!(matches!(
            TpduHeader::decode(&mut cursor, TpduCode::CONNECTION_CONFIRM),
            Err(DecodeError::InvalidValue {
                field: "tpdu.code",
                ..
            })
        ));
    }
}
