//! GFX session state machine + DVC handler.
//!
//! After CapsConfirm the server immediately sends ResetGraphics / CreateSurface
//! / MapSurfaceToOutput using the known capture size (recreated on resize).
//! H.264 on the wire is Annex B per MS-RDPEGFX `RFX_AVC420_BITMAP_STREAM`.

use std::sync::{Arc, Mutex};

use rdpcore_dvc::DvcHandler;
use tracing::{debug, info, warn};

use crate::encoder::{H264Encoder, MockH264Encoder};
use crate::pdu::{self, ClientMessage, MonitorDef, RawCapabilitySet, select_avc420_capability};

const DEFAULT_SURFACE_ID: u16 = 1;
/// Soft cap — mstsc often delays FrameAcknowledge.
const MAX_FRAMES_IN_FLIGHT: u32 = 32;
const QUEUE_DEPTH_UNAVAILABLE: u32 = 0xffff_ffff;
/// Force an IDR at least this often so a lost/corrupt frame cannot leave
/// the client stuck on a black surface forever.
const IDR_INTERVAL_FRAMES: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    WaitCaps,
    Ready,
    Failed,
}

struct Inner {
    state: State,
    /// False until ResetGraphics/CreateSurface/Map have been sent for `width`×`height`.
    surface_configured: bool,
    surface_id: u16,
    width: u16,
    height: u16,
    next_frame_id: u32,
    frames_in_flight: u32,
    frames_sent: u64,
    encoder: Box<dyn H264Encoder>,
    force_next_idr: bool,
    timestamp_ms: u32,
}

impl Inner {
    fn new(encoder: Box<dyn H264Encoder>, width: u16, height: u16) -> Self {
        Self {
            state: State::WaitCaps,
            surface_configured: false,
            surface_id: DEFAULT_SURFACE_ID,
            width,
            height,
            next_frame_id: 1,
            frames_in_flight: 0,
            frames_sent: 0,
            encoder,
            force_next_idr: true,
            timestamp_ms: 0,
        }
    }

    fn is_ready(&self) -> bool {
        self.state == State::Ready
    }

    fn on_caps(&mut self, sets: &[RawCapabilitySet]) -> Vec<Vec<u8>> {
        let Some(selected) = select_avc420_capability(sets) else {
            self.state = State::Failed;
            warn!("GFX CapsAdvertise has no AVC420; falling back to Planar/NSCodec");
            return Vec::new();
        };

        let mut out = Vec::new();

        // Mid-session CapsAdvertise is a real re-negotiate (mstsc does this).
        // Tear down the live surface cleanly — creating the same surfaceId
        // again without DeleteSurface, or ignoring Caps while still streaming
        // WireToSurface, both trigger client protocol errors.
        if self.state == State::Ready && self.surface_configured {
            let old = self.surface_id;
            out.push(pdu::encode_segmented_single(&pdu::encode_delete_surface(
                old,
            )));
            self.surface_id = self.surface_id.wrapping_add(1).max(1);
            info!(
                old_surface = old,
                new_surface = self.surface_id,
                "GFX Caps re-negotiate: deleted surface"
            );
        }

        self.state = State::Ready;
        self.force_next_idr = true;
        self.frames_in_flight = 0;
        self.frames_sent = 0;
        self.encoder.reset();
        info!(
            version = format_args!("0x{:08x}", selected.version),
            width = self.width,
            height = self.height,
            "GFX CapsConfirm: AVC420 negotiated"
        );
        out.push(pdu::encode_segmented_single(&pdu::encode_caps_confirm(
            &selected,
        )));
        // Typical sequence: CapsConfirm then Reset/Create/Map before any frames.
        // Use the capture size known at session construction (updated on resize).
        if self.width > 0 && self.height > 0 {
            for pdu in self.setup_pdus() {
                out.push(pdu::encode_segmented_single(&pdu));
            }
            self.surface_configured = true;
            info!(
                width = self.width,
                height = self.height,
                surface_id = self.surface_id,
                "GFX surface configured after CapsConfirm"
            );
        } else {
            self.surface_configured = false;
        }
        out
    }

