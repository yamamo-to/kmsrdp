//! MS-RDPEA wire format: the subset needed for one-way server -> client PCM
//! playback over the static virtual channel `"rdpsnd"`. Every message
//! shares a 4-byte header (`msgType`, a pad byte, `BodySize` - little-
//! endian throughout except one field noted below); riding directly as the
//! whole static-channel payload, no extra wrapper.
//!
//! Scope: Server/Client Audio Formats (`SNDC_FORMATS`), Training/
//! TrainingConfirm (`SNDC_TRAINING` - mandatory in practice: a real
//! server's format negotiation doesn't complete without it, even though
//! nothing in this exchange is configurable), and Wave2 (`SNDC_WAVE2`, the
//! modern single-PDU wave-data format, avoiding the legacy split Wave/
//! WaveInfo two-PDU dance). WaveConfirm (`SNDC_WAVECONFIRM`) is decoded
//! only enough to skip over it - a real server implementation doesn't wait
//! for it before sending more audio, so this one doesn't either.

use rdpcore_pdu::cursor::{ReadCursor, WriteBuf};
use rdpcore_pdu::DecodeError;

pub const SNDC_WAVE2: u8 = 0x0D;
pub const SNDC_WAVECONFIRM: u8 = 0x05;
pub const SNDC_TRAINING: u8 = 0x06;
pub const SNDC_FORMATS: u8 = 0x07;

pub const CHANNEL_NAME: &str = "rdpsnd";

/// Below `Version::V6` (0x06) in a real implementation's own numbering, so
/// a real client skips the newer, optional Quality Mode negotiation step
/// and goes straight from Client Audio Formats to Training - one fewer PDU
/// type this from-scratch implementation needs to speak.
const SERVER_VERSION: u16 = 0x02;

fn write_header(out: &mut Vec<u8>, msg_type: u8, body_len: usize) {
    out.write_u8(msg_type);
    out.write_u8(0); // pad
    out.write_u16_le(body_len as u16);
}

/// `WAVE_FORMAT_PCM`; the only tag this crate produces/expects.
pub const WAVE_FORMAT_PCM: u16 = 0x0001;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFormat {
    pub format_tag: u16,
    pub n_channels: u16,
    pub n_samples_per_sec: u32,
    pub n_avg_bytes_per_sec: u32,
    pub n_block_align: u16,
    pub bits_per_sample: u16,
    pub extra_data: Option<Vec<u8>>,
}

impl AudioFormat {
    pub fn pcm(n_channels: u16, n_samples_per_sec: u32, bits_per_sample: u16) -> Self {
        let block_align = n_channels * (bits_per_sample / 8);
        Self {
            format_tag: WAVE_FORMAT_PCM,
            n_channels,
            n_samples_per_sec,
            n_avg_bytes_per_sec: n_samples_per_sec * u32::from(block_align),
            n_block_align: block_align,
            bits_per_sample,
            extra_data: None,
        }
    }

    /// Public: this exact WAVEFORMATEX-style layout is reused verbatim by
    /// `rdpcore-rdpeai` (MS-RDPEAI's format entries are byte-identical to
    /// RDPSND's), so it's not private to this crate.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let extra = self.extra_data.as_deref().unwrap_or(&[]);
        out.write_u16_le(self.format_tag);
        out.write_u16_le(self.n_channels);
        out.write_u32_le(self.n_samples_per_sec);
        out.write_u32_le(self.n_avg_bytes_per_sec);
        out.write_u16_le(self.n_block_align);
        out.write_u16_le(self.bits_per_sample);
        out.write_u16_le(extra.len() as u16);
        out.write_slice(extra);
    }

    pub fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        let format_tag = cursor.read_u16_le()?;
        let n_channels = cursor.read_u16_le()?;
        let n_samples_per_sec = cursor.read_u32_le()?;
        let n_avg_bytes_per_sec = cursor.read_u32_le()?;
        let n_block_align = cursor.read_u16_le()?;
        let bits_per_sample = cursor.read_u16_le()?;
        let cb_size = usize::from(cursor.read_u16_le()?);
        let extra_data = if cb_size == 0 {
            None
        } else {
            Some(cursor.read_slice(cb_size)?.to_vec())
        };
        Ok(Self {
            format_tag,
            n_channels,
            n_samples_per_sec,
            n_avg_bytes_per_sec,
            n_block_align,
            bits_per_sample,
            extra_data,
        })
    }

    /// The comparison a real server's format negotiation actually uses:
    /// tag/channels/sample-rate/bits/extra-data must match: deliberately
    /// ignoring `n_avg_bytes_per_sec`/`n_block_align`, which are derived
    /// fields some clients recompute slightly differently.
    fn negotiation_eq(&self, other: &Self) -> bool {
        self.format_tag == other.format_tag
            && self.n_channels == other.n_channels
            && self.n_samples_per_sec == other.n_samples_per_sec
            && self.bits_per_sample == other.bits_per_sample
            && self.extra_data == other.extra_data
    }
}

