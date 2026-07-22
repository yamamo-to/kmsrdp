//! Device I/O Request/Completion (MS-RDPEFS 2.2.1.4/2.2.1.5): the
//! server-issues/client-completes IRP mechanism used to actually operate
//! on a previously-announced filesystem device.
//!
//! Scope matches FreeRDP's own rdpdr server for the common drive ops:
//! CREATE, CLOSE, READ, WRITE, DIRECTORY_CONTROL/QUERY_DIRECTORY, and
//! SET_INFORMATION (rename / end-of-file / basic times / disposition). Deletes
//! use CREATE with `DELETE` (+ `FILE_DELETE_ON_CLOSE`) and
//! `FileDispositionInformation`, then CLOSE.

use rdpcore_pdu::DecodeError;
use rdpcore_pdu::cursor::{ReadCursor, WriteBuf};
use rdpcore_pdu::utf16;

use crate::pdu;

pub const IRP_MJ_CREATE: u32 = 0x0000_0000;
pub const IRP_MJ_CLOSE: u32 = 0x0000_0002;
pub const IRP_MJ_READ: u32 = 0x0000_0003;
pub const IRP_MJ_WRITE: u32 = 0x0000_0004;
pub const IRP_MJ_SET_INFORMATION: u32 = 0x0000_0006;
pub const IRP_MJ_DIRECTORY_CONTROL: u32 = 0x0000_000C;
pub const IRP_MN_QUERY_DIRECTORY: u32 = 0x0000_0001;

pub const FILE_SUPERSEDE: u32 = 0x0000_0000;
pub const FILE_OPEN: u32 = 0x0000_0001;
pub const FILE_CREATE: u32 = 0x0000_0002;
pub const FILE_OPEN_IF: u32 = 0x0000_0003;
pub const FILE_OVERWRITE: u32 = 0x0000_0004;
pub const FILE_OVERWRITE_IF: u32 = 0x0000_0005;

pub const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
pub const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
pub const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
pub const FILE_DELETE_ON_CLOSE: u32 = 0x0000_1000;

pub const GENERIC_READ: u32 = 0x8000_0000;
pub const GENERIC_WRITE: u32 = 0x4000_0000;
pub const FILE_LIST_DIRECTORY: u32 = 0x0000_0001;
pub const FILE_READ_DATA: u32 = 0x0000_0001;
pub const FILE_WRITE_DATA: u32 = 0x0000_0002;
pub const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;
pub const DELETE: u32 = 0x0001_0000;
pub const SYNCHRONIZE: u32 = 0x0010_0000;

pub const FILE_SHARE_READ: u32 = 0x0000_0001;
pub const FILE_SHARE_WRITE: u32 = 0x0000_0002;
pub const FILE_SHARE_DELETE: u32 = 0x0000_0004;
/// FreeRDP server default: read+write share (no delete share).
pub const FILE_SHARE_READ_WRITE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;
/// Prefer when opening for delete so concurrent readers do not block us.
pub const FILE_SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;

pub const FILE_SUPERSEDED: u8 = 0;
pub const FILE_OPENED: u8 = 1;
pub const FILE_CREATED: u8 = 2;
pub const FILE_OVERWRITTEN: u8 = 3;
pub const FILE_EXISTS: u8 = 4;
pub const FILE_DOES_NOT_EXIST: u8 = 5;

/// MS-FSCC / MS-RDPEFS `FsInformationClass` values used with
/// [`IRP_MJ_SET_INFORMATION`].
pub const FILE_BASIC_INFORMATION: u32 = 0x0000_0004;
pub const FILE_RENAME_INFORMATION: u32 = 0x0000_000A;
pub const FILE_DISPOSITION_INFORMATION: u32 = 0x0000_000D;
pub const FILE_ALLOCATION_INFORMATION: u32 = 0x0000_0013;
pub const FILE_END_OF_FILE_INFORMATION: u32 = 0x0000_0014;

/// MS-FSCC 2.4 `FileDirectoryInformation` class value - the only
/// `FsInformationClass` this crate (and FreeRDP's own server) issues for
/// `QUERY_DIRECTORY`.
const FILE_DIRECTORY_INFORMATION_CLASS: u32 = 1;

/// NTSTATUS "no more files in this enumeration" - the expected terminal
/// status for a `QUERY_DIRECTORY` loop.
pub const STATUS_NO_MORE_FILES: u32 = 0x8000_0006;

