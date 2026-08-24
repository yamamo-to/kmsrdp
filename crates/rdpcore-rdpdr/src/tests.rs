use super::*;
use rdpcore_pdu::cursor::WriteBuf;
use std::sync::{Arc, Mutex};

type SetInfoReplies = Arc<Mutex<Vec<(u64, Result<(), u32>)>>>;

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
    fn on_set_information_reply(
        &mut self,
        _request_tag: u64,
        _result: Result<(), u32>,
    ) -> Vec<DriveCommand> {
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

fn device_list_remove(device_ids: &[u32]) -> Vec<u8> {
    let mut body = Vec::new();
    body.write_u32_le(device_ids.len() as u32);
    for id in device_ids {
        body.write_u32_le(*id);
    }
    let mut out = Vec::new();
    out.write_u16_le(pdu::RDPDR_CTYP_CORE);
    out.write_u16_le(pdu::PAKID_CORE_DEVICELIST_REMOVE);
    out.write_slice(&body);
    out
}

#[test]
fn device_list_remove_evicts_announced_device() {
    let (mut channel, _initial) = RdpdrChannel::new(
        1004,
        1002,
        pdu::RDPDR_DTYP_FILESYSTEM,
        Box::new(RecordingConsumer::default()),
    );
    send_message(
        &mut channel,
        device_list_announce(&[(pdu::RDPDR_DTYP_FILESYSTEM, 1, "share")]),
    );
    assert!(channel.devices().contains_key(&1));

    send_message(&mut channel, device_list_remove(&[1]));
    assert!(channel.devices().is_empty());
}

#[test]
fn device_list_remove_fails_pending_ops_against_that_device() {
    type CreateReplies = Arc<Mutex<Vec<(u64, Result<irp::CreateReply, u32>)>>>;

    struct CreateOnReadyConsumer {
        replies: CreateReplies,
    }
    impl DriveConsumer for CreateOnReadyConsumer {
        fn on_device_ready(
            &mut self,
            device_id: u32,
            _device_type: u32,
            _dos_name: &str,
        ) -> Vec<DriveCommand> {
            vec![DriveCommand::Create {
                device_id,
                path: "\\".to_owned(),
                desired_access: irp::GENERIC_READ,
                create_disposition: irp::FILE_OPEN,
                create_options: irp::FILE_DIRECTORY_FILE,
                request_tag: 1,
            }]
        }
        fn on_create_reply(
            &mut self,
            request_tag: u64,
            result: Result<irp::CreateReply, u32>,
        ) -> Vec<DriveCommand> {
            self.replies.lock().unwrap().push((request_tag, result));
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
            _request_tag: u64,
            _result: Result<Option<irp::DirectoryEntry>, u32>,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn on_set_information_reply(
            &mut self,
            _request_tag: u64,
            _result: Result<(), u32>,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
    }

    let replies = Arc::new(Mutex::new(Vec::new()));
    let (mut channel, _initial) = RdpdrChannel::new(
        1004,
        1002,
        pdu::RDPDR_DTYP_FILESYSTEM,
        Box::new(CreateOnReadyConsumer {
            replies: Arc::clone(&replies),
        }),
    );

    // on_device_ready issues a CREATE, now pending with no completion
    // sent yet.
    send_message(
        &mut channel,
        device_list_announce(&[(pdu::RDPDR_DTYP_FILESYSTEM, 1, "share")]),
    );
    assert!(replies.lock().unwrap().is_empty());

    // The client unplugs the device without ever completing that
    // CREATE - the consumer must still be told it failed, not left
    // hanging forever.
    send_message(&mut channel, device_list_remove(&[1]));
    assert_eq!(
        replies.lock().unwrap().as_slice(),
        &[(1, Err(pdu::STATUS_UNSUCCESSFUL))]
    );
}

#[test]
fn device_announce_beyond_max_devices_is_rejected() {
    let (mut channel, _initial) = RdpdrChannel::new(
        1004,
        1002,
        pdu::RDPDR_DTYP_FILESYSTEM,
        Box::new(RecordingConsumer::default()),
    );
    for id in 0..MAX_DEVICES as u32 {
        send_message(
            &mut channel,
            device_list_announce(&[(pdu::RDPDR_DTYP_FILESYSTEM, id, "share")]),
        );
    }
    assert_eq!(channel.devices().len(), MAX_DEVICES);

    let out = send_message(
        &mut channel,
        device_list_announce(&[(
            pdu::RDPDR_DTYP_FILESYSTEM,
            MAX_DEVICES as u32,
            "one-too-many",
        )]),
    );
    assert_eq!(out.len(), 1); // rejection reply only, no on_device_ready commands
    assert_eq!(channel.devices().len(), MAX_DEVICES);
    assert!(!channel.devices().contains_key(&(MAX_DEVICES as u32)));

    // Re-announcing an already-registered device at capacity must
    // still work (it's not a *new* entry).
    let out = send_message(
        &mut channel,
        device_list_announce(&[(pdu::RDPDR_DTYP_FILESYSTEM, 0, "share-again")]),
    );
    assert_eq!(out.len(), 1);
    // The wire DosName field is a fixed 8 bytes, so "share-again" is
    // truncated to "share-ag" - unrelated to what this test checks.
    assert_eq!(
        channel.devices().get(&0),
        Some(&(pdu::RDPDR_DTYP_FILESYSTEM, "share-ag".to_owned()))
    );
}

#[test]
fn command_beyond_max_pending_ops_fails_instead_of_hanging() {
    type CreateReplies = Arc<Mutex<Vec<(u64, Result<irp::CreateReply, u32>)>>>;

    struct FloodingConsumer {
        replies: CreateReplies,
        next_tag: u64,
    }
    impl DriveConsumer for FloodingConsumer {
        fn on_device_ready(
            &mut self,
            _device_id: u32,
            _device_type: u32,
            _dos_name: &str,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn on_create_reply(
            &mut self,
            request_tag: u64,
            result: Result<irp::CreateReply, u32>,
        ) -> Vec<DriveCommand> {
            self.replies.lock().unwrap().push((request_tag, result));
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
            _request_tag: u64,
            _result: Result<Option<irp::DirectoryEntry>, u32>,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn on_set_information_reply(
            &mut self,
            _request_tag: u64,
            _result: Result<(), u32>,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn poll_commands(&mut self) -> Vec<DriveCommand> {
            self.next_tag += 1;
            vec![DriveCommand::Create {
                device_id: 1,
                path: "\\".to_owned(),
                desired_access: irp::GENERIC_READ,
                create_disposition: irp::FILE_OPEN,
                create_options: irp::FILE_DIRECTORY_FILE,
                request_tag: self.next_tag,
            }]
        }
    }

    let replies = Arc::new(Mutex::new(Vec::new()));
    let (mut channel, _initial) = RdpdrChannel::new(
        1004,
        1002,
        pdu::RDPDR_DTYP_FILESYSTEM,
        Box::new(FloodingConsumer {
            replies: Arc::clone(&replies),
            next_tag: 0,
        }),
    );

    for _ in 0..MAX_PENDING_OPS {
        let out = channel.flush_pending_commands();
        assert_eq!(out.len(), 1);
    }
    assert!(replies.lock().unwrap().is_empty());

    // One more, past the cap: must fail immediately via the
    // consumer's callback rather than being silently dropped.
    let out = channel.flush_pending_commands();
    assert!(out.is_empty());
    let got = replies.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(
        got[0],
        (MAX_PENDING_OPS as u64 + 1, Err(pdu::STATUS_UNSUCCESSFUL))
    );
}

#[test]
fn a_long_chain_of_immediately_failing_follow_ups_does_not_overflow_the_stack() {
    // A consumer whose error callback itself always returns another
    // command (a retry loop) used to recurse once per link in
    // encode_commands - with pending already pinned at the cap, every
    // one of these fails instantly and chains into the next. Proves
    // encode_commands' work-queue rewrite handles a long chain without
    // growing the call stack.
    const CHAIN_LEN: u64 = 200_000;

    struct RetryingConsumer {
        remaining: u64,
    }
    impl DriveConsumer for RetryingConsumer {
        fn on_device_ready(
            &mut self,
            _device_id: u32,
            _device_type: u32,
            _dos_name: &str,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn on_create_reply(
            &mut self,
            _request_tag: u64,
            _result: Result<irp::CreateReply, u32>,
        ) -> Vec<DriveCommand> {
            if self.remaining == 0 {
                return Vec::new();
            }
            self.remaining -= 1;
            vec![DriveCommand::Create {
                device_id: 1,
                path: "\\".to_owned(),
                desired_access: irp::GENERIC_READ,
                create_disposition: irp::FILE_OPEN,
                create_options: irp::FILE_DIRECTORY_FILE,
                request_tag: 0,
            }]
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
            _request_tag: u64,
            _result: Result<Option<irp::DirectoryEntry>, u32>,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn on_set_information_reply(
            &mut self,
            _request_tag: u64,
            _result: Result<(), u32>,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn poll_commands(&mut self) -> Vec<DriveCommand> {
            Vec::new()
        }
    }

    let (mut channel, _initial) = RdpdrChannel::new(
        1004,
        1002,
        pdu::RDPDR_DTYP_FILESYSTEM,
        Box::new(RetryingConsumer {
            remaining: CHAIN_LEN,
        }),
    );
    // Fill pending to the cap so the very first command below already
    // fails and kicks off the chain.
    let mut fill = Vec::new();
    for i in 0..MAX_PENDING_OPS as u64 {
        fill.push(DriveCommand::Create {
            device_id: 1,
            path: "\\".to_owned(),
            desired_access: irp::GENERIC_READ,
            create_disposition: irp::FILE_OPEN,
            create_options: irp::FILE_DIRECTORY_FILE,
            request_tag: i,
        });
    }
    let mut out = Vec::new();
    channel.encode_commands(fill, &mut out);
    assert!(!out.is_empty());
    out.clear();
    channel.encode_commands(
        vec![DriveCommand::Create {
            device_id: 1,
            path: "\\".to_owned(),
            desired_access: irp::GENERIC_READ,
            create_disposition: irp::FILE_OPEN,
            create_options: irp::FILE_DIRECTORY_FILE,
            request_tag: 0,
        }],
        &mut out,
    );
    assert!(out.is_empty());
}

#[test]
fn device_io_completion_with_mismatched_device_id_is_dropped() {
    type CreateReplies = Arc<Mutex<Vec<(u64, Result<irp::CreateReply, u32>)>>>;

    struct CreateOnReadyConsumer {
        replies: CreateReplies,
    }
    impl DriveConsumer for CreateOnReadyConsumer {
        fn on_device_ready(
            &mut self,
            device_id: u32,
            _device_type: u32,
            _dos_name: &str,
        ) -> Vec<DriveCommand> {
            vec![DriveCommand::Create {
                device_id,
                path: "\\".to_owned(),
                desired_access: irp::GENERIC_READ,
                create_disposition: irp::FILE_OPEN,
                create_options: irp::FILE_DIRECTORY_FILE,
                request_tag: 1,
            }]
        }
        fn on_create_reply(
            &mut self,
            request_tag: u64,
            result: Result<irp::CreateReply, u32>,
        ) -> Vec<DriveCommand> {
            self.replies.lock().unwrap().push((request_tag, result));
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
            _request_tag: u64,
            _result: Result<Option<irp::DirectoryEntry>, u32>,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn on_set_information_reply(
            &mut self,
            _request_tag: u64,
            _result: Result<(), u32>,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
    }

    let replies = Arc::new(Mutex::new(Vec::new()));
    let (mut channel, _initial) = RdpdrChannel::new(
        1004,
        1002,
        pdu::RDPDR_DTYP_FILESYSTEM,
        Box::new(CreateOnReadyConsumer {
            replies: Arc::clone(&replies),
        }),
    );

    send_message(
        &mut channel,
        device_list_announce(&[(pdu::RDPDR_DTYP_FILESYSTEM, 1, "share")]),
    );

    // Completion claims a different DeviceId (2) than the pending
    // CREATE was issued against (1) - must be dropped, not routed to
    // the consumer as if it were a real reply.
    let mut completion = Vec::new();
    completion.write_u16_le(pdu::RDPDR_CTYP_CORE);
    completion.write_u16_le(pdu::PAKID_CORE_DEVICE_IOCOMPLETION);
    completion.write_u32_le(2); // DeviceId (wrong)
    completion.write_u32_le(0); // CompletionId
    completion.write_u32_le(pdu::STATUS_SUCCESS);
    completion.write_u32_le(99); // FileId
    completion.write_u8(irp::FILE_OPENED);

    let out = send_message(&mut channel, completion);
    assert!(out.is_empty());
    assert!(
        replies.lock().unwrap().is_empty(),
        "mismatched-device_id completion must not reach the consumer"
    );
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

#[test]
fn flush_pending_commands_encodes_polled_queue() {
    struct QueuedConsumer {
        queued: Vec<DriveCommand>,
    }
    impl DriveConsumer for QueuedConsumer {
        fn on_device_ready(
            &mut self,
            _device_id: u32,
            _device_type: u32,
            _dos_name: &str,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn on_create_reply(
            &mut self,
            _request_tag: u64,
            _result: Result<irp::CreateReply, u32>,
        ) -> Vec<DriveCommand> {
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
            _request_tag: u64,
            _result: Result<Option<irp::DirectoryEntry>, u32>,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn on_set_information_reply(
            &mut self,
            _request_tag: u64,
            _result: Result<(), u32>,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn poll_commands(&mut self) -> Vec<DriveCommand> {
            std::mem::take(&mut self.queued)
        }
    }

    let (mut channel, _initial) = RdpdrChannel::new(
        1004,
        1002,
        pdu::RDPDR_DTYP_FILESYSTEM,
        Box::new(QueuedConsumer {
            queued: vec![DriveCommand::Create {
                device_id: 1,
                path: "\\".to_owned(),
                desired_access: irp::GENERIC_READ,
                create_disposition: irp::FILE_OPEN,
                create_options: irp::FILE_DIRECTORY_FILE,
                request_tag: 42,
            }],
        }),
    );
    let out = channel.flush_pending_commands();
    assert_eq!(out.len(), 1);
    assert!(channel.flush_pending_commands().is_empty());
}

#[test]
fn set_information_command_encodes_and_completes() {
    let replies = Arc::new(Mutex::new(Vec::<(u64, Result<(), u32>)>::new()));
    let replies_cb = Arc::clone(&replies);

    struct SetInfoConsumer {
        replies: SetInfoReplies,
    }
    impl DriveConsumer for SetInfoConsumer {
        fn on_device_ready(
            &mut self,
            _device_id: u32,
            _device_type: u32,
            _dos_name: &str,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn on_create_reply(
            &mut self,
            _request_tag: u64,
            _result: Result<irp::CreateReply, u32>,
        ) -> Vec<DriveCommand> {
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
            _request_tag: u64,
            _result: Result<Option<irp::DirectoryEntry>, u32>,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn on_set_information_reply(
            &mut self,
            request_tag: u64,
            result: Result<(), u32>,
        ) -> Vec<DriveCommand> {
            self.replies.lock().unwrap().push((request_tag, result));
            Vec::new()
        }
        fn poll_commands(&mut self) -> Vec<DriveCommand> {
            vec![DriveCommand::SetInformation {
                device_id: 1,
                file_id: 9,
                fs_information_class: irp::FILE_DISPOSITION_INFORMATION,
                set_buffer: irp::disposition_information_buffer(true),
                request_tag: 77,
            }]
        }
    }

    let (mut channel, _initial) = RdpdrChannel::new(
        1004,
        1002,
        pdu::RDPDR_DTYP_FILESYSTEM,
        Box::new(SetInfoConsumer {
            replies: replies_cb,
        }),
    );
    let out = channel.flush_pending_commands();
    assert_eq!(out.len(), 1);
    let wire = out.into_iter().flatten().collect::<Vec<_>>();
    assert!(
        wire.windows(4)
            .any(|w| w == irp::IRP_MJ_SET_INFORMATION.to_le_bytes()),
        "expected SET_INFORMATION major function in wire bytes"
    );

    let mut completion = Vec::new();
    completion.write_u16_le(pdu::RDPDR_CTYP_CORE);
    completion.write_u16_le(pdu::PAKID_CORE_DEVICE_IOCOMPLETION);
    completion.write_u32_le(1);
    completion.write_u32_le(0);
    completion.write_u32_le(pdu::STATUS_SUCCESS);
    assert!(send_message(&mut channel, completion).is_empty());
    assert_eq!(replies.lock().unwrap().as_slice(), &[(77, Ok(()))]);
}

#[test]
fn set_information_failure_surfaces_to_consumer() {
    let replies = Arc::new(Mutex::new(Vec::<(u64, Result<(), u32>)>::new()));
    let replies_cb = Arc::clone(&replies);

    struct SetInfoConsumer {
        replies: SetInfoReplies,
    }
    impl DriveConsumer for SetInfoConsumer {
        fn on_device_ready(
            &mut self,
            _device_id: u32,
            _device_type: u32,
            _dos_name: &str,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn on_create_reply(
            &mut self,
            _request_tag: u64,
            _result: Result<irp::CreateReply, u32>,
        ) -> Vec<DriveCommand> {
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
            _request_tag: u64,
            _result: Result<Option<irp::DirectoryEntry>, u32>,
        ) -> Vec<DriveCommand> {
            Vec::new()
        }
        fn on_set_information_reply(
            &mut self,
            request_tag: u64,
            result: Result<(), u32>,
        ) -> Vec<DriveCommand> {
            self.replies.lock().unwrap().push((request_tag, result));
            Vec::new()
        }
        fn poll_commands(&mut self) -> Vec<DriveCommand> {
            vec![DriveCommand::SetInformation {
                device_id: 1,
                file_id: 2,
                fs_information_class: irp::FILE_RENAME_INFORMATION,
                set_buffer: irp::rename_information_buffer("\\new.txt", false),
                request_tag: 5,
            }]
        }
    }

    let (mut channel, _initial) = RdpdrChannel::new(
        1004,
        1002,
        pdu::RDPDR_DTYP_FILESYSTEM,
        Box::new(SetInfoConsumer {
            replies: replies_cb,
        }),
    );
    let _ = channel.flush_pending_commands();

    let mut completion = Vec::new();
    completion.write_u16_le(pdu::RDPDR_CTYP_CORE);
    completion.write_u16_le(pdu::PAKID_CORE_DEVICE_IOCOMPLETION);
    completion.write_u32_le(1);
    completion.write_u32_le(0);
    completion.write_u32_le(0xC000_0022);
    assert!(send_message(&mut channel, completion).is_empty());
    let got = replies.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, 5);
    assert_eq!(got[0].1, Err(0xC000_0022));
}

#[test]
fn rejects_oversized_payload() {
    let (mut channel, _) = RdpdrChannel::new(
        1004,
        1002,
        pdu::RDPDR_DTYP_FILESYSTEM,
        Box::new(RecordingConsumer::default()),
    );
    // Craft an SVC chunk declaring FLAG_FIRST with a payload exceeding 16 MB
    let mut raw = Vec::new();
    raw.extend_from_slice(&(MAX_RDPDR_MESSAGE_SIZE as u32 + 10).to_le_bytes()); // total_length
    raw.extend_from_slice(&svc::CHANNEL_FLAG_FIRST.to_le_bytes()); // flags
    raw.resize(8 + MAX_RDPDR_MESSAGE_SIZE + 1, 0x41);
    let result = channel.on_channel_data(&raw);
    assert!(
        matches!(result, Err(DecodeError::InvalidValue { field, .. }) if field == "rdpdr.incoming_buffer")
    );
}
