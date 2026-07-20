//! Fast-Path Input (client -> server) and Fast-Path Update/Output (server ->
//! client) PDUs (MS-RDPBCGR 2.2.8.1.2 / 2.2.9.1.2.1) - the steady-state
//! wire format once the connection sequence completes. No encryption
//! support (this server only ever negotiates TLS/`PROTOCOL_SSL`, so the
//! legacy per-packet FIPS/MAC fields never apply).
//!
//! Input event coverage is intentionally narrow: only Scancode, Mouse,
//! Sync, and Unicode (needed for CJK/IME text input - see
//! `kmsrdp::x11_unicode`) are decoded. Every other event type (MouseX,
//! MouseRel, QoeTimestamp) is gated behind an `InputCapability` flag this
//! server deliberately doesn't advertise (see
//! `capability_sets::InputCapability`), so a well-behaved client never
//! sends them; encountering one anyway is treated as a hard decode error
//! rather than guessed at, since fast-path event bodies have type-dependent
//! sizes and guessing wrong would desync the whole PDU.

use crate::cursor::{ReadCursor, WriteBuf};
use crate::{DecodeError, per};

/// The chunk size this server uses to fragment large updates (e.g. a
/// full-screen bitmap), matching a real implementation's own hardcoded
/// wire-chunking constant - chosen to comfortably fit typical MTU-friendly
/// TCP segments, independent of the much larger `MaxRequestSize` advertised
/// via the `MultiFragmentUpdate` capability set.
pub const MAX_FASTPATH_CHUNK_SIZE: usize = 16_374;

pub const UPDATE_CODE_BITMAP: u8 = 0x1;
pub const UPDATE_CODE_SURFACE_COMMANDS: u8 = 0x4;

// ---------------------------------------------------------------------
// Fast-Path Input (decode-only in production; encode kept for tests)
// ---------------------------------------------------------------------