fn write_io_request_header(
    out: &mut Vec<u8>,
    device_id: u32,
    file_id: u32,
    completion_id: u32,
    major_function: u32,
    minor_function: u32,
) {
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

pub fn encode_create_request(
    device_id: u32,
    completion_id: u32,
    path: &str,
    desired_access: u32,
    create_disposition: u32,
    create_options: u32,
) -> Vec<u8> {
    let mut path_bytes = utf16::encode_units(path);
    path_bytes.write_u16_le(0); // NUL terminator

    let mut out = Vec::with_capacity(4 + 20 + 32 + path_bytes.len());
    write_io_request_header(&mut out, device_id, 0, completion_id, IRP_MJ_CREATE, 0);
    out.write_u32_le(desired_access);
    out.write_u32_le(0); // AllocationSize (low)
    out.write_u32_le(0); // AllocationSize (high) - server always sends 0
    out.write_u32_le(0); // FileAttributes
    // FreeRDP uses read+write (3); include DELETE share so an open-for-delete
    // is not blocked by concurrent readers (common during FUSE rm).
    out.write_u32_le(FILE_SHARE_ALL);
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
    Ok(CreateReply {
        file_id,
        information,
    })
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
pub fn encode_read_request(
    device_id: u32,
    file_id: u32,
    completion_id: u32,
    length: u32,
    offset: u64,
) -> Vec<u8> {
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

pub fn encode_write_request(
    device_id: u32,
    file_id: u32,
    completion_id: u32,
    offset: u64,
    data: &[u8],
) -> Vec<u8> {
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
pub fn encode_query_directory_request(
    device_id: u32,
    file_id: u32,
    completion_id: u32,
    path: Option<&str>,
) -> Vec<u8> {
    let path_bytes = match path {
        Some(p) => {
            let mut b = utf16::encode_units(p);
            b.write_u16_le(0);
            b
        }
        None => Vec::new(),
    };
    let mut out = Vec::with_capacity(4 + 20 + 32 + path_bytes.len());
    write_io_request_header(
        &mut out,
        device_id,
        file_id,
        completion_id,
        IRP_MJ_DIRECTORY_CONTROL,
        IRP_MN_QUERY_DIRECTORY,
    );
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

/// DR_DRIVE_SET_INFORMATION_REQ (MS-RDPEFS 2.2.3.3.9) with an arbitrary
/// `FsInformationClass` and `SetBuffer`.
pub fn encode_set_information_request(
    device_id: u32,
    file_id: u32,
    completion_id: u32,
    fs_information_class: u32,
    set_buffer: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 20 + 32 + set_buffer.len());
    write_io_request_header(
        &mut out,
        device_id,
        file_id,
        completion_id,
        IRP_MJ_SET_INFORMATION,
        0,
    );
    out.write_u32_le(fs_information_class);
    out.write_u32_le(set_buffer.len() as u32);
    zero_padding(&mut out, 24);
    out.write_slice(set_buffer);
    out
}

/// `FileDispositionInformation` SetBuffer (`DeleteFile` boolean).
pub fn disposition_information_buffer(delete_file: bool) -> Vec<u8> {
    vec![u8::from(delete_file)]
}

/// Mark for delete via [`FILE_DISPOSITION_INFORMATION`].
pub fn encode_set_disposition_request(
    device_id: u32,
    file_id: u32,
    completion_id: u32,
    delete_file: bool,
) -> Vec<u8> {
    encode_set_information_request(
        device_id,
        file_id,
        completion_id,
        FILE_DISPOSITION_INFORMATION,
        &disposition_information_buffer(delete_file),
    )
}

/// `FileRenameInformation` / `RDP_FILE_RENAME_INFORMATION` SetBuffer
/// (FreeRDP `rdpdr_server_send_device_file_rename_request` layout).
/// `new_path` is a full Windows path such as `\\dir\\new.txt`.
pub fn rename_information_buffer(new_path: &str, replace_if_exists: bool) -> Vec<u8> {
    let mut name_bytes = utf16::encode_units(new_path);
    name_bytes.write_u16_le(0); // NUL
    let mut set_buffer = Vec::with_capacity(6 + name_bytes.len());
    set_buffer.write_u8(u8::from(replace_if_exists));
    set_buffer.write_u8(0); // RootDirectory (always 0 for RDP)
    set_buffer.write_u32_le(name_bytes.len() as u32);
    set_buffer.write_slice(&name_bytes);
    set_buffer
}

/// Rename via [`FILE_RENAME_INFORMATION`].
pub fn encode_set_rename_request(
    device_id: u32,
    file_id: u32,
    completion_id: u32,
    new_path: &str,
    replace_if_exists: bool,
) -> Vec<u8> {
    encode_set_information_request(
        device_id,
        file_id,
        completion_id,
        FILE_RENAME_INFORMATION,
        &rename_information_buffer(new_path, replace_if_exists),
    )
}

/// `FileEndOfFileInformation` SetBuffer (8-byte EndOfFile).
pub fn end_of_file_information_buffer(end_of_file: i64) -> Vec<u8> {
    let mut set_buffer = Vec::with_capacity(8);
    set_buffer.write_u64_le(end_of_file as u64);
    set_buffer
}

/// Truncate/extend via [`FILE_END_OF_FILE_INFORMATION`].
pub fn encode_set_end_of_file_request(
    device_id: u32,
    file_id: u32,
    completion_id: u32,
    end_of_file: i64,
) -> Vec<u8> {
    encode_set_information_request(
        device_id,
        file_id,
        completion_id,
        FILE_END_OF_FILE_INFORMATION,
        &end_of_file_information_buffer(end_of_file),
    )
}

/// `FileBasicInformation` SetBuffer. A time or attribute value of `0`
/// means "do not change" (MS-FSCC). The 4-byte reserved field is omitted
/// on the wire (FreeRDP / IronRDP convention).
pub fn basic_information_buffer(
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    file_attributes: u32,
) -> Vec<u8> {
    let mut set_buffer = Vec::with_capacity(36);
    set_buffer.write_u64_le(creation_time as u64);
    set_buffer.write_u64_le(last_access_time as u64);
    set_buffer.write_u64_le(last_write_time as u64);
    set_buffer.write_u64_le(change_time as u64);
    set_buffer.write_u32_le(file_attributes);
    set_buffer
}

#[allow(clippy::too_many_arguments)] // mirrors MS-FSCC FileBasicInformation fields
/// Timestamps / attributes via [`FILE_BASIC_INFORMATION`].
pub fn encode_set_basic_information_request(
    device_id: u32,
    file_id: u32,
    completion_id: u32,
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    file_attributes: u32,
) -> Vec<u8> {
    encode_set_information_request(
        device_id,
        file_id,
        completion_id,
        FILE_BASIC_INFORMATION,
        &basic_information_buffer(
            creation_time,
            last_access_time,
            last_write_time,
            change_time,
            file_attributes,
        ),
    )
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
        assert_eq!(
            &encoded[2..4],
            &pdu::PAKID_CORE_DEVICE_IOREQUEST.to_le_bytes()
        );
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
        assert_eq!(
            decoded,
            CreateReply {
                file_id: 42,
                information: FILE_OPENED
            }
        );
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

    #[test]
    fn set_disposition_request_is_one_byte_true() {
        let encoded = encode_set_disposition_request(1, 2, 3, true);
        assert_eq!(
            &encoded[24..28],
            &FILE_DISPOSITION_INFORMATION.to_le_bytes()
        );
        assert_eq!(u32::from_le_bytes(encoded[28..32].try_into().unwrap()), 1);
        assert_eq!(encoded[32 + 24], 1);
    }

    #[test]
    fn set_rename_request_layout_matches_freerdp() {
        let encoded = encode_set_rename_request(1, 7, 9, "\\dir\\new.txt", false);
        assert_eq!(&encoded[16..20], &IRP_MJ_SET_INFORMATION.to_le_bytes());
        assert_eq!(&encoded[24..28], &FILE_RENAME_INFORMATION.to_le_bytes());
        let length = u32::from_le_bytes(encoded[28..32].try_into().unwrap()) as usize;
        let set_buffer = &encoded[32 + 24..];
        assert_eq!(set_buffer.len(), length);
        assert_eq!(set_buffer[0], 0); // ReplaceIfExists
        assert_eq!(set_buffer[1], 0); // RootDirectory
        let name_len = u32::from_le_bytes(set_buffer[2..6].try_into().unwrap()) as usize;
        assert_eq!(name_len + 6, length);
        assert_eq!(&set_buffer[6..], &{
            let mut n = utf16::encode_units("\\dir\\new.txt");
            n.write_u16_le(0);
            n
        });
    }

    #[test]
    fn set_end_of_file_request_carries_eight_byte_size() {
        let encoded = encode_set_end_of_file_request(1, 2, 3, 1234);
        assert_eq!(
            &encoded[24..28],
            &FILE_END_OF_FILE_INFORMATION.to_le_bytes()
        );
        assert_eq!(u32::from_le_bytes(encoded[28..32].try_into().unwrap()), 8);
        assert_eq!(&encoded[56..64], &1234u64.to_le_bytes());
    }

    #[test]
    fn set_basic_information_is_36_bytes_without_reserved() {
        let encoded =
            encode_set_basic_information_request(1, 2, 3, 10, 20, 30, 40, FILE_ATTRIBUTE_NORMAL);
        assert_eq!(&encoded[24..28], &FILE_BASIC_INFORMATION.to_le_bytes());
        assert_eq!(u32::from_le_bytes(encoded[28..32].try_into().unwrap()), 36);
        assert_eq!(encoded.len(), 32 + 24 + 36);
    }
}
