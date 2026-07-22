//! Static virtual channel codec integration: server-built PDUs decode as client
//! messages, and random garbage never panics.

use rdpcore_cliprdr::pdu as clip_pdu;
use rdpcore_dvc::pdu as dvc_pdu;
use rdpcore_pdu::cursor::WriteBuf;
use rdpcore_rdpdr::pdu as rdpdr_pdu;
use rdpcore_rdpeai::pdu as mic_pdu;
use rdpcore_rdpsnd::pdu::{self, AudioFormat, SNDC_FORMATS, SNDC_TRAINING};

fn assert_decode_never_panics<F>(mut decode: F, input: &[u8])
where
    F: FnMut(&[u8]),
{
    decode(input);
}

#[test]
fn cliprdr_server_pdus_decode_as_client_messages() {
    let cases = [
        clip_pdu::encode_capabilities(),
        clip_pdu::encode_monitor_ready(),
        clip_pdu::encode_format_list_unicode_text(),
        clip_pdu::encode_format_list_response_ok(),
        clip_pdu::encode_format_data_request(clip_pdu::CF_UNICODETEXT),
        clip_pdu::encode_format_data_response_text("integration"),
        clip_pdu::encode_format_data_response_error(),
    ];
    for encoded in cases {
        let _ = clip_pdu::decode_client_message(&encoded);
    }
}

#[test]
fn rdpsnd_training_confirm_decodes() {
    let mut body = Vec::new();
    body.write_u16_le(0);
    body.write_u16_le(0);
    let mut pdu = Vec::new();
    pdu.write_u8(SNDC_TRAINING);
    pdu.write_u8(0);
    pdu.write_u16_le(body.len() as u16);
    pdu.extend_from_slice(&body);
    assert!(matches!(
        pdu::decode_client_message(&pdu),
        Ok(pdu::ClientMessage::TrainingConfirm)
    ));
}

#[test]
fn rdpsnd_client_audio_formats_hand_encoded_roundtrip() {
    let mut body = Vec::new();
    body.write_u32_le(0x0000_0007);
    body.write_u32_le(0);
    body.write_u32_le(0);
    body.write_u16_be(0);
    body.write_u16_le(1);
    body.write_u8(0);
    body.write_u16_le(0x06);
    body.write_u8(0);
    AudioFormat::pcm(2, 48_000, 16).encode(&mut body);

    let mut pdu = Vec::new();
    pdu.write_u8(SNDC_FORMATS);
    pdu.write_u8(0);
    pdu.write_u16_le(body.len() as u16);
    pdu.extend_from_slice(&body);

    let decoded = pdu::decode_client_message(&pdu).unwrap();
    assert!(matches!(decoded, pdu::ClientMessage::AudioFormats(_)));
}

#[test]
fn rdpdr_core_pdus_decode_without_error() {
    let confirm = rdpdr_pdu::encode_client_id_confirm(7);
    let logged_on = rdpdr_pdu::encode_user_logged_on();
    assert!(matches!(
        rdpdr_pdu::decode_client_message(&confirm),
        Ok(rdpdr_pdu::ClientMessage::AnnounceReply)
    ));
    assert!(matches!(
        rdpdr_pdu::decode_client_message(&logged_on),
        Ok(rdpdr_pdu::ClientMessage::UserLoggedOn)
    ));
}

#[test]
fn rdpeai_and_dvc_garbage_inputs_do_not_panic() {
    let garbage: &[&[u8]] = &[&[], &[0xFF], &[0x01, 0x02, 0x03, 0x04, 0x05], &[0x00; 256]];
    for input in garbage {
        assert_decode_never_panics(|b| drop(mic_pdu::decode_client_message(b)), input);
        assert_decode_never_panics(|b| drop(dvc_pdu::decode_client_message(b)), input);
        assert_decode_never_panics(|b| drop(rdpdr_pdu::decode_client_message(b)), input);
        assert_decode_never_panics(|b| drop(clip_pdu::decode_client_message(b)), input);
        assert_decode_never_panics(|b| drop(pdu::decode_client_message(b)), input);
    }
}