    fn setup_pdus(&self) -> Vec<Vec<u8>> {
        let monitors = [MonitorDef {
            left: 0,
            top: 0,
            right: i32::from(self.width).saturating_sub(1),
            bottom: i32::from(self.height).saturating_sub(1),
            primary: true,
        }];
        vec![
            pdu::encode_reset_graphics(u32::from(self.width), u32::from(self.height), &monitors),
            pdu::encode_create_surface(self.surface_id, self.width, self.height),
            pdu::encode_map_surface_to_output(self.surface_id, 0, 0),
        ]
    }

    fn on_frame_ack(&mut self, queue_depth: u32, frame_id: u32) {
        if queue_depth == QUEUE_DEPTH_UNAVAILABLE {
            self.frames_in_flight = 0;
        } else {
            self.frames_in_flight = queue_depth.min(MAX_FRAMES_IN_FLIGHT);
        }
        debug!(
            frame_id,
            queue_depth,
            in_flight = self.frames_in_flight,
            "GFX FrameAcknowledge"
        );
    }

    fn resize(&mut self, width: u16, height: u16) -> Option<Vec<Vec<u8>>> {
        if width == 0 || height == 0 {
            return None;
        }
        if self.surface_configured && self.width == width && self.height == height {
            return None;
        }
        self.encoder.reset();
        let old_surface = self.surface_id;
        let had_surface = self.surface_configured;
        self.width = width;
        self.height = height;
        if had_surface {
            self.surface_id = self.surface_id.wrapping_add(1).max(1);
        }
        self.force_next_idr = true;
        self.frames_in_flight = 0;
        self.surface_configured = false;
        if self.state != State::Ready {
            return None;
        }
        let mut out = Vec::new();
        if had_surface {
            out.push(pdu::encode_segmented_single(&pdu::encode_delete_surface(
                old_surface,
            )));
        }
        for pdu in self.setup_pdus() {
            out.push(pdu::encode_segmented_single(&pdu));
        }
        self.surface_configured = true;
        info!(
            width,
            height,
            surface_id = self.surface_id,
            "GFX surface configured"
        );
        Some(out)
    }

    fn encode_frame(
        &mut self,
        width: u16,
        height: u16,
        stride: usize,
        pixels: &[u8],
    ) -> Option<Vec<Vec<u8>>> {
        if self.state != State::Ready {
            return None;
        }
        if width == 0 || height == 0 {
            return None;
        }

        let mut prefix = Vec::new();
        if !self.surface_configured || self.width != width || self.height != height {
            match self.resize(width, height) {
                Some(pdus) => prefix = pdus,
                None if self.state != State::Ready => return None,
                None => {}
            }
        }
        if !self.surface_configured {
            return None;
        }

        if self.frames_in_flight >= MAX_FRAMES_IN_FLIGHT {
            self.frames_in_flight = MAX_FRAMES_IN_FLIGHT / 2;
            self.force_next_idr = true;
        }

        let force_idr = self.force_next_idr
            || self.frames_sent == 0
            || self.frames_sent.is_multiple_of(IDR_INTERVAL_FRAMES);
        let encoded = match self
            .encoder
            .encode_bgrx(width, height, stride, pixels, force_idr)
        {
            Ok(au) if !au.annex_b.is_empty() => au,
            Ok(_) | Err(_) => {
                // Soft skip / transient RC failure: force an IDR and retry once
                // instead of falling through to Planar (which leaves the GFX
                // surface black while FrameAcks keep arriving).
                self.force_next_idr = true;
                match self
                    .encoder
                    .encode_bgrx(width, height, stride, pixels, true)
                {
                    Ok(au) if !au.annex_b.is_empty() => au,
                    Ok(_) => {
                        debug!("GFX H.264 encode skipped (empty bitstream)");
                        return None;
                    }
                    Err(e) => {
                        warn!(error = %e, "GFX H.264 encode failed");
                        return None;
                    }
                }
            }
        };
        self.force_next_idr = false;

        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1).max(1);
        self.timestamp_ms = self.timestamp_ms.wrapping_add(33);
        self.frames_in_flight = self.frames_in_flight.saturating_add(1);
        self.frames_sent = self.frames_sent.saturating_add(1);

        // MS-RDPEGFX RFX_AVC420_BITMAP_STREAM requires Annex B on the wire.
        let bitmap =
            pdu::encode_avc420_bitmap_stream(width, height, encoded.qp, 100, &encoded.annex_b);

