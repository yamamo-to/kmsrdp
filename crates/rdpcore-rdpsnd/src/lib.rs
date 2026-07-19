//! MS-RDPEA audio-output virtual channel: PDU codec (`pdu`) plus the
//! connection-scoped state machine (`RdpsndChannel`) and backend trait
//! (`RdpsndServerHandler`) a server plugs an audio source into. Shaped
//! close to a real implementation's own trait so an existing backend
//! (e.g. kmsrdp's `parec`-based one) ports with import-path changes only.

pub mod pdu;

use rdpcore_pdu::DecodeError;
use rdpcore_pdu::svc::wrap_indication;

pub trait RdpsndError: std::error::Error + Send {}
impl<T: std::error::Error + Send> RdpsndError for T {}

impl<'a, E: RdpsndError + 'a> From<E> for Box<dyn RdpsndError + 'a> {
    fn from(e: E) -> Self {
        Box::new(e)
    }
}

/// A chunk of PCM (or whatever codec was negotiated) audio, plus a
/// timestamp - produced by a backend once `start` is called, consumed by
/// [`RdpsndChannel::encode_wave`].
pub enum RdpsndServerMessage {
    Wave(Vec<u8>, u32),
}

pub trait RdpsndServerHandler: Send + core::fmt::Debug {
    fn get_formats(&self) -> &[pdu::AudioFormat];
    fn choose_format(&mut self, common: &[pdu::NegotiatedFormat]) -> Option<pdu::NegotiatedFormat>;
    fn start(&mut self, format: &pdu::NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>>;
    fn stop(&mut self);
}

/// Builds a per-connection audio backend. The outgoing-message `sender` is
/// supplied by the server for that connection so concurrent sessions each
/// get an independent channel.
pub trait SoundServerFactory: Send + Sync {
    fn build_backend(
        &self,
        sender: tokio::sync::mpsc::UnboundedSender<RdpsndServerMessage>,
    ) -> Box<dyn RdpsndServerHandler>;
}

enum State {
    WaitFormats,
    WaitTrainingConfirm,
    Ready,
}

/// One connection's rdpsnd channel: negotiates formats, then encodes
/// outgoing PCM chunks as `Wave2` PDUs. Every method that produces
/// something to send returns fully wire-ready bytes (TPKT/X.224/MCS
/// SendDataIndication already applied) - the caller (`rdpcore-server`)
/// only needs to wrap them in a `rdpcore_transport::Frame` and hand them
/// to the scheduler, it doesn't need to know anything about RDPSND's PDU
/// shapes itself.
pub struct RdpsndChannel {
    channel_id: u16,
    user_channel_id: u16,
    handler: Box<dyn RdpsndServerHandler>,
    state: State,
    negotiated: Option<pdu::NegotiatedFormat>,
    block_no: u8,
}

impl RdpsndChannel {
    /// Returns the channel plus the initial Server Audio Formats frame(s)
    /// the caller should send immediately, in order.
    pub fn new(
        channel_id: u16,
        user_channel_id: u16,
        handler: Box<dyn RdpsndServerHandler>,
    ) -> (Self, Vec<Vec<u8>>) {
        let initial = wrap_indication(
            user_channel_id,
            channel_id,
            pdu::encode_server_audio_formats(handler.get_formats()),
        );
        (
            Self {
                channel_id,
                user_channel_id,
                handler,
                state: State::WaitFormats,
                negotiated: None,
                block_no: 0,
            },
            initial,
        )
    }

    pub fn channel_id(&self) -> u16 {
        self.channel_id
    }

