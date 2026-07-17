//! Device I/O Request/Completion (MS-RDPEFS 2.2.1.4/2.2.1.5): the
//! server-issues/client-completes IRP mechanism used to actually operate
//! on a previously-announced filesystem device.
//!
//! Scope matches FreeRDP's own rdpdr server implementation exactly:
//! CREATE, CLOSE, READ, WRITE, and DIRECTORY_CONTROL/QUERY_DIRECTORY.
//! FreeRDP's own server doesn't implement QUERY_INFORMATION/
//! QUERY_VOLUME_INFORMATION either (dead stub code only, confirmed by
//! reading its source) - rather than invent those wire layouts without a
//! tested reference to check against, this crate defers them; a
//! `FILE_DIRECTORY_INFORMATION` entry from `QUERY_DIRECTORY` already
//! carries every field a `stat()`-like consumer needs per file
//! (timestamps, size, attributes), which covers the common case.

use rdpcore_pdu::cursor::{ReadCursor, WriteBuf};
use rdpcore_pdu::utf16;
use rdpcore_pdu::DecodeError;

use crate::pdu;

pub const IRP_MJ_CREATE: u32 = 0x0000_0000;
pub const IRP_MJ_CLOSE: u32 = 0x0000_0002;
pub const IRP_MJ_READ: u32 = 0x0000_0003;
pub const IRP_MJ_WRITE: u32 = 0x0000_0004;
pub const IRP_MJ_DIRECTORY_CONTROL: u32 = 0x0000_000C;
pub const IRP_MN_QUERY_DIRECTORY: u32 = 0x0000_0001;

pub const FILE_SUPERSEDE: u32 = 0x0000_0000;
pub const FILE_OPEN: u32 = 0x0000_0001;
pub const FILE_CREATE: u32 = 0x0000_0002;
pub const FILE_OPEN_IF: u32 = 0x0000_0003;
pub const FILE_OVERWRITE: u32 = 0x0000_0004;
pub const FILE_OVERWRITE_IF: u32 = 0x0000_0005;

pub const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
pub const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;

pub const GENERIC_READ: u32 = 0x8000_0000;
pub const GENERIC_WRITE: u32 = 0x4000_0000;
pub const FILE_LIST_DIRECTORY: u32 = 0x0000_0001;
pub const SYNCHRONIZE: u32 = 0x0010_0000;

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;

pub const FILE_SUPERSEDED: u8 = 0;
pub const FILE_OPENED: u8 = 1;
pub const FILE_CREATED: u8 = 2;
pub const FILE_OVERWRITTEN: u8 = 3;
pub const FILE_EXISTS: u8 = 4;
pub const FILE_DOES_NOT_EXIST: u8 = 5;

/// MS-FSCC 2.4 `FileDirectoryInformation` class value - the only
/// `FsInformationClass` this crate (and FreeRDP's own server) issues for
/// `QUERY_DIRECTORY`.
const FILE_DIRECTORY_INFORMATION_CLASS: u32 = 1;

/// NTSTATUS "no more files in this enumeration" - the expected terminal
/// status for a `QUERY_DIRECTORY` loop.
pub const STATUS_NO_MORE_FILES: u32 = 0x8000_0006;

fn write_io_request_header(out: &mut Vec<u8>, device_id: u32, file_id: u32, completion_id: u32, major_function: u32, minor_function: u32) {
    out.write_u16_le(pdu::RDPDR_CTYP_CORE);
    out.write_u16_le(pdu::PAKID_CORE_DEVICE_IOREQUEST);
    out.write_u32_le(device_id);
    out.write_u32_le(file_id);
    out.write_u32_le(completion_id);
    out.write_u32_le(major_function);
    out.write_u32_le(minor_function);
}

fn zero_padding(out: &mut Vec<u8>, n: usize) {
    out.resize(out.len() + n, 0u8);
}