        prefix.extend([
            pdu::encode_segmented_single(&pdu::encode_start_frame(self.timestamp_ms, frame_id)),
            pdu::encode_segmented_single(&pdu::encode_wire_to_surface_1_avc420(
                self.surface_id,
                width,
                height,
                &bitmap,
            )),
            pdu::encode_segmented_single(&pdu::encode_end_frame(frame_id)),
        ]);
        if self.frames_sent == 1 || force_idr || self.frames_sent.is_multiple_of(300) {
            debug!(
                frames_sent = self.frames_sent,
                frame_id,
                annex_b_len = encoded.annex_b.len(),
                force_idr,
                "GFX frame sent"
            );
        }
        Some(prefix)
    }
}

/// Shared GFX session used both as a [`DvcHandler`] (inbound Caps/Ack) and
/// from the connection loop (outbound frames).
#[derive(Clone)]
pub struct GfxSession {
    inner: Arc<Mutex<Inner>>,
}

impl core::fmt::Debug for GfxSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GfxSession").finish_non_exhaustive()
    }
}

impl GfxSession {
    pub fn new(encoder: Box<dyn H264Encoder>, width: u16, height: u16) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::new(encoder, width, height))),
        }
    }

    pub fn mock(width: u16, height: u16) -> Self {
        Self::new(Box::new(MockH264Encoder::default()), width, height)
    }

    pub fn is_ready(&self) -> bool {
        self.inner.lock().unwrap().is_ready()
    }

    pub fn failed(&self) -> bool {
        self.inner.lock().unwrap().state == State::Failed
    }

    pub fn encode_frame(
        &self,
        width: u16,
        height: u16,
        stride: usize,
        pixels: &[u8],
    ) -> Option<Vec<Vec<u8>>> {
        self.inner
            .lock()
            .unwrap()
            .encode_frame(width, height, stride, pixels)
    }

    pub fn resize(&self, width: u16, height: u16) -> Option<Vec<Vec<u8>>> {
        self.inner.lock().unwrap().resize(width, height)
    }

    pub fn dvc_handler(&self) -> GfxDvcHandler {
        GfxDvcHandler {
            inner: Arc::clone(&self.inner),
        }
    }
}

pub struct GfxDvcHandler {
    inner: Arc<Mutex<Inner>>,
}

impl core::fmt::Debug for GfxDvcHandler {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GfxDvcHandler").finish_non_exhaustive()
    }
}

impl DvcHandler for GfxDvcHandler {
    fn channel_name(&self) -> &str {
        pdu::CHANNEL_NAME
    }

    fn on_open(&mut self) -> Vec<Vec<u8>> {
        Vec::new()
    }

