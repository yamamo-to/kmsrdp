//! ITU-T T.125 Multipoint Communication Service (MCS): the `Connect-Initial`/
//! `Connect-Response` exchange (BER-encoded, carrying the GCC Conference
//! Create Request/Response from `gcc.rs` as opaque `userData`), followed by
//! the domain PDUs used to erect the MCS domain and join channels (PER-ish
//! encoded, per `per.rs`).

use crate::cursor::{ReadCursor, WriteBuf};
use crate::{ber, per, DecodeError};

/// The MCS user (initiator) channel id and every subsequent domain-PDU
/// "initiator"/channel-id field is encoded relative to this base.
pub const BASE_CHANNEL_ID: u16 = 1001;

// ---------------------------------------------------------------------
// Connect-Initial / Connect-Response (BER)
// ---------------------------------------------------------------------

const MCS_TYPE_CONNECT_INITIAL: u8 = 0x65;
const MCS_TYPE_CONNECT_RESPONSE: u8 = 0x66;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DomainParameters {
    pub max_channel_ids: u32,
    pub max_user_ids: u32,
    pub max_token_ids: u32,
    pub num_priorities: u32,
    pub min_throughput: u32,
    pub max_height: u32,
    pub max_mcs_pdu_size: u32,
    pub protocol_version: u32,
}

impl DomainParameters {
    /// The fixed value set a server always sends back in Connect-Response,
    /// regardless of what the client asked for in Connect-Initial - this
    /// matches a real, interop-proven implementation's strategy rather than
    /// trying to "smart"-echo client values.
    pub const fn target() -> Self {
        Self {
            max_channel_ids: 34,
            max_user_ids: 2,
            max_token_ids: 0,
            num_priorities: 1,
            min_throughput: 0,
            max_height: 1,
            max_mcs_pdu_size: 65535,
            protocol_version: 2,
        }
    }

    /// A client's Connect-Initial also carries `min`/`max` bookends around
    /// its `target`; a server only ever needs `target()` for its own
    /// Connect-Response, but these two are handy for building test fixtures
    /// that look like a real client's Connect-Initial.
    pub const fn min() -> Self {
        Self {
            max_channel_ids: 1,
            max_user_ids: 1,
            max_token_ids: 1,
            num_priorities: 1,
            min_throughput: 0,
            max_height: 1,
            max_mcs_pdu_size: 1056,
            protocol_version: 2,
        }
    }

    pub const fn max() -> Self {
        Self {
            max_channel_ids: 65535,
            max_user_ids: 64535,
            max_token_ids: 65535,
            num_priorities: 1,
            min_throughput: 0,
            max_height: 1,
            max_mcs_pdu_size: 65535,
            protocol_version: 2,
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        let mut body = Vec::new();
        ber::write_integer(&mut body, self.max_channel_ids);
        ber::write_integer(&mut body, self.max_user_ids);
        ber::write_integer(&mut body, self.max_token_ids);
        ber::write_integer(&mut body, self.num_priorities);
        ber::write_integer(&mut body, self.min_throughput);
        ber::write_integer(&mut body, self.max_height);
        ber::write_integer(&mut body, self.max_mcs_pdu_size);
        ber::write_integer(&mut body, self.protocol_version);
        ber::write_sequence_tag(out, body.len());
        out.write_slice(&body);
    }

    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        let _len = ber::read_sequence_tag(cursor)?;
        Ok(Self {
            max_channel_ids: ber::read_integer(cursor)?,
            max_user_ids: ber::read_integer(cursor)?,
            max_token_ids: ber::read_integer(cursor)?,
            num_priorities: ber::read_integer(cursor)?,
            min_throughput: ber::read_integer(cursor)?,
            max_height: ber::read_integer(cursor)?,
            max_mcs_pdu_size: ber::read_integer(cursor)?,
            protocol_version: ber::read_integer(cursor)?,
        })
    }
}

