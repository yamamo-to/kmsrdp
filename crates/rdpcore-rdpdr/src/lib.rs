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
//! issue, letting a consumer (e.g. a directory-listing walk, or a FUSE
//! filesystem) drive a whole operation one reply at a time without
//! re-entrant calls back into [`RdpdrChannel`]. Externally driven
//! consumers (FUSE) also expose commands via [`DriveConsumer::poll_commands`]
//! and wake the connection loop through the factory-supplied sender.

pub mod irp;
pub mod pdu;

#[cfg(feature = "diagnostic")]
pub mod diagnostic;

use std::collections::HashMap;

use rdpcore_pdu::svc::wrap_indication;
use rdpcore_pdu::{DecodeError, svc};
use tokio::sync::mpsc::UnboundedSender;

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
    /// `IRP_MJ_SET_INFORMATION` with a pre-encoded SetBuffer (rename,
    /// end-of-file, basic times, …).
    SetInformation {
        device_id: u32,
        file_id: u32,
        fs_information_class: u32,
        set_buffer: Vec<u8>,
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
    /// Completion for [`DriveCommand::SetInformation`]. `Ok(())` when
    /// `IoStatus` was success.
    fn on_set_information_reply(
        &mut self,
        request_tag: u64,
        result: Result<(), u32>,
    ) -> Vec<DriveCommand>;
    /// Drain any commands queued by an external driver (e.g. FUSE) since
    /// the last poll. Reply callbacks may still return follow-up commands
    /// directly; this is the path for ops that originate outside the RDP
    /// completion cycle.
    fn poll_commands(&mut self) -> Vec<DriveCommand> {
        Vec::new()
    }
}

/// One [`DriveConsumer`] per connection, mirroring `SoundServerFactory`/
/// `CliprdrBackendFactory` - a fresh consumer (fresh open-file state) is
/// needed per connection, not a shared singleton.
pub trait DriveConsumerFactory: Send + Sync {
    /// OR of `pdu::RDPDR_DTYP_*` - which device types to accept and
    /// advertise capability for on every connection this factory serves.
    fn supported_device_types(&self) -> u32;
    /// `wake` is fired whenever the consumer may have new
    /// [`DriveConsumer::poll_commands`] ready (typically from a FUSE
    /// thread). The connection loop should call
    /// [`RdpdrChannel::flush_pending_commands`] on each wake.
    fn build_drive_consumer(&self, wake: UnboundedSender<()>) -> Box<dyn DriveConsumer>;
}

struct PendingOp {
    device_id: u32,
    major_function: u32,
    request_tag: u64,
}

/// Bounds the number of outstanding (issued but not yet completed) device
/// I/O requests for one connection. Without this, a stalled or
/// unresponsive client that stops sending `DeviceIoCompletion`s would let
/// `pending` grow without bound for as long as the consumer keeps issuing
/// new commands.
const MAX_PENDING_OPS: usize = 4096;

/// Bounds how many devices a single connection can have announced at
/// once. Without this, a client repeatedly sending `DeviceListAnnounce`
/// (each re-invoking `DriveConsumer::on_device_ready`, which may open
/// files/handles on the consumer side) could grow `devices` without bound
/// for the life of the connection.
const MAX_DEVICES: usize = 256;

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

/// Maximum allowed size for an rdpdr channel message (16 MB).
pub const MAX_RDPDR_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

