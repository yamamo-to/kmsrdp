//! Client Info PDU (MS-RDPBCGR 2.2.1.11): username/password/domain, sent by
//! the client as an MCS Send Data Request payload immediately after Channel
//! Join completes, wrapped in a [`BasicSecurityHeader`].
//!
//! Unicode-only: real clients (xfreerdp, mstsc) always set `INFO_UNICODE`;
//! the legacy ANSI encoding path is a phase-1 simplification left out.

use crate::cursor::{ReadCursor, WriteBuf};
use crate::headers::{BasicSecurityHeader, BasicSecurityHeaderFlags};
use crate::utf16::{decode_units, encode_units, read_fixed as read_utf16_fixed};
use crate::DecodeError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClientInfoFlags(pub u32);

impl ClientInfoFlags {
    pub const MOUSE: Self = Self(0x0001);
    pub const AUTOLOGON: Self = Self(0x0008);
    pub const UNICODE: Self = Self(0x0010);
    pub const ENABLE_WINDOWS_KEY: Self = Self(0x0100);

    pub fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }
}

impl core::ops::BitOr for ClientInfoFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// `clientAddressFamily`/`clientAddress`/`clientDir` - the only
/// `ExtendedClientInfo` fields this server parses; everything after them
/// (timezone, session id, performance flags, auto-reconnect cookie) is
/// optional, self-truncating, and safe to leave unparsed for a minimal
/// server (see phase-1 research notes).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExtendedClientInfo {
    pub address_family: u16,
    pub address: String,
    pub dir: String,
}

impl ExtendedClientInfo {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.write_u16_le(self.address_family);
        write_len_included_string(&mut out, &self.address);
        write_len_included_string(&mut out, &self.dir);
        out
    }

    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        let address_family = cursor.read_u16_le()?;
        let address = read_len_included_string(cursor)?;
        let dir = read_len_included_string(cursor)?;
        Ok(Self {
            address_family,
            address,
            dir,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientInfo {
    pub code_page: u32,
    pub flags: ClientInfoFlags,
    pub domain: String,
    pub username: String,
    pub password: String,
    pub alternate_shell: String,
    pub working_dir: String,
    pub extended: ExtendedClientInfo,
}

impl ClientInfo {
    /// All five `cbXxx` length fields are packed together as one block
    /// *before* any of the five strings - not interleaved
    /// (length,string,length,string,...) as it might look from the field
    /// list alone. Getting this wrong doesn't break a self-consistent
    /// encode/decode round-trip test (both sides agree with each other),
    /// only real-client bytes exposed it.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.write_u32_le(self.code_page);
        out.write_u32_le((self.flags | ClientInfoFlags::UNICODE).0);

        let domain_bytes = encode_units(&self.domain);
        let username_bytes = encode_units(&self.username);
        let password_bytes = encode_units(&self.password);
        let shell_bytes = encode_units(&self.alternate_shell);
        let dir_bytes = encode_units(&self.working_dir);

        out.write_u16_le(domain_bytes.len() as u16);
        out.write_u16_le(username_bytes.len() as u16);
        out.write_u16_le(password_bytes.len() as u16);
        out.write_u16_le(shell_bytes.len() as u16);
        out.write_u16_le(dir_bytes.len() as u16);

        for bytes in [domain_bytes, username_bytes, password_bytes, shell_bytes, dir_bytes] {
            out.write_slice(&bytes);
            out.write_u16_le(0); // NUL terminator
        }

        out.write_slice(&self.extended.encode());
        out
    }

    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        let code_page = cursor.read_u32_le()?;
        let flags = ClientInfoFlags(cursor.read_u32_le()?);
        if !flags.contains(ClientInfoFlags::UNICODE) {
            return Err(DecodeError::InvalidValue {
                field: "client_info.flags",
                reason: "only INFO_UNICODE clients are supported",
            });
        }

        let cb_domain = usize::from(cursor.read_u16_le()?);
        let cb_username = usize::from(cursor.read_u16_le()?);
        let cb_password = usize::from(cursor.read_u16_le()?);
        let cb_alternate_shell = usize::from(cursor.read_u16_le()?);
        let cb_working_dir = usize::from(cursor.read_u16_le()?);

        let domain = read_string_body(cursor, cb_domain)?;
        let username = read_string_body(cursor, cb_username)?;
        let password = read_string_body(cursor, cb_password)?;
        let alternate_shell = read_string_body(cursor, cb_alternate_shell)?;
        let working_dir = read_string_body(cursor, cb_working_dir)?;

        let extended = ExtendedClientInfo::decode(cursor)?;
        Ok(Self {
            code_page,
            flags,
            domain,
            username,
            password,
            alternate_shell,
            working_dir,
            extended,
        })
    }
}

/// Reads a Client Info string field given its already-read `cbXxx` length
/// (which excludes the terminator, per the field's own convention) plus
/// the 2-byte NUL terminator that's still on the wire after it.
fn read_string_body(cursor: &mut ReadCursor<'_>, len: usize) -> Result<String, DecodeError> {
    let s = decode_units(cursor.read_slice(len)?);
    cursor.advance(2); // NUL terminator, not part of `len`
    Ok(s)
}

/// Length field includes the terminator (`ExtendedClientInfo`'s
/// clientAddress/clientDir convention).
fn write_len_included_string(out: &mut Vec<u8>, s: &str) {
    let bytes = encode_units(s);
    out.write_u16_le((bytes.len() + 2) as u16);
    out.write_slice(&bytes);
    out.write_u16_le(0);
}

