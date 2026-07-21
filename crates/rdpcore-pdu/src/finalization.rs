//! Share Data Header + the four finalization messages (MS-RDPBCGR
//! 2.2.1.14-2.2.1.20): Synchronize, Control, Font List/Map. All four ride
//! inside a Share Control Header `DataPdu` (type 0x7).

use crate::DecodeError;
use crate::capability_sets::{ShareControlHeader, ShareControlPduType};
use crate::cursor::{ReadCursor, WriteBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareDataPduType {
    Synchronize,
    Control,
    FontList,
    FontMap,
    /// Client → server: redraw requested areas (MS-RDPBCGR 2.2.11.2).
    RefreshRect,
    /// Client → server: pause/resume display updates (MS-RDPBCGR 2.2.11.3).
    SuppressOutput,
}

impl ShareDataPduType {
    fn as_u8(self) -> u8 {
        match self {
            Self::Control => 0x14,
            Self::FontList => 0x27,
            Self::FontMap => 0x28,
            Self::Synchronize => 0x1F,
            Self::RefreshRect => 0x21,
            Self::SuppressOutput => 0x23,
        }
    }

    fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0x14 => Self::Control,
            0x27 => Self::FontList,
            0x28 => Self::FontMap,
            0x1F => Self::Synchronize,
            0x21 => Self::RefreshRect,
            0x23 => Self::SuppressOutput,
            _ => return None,
        })
    }
}

struct ShareDataHeader {
    stream_id: u8,
    pdu_type2: ShareDataPduType,
}

impl ShareDataHeader {
    const SIZE: usize = 8;

    fn write(&self, out: &mut Vec<u8>, inner_len: usize) {
        out.write_u8(0); // pad1
        out.write_u8(self.stream_id);
        out.write_u16_le(inner_len as u16); // uncompressedLength
        out.write_u8(self.pdu_type2.as_u8());
        out.write_u8(0); // compressedType: no compression
        out.write_u16_le(0); // compressedLength
    }

    fn decode(cursor: &mut ReadCursor<'_>) -> Result<(Self, u16), DecodeError> {
        let _pad1 = cursor.read_u8()?;
        let stream_id = cursor.read_u8()?;
        let uncompressed_length = cursor.read_u16_le()?;
        let pdu_type2 =
            ShareDataPduType::from_u8(cursor.read_u8()?).ok_or(DecodeError::InvalidValue {
                field: "share_data_header.pdu_type2",
                reason: "unrecognized data PDU subtype",
            })?;
        let _compressed_type = cursor.read_u8()?;
        let _compressed_length = cursor.read_u16_le()?;
        Ok((
            Self {
                stream_id,
                pdu_type2,
            },
            uncompressed_length,
        ))
    }
}

/// Undefined/Low/Medium/High stream priority - `Undefined` (0) is fine for
/// every finalization message and steady-state update this server sends.
pub const STREAM_UNDEFINED: u8 = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPdu {
    pub share_id: u32,
    pub pdu_source: u16,
    pub stream_id: u8,
    pub pdu_type2: ShareDataPduType,
    pub body: Vec<u8>,
}

impl DataPdu {
    pub fn encode(&self) -> Vec<u8> {
        let mut inner = Vec::with_capacity(ShareDataHeader::SIZE + self.body.len());
        ShareDataHeader {
            stream_id: self.stream_id,
            pdu_type2: self.pdu_type2,
        }
        .write(&mut inner, self.body.len());
        inner.write_slice(&self.body);

        let mut out = Vec::with_capacity(ShareControlHeader::SIZE + inner.len());
        ShareControlHeader {
            pdu_type: ShareControlPduType::Data,
            pdu_source: self.pdu_source,
            share_id: self.share_id,
        }
        .write(&mut out, inner.len());
        out.write_slice(&inner);
        out
    }

