//! MS-RDPEFS (RDPDR) device redirection: filesystem ("drive") and printer
//! devices - both are just IRP-addressable devices (open/read-or-write/
//! close) from the wire's perspective, so they share the exact same
//! [`DriveCommand`]/[`DriveConsumer`] machinery (a print job is simply a
//! device a consumer only ever `Create`s, `Write`s to, and `Close`s -
//! never `Read`s or `QueryDirectory`s). The name predates printer support
//! and stuck for lack of a better one; it isn't filesystem-only anymore.
//!
//! Direction matters here more than in any other channel this stack
//! implements: RDPDR redirects the *client's* local devices into the
//! *server's* session, so this server issues Device I/O Requests and the
//! connected client (the device's real owner) completes them - the
//! opposite of rdpsnd/cliprdr, where the server mostly reacts to what the
//! client sends. [`DriveConsumer`] is therefore command-driven rather than
//! event-driven: every callback returns the next [`DriveCommand`]s to
//! issue, letting a consumer (e.g. a directory-listing walk, or eventually
//! a FUSE filesystem / CUPS backend) drive a whole operation one reply at
//! a time without re-entrant calls back into [`RdpdrChannel`].

pub mod diagnostic;
pub mod irp;
pub mod pdu;

use std::collections::HashMap;

use rdpcore_pdu::mcs::SendData;
use rdpcore_pdu::{DecodeError, svc, x224};

/// Arbitrary, fixed - the client only ever echoes this back
/// (`Client Announce Reply`/`Client ID Confirm`), it carries no other
/// meaning.
const CLIENT_ID: u32 = 0x0001;

#[derive(Debug, Clone)]
pub enum DriveCommand {
    Create {
        device_id: u32,
        path: String,
        desired_access: u32,
        create_disposition: u32,
        create_options: u32,
        request_tag: u64,
    },
    Close {
        device_id: u32,
        file_id: u32,
        request_tag: u64,
    },
    Read {
        device_id: u32,
        file_id: u32,
        length: u32,
        offset: u64,
        request_tag: u64,
    },
    Write {
        device_id: u32,
        file_id: u32,
        offset: u64,
        data: Vec<u8>,
        request_tag: u64,
    },
    /// `path: Some(pattern)` starts a fresh directory enumeration;
    /// `None` asks for the next entry of one already in progress.
    QueryDirectory {
        device_id: u32,
        file_id: u32,
        path: Option<String>,
        request_tag: u64,
    },
}

/// `request_tag` is an opaque correlator the consumer chose when issuing
/// the original [`DriveCommand`] - distinct from the wire-level
/// `CompletionId`, which [`RdpdrChannel`] manages internally.
pub trait DriveConsumer: Send {
    /// A device (filesystem or printer, per `device_type` - one of the
    /// `pdu::RDPDR_DTYP_*` values) was just announced and acknowledged -
    /// return any commands to kick off against it (e.g. open the root
    /// directory, or nothing yet for a printer until something wants to
    /// print).
    fn on_device_ready(
        &mut self,
        device_id: u32,
        device_type: u32,
        dos_name: &str,
    ) -> Vec<DriveCommand>;
    fn on_create_reply(
        &mut self,
        request_tag: u64,
        result: Result<irp::CreateReply, u32>,
    ) -> Vec<DriveCommand>;
    fn on_close_reply(&mut self, request_tag: u64, status: u32) -> Vec<DriveCommand>;
    fn on_read_reply(
        &mut self,
        request_tag: u64,
        result: Result<Vec<u8>, u32>,
    ) -> Vec<DriveCommand>;
    fn on_write_reply(&mut self, request_tag: u64, result: Result<u32, u32>) -> Vec<DriveCommand>;
    /// `Ok(None)` means the enumeration this `request_tag` belonged to has
    /// finished (`STATUS_NO_MORE_FILES` or an empty reply body).
    fn on_query_directory_reply(
        &mut self,
        request_tag: u64,
        result: Result<Option<irp::DirectoryEntry>, u32>,
    ) -> Vec<DriveCommand>;
}