pub mod keyboard_flags {
    pub const RELEASE: u8 = 0x01;
    pub const EXTENDED: u8 = 0x02;
    pub const EXTENDED1: u8 = 0x04;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastPathInputEvent {
    Scancode {
        flags: u8,
        code: u8,
    },
    Mouse {
        pointer_flags: u16,
        x: u16,
        y: u16,
    },
    Sync {
        flags: u8,
    },
    /// `TS_UNICODE_KEYBOARD_EVENT` (MS-RDPBCGR 2.2.8.1.2.2.2) - `code` is a
    /// single UTF-16 code unit, not a full codepoint; characters outside
    /// the BMP arrive as a surrogate pair across two events. `flags` only
    /// ever carries `keyboard_flags::RELEASE`, never `EXTENDED[1]`.
    Unicode {
        flags: u8,
        code: u16,
    },
}

impl FastPathInputEvent {
    fn encode(&self, out: &mut Vec<u8>) {
        match *self {
            Self::Scancode { flags, code } => {
                out.write_u8(flags & 0x1F);
                out.write_u8(code);
            }
            Self::Mouse {
                pointer_flags,
                x,
                y,
            } => {
                out.write_u8(1 << 5);
                out.write_u16_le(pointer_flags);
                out.write_u16_le(x);
                out.write_u16_le(y);
            }
            Self::Sync { flags } => {
                out.write_u8((flags & 0x1F) | (3 << 5));
            }
            Self::Unicode { flags, code } => {
                out.write_u8((flags & 0x1F) | (4 << 5));
                out.write_u16_le(code);
            }
        }
    }

    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        let header = cursor.read_u8()?;
        let event_flags = header & 0x1F;
        let event_code = header >> 5;
        match event_code {
            0 => Ok(Self::Scancode {
                flags: event_flags,
                code: cursor.read_u8()?,
            }),
            1 => Ok(Self::Mouse {
                pointer_flags: cursor.read_u16_le()?,
                x: cursor.read_u16_le()?,
                y: cursor.read_u16_le()?,
            }),
            3 => Ok(Self::Sync { flags: event_flags }),
            4 => Ok(Self::Unicode {
                flags: event_flags,
                code: cursor.read_u16_le()?,
            }),
            _ => Err(DecodeError::InvalidValue {
                field: "fast_path_input_event.code",
                reason: "unsupported or unadvertised event code",
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastPathInput {
    pub events: Vec<FastPathInputEvent>,
}

impl FastPathInput {
    pub fn encode(&self) -> Vec<u8> {
        let mut events_bytes = Vec::new();
        for event in &self.events {
            event.encode(&mut events_bytes);
        }

        let count = self.events.len();
        let (num_events_nibble, extra_count_byte) = if count < 15 {
            (count as u8, false)
        } else {
            (0, true)
        };

        let fixed_prefix = 1 + usize::from(extra_count_byte) + events_bytes.len();
        let total = if fixed_prefix < 0x7F {
            fixed_prefix + 1
        } else {
            fixed_prefix + 2
        };

        let mut out = Vec::with_capacity(total);
        out.write_u8(num_events_nibble << 2); // action=0, flags=0
        per::write_length(&mut out, total);
        if extra_count_byte {
            out.write_u8(count as u8);
        }
        out.write_slice(&events_bytes);
        out
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        let header = cursor.read_u8()?;
        if header & 0x03 != 0 {
            return Err(DecodeError::InvalidValue {
                field: "fast_path_input.action",
                reason: "expected FASTPATH_INPUT",
            });
        }
        if (header >> 6) & 0x03 != 0 {
            return Err(DecodeError::InvalidValue {
                field: "fast_path_input.flags",
                reason: "encryption is not supported",
            });
        }
        let num_events_nibble = (header >> 2) & 0x0F;
        let _total_length = per::read_length(&mut cursor)?;
        let num_events = if num_events_nibble == 0 {
            cursor.read_u8()?
        } else {
            num_events_nibble
        };
        let events = (0..num_events)
            .map(|_| FastPathInputEvent::decode(&mut cursor))
            .collect::<Result<_, _>>()?;
        Ok(Self { events })
    }
}

// ---------------------------------------------------------------------
// Fast-Path Output/Update (encode-only in production; decode kept for tests)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fragmentation {
    Single,
    First,
    Next,
    Last,
}

impl Fragmentation {
    fn as_u8(self) -> u8 {
        match self {
            Self::Single => 0,
            Self::Last => 1,
            Self::First => 2,
            Self::Next => 3,
        }
    }

    fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Single,
            1 => Self::Last,
            2 => Self::First,
            3 => Self::Next,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastPathUpdatePdu {
    pub update_code: u8,
    pub fragmentation: Fragmentation,
    pub data: Vec<u8>,
}

impl FastPathUpdatePdu {
    fn encode(&self, out: &mut Vec<u8>) {
        let header = (self.update_code & 0x0F) | ((self.fragmentation.as_u8() & 0x3) << 4);
        out.write_u8(header);
        out.write_u16_le(self.data.len() as u16);
        out.write_slice(&self.data);
    }

    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        let header = cursor.read_u8()?;
        let update_code = header & 0x0F;
        let fragmentation =
            Fragmentation::from_u8((header >> 4) & 0x3).ok_or(DecodeError::InvalidValue {
                field: "fast_path_update.fragmentation",
                reason: "invalid fragmentation value",
            })?;
        if (header >> 6) & 0x2 != 0 {
            return Err(DecodeError::InvalidValue {
                field: "fast_path_update.compression",
                reason: "compression is not supported",
            });
        }
        let size = cursor.read_u16_le()?;
        let data = cursor.read_slice(usize::from(size))?.to_vec();
        Ok(Self {
            update_code,
            fragmentation,
            data,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastPathOutput {
    pub updates: Vec<FastPathUpdatePdu>,
}

impl FastPathOutput {
    pub fn encode(&self) -> Vec<u8> {
        let mut inner = Vec::new();
        for update in &self.updates {
            update.encode(&mut inner);
        }

        let fixed_prefix = 1 + inner.len();
        let total = if fixed_prefix < 0x7F {
            fixed_prefix + 1
        } else {
            fixed_prefix + 2
        };

        let mut out = Vec::with_capacity(total);
        out.write_u8(0); // action=0, encryption flags=0
        per::write_length(&mut out, total);
        out.write_slice(&inner);
        out
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        let header = cursor.read_u8()?;
        if header & 0x03 != 0 {
            return Err(DecodeError::InvalidValue {
                field: "fast_path_output.action",
                reason: "expected FASTPATH_OUTPUT",
            });
        }
        if (header >> 6) & 0x03 != 0 {
            return Err(DecodeError::InvalidValue {
                field: "fast_path_output.flags",
                reason: "encryption is not supported",
            });
        }
        let total_length = per::read_length(&mut cursor)?;
        let consumed_before_updates = cursor.pos();
        let updates_len = total_length.saturating_sub(consumed_before_updates);
        let updates_bytes = cursor.read_slice(updates_len)?;

        let mut updates_cursor = ReadCursor::new(updates_bytes);
        let mut updates = Vec::new();
        while updates_cursor.remaining() > 0 {
            updates.push(FastPathUpdatePdu::decode(&mut updates_cursor)?);
        }
        Ok(Self { updates })
    }
}

// ---------------------------------------------------------------------
// Bitmap Update body (rides inside a FastPathUpdatePdu with
// update_code == UPDATE_CODE_BITMAP)
// ---------------------------------------------------------------------

const BITMAP_UPDATE_TYPE: u16 = 0x0001;

const BITMAP_COMPRESSION: u16 = 0x0001;

/// `data` is bottom-up (first row = bottom scanline) per the classic
/// `TS_BITMAP_DATA` convention - get this backwards and the image renders
/// upside down. `compressed_scan_width`: `None` for raw/uncompressed
/// `data` (`flags` written as 0, `data.len()` is `bitmapLength` directly);
/// `Some(scan_width_bytes)` when `data` is an RDP6-Planar-compressed
/// stream (`crate::rdp6::encode`) - `BITMAP_COMPRESSION` is set and an
/// 8-byte `bitmapComprHdr` precedes `data` (MS-RDPBCGR
/// 2.2.9.1.1.3.1.2.3). `cbScanWidth` is the scan line width in **bytes**
/// (must be divisible by 4), typically `width * bytes_per_pixel`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapRect {
    pub dest_left: u16,
    pub dest_top: u16,
    pub dest_right: u16,
    pub dest_bottom: u16,
    pub width: u16,
    pub height: u16,
    pub bits_per_pixel: u16,
    pub data: Vec<u8>,
    pub compressed_scan_width: Option<u16>,
}

impl BitmapRect {
    fn encode(&self, out: &mut Vec<u8>) {
        out.write_u16_le(self.dest_left);
        out.write_u16_le(self.dest_top);
        out.write_u16_le(self.dest_right);
        out.write_u16_le(self.dest_bottom);
        out.write_u16_le(self.width);
        out.write_u16_le(self.height);
        out.write_u16_le(self.bits_per_pixel);

        match self.compressed_scan_width {
            Some(scan_width_bytes) => {
                // `bitmapLength` is a 16-bit field, same truncation risk
                // as the raw path below - compressed tiles are expected
                // to always be far smaller than the raw source anyway.
                let bitmap_length = 8 + self.data.len();
                debug_assert!(
                    bitmap_length <= usize::from(u16::MAX),
                    "compressed BitmapRect ({bitmap_length} bytes incl. header) exceeds the 16-bit bitmapLength field"
                );
                debug_assert!(
                    scan_width_bytes.is_multiple_of(4),
                    "cbScanWidth ({scan_width_bytes}) must be divisible by 4 (MS-RDPBCGR 2.2.9.1.1.3.1.2.3)"
                );
                let uncompressed_size =
                    usize::from(self.height) * usize::from(scan_width_bytes);
                out.write_u16_le(BITMAP_COMPRESSION);
                out.write_u16_le(bitmap_length as u16);
                out.write_u16_le(0); // cbCompFirstRowSize, fixed 0
                out.write_u16_le(self.data.len() as u16); // cbCompMainBodySize
                out.write_u16_le(scan_width_bytes); // cbScanWidth (bytes)
                out.write_u16_le(uncompressed_size as u16); // cbUncompressedSize
                out.write_slice(&self.data);
            }
            None => {
                // A rect covering more than ~65535 bytes of raw pixel
                // data (e.g. a naive single rect for a whole 1024x768x32bpp
                // frame, 3MB+) would silently truncate here (`as u16`
                // wraps, it doesn't panic in release builds) rather than
                // erroring, corrupting the PDU in a way that looks fine
                // until a real client fails to parse it. Callers must tile
                // large updates into small-enough rectangles (e.g. 64x64)
                // before building one of these - see `rdpcore-server`'s
                // `encode_bitmap_update`.
                debug_assert!(
                    self.data.len() <= usize::from(u16::MAX),
                    "BitmapRect::data ({} bytes) exceeds the 16-bit bitmapLength field - tile the update first",
                    self.data.len()
                );
                out.write_u16_le(0); // flags: no compression
                out.write_u16_le(self.data.len() as u16);
                out.write_slice(&self.data);
            }
        }
    }

    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        let dest_left = cursor.read_u16_le()?;
        let dest_top = cursor.read_u16_le()?;
        let dest_right = cursor.read_u16_le()?;
        let dest_bottom = cursor.read_u16_le()?;
        let width = cursor.read_u16_le()?;
        let height = cursor.read_u16_le()?;
        let bits_per_pixel = cursor.read_u16_le()?;
        let flags = cursor.read_u16_le()?;
        let bitmap_length = cursor.read_u16_le()?;

        let (data, compressed_scan_width) = if flags & BITMAP_COMPRESSION != 0 {
            let _cb_comp_first_row_size = cursor.read_u16_le()?;
            let cb_comp_main_body_size = cursor.read_u16_le()?;
            let cb_scan_width = cursor.read_u16_le()?;
            let _cb_uncompressed_size = cursor.read_u16_le()?;
            let data = cursor
                .read_slice(usize::from(cb_comp_main_body_size))?
                .to_vec();
            (data, Some(cb_scan_width))
        } else {
            (
                cursor.read_slice(usize::from(bitmap_length))?.to_vec(),
                None,
            )
        };

        Ok(Self {
            dest_left,
            dest_top,
            dest_right,
            dest_bottom,
            width,
            height,
            bits_per_pixel,
            data,
            compressed_scan_width,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapUpdateData {
    pub rectangles: Vec<BitmapRect>,
}

impl BitmapUpdateData {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.write_u16_le(BITMAP_UPDATE_TYPE);
        out.write_u16_le(self.rectangles.len() as u16);
        for rect in &self.rectangles {
            rect.encode(&mut out);
        }
        out
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        let update_type = cursor.read_u16_le()?;
        if update_type & BITMAP_UPDATE_TYPE == 0 {
            return Err(DecodeError::InvalidValue {
                field: "bitmap_update_data.update_type",
                reason: "expected the BITMAP_UPDATE_TYPE bit set",
            });
        }
        let count = cursor.read_u16_le()?;
        let rectangles = (0..count)
            .map(|_| BitmapRect::decode(&mut cursor))
            .collect::<Result<_, _>>()?;
        Ok(Self { rectangles })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scancode_event_round_trip() {
        let input = FastPathInput {
            events: vec![FastPathInputEvent::Scancode {
                flags: 0,
                code: 0x1E, // 'A'
            }],
        };
        let encoded = input.encode();
        assert_eq!(FastPathInput::decode(&encoded).unwrap(), input);
    }

    #[test]
    fn mouse_event_round_trip() {
        let input = FastPathInput {
            events: vec![FastPathInputEvent::Mouse {
                pointer_flags: 0x8000 | 0x1000, // DOWN | LEFT_BUTTON
                x: 640,
                y: 360,
            }],
        };
        let encoded = input.encode();
        assert_eq!(FastPathInput::decode(&encoded).unwrap(), input);
    }

    #[test]
    fn sync_event_round_trip() {
        let input = FastPathInput {
            events: vec![FastPathInputEvent::Sync { flags: 0x01 }],
        };
        let encoded = input.encode();
        assert_eq!(FastPathInput::decode(&encoded).unwrap(), input);
    }

    #[test]
    fn many_events_use_the_explicit_count_byte() {
        let input = FastPathInput {
            events: (0..20)
                .map(|i| FastPathInputEvent::Scancode { flags: 0, code: i })
                .collect(),
        };
        let encoded = input.encode();
        // action=0, numEvents nibble = 0 (sentinel) -> byte value 0.
        assert_eq!(encoded[0], 0);
        assert_eq!(FastPathInput::decode(&encoded).unwrap(), input);
    }

    #[test]
    fn unicode_event_round_trip() {
        let input = FastPathInput {
            events: vec![FastPathInputEvent::Unicode {
                flags: 0,
                code: 0x3042, // U+3042 HIRAGANA LETTER A
            }],
        };
        let encoded = input.encode();
        assert_eq!(FastPathInput::decode(&encoded).unwrap(), input);
    }

    #[test]
    fn rejects_unadvertised_event_code() {
        // event_code=2 (MouseX), which this server never advertises support for.
        let encoded = vec![
            1 << 2, // header: 1 event
            3,      // 1-byte PER length
            2 << 5, // event header: code=2 (MouseX), flags=0
        ];
        assert!(FastPathInput::decode(&encoded).is_err());
    }

    #[test]
    fn bitmap_update_round_trip_via_fastpath_output() {
        let bitmap = BitmapUpdateData {
            rectangles: vec![BitmapRect {
                dest_left: 0,
                dest_top: 0,
                dest_right: 63,
                dest_bottom: 63,
                width: 64,
                height: 64,
                bits_per_pixel: 32,
                data: vec![0xABu8; 64 * 64 * 4],
                compressed_scan_width: None,
            }],
        };
        let output = FastPathOutput {
            updates: vec![FastPathUpdatePdu {
                update_code: UPDATE_CODE_BITMAP,
                fragmentation: Fragmentation::Single,
                data: bitmap.encode(),
            }],
        };
        let encoded = output.encode();
        let decoded = FastPathOutput::decode(&encoded).unwrap();
        assert_eq!(decoded, output);
        assert_eq!(
            BitmapUpdateData::decode(&decoded.updates[0].data).unwrap(),
            bitmap
        );
    }

    #[test]
    fn compressed_bitmap_rect_round_trips_with_compression_header() {
        let compressed_payload = vec![crate::rdp6::FORMAT_HEADER_RLE_NO_ALPHA_ARGB, 0x10, 0xAB];
        let bitmap = BitmapUpdateData {
            rectangles: vec![BitmapRect {
                dest_left: 0,
                dest_top: 0,
                dest_right: 63,
                dest_bottom: 63,
                width: 64,
                height: 64,
                bits_per_pixel: 32,
                data: compressed_payload.clone(),
                compressed_scan_width: Some(64 * 4), // bytes (cbScanWidth)
            }],
        };
        let encoded = bitmap.encode();
        let decoded = BitmapUpdateData::decode(&encoded).unwrap();
        assert_eq!(decoded, bitmap);
        assert_eq!(decoded.rectangles[0].data, compressed_payload);
        assert_eq!(decoded.rectangles[0].compressed_scan_width, Some(64 * 4));
        // Layout: updateType(2) + count(2) + dest(8) + wh+bpp(6) + flags(2)
        // + bitmapLength(2) + firstRow(2) + mainBody(2) + scanWidth(2)
        // + uncompressedSize(2) = bytes [28..30].
        assert_eq!(&encoded[28..30], &(64u16 * 64 * 4).to_le_bytes());
        assert_eq!(&encoded[26..28], &(256u16).to_le_bytes()); // cbScanWidth in bytes
    }

    #[test]
    fn multiple_updates_in_one_fastpath_output_round_trip() {
        let output = FastPathOutput {
            updates: vec![
                FastPathUpdatePdu {
                    update_code: UPDATE_CODE_BITMAP,
                    fragmentation: Fragmentation::First,
                    data: vec![0x11; 10],
                },
                FastPathUpdatePdu {
                    update_code: UPDATE_CODE_BITMAP,
                    fragmentation: Fragmentation::Last,
                    data: vec![0x22; 20],
                },
            ],
        };
        let encoded = output.encode();
        assert_eq!(FastPathOutput::decode(&encoded).unwrap(), output);
    }
}
