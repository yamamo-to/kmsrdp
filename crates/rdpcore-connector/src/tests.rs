use super::*;
use rdpcore_pdu::cursor::WriteBuf;
use rdpcore_pdu::gcc::{
    CS_MCS_MSGCHANNEL, ChannelDef, ClientCoreData, ClientNetworkData, ClientSecurityData,
};

fn client_connect_initial(static_channel_names: &[&str], with_message_channel: bool) -> Vec<u8> {
    let client_blocks = ClientGccBlocks {
        core: ClientCoreData {
            version: 0x0008_0004,
            desktop_width: 1024,
            desktop_height: 768,
            color_depth: 0xCA01,
            sas_sequence: 0xAA03,
            keyboard_layout: 0x0409,
            client_build: 2600,
            client_name: "test-client".to_owned(),
            keyboard_type: 4,
            keyboard_subtype: 0,
            keyboard_function_key: 12,
            ime_file_name: String::new(),
        },
        security: ClientSecurityData::default(),
        network: Some(ClientNetworkData {
            channels: static_channel_names
                .iter()
                .map(|name| ChannelDef {
                    name: (*name).to_owned(),
                    options: 0,
                })
                .collect(),
        }),
        early_capability_flags: None,
        cluster: None,
        message_channel: None,
    };
    let mut client_gcc_blocks = client_blocks.encode();
    if with_message_channel {
        client_gcc_blocks.write_u16_le(CS_MCS_MSGCHANNEL);
        client_gcc_blocks.write_u16_le(8); // 4-byte header + 4-byte flags body
        client_gcc_blocks.write_u32_le(0xC000_0000);
    }
    let request = ConferenceCreateRequest { client_gcc_blocks };
    let connect_initial = ConnectInitial {
        target_parameters: DomainParameters::target(),
        min_parameters: DomainParameters::min(),
        max_parameters: DomainParameters::max(),
        user_data: request.encode(),
    };
    x224::wrap_data(&connect_initial.encode())
}

/// Drives an `Acceptor` all the way to `Accepted`, standing in for a
/// real client's byte stream so the whole handshake can be tested
/// without a socket.
fn run_full_handshake(
    static_channel_names: &[&str],
) -> (Acceptor, AcceptedConnection, ClientCredentials) {
    run_full_handshake_inner(static_channel_names, false)
}

fn run_full_handshake_inner(
    static_channel_names: &[&str],
    with_message_channel: bool,
) -> (Acceptor, AcceptedConnection, ClientCredentials) {
    let (mut acceptor, credentials) =
        drive_to_wait_confirm_active(static_channel_names, with_message_channel);
    let accepted = drive_confirm_active_and_finalization(&mut acceptor);
    (acceptor, accepted, credentials)
}

