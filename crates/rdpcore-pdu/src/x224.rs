//! X.224 Connection Request / Connection Confirm, carrying the RDP
//! Negotiation Request/Response/Failure structures used to pick a security
//! protocol (MS-RDPBCGR 2.2.1.1 / 2.2.1.2).
//!
//! Every other TPDU type (Data, Disconnect, ...) belongs to later phases;
//! this module only covers what's needed to complete the initial X.224
//! exchange.

use crate::cursor::{ReadCursor, WriteBuf};
use crate::tpdu::{TpduCode, TpduHeader};
use crate::tpkt::{self, TpktHeader};
use crate::DecodeError;

const COOKIE_PREFIX: &str = "Cookie: mstshash=";
const CRLF: u16 = 0x0A0D; // written little-endian -> bytes [0x0D, 0x0A] = "\r\n"

const NEGO_REQUEST: u8 = 0x01;
const NEGO_RESPONSE: u8 = 0x02;
const NEGO_FAILURE: u8 = 0x03;
const NEGO_BODY_SIZE: u16 = 8; // type(1) + flags(1) + length(2) + protocol/code(4)

/// PROTOCOL_* flags from MS-RDPBCGR 2.2.1.1.1 (`RDP_NEG_REQ.requestedProtocols`)
/// and 2.2.1.2.1 (`RDP_NEG_RSP.selectedProtocol`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SecurityProtocol(pub u32);

impl SecurityProtocol {
    pub const RDP: Self = Self(0x0000_0000);
    pub const SSL: Self = Self(0x0000_0001);
    pub const HYBRID: Self = Self(0x0000_0002);
    pub const RDSTLS: Self = Self(0x0000_0004);
    pub const HYBRID_EX: Self = Self(0x0000_0008);

    pub fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }
}

impl core::ops::BitOr for SecurityProtocol {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// `RDP_NEG_REQ.flags` (MS-RDPBCGR 2.2.1.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RequestFlags(pub u8);

/// `RDP_NEG_RSP.flags` (MS-RDPBCGR 2.2.1.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResponseFlags(pub u8);

/// `RDP_NEG_FAILURE.failureCode` (MS-RDPBCGR 2.2.1.2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureCode(pub u32);

impl FailureCode {
    pub const SSL_REQUIRED_BY_SERVER: Self = Self(1);
    pub const SSL_NOT_ALLOWED_BY_SERVER: Self = Self(2);
    pub const SSL_CERT_NOT_ON_SERVER: Self = Self(3);
    pub const INCONSISTENT_FLAGS: Self = Self(4);
    pub const HYBRID_REQUIRED_BY_SERVER: Self = Self(5);
    pub const SSL_WITH_USER_AUTH_REQUIRED_BY_SERVER: Self = Self(6);
}

/// Client X.224 Connection Request (MS-RDPBCGR 2.2.1.1), the very first PDU
/// on the wire - always sent in cleartext, even when TLS is negotiated
/// (2.2.1.1: the RDP Negotiation Request rides inside this, before any
/// security upgrade happens).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionRequest {
    /// `mstshash=` cookie carrying a client-chosen identifier (usually the
    /// Windows username); routing tokens (TS Gateway) are out of scope.
    pub cookie: Option<String>,
    pub flags: RequestFlags,
    pub protocol: SecurityProtocol,
}

impl ConnectionRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        if let Some(cookie) = &self.cookie {
            body.write_slice(COOKIE_PREFIX.as_bytes());
            body.write_slice(cookie.as_bytes());
            body.write_u16_le(CRLF);
        }
        body.write_u8(NEGO_REQUEST);
        body.write_u8(self.flags.0);
        body.write_u16_le(NEGO_BODY_SIZE);
        body.write_u32_le(self.protocol.0);

        encode_x224(TpduCode::CONNECTION_REQUEST, &body)
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        let (_tpkt, tpdu) = decode_x224_header(&mut cursor, TpduCode::CONNECTION_REQUEST)?;
        let variable_part_len = tpdu.variable_part_size();
        cursor.ensure(variable_part_len)?;

        let cookie = read_cookie(&mut cursor)?;

        if cursor.read_u8()? != NEGO_REQUEST {
            return Err(DecodeError::InvalidValue {
                field: "x224.nego_request.type",
                reason: "expected RDP_NEG_REQ type byte",
            });
        }
        let flags = RequestFlags(cursor.read_u8()?);
        let _length = cursor.read_u16_le()?;
        let protocol = SecurityProtocol(cursor.read_u32_le()?);

        Ok(Self { cookie, flags, protocol })
    }
}

/// Server X.224 Connection Confirm (MS-RDPBCGR 2.2.1.2): either accepts a
/// security protocol (`RDP_NEG_RSP`) or rejects the connection with a reason
/// (`RDP_NEG_FAILURE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionConfirm {
    Response { flags: ResponseFlags, protocol: SecurityProtocol },
    Failure { code: FailureCode },
}

