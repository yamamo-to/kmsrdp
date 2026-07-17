//! TPKT header - RFC 1006 / ITU-T T.123. Wraps every X.224 TPDU with a
//! 4-byte header giving the total packet length.
//!
//! ```text
//! byte 1: version (always 3)
//! byte 2: reserved (0)
//! bytes 3-4: total packet length, big-endian, header included
//! ```

use crate::cursor::{ReadCursor, WriteBuf};
use crate::DecodeError;

pub const VERSION: u8 = 3;
pub const HEADER_SIZE: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TpktHeader {
    /// Total length of the packet in bytes, this header included.
    pub packet_length: u16,
}

impl TpktHeader {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.write_u8(VERSION);
        out.write_u8(0); // reserved
        out.write_u16_be(self.packet_length);
    }

    pub fn decode(src: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        src.ensure(HEADER_SIZE).map_err(DecodeError::NotEnoughBytes)?;
        let version = src.read_u8().map_err(DecodeError::NotEnoughBytes)?;
        if version != VERSION {
            return Err(DecodeError::InvalidValue {
                field: "tpkt.version",
                reason: "unsupported TPKT version",
            });
        }
        src.advance(1); // reserved
        let packet_length = src.read_u16_be().map_err(DecodeError::NotEnoughBytes)?;
        Ok(Self { packet_length })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let header = TpktHeader { packet_length: 42 };
        let mut buf = Vec::new();
        header.write(&mut buf);
        assert_eq!(buf, [0x03, 0x00, 0x00, 0x2a]);

        let mut cursor = ReadCursor::new(&buf);
        let decoded = TpktHeader::decode(&mut cursor).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(cursor.pos(), HEADER_SIZE);
    }

    #[test]
    fn rejects_unsupported_version() {
        let buf = [0x02, 0x00, 0x00, 0x07];
        let mut cursor = ReadCursor::new(&buf);
        assert!(matches!(
            TpktHeader::decode(&mut cursor),
            Err(DecodeError::InvalidValue { field: "tpkt.version", .. })
        ));
    }

    #[test]
    fn rejects_truncated_header() {
        let buf = [0x03, 0x00];
        let mut cursor = ReadCursor::new(&buf);
        assert!(matches!(TpktHeader::decode(&mut cursor), Err(DecodeError::NotEnoughBytes(_))));
    }
}
