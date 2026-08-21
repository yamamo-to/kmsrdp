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

use crate::DecodeError;
use crate::cursor::{ReadCursor, WriteBuf};
use crate::mcs::SendData;
use crate::x224;

pub const CHANNEL_FLAG_FIRST: u32 = 0x0000_0001;
pub const CHANNEL_FLAG_LAST: u32 = 0x0000_0002;

/// MS-RDPBCGR's recommended default chunk size, used whenever a server
/// doesn't negotiate a different `VCChunkSize` via the Virtual Channel
/// capability set (this one doesn't, yet - see
/// `rdpcore_connector`'s `VirtualChannelCapability` usage).
pub const DEFAULT_CHUNK_LENGTH: usize = 1600;

/// Applies static-virtual-channel chunking (MS-RDPBCGR 2.2.6.1) to `data`,
/// then wraps each resulting chunk as its own MCS Send Data Indication +
/// X.224 Data TPDU - the full framing stack a real client expects for
/// static channel traffic, in wire order, ready to write one after another.
pub fn wrap_indication(initiator: u16, channel_id: u16, data: Vec<u8>) -> Vec<Vec<u8>> {
    chunkify(&data)
        .into_iter()
        .map(|chunk| {
            x224::wrap_data(
                &SendData {
                    initiator,
                    channel_id,
                    data: chunk,
                    complete: true,
                }
                .encode_indication(),
            )
        })
        .collect()
}

/// Splits `data` into one-or-more Channel-PDU-Header-prefixed chunks, each
/// ready to become its own MCS Send Data Request/Indication payload.
pub fn chunkify(data: &[u8]) -> Vec<Vec<u8>> {
    let total_length = data.len() as u32;
    if data.is_empty() {
        let mut out = Vec::with_capacity(8);
        out.write_u32_le(total_length);
        out.write_u32_le(CHANNEL_FLAG_FIRST | CHANNEL_FLAG_LAST);
        return vec![out];
    }

    // `Chunks` is an `ExactSizeIterator`, so the chunk count needed for
    // first/last flags can be read straight off it instead of collecting
    // into a throwaway `Vec<&[u8]>` first just to call `.len()`.
    let count = data.len().div_ceil(DEFAULT_CHUNK_LENGTH);
    data.chunks(DEFAULT_CHUNK_LENGTH)
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

/// Feeds one already-dechunked SVC payload into `buffer`, accumulating
/// across calls until a `CHANNEL_FLAG_LAST` chunk completes one logical
/// message - the exact same reassembly state machine `rdpcore-dvc`,
/// `rdpcore-cliprdr`, and `rdpcore-rdpdr` each hand-rolled independently
/// (clear on FIRST, reject if the running total would exceed `max_size`,
/// extend, wait for LAST, then take the finished buffer).
///
/// Returns `Ok(Some(message))` once the message is complete, `Ok(None)`
/// while still waiting for more chunks. `field` names the caller's
/// buffer in the size-limit error, e.g. `"cliprdr.incoming_buffer"`.
pub fn reassemble(
    buffer: &mut Vec<u8>,
    payload: &[u8],
    max_size: usize,
    field: &'static str,
) -> Result<Option<Vec<u8>>, DecodeError> {
    let (_total_length, flags, chunk) = dechunkify(payload)?;
    if flags & CHANNEL_FLAG_FIRST != 0 {
        buffer.clear();
    }
    if buffer.len().saturating_add(chunk.len()) > max_size {
        buffer.clear();
        return Err(DecodeError::InvalidValue {
            field,
            reason: "reassembled SVC message exceeded maximum allowed size",
        });
    }
    buffer.extend_from_slice(chunk);
    if flags & CHANNEL_FLAG_LAST == 0 {
        return Ok(None);
    }
    Ok(Some(core::mem::take(buffer)))
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
    fn empty_payload_is_a_single_first_and_last_chunk() {
        let chunks = chunkify(&[]);
        assert_eq!(chunks.len(), 1);
        let (total_length, flags, body) = dechunkify(&chunks[0]).unwrap();
        assert_eq!(total_length, 0);
        assert_eq!(flags, CHANNEL_FLAG_FIRST | CHANNEL_FLAG_LAST);
        assert!(body.is_empty());
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

    #[test]
    fn reassemble_waits_for_the_last_chunk_then_returns_the_full_message() {
        let data = vec![0xCD; DEFAULT_CHUNK_LENGTH * 2 + 42];
        let chunks = chunkify(&data);
        assert_eq!(chunks.len(), 3);

        let mut buffer = Vec::new();
        assert_eq!(
            reassemble(&mut buffer, &chunks[0], usize::MAX, "test").unwrap(),
            None
        );
        assert_eq!(
            reassemble(&mut buffer, &chunks[1], usize::MAX, "test").unwrap(),
            None
        );
        assert_eq!(
            reassemble(&mut buffer, &chunks[2], usize::MAX, "test").unwrap(),
            Some(data)
        );
        assert!(buffer.is_empty(), "buffer must be taken, not left behind");
    }

    #[test]
    fn reassemble_rejects_messages_over_the_size_cap_and_clears_the_buffer() {
        let mut buffer = Vec::new();
        let mut first = Vec::new();
        first.extend_from_slice(&20u32.to_le_bytes()); // total_length (unchecked)
        first.extend_from_slice(&CHANNEL_FLAG_FIRST.to_le_bytes());
        first.extend_from_slice(&[0u8; 20]);

        let err = reassemble(&mut buffer, &first, 10, "test.buf").unwrap_err();
        assert!(
            matches!(
                err,
                DecodeError::InvalidValue {
                    field: "test.buf",
                    ..
                }
            ),
            "unexpected error: {err:?}"
        );
        assert!(buffer.is_empty(), "buffer must be cleared on rejection");
    }

    #[test]
    fn reassemble_drops_a_stale_partial_message_on_a_new_first_chunk() {
        // A FIRST chunk arriving mid-reassembly (client abandoned the
        // previous message, or this is simply the next one) must
        // discard whatever was buffered, not append to it.
        let mut buffer = Vec::new();
        let stale_first = {
            let mut raw = Vec::new();
            raw.extend_from_slice(&99u32.to_le_bytes());
            raw.extend_from_slice(&CHANNEL_FLAG_FIRST.to_le_bytes());
            raw.extend_from_slice(b"stale-partial");
            raw
        };
        assert_eq!(
            reassemble(&mut buffer, &stale_first, usize::MAX, "test").unwrap(),
            None
        );
        assert_eq!(buffer, b"stale-partial");

        let fresh = chunkify(b"fresh message")
            .pop()
            .expect("single chunk for a short payload");
        assert_eq!(
            reassemble(&mut buffer, &fresh, usize::MAX, "test").unwrap(),
            Some(b"fresh message".to_vec())
        );
    }
}