/// Drives an `Acceptor` up to (but not through) `WaitConfirmActive` -
/// the shared prefix of `run_full_handshake_inner`, factored out so
/// tests can exercise `WaitConfirmActive`'s own behavior (e.g. Confirm
/// Active fragment reassembly) directly.
fn drive_to_wait_confirm_active(
    static_channel_names: &[&str],
    with_message_channel: bool,
) -> (Acceptor, ClientCredentials) {
    let mut acceptor = Acceptor::new(1024, 768);

    let request = ConnectionRequest {
        cookie: Some("kmsrdp".to_owned()),
        flags: x224::RequestFlags(0),
        protocol: SecurityProtocol::SSL,
    };
    let result = acceptor.step(&request.encode()).unwrap();
    assert_eq!(result.event, AcceptorEvent::TlsUpgrade);

    let result = acceptor
        .step(&client_connect_initial(
            static_channel_names,
            with_message_channel,
        ))
        .unwrap();
    assert!(matches!(result.event, AcceptorEvent::None));

    let result = acceptor
        .step(&x224::wrap_data(
            &ErectDomainRequest {
                sub_height: 0,
                sub_interval: 0,
            }
            .encode(),
        ))
        .unwrap();
    assert!(matches!(result.event, AcceptorEvent::None));

    let result = acceptor
        .step(&x224::wrap_data(&AttachUserRequest.encode()))
        .unwrap();
    assert!(matches!(result.event, AcceptorEvent::None));

    let mut channel_ids = vec![USER_CHANNEL_ID, IO_CHANNEL_ID];
    channel_ids.extend((0..static_channel_names.len()).map(|i| IO_CHANNEL_ID + 1 + i as u16));
    if with_message_channel {
        channel_ids.push(IO_CHANNEL_ID + 1 + static_channel_names.len() as u16);
    }
    for channel_id in channel_ids {
        let join = ChannelJoinRequest {
            initiator: USER_CHANNEL_ID,
            channel_id,
        };
        let result = acceptor.step(&x224::wrap_data(&join.encode())).unwrap();
        assert!(matches!(result.event, AcceptorEvent::None));
    }

    let client_info_pdu = ClientInfoPdu {
        info: rdpcore_pdu::client_info::ClientInfo {
            username: "kmsrdp".to_owned(),
            password: "hunter2".to_owned(),
            flags: rdpcore_pdu::client_info::ClientInfoFlags::UNICODE,
            ..Default::default()
        },
    };
    let send_data = SendData {
        initiator: USER_CHANNEL_ID,
        channel_id: IO_CHANNEL_ID,
        data: client_info_pdu.encode(),
        complete: true,
    };
    let result = acceptor
        .step(&x224::wrap_data(&send_data.encode_request()))
        .unwrap();
    let AcceptorEvent::ClientInfoReceived(credentials) = result.event else {
        panic!("expected ClientInfoReceived, got {:?}", result.event);
    };
    acceptor.approve_client_info().unwrap();

    (acceptor, credentials)
}

/// Drives an `Acceptor` from `WaitConfirmActive` through to `Accepted` -
/// the tail shared by the initial handshake and every server-initiated
/// resize (`Acceptor::begin_resize` re-enters at exactly this state).
fn drive_confirm_active_and_finalization(acceptor: &mut Acceptor) -> AcceptedConnection {
    // Client's Confirm Active - content doesn't matter, just needs to
    // structurally decode (share control header + at least 0 caps).
    let send_data = SendData {
        initiator: USER_CHANNEL_ID,
        channel_id: IO_CHANNEL_ID,
        data: confirm_active_fixture(),
        complete: true,
    };
    let result = acceptor
        .step(&x224::wrap_data(&send_data.encode_request()))
        .unwrap();
    assert!(matches!(result.event, AcceptorEvent::None));
    assert!(
        !result.response.is_empty(),
        "server must answer Confirm Active with Synchronize + Cooperate"
    );

    for (pdu_type2, body) in [
        (
            ShareDataPduType::Synchronize,
            SynchronizePdu {
                target_user: USER_CHANNEL_ID,
            }
            .encode_body(),
        ),
        (
            ShareDataPduType::Control,
            ControlPdu {
                action: ControlPdu::COOPERATE,
                grant_id: 0,
                control_id: 0,
            }
            .encode_body(),
        ),
        (
            ShareDataPduType::Control,
            ControlPdu {
                action: ControlPdu::REQUEST_CONTROL,
                grant_id: 0,
                control_id: 0,
            }
            .encode_body(),
        ),
        (
            ShareDataPduType::FontList,
            FontPdu::font_map_default().encode_body(),
        ),
    ] {
        let data_pdu = data_pdu_bytes(pdu_type2, body);
        let send_data = SendData {
            initiator: USER_CHANNEL_ID,
            channel_id: IO_CHANNEL_ID,
            data: data_pdu,
            complete: true,
        };
        let result = acceptor
            .step(&x224::wrap_data(&send_data.encode_request()))
            .unwrap();
        if let AcceptorEvent::Accepted(accepted) = result.event {
            assert!(acceptor.is_finished());
            return accepted;
        }
    }
    panic!("acceptor never reached Accepted");
}