    /// Body length comes from Share Control `totalLength`, not from
    /// `uncompressedLength`. mstsc sets `uncompressedLength` to
    /// (Share Data Header tail + payload) — e.g. 8 for a 4-byte Synchronize
    /// — so treating that field as the post-header body size fails with
    /// "needed 8, only 4 remaining". IronRDP ignores `uncompressedLength`
    /// on decode for the same reason. Trailing padding inside `totalLength`
    /// is kept in `body`; type-specific decoders only read what they need.
    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        let (header, declared_body_len) = ShareControlHeader::decode(&mut cursor)?;
        if header.pdu_type != ShareControlPduType::Data {
            return Err(DecodeError::InvalidValue {
                field: "data_pdu.share_control_header.pdu_type",
                reason: "expected DataPdu",
            });
        }
        let before_share_data = cursor.pos();
        let (share_data_header, _uncompressed_length) = ShareDataHeader::decode(&mut cursor)?;
        let share_data_consumed = cursor.pos() - before_share_data;
        let body_len = declared_body_len
            .saturating_sub(share_data_consumed)
            .min(cursor.remaining());
        let body = cursor.read_slice(body_len)?.to_vec();
        Ok(Self {
            share_id: header.share_id,
            pdu_source: header.pdu_source,
            stream_id: share_data_header.stream_id,
            pdu_type2: share_data_header.pdu_type2,
            body,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SynchronizePdu {
    pub target_user: u16,
}

impl SynchronizePdu {
    pub fn encode_body(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4);
        out.write_u16_le(1); // messageType, must be 1
        out.write_u16_le(self.target_user);
        out
    }

    pub fn decode_body(body: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(body);
        let message_type = cursor.read_u16_le()?;
        if message_type != 1 {
            return Err(DecodeError::InvalidValue {
                field: "synchronize_pdu.message_type",
                reason: "must be 1",
            });
        }
        Ok(Self {
            target_user: cursor.read_u16_le()?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlPdu {
    pub action: u16,
    pub grant_id: u16,
    pub control_id: u32,
}

impl ControlPdu {
    pub const COOPERATE: u16 = 4;
    pub const REQUEST_CONTROL: u16 = 1;
    pub const GRANTED_CONTROL: u16 = 2;

    pub fn encode_body(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8);
        out.write_u16_le(self.action);
        out.write_u16_le(self.grant_id);
        out.write_u32_le(self.control_id);
        out
    }

    pub fn decode_body(body: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(body);
        Ok(Self {
            action: cursor.read_u16_le()?,
            grant_id: cursor.read_u16_le()?,
            control_id: cursor.read_u32_le()?,
        })
    }
}

/// Same shape for both directions: Font List (client -> server, content not
/// validated beyond structural decode) and Font Map (server -> client,
/// `FontPdu::default()`-equivalent: `flags = FIRST | LAST`, `entry_size = 4`,
/// matching a real, interop-proven server's shipped defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FontPdu {
    pub number_fonts: u16,
    pub total_num_fonts: u16,
    pub flags: u16,
    pub entry_size: u16,
}

impl FontPdu {
    pub const FIRST: u16 = 1;
    pub const LAST: u16 = 2;

    pub fn font_map_default() -> Self {
        Self {
            number_fonts: 0,
            total_num_fonts: 0,
            flags: Self::FIRST | Self::LAST,
            entry_size: 4,
        }
    }

    pub fn encode_body(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8);
        out.write_u16_le(self.number_fonts);
        out.write_u16_le(self.total_num_fonts);
        out.write_u16_le(self.flags);
        out.write_u16_le(self.entry_size);
        out
    }

    pub fn decode_body(body: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(body);
        Ok(Self {
            number_fonts: cursor.read_u16_le()?,
            total_num_fonts: cursor.read_u16_le()?,
            flags: cursor.read_u16_le()?,
            entry_size: cursor.read_u16_le()?,
        })
    }
}

/// Inclusive rectangle from Refresh Rect / Suppress Output (MS-RDPBCGR 2.2.11.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InclusiveRect {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

impl InclusiveRect {
    pub fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            left: cursor.read_u16_le()?,
            top: cursor.read_u16_le()?,
            right: cursor.read_u16_le()?,
            bottom: cursor.read_u16_le()?,
        })
    }
}

/// Parse `TS_SUPPRESS_OUTPUT_PDU` body after the Share Data Header.
/// Returns whether display updates are allowed.
pub fn decode_suppress_output(body: &[u8]) -> Result<bool, DecodeError> {
    let mut cursor = ReadCursor::new(body);
    let allow = cursor.read_u8()?;
    let _pad0 = cursor.read_u8()?;
    let _pad1 = cursor.read_u8()?;
    let _pad2 = cursor.read_u8()?;
    match allow {
        0 => Ok(false),
        1 => {
            if cursor.remaining() >= 8 {
                let _ = InclusiveRect::decode(&mut cursor)?;
            }
            Ok(true)
        }
        _ => Err(DecodeError::InvalidValue {
            field: "suppress_output.allow_display_updates",
            reason: "expected 0 or 1",
        }),
    }
}