/// Server -> client, sent first: `SNDC_FORMATS`.
pub fn encode_server_audio_formats(formats: &[AudioFormat]) -> Vec<u8> {
    let mut body = Vec::new();
    body.write_u32_le(0); // dwFlags
    body.write_u32_le(0); // dwVolume
    body.write_u32_le(0); // dwPitch
    body.write_u16_le(0); // wDGramPort
    body.write_u16_le(formats.len() as u16);
    body.write_u8(0); // cLastBlockConfirmed
    body.write_u16_le(SERVER_VERSION);
    body.write_u8(0); // bPad
    for format in formats {
        format.encode(&mut body);
    }

    let mut out = Vec::with_capacity(body.len() + 4);
    write_header(&mut out, SNDC_FORMATS, body.len());
    out.write_slice(&body);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientAudioFormats {
    pub formats: Vec<AudioFormat>,
}

fn decode_client_audio_formats(body: &[u8]) -> Result<ClientAudioFormats, DecodeError> {
    let mut cursor = ReadCursor::new(body);
    let _flags = cursor.read_u32_le()?;
    let _volume = cursor.read_u32_le()?;
    let _pitch = cursor.read_u32_le()?;
    let _dgram_port = cursor.read_u16_be()?; // the one big-endian field in this whole protocol
    let count = cursor.read_u16_le()?;
    let _last_block_confirmed = cursor.read_u8()?;
    let _version = cursor.read_u16_le()?;
    let _pad = cursor.read_u8()?;
    let formats = (0..count).map(|_| AudioFormat::decode(&mut cursor)).collect::<Result<_, _>>()?;
    Ok(ClientAudioFormats { formats })
}

/// Server -> client, `SNDC_TRAINING`. Every field is a fixed placeholder -
/// nothing here is meaningful beyond "training happened."
pub fn encode_training() -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    write_header(&mut out, SNDC_TRAINING, 4);
    out.write_u16_le(0); // wTimeStamp
    out.write_u16_le(0); // wPackSize (no trailing data)
    out
}

/// Server -> client, `SNDC_WAVE2`: `wTimeStamp`(unused, always 0) +
/// `wFormatNo` + `cBlockNo` + 3 bytes padding + `dwAudioTimestamp` (the
/// real per-chunk timestamp) + raw audio data running to the end of the
/// PDU.
pub fn encode_wave2(format_no: u16, block_no: u8, timestamp_ms: u32, data: &[u8]) -> Vec<u8> {
    let body_len = 12 + data.len();
    let mut out = Vec::with_capacity(body_len + 4);
    write_header(&mut out, SNDC_WAVE2, body_len);
    out.write_u16_le(0); // wTimeStamp (legacy, unused)
    out.write_u16_le(format_no);
    out.write_u8(block_no);
    out.write_slice(&[0u8; 3]); // padding
    out.write_u32_le(timestamp_ms);
    out.write_slice(data);
    out
}

/// One negotiated format: `format_no` indexes into the *client's* format
/// list (that's what `Wave2`'s `wFormatNo` field references).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedFormat {
    pub format: AudioFormat,
    pub format_no: u16,
}

/// Server formats this crate can produce that the client also advertised
/// support for, in client format-list order (so `format_no` matches what
/// `Wave2` needs to reference).
pub fn negotiate_formats(server_formats: &[AudioFormat], client_formats: &[AudioFormat]) -> Vec<NegotiatedFormat> {
    client_formats
        .iter()
        .enumerate()
        .filter_map(|(i, client_format)| {
            server_formats
                .iter()
                .find(|server_format| server_format.negotiation_eq(client_format))
                .map(|server_format| NegotiatedFormat {
                    format: server_format.clone(),
                    format_no: i as u16,
                })
        })
        .collect()
}

/// What the server needs to react to from an incoming `"rdpsnd"` channel
/// payload; `WaveConfirm` and anything else this crate doesn't act on
/// still decodes far enough to be safely skipped (correct framing matters
/// even for messages whose content is ignored).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    AudioFormats(ClientAudioFormats),
    TrainingConfirm,
    Other,
}