pub fn encode_create_request(device_id: u32, completion_id: u32, path: &str, desired_access: u32, create_disposition: u32, create_options: u32) -> Vec<u8> {
    let mut path_bytes = utf16::encode_units(path);
    path_bytes.write_u16_le(0); // NUL terminator

    let mut out = Vec::with_capacity(4 + 20 + 32 + path_bytes.len());
    write_io_request_header(&mut out, device_id, 0, completion_id, IRP_MJ_CREATE, 0);
    out.write_u32_le(desired_access);
    out.write_u32_le(0); // AllocationSize (low)
    out.write_u32_le(0); // AllocationSize (high) - server always sends 0
    out.write_u32_le(0); // FileAttributes
    out.write_u32_le(3); // SharedAccess - matches the reference server (read+write+delete share)
    out.write_u32_le(create_disposition);
    out.write_u32_le(create_options);
    out.write_u32_le(path_bytes.len() as u32);
    out.write_slice(&path_bytes);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateReply {
    pub file_id: u32,
    pub information: u8,
}

pub fn decode_create_reply(body: &[u8]) -> Result<CreateReply, DecodeError> {
    let mut cursor = ReadCursor::new(body);
    let file_id = cursor.read_u32_le()?;
    let information = cursor.read_u8()?;
    Ok(CreateReply { file_id, information })
}

/// 32 bytes of trailing padding (confirmed against both the reference
/// server's send side and its defensive receive-side stub).
pub fn encode_close_request(device_id: u32, file_id: u32, completion_id: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 20 + 32);
    write_io_request_header(&mut out, device_id, file_id, completion_id, IRP_MJ_CLOSE, 0);
    zero_padding(&mut out, 32);
    out
}
// CLOSE completion carries no fields beyond the shared DeviceId/CompletionId/
// IoStatus header - any trailing padding bytes are simply ignored by the
// caller, sidestepping a genuine ambiguity in the reference source (its own
// receive-side stub reads 5 padding bytes, not the 4 a spec skim would
// suggest).

/// `offset`'s high dword is always sent as 0 - the reference server's own
/// request-sending API only supports 32-bit offsets.
pub fn encode_read_request(device_id: u32, file_id: u32, completion_id: u32, length: u32, offset: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 20 + 32);
    write_io_request_header(&mut out, device_id, file_id, completion_id, IRP_MJ_READ, 0);
    out.write_u32_le(length);
    out.write_u64_le(offset);
    zero_padding(&mut out, 20);
    out
}

pub fn decode_read_reply(body: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let mut cursor = ReadCursor::new(body);
    let length = cursor.read_u32_le()? as usize;
    Ok(cursor.read_slice(length)?.to_vec())
}

pub fn encode_write_request(device_id: u32, file_id: u32, completion_id: u32, offset: u64, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 20 + 32 + data.len());
    write_io_request_header(&mut out, device_id, file_id, completion_id, IRP_MJ_WRITE, 0);
    out.write_u32_le(data.len() as u32);
    out.write_u64_le(offset);
    zero_padding(&mut out, 20);
    out.write_slice(data);
    out
}

/// Bytes actually written; trailing 1-byte padding ignored.
pub fn decode_write_reply(body: &[u8]) -> Result<u32, DecodeError> {
    let mut cursor = ReadCursor::new(body);
    Ok(cursor.read_u32_le()?)
}