impl ConnectionConfirm {
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        match self {
            Self::Response { flags, protocol } => {
                body.write_u8(NEGO_RESPONSE);
                body.write_u8(flags.0);
                body.write_u16_le(NEGO_BODY_SIZE);
                body.write_u32_le(protocol.0);
            }
            Self::Failure { code } => {
                body.write_u8(NEGO_FAILURE);
                body.write_u8(0); // reserved
                body.write_u16_le(NEGO_BODY_SIZE);
                body.write_u32_le(code.0);
            }
        }

        encode_x224(TpduCode::CONNECTION_CONFIRM, &body)
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        let (_tpkt, tpdu) = decode_x224_header(&mut cursor, TpduCode::CONNECTION_CONFIRM)?;
        let variable_part_len = tpdu.variable_part_size();
        cursor.ensure(variable_part_len)?;
        if variable_part_len < usize::from(NEGO_BODY_SIZE) {
            return Err(DecodeError::InvalidValue {
                field: "x224.connection_confirm",
                reason: "missing RDP_NEG_RSP/RDP_NEG_FAILURE body",
            });
        }

        match cursor.read_u8()? {
            NEGO_RESPONSE => {
                let flags = ResponseFlags(cursor.read_u8()?);
                let _length = cursor.read_u16_le()?;
                let protocol = SecurityProtocol(cursor.read_u32_le()?);
                Ok(Self::Response { flags, protocol })
            }
            NEGO_FAILURE => {
                let _reserved = cursor.read_u8()?;
                let _length = cursor.read_u16_le()?;
                let code = FailureCode(cursor.read_u32_le()?);
                Ok(Self::Failure { code })
            }
            _ => Err(DecodeError::InvalidValue {
                field: "x224.connection_confirm.type",
                reason: "expected RDP_NEG_RSP or RDP_NEG_FAILURE type byte",
            }),
        }
    }
}

/// Reads an optional `Cookie: mstshash=<value>\r\n` prefix without consuming
/// anything if it isn't present (routing tokens / no-cookie clients).
fn read_cookie(cursor: &mut ReadCursor<'_>) -> Result<Option<String>, DecodeError> {
    if cursor.remaining() < COOKIE_PREFIX.len() + 2 {
        return Ok(None);
    }
    if cursor.peek_slice(COOKIE_PREFIX.len())? != COOKIE_PREFIX.as_bytes() {
        return Ok(None);
    }
    cursor.advance(COOKIE_PREFIX.len());

    let value_start = cursor.pos();
    loop {
        if cursor.peek_u16_be()? == 0x0D0A {
            break;
        }
        cursor.advance(1);
    }
    let value_end = cursor.pos();
    cursor.advance(2); // CRLF

    let value = core::str::from_utf8(cursor.slice_from_to(value_start, value_end))
        .map_err(|_| DecodeError::InvalidValue {
            field: "x224.cookie",
            reason: "not valid UTF-8",
        })?
        .to_owned();
    Ok(Some(value))
}

fn encode_x224(code: TpduCode, body: &[u8]) -> Vec<u8> {
    let tpdu = TpduHeader::for_pdu(code, body.len());
    let packet_length = tpkt::HEADER_SIZE + tpdu.li as usize + 1;
    let mut out = Vec::with_capacity(packet_length);
    TpktHeader {
        packet_length: packet_length as u16,
    }
    .write(&mut out);
    tpdu.write(&mut out);
    out.write_slice(body);
    out
}

fn decode_x224_header(
    cursor: &mut ReadCursor<'_>,
    expected: TpduCode,
) -> Result<(TpktHeader, TpduHeader), DecodeError> {
    let tpkt = TpktHeader::decode(cursor)?;
    let tpdu = TpduHeader::decode(cursor, expected)?;
    Ok((tpkt, tpdu))
}

/// Every PDU after the initial Connection Request/Confirm exchange (MCS
/// Connect-Initial/Response and, later, every MCS domain PDU) rides as the
/// opaque payload of a plain X.224 Data TPDU - this pair is the "everything
/// else" framing layer the connector wraps/unwraps around them.
///
/// Unlike Connection Request/Confirm (where the nego bytes are genuinely
/// the TPDU header's "variable part", so `encode_x224`/`decode_x224_header`
/// folding them into `LI` is correct), a Data TPDU's `LI` is a *fixed*
/// value of 2 (code + EOT, nothing else) regardless of payload size - the
/// payload is separate "user data", framed only by the TPKT length. Reusing
/// `encode_x224` here would write a wrong (and `u8`-overflowing past ~253
/// bytes of payload) `LI` byte that a real client computing
/// `user_data_size` from `LI` would misparse.
pub fn wrap_data(payload: &[u8]) -> Vec<u8> {
    const DATA_TPDU_FIXED_SIZE: usize = 3; // LI + code + EOT
    let tpdu = TpduHeader {
        li: (DATA_TPDU_FIXED_SIZE - 1) as u8,
        code: TpduCode::DATA,
    };
    let packet_length = tpkt::HEADER_SIZE + DATA_TPDU_FIXED_SIZE + payload.len();
    let mut out = Vec::with_capacity(packet_length);
    TpktHeader {
        packet_length: packet_length as u16,
    }
    .write(&mut out);
    tpdu.write(&mut out);
    out.write_slice(payload);
    out
}