/// A minimal but structurally valid Confirm Active (Share Control
/// Header + originatorId + zero capabilities) - the acceptor doesn't
/// interpret capability content, only needs it to parse.
fn confirm_active_fixture() -> Vec<u8> {
    use rdpcore_pdu::cursor::WriteBuf;
    let mut body = Vec::new();
    body.write_u16_le(USER_CHANNEL_ID); // originatorId
    body.write_u16_le(1); // lengthSourceDescriptor
    body.write_u16_le(4); // lengthCombinedCapabilities
    body.write_u8(0); // sourceDescriptor: empty
    body.write_u16_le(0); // numberCapabilities
    body.write_u16_le(0); // pad2Octets

    let mut out = Vec::new();
    out.write_u16_le((10 + body.len()) as u16); // totalLength
    out.write_u16_le(0x10 | 0x3); // version sentinel | ConfirmActivePdu
    out.write_u16_le(USER_CHANNEL_ID); // pdu_source
    out.write_u32_le(SHARE_ID);
    out.extend(body);
    out
}

#[test]
fn client_credentials_debug_redacts_password() {
    let creds = ClientCredentials {
        domain: "DOMAIN".to_owned(),
        username: "admin".to_owned(),
        password: "super_secret_password".to_owned(),
    };
    let formatted = format!("{creds:?}");
    assert!(!formatted.contains("super_secret_password"));
    assert!(formatted.contains("[REDACTED]"));
}

#[test]
fn attach_user_request_with_non_standard_optional_field() {
    let mut payload = vec![(10 << 2) | 2];
    payload.extend_from_slice(&[0x00, 0x00]);
    let mut cursor = ReadCursor::new(&payload);
    assert_eq!(
        AttachUserRequest::decode_from_cursor(&mut cursor).unwrap(),
        AttachUserRequest
    );
    assert_eq!(cursor.remaining(), 0);
}

#[test]
fn batched_mcs_domain_pdus_in_one_x224_payload() {
    let mut acceptor = Acceptor::new(1024, 768);
    let request = ConnectionRequest {
        cookie: Some("kmsrdp".to_owned()),
        flags: x224::RequestFlags(0),
        protocol: SecurityProtocol::SSL,
    };
    acceptor.step(&request.encode()).unwrap();
    acceptor.step(&client_connect_initial(&[], false)).unwrap();

    let mut mcs_payload = ErectDomainRequest {
        sub_height: 0,
        sub_interval: 0,
    }
    .encode();
    mcs_payload.extend(AttachUserRequest.encode());
    mcs_payload.extend(
        ChannelJoinRequest {
            initiator: USER_CHANNEL_ID,
            channel_id: USER_CHANNEL_ID,
        }
        .encode(),
    );

    let result = acceptor.step(&x224::wrap_data(&mcs_payload)).unwrap();
    assert!(
        !result.response.is_empty(),
        "attach user + first join must be answered"
    );
    assert!(matches!(result.event, AcceptorEvent::None));
}

#[test]
fn full_handshake_reaches_accepted_with_no_static_channels() {
    let (_acceptor, accepted, credentials) = run_full_handshake(&[]);
    assert_eq!(accepted.io_channel_id, IO_CHANNEL_ID);
    assert_eq!(accepted.user_channel_id, USER_CHANNEL_ID);
    assert!(accepted.static_channels.is_empty());
    assert_eq!(accepted.desktop_width, 1024);
    assert_eq!(accepted.desktop_height, 768);
    assert_eq!(credentials.username, "kmsrdp");
    assert_eq!(credentials.password, "hunter2");
}

#[test]
fn confirm_active_reassembly_rejects_fragments_beyond_the_size_cap() {
    let (mut acceptor, _credentials) = drive_to_wait_confirm_active(&[], false);

    // Feed fragments (never `complete`) past MAX_CONFIRM_ACTIVE_LEN -
    // must be rejected rather than growing confirm_active_buf without
    // bound.
    let chunk = vec![0x41u8; 64 * 1024];
    let mut sent = 0usize;
    let mut result = Ok(());
    while sent <= MAX_CONFIRM_ACTIVE_LEN {
        let send_data = SendData {
            initiator: USER_CHANNEL_ID,
            channel_id: IO_CHANNEL_ID,
            data: chunk.clone(),
            complete: false,
        };
        result = acceptor
            .step(&x224::wrap_data(&send_data.encode_request()))
            .map(|_| ());
        sent += chunk.len();
        if result.is_err() {
            break;
        }
    }
    assert!(
        matches!(result, Err(ConnectorError::Decode(_))),
        "expected reassembly to be rejected once past the cap, got {result:?}"
    );
}