/// Client -> Server. `user_data` is the raw PER-encoded GCC
/// `ConferenceCreateRequest` blob (see `gcc::ConferenceCreateRequest`),
/// treated here as an opaque `OCTET STRING`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectInitial {
    pub target_parameters: DomainParameters,
    pub min_parameters: DomainParameters,
    pub max_parameters: DomainParameters,
    pub user_data: Vec<u8>,
}

impl ConnectInitial {
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        ber::write_octet_string(&mut body, &[0x01]); // callingDomainSelector
        ber::write_octet_string(&mut body, &[0x01]); // calledDomainSelector
        ber::write_boolean(&mut body, true); // upwardFlag
        self.target_parameters.write(&mut body);
        self.min_parameters.write(&mut body);
        self.max_parameters.write(&mut body);
        ber::write_octet_string(&mut body, &self.user_data);

        let mut out = Vec::with_capacity(body.len() + 4);
        ber::write_application_tag(&mut out, MCS_TYPE_CONNECT_INITIAL, body.len());
        out.write_slice(&body);
        out
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        let _len = ber::read_application_tag(&mut cursor, MCS_TYPE_CONNECT_INITIAL)?;
        let _calling_domain_selector = ber::read_octet_string(&mut cursor)?;
        let _called_domain_selector = ber::read_octet_string(&mut cursor)?;
        let _upward_flag = ber::read_boolean(&mut cursor)?;
        let target_parameters = DomainParameters::decode(&mut cursor)?;
        let min_parameters = DomainParameters::decode(&mut cursor)?;
        let max_parameters = DomainParameters::decode(&mut cursor)?;
        let user_data = ber::read_octet_string(&mut cursor)?.to_vec();
        Ok(Self {
            target_parameters,
            min_parameters,
            max_parameters,
            user_data,
        })
    }
}

/// Server -> Client. `user_data` is the raw PER-encoded GCC
/// `ConferenceCreateResponse` blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectResponse {
    pub called_connect_id: u32,
    pub domain_parameters: DomainParameters,
    pub user_data: Vec<u8>,
}

impl ConnectResponse {
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        ber::write_enumerated(&mut body, 0); // result: rt-successful
        ber::write_integer(&mut body, self.called_connect_id);
        self.domain_parameters.write(&mut body);
        ber::write_octet_string(&mut body, &self.user_data);

        let mut out = Vec::with_capacity(body.len() + 4);
        ber::write_application_tag(&mut out, MCS_TYPE_CONNECT_RESPONSE, body.len());
        out.write_slice(&body);
        out
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        let _len = ber::read_application_tag(&mut cursor, MCS_TYPE_CONNECT_RESPONSE)?;
        let _result = ber::read_enumerated(&mut cursor)?;
        let called_connect_id = ber::read_integer(&mut cursor)?;
        let domain_parameters = DomainParameters::decode(&mut cursor)?;
        let user_data = ber::read_octet_string(&mut cursor)?.to_vec();
        Ok(Self {
            called_connect_id,
            domain_parameters,
            user_data,
        })
    }
}

// ---------------------------------------------------------------------
// Domain PDUs (PER-ish, one choice byte selecting the PDU type)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainMcsPdu {
    ErectDomainRequest,
    DisconnectProviderUltimatum,
    AttachUserRequest,
    AttachUserConfirm,
    ChannelJoinRequest,
    ChannelJoinConfirm,
    SendDataRequest,
    SendDataIndication,
}

impl DomainMcsPdu {
    fn as_u8(self) -> u8 {
        match self {
            Self::ErectDomainRequest => 1,
            Self::DisconnectProviderUltimatum => 8,
            Self::AttachUserRequest => 10,
            Self::AttachUserConfirm => 11,
            Self::ChannelJoinRequest => 14,
            Self::ChannelJoinConfirm => 15,
            Self::SendDataRequest => 25,
            Self::SendDataIndication => 26,
        }
    }

    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::ErectDomainRequest,
            8 => Self::DisconnectProviderUltimatum,
            10 => Self::AttachUserRequest,
            11 => Self::AttachUserConfirm,
            14 => Self::ChannelJoinRequest,
            15 => Self::ChannelJoinConfirm,
            25 => Self::SendDataRequest,
            26 => Self::SendDataIndication,
            _ => return None,
        })
    }
}

