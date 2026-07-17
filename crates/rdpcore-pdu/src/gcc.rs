//! ITU-T T.124 Generic Conference Control: the `ConferenceCreateRequest`/
//! `ConferenceCreateResponse` wrapper (PER-encoded) carrying the actual
//! Client/Server GCC data blocks (Core/Security/Network - each a plain
//! little-endian struct behind a 4-byte type+length header). This whole
//! blob rides as the opaque `userData` OCTET STRING inside MCS
//! Connect-Initial/Connect-Response (`mcs.rs`).
//!
//! Cluster Data and every other optional client block (monitor layout,
//! ...) are accepted on decode (skipped by their header's declared length)
//! but not modeled - a from-scratch phase-1 server has no use for their
//! content yet. CS_MCS_MSGCHANNEL / SC_MCS_MSGCHANNEL are modeled because
//! mstsc requires the server to answer with a message-channel MCS ID.

use crate::cursor::{ReadCursor, WriteBuf};
use crate::utf16::{read_fixed as read_utf16_fixed, write_fixed as write_utf16_fixed};
use crate::{per, DecodeError};

const GCC_CONFERENCE_OBJECT_ID: [u8; 6] = [0, 0, 20, 124, 0, 1];
const CLIENT_TO_SERVER_H221: &[u8; 4] = b"Duca";
const SERVER_TO_CLIENT_H221: &[u8; 4] = b"McDn";
const CONFERENCE_NAME: &str = "1";

pub const CS_CORE: u16 = 0xC001;
pub const CS_SECURITY: u16 = 0xC002;
pub const CS_NET: u16 = 0xC003;
pub const CS_CLUSTER: u16 = 0xC004;
pub const CS_MONITOR: u16 = 0xC005;
pub const CS_MCS_MSGCHANNEL: u16 = 0xC006;

pub const SC_CORE: u16 = 0x0C01;
pub const SC_SECURITY: u16 = 0x0C02;
pub const SC_NET: u16 = 0x0C03;
pub const SC_MCS_MSGCHANNEL: u16 = 0x0C04;

const CHANNELS_MAX: usize = 31;

// ---------------------------------------------------------------------
// Per-block 4-byte header
// ---------------------------------------------------------------------

struct UserDataHeader {
    kind: u16,
    length: u16,
}

impl UserDataHeader {
    fn write(&self, out: &mut Vec<u8>) {
        out.write_u16_le(self.kind);
        out.write_u16_le(self.length);
    }

    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            kind: cursor.read_u16_le()?,
            length: cursor.read_u16_le()?,
        })
    }
}

/// Writes `body` prefixed by a `UserDataHeader{kind, length: body.len()+4}`.
fn write_block(out: &mut Vec<u8>, kind: u16, body: &[u8]) {
    UserDataHeader {
        kind,
        length: (body.len() + 4) as u16,
    }
    .write(out);
    out.write_slice(body);
}

// ---------------------------------------------------------------------
// Client Core Data (CS_CORE) - fixed 128-byte part only; the optional tail
// (postBeta2ColorDepth, ..., deviceScaleFactor) is accepted-but-ignored.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCoreData {
    pub version: u32,
    pub desktop_width: u16,
    pub desktop_height: u16,
    pub color_depth: u16,
    pub sas_sequence: u16,
    pub keyboard_layout: u32,
    pub client_build: u32,
    pub client_name: String,
    pub keyboard_type: u32,
    pub keyboard_subtype: u32,
    pub keyboard_function_key: u32,
    pub ime_file_name: String,
}

