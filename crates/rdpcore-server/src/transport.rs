//! Byte-stream framing: how many bytes make up "one PDU" for each of the
//! two wire formats in play - TPKT (everything during the connection
//! sequence) and fast-path (everything at steady state). Pure I/O, no PDU
//! interpretation - that's `rdpcore-connector`/`rdpcore-pdu`'s job.

use tokio::io::{AsyncRead, AsyncReadExt};

/// Reads one complete TPKT-framed unit (the 4-byte header plus however many
/// bytes its `packet_length` field declares), header included - this is
/// exactly the byte slice `rdpcore_connector::Acceptor::step` and
/// `rdpcore_pdu::x224` expect.
pub async fn read_tpkt_frame<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header).await?;
    let packet_length = usize::from(u16::from_be_bytes([header[2], header[3]]));
    let mut rest = vec![0u8; packet_length.saturating_sub(header.len())];
    reader.read_exact(&mut rest).await?;
    let mut frame = header.to_vec();
    frame.extend(rest);
    Ok(frame)
}

/// Reads one complete fast-path-framed unit (action/flags byte + the
/// 1-or-2-byte PER length + that many bytes total, header included) -
/// matches `rdpcore_pdu::fastpath`'s framing exactly. Assumes the header
/// byte hasn't been read yet; see [`read_steady_state_frame`] for the
/// steady-state case, where the header byte must be peeked first to tell
/// this framing apart from TPKT.
async fn read_fastpath_frame_after<R: AsyncRead + Unpin>(
    reader: &mut R,
    header_byte: u8,
) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![header_byte, 0u8];
    reader.read_exact(&mut buf[1..2]).await?;

    let total_length = if buf[1] & 0x80 == 0 {
        usize::from(buf[1])
    } else {
        let mut third = [0u8; 1];
        reader.read_exact(&mut third).await?;
        buf.push(third[0]);
        ((usize::from(buf[1]) & 0x7F) << 8) | usize::from(third[0])
    };

    let mut rest = vec![0u8; total_length.saturating_sub(buf.len())];
    reader.read_exact(&mut rest).await?;
    buf.extend(rest);
    Ok(buf)
}

/// Reads one complete TPKT-framed unit given that the leading `0x03`
/// version byte has already been consumed (see [`read_steady_state_frame`]).
async fn read_tpkt_frame_after<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut rest_of_header = [0u8; 3]; // reserved(1) + packet_length(2)
    reader.read_exact(&mut rest_of_header).await?;
    let packet_length = usize::from(u16::from_be_bytes([rest_of_header[1], rest_of_header[2]]));
    let mut rest = vec![0u8; packet_length.saturating_sub(4)];
    reader.read_exact(&mut rest).await?;

    let mut frame = Vec::with_capacity(packet_length);
    frame.push(0x03);
    frame.extend_from_slice(&rest_of_header);
    frame.extend(rest);
    Ok(frame)
}

/// At steady state, incoming bytes are *not* uniformly one framing: fast-
/// path input (fast-path's whole reason to exist - no TPKT/X.224/MCS
/// overhead) shares the wire with ordinary TPKT-framed static-channel
/// traffic (e.g. rdpsnd's `WaveConfirm`/format negotiation replies), which
/// never switches to fast-path. The two are told apart by a single leading
/// byte: TPKT always starts with version `0x03`; a fast-path header byte's
/// low 2 bits (the `action` field) are `0` for input, and `0x03` can never
/// occur there in practice (real clients only ever send action `0`) - so
/// peeking that one byte is enough to dispatch correctly.
pub enum SteadyStateFrame {
    FastPathInput(Vec<u8>),
    SlowPath(Vec<u8>),
}

pub async fn read_steady_state_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<SteadyStateFrame> {
    let mut header_byte = [0u8; 1];
    reader.read_exact(&mut header_byte).await?;
    if header_byte[0] == 0x03 {
        Ok(SteadyStateFrame::SlowPath(
            read_tpkt_frame_after(reader).await?,
        ))
    } else {
        Ok(SteadyStateFrame::FastPathInput(
            read_fastpath_frame_after(reader, header_byte[0]).await?,
        ))
    }
}
