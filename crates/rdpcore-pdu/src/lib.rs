//! From-scratch RDP wire-format codecs: no I/O, no async, no other
//! `rdpcore-*` crate or third-party protocol library involved. This is the
//! layer meant to be exhaustively unit-tested and fuzzed (`cargo fuzz run
//! decode_wire` in `fuzz/`) against arbitrary wire bytes.
//!
//! Covers TPKT/X.224, MCS, GCC, capabilities, FastPath, static-channel
//! SVC framing, and related codecs used by the rest of the stack.

pub mod ber;
pub mod capability_sets;
pub mod client_info;
pub mod cursor;
pub mod fastpath;
pub mod finalization;
pub mod gcc;
pub mod headers;
pub mod licensing;
pub mod mcs;
pub mod nscodec;
pub mod per;
pub mod pointer;
pub mod rdp6;
pub mod surface_commands;
pub mod svc;
pub mod tpdu;
pub mod tpkt;
pub mod utf16;
pub mod x224;

use cursor::NotEnoughBytes;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    NotEnoughBytes(NotEnoughBytes),
    InvalidValue {
        field: &'static str,
        reason: &'static str,
    },
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotEnoughBytes(e) => write!(f, "not enough bytes: {e}"),
            Self::InvalidValue { field, reason } => {
                write!(f, "invalid value for {field}: {reason}")
            }
        }
    }
}

impl core::error::Error for DecodeError {}

impl From<NotEnoughBytes> for DecodeError {
    fn from(e: NotEnoughBytes) -> Self {
        Self::NotEnoughBytes(e)
    }
}