impl ClientCoreData {
    const FIXED_PART_SIZE: usize = 128;

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::FIXED_PART_SIZE);
        out.write_u32_le(self.version);
        out.write_u16_le(self.desktop_width);
        out.write_u16_le(self.desktop_height);
        out.write_u16_le(self.color_depth);
        out.write_u16_le(self.sas_sequence);
        out.write_u32_le(self.keyboard_layout);
        out.write_u32_le(self.client_build);
        write_utf16_fixed(&mut out, &self.client_name, 32);
        out.write_u32_le(self.keyboard_type);
        out.write_u32_le(self.keyboard_subtype);
        out.write_u32_le(self.keyboard_function_key);
        write_utf16_fixed(&mut out, &self.ime_file_name, 64);
        debug_assert_eq!(out.len(), Self::FIXED_PART_SIZE);
        out
    }

    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        cursor.ensure(Self::FIXED_PART_SIZE)?;
        let version = cursor.read_u32_le()?;
        let desktop_width = cursor.read_u16_le()?;
        let desktop_height = cursor.read_u16_le()?;
        let color_depth = cursor.read_u16_le()?;
        let sas_sequence = cursor.read_u16_le()?;
        let keyboard_layout = cursor.read_u32_le()?;
        let client_build = cursor.read_u32_le()?;
        let client_name = read_utf16_fixed(cursor.read_slice(32)?);
        let keyboard_type = cursor.read_u32_le()?;
        let keyboard_subtype = cursor.read_u32_le()?;
        let keyboard_function_key = cursor.read_u32_le()?;
        let ime_file_name = read_utf16_fixed(cursor.read_slice(64)?);
        Ok(Self {
            version,
            desktop_width,
            desktop_height,
            color_depth,
            sas_sequence,
            keyboard_layout,
            client_build,
            client_name,
            keyboard_type,
            keyboard_subtype,
            keyboard_function_key,
            ime_file_name,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClientSecurityData {
    pub encryption_methods: u32,
    pub ext_encryption_methods: u32,
}

impl ClientSecurityData {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8);
        out.write_u32_le(self.encryption_methods);
        out.write_u32_le(self.ext_encryption_methods);
        out
    }

    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            encryption_methods: cursor.read_u32_le()?,
            ext_encryption_methods: cursor.read_u32_le()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelDef {
    pub name: String,
    pub options: u32,
}

impl ChannelDef {
    fn encode(&self, out: &mut Vec<u8>) {
        let mut name_bytes = [0u8; 8];
        let ascii = self.name.as_bytes();
        let n = ascii.len().min(7);
        name_bytes[..n].copy_from_slice(&ascii[..n]);
        out.write_slice(&name_bytes);
        out.write_u32_le(self.options);
    }

    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        let name_bytes = cursor.read_slice(8)?;
        let end = name_bytes.iter().position(|&b| b == 0).unwrap_or(8);
        let name = String::from_utf8_lossy(&name_bytes[..end]).into_owned();
        let options = cursor.read_u32_le()?;
        Ok(Self { name, options })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientNetworkData {
    pub channels: Vec<ChannelDef>,
}

impl ClientNetworkData {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.write_u32_le(self.channels.len() as u32);
        for channel in &self.channels {
            channel.encode(&mut out);
        }
        out
    }

    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        let count = (cursor.read_u32_le()? as usize).min(CHANNELS_MAX);
        let channels = (0..count).map(|_| ChannelDef::decode(cursor)).collect::<Result<_, _>>()?;
        Ok(Self { channels })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientClusterData {
    pub flags: u32,
    pub redirected_session_id: u32,
}

impl ClientClusterData {
    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            flags: cursor.read_u32_le()?,
            redirected_session_id: cursor.read_u32_le()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientMessageChannelData {
    pub flags: u32,
}

impl ClientMessageChannelData {
    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            flags: cursor.read_u32_le()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientGccBlocks {
    pub core: ClientCoreData,
    pub security: ClientSecurityData,
    pub network: Option<ClientNetworkData>,
    /// Present when the client sent CS_CLUSTER (mstsc always does).
    pub cluster: Option<ClientClusterData>,
    /// Present when the client sent CS_MCS_MSGCHANNEL (0xC006).
    pub message_channel: Option<ClientMessageChannelData>,
    /// Parsed from the CS_CORE optional tail when the client sent one
    /// (mstsc always does); `None` for minimal 128-byte-only fixtures.
    pub early_capability_flags: Option<u16>,
}

impl ClientGccBlocks {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_block(&mut out, CS_CORE, &self.core.encode());
        write_block(&mut out, CS_SECURITY, &self.security.encode());
        if let Some(network) = &self.network {
            write_block(&mut out, CS_NET, &network.encode());
        }
        out
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        let mut core = None;
        let mut security = None;
        let mut network = None;
        let mut cluster = None;
        let mut message_channel = None;
        let mut core_early_capability_flags = None;

        while cursor.remaining() >= 4 {
            let header = UserDataHeader::decode(&mut cursor)?;
            let body_len = usize::from(header.length).saturating_sub(4);
            let body = cursor.read_slice(body_len)?;
            let mut body_cursor = ReadCursor::new(body);
            match header.kind {
                CS_CORE => {
                    core_early_capability_flags = early_capability_flags_from_cs_core(body);
                    core = Some(ClientCoreData::decode(&mut body_cursor)?);
                }
                CS_SECURITY => security = Some(ClientSecurityData::decode(&mut body_cursor)?),
                CS_NET => network = Some(ClientNetworkData::decode(&mut body_cursor)?),
                CS_CLUSTER => cluster = Some(ClientClusterData::decode(&mut body_cursor)?),
                CS_MCS_MSGCHANNEL => message_channel = Some(ClientMessageChannelData::decode(&mut body_cursor)?),
                CS_MONITOR => {} // client-only; no server response block exists.
                _ => {}
            }
        }

        Ok(Self {
            core: core.ok_or(DecodeError::InvalidValue {
                field: "gcc.client_blocks.core",
                reason: "missing mandatory CS_CORE block",
            })?,
            security: security.ok_or(DecodeError::InvalidValue {
                field: "gcc.client_blocks.security",
                reason: "missing mandatory CS_SECURITY block",
            })?,
            network,
            cluster,
            message_channel,
            early_capability_flags: core_early_capability_flags,
        })
    }
}

/// MS-RDPBCGR 2.2.1.3.2.1 client `earlyCapabilityFlags` bits (RNS_UD_CS_*).
pub const CS_SUPPORT_ERRINFO_PDU: u16 = 0x0001;
pub const CS_WANT_32BPP_SESSION: u16 = 0x0002;

/// MS-RDPBCGR 2.2.1.4.2 server `earlyCapabilityFlags` bits (RNS_UD_SC_*).
pub const SC_EDGE_ACTIONS_SUPPORTED_V1: u16 = 0x0001;
pub const SC_DYNAMIC_DST_SUPPORTED: u16 = 0x0002;

/// Reads `earlyCapabilityFlags` from a CS_CORE block body when the client
/// sent the optional tail (present in every modern mstsc connect).
fn early_capability_flags_from_cs_core(body: &[u8]) -> Option<u16> {
    // MS-RDPBCGR 2.2.1.3.2 optional tail after the fixed 128-byte header:
    // postBeta2ColorDepth(2) + clientProductId(2) + serialNumber(4) +
    // highColorDepth(2) + supportedColorDepths(2) = 12 bytes.
    const OFFSET: usize = 128 + 12;
    if body.len() < OFFSET + 2 {
        return None;
    }
    Some(u16::from_le_bytes([body[OFFSET], body[OFFSET + 1]]))
}

// ---------------------------------------------------------------------
// Server blocks (Core, Network, Security - in this wire order)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerCoreData {
    pub version: u32,
    pub client_requested_protocols: Option<u32>,
    /// Present when answering a client that sent the CS_CORE optional tail.
    /// These are **server** flags (RNS_UD_SC_*), 32-bit per MS-RDPBCGR
    /// 2.2.1.4.2 - not an echo of the client's 16-bit CS earlyCapabilityFlags.
    pub early_capability_flags: Option<u32>,
}

impl ServerCoreData {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.write_u32_le(self.version);
        if let Some(protocols) = self.client_requested_protocols {
            out.write_u32_le(protocols);
        }
        if let Some(flags) = self.early_capability_flags {
            out.write_u32_le(flags);
        }
        out
    }

    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        let version = cursor.read_u32_le()?;
        let client_requested_protocols = if cursor.remaining() >= 4 {
            Some(cursor.read_u32_le()?)
        } else {
            None
        };
        let early_capability_flags = if cursor.remaining() >= 4 {
            Some(cursor.read_u32_le()?)
        } else {
            None
        };
        Ok(Self {
            version,
            client_requested_protocols,
            early_capability_flags,
        })
    }
}

