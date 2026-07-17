//! MS-RDPEAI wire format (audio input / microphone redirection over the
//! `"AUDIO_INPUT"` dynamic virtual channel). Every message is just a
//! 1-byte `MessageId` followed by its body, running to the end of the
//! DVC-layer payload - unlike RDPSND/CLIPRDR, there is no length/flags
//! preamble at this protocol layer at all (DVC framing, see
//! `rdpcore_dvc`, already provides it).
//!
//! Reference: FreeRDP's own server-side implementation
//! (`channels/audin/server/audin.c`, `include/freerdp/channels/audin.h`)
//!
//! No local IronRDP equivalent exists for this protocol, so this is the
//! one crate in this workspace verified against a different real
//! implementation than the others.

use rdpcore_pdu::DecodeError;
use rdpcore_pdu::cursor::{ReadCursor, WriteBuf};
pub use rdpcore_rdpsnd::pdu::AudioFormat;

const MSG_SNDIN_VERSION: u8 = 0x01;
const MSG_SNDIN_FORMATS: u8 = 0x02;
const MSG_SNDIN_OPEN: u8 = 0x03;
const MSG_SNDIN_OPEN_REPLY: u8 = 0x04;
const MSG_SNDIN_DATA_INCOMING: u8 = 0x05;
const MSG_SNDIN_DATA: u8 = 0x06;
const MSG_SNDIN_FORMATCHANGE: u8 = 0x07;

pub const CHANNEL_NAME: &str = "AUDIO_INPUT";

/// `SNDIN_VERSION_Version_2` - the version this server always advertises;
/// FreeRDP's own client falls back to it for any value it doesn't
/// recognize, so there's nothing to gain from advertising `1` instead.
pub const VERSION_2: u32 = 2;

pub fn encode_version(version: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    out.write_u8(MSG_SNDIN_VERSION);
    out.write_u32_le(version);
    out
}

pub fn encode_formats(formats: &[AudioFormat]) -> Vec<u8> {
    let mut formats_bytes = Vec::new();
    for format in formats {
        format.encode(&mut formats_bytes);
    }

    let mut out = Vec::with_capacity(9 + formats_bytes.len());
    out.write_u8(MSG_SNDIN_FORMATS);
    out.write_u32_le(formats.len() as u32);
    out.write_u32_le(formats_bytes.len() as u32); // cbSizeFormatsPacket
    out.write_slice(&formats_bytes);
    out
}

/// `captureFormat` is always encoded without a WAVEFORMATEXTENSIBLE tail
/// (`cbSize = 0`) - correct as long as it's PCM (`WAVE_FORMAT_PCM`, never
/// `WAVE_FORMAT_EXTENSIBLE`), which is all this server ever offers.
pub fn encode_open(
    frames_per_packet: u32,
    initial_format_index: u32,
    capture_format: &AudioFormat,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(9 + 18);
    out.write_u8(MSG_SNDIN_OPEN);
    out.write_u32_le(frames_per_packet);
    out.write_u32_le(initial_format_index);
    capture_format.encode(&mut out);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    Version { version: u32 },
    Formats { formats: Vec<AudioFormat> },
    OpenReply { result: u32 },
    DataIncoming,
    Data { data: Vec<u8> },
    FormatChange { new_format: u32 },
    Other,
}

pub fn decode_client_message(input: &[u8]) -> Result<ClientMessage, DecodeError> {
    let mut cursor = ReadCursor::new(input);
    let message_id = cursor.read_u8()?;

    match message_id {
        MSG_SNDIN_VERSION => Ok(ClientMessage::Version {
            version: cursor.read_u32_le()?,
        }),
        MSG_SNDIN_FORMATS => {
            let num_formats = cursor.read_u32_le()?;
            let _cb_size_formats_packet = cursor.read_u32_le()?;
            let formats = (0..num_formats)
                .map(|_| AudioFormat::decode(&mut cursor))
                .collect::<Result<_, _>>()?;
            Ok(ClientMessage::Formats { formats })
        }
        MSG_SNDIN_OPEN_REPLY => Ok(ClientMessage::OpenReply {
            result: cursor.read_u32_le()?,
        }),
        MSG_SNDIN_DATA_INCOMING => Ok(ClientMessage::DataIncoming),
        MSG_SNDIN_DATA => Ok(ClientMessage::Data {
            data: cursor.read_rest().to_vec(),
        }),
        MSG_SNDIN_FORMATCHANGE => Ok(ClientMessage::FormatChange {
            new_format: cursor.read_u32_le()?,
        }),
        _ => Ok(ClientMessage::Other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_round_trip() {
        let encoded = encode_version(VERSION_2);
        assert_eq!(encoded.len(), 5);
        assert_eq!(
            decode_client_message(&encoded).unwrap(),
            ClientMessage::Version { version: VERSION_2 }
        );
    }

    #[test]
    fn formats_round_trip() {
        let formats = vec![
            AudioFormat::pcm(2, 44100, 16),
            AudioFormat::pcm(1, 16000, 16),
        ];
        let encoded = encode_formats(&formats);
        assert_eq!(
            decode_client_message(&encoded).unwrap(),
            ClientMessage::Formats { formats }
        );
    }

    #[test]
    fn open_encodes_frames_per_packet_and_format_index() {
        let format = AudioFormat::pcm(2, 44100, 16);
        let encoded = encode_open(441, 0, &format);
        // header(1) + framesPerPacket(4) + initialFormat(4) + 18-byte PCM core, no extension tail
        assert_eq!(encoded.len(), 1 + 4 + 4 + 18);
        assert_eq!(u32::from_le_bytes(encoded[1..5].try_into().unwrap()), 441);
        assert_eq!(u32::from_le_bytes(encoded[5..9].try_into().unwrap()), 0);
    }

    #[test]
    fn open_reply_decode() {
        let mut raw = Vec::new();
        raw.write_u8(MSG_SNDIN_OPEN_REPLY);
        raw.write_u32_le(0);
        assert_eq!(
            decode_client_message(&raw).unwrap(),
            ClientMessage::OpenReply { result: 0 }
        );
    }

    #[test]
    fn data_incoming_has_no_body() {
        let raw = vec![MSG_SNDIN_DATA_INCOMING];
        assert_eq!(
            decode_client_message(&raw).unwrap(),
            ClientMessage::DataIncoming
        );
    }

    #[test]
    fn data_is_raw_bytes_to_end_of_message() {
        let mut raw = vec![MSG_SNDIN_DATA];
        raw.extend_from_slice(&[0xAB; 100]);
        assert_eq!(
            decode_client_message(&raw).unwrap(),
            ClientMessage::Data {
                data: vec![0xAB; 100]
            }
        );
    }

    #[test]
    fn format_change_new_format_is_four_bytes() {
        let mut raw = Vec::new();
        raw.write_u8(MSG_SNDIN_FORMATCHANGE);
        raw.write_u32_le(3);
        assert_eq!(
            decode_client_message(&raw).unwrap(),
            ClientMessage::FormatChange { new_format: 3 }
        );
    }

    #[test]
    fn unknown_message_id_decodes_as_other() {
        let raw = vec![0xFF];
        assert_eq!(decode_client_message(&raw).unwrap(), ClientMessage::Other);
    }
}