pub fn unwrap_data(input: &[u8]) -> Result<&[u8], DecodeError> {
    let mut cursor = ReadCursor::new(input);
    let (_tpkt, _tpdu) = decode_x224_header(&mut cursor, TpduCode::DATA)?;
    Ok(cursor.read_rest())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_request_round_trip_with_cookie() {
        let request = ConnectionRequest {
            cookie: Some("kmsrdp".to_owned()),
            flags: RequestFlags(0),
            protocol: SecurityProtocol::SSL,
        };
        let encoded = request.encode();
        let decoded = ConnectionRequest::decode(&encoded).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn connection_request_round_trip_without_cookie() {
        let request = ConnectionRequest {
            cookie: None,
            flags: RequestFlags(0),
            protocol: SecurityProtocol::SSL | SecurityProtocol::HYBRID,
        };
        let encoded = request.encode();
        let decoded = ConnectionRequest::decode(&encoded).unwrap();
        assert_eq!(decoded, request);
        assert!(decoded.protocol.contains(SecurityProtocol::SSL));
        assert!(decoded.protocol.contains(SecurityProtocol::HYBRID));
    }

    #[test]
    fn connection_confirm_response_round_trip() {
        let confirm = ConnectionConfirm::Response {
            flags: ResponseFlags(0),
            protocol: SecurityProtocol::SSL,
        };
        let encoded = confirm.encode();
        let decoded = ConnectionConfirm::decode(&encoded).unwrap();
        assert_eq!(decoded, confirm);
    }

    #[test]
    fn connection_confirm_failure_round_trip() {
        let confirm = ConnectionConfirm::Failure {
            code: FailureCode::SSL_REQUIRED_BY_SERVER,
        };
        let encoded = confirm.encode();
        let decoded = ConnectionConfirm::decode(&encoded).unwrap();
        assert_eq!(decoded, confirm);
    }

    /// Real bytes captured off the wire: `xfreerdp /v:127.0.0.1:33890
    /// /u:kmsrdp /p:x /cert:ignore /sec:tls` connecting to a plain `nc -l`
    /// listener (the Connection Request is always cleartext, sent before
    /// any TLS upgrade, so this needed no packet-capture tooling - just a
    /// raw TCP listener in place of the real server). Confirms our encoding
    /// matches a real client byte-for-byte, not just our own round-trip.
    #[test]
    fn decodes_real_xfreerdp_connection_request() {
        #[rustfmt::skip]
        let captured: &[u8] = &[
            0x03, 0x00, 0x00, 0x2c, 0x27, 0xe0, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x43, 0x6f, 0x6f, 0x6b, 0x69, 0x65, 0x3a, 0x20, 0x6d, 0x73, 0x74,
            0x73, 0x68, 0x61, 0x73, 0x68, 0x3d, 0x6b, 0x6d, 0x73, 0x72, 0x64,
            0x70, 0x0d, 0x0a, 0x01, 0x00, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00,
        ];

        let decoded = ConnectionRequest::decode(captured).unwrap();
        assert_eq!(
            decoded,
            ConnectionRequest {
                cookie: Some("kmsrdp".to_owned()),
                flags: RequestFlags(0),
                protocol: SecurityProtocol::SSL,
            }
        );

        // Round-trip: re-encoding what we just decoded must reproduce the
        // exact bytes xfreerdp put on the wire.
        assert_eq!(decoded.encode(), captured);
    }

    #[test]
    fn wrap_data_keeps_li_fixed_at_2_regardless_of_payload_size() {
        for payload in [vec![], vec![0xAB; 10], vec![0xCD; 300], vec![0xEF; 5000]] {
            let wrapped = wrap_data(&payload);
            // byte 4 is LI (after the 4-byte TPKT header) - must always be 2.
            assert_eq!(wrapped[4], 2, "LI must stay fixed at 2 for payload len {}", payload.len());
            assert_eq!(wrapped[5], 0xF0); // code: DATA
            assert_eq!(wrapped.len(), 4 + 3 + payload.len());

            let unwrapped = unwrap_data(&wrapped).unwrap();
            assert_eq!(unwrapped, payload.as_slice());
        }
    }

    #[test]
    fn rejects_truncated_connection_request() {
        let request = ConnectionRequest {
            cookie: Some("kmsrdp".to_owned()),
            flags: RequestFlags(0),
            protocol: SecurityProtocol::SSL,
        };
        let mut encoded = request.encode();
        encoded.truncate(encoded.len() - 2);
        assert!(matches!(
            ConnectionRequest::decode(&encoded),
            Err(DecodeError::NotEnoughBytes(_))
        ));
    }
}