/// One [`DriveConsumer`] per connection, mirroring `SoundServerFactory`/
/// `CliprdrBackendFactory` - a fresh consumer (fresh open-file state) is
/// needed per connection, not a shared singleton.
pub trait DriveConsumerFactory: Send + Sync {
    /// OR of `pdu::RDPDR_DTYP_*` - which device types to accept and
    /// advertise capability for on every connection this factory serves.
    fn supported_device_types(&self) -> u32;
    fn build_drive_consumer(&self) -> Box<dyn DriveConsumer>;
}

struct PendingOp {
    major_function: u32,
    request_tag: u64,
}

pub struct RdpdrChannel {
    channel_id: u16,
    user_channel_id: u16,
    /// OR of `pdu::RDPDR_DTYP_*` - which device types this connection
    /// accepts; gates both what's advertised in the Server Capability
    /// Request and what's accepted in Device List Announce.
    supported: u32,
    /// Devices the client has announced and we've accepted, keyed by
    /// `DeviceId` -> `(device_type, PreferredDosName)`.
    devices: HashMap<u32, (u32, String)>,
    next_completion_id: u32,
    pending: HashMap<u32, PendingOp>,
    consumer: Box<dyn DriveConsumer>,
    /// Accumulates SVC chunks across possibly-multiple `on_channel_data`
    /// calls until a `CHANNEL_FLAG_LAST` chunk completes one logical PDU.
    incoming_buffer: Vec<u8>,
}

impl RdpdrChannel {
    /// `supported` is an OR of `pdu::RDPDR_DTYP_*` (e.g. just
    /// `RDPDR_DTYP_FILESYSTEM`, or also `| RDPDR_DTYP_PRINT`). Returns the
    /// channel plus the Server Announce Request the caller should send
    /// immediately - the server always speaks first on this channel.
    pub fn new(
        channel_id: u16,
        user_channel_id: u16,
        supported: u32,
        consumer: Box<dyn DriveConsumer>,
    ) -> (Self, Vec<Vec<u8>>) {
        let initial = wrap_indication(
            user_channel_id,
            channel_id,
            pdu::encode_server_announce_request(CLIENT_ID),
        );
        (
            Self {
                channel_id,
                user_channel_id,
                supported,
                devices: HashMap::new(),
                next_completion_id: 0,
                pending: HashMap::new(),
                consumer,
                incoming_buffer: Vec::new(),
            },
            initial,
        )
    }

    pub fn channel_id(&self) -> u16 {
        self.channel_id
    }

    pub fn devices(&self) -> &HashMap<u32, (u32, String)> {
        &self.devices
    }