fn drive_command_device_id(command: &DriveCommand) -> u32 {
    match command {
        DriveCommand::Create { device_id, .. }
        | DriveCommand::Close { device_id, .. }
        | DriveCommand::Read { device_id, .. }
        | DriveCommand::Write { device_id, .. }
        | DriveCommand::QueryDirectory { device_id, .. }
        | DriveCommand::SetInformation { device_id, .. } => *device_id,
    }
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

    /// Encode any commands waiting in [`DriveConsumer::poll_commands`].
    /// Call after the factory `wake` sender fires.
    pub fn flush_pending_commands(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let commands = self.consumer.poll_commands();
        self.encode_commands(commands, &mut out);
        out
    }

    /// `payload` is one SVC chunk (Channel PDU Header included) of
    /// `"rdpdr"`-channel data from an incoming MCS Send Data Request.
    pub fn on_channel_data(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>, DecodeError> {
        let Some(message) = svc::reassemble(
            &mut self.incoming_buffer,
            payload,
            MAX_RDPDR_MESSAGE_SIZE,
            "rdpdr.incoming_buffer",
        )?
        else {
            return Ok(Vec::new()); // wait for the rest
        };
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
                    let at_capacity = !self.devices.contains_key(&device.device_id)
                        && self.devices.len() >= MAX_DEVICES;
                    if self.supported & device.device_type != 0 && !at_capacity {
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
                        if at_capacity {
                            tracing::warn!(
                                "rdpdr: rejecting device {} - {MAX_DEVICES} devices already announced",
                                device.device_id
                            );
                        }
                        out.extend(self.wrap(pdu::encode_device_reply(
                            device.device_id,
                            pdu::STATUS_UNSUCCESSFUL,
                        )));
                    }
                }
            }
            pdu::ClientMessage::DeviceListRemove(device_ids) => {
                for device_id in &device_ids {
                    self.devices.remove(device_id);
                }
                // The client will never send a DeviceIoCompletion for a
                // device it just unplugged - without this, any I/O still
                // outstanding against it would sit in `pending` forever:
                // the consumer's callback never fires (a FUSE-style
                // consumer blocking on that reply would hang), and the
                // slot stays charged against MAX_PENDING_OPS. Synthesize a
                // failure completion for each instead.
                let stale: Vec<u32> = self
                    .pending
                    .iter()
                    .filter(|(_, op)| device_ids.contains(&op.device_id))
                    .map(|(&completion_id, _)| completion_id)
                    .collect();
                for completion_id in stale {
                    if let Some(pending) = self.pending.remove(&completion_id) {
                        // Consistent with the DeviceIoCompletion arm below:
                        // encode whatever follow-up commands the callback
                        // returns rather than assuming there can't be any
                        // (e.g. against a still-valid device) just because
                        // *this* device is gone.
                        let commands =
                            self.dispatch_completion(pending, pdu::STATUS_UNSUCCESSFUL, &[])?;
                        self.encode_commands(commands, &mut out);
                    }
                }
            }
            pdu::ClientMessage::DeviceIoCompletion {
                device_id,
                completion_id,
                io_status,
                body,
            } => {
                if let Some(pending) = self.pending.remove(&completion_id) {
                    if pending.device_id != device_id {
                        tracing::warn!(
                            "rdpdr: DeviceIoCompletion device_id mismatch for completion {completion_id} \
                             (expected {}, got {device_id}); dropping",
                            pending.device_id
                        );
                    } else {
                        let commands = self.dispatch_completion(pending, io_status, &body)?;
                        self.encode_commands(commands, &mut out);
                    }
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
                    match irp::decode_create_reply(body) {
                        Ok(reply) => Ok(reply),
                        Err(e) => {
                            tracing::warn!(
                                "rdpdr: CREATE completion decode failed ({e}); treating as I/O error"
                            );
                            Err(io_status.max(1))
                        }
                    }
                } else {
                    Err(io_status)
                };
                if let Err(status) = result {
                    tracing::warn!(
                        "rdpdr: CREATE failed NTSTATUS={status:#010x} (tag={})",
                        pending.request_tag
                    )
                }
                self.consumer.on_create_reply(pending.request_tag, result)
            }
            irp::IRP_MJ_CLOSE => self.consumer.on_close_reply(pending.request_tag, io_status),
            irp::IRP_MJ_READ => {
                let result = if ok {
                    match irp::decode_read_reply(body) {
                        Ok(data) => Ok(data),
                        Err(e) => {
                            tracing::warn!("rdpdr: READ completion decode failed ({e})");
                            Err(io_status.max(1))
                        }
                    }
                } else {
                    Err(io_status)
                };
                self.consumer.on_read_reply(pending.request_tag, result)
            }
            irp::IRP_MJ_WRITE => {
                let result = if ok {
                    match irp::decode_write_reply(body) {
                        Ok(n) => Ok(n),
                        Err(e) => {
                            tracing::warn!("rdpdr: WRITE completion decode failed ({e})");
                            Err(io_status.max(1))
                        }
                    }
                } else {
                    Err(io_status)
                };
                self.consumer.on_write_reply(pending.request_tag, result)
            }
            irp::IRP_MJ_DIRECTORY_CONTROL => {
                let result = if ok {
                    match irp::decode_query_directory_reply(body) {
                        Ok(entry) => Ok(entry),
                        Err(e) => {
                            tracing::warn!("rdpdr: QueryDirectory completion decode failed ({e})");
                            Err(io_status.max(1))
                        }
                    }
                } else if io_status == irp::STATUS_NO_MORE_FILES {
                    Ok(None)
                } else {
                    tracing::warn!(
                        "rdpdr: QueryDirectory failed NTSTATUS={io_status:#010x} (tag={})",
                        pending.request_tag
                    );
                    Err(io_status)
                };
                self.consumer
                    .on_query_directory_reply(pending.request_tag, result)
            }
            irp::IRP_MJ_SET_INFORMATION => {
                let result = if ok { Ok(()) } else { Err(io_status) };
                if let Err(status) = result {
                    tracing::warn!(
                        "rdpdr: SET_INFORMATION failed NTSTATUS={status:#010x} (tag={})",
                        pending.request_tag
                    );
                }
                self.consumer
                    .on_set_information_reply(pending.request_tag, result)
            }
            _ => Vec::new(),
        })
    }

    /// Reports a command as failed with `status` without ever issuing it
    /// on the wire (no completion_id allocated, nothing sent) - used when
    /// [`MAX_PENDING_OPS`] is already reached, so the consumer's callback
    /// still fires instead of the command being silently dropped.
    fn fail_command_without_issuing(
        &mut self,
        command: DriveCommand,
        status: u32,
    ) -> Vec<DriveCommand> {
        match command {
            DriveCommand::Create { request_tag, .. } => {
                self.consumer.on_create_reply(request_tag, Err(status))
            }
            DriveCommand::Close { request_tag, .. } => {
                self.consumer.on_close_reply(request_tag, status)
            }
            DriveCommand::Read { request_tag, .. } => {
                self.consumer.on_read_reply(request_tag, Err(status))
            }
            DriveCommand::Write { request_tag, .. } => {
                self.consumer.on_write_reply(request_tag, Err(status))
            }
            DriveCommand::QueryDirectory { request_tag, .. } => self
                .consumer
                .on_query_directory_reply(request_tag, Err(status)),
            DriveCommand::SetInformation { request_tag, .. } => self
                .consumer
                .on_set_information_reply(request_tag, Err(status)),
        }
    }

    fn encode_commands(&mut self, commands: Vec<DriveCommand>, out: &mut Vec<Vec<u8>>) {
        // A work queue rather than recursing on follow-up commands
        // (below, when MAX_PENDING_OPS is hit) - a consumer whose error
        // callback itself returns another command would otherwise recurse
        // once per command with no depth limit.
        let mut queue: std::collections::VecDeque<DriveCommand> = commands.into();
        while let Some(command) = queue.pop_front() {
            if self.pending.len() >= MAX_PENDING_OPS {
                tracing::warn!(
                    "rdpdr: {MAX_PENDING_OPS} I/O requests already outstanding (client not \
                     completing I/O?); failing new command instead of issuing it"
                );
                // The consumer is still waiting on a reply for this
                // command (e.g. a FUSE op blocked on it) - fail it
                // directly rather than silently dropping it, same as a
                // real I/O error completion would. A dropped Close in
                // particular would otherwise leak the handle on the
                // client for the rest of the session.
                let follow_up =
                    self.fail_command_without_issuing(command, pdu::STATUS_UNSUCCESSFUL);
                queue.extend(follow_up);
                continue;
            }
            let device_id = drive_command_device_id(&command);
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
                DriveCommand::SetInformation {
                    device_id,
                    file_id,
                    fs_information_class,
                    set_buffer,
                    request_tag,
                } => (
                    irp::IRP_MJ_SET_INFORMATION,
                    request_tag,
                    irp::encode_set_information_request(
                        device_id,
                        file_id,
                        completion_id,
                        fs_information_class,
                        &set_buffer,
                    ),
                ),
            };
            self.pending.insert(
                completion_id,
                PendingOp {
                    device_id,
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

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
