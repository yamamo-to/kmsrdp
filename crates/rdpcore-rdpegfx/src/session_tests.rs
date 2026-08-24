use super::*;
use crate::encoder::H264Encoder;
use crate::pdu::{CAP_VERSION_81, CAPS_FLAG_AVC420_ENABLED};
use crate::{EncodedAu, EncoderError};
use rdpcore_pdu::cursor::WriteBuf;

fn expect_frames(result: GfxFrameResult) -> Vec<Vec<u8>> {
    match result {
        GfxFrameResult::Frames(frames) => frames,
        other => panic!("expected Frames, got {other:?}"),
    }
}

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
    let frames = expect_frames(session.encode_frame(64, 64, 64 * 4, &pixels));
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
    let first = expect_frames(session.encode_frame(64, 64, 64 * 4, &pixels));
    assert_eq!(first.len(), 3); // Start+Wire+End

    // Second CapsAdvertise: Delete + CapsConfirm + Reset + Create + Map
    let replies = handler.on_data(&advertise);
    assert_eq!(replies.len(), 5);

    let second = expect_frames(session.encode_frame(64, 64, 64 * 4, &pixels));
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
    let _ = expect_frames(session.encode_frame(64, 64, 64 * 4, &pixels));
    let pixels2 = vec![0u8; 80 * 48 * 4];
    let frames = expect_frames(session.encode_frame(80, 48, 80 * 4, &pixels2));
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
    let frames = expect_frames(session.encode_frame(32, 32, 32 * 4, &pixels));
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
    assert_eq!(
        session.encode_frame(64, 64, 64 * 4, &pixels),
        GfxFrameResult::Skip
    );

    let mut handler = session.dvc_handler();
    let advertise = encode_caps_advertise_for_test(&[RawCapabilitySet::flags_only(
        CAP_VERSION_81,
        CAPS_FLAG_AVC420_ENABLED,
    )]);
    let _ = handler.on_data(&advertise);
    assert_eq!(session.encode_frame(0, 64, 0, &[]), GfxFrameResult::Skip);
    assert_eq!(
        session.encode_frame(64, 0, 64 * 4, &pixels),
        GfxFrameResult::Skip
    );
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
        let _ = expect_frames(session.encode_frame(16, 16, 16 * 4, &pixels));
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
    assert!(matches!(
        session.encode_frame(16, 16, 16 * 4, &pixels),
        GfxFrameResult::Frames(_)
    ));
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
    ) -> Result<EncodedAu, EncoderError> {
        self.calls += 1;
        if self.calls == 1 {
            return Err(EncoderError::EncodeFailed("transient".into()));
        }
        Ok(EncodedAu {
            annex_b: vec![0, 0, 0, 1, if force_idr { 0x65 } else { 0x41 }],
            qp: 22,
        })
    }

    fn reset(&mut self) {}
}

struct BlockingEncoder {
    started: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

impl H264Encoder for BlockingEncoder {
    fn encode_bgrx(
        &mut self,
        _width: u16,
        _height: u16,
        _stride: usize,
        _pixels: &[u8],
        _force_idr: bool,
    ) -> Result<EncodedAu, EncoderError> {
        let _ = self.started.send(());
        let _ = self.release.recv();
        Ok(EncodedAu {
            annex_b: vec![0, 0, 0, 1, 0x65],
            qp: 22,
        })
    }

    fn reset(&mut self) {}
}

#[test]
fn encode_frame_does_not_hold_inner_lock_during_the_encode() {
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let session = GfxSession::new(
        Box::new(BlockingEncoder {
            started: started_tx,
            release: release_rx,
        }),
        16,
        16,
    );
    let mut handler = session.dvc_handler();
    let advertise = encode_caps_advertise_for_test(&[RawCapabilitySet::flags_only(
        CAP_VERSION_81,
        CAPS_FLAG_AVC420_ENABLED,
    )]);
    let _ = handler.on_data(&advertise);

    let pixels = vec![0u8; 16 * 16 * 4];
    let encode_session = session.clone();
    let encode_thread = std::thread::spawn(move || {
        expect_frames(encode_session.encode_frame(16, 16, 16 * 4, &pixels))
    });

    started_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("encode_bgrx should have started");

    // If encode_frame still held `inner`'s lock while the CPU-bound
    // encode above is blocked, this would deadlock (is_ready also
    // locks `inner`) instead of returning immediately.
    assert!(
        session.is_ready(),
        "inner's lock must not be held during the encode"
    );

    release_tx.send(()).unwrap();
    let frames = encode_thread.join().unwrap();
    assert_eq!(frames.len(), 3);
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
    let frames = expect_frames(session.encode_frame(16, 16, 16 * 4, &pixels));
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
    ) -> Result<EncodedAu, EncoderError> {
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
    assert_eq!(
        session.encode_frame(16, 16, 16 * 4, &pixels),
        GfxFrameResult::Skip
    );
    assert!(!session.failed());
}

#[test]
fn encode_fallback_teardown_is_delete_surface() {
    let session = GfxSession::new(Box::new(EmptyEncoder), 16, 16);
    let mut handler = session.dvc_handler();
    let advertise = encode_caps_advertise_for_test(&[RawCapabilitySet::flags_only(
        CAP_VERSION_81,
        CAPS_FLAG_AVC420_ENABLED,
    )]);
    let _ = handler.on_data(&advertise);
    let pixels = vec![0u8; 16 * 16 * 4];
    let mut result = GfxFrameResult::Skip;
    for _ in 0..super::ENCODE_FAIL_FALLBACK {
        result = session.encode_frame(16, 16, 16 * 4, &pixels);
    }
    let GfxFrameResult::Fallback { teardown } = result else {
        panic!("expected Fallback, got {result:?}");
    };
    assert_eq!(teardown.len(), 1);
    assert_eq!(segmented_cmd_id(&teardown[0]), 0x000a); // DeleteSurface
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

#[test]
fn recover_lock_yields_the_value_after_poison() {
    let mutex = Mutex::new(7u32);
    let _ = std::panic::catch_unwind(|| {
        let _guard = mutex.lock().unwrap();
        panic!("poison");
    });
    assert!(mutex.lock().is_err());
    *recover_lock(&mutex) += 1;
    assert_eq!(*recover_lock(&mutex), 8);
}
