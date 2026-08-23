#![no_main]

use libfuzzer_sys::fuzz_target;
use rdpcore_cliprdr::pdu;

fuzz_target!(|data: &[u8]| {
    let _ = pdu::decode_client_message(data);
});