    fn on_data(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        let mut rest = data;
        let mut out = Vec::new();
        while !rest.is_empty() {
            let Ok(msg) = pdu::decode_client_message(rest) else {
                break;
            };
            if rest.len() < 8 {
                break;
            }
            let pdu_len = u32::from_le_bytes(rest[4..8].try_into().unwrap_or([0; 4])) as usize;
            if pdu_len < 8 || pdu_len > rest.len() {
                break;
            }
            rest = &rest[pdu_len..];

            let mut inner = self.inner.lock().unwrap();
            match msg {
                ClientMessage::CapsAdvertise { sets } => out.extend(inner.on_caps(&sets)),
                ClientMessage::FrameAcknowledge {
                    queue_depth,
                    frame_id,
                    ..
                } => {
                    inner.on_frame_ack(queue_depth, frame_id);
                }
                ClientMessage::CacheImportOffer | ClientMessage::Other { .. } => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EncodedAu;
    use crate::encoder::H264Encoder;
    use crate::pdu::{CAP_VERSION_81, CAPS_FLAG_AVC420_ENABLED};
    use rdpcore_pdu::cursor::WriteBuf;

    fn encode_caps_advertise_for_test(sets: &[RawCapabilitySet]) -> Vec<u8> {
        let mut body = Vec::new();
        body.write_u16_le(sets.len() as u16);
        for s in sets {
            body.write_u32_le(s.version);
            body.write_u32_le(s.data.len() as u32);
            body.write_slice(&s.data);
        }
        let mut out = Vec::new();
        out.write_u16_le(0x0012);
        out.write_u16_le(0);
        out.write_u32_le((8 + body.len()) as u32);
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn caps_configures_surface_immediately() {
        let session = GfxSession::mock(64, 64);
        let mut handler = session.dvc_handler();
        let advertise = encode_caps_advertise_for_test(&[RawCapabilitySet::flags_only(
            CAP_VERSION_81,
            CAPS_FLAG_AVC420_ENABLED,
        )]);
        let replies = handler.on_data(&advertise);
        // CapsConfirm + Reset + Create + Map
        assert_eq!(replies.len(), 4);
        assert!(session.is_ready());

        let pixels = vec![0u8; 64 * 64 * 4];
        let frames = session.encode_frame(64, 64, 64 * 4, &pixels).unwrap();
        // Start + Wire + End (surface already configured)
        assert_eq!(frames.len(), 3);
        assert!(frames.iter().all(|r| r[0] == 0xe0));
    }

    #[test]
    fn no_avc_capability_marks_failed() {
        let session = GfxSession::mock(32, 32);
        let mut handler = session.dvc_handler();
        let advertise =
            encode_caps_advertise_for_test(&[RawCapabilitySet::flags_only(CAP_VERSION_81, 0)]);
        assert!(handler.on_data(&advertise).is_empty());
        assert!(session.failed());
        assert!(!session.is_ready());
    }

    #[test]
    fn duplicate_caps_deletes_then_recreates_surface() {
        let session = GfxSession::mock(64, 64);
        let mut handler = session.dvc_handler();
        let advertise = encode_caps_advertise_for_test(&[RawCapabilitySet::flags_only(
            CAP_VERSION_81,
            CAPS_FLAG_AVC420_ENABLED,
        )]);
        assert_eq!(handler.on_data(&advertise).len(), 4);
        let pixels = vec![0u8; 64 * 64 * 4];
        let first = session.encode_frame(64, 64, 64 * 4, &pixels).unwrap();
        assert_eq!(first.len(), 3); // Start+Wire+End

        // Second CapsAdvertise: Delete + CapsConfirm + Reset + Create + Map
        let replies = handler.on_data(&advertise);
        assert_eq!(replies.len(), 5);

        let second = session.encode_frame(64, 64, 64 * 4, &pixels).unwrap();
        assert_eq!(second.len(), 3);
    }

    #[test]
    fn encode_auto_resizes_surface() {
        let session = GfxSession::mock(64, 64);
        let mut handler = session.dvc_handler();
        let advertise = encode_caps_advertise_for_test(&[RawCapabilitySet::flags_only(
            CAP_VERSION_81,
            CAPS_FLAG_AVC420_ENABLED,
        )]);
        let _ = handler.on_data(&advertise);
        let pixels = vec![0u8; 64 * 64 * 4];
        let _ = session.encode_frame(64, 64, 64 * 4, &pixels).unwrap();
        let pixels2 = vec![0u8; 80 * 48 * 4];
        let frames = session.encode_frame(80, 48, 80 * 4, &pixels2).unwrap();
        // Delete + Reset + Create + Map + Start + Wire + End
        assert_eq!(frames.len(), 7);
    }

    fn segmented_cmd_id(segmented: &[u8]) -> u16 {
        assert!(segmented.len() >= 4);
        assert_eq!(segmented[0], 0xe0);
        assert_eq!(segmented[1], 0x04);
        u16::from_le_bytes([segmented[2], segmented[3]])
    }

    fn wire_bitmap_payload(segmented_wire: &[u8]) -> &[u8] {
        // SEGMENTED + GFX header(8) + surfaceId(2)+codec(2)+pix(1)+rect(8)+bitmapLen(4)
        let gfx = &segmented_wire[2..];
        let bitmap_len = u32::from_le_bytes(gfx[21..25].try_into().unwrap()) as usize;
        &gfx[25..25 + bitmap_len]
    }

    #[test]
    fn encode_frame_sends_annex_b_not_avcc() {
        let session = GfxSession::mock(32, 32);
        let mut handler = session.dvc_handler();
        let advertise = encode_caps_advertise_for_test(&[RawCapabilitySet::flags_only(
            CAP_VERSION_81,
            CAPS_FLAG_AVC420_ENABLED,
        )]);
        let _ = handler.on_data(&advertise);
        let pixels = vec![0u8; 32 * 32 * 4];
        let frames = session.encode_frame(32, 32, 32 * 4, &pixels).unwrap();
        assert_eq!(segmented_cmd_id(&frames[0]), 0x000b); // StartFrame
        assert_eq!(segmented_cmd_id(&frames[1]), 0x0001); // WireToSurface1
        assert_eq!(segmented_cmd_id(&frames[2]), 0x000c); // EndFrame

        let bitmap = wire_bitmap_payload(&frames[1]);
        // RFX_AVC420: after metablock (14 bytes) comes Annex B start code
        assert_eq!(&bitmap[14..18], &[0, 0, 0, 1]);
        // Mock IDR NAL type
        assert_eq!(bitmap[18], 0x65);
    }

    #[test]
    fn encode_before_caps_or_zero_size_returns_none() {
        let session = GfxSession::mock(64, 64);
        let pixels = vec![0u8; 64 * 64 * 4];
        assert!(session.encode_frame(64, 64, 64 * 4, &pixels).is_none());

        let mut handler = session.dvc_handler();
        let advertise = encode_caps_advertise_for_test(&[RawCapabilitySet::flags_only(
            CAP_VERSION_81,
            CAPS_FLAG_AVC420_ENABLED,
        )]);
        let _ = handler.on_data(&advertise);
        assert!(session.encode_frame(0, 64, 0, &[]).is_none());
        assert!(session.encode_frame(64, 0, 64 * 4, &pixels).is_none());
    }

    #[test]
    fn frame_ack_unavailable_clears_in_flight_pressure() {
        let session = GfxSession::mock(16, 16);
        let mut handler = session.dvc_handler();
        let advertise = encode_caps_advertise_for_test(&[RawCapabilitySet::flags_only(
            CAP_VERSION_81,
            CAPS_FLAG_AVC420_ENABLED,
        )]);
        let _ = handler.on_data(&advertise);
        let pixels = vec![0u8; 16 * 16 * 4];
        for _ in 0..5 {
            let _ = session.encode_frame(16, 16, 16 * 4, &pixels).unwrap();
        }
        // QUEUE_DEPTH_UNAVAILABLE
        let mut ack = Vec::new();
        ack.write_u16_le(0x000d);
        ack.write_u16_le(0);
        ack.write_u32_le(20);
        ack.write_u32_le(0xffff_ffff);
        ack.write_u32_le(1);
        ack.write_u32_le(5);
        assert!(handler.on_data(&ack).is_empty());
        // Still able to encode after ack (session not stuck)
        assert!(session.encode_frame(16, 16, 16 * 4, &pixels).is_some());
    }

    #[test]
    fn batched_caps_and_ack_in_one_buffer() {
        let session = GfxSession::mock(16, 16);
        let mut handler = session.dvc_handler();
        let mut advertise = encode_caps_advertise_for_test(&[RawCapabilitySet::flags_only(
            CAP_VERSION_81,
            CAPS_FLAG_AVC420_ENABLED,
        )]);
        let mut ack = Vec::new();
        ack.write_u16_le(0x000d);
        ack.write_u16_le(0);
        ack.write_u32_le(20);
        ack.write_u32_le(0);
        ack.write_u32_le(1);
        ack.write_u32_le(0);
        advertise.extend_from_slice(&ack);
        let replies = handler.on_data(&advertise);
        assert_eq!(replies.len(), 4);
        assert!(session.is_ready());
    }

    #[test]
    fn cache_import_offer_is_ignored() {
        let session = GfxSession::mock(16, 16);
        let mut handler = session.dvc_handler();
        let mut offer = Vec::new();
        offer.write_u16_le(0x0010);
        offer.write_u16_le(0);
        offer.write_u32_le(8);
        assert!(handler.on_data(&offer).is_empty());
        assert!(!session.is_ready());
        assert!(!session.failed());
    }

    #[test]
    fn caps_confirm_echoes_selected_version() {
        let session = GfxSession::mock(16, 16);
        let mut handler = session.dvc_handler();
        let advertise = encode_caps_advertise_for_test(&[
            RawCapabilitySet::flags_only(CAP_VERSION_81, CAPS_FLAG_AVC420_ENABLED),
            RawCapabilitySet::flags_only(crate::pdu::CAP_VERSION_10, 0),
        ]);
        let replies = handler.on_data(&advertise);
        let confirm = &replies[0];
        assert_eq!(segmented_cmd_id(confirm), 0x0013);
        // CapsConfirm body: version at offset 2(seg)+8(header)=10
        let ver = u32::from_le_bytes(confirm[10..14].try_into().unwrap());
        assert_eq!(ver, crate::pdu::CAP_VERSION_10);
    }

    /// Encoder that fails once then returns Annex B (IDR retry path).
    #[derive(Default)]
    struct FailOnceEncoder {
        calls: u32,
    }

    impl H264Encoder for FailOnceEncoder {
        fn encode_bgrx(
            &mut self,
            _width: u16,
            _height: u16,
            _stride: usize,
            _pixels: &[u8],
            force_idr: bool,
        ) -> Result<EncodedAu, String> {
            self.calls += 1;
            if self.calls == 1 {
                return Err("transient".into());
            }
            Ok(EncodedAu {
                annex_b: vec![0, 0, 0, 1, if force_idr { 0x65 } else { 0x41 }],
                qp: 22,
            })
        }

        fn reset(&mut self) {}
    }

    #[test]
    fn encode_retries_idr_after_transient_failure() {
        let session = GfxSession::new(Box::new(FailOnceEncoder::default()), 16, 16);
        let mut handler = session.dvc_handler();
        let advertise = encode_caps_advertise_for_test(&[RawCapabilitySet::flags_only(
            CAP_VERSION_81,
            CAPS_FLAG_AVC420_ENABLED,
        )]);
        let _ = handler.on_data(&advertise);
        let pixels = vec![0u8; 16 * 16 * 4];
        let frames = session.encode_frame(16, 16, 16 * 4, &pixels).unwrap();
        assert_eq!(frames.len(), 3);
        let bitmap = wire_bitmap_payload(&frames[1]);
        assert_eq!(bitmap[18], 0x65); // forced IDR on retry
    }

    #[derive(Default)]
    struct EmptyEncoder;

    impl H264Encoder for EmptyEncoder {
        fn encode_bgrx(
            &mut self,
            _width: u16,
            _height: u16,
            _stride: usize,
            _pixels: &[u8],
            _force_idr: bool,
        ) -> Result<EncodedAu, String> {
            Ok(EncodedAu {
                annex_b: Vec::new(),
                qp: 22,
            })
        }

        fn reset(&mut self) {}
    }

    #[test]
    fn encode_skips_when_bitstream_empty() {
        let session = GfxSession::new(Box::new(EmptyEncoder), 16, 16);
        let mut handler = session.dvc_handler();
        let advertise = encode_caps_advertise_for_test(&[RawCapabilitySet::flags_only(
            CAP_VERSION_81,
            CAPS_FLAG_AVC420_ENABLED,
        )]);
        let _ = handler.on_data(&advertise);
        let pixels = vec![0u8; 16 * 16 * 4];
        assert!(session.encode_frame(16, 16, 16 * 4, &pixels).is_none());
    }

    #[test]
    fn caps_setup_pdu_order() {
        let session = GfxSession::mock(64, 48);
        let mut handler = session.dvc_handler();
        let advertise = encode_caps_advertise_for_test(&[RawCapabilitySet::flags_only(
            CAP_VERSION_81,
            CAPS_FLAG_AVC420_ENABLED,
        )]);
        let replies = handler.on_data(&advertise);
        assert_eq!(segmented_cmd_id(&replies[0]), 0x0013); // CapsConfirm
        assert_eq!(segmented_cmd_id(&replies[1]), 0x000e); // ResetGraphics
        assert_eq!(segmented_cmd_id(&replies[2]), 0x0009); // CreateSurface
        assert_eq!(segmented_cmd_id(&replies[3]), 0x000f); // MapSurfaceToOutput
        // CreateSurface carries session size
        let create = &replies[2][2..];
        assert_eq!(&create[10..12], &64u16.to_le_bytes());
        assert_eq!(&create[12..14], &48u16.to_le_bytes());
    }
}
