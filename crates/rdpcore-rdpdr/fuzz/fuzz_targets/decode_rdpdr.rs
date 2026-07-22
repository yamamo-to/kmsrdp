#![no_main]

use libfuzzer_sys::fuzz_target;
use rdpcore_rdpdr::{irp, pdu};

fuzz_target!(|data: &[u8]| {
    let _ = pdu::decode_client_message(data);
    let _ = irp::decode_create_reply(data);
    let _ = irp::decode_read_reply(data);
    let _ = irp::decode_write_reply(data);
    let _ = irp::decode_query_directory_reply(data);
});