#[test]
fn full_handshake_with_message_channel_reaches_accepted() {
    let (_acceptor, accepted, _credentials) =
        run_full_handshake_inner(&["rdpdr", "rdpsnd", "cliprdr", "drdynvc"], true);
    assert_eq!(accepted.static_channels.len(), 4);
}

#[test]
fn full_handshake_negotiates_static_channels_in_client_order() {
    let (_acceptor, accepted, _credentials) = run_full_handshake(&["cliprdr", "rdpsnd"]);
    assert_eq!(
        accepted.static_channels,
        vec![
            ("cliprdr".to_owned(), IO_CHANNEL_ID + 1),
            ("rdpsnd".to_owned(), IO_CHANNEL_ID + 2)
        ]
    );
}

#[test]
fn rejects_client_that_does_not_offer_ssl() {
    let mut acceptor = Acceptor::new(1024, 768);
    let request = ConnectionRequest {
        cookie: None,
        flags: x224::RequestFlags(0),
        protocol: SecurityProtocol::RDP,
    };
    let result = acceptor.step(&request.encode()).unwrap();
    assert_eq!(result.event, AcceptorEvent::Rejected);
    assert!(acceptor.is_finished());

    let confirm = ConnectionConfirm::decode(&result.response).unwrap();
    assert!(matches!(
        confirm,
        ConnectionConfirm::Failure {
            code: FailureCode::SSL_REQUIRED_BY_SERVER
        }
    ));
}

#[test]
fn prefers_hybrid_when_client_offers_nla() {
    let mut acceptor = Acceptor::new(1024, 768);
    let request = ConnectionRequest {
        cookie: None,
        flags: x224::RequestFlags(0),
        protocol: SecurityProtocol::SSL | SecurityProtocol::HYBRID,
    };
    let result = acceptor.step(&request.encode()).unwrap();
    assert_eq!(result.event, AcceptorEvent::TlsUpgrade);
    assert!(acceptor.requires_credssp());
    assert_eq!(acceptor.selected_protocol(), SecurityProtocol::HYBRID);
    match ConnectionConfirm::decode(&result.response).unwrap() {
        ConnectionConfirm::Response { protocol, .. } => {
            assert_eq!(protocol, SecurityProtocol::HYBRID);
        }
        other => panic!("expected Response, got {other:?}"),
    }
}

#[test]
fn falls_back_to_ssl_when_client_omits_hybrid() {
    let mut acceptor = Acceptor::new(1024, 768);
    let request = ConnectionRequest {
        cookie: None,
        flags: x224::RequestFlags(0),
        protocol: SecurityProtocol::SSL,
    };
    let result = acceptor.step(&request.encode()).unwrap();
    assert_eq!(result.event, AcceptorEvent::TlsUpgrade);
    assert!(!acceptor.requires_credssp());
    assert_eq!(acceptor.selected_protocol(), SecurityProtocol::SSL);
    match ConnectionConfirm::decode(&result.response).unwrap() {
        ConnectionConfirm::Response { protocol, .. } => {
            assert_eq!(protocol, SecurityProtocol::SSL);
        }
        other => panic!("expected Response, got {other:?}"),
    }
}