    /// `payload` is the raw `"rdpsnd"`-channel bytes carried by an
    /// incoming MCS Send Data Request for this channel, Channel PDU
    /// Header (length+flags) included - every message this server needs
    /// to react to is small enough to arrive as a single `FIRST | LAST`
    /// chunk, so no reassembly across multiple Send Data Requests is
    /// needed here. Returns wire-ready response frames, if this message
    /// warrants any.
    pub fn on_channel_data(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>, DecodeError> {
        let (_total_length, _flags, body) = rdpcore_pdu::svc::dechunkify(payload)?;

        match (pdu::decode_client_message(body)?, &self.state) {
            (pdu::ClientMessage::AudioFormats(client_formats), State::WaitFormats) => {
                let negotiated =
                    pdu::negotiate_formats(self.handler.get_formats(), &client_formats.formats);
                self.negotiated = self.handler.choose_format(&negotiated);
                self.state = State::WaitTrainingConfirm;
                Ok(wrap_indication(
                    self.user_channel_id,
                    self.channel_id,
                    pdu::encode_training(),
                ))
            }
            (pdu::ClientMessage::TrainingConfirm, State::WaitTrainingConfirm) => {
                if let Some(negotiated) = self.negotiated.clone() {
                    let _ = self.handler.start(&negotiated);
                }
                self.state = State::Ready;
                Ok(Vec::new())
            }
            // WaveConfirm, or anything arriving out of the expected
            // sequence - a real implementation ignores WaveConfirm
            // entirely (no flow control gating on it) and this one does
            // the same; out-of-sequence messages are likewise dropped
            // rather than treated as connection-fatal.
            _ => Ok(Vec::new()),
        }
    }

    /// Encodes one PCM chunk as `Wave2` and wraps it for the wire, or an
    /// empty `Vec` if nothing has been negotiated/started yet (the caller
    /// should just drop the chunk - there's no destination format to
    /// encode it against).
    pub fn encode_wave(&mut self, pcm: Vec<u8>, timestamp_ms: u32) -> Vec<Vec<u8>> {
        if !matches!(self.state, State::Ready) {
            return Vec::new();
        }
        let Some(format_no) = self.negotiated.as_ref().map(|n| n.format_no) else {
            return Vec::new();
        };
        let body = pdu::encode_wave2(format_no, self.block_no, timestamp_ms, &pcm);
        self.block_no = self.block_no.wrapping_add(1);
        wrap_indication(self.user_channel_id, self.channel_id, body)
    }
}

impl Drop for RdpsndChannel {
    fn drop(&mut self) {
        self.handler.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct FakeHandler {
        formats: Vec<pdu::AudioFormat>,
        started_with: Option<pdu::NegotiatedFormat>,
        stopped: bool,
    }

    impl RdpsndServerHandler for FakeHandler {
        fn get_formats(&self) -> &[pdu::AudioFormat] {
            &self.formats
        }

        fn choose_format(
            &mut self,
            common: &[pdu::NegotiatedFormat],
        ) -> Option<pdu::NegotiatedFormat> {
            common.first().cloned()
        }

        fn start(&mut self, format: &pdu::NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
            self.started_with = Some(format.clone());
            Ok(())
        }

        fn stop(&mut self) {
            self.stopped = true;
        }
    }

    /// Incoming channel data always carries the Channel PDU Header
    /// (length+flags) too - real clients send it, and `on_channel_data`
    /// strips it via `svc::dechunkify` before touching RDPSND content.
    fn as_single_incoming_chunk(rdpsnd_pdu_bytes: &[u8]) -> Vec<u8> {
        let mut chunks = rdpcore_pdu::svc::chunkify(rdpsnd_pdu_bytes);
        assert_eq!(chunks.len(), 1, "test fixture too big for a single chunk");
        chunks.remove(0)
    }

    fn client_audio_formats_payload(formats: &[pdu::AudioFormat]) -> Vec<u8> {
        // Build a well-formed Client Audio Formats PDU the same way a real
        // client would - reuses the crate's own encoder (there isn't a
        // public client-side encoder, so hand-assemble via the same field
        // layout `pdu`'s tests already verified byte-for-byte).
        use rdpcore_pdu::cursor::WriteBuf;
        let mut body = Vec::new();
        body.write_u32_le(0);
        body.write_u32_le(0);
        body.write_u32_le(0);
        body.write_u16_be(0);
        body.write_u16_le(formats.len() as u16);
        body.write_u8(0);
        body.write_u16_le(6);
        body.write_u8(0);
        for f in formats {
            body.write_u16_le(f.format_tag);
            body.write_u16_le(f.n_channels);
            body.write_u32_le(f.n_samples_per_sec);
            body.write_u32_le(f.n_avg_bytes_per_sec);
            body.write_u16_le(f.n_block_align);
            body.write_u16_le(f.bits_per_sample);
            body.write_u16_le(0);
        }
        let mut out = Vec::new();
        out.write_u8(pdu::SNDC_FORMATS);
        out.write_u8(0);
        out.write_u16_le(body.len() as u16);
        out.extend(body);
        as_single_incoming_chunk(&out)
    }

    fn training_confirm_payload() -> Vec<u8> {
        use rdpcore_pdu::cursor::WriteBuf;
        let mut out = Vec::new();
        out.write_u8(pdu::SNDC_TRAINING);
        out.write_u8(0);
        out.write_u16_le(4);
        out.write_u16_le(0);
        out.write_u16_le(0);
        as_single_incoming_chunk(&out)
    }

    #[test]
    fn full_negotiation_reaches_ready_and_starts_handler() {
        let formats = vec![pdu::AudioFormat::pcm(2, 48000, 16)];
        let handler = Box::new(FakeHandler {
            formats: formats.clone(),
            ..Default::default()
        });
        let (mut channel, initial) = RdpsndChannel::new(1004, 1002, handler);
        assert!(!initial.is_empty()); // Server Audio Formats went out immediately

        // No wave frames before negotiation completes.
        assert!(channel.encode_wave(vec![0; 10], 0).is_empty());

        let training = channel
            .on_channel_data(&client_audio_formats_payload(&formats))
            .unwrap();
        assert!(!training.is_empty(), "expects a Training PDU in response");

        assert!(
            channel
                .on_channel_data(&training_confirm_payload())
                .unwrap()
                .is_empty()
        );

        // Now negotiated and started - a wave chunk should encode.
        let wave = channel.encode_wave(vec![0xAB; 100], 1234);
        assert!(!wave.is_empty());
    }

    #[test]
    fn wave_confirm_is_ignored_without_erroring() {
        use rdpcore_pdu::cursor::WriteBuf;
        let handler = Box::<FakeHandler>::default();
        let (mut channel, _initial) = RdpsndChannel::new(1004, 1002, handler);

        let mut wave_confirm = Vec::new();
        wave_confirm.write_u8(pdu::SNDC_WAVECONFIRM);
        wave_confirm.write_u8(0);
        wave_confirm.write_u16_le(4);
        wave_confirm.write_u16_le(0);
        wave_confirm.write_u8(0);
        wave_confirm.write_u8(0);

        assert!(
            channel
                .on_channel_data(&as_single_incoming_chunk(&wave_confirm))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_large_wave_chunk_splits_into_multiple_svc_chunks() {
        let formats = vec![pdu::AudioFormat::pcm(2, 48000, 16)];
        let handler = Box::new(FakeHandler {
            formats: formats.clone(),
            ..Default::default()
        });
        let (mut channel, _initial) = RdpsndChannel::new(1004, 1002, handler);
        channel
            .on_channel_data(&client_audio_formats_payload(&formats))
            .unwrap();
        channel
            .on_channel_data(&training_confirm_payload())
            .unwrap();

        // A 20ms/48kHz/stereo/16-bit chunk is 3840 bytes of PCM - plus the
        // 12-byte Wave2 header, well over the 1600-byte default SVC chunk
        // size, so this must come back as more than one wire frame.
        let wave_frames = channel.encode_wave(vec![0x11; 3840], 0);
        assert!(
            wave_frames.len() > 1,
            "expected multiple SVC chunks, got {}",
            wave_frames.len()
        );
    }
}
