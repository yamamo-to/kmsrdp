//! The "Basic Security Header" that precedes any PDU sent as MCS Send Data
//! Request/Indication payload once standard RDP security framing is in
//! play (Client Info PDU, Licensing) - shared by `client_info.rs` and
//! `licensing.rs`. Since this server only ever negotiates `PROTOCOL_SSL`
//! (external/TLS security, never RDP Standard Security), `ENCRYPT` is never
//! set on anything this crate builds.

use crate::DecodeError;
use crate::cursor::{ReadCursor, WriteBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BasicSecurityHeaderFlags(pub u16);

impl BasicSecurityHeaderFlags {
    pub const ENCRYPT: Self = Self(0x0008);
    pub const INFO_PKT: Self = Self(0x0040);
    pub const LICENSE_PKT: Self = Self(0x0080);

    pub fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }
}

impl core::ops::BitOr for BasicSecurityHeaderFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BasicSecurityHeader {
    pub flags: BasicSecurityHeaderFlags,
}

impl BasicSecurityHeader {
    pub const SIZE: usize = 4;

    pub fn write(&self, out: &mut Vec<u8>) {
        out.write_u16_le(self.flags.0);
        out.write_u16_le(0); // flagsHi, unused
    }

    pub fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        let flags = BasicSecurityHeaderFlags(cursor.read_u16_le()?);
        let _flags_hi = cursor.read_u16_le()?;
        Ok(Self { flags })
    }
}