#[test]
fn begin_resize_round_trips_to_accepted_with_new_dimensions() {
    let (mut acceptor, first_accepted, _credentials) = run_full_handshake(&[]);
    assert_eq!(
        (first_accepted.desktop_width, first_accepted.desktop_height),
        (1024, 768)
    );

    let response = acceptor.begin_resize(1920, 1080).unwrap();
    assert!(!response.is_empty());
    assert!(
        !acceptor.is_finished(),
        "begin_resize must reopen the connection sequence"
    );

    let resized = drive_confirm_active_and_finalization(&mut acceptor);
    assert_eq!(
        (resized.desktop_width, resized.desktop_height),
        (1920, 1080)
    );
    assert_eq!(
        resized.share_id, first_accepted.share_id,
        "share_id doesn't change across a resize"
    );
    assert!(acceptor.is_finished());
}

#[test]
fn begin_resize_before_accepted_is_rejected() {
    let mut acceptor = Acceptor::new(1024, 768);
    assert!(matches!(
        acceptor.begin_resize(1920, 1080),
        Err(ConnectorError::NotReady)
    ));
}

#[test]
fn step_after_accepted_preserves_accepted_so_resize_still_works() {
    let (mut acceptor, _accepted, _credentials) = run_full_handshake(&[]);
    let bogus = x224::wrap_data(
        &SendData {
            initiator: USER_CHANNEL_ID,
            channel_id: IO_CHANNEL_ID,
            data: vec![0, 1, 2, 3],
            complete: true,
        }
        .encode_request(),
    );
    assert!(matches!(
        acceptor.step(&bogus),
        Err(ConnectorError::AlreadyFinished)
    ));
    assert!(
        acceptor.is_finished(),
        "AlreadyFinished must not poison Accepted into Rejected"
    );
    assert!(
        acceptor.begin_resize(1280, 720).is_ok(),
        "resize must still be possible after a stray post-Accepted step"
    );
}

#[test]
fn resize_finalization_ignores_static_channel_traffic() {
    let (mut acceptor, _accepted, _credentials) = run_full_handshake(&["cliprdr"]);
    acceptor.begin_resize(1920, 1080).unwrap();

    // Confirm Active first.
    let send_data = SendData {
        initiator: USER_CHANNEL_ID,
        channel_id: IO_CHANNEL_ID,
        data: confirm_active_fixture(),
        complete: true,
    };
    assert!(matches!(
        acceptor
            .step(&x224::wrap_data(&send_data.encode_request()))
            .unwrap()
            .event,
        AcceptorEvent::None
    ));

    // Interleaved cliprdr traffic (non-IO) must not abort finalization.
    let cliprdr_noise = SendData {
        initiator: USER_CHANNEL_ID,
        channel_id: IO_CHANNEL_ID + 1,
        data: vec![0xDE, 0xAD, 0xBE, 0xEF],
        complete: true,
    };
    let ignored = acceptor
        .step(&x224::wrap_data(&cliprdr_noise.encode_request()))
        .unwrap();
    assert!(matches!(ignored.event, AcceptorEvent::None));
    assert!(!acceptor.is_finished());

    let resized = drive_confirm_active_and_finalization_from_sync(&mut acceptor);
    assert_eq!(
        (resized.desktop_width, resized.desktop_height),
        (1920, 1080)
    );
}

#[test]
fn resize_finalization_accepts_batched_mcs_send_data() {
    let (mut acceptor, _accepted, _credentials) = run_full_handshake(&[]);
    acceptor.begin_resize(1920, 1080).unwrap();

    let confirm = SendData {
        initiator: USER_CHANNEL_ID,
        channel_id: IO_CHANNEL_ID,
        data: confirm_active_fixture(),
        complete: true,
    };
    assert!(matches!(
        acceptor
            .step(&x224::wrap_data(&confirm.encode_request()))
            .unwrap()
            .event,
        AcceptorEvent::None
    ));

    // mstsc often packs Synchronize + Cooperate + Request Control + FontList
    // into a single X.224 Data TPDU. Losing FontList here used to leave the
    // acceptor unfinished while the client already painted a blank canvas.
    let mut mcs_payload = Vec::new();
    for (pdu_type2, body) in [
        (
            ShareDataPduType::Synchronize,
            SynchronizePdu {
                target_user: USER_CHANNEL_ID,
            }
            .encode_body(),
        ),
        (
            ShareDataPduType::Control,
            ControlPdu {
                action: ControlPdu::COOPERATE,
                grant_id: 0,
                control_id: 0,
            }
            .encode_body(),
        ),
        (
            ShareDataPduType::Control,
            ControlPdu {
                action: ControlPdu::REQUEST_CONTROL,
                grant_id: 0,
                control_id: 0,
            }
            .encode_body(),
        ),
        (
            ShareDataPduType::FontList,
            FontPdu::font_map_default().encode_body(),
        ),
    ] {
        mcs_payload.extend(
            SendData {
                initiator: USER_CHANNEL_ID,
                channel_id: IO_CHANNEL_ID,
                data: data_pdu_bytes(pdu_type2, body),
                complete: true,
            }
            .encode_request(),
        );
    }

    let result = acceptor.step(&x224::wrap_data(&mcs_payload)).unwrap();
    assert!(
        matches!(result.event, AcceptorEvent::Accepted(_)),
        "batched finalization must reach Accepted in one step"
    );
    assert!(acceptor.is_finished());
}

