#![no_main]

use libfuzzer_sys::fuzz_target;
use rdpcore_pdu::cursor::ReadCursor;
use rdpcore_pdu::mcs::{
    AttachUserConfirm, AttachUserRequest, ChannelJoinConfirm, ChannelJoinRequest,
    ConnectInitial, ConnectResponse, ErectDomainRequest, SendData,
};
use rdpcore_pdu::{finalization, licensing, rdp6, svc, utf16, x224};
use rdpcore_pdu::{fastpath, tpkt};

fuzz_target!(|data: &[u8]| {
    let mut cursor = ReadCursor::new(data);
    let _ = tpkt::TpktHeader::decode(&mut cursor);
    let _ = x224::unwrap_data(data);
    let _ = fastpath::FastPathInput::decode(data);
    let _ = fastpath::FastPathOutput::decode(data);
    let _ = ConnectInitial::decode(data);
    let _ = ConnectResponse::decode(data);
    let _ = ErectDomainRequest::decode(data);
    let _ = AttachUserRequest::decode(data);
    let _ = AttachUserConfirm::decode(data);
    let _ = ChannelJoinRequest::decode(data);
    let _ = ChannelJoinConfirm::decode(data);
    let _ = SendData::decode_request(data);
    let _ = SendData::decode_indication(data);
    let _ = svc::dechunkify(data);
    let _ = utf16::decode_units(data);
    let _ = finalization::decode_suppress_output(data);
    let _ = finalization::decode_refresh_rect(data);
    let _ = finalization::decode_monitor_layout(data);
    let _ = licensing::decode_valid_client(data);
    let _ = rdp6::decode(data, 64, 64);
});
