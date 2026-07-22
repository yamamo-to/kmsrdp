//! Cross-layer wire framing: SVC chunks inside MCS Send Data inside X.224 Data.

use rdpcore_pdu::fastpath::{
    FastPathInput, FastPathInputEvent, FastPathOutput, FastPathUpdatePdu, Fragmentation,
    UPDATE_CODE_BITMAP,
};
use rdpcore_pdu::mcs::SendData;
use rdpcore_pdu::svc::{self, CHANNEL_FLAG_FIRST, DEFAULT_CHUNK_LENGTH};
use rdpcore_pdu::x224;

#[test]
fn x224_wrap_unwrap_preserves_payload() {
    let payload = b"mcs-layer user data";
    let frame = x224::wrap_data(payload);
    assert_eq!(x224::unwrap_data(&frame).unwrap(), payload);
}

#[test]
fn svc_multi_chunk_reassembles_through_mcs_indication_frames() {
    let payload = vec![0xCDu8; DEFAULT_CHUNK_LENGTH * 2 + 77];
    let wire_frames = svc::wrap_indication(1002, 1004, payload.clone());

    let mut reassembled = Vec::new();
    for frame in wire_frames {
        let mcs_body = x224::unwrap_data(&frame).unwrap();
        let indication = SendData::decode_indication(mcs_body).unwrap();
        let (_total, flags, chunk) = svc::dechunkify(&indication.data).unwrap();
        if flags & CHANNEL_FLAG_FIRST != 0 {
            reassembled.clear();
        }
        reassembled.extend_from_slice(chunk);
    }
    assert_eq!(reassembled, payload);
}

#[test]
fn fastpath_input_survives_x224_envelope() {
    let input = FastPathInput {
        events: vec![
            FastPathInputEvent::Scancode {
                flags: 0,
                code: 0x1E,
            },
            FastPathInputEvent::Unicode {
                flags: 0,
                code: 0x3042,
            },
        ],
    };
    let fp_bytes = input.encode();
    let frame = x224::wrap_data(&fp_bytes);
    let decoded = FastPathInput::decode(x224::unwrap_data(&frame).unwrap()).unwrap();
    assert_eq!(decoded, input);
}

#[test]
fn fastpath_output_bitmap_update_roundtrips() {
    let output = FastPathOutput {
        updates: vec![FastPathUpdatePdu {
            update_code: UPDATE_CODE_BITMAP,
            fragmentation: Fragmentation::Single,
            data: vec![0x01, 0x00, 0x00, 0x00, 0x00, 0x00], // minimal bitmap header stub
        }],
    };
    let encoded = output.encode();
    let decoded = FastPathOutput::decode(&encoded).unwrap();
    assert_eq!(decoded.updates.len(), 1);
    assert_eq!(decoded.updates[0].update_code, UPDATE_CODE_BITMAP);
}