/// Like `drive_confirm_active_and_finalization`, but skips Confirm Active
/// (caller already sent it) and starts at Synchronize.
fn drive_confirm_active_and_finalization_from_sync(acceptor: &mut Acceptor) -> AcceptedConnection {
    for (pdu_type2, body) in [
        (
            ShareDataPduType::Synchronize,
            SynchronizePdu {
                target_user: USER_CHANNEL_ID,
            }
            .encode_body(),
        ),
        (
            ShareDataPduType::Control,
            ControlPdu {
                action: ControlPdu::COOPERATE,
                grant_id: 0,
                control_id: 0,
            }
            .encode_body(),
        ),
        (
            ShareDataPduType::Control,
            ControlPdu {
                action: ControlPdu::REQUEST_CONTROL,
                grant_id: 0,
                control_id: 0,
            }
            .encode_body(),
        ),
        (
            ShareDataPduType::FontList,
            FontPdu::font_map_default().encode_body(),
        ),
    ] {
        let data_pdu = data_pdu_bytes(pdu_type2, body);
        let send_data = SendData {
            initiator: USER_CHANNEL_ID,
            channel_id: IO_CHANNEL_ID,
            data: data_pdu,
            complete: true,
        };
        let result = acceptor
            .step(&x224::wrap_data(&send_data.encode_request()))
            .unwrap();
        if let AcceptorEvent::Accepted(accepted) = result.event {
            assert!(acceptor.is_finished());
            return accepted;
        }
    }
    panic!("finalization did not reach Accepted");
}

#[test]
fn require_nla_rejects_ssl_and_accepts_hybrid() {
    let mut acceptor = Acceptor::new(1024, 768).with_require_nla(true);
    // Client requests SSL only (no HYBRID)
    let ssl_only_req = ConnectionRequest {
        cookie: None,
        flags: x224::RequestFlags(0),
        protocol: SecurityProtocol::SSL,
    }
    .encode();
    let result = acceptor.step(&ssl_only_req).unwrap();
    assert_eq!(result.event, AcceptorEvent::Rejected);
    let confirm = ConnectionConfirm::decode(&result.response).unwrap();
    assert_eq!(
        confirm,
        ConnectionConfirm::Failure {
            code: FailureCode::HYBRID_REQUIRED_BY_SERVER
        }
    );

    // Client requests HYBRID + SSL
    let mut acceptor = Acceptor::new(1024, 768).with_require_nla(true);
    let hybrid_req = ConnectionRequest {
        cookie: None,
        flags: x224::RequestFlags(0),
        protocol: SecurityProtocol::HYBRID | SecurityProtocol::SSL,
    }
    .encode();
    let result = acceptor.step(&hybrid_req).unwrap();
    assert_eq!(result.event, AcceptorEvent::TlsUpgrade);
    let confirm = ConnectionConfirm::decode(&result.response).unwrap();
    assert_eq!(
        confirm,
        ConnectionConfirm::Response {
            flags: x224::ResponseFlags(0),
            protocol: SecurityProtocol::HYBRID
        }
    );
}
