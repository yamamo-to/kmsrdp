//! MS-RDPEECO (Echo Virtual Channel Extension): the simplest possible real
//! dynamic channel - a server sends arbitrary bytes, a compliant client
//! echoes the exact same bytes back, no envelope of its own beyond
//! whatever `DvcMux` already applies. Used only to smoke-test that the
//! DVC transport itself works end-to-end against a real client, before
//! anything real (audio input, RDPDR) is built on top of it.

use crate::DvcHandler;

const CHANNEL_NAME: &str = "ECHO";

/// Sends `payload` once the channel opens, and reports whether the
/// client's response matched byte-for-byte via `on_result`.
pub struct EchoHandler {
    payload: Vec<u8>,
    on_result: Box<dyn FnMut(bool) + Send>,
}

impl EchoHandler {
    pub fn new(payload: Vec<u8>, on_result: impl FnMut(bool) + Send + 'static) -> Self {
        Self {
            payload,
            on_result: Box::new(on_result),
        }
    }
}

impl core::fmt::Debug for EchoHandler {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EchoHandler")
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

impl DvcHandler for EchoHandler {
    fn channel_name(&self) -> &str {
        CHANNEL_NAME
    }

    fn on_open(&mut self) -> Vec<Vec<u8>> {
        vec![self.payload.clone()]
    }

    fn on_data(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        (self.on_result)(data == self.payload.as_slice());
        Vec::new()
    }
}