/// Only the TLS-only (`no_security`) encoding: `encryptionMethod` and
/// `encryptionLevel` both zero, no server-random/certificate fields at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ServerSecurityData;

impl ServerSecurityData {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8);
        out.write_u32_le(0); // encryptionMethod = NONE
        out.write_u32_le(0); // encryptionLevel = None
        out
    }

    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        let encryption_method = cursor.read_u32_le()?;
        let encryption_level = cursor.read_u32_le()?;
        if encryption_method != 0 || encryption_level != 0 {
            return Err(DecodeError::InvalidValue {
                field: "gcc.server_security",
                reason: "only the no-security (TLS-only) encoding is supported",
            });
        }
        Ok(Self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerNetworkData {
    pub io_channel_id: u16,
    pub channel_ids: Vec<u16>,
}

impl ServerNetworkData {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.write_u16_le(self.io_channel_id);
        out.write_u16_le(self.channel_ids.len() as u16);
        for &id in &self.channel_ids {
            out.write_u16_le(id);
        }
        if self.channel_ids.len() % 2 == 1 {
            out.write_u16_le(0); // pad to a multiple of 4 bytes
        }
        out
    }

    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        let io_channel_id = cursor.read_u16_le()?;
        let count = cursor.read_u16_le()? as usize;
        let channel_ids = (0..count).map(|_| cursor.read_u16_le()).collect::<Result<_, _>>()?;
        if count % 2 == 1 && cursor.remaining() >= 2 {
            cursor.advance(2); // padding
        }
        Ok(Self { io_channel_id, channel_ids })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerMessageChannelData {
    pub mcs_channel_id: u16,
}

impl ServerMessageChannelData {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2);
        out.write_u16_le(self.mcs_channel_id);
        out
    }

    fn decode(cursor: &mut ReadCursor<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            mcs_channel_id: cursor.read_u16_le()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerGccBlocks {
    pub core: ServerCoreData,
    pub network: ServerNetworkData,
    pub security: ServerSecurityData,
    /// Required in Connect Response when the client sent CS_MCS_MSGCHANNEL.
    pub message_channel: Option<ServerMessageChannelData>,
}

impl ServerGccBlocks {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_block(&mut out, SC_CORE, &self.core.encode());
        write_block(&mut out, SC_NET, &self.network.encode());
        write_block(&mut out, SC_SECURITY, &self.security.encode());
        if let Some(message_channel) = &self.message_channel {
            write_block(&mut out, SC_MCS_MSGCHANNEL, &message_channel.encode());
        }
        out
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        let mut core = None;
        let mut network = None;
        let mut security = None;
        let mut message_channel = None;

        while cursor.remaining() >= 4 {
            let header = UserDataHeader::decode(&mut cursor)?;
            let body_len = usize::from(header.length).saturating_sub(4);
            let body = cursor.read_slice(body_len)?;
            let mut body_cursor = ReadCursor::new(body);
            match header.kind {
                SC_CORE => core = Some(ServerCoreData::decode(&mut body_cursor)?),
                SC_NET => network = Some(ServerNetworkData::decode(&mut body_cursor)?),
                SC_SECURITY => security = Some(ServerSecurityData::decode(&mut body_cursor)?),
                SC_MCS_MSGCHANNEL => message_channel = Some(ServerMessageChannelData::decode(&mut body_cursor)?),
                _ => {}
            }
        }

        Ok(Self {
            core: core.ok_or(DecodeError::InvalidValue {
                field: "gcc.server_blocks.core",
                reason: "missing mandatory SC_CORE block",
            })?,
            network: network.ok_or(DecodeError::InvalidValue {
                field: "gcc.server_blocks.network",
                reason: "missing mandatory SC_NET block",
            })?,
            security: security.ok_or(DecodeError::InvalidValue {
                field: "gcc.server_blocks.security",
                reason: "missing mandatory SC_SECURITY block",
            })?,
            message_channel,
        })
    }
}