/// Parse `TS_REFRESH_RECT_PDU` body after the Share Data Header.
pub fn decode_refresh_rect(body: &[u8]) -> Result<Vec<InclusiveRect>, DecodeError> {
    let mut cursor = ReadCursor::new(body);
    let count = cursor.read_u8()?;
    let _pad0 = cursor.read_u8()?;
    let _pad1 = cursor.read_u8()?;
    let _pad2 = cursor.read_u8()?;
    let mut rects = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        rects.push(InclusiveRect::decode(&mut cursor)?);
    }
    Ok(rects)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_mstsc_synchronize_with_header_inclusive_uncompressed_length() {
        // Captured from mstsc after Confirm Active: uncompressedLength=8
        // (Share Data Header tail 4 + Synchronize body 4), not the post-header
        // body alone. Using that field as body length previously failed.
        #[rustfmt::skip]
        let user_data: &[u8] = &[
            0x16, 0x00, 0x17, 0x00, 0xea, 0x03, 0x00, 0x00, 0x01, 0x00,
            0x00, 0x01, 0x08, 0x00, 0x1f, 0x00, 0x00, 0x00,
            0x01, 0x00, 0xeb, 0x03,
        ];
        let decoded = DataPdu::decode(user_data).unwrap();
        assert_eq!(decoded.pdu_type2, ShareDataPduType::Synchronize);
        assert_eq!(
            SynchronizePdu::decode_body(&decoded.body).unwrap(),
            SynchronizePdu { target_user: 1003 }
        );
    }

    #[test]
    fn synchronize_round_trip_via_data_pdu() {
        let pdu = DataPdu {
            share_id: 0x1000,
            pdu_source: 1003,
            stream_id: STREAM_UNDEFINED,
            pdu_type2: ShareDataPduType::Synchronize,
            body: SynchronizePdu { target_user: 0 }.encode_body(),
        };
        let encoded = pdu.encode();
        let decoded = DataPdu::decode(&encoded).unwrap();
        assert_eq!(decoded, pdu);
        assert_eq!(
            SynchronizePdu::decode_body(&decoded.body).unwrap(),
            SynchronizePdu { target_user: 0 }
        );
    }

    #[test]
    fn control_round_trip_via_data_pdu() {
        let control = ControlPdu {
            action: ControlPdu::GRANTED_CONTROL,
            grant_id: 1002,
            control_id: 0x03EA,
        };
        let pdu = DataPdu {
            share_id: 0x1000,
            pdu_source: 1003,
            stream_id: STREAM_UNDEFINED,
            pdu_type2: ShareDataPduType::Control,
            body: control.encode_body(),
        };
        let decoded = DataPdu::decode(&pdu.encode()).unwrap();
        assert_eq!(ControlPdu::decode_body(&decoded.body).unwrap(), control);
    }

    #[test]
    fn font_map_default_round_trip() {
        let font_map = FontPdu::font_map_default();
        assert_eq!(
            FontPdu::decode_body(&font_map.encode_body()).unwrap(),
            font_map
        );
        assert_eq!(font_map.flags, FontPdu::FIRST | FontPdu::LAST);
        assert_eq!(font_map.entry_size, 4);
    }

    #[test]
    fn data_pdu_tolerates_trailing_padding() {
        let mut encoded = DataPdu {
            share_id: 0x1000,
            pdu_source: 1003,
            stream_id: STREAM_UNDEFINED,
            pdu_type2: ShareDataPduType::FontList,
            body: FontPdu {
                number_fonts: 0,
                total_num_fonts: 0,
                flags: FontPdu::FIRST | FontPdu::LAST,
                entry_size: 0x32,
            }
            .encode_body(),
        }
        .encode();
        encoded.extend_from_slice(&[0xAA; 4]); // pretend padding some client tacked on
        let decoded = DataPdu::decode(&encoded).unwrap();
        assert_eq!(decoded.body.len(), 8);
    }
}