fn read_len_included_string(cursor: &mut ReadCursor<'_>) -> Result<String, DecodeError> {
    let len = usize::from(cursor.read_u16_le()?);
    Ok(read_utf16_fixed(cursor.read_slice(len)?))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfoPdu {
    pub info: ClientInfo,
}

impl ClientInfoPdu {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        BasicSecurityHeader {
            flags: BasicSecurityHeaderFlags::INFO_PKT,
        }
        .write(&mut out);
        out.write_slice(&self.info.encode());
        out
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        let header = BasicSecurityHeader::decode(&mut cursor)?;
        if !header.flags.contains(BasicSecurityHeaderFlags::INFO_PKT) {
            return Err(DecodeError::InvalidValue {
                field: "client_info_pdu.security_header.flags",
                reason: "expected INFO_PKT set",
            });
        }
        let info = ClientInfo::decode(&mut cursor)?;
        Ok(Self { info })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real bytes captured from `xfreerdp /u:testuser /p:testpass123`
    /// connecting through the full connection sequence (Xvfb + a raw TCP
    /// listener standing in for the server, same technique as x224's real-
    /// capture test). This is the fixture that caught a real bug: the five
    /// `cbXxx` length fields are packed together as one block *before* any
    /// of the five strings, not interleaved (length,string,length,...) as
    /// an initial implementation assumed - a self-consistent encode/decode
    /// round-trip test alone could never have caught that, since both
    /// sides agreed with each other under the same wrong assumption.
    #[test]
    fn decodes_real_xfreerdp_client_info() {
        #[rustfmt::skip]
        let captured: &[u8] = &[
            0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xfb, 0x47, 0x0b, 0x00, 0x00, 0x00, 0x10, 0x00,
            0x16, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x74, 0x00, 0x65, 0x00, 0x73, 0x00, 0x74, 0x00,
            0x75, 0x00, 0x73, 0x00, 0x65, 0x00, 0x72, 0x00, 0x00, 0x00, 0x74, 0x00, 0x65, 0x00, 0x73, 0x00,
            0x74, 0x00, 0x70, 0x00, 0x61, 0x00, 0x73, 0x00, 0x73, 0x00, 0x31, 0x00, 0x32, 0x00, 0x33, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x14, 0x00, 0x31, 0x00, 0x32, 0x00, 0x37, 0x00,
            0x2e, 0x00, 0x30, 0x00, 0x2e, 0x00, 0x30, 0x00, 0x2e, 0x00, 0x31, 0x00, 0x00, 0x00, 0x40, 0x00,
            0x43, 0x00, 0x3a, 0x00, 0x5c, 0x00, 0x57, 0x00, 0x69, 0x00, 0x6e, 0x00, 0x64, 0x00, 0x6f, 0x00,
            0x77, 0x00, 0x73, 0x00, 0x5c, 0x00, 0x53, 0x00, 0x79, 0x00, 0x73, 0x00, 0x74, 0x00, 0x65, 0x00,
            0x6d, 0x00, 0x33, 0x00, 0x32, 0x00, 0x5c, 0x00, 0x6d, 0x00, 0x73, 0x00, 0x74, 0x00, 0x73, 0x00,
            0x63, 0x00, 0x61, 0x00, 0x78, 0x00, 0x2e, 0x00, 0x64, 0x00, 0x6c, 0x00, 0x6c, 0x00, 0x00, 0x00,
            0xe4, 0xfd, 0xff, 0xff, // TimezoneInfo.bias (-540) - not parsed, left as trailing bytes
        ];

        let decoded = ClientInfoPdu::decode(captured).unwrap();
        assert_eq!(decoded.info.domain, "");
        assert_eq!(decoded.info.username, "testuser");
        assert_eq!(decoded.info.password, "testpass123");
        assert_eq!(decoded.info.alternate_shell, "");
        assert_eq!(decoded.info.working_dir, "");
        assert_eq!(decoded.info.extended.address_family, 0x0002);
        assert_eq!(decoded.info.extended.address, "127.0.0.1");
        assert_eq!(decoded.info.extended.dir, "C:\\Windows\\System32\\mstscax.dll");
    }

    #[test]
    fn client_info_pdu_round_trip() {
        let pdu = ClientInfoPdu {
            info: ClientInfo {
                code_page: 0,
                flags: ClientInfoFlags::MOUSE | ClientInfoFlags::AUTOLOGON | ClientInfoFlags::UNICODE,
                domain: String::new(),
                username: "kmsrdp".to_owned(),
                password: "hunter2".to_owned(),
                alternate_shell: String::new(),
                working_dir: String::new(),
                extended: ExtendedClientInfo {
                    address_family: 0x0002,
                    address: "192.0.2.1".to_owned(),
                    dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
                },
            },
        };
        let encoded = pdu.encode();
        let decoded = ClientInfoPdu::decode(&encoded).unwrap();
        assert_eq!(decoded, pdu);
        assert!(decoded.info.flags.contains(ClientInfoFlags::UNICODE));
    }

    #[test]
    fn rejects_non_unicode_client_info() {
        let mut pdu = ClientInfoPdu {
            info: ClientInfo {
                username: "kmsrdp".to_owned(),
                ..Default::default()
            },
        };
        pdu.info.flags = ClientInfoFlags::default(); // no UNICODE
        let mut encoded = Vec::new();
        BasicSecurityHeader {
            flags: BasicSecurityHeaderFlags::INFO_PKT,
        }
        .write(&mut encoded);
        encoded.write_u32_le(0); // code page
        encoded.write_u32_le(0); // flags, no UNICODE bit
        assert!(matches!(
            ClientInfoPdu::decode(&encoded),
            Err(DecodeError::InvalidValue {
                field: "client_info.flags",
                ..
            })
        ));
    }
}