// ---------------------------------------------------------------------
// ConferenceCreateRequest / ConferenceCreateResponse (the PER wrapper)
// ---------------------------------------------------------------------

const CONFERENCE_REQUEST_CONNECT_PDU_OVERHEAD: usize = 12;
const CONFERENCE_RESPONSE_CONNECT_PDU_OVERHEAD: usize = 13;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConferenceCreateRequest {
    pub client_gcc_blocks: Vec<u8>,
}

impl ConferenceCreateRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.write_u8(0x00); // ConnectData::Key CHOICE = object
        per::write_object_id(&mut out, GCC_CONFERENCE_OBJECT_ID);
        per::write_length(&mut out, self.client_gcc_blocks.len() + CONFERENCE_REQUEST_CONNECT_PDU_OVERHEAD);
        out.write_u8(0x00); // ConnectGCCPDU CHOICE = conferenceCreateRequest
        out.write_u8(0x08); // optional-field selection: userData present
        per::write_numeric_string(&mut out, CONFERENCE_NAME, 1);
        per::write_padding(&mut out, 1);
        per::write_number_of_sets(&mut out, 1);
        out.write_u8(0xC0); // UserData entry Key CHOICE = h221NonStandard
        per::write_octet_string(&mut out, CLIENT_TO_SERVER_H221, 4);
        per::write_length(&mut out, self.client_gcc_blocks.len());
        out.write_slice(&self.client_gcc_blocks);
        out
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        expect_byte(&mut cursor, 0x00, "gcc.conference_request.key_choice")?;
        let _oid = per::read_object_id(&mut cursor)?;
        let _connect_pdu_len = per::read_length(&mut cursor)?;
        expect_byte(&mut cursor, 0x00, "gcc.conference_request.gcc_pdu_choice")?;
        let _selection = per::read_selection(&mut cursor)?;
        let _conference_name = per::read_numeric_string(&mut cursor, 1)?;
        per::read_padding(&mut cursor, 1)?;
        let _number_of_sets = per::read_number_of_sets(&mut cursor)?;
        let _entry_key_choice = cursor.read_u8()?;
        let _h221_id = per::read_octet_string(&mut cursor, 4)?;
        let blocks_len = per::read_length(&mut cursor)?;
        let client_gcc_blocks = cursor.read_slice(blocks_len)?.to_vec();
        Ok(Self { client_gcc_blocks })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConferenceCreateResponse {
    pub node_id: u16,
    pub server_gcc_blocks: Vec<u8>,
}

