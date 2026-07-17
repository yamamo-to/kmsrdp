//! Licensing (MS-RDPELE), reduced to the one PDU a from-scratch server
//! actually needs: "Server License Error PDU - Valid Client", sent
//! immediately after the Client Info PDU to skip real licensing entirely.
//! Every field is a fixed constant - there is nothing to configure.

use crate::DecodeError;
use crate::cursor::{ReadCursor, WriteBuf};
use crate::headers::{BasicSecurityHeader, BasicSecurityHeaderFlags};

const PREAMBLE_ERROR_ALERT: u8 = 0xFF;
const PREAMBLE_VERSION_3: u8 = 3;
const STATUS_VALID_CLIENT: u32 = 0x0000_0007;
const ST_NO_TRANSITION: u32 = 0x0000_0002;
const BLOB_TYPE_ERROR: u16 = 0x0004;

/// `wMsgSize`: total size of everything from the preamble onward (8-byte
/// preamble... no - per MS-RDPBCGR, size of the licensing message *after*
/// the 4-byte Basic Security Header): 4 (preamble) + 4 (dwErrorCode) + 4
/// (dwStateTransition) + 4 (empty BlobHeader) = 16.
const MSG_SIZE: u16 = 16;

/// Encodes the fixed "Valid Client" license error message, Basic Security
/// Header included - ready to send as an MCS Send Data Indication payload.
pub fn encode_valid_client() -> Vec<u8> {
    let mut out = Vec::new();
    BasicSecurityHeader {
        flags: BasicSecurityHeaderFlags::LICENSE_PKT,
    }
    .write(&mut out);
    out.write_u8(PREAMBLE_ERROR_ALERT);
    out.write_u8(PREAMBLE_VERSION_3); // preambleFlags(0) | version(3)
    out.write_u16_le(MSG_SIZE);
    out.write_u32_le(STATUS_VALID_CLIENT);
    out.write_u32_le(ST_NO_TRANSITION);
    out.write_u16_le(BLOB_TYPE_ERROR);
    out.write_u16_le(0); // wBlobLen, empty
    out
}

/// Validates that `input` is exactly the fixed "Valid Client" message -
/// used by tests and by a client-side decoder if this crate is ever used
/// that way; a server never needs to decode its own outgoing PDU.
pub fn decode_valid_client(input: &[u8]) -> Result<(), DecodeError> {
    let mut cursor = ReadCursor::new(input);
    let header = BasicSecurityHeader::decode(&mut cursor)?;
    if !header.flags.contains(BasicSecurityHeaderFlags::LICENSE_PKT) {
        return Err(DecodeError::InvalidValue {
            field: "licensing.security_header.flags",
            reason: "expected LICENSE_PKT set",
        });
    }
    let msg_type = cursor.read_u8()?;
    if msg_type != PREAMBLE_ERROR_ALERT {
        return Err(DecodeError::InvalidValue {
            field: "licensing.preamble.msg_type",
            reason: "expected ErrorAlert",
        });
    }
    let _flags_and_version = cursor.read_u8()?;
    let _msg_size = cursor.read_u16_le()?;
    let error_code = cursor.read_u32_le()?;
    if error_code != STATUS_VALID_CLIENT {
        return Err(DecodeError::InvalidValue {
            field: "licensing.error_code",
            reason: "expected STATUS_VALID_CLIENT",
        });
    }
    let _state_transition = cursor.read_u32_le()?;
    let _blob_type = cursor.read_u16_le()?;
    let blob_len = cursor.read_u16_le()?;
    if blob_len != 0 {
        return Err(DecodeError::InvalidValue {
            field: "licensing.error_info.blob_len",
            reason: "expected an empty error-info blob",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_client_round_trip() {
        let encoded = encode_valid_client();
        assert_eq!(encoded.len(), 4 + usize::from(MSG_SIZE));
        decode_valid_client(&encoded).unwrap();
    }
}
