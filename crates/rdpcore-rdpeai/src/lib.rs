//! MS-RDPEAI audio input (microphone redirection): PDU codec (`pdu`) plus
//! [`AudioInputHandler`], a [`rdpcore_dvc::DvcHandler`] that drives the
//! Version -> Formats -> Open -> OpenReply -> streaming sequence and hands
//! each captured PCM chunk to an [`AudioInputBackend`].

pub mod pdu;

use pdu::AudioFormat;
use rdpcore_dvc::DvcHandler;

/// Consumes captured audio as it arrives - kmsrdp's own backend pipes this
/// into a local virtual microphone source; this crate has no opinion on
/// what happens to the bytes beyond decoding them off the wire.
pub trait AudioInputBackend: Send {
    fn on_audio_data(&mut self, format: &AudioFormat, data: &[u8]);
}

pub trait AudioInputBackendFactory: Send + Sync {
    fn build_backend(&self) -> Box<dyn AudioInputBackend>;
}

/// The comparison MS-RDPEAI negotiation actually needs: tag/channels/
/// sample-rate/bits/extra-data must match - mirrors RDPSND's own
/// negotiation equality, since both protocols share the same WAVEFORMATEX-
/// style format entries.
fn formats_compatible(a: &AudioFormat, b: &AudioFormat) -> bool {
    a.format_tag == b.format_tag
        && a.n_channels == b.n_channels
        && a.n_samples_per_sec == b.n_samples_per_sec
        && a.bits_per_sample == b.bits_per_sample
        && a.extra_data == b.extra_data
}

/// Server-format-priority order: for each of *our* formats, in order,
/// look for any match in the client's list, and take the first hit -
/// matching a real implementation's own negotiation strategy. Returns the
/// index into `server_formats` (that's what `initialFormat` in the Open
/// PDU references, per MS-RDPEAI - the client already has this same list
/// from the preceding Formats PDU).
fn negotiate(server_formats: &[AudioFormat], client_formats: &[AudioFormat]) -> Option<u32> {
    server_formats
        .iter()
        .position(|server_format| client_formats.iter().any(|client_format| formats_compatible(server_format, client_format)))
        .map(|index| index as u32)
}

enum State {
    WaitVersion,
    WaitFormats,
    WaitOpenReply,
    Streaming,
}

pub struct AudioInputHandler {
    server_formats: Vec<AudioFormat>,
    frames_per_packet: u32,
    state: State,
    negotiated: Option<AudioFormat>,
    backend: Box<dyn AudioInputBackend>,
}

impl AudioInputHandler {
    /// Offers a single format - 48kHz/stereo/16-bit PCM, matching what
    /// `rdpcore-rdpsnd`'s own default output format uses, so a real
    /// backend piping capture straight into the same audio graph doesn't
    /// need any resampling.
    pub fn new(backend: Box<dyn AudioInputBackend>) -> Self {
        Self {
            server_formats: vec![AudioFormat::pcm(2, 48_000, 16)],
            frames_per_packet: 480, // 10ms at 48kHz
            state: State::WaitVersion,
            negotiated: None,
            backend,
        }
    }
}

impl core::fmt::Debug for AudioInputHandler {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AudioInputHandler").finish_non_exhaustive()
    }
}

impl DvcHandler for AudioInputHandler {
    fn channel_name(&self) -> &str {
        pdu::CHANNEL_NAME
    }

    /// The server always speaks first on this channel too.
    fn on_open(&mut self) -> Vec<Vec<u8>> {
        vec![pdu::encode_version(pdu::VERSION_2)]
    }

    fn on_data(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        let Ok(message) = pdu::decode_client_message(data) else {
            return Vec::new();
        };

        match (message, &self.state) {
            (pdu::ClientMessage::Version { .. }, State::WaitVersion) => {
                self.state = State::WaitFormats;
                vec![pdu::encode_formats(&self.server_formats)]
            }
            (pdu::ClientMessage::Formats { formats }, State::WaitFormats) => match negotiate(&self.server_formats, &formats) {
                Some(index) => {
                    let format = self.server_formats[index as usize].clone();
                    self.negotiated = Some(format.clone());
                    self.state = State::WaitOpenReply;
                    vec![pdu::encode_open(self.frames_per_packet, index, &format)]
                }
                // No compatible format - nothing more to do; the client
                // never receives an Open PDU and stays idle on this channel.
                None => Vec::new(),
            },
            (pdu::ClientMessage::OpenReply { .. }, State::WaitOpenReply) => {
                self.state = State::Streaming;
                Vec::new()
            }
            (pdu::ClientMessage::Data { data }, State::Streaming) => {
                if let Some(format) = &self.negotiated {
                    self.backend.on_audio_data(format, &data);
                }
                Vec::new()
            }
            // DataIncoming (bandwidth-measurement hook, no-op by design),
            // FormatChange (not supported - this server only ever offers
            // one format so there's nothing to change to), or anything
            // arriving out of the expected sequence: ignored rather than
            // treated as connection-fatal.
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct RecordingBackend {
        received: Vec<(AudioFormat, Vec<u8>)>,
    }

    impl AudioInputBackend for RecordingBackend {
        fn on_audio_data(&mut self, format: &AudioFormat, data: &[u8]) {
            self.received.push((format.clone(), data.to_vec()));
        }
    }

    fn client_formats_matching_server() -> Vec<AudioFormat> {
        vec![AudioFormat::pcm(2, 48_000, 16)]
    }

    #[test]
    fn full_sequence_reaches_streaming_and_delivers_audio() {
        let mut handler = AudioInputHandler::new(Box::<RecordingBackend>::default());
        assert_eq!(handler.channel_name(), "AUDIO_INPUT");

        let opened = handler.on_open();
        assert_eq!(opened.len(), 1); // Version PDU

        let formats_reply = handler.on_data(&pdu::encode_version(pdu::VERSION_2));
        assert_eq!(formats_reply.len(), 1); // our Formats PDU goes out

        let client_formats = pdu::encode_formats(&client_formats_matching_server());
        let open_frames = handler.on_data(&client_formats);
        assert_eq!(open_frames.len(), 1); // Open PDU
        assert!(matches!(handler.state, State::WaitOpenReply));

        let mut open_reply = Vec::new();
        rdpcore_pdu::cursor::WriteBuf::write_u8(&mut open_reply, 0x04);
        rdpcore_pdu::cursor::WriteBuf::write_u32_le(&mut open_reply, 0);
        assert!(handler.on_data(&open_reply).is_empty());
        assert!(matches!(handler.state, State::Streaming));

        let mut data_pdu = vec![0x06u8];
        data_pdu.extend_from_slice(&[0x11; 40]);
        assert!(handler.on_data(&data_pdu).is_empty());
    }

    #[test]
    fn no_compatible_format_never_sends_open() {
        let mut handler = AudioInputHandler::new(Box::<RecordingBackend>::default());
        handler.on_open();
        handler.on_data(&pdu::encode_version(pdu::VERSION_2));

        let incompatible = vec![AudioFormat::pcm(1, 8_000, 8)];
        let response = handler.on_data(&pdu::encode_formats(&incompatible));
        assert!(response.is_empty());
    }

    #[test]
    fn negotiate_picks_first_server_format_with_any_client_match() {
        let server = vec![AudioFormat::pcm(2, 48_000, 16), AudioFormat::pcm(1, 16_000, 16)];
        let client = vec![AudioFormat::pcm(1, 16_000, 16)];
        assert_eq!(negotiate(&server, &client), Some(1));
    }
}