/// `path`: `Some(pattern)` (e.g. `"\\*"`) starts a fresh enumeration
/// (`InitialQuery = 1`); `None` continues a prior one, asking for the next
/// entry (`InitialQuery = 0`) - matches the reference server's own
/// convention of re-issuing one request per directory entry rather than
/// consuming a `NextEntryOffset`-linked batch from a single reply.
pub fn encode_query_directory_request(device_id: u32, file_id: u32, completion_id: u32, path: Option<&str>) -> Vec<u8> {
    let path_bytes = match path {
        Some(p) => {
            let mut b = utf16::encode_units(p);
            b.write_u16_le(0);
            b
        }
        None => Vec::new(),
    };
    let mut out = Vec::with_capacity(4 + 20 + 32 + path_bytes.len());
    write_io_request_header(&mut out, device_id, file_id, completion_id, IRP_MJ_DIRECTORY_CONTROL, IRP_MN_QUERY_DIRECTORY);
    out.write_u32_le(FILE_DIRECTORY_INFORMATION_CLASS);
    out.write_u8(u8::from(path.is_some()));
    out.write_u32_le(path_bytes.len() as u32);
    zero_padding(&mut out, 23);
    out.write_slice(&path_bytes);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub file_index: u32,
    pub creation_time: i64,
    pub last_access_time: i64,
    pub last_write_time: i64,
    pub change_time: i64,
    pub end_of_file: i64,
    pub allocation_size: i64,
    pub file_attributes: u32,
    pub file_name: String,
}