impl ConferenceCreateResponse {
    const CHOICE: u8 = 0x14;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.write_u8(0x00); // ConnectData::Key CHOICE = object
        per::write_object_id(&mut out, GCC_CONFERENCE_OBJECT_ID);
        per::write_length(&mut out, self.server_gcc_blocks.len() + CONFERENCE_RESPONSE_CONNECT_PDU_OVERHEAD);
        out.write_u8(Self::CHOICE); // ConnectGCCPDU CHOICE = conferenceCreateResponse
        per::write_u16(&mut out, self.node_id, 1001);
        per::write_u32(&mut out, 1); // tag, fixed
        per::write_enum(&mut out, 0); // result, fixed (rt-success-ish placeholder)
        per::write_number_of_sets(&mut out, 1);
        out.write_u8(0xC0);
        per::write_octet_string(&mut out, SERVER_TO_CLIENT_H221, 4);
        per::write_length(&mut out, self.server_gcc_blocks.len());
        out.write_slice(&self.server_gcc_blocks);
        out
    }

    pub fn decode(input: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = ReadCursor::new(input);
        expect_byte(&mut cursor, 0x00, "gcc.conference_response.key_choice")?;
        let _oid = per::read_object_id(&mut cursor)?;
        let _connect_pdu_len = per::read_length(&mut cursor)?;
        expect_byte(&mut cursor, Self::CHOICE, "gcc.conference_response.gcc_pdu_choice")?;
        let node_id = per::read_u16(&mut cursor, 1001)?;
        let _tag = per::read_u32(&mut cursor)?;
        let _result = per::read_enum(&mut cursor)?;
        let _number_of_sets = per::read_number_of_sets(&mut cursor)?;
        let _entry_key_choice = cursor.read_u8()?;
        let _h221_id = per::read_octet_string(&mut cursor, 4)?;
        let blocks_len = per::read_length(&mut cursor)?;
        let server_gcc_blocks = cursor.read_slice(blocks_len)?.to_vec();
        Ok(Self { node_id, server_gcc_blocks })
    }
}