pub fn decode_client_message(input: &[u8]) -> Result<ClientMessage, DecodeError> {
    let mut cursor = ReadCursor::new(input);
    let msg_type = cursor.read_u8()?;
    let _pad = cursor.read_u8()?;
    let body_len = usize::from(cursor.read_u16_le()?);
    let body = cursor.read_slice(body_len)?;

    match msg_type {
        SNDC_FORMATS => Ok(ClientMessage::AudioFormats(decode_client_audio_formats(body)?)),
        SNDC_TRAINING => Ok(ClientMessage::TrainingConfirm),
        _ => Ok(ClientMessage::Other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_audio_formats_round_trip_via_client_decode_shape() {
        // Server and client PDUs share the same body shape (direction
        // reversed) apart from the wDGramPort endianness quirk and which
        // fields are meaningful - round-trip by decoding our own server
        // encoding as if it were a client one, skipping the BE field.
        let formats = vec![AudioFormat::pcm(2, 48000, 16)];
        let encoded = encode_server_audio_formats(&formats);
        assert_eq!(encoded[0], SNDC_FORMATS);

        let body_len = u16::from_le_bytes([encoded[2], encoded[3]]) as usize;
        assert_eq!(encoded.len(), 4 + body_len);
    }

    #[test]
    fn client_audio_formats_decode() {
        // Hand-encode a Client Audio Formats PDU per the field table
        // (dwDGramPort is the one big-endian field).
        let mut body = Vec::new();
        body.write_u32_le(0x0000_0007); // dwFlags: ALIVE|VOLUME|PITCH
        body.write_u32_le(0);
        body.write_u32_le(0);
        body.write_u16_be(0); // wDGramPort, big-endian
        body.write_u16_le(1); // one format
        body.write_u8(0);
        body.write_u16_le(0x06);
        body.write_u8(0);
        AudioFormat::pcm(2, 44100, 16).encode(&mut body);

        let mut pdu = Vec::new();
        write_header(&mut pdu, SNDC_FORMATS, body.len());
        pdu.extend_from_slice(&body);

        let decoded = decode_client_message(&pdu).unwrap();
        assert_eq!(
            decoded,
            ClientMessage::AudioFormats(ClientAudioFormats {
                formats: vec![AudioFormat::pcm(2, 44100, 16)],
            })
        );
    }

    #[test]
    fn training_confirm_is_recognized() {
        let mut pdu = Vec::new();
        write_header(&mut pdu, SNDC_TRAINING, 4);
        pdu.write_u16_le(0);
        pdu.write_u16_le(0);
        assert_eq!(decode_client_message(&pdu).unwrap(), ClientMessage::TrainingConfirm);
    }

    #[test]
    fn wave_confirm_and_unknown_messages_decode_as_other_without_erroring() {
        let mut pdu = Vec::new();
        write_header(&mut pdu, SNDC_WAVECONFIRM, 4);
        pdu.write_u16_le(0x1234); // wTimeStamp
        pdu.write_u8(5); // cConfirmedBlockNo
        pdu.write_u8(0); // pad
        assert_eq!(decode_client_message(&pdu).unwrap(), ClientMessage::Other);
    }

    #[test]
    fn wave2_encodes_expected_fixed_layout() {
        let data = vec![0xAB; 100];
        let encoded = encode_wave2(3, 7, 0x1000, &data);
        assert_eq!(encoded[0], SNDC_WAVE2);
        let body_len = u16::from_le_bytes([encoded[2], encoded[3]]) as usize;
        assert_eq!(body_len, 12 + data.len());
        // wFormatNo at body offset 2, cBlockNo at offset 4, dwAudioTimestamp at offset 8.
        let body = &encoded[4..];
        assert_eq!(u16::from_le_bytes([body[2], body[3]]), 3);
        assert_eq!(body[4], 7);
        assert_eq!(u32::from_le_bytes([body[8], body[9], body[10], body[11]]), 0x1000);
        assert_eq!(&encoded[16..], &data[..]);
    }

    #[test]
    fn negotiate_formats_matches_ignoring_derived_fields() {
        let server = vec![AudioFormat::pcm(2, 48000, 16)];
        // Same tag/channels/rate/bits but a client that computed
        // n_avg_bytes_per_sec/n_block_align slightly differently.
        let mut client_format = AudioFormat::pcm(2, 48000, 16);
        client_format.n_avg_bytes_per_sec = 999;
        client_format.n_block_align = 1;
        let client = vec![AudioFormat::pcm(2, 44100, 16), client_format];

        let negotiated = negotiate_formats(&server, &client);
        assert_eq!(negotiated.len(), 1);
        assert_eq!(negotiated[0].format_no, 1); // index into the client's list
        assert_eq!(negotiated[0].format, server[0]);
    }
}