/// `None` when the response body is empty - the reference server issues
/// one request per directory entry rather than batching, so an empty
/// reply body (paired with a non-`STATUS_SUCCESS` `IoStatus`, typically
/// `STATUS_NO_MORE_FILES`) signals end-of-listing.
pub fn decode_query_directory_reply(body: &[u8]) -> Result<Option<DirectoryEntry>, DecodeError> {
    if body.is_empty() {
        return Ok(None);
    }
    let mut cursor = ReadCursor::new(body);
    let length = cursor.read_u32_le()? as usize;
    if length == 0 {
        return Ok(None);
    }
    let entry_bytes = cursor.read_slice(length)?;
    let mut entry = ReadCursor::new(entry_bytes);
    let _next_entry_offset = entry.read_u32_le()?;
    let file_index = entry.read_u32_le()?;
    let creation_time = entry.read_u64_le()? as i64;
    let last_access_time = entry.read_u64_le()? as i64;
    let last_write_time = entry.read_u64_le()? as i64;
    let change_time = entry.read_u64_le()? as i64;
    let end_of_file = entry.read_u64_le()? as i64;
    let allocation_size = entry.read_u64_le()? as i64;
    let file_attributes = entry.read_u32_le()?;
    let file_name_length = entry.read_u32_le()? as usize;
    let file_name = utf16::decode_units(entry.read_slice(file_name_length)?);
    Ok(Some(DirectoryEntry {
        file_index,
        creation_time,
        last_access_time,
        last_write_time,
        change_time,
        end_of_file,
        allocation_size,
        file_attributes,
        file_name,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_directory_entry_body(file_name: &str, end_of_file: i64, attributes: u32) -> Vec<u8> {
        let mut name_bytes = utf16::encode_units(file_name);
        let mut entry = Vec::new();
        entry.write_u32_le(0); // NextEntryOffset
        entry.write_u32_le(1); // FileIndex
        entry.write_u64_le(0); // CreationTime
        entry.write_u64_le(0); // LastAccessTime
        entry.write_u64_le(0); // LastWriteTime
        entry.write_u64_le(0); // ChangeTime
        entry.write_u64_le(end_of_file as u64);
        entry.write_u64_le(0); // AllocationSize
        entry.write_u32_le(attributes);
        entry.write_u32_le(name_bytes.len() as u32);
        entry.append(&mut name_bytes);

        let mut body = Vec::new();
        body.write_u32_le(entry.len() as u32);
        body.append(&mut entry);
        body
    }

    #[test]
    fn create_request_round_trip_layout() {
        let encoded = encode_create_request(1, 5, "\\foo.txt", GENERIC_READ, FILE_OPEN, 0);
        assert_eq!(&encoded[0..2], &pdu::RDPDR_CTYP_CORE.to_le_bytes());
        assert_eq!(&encoded[2..4], &pdu::PAKID_CORE_DEVICE_IOREQUEST.to_le_bytes());
        assert_eq!(&encoded[4..8], &1u32.to_le_bytes()); // DeviceId
        assert_eq!(&encoded[8..12], &0u32.to_le_bytes()); // FileId (unknown yet)
        assert_eq!(&encoded[12..16], &5u32.to_le_bytes()); // CompletionId
        assert_eq!(&encoded[16..20], &IRP_MJ_CREATE.to_le_bytes());
        // fixed body starts at offset 24: DesiredAccess..PathLength = 32 bytes
        assert_eq!(&encoded[24..28], &GENERIC_READ.to_le_bytes());
        let path_length = u32::from_le_bytes(encoded[24 + 28..24 + 32].try_into().unwrap());
        assert_eq!(path_length as usize, encoded.len() - (24 + 32));
    }

    #[test]
    fn create_reply_decodes_file_id_and_information() {
        let mut body = Vec::new();
        body.write_u32_le(42);
        body.write_u8(FILE_OPENED);
        let decoded = decode_create_reply(&body).unwrap();
        assert_eq!(decoded, CreateReply { file_id: 42, information: FILE_OPENED });
    }

    #[test]
    fn close_request_has_32_bytes_padding() {
        let encoded = encode_close_request(1, 42, 6);
        assert_eq!(encoded.len(), 4 + 20 + 32);
    }

    #[test]
    fn read_request_and_reply_round_trip() {
        let encoded = encode_read_request(1, 42, 7, 1024, 4096);
        assert_eq!(encoded.len(), 4 + 20 + 32);
        assert_eq!(&encoded[24..28], &1024u32.to_le_bytes());
        assert_eq!(&encoded[28..36], &4096u64.to_le_bytes());

        let mut reply_body = Vec::new();
        reply_body.write_u32_le(3);
        reply_body.write_slice(b"abc");
        assert_eq!(decode_read_reply(&reply_body).unwrap(), b"abc".to_vec());
    }

    #[test]
    fn write_request_and_reply_round_trip() {
        let encoded = encode_write_request(1, 42, 8, 0, b"hello");
        assert_eq!(&encoded[24..28], &5u32.to_le_bytes()); // Length
        assert_eq!(&encoded[encoded.len() - 5..], b"hello");

        let mut reply_body = Vec::new();
        reply_body.write_u32_le(5);
        reply_body.write_u8(0); // padding
        assert_eq!(decode_write_reply(&reply_body).unwrap(), 5);
    }

    #[test]
    fn query_directory_initial_query_sets_flag_and_carries_pattern() {
        let encoded = encode_query_directory_request(1, 42, 9, Some("\\*"));
        // fixed(32) starts right after the 24-byte io-request header
        let initial_query = encoded[24 + 4];
        assert_eq!(initial_query, 1);
        let path_length = u32::from_le_bytes(encoded[24 + 5..24 + 9].try_into().unwrap());
        assert_eq!(path_length as usize, encoded.len() - (24 + 32));
    }

    #[test]
    fn query_directory_continue_has_no_path_and_clears_initial_query() {
        let encoded = encode_query_directory_request(1, 42, 10, None);
        assert_eq!(encoded.len(), 4 + 20 + 32);
        assert_eq!(encoded[24 + 4], 0); // InitialQuery
    }

    #[test]
    fn query_directory_reply_decodes_one_entry() {
        let body = encode_directory_entry_body("file.txt", 123, FILE_ATTRIBUTE_NORMAL);
        let decoded = decode_query_directory_reply(&body).unwrap().unwrap();
        assert_eq!(decoded.file_name, "file.txt");
        assert_eq!(decoded.end_of_file, 123);
        assert_eq!(decoded.file_attributes, FILE_ATTRIBUTE_NORMAL);
    }

    #[test]
    fn query_directory_reply_empty_body_means_no_more_files() {
        assert_eq!(decode_query_directory_reply(&[]).unwrap(), None);
    }
}