fn write_choice_byte(out: &mut Vec<u8>, pdu: DomainMcsPdu, options: u8) {
    per::write_choice(out, (pdu.as_u8() << 2) | options);
}

fn read_choice_byte(cursor: &mut ReadCursor<'_>, expected: DomainMcsPdu) -> Result<u8, DecodeError> {
    let byte = per::read_choice(cursor)?;
    let (pdu, options) = (DomainMcsPdu::from_u8(byte >> 2), byte & 0x3);
    if pdu != Some(expected) {
        return Err(DecodeError::InvalidValue {
            field: "mcs.domain_pdu.choice",
            reason: "unexpected domain PDU choice byte",
        });
    }
    Ok(options)
}

/// MS-RDPBCGR optional `nonStandard` on several domain PDUs: when the
/// choice-byte option bit 0x2 is set, a 2-byte field follows.
fn skip_optional_non_standard(cursor: &mut ReadCursor<'_>, options: u8) -> Result<(), DecodeError> {
    if options & 0x2 != 0 {
        cursor.ensure(2).map_err(DecodeError::from)?;
        cursor.advance(2);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErectDomainRequest {
    pub sub_height: u32,
    pub sub_interval: u32,
}

impl ErectDomainRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_choice_byte(&mut out, DomainMcsPdu::ErectDomainRequest, 0);
        per::write_u32(&mut out, self.sub_height);
        per::write_u32(&mut out, self.sub_interval);
        out
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        Self::decode_from_cursor(&mut cursor)
    }

    pub fn decode_from_cursor(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        let options = read_choice_byte(cursor, DomainMcsPdu::ErectDomainRequest)?;
        let req = Self {
            sub_height: per::read_u32(cursor)?,
            sub_interval: per::read_u32(cursor)?,
        };
        skip_optional_non_standard(cursor, options)?;
        Ok(req)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachUserRequest;

impl AttachUserRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_choice_byte(&mut out, DomainMcsPdu::AttachUserRequest, 0);
        out
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        Self::decode_from_cursor(&mut cursor)
    }

    pub fn decode_from_cursor(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        let options = read_choice_byte(cursor, DomainMcsPdu::AttachUserRequest)?;
        skip_optional_non_standard(cursor, options)?;
        Ok(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachUserConfirm {
    pub result: u8,
    pub initiator: u16,
}

impl AttachUserConfirm {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_choice_byte(&mut out, DomainMcsPdu::AttachUserConfirm, 2);
        per::write_enum(&mut out, self.result);
        per::write_u16(&mut out, self.initiator, BASE_CHANNEL_ID);
        out
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        read_choice_byte(&mut cursor, DomainMcsPdu::AttachUserConfirm)?;
        Ok(Self {
            result: per::read_enum(&mut cursor)?,
            initiator: per::read_u16(&mut cursor, BASE_CHANNEL_ID)?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelJoinRequest {
    pub initiator: u16,
    pub channel_id: u16,
}

impl ChannelJoinRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_choice_byte(&mut out, DomainMcsPdu::ChannelJoinRequest, 0);
        per::write_u16(&mut out, self.initiator, BASE_CHANNEL_ID);
        per::write_u16(&mut out, self.channel_id, 0);
        out
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        Self::decode_from_cursor(&mut cursor)
    }

    pub fn decode_from_cursor(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        read_choice_byte(cursor, DomainMcsPdu::ChannelJoinRequest)?;
        Ok(Self {
            initiator: per::read_u16(cursor, BASE_CHANNEL_ID)?,
            channel_id: per::read_u16(cursor, 0)?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelJoinConfirm {
    pub result: u8,
    pub initiator: u16,
    pub requested_channel_id: u16,
    pub channel_id: u16,
}

impl ChannelJoinConfirm {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_choice_byte(&mut out, DomainMcsPdu::ChannelJoinConfirm, 2);
        per::write_enum(&mut out, self.result);
        per::write_u16(&mut out, self.initiator, BASE_CHANNEL_ID);
        per::write_u16(&mut out, self.requested_channel_id, 0);
        per::write_u16(&mut out, self.channel_id, 0);
        out
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        read_choice_byte(&mut cursor, DomainMcsPdu::ChannelJoinConfirm)?;
        Ok(Self {
            result: per::read_enum(&mut cursor)?,
            initiator: per::read_u16(&mut cursor, BASE_CHANNEL_ID)?,
            requested_channel_id: per::read_u16(&mut cursor, 0)?,
            channel_id: per::read_u16(&mut cursor, 0)?,
        })
    }
}

/// Fixed `dataPriority | segmentation` byte: high priority with both
/// `begin`/`end` segmentation bits set - RDP never fragments at the MCS
/// layer, every Send Data Request/Indication is one complete PDU.
const SEND_DATA_PRIORITY_AND_SEGMENTATION: u8 = 0x70;
const SEND_DATA_SEG_END: u8 = 0x10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendData {
    pub initiator: u16,
    pub channel_id: u16,
    pub data: Vec<u8>,
    /// MCS segmentation `end` bit. When false, more Send Data PDUs follow
    /// that continue this user-data stream and must be concatenated before
    /// the upper-layer PDU is complete.
    pub complete: bool,
}

impl SendData {
    fn encode(&self, pdu: DomainMcsPdu) -> Vec<u8> {
        let mut out = Vec::new();
        write_choice_byte(&mut out, pdu, 0);
        per::write_u16(&mut out, self.initiator, BASE_CHANNEL_ID);
        per::write_u16(&mut out, self.channel_id, 0);
        out.write_u8(SEND_DATA_PRIORITY_AND_SEGMENTATION);
        per::write_length(&mut out, self.data.len());
        out.write_slice(&self.data);
        out
    }

    fn decode(input: &[u8], pdu: DomainMcsPdu) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        read_choice_byte(&mut cursor, pdu)?;
        let initiator = per::read_u16(&mut cursor, BASE_CHANNEL_ID)?;
        let channel_id = per::read_u16(&mut cursor, 0)?;
        let priority_and_segmentation = cursor.read_u8()?;
        let length = per::read_length(&mut cursor)?;
        let data = cursor.read_slice(length)?.to_vec();
        Ok(Self {
            initiator,
            channel_id,
            data,
            complete: priority_and_segmentation & SEND_DATA_SEG_END != 0,
        })
    }

    pub fn encode_request(&self) -> Vec<u8> {
        self.encode(DomainMcsPdu::SendDataRequest)
    }

    pub fn decode_request(input: &[u8]) -> Result<Self, DecodeError> {
        Self::decode(input, DomainMcsPdu::SendDataRequest)
    }

    pub fn encode_indication(&self) -> Vec<u8> {
        self.encode(DomainMcsPdu::SendDataIndication)
    }

    pub fn decode_indication(input: &[u8]) -> Result<Self, DecodeError> {
        Self::decode(input, DomainMcsPdu::SendDataIndication)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisconnectProviderUltimatum {
    pub reason: u8,
}

impl DisconnectProviderUltimatum {
    pub fn encode(&self) -> Vec<u8> {
        let byte1 = (DomainMcsPdu::DisconnectProviderUltimatum.as_u8() << 2) | ((self.reason >> 1) & 0x03);
        let byte2 = ((u16::from(self.reason) << 7) & 0xFF) as u8;
        vec![byte1, byte2]
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        let byte1 = cursor.read_u8()?;
        let byte2 = cursor.read_u8()?;
        let (pdu, low_bits) = (DomainMcsPdu::from_u8(byte1 >> 2), byte1 & 0x03);
        if pdu != Some(DomainMcsPdu::DisconnectProviderUltimatum) {
            return Err(DecodeError::InvalidValue {
                field: "mcs.disconnect_provider_ultimatum.choice",
                reason: "unexpected choice byte",
            });
        }
        let reason = (low_bits << 1) | (byte2 >> 7);
        Ok(Self { reason })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_parameters_round_trip() {
        let params = DomainParameters::target();
        let mut buf = Vec::new();
        params.write(&mut buf);
        let mut cursor = ReadCursor::new(&buf);
        assert_eq!(DomainParameters::decode(&mut cursor).unwrap(), params);
    }

    #[test]
    fn connect_initial_round_trip() {
        let ci = ConnectInitial {
            target_parameters: DomainParameters::target(),
            min_parameters: DomainParameters::min(),
            max_parameters: DomainParameters::max(),
            user_data: b"opaque-gcc-bytes".to_vec(),
        };
        let encoded = ci.encode();
        let decoded = ConnectInitial::decode(&encoded).unwrap();
        assert_eq!(decoded, ci);
    }

    #[test]
    fn connect_response_round_trip() {
        let cr = ConnectResponse {
            called_connect_id: 0,
            domain_parameters: DomainParameters::target(),
            user_data: b"opaque-gcc-response-bytes".to_vec(),
        };
        let encoded = cr.encode();
        let decoded = ConnectResponse::decode(&encoded).unwrap();
        assert_eq!(decoded, cr);
    }

    #[test]
    fn erect_domain_request_round_trip() {
        let pdu = ErectDomainRequest {
            sub_height: 0,
            sub_interval: 0,
        };
        assert_eq!(ErectDomainRequest::decode(&pdu.encode()).unwrap(), pdu);
    }

    #[test]
    fn attach_user_round_trip() {
        assert_eq!(AttachUserRequest::decode(&AttachUserRequest.encode()).unwrap(), AttachUserRequest);

        let confirm = AttachUserConfirm { result: 0, initiator: 1002 };
        assert_eq!(AttachUserConfirm::decode(&confirm.encode()).unwrap(), confirm);
    }

    #[test]
    fn channel_join_round_trip() {
        let request = ChannelJoinRequest {
            initiator: 1002,
            channel_id: 1003,
        };
        assert_eq!(ChannelJoinRequest::decode(&request.encode()).unwrap(), request);

        let confirm = ChannelJoinConfirm {
            result: 0,
            initiator: 1002,
            requested_channel_id: 1003,
            channel_id: 1003,
        };
        assert_eq!(ChannelJoinConfirm::decode(&confirm.encode()).unwrap(), confirm);
    }

    #[test]
    fn send_data_round_trip() {
        let msg = SendData {
            initiator: 1002,
            channel_id: 1003,
            data: b"hello virtual channel".to_vec(),
            complete: true,
        };
        assert_eq!(SendData::decode_request(&msg.encode_request()).unwrap(), msg);
        assert_eq!(SendData::decode_indication(&msg.encode_indication()).unwrap(), msg);
    }

    #[test]
    fn send_data_large_payload_uses_long_form_length() {
        let msg = SendData {
            initiator: 1002,
            channel_id: 1004,
            data: vec![0xAB; 5000],
            complete: true,
        };
        assert_eq!(SendData::decode_request(&msg.encode_request()).unwrap(), msg);
    }

    #[test]
    fn disconnect_provider_ultimatum_matches_known_example() {
        // From MS-RDPBCGR's own worked example: choice=8, reason=3 -> `21 80`.
        let pdu = DisconnectProviderUltimatum { reason: 3 };
        assert_eq!(pdu.encode(), [0x21, 0x80]);
        assert_eq!(DisconnectProviderUltimatum::decode(&pdu.encode()).unwrap(), pdu);
    }
}