fn expect_byte(cursor: &mut ReadCursor<'_>, expected: u8, field: &'static str) -> Result<(), DecodeError> {
    let got = cursor.read_u8()?;
    if got != expected {
        return Err(DecodeError::InvalidValue {
            field,
            reason: "unexpected fixed byte value",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_client_blocks() -> ClientGccBlocks {
        ClientGccBlocks {
            core: ClientCoreData {
                version: 0x0008_0004,
                desktop_width: 1920,
                desktop_height: 1080,
                color_depth: 0xCA01,
                sas_sequence: 0xAA03,
                keyboard_layout: 0x0409,
                client_build: 2600,
                client_name: "kmsrdp".to_owned(),
                keyboard_type: 4,
                keyboard_subtype: 0,
                keyboard_function_key: 12,
                ime_file_name: String::new(),
            },
            security: ClientSecurityData::default(),
            network: Some(ClientNetworkData {
                channels: vec![ChannelDef {
                    name: "cliprdr".to_owned(),
                    options: 0xC000_0000,
                }],
            }),
            early_capability_flags: None,
            cluster: None,
            message_channel: None,
        }
    }

    #[test]
    fn client_gcc_blocks_round_trip() {
        let blocks = sample_client_blocks();
        let encoded = blocks.encode();
        let decoded = ClientGccBlocks::decode(&encoded).unwrap();
        assert_eq!(decoded, blocks);
    }

    #[test]
    fn client_gcc_blocks_round_trip_without_network() {
        let mut blocks = sample_client_blocks();
        blocks.network = None;
        let encoded = blocks.encode();
        let decoded = ClientGccBlocks::decode(&encoded).unwrap();
        assert_eq!(decoded, blocks);
    }

    #[test]
    fn server_gcc_blocks_round_trip() {
        let blocks = ServerGccBlocks {
            core: ServerCoreData {
                version: 0x0008_0004,
                client_requested_protocols: Some(1),
                early_capability_flags: None,
            },
            network: ServerNetworkData {
                io_channel_id: 1003,
                channel_ids: vec![1004, 1005],
            },
            security: ServerSecurityData,
            message_channel: None,
        };
        let encoded = blocks.encode();
        let decoded = ServerGccBlocks::decode(&encoded).unwrap();
        assert_eq!(decoded, blocks);
    }

    #[test]
    fn server_network_data_odd_channel_count_pads() {
        let data = ServerNetworkData {
            io_channel_id: 1003,
            channel_ids: vec![1004],
        };
        let encoded = data.encode();
        assert_eq!(encoded.len(), 8); // 2+2+2 + 2 bytes padding
        let mut cursor = ReadCursor::new(&encoded);
        assert_eq!(ServerNetworkData::decode(&mut cursor).unwrap(), data);
    }

    #[test]
    fn conference_create_request_round_trip() {
        let blocks = sample_client_blocks().encode();
        let request = ConferenceCreateRequest {
            client_gcc_blocks: blocks,
        };
        let encoded = request.encode();
        let decoded = ConferenceCreateRequest::decode(&encoded).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(ClientGccBlocks::decode(&decoded.client_gcc_blocks).unwrap(), sample_client_blocks());
    }

    #[test]
    fn sc_core_does_not_echo_client_early_capability_flags() {
        let mut body = ClientCoreData {
            version: 0x0008_0004,
            desktop_width: 1920,
            desktop_height: 1080,
            color_depth: 0xCA01,
            sas_sequence: 0xAA03,
            keyboard_layout: 0x0409,
            client_build: 2600,
            client_name: "mstsc".to_owned(),
            keyboard_type: 4,
            keyboard_subtype: 0,
            keyboard_function_key: 12,
            ime_file_name: String::new(),
        }
        .encode();
        body.extend_from_slice(&[0u8; 12]); // up to earlyCapabilityFlags
        body.extend_from_slice(&CS_SUPPORT_ERRINFO_PDU.to_le_bytes());

        let mut blocks = Vec::new();
        write_block(&mut blocks, CS_CORE, &body);
        write_block(&mut blocks, CS_SECURITY, &ClientSecurityData::default().encode());
        let client = ClientGccBlocks::decode(&blocks).unwrap();
        assert_eq!(client.early_capability_flags, Some(CS_SUPPORT_ERRINFO_PDU));

        let server = ServerGccBlocks {
            core: ServerCoreData {
                version: 0x0008_0004,
                client_requested_protocols: Some(1),
                early_capability_flags: None,
            },
            network: ServerNetworkData {
                io_channel_id: 1003,
                channel_ids: vec![1004],
            },
            security: ServerSecurityData,
            message_channel: None,
        };
        let encoded = server.encode();
        let decoded = ServerGccBlocks::decode(&encoded).unwrap();
        assert_eq!(decoded.core.early_capability_flags, None);
    }

    #[test]
    fn early_capability_flags_parsed_from_extended_cs_core() {
        let mut body = ClientCoreData {
            version: 0x0008_0004,
            desktop_width: 1920,
            desktop_height: 1080,
            color_depth: 0xCA01,
            sas_sequence: 0xAA03,
            keyboard_layout: 0x0409,
            client_build: 2600,
            client_name: "mstsc".to_owned(),
            keyboard_type: 4,
            keyboard_subtype: 0,
            keyboard_function_key: 12,
            ime_file_name: String::new(),
        }
        .encode();
        body.extend_from_slice(&[0u8; 12]); // up to earlyCapabilityFlags
        body.extend_from_slice(&(CS_SUPPORT_ERRINFO_PDU | CS_WANT_32BPP_SESSION).to_le_bytes());

        let mut blocks = Vec::new();
        write_block(&mut blocks, CS_CORE, &body);
        write_block(&mut blocks, CS_SECURITY, &ClientSecurityData::default().encode());
        let decoded = ClientGccBlocks::decode(&blocks).unwrap();
        assert_eq!(decoded.early_capability_flags, Some(CS_SUPPORT_ERRINFO_PDU | CS_WANT_32BPP_SESSION));
    }

    #[test]
    fn cs_cluster_is_not_mistaken_for_message_channel() {
        let mut wire = sample_client_blocks().encode();
        write_block(&mut wire, CS_CLUSTER, &[0x0D, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        let decoded = ClientGccBlocks::decode(&wire).unwrap();
        assert!(decoded.cluster.is_some());
        assert_eq!(decoded.message_channel, None);
    }

    #[test]
    fn message_channel_gcc_blocks_round_trip() {
        let mut wire = sample_client_blocks().encode();
        write_block(&mut wire, CS_MCS_MSGCHANNEL, &0xC000_0000u32.to_le_bytes());
        let decoded = ClientGccBlocks::decode(&wire).unwrap();
        assert_eq!(decoded.message_channel, Some(ClientMessageChannelData { flags: 0xC000_0000 }));

        let server = ServerGccBlocks {
            core: ServerCoreData {
                version: 0x0008_0004,
                client_requested_protocols: Some(1),
                early_capability_flags: Some(0x0001),
            },
            network: ServerNetworkData {
                io_channel_id: 1003,
                channel_ids: vec![1004, 1005, 1006, 1007],
            },
            security: ServerSecurityData,
            message_channel: Some(ServerMessageChannelData { mcs_channel_id: 1008 }),
        };
        let encoded = server.encode();
        let decoded = ServerGccBlocks::decode(&encoded).unwrap();
        assert_eq!(decoded, server);
    }

    #[test]
    fn conference_create_response_round_trip() {
        let blocks = ServerGccBlocks {
            core: ServerCoreData {
                version: 0x0008_0004,
                client_requested_protocols: Some(1),
                early_capability_flags: None,
            },
            network: ServerNetworkData {
                io_channel_id: 1003,
                channel_ids: vec![1004],
            },
            security: ServerSecurityData,
            message_channel: None,
        }
        .encode();
        let response = ConferenceCreateResponse {
            node_id: 1002,
            server_gcc_blocks: blocks,
        };
        let encoded = response.encode();
        let decoded = ConferenceCreateResponse::decode(&encoded).unwrap();
        assert_eq!(decoded, response);
    }
}