    /// `payload` is one SVC chunk (Channel PDU Header included) of
    /// `"rdpdr"`-channel data from an incoming MCS Send Data Request.
    pub fn on_channel_data(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>, DecodeError> {
        let (_total_length, flags, chunk_body) = svc::dechunkify(payload)?;
        if flags & svc::CHANNEL_FLAG_FIRST != 0 {
            self.incoming_buffer.clear();
        }
        self.incoming_buffer.extend_from_slice(chunk_body);
        if flags & svc::CHANNEL_FLAG_LAST == 0 {
            return Ok(Vec::new()); // wait for the rest
        }

        let message = core::mem::take(&mut self.incoming_buffer);
        let mut out = Vec::new();
        match pdu::decode_client_message(&message)? {
            pdu::ClientMessage::AnnounceReply
            | pdu::ClientMessage::UserLoggedOn
            | pdu::ClientMessage::Other => {}
            pdu::ClientMessage::ClientName => {
                out.extend(self.wrap(pdu::encode_server_capability_request(self.supported)));
                out.extend(self.wrap(pdu::encode_client_id_confirm(CLIENT_ID)));
            }
            pdu::ClientMessage::ClientCapability => {
                out.extend(self.wrap(pdu::encode_user_logged_on()));
            }
            pdu::ClientMessage::DeviceListAnnounce(devices) => {
                for device in devices {
                    if self.supported & device.device_type != 0 {
                        self.devices.insert(
                            device.device_id,
                            (device.device_type, device.preferred_dos_name.clone()),
                        );
                        out.extend(self.wrap(pdu::encode_device_reply(
                            device.device_id,
                            pdu::STATUS_SUCCESS,
                        )));
                        let commands = self.consumer.on_device_ready(
                            device.device_id,
                            device.device_type,
                            &device.preferred_dos_name,
                        );
                        self.encode_commands(commands, &mut out);
                    } else {
                        out.extend(self.wrap(pdu::encode_device_reply(
                            device.device_id,
                            pdu::STATUS_UNSUCCESSFUL,
                        )));
                    }
                }
            }
            pdu::ClientMessage::DeviceIoCompletion {
                completion_id,
                io_status,
                body,
                ..
            } => {
                if let Some(pending) = self.pending.remove(&completion_id) {
                    let commands = self.dispatch_completion(pending, io_status, &body)?;
                    self.encode_commands(commands, &mut out);
                }
            }
        }
        Ok(out)
    }

    fn dispatch_completion(
        &mut self,
        pending: PendingOp,
        io_status: u32,
        body: &[u8],
    ) -> Result<Vec<DriveCommand>, DecodeError> {
        let ok = io_status == pdu::STATUS_SUCCESS;
        Ok(match pending.major_function {
            irp::IRP_MJ_CREATE => {
                let result = if ok {
                    Ok(irp::decode_create_reply(body)?)
                } else {
                    Err(io_status)
                };
                self.consumer.on_create_reply(pending.request_tag, result)
            }
            irp::IRP_MJ_CLOSE => self.consumer.on_close_reply(pending.request_tag, io_status),
            irp::IRP_MJ_READ => {
                let result = if ok {
                    Ok(irp::decode_read_reply(body)?)
                } else {
                    Err(io_status)
                };
                self.consumer.on_read_reply(pending.request_tag, result)
            }
            irp::IRP_MJ_WRITE => {
                let result = if ok {
                    Ok(irp::decode_write_reply(body)?)
                } else {
                    Err(io_status)
                };
                self.consumer.on_write_reply(pending.request_tag, result)
            }
            irp::IRP_MJ_DIRECTORY_CONTROL => {
                let result = if ok {
                    Ok(irp::decode_query_directory_reply(body)?)
                } else if io_status == irp::STATUS_NO_MORE_FILES {
                    Ok(None)
                } else {
                    Err(io_status)
                };
                self.consumer
                    .on_query_directory_reply(pending.request_tag, result)
            }
            _ => Vec::new(),
        })
    }

    fn encode_commands(&mut self, commands: Vec<DriveCommand>, out: &mut Vec<Vec<u8>>) {
        for command in commands {
            let completion_id = self.next_completion_id;
            self.next_completion_id = self.next_completion_id.wrapping_add(1);
            let (major_function, request_tag, bytes) = match command {
                DriveCommand::Create {
                    device_id,
                    path,
                    desired_access,
                    create_disposition,
                    create_options,
                    request_tag,
                } => (
                    irp::IRP_MJ_CREATE,
                    request_tag,
                    irp::encode_create_request(
                        device_id,
                        completion_id,
                        &path,
                        desired_access,
                        create_disposition,
                        create_options,
                    ),
                ),
                DriveCommand::Close {
                    device_id,
                    file_id,
                    request_tag,
                } => (
                    irp::IRP_MJ_CLOSE,
                    request_tag,
                    irp::encode_close_request(device_id, file_id, completion_id),
                ),
                DriveCommand::Read {
                    device_id,
                    file_id,
                    length,
                    offset,
                    request_tag,
                } => (
                    irp::IRP_MJ_READ,
                    request_tag,
                    irp::encode_read_request(device_id, file_id, completion_id, length, offset),
                ),
                DriveCommand::Write {
                    device_id,
                    file_id,
                    offset,
                    data,
                    request_tag,
                } => (
                    irp::IRP_MJ_WRITE,
                    request_tag,
                    irp::encode_write_request(device_id, file_id, completion_id, offset, &data),
                ),
                DriveCommand::QueryDirectory {
                    device_id,
                    file_id,
                    path,
                    request_tag,
                } => (
                    irp::IRP_MJ_DIRECTORY_CONTROL,
                    request_tag,
                    irp::encode_query_directory_request(
                        device_id,
                        file_id,
                        completion_id,
                        path.as_deref(),
                    ),
                ),
            };
            self.pending.insert(
                completion_id,
                PendingOp {
                    major_function,
                    request_tag,
                },
            );
            out.extend(self.wrap(bytes));
        }
    }

    fn wrap(&self, data: Vec<u8>) -> Vec<Vec<u8>> {
        wrap_indication(self.user_channel_id, self.channel_id, data)
    }
}

fn wrap_indication(initiator: u16, channel_id: u16, data: Vec<u8>) -> Vec<Vec<u8>> {
    svc::chunkify(&data)
        .into_iter()
        .map(|chunk| {
            x224::wrap_data(
                &SendData {
                    initiator,
                    channel_id,
                    data: chunk,
                    complete: true,
                }
                .encode_indication(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdpcore_pdu::cursor::WriteBuf;

    #[derive(Default)]
    struct RecordingConsumer {
        ready_devices: Vec<(u32, String)>,
        create_replies: Vec<(u64, Result<irp::CreateReply, u32>)>,
        query_directory_replies: Vec<(u64, Result<Option<irp::DirectoryEntry>, u32>)>,
        next_create_on_ready: bool,
    }

    impl DriveConsumer for RecordingConsumer {
        fn on_device_ready(
            &mut self,
            device_id: u32,
            _device_type: u32,
            dos_name: &str,
        ) -> Vec<DriveCommand> {
            self.ready_devices.push((device_id, dos_name.to_owned()));
            if self.next_create_on_ready {
                vec![DriveCommand::Create {
                    device_id,
                    path: "\\".to_owned(),
                    desired_access: irp::GENERIC_READ,
                    create_disposition: irp::FILE_OPEN,
                    create_options: irp::FILE_DIRECTORY_FILE,
                    request_tag: 1,
                }]
            } else {
                Vec::new()
            }
        }
        fn on_create_reply(
            &mut self,
            request_tag: u64,
            result: Result<irp::CreateReply, u32>,
        ) -> Vec<DriveCommand> {
            self.create_replies.push((request_tag, result));
            Vec::new()
        }
        fn on_close_reply(&mut self, _request_tag: u64, _status: u32) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn on_read_reply(
            &mut self,
            _request_tag: u64,
            _result: Result<Vec<u8>, u32>,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn on_write_reply(
            &mut self,
            _request_tag: u64,
            _result: Result<u32, u32>,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn on_query_directory_reply(
            &mut self,
            request_tag: u64,
            result: Result<Option<irp::DirectoryEntry>, u32>,
        ) -> Vec<DriveCommand> {
            self.query_directory_replies.push((request_tag, result));
            Vec::new()
        }
    }

    fn send_message(channel: &mut RdpdrChannel, body: Vec<u8>) -> Vec<Vec<u8>> {
        let wire = svc::chunkify(&body);
        assert_eq!(
            wire.len(),
            1,
            "test messages are expected to fit in one SVC chunk"
        );
        channel.on_channel_data(&wire[0]).unwrap()
    }

    #[test]
    fn new_channel_sends_server_announce_request() {
        let (_channel, initial) = RdpdrChannel::new(
            1004,
            1002,
            pdu::RDPDR_DTYP_FILESYSTEM,
            Box::new(RecordingConsumer::default()),
        );
        assert_eq!(initial.len(), 1);
    }

    #[test]
    fn client_name_triggers_capability_request_then_client_id_confirm() {
        let (mut channel, _initial) = RdpdrChannel::new(
            1004,
            1002,
            pdu::RDPDR_DTYP_FILESYSTEM,
            Box::new(RecordingConsumer::default()),
        );
        let mut client_name = Vec::new();
        client_name.write_u16_le(pdu::RDPDR_CTYP_CORE);
        client_name.write_u16_le(pdu::PAKID_CORE_CLIENT_NAME);

        let out = send_message(&mut channel, client_name);
        assert_eq!(out.len(), 2); // capability request, then client id confirm
    }

    #[test]
    fn client_capability_triggers_user_logged_on() {
        let (mut channel, _initial) = RdpdrChannel::new(
            1004,
            1002,
            pdu::RDPDR_DTYP_FILESYSTEM,
            Box::new(RecordingConsumer::default()),
        );
        let mut client_cap = Vec::new();
        client_cap.write_u16_le(pdu::RDPDR_CTYP_CORE);
        client_cap.write_u16_le(pdu::PAKID_CORE_CLIENT_CAPABILITY);

        let out = send_message(&mut channel, client_cap);
        assert_eq!(out.len(), 1);
    }

    fn device_list_announce(devices: &[(u32, u32, &str)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.write_u32_le(devices.len() as u32);
        for (device_type, device_id, dos_name) in devices {
            body.write_u32_le(*device_type);
            body.write_u32_le(*device_id);
            let mut name_bytes = dos_name.as_bytes().to_vec();
            name_bytes.resize(8, 0);
            body.write_slice(&name_bytes);
            body.write_u32_le(0);
        }
        let mut out = Vec::new();
        out.write_u16_le(pdu::RDPDR_CTYP_CORE);
        out.write_u16_le(pdu::PAKID_CORE_DEVICELIST_ANNOUNCE);
        out.write_slice(&body);
        out
    }

    #[test]
    fn filesystem_device_announce_registers_device_and_replies_success() {
        let (mut channel, _initial) = RdpdrChannel::new(
            1004,
            1002,
            pdu::RDPDR_DTYP_FILESYSTEM,
            Box::new(RecordingConsumer::default()),
        );
        let out = send_message(
            &mut channel,
            device_list_announce(&[(pdu::RDPDR_DTYP_FILESYSTEM, 1, "share")]),
        );
        assert_eq!(out.len(), 1); // just the device reply, consumer issued no commands
        assert_eq!(
            channel.devices().get(&1),
            Some(&(pdu::RDPDR_DTYP_FILESYSTEM, "share".to_owned()))
        );
    }

    #[test]
    fn unsupported_device_type_gets_rejected_reply_and_is_not_registered() {
        let (mut channel, _initial) = RdpdrChannel::new(
            1004,
            1002,
            pdu::RDPDR_DTYP_FILESYSTEM,
            Box::new(RecordingConsumer::default()),
        );
        let out = send_message(
            &mut channel,
            device_list_announce(&[(pdu::RDPDR_DTYP_PRINT, 2, "printer")]),
        );
        assert_eq!(out.len(), 1);
        assert!(channel.devices().is_empty());
    }

    #[test]
    fn printer_device_is_accepted_when_supported() {
        let (mut channel, _initial) = RdpdrChannel::new(
            1004,
            1002,
            pdu::RDPDR_DTYP_FILESYSTEM | pdu::RDPDR_DTYP_PRINT,
            Box::new(RecordingConsumer::default()),
        );
        let out = send_message(
            &mut channel,
            device_list_announce(&[(pdu::RDPDR_DTYP_PRINT, 3, "printer")]),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(
            channel.devices().get(&3),
            Some(&(pdu::RDPDR_DTYP_PRINT, "printer".to_owned()))
        );
    }

    #[test]
    fn device_ready_command_is_encoded_and_completion_routes_back_to_consumer() {
        let consumer = RecordingConsumer {
            next_create_on_ready: true,
            ..Default::default()
        };
        let (mut channel, _initial) =
            RdpdrChannel::new(1004, 1002, pdu::RDPDR_DTYP_FILESYSTEM, Box::new(consumer));

        let out = send_message(
            &mut channel,
            device_list_announce(&[(pdu::RDPDR_DTYP_FILESYSTEM, 1, "share")]),
        );
        assert_eq!(out.len(), 2); // device reply + the CREATE command the consumer issued

        // Craft a matching Device I/O Completion (CompletionId 0, the first
        // one this connection ever allocated) and feed it back in.
        let mut completion = Vec::new();
        completion.write_u16_le(pdu::RDPDR_CTYP_CORE);
        completion.write_u16_le(pdu::PAKID_CORE_DEVICE_IOCOMPLETION);
        completion.write_u32_le(1); // DeviceId
        completion.write_u32_le(0); // CompletionId
        completion.write_u32_le(pdu::STATUS_SUCCESS);
        completion.write_u32_le(99); // FileId
        completion.write_u8(irp::FILE_OPENED);

        let out = send_message(&mut channel, completion);
        assert!(out.is_empty()); // RecordingConsumer::on_create_reply issues no further commands
    }
}
