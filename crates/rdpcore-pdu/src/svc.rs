//! Static virtual channel data framing (MS-RDPBCGR 2.2.6.1, "Channel PDU
//! Header"): every chunk of static-virtual-channel payload (rdpsnd,
//! cliprdr, ...) - as opposed to the MCS-level PDUs in `mcs.rs` - is
//! prefixed with an 8-byte header giving the *total* reassembled length
//! plus first/last chunk flags, independent of and in addition to the
//! MCS Send Data Request/Indication framing that wraps each individual
//! chunk on the wire.
//!
//! Skipping this (sending raw channel bytes directly as MCS `data`) is a
//! real, silent-until-tested bug: a real client reads the first 8 bytes
//! of any static-channel payload as this header regardless, so it
//! misinterprets the channel protocol's own leading bytes as a length/
//! flags pair - typically producing a huge bogus "total length" and
//! corrupting reassembly (confirmed by crashing a real client's channel
//! stream-capacity check during this crate's own development).

use crate::cursor::{ReadCursor, WriteBuf};
use crate::DecodeError;

pub const CHANNEL_FLAG_FIRST: u32 = 0x0000_0001;
pub const CHANNEL_FLAG_LAST: u32 = 0x0000_0002;

/// MS-RDPBCGR's recommended default chunk size, used whenever a server
/// doesn't negotiate a different `VCChunkSize` via the Virtual Channel
/// capability set (this one doesn't, yet - see
/// `rdpcore_connector`'s `VirtualChannelCapability` usage).
pub const DEFAULT_CHUNK_LENGTH: usize = 1600;

/// Splits `data` into one-or-more Channel-PDU-Header-prefixed chunks, each
/// ready to become its own MCS Send Data Request/Indication payload.
pub fn chunkify(data: &[u8]) -> Vec<Vec<u8>> {
    let total_length = data.len() as u32;
    let body_chunks: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(DEFAULT_CHUNK_LENGTH).collect()
    };
    let count = body_chunks.len();

    body_chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let mut flags = 0u32;
            if i == 0 {
                flags |= CHANNEL_FLAG_FIRST;
            }
            if i == count - 1 {
                flags |= CHANNEL_FLAG_LAST;
            }
            let mut out = Vec::with_capacity(8 + chunk.len());
            out.write_u32_le(total_length);
            out.write_u32_le(flags);
            out.write_slice(chunk);
            out
        })
        .collect()
}

/// Strips one chunk's Channel PDU Header, returning `(total_length,
/// flags, chunk_body)`. Full multi-chunk reassembly is the caller's job
/// if it ever needs to receive something larger than one chunk; every
/// incoming message this codebase currently handles fits in a single
/// `FIRST | LAST` chunk.
pub fn dechunkify(input: &[u8]) -> Result<(u32, u32, &[u8]), DecodeError> {
    let mut cursor = ReadCursor::new(input);
    let total_length = cursor.read_u32_le()?;
    let flags = cursor.read_u32_le()?;
    Ok((total_length, flags, cursor.read_rest()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_payload_is_a_single_first_and_last_chunk() {
        let data = b"hello rdpsnd";
        let chunks = chunkify(data);
        assert_eq!(chunks.len(), 1);
        let (total_length, flags, body) = dechunkify(&chunks[0]).unwrap();
        assert_eq!(total_length as usize, data.len());
        assert_eq!(flags, CHANNEL_FLAG_FIRST | CHANNEL_FLAG_LAST);
        assert_eq!(body, data);
    }

    #[test]
    fn large_payload_splits_with_correct_first_middle_last_flags() {
        let data = vec![0xAB; DEFAULT_CHUNK_LENGTH * 2 + 100];
        let chunks = chunkify(&data);
        assert_eq!(chunks.len(), 3);

        let (total0, flags0, body0) = dechunkify(&chunks[0]).unwrap();
        assert_eq!(total0 as usize, data.len());
        assert_eq!(flags0, CHANNEL_FLAG_FIRST);
        assert_eq!(body0.len(), DEFAULT_CHUNK_LENGTH);

        let (_, flags1, body1) = dechunkify(&chunks[1]).unwrap();
        assert_eq!(flags1, 0); // neither first nor last
        assert_eq!(body1.len(), DEFAULT_CHUNK_LENGTH);

        let (_, flags2, body2) = dechunkify(&chunks[2]).unwrap();
        assert_eq!(flags2, CHANNEL_FLAG_LAST);
        assert_eq!(body2.len(), 100);

        let mut reassembled: Vec<u8> = Vec::new();
        reassembled.extend(body0);
        reassembled.extend(body1);
        reassembled.extend(body2);
        assert_eq!(reassembled, data);
    }
}
