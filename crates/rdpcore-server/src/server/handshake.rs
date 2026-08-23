use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};

use rdpcore_connector::{AcceptedConnection, Acceptor, AcceptorEvent};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::auth_limit::AuthLimiter;
use crate::credentials::{CredentialValidator, Credentials};
use crate::credssp;
use crate::display::DesktopSize;
use crate::transport::read_tpkt_frame;

pub struct HandshakeParams<'a> {
    pub desktop: DesktopSize,
    pub require_nla: bool,
    pub tls_acceptor: &'a TlsAcceptor,
    pub tls_public_key: &'a [u8],
    pub nla_credentials: Option<&'a Credentials>,
    pub credential_validator: Option<&'a dyn CredentialValidator>,
    pub auth_limiter: &'a AuthLimiter,
}

/// Runs cleartext negotiation through MCS finalization. `Ok(None)`
/// means the connection ended for an expected reason (rejected
/// negotiation, failed auth, ...) and the caller should just return
/// `Ok(())`; errors are unexpected I/O/protocol failures.
pub async fn negotiate(
    mut tcp: TcpStream,
    peer_ip: IpAddr,
    params: HandshakeParams<'_>,
    authenticated: &AtomicBool,
) -> Result<
    Option<(
        tokio_rustls::server::TlsStream<TcpStream>,
        Acceptor,
        AcceptedConnection,
    )>,
    crate::error::ServerError,
> {
    let mut acceptor =
        Acceptor::new(params.desktop.width, params.desktop.height).with_require_nla(params.require_nla);

    // Connection Request/Confirm is always cleartext, even under
    // PROTOCOL_SSL / PROTOCOL_HYBRID - the TLS handshake only starts
    // after this.
    let frame = read_tpkt_frame(&mut tcp).await?;
    let result = acceptor.step(&frame).map_err(|e| {
        warn!("cleartext negotiation PDU error: {e}");
        e
    })?;
    tcp.write_all(&result.response).await?;
    tcp.flush().await?;
    match result.event {
        AcceptorEvent::TlsUpgrade => {
            if acceptor.requires_credssp() {
                info!("negotiation ok (NLA/HYBRID), starting TLS");
            } else {
                info!("negotiation ok (TLS), starting TLS");
            }
        }
        AcceptorEvent::Rejected => {
            warn!(
                "rejected at negotiation - client offered neither \
                 PROTOCOL_HYBRID nor PROTOCOL_SSL"
            );
            return Ok(None);
        }
        other => return Err(crate::error::ServerError::UnexpectedAcceptorEvent(other)),
    }

    let mut tls = match params.tls_acceptor.accept(tcp).await {
        Ok(stream) => {
            info!("TLS established");
            stream
        }
        Err(e) => {
            warn!("TLS handshake failed: {e}");
            return Err(e.into());
        }
    };

    // NLA: CredSSP runs on the TLS stream before MCS Connect Initial.
    let mut nla_authenticated_user: Option<String> = None;
    if acceptor.requires_credssp() {
        let Some(credentials) = params.nla_credentials.cloned() else {
            info!("client requested NLA but server has no NLA credentials configured");
            return Ok(None);
        };
        if params.tls_public_key.is_empty() {
            warn!("client requested NLA but server TLS public key is missing");
            return Ok(None);
        }
        info!("starting CredSSP (NTLMv2)");
        match credssp::run_credssp_nla(
            &mut tls,
            params.tls_public_key.to_vec(),
            credentials,
            "kmsrdp",
        )
        .await
        {
            Ok(user) => {
                info!("CredSSP succeeded for user {user:?}");
                nla_authenticated_user = Some(user);
            }
            Err(e) => {
                warn!("CredSSP failed: {e}");
                params.auth_limiter.record_failure(peer_ip);
                return Ok(None);
            }
        }
    }

    let accepted = loop {
        let frame = read_tpkt_frame(&mut tls).await.map_err(|e| {
            warn!(
                "read failed during handshake (waiting for {}): {e}",
                acceptor.handshake_phase()
            );
            e
        })?;
        if frame.first() != Some(&0x03) {
            debug!(
                "first byte during RDP handshake is 0x{:02x}, not TPKT 0x03",
                frame.first().copied().unwrap_or(0)
            );
        }
        let result = acceptor.step(&frame).map_err(|e| {
            warn!(
                "handshake PDU error while waiting for {}: {e}",
                acceptor.handshake_phase()
            );
            e
        })?;
        match result.event {
            AcceptorEvent::None | AcceptorEvent::TlsUpgrade => {
                if !result.response.is_empty() {
                    tls.write_all(&result.response).await?;
                    tls.flush().await?;
                }
            }
            AcceptorEvent::ClientInfoReceived(credentials) => {
                info!(
                    "client info user={:?} domain={:?} (nla={})",
                    credentials.username,
                    credentials.domain,
                    nla_authenticated_user.is_some()
                );
                let valid = if let Some(nla_user) = &nla_authenticated_user {
                    // CredSSP already proved the account. Bind the
                    // session to that identity: empty ClientInfo
                    // usernames (mstsc after NLA) are filled in as the
                    // NLA account; a non-empty mismatch is rejected.
                    crate::credentials::client_info_is_authorized(
                        Some(nla_user),
                        params.credential_validator,
                        &credentials.username,
                        &credentials.password,
                        &credentials.domain,
                    )
                } else {
                    crate::credentials::client_info_is_authorized(
                        None,
                        params.credential_validator,
                        &credentials.username,
                        &credentials.password,
                        &credentials.domain,
                    )
                };
                if !valid {
                    let password_hint = if credentials.password.is_empty() {
                        "password empty (client did not send one — enter the password in the client, or enable NLA)"
                    } else {
                        "password non-empty but does not match"
                    };
                    warn!(
                        "rejecting invalid credentials for user {:?} domain {:?} ({password_hint})",
                        credentials.username, credentials.domain
                    );
                    params.auth_limiter.record_failure(peer_ip);
                    acceptor.reject_client_info();
                    return Ok(None);
                }
                params.auth_limiter.record_success(peer_ip);
                authenticated.store(true, Ordering::Relaxed);
                if !result.response.is_empty() {
                    tls.write_all(&result.response).await?;
                }
                tls.write_all(&acceptor.approve_client_info()?).await?;
                tls.flush().await?;
                info!("credentials accepted, sent Demand Active");
            }
            AcceptorEvent::Accepted(accepted) => {
                if !result.response.is_empty() {
                    tls.write_all(&result.response).await?;
                    tls.flush().await?;
                }
                info!("handshake complete");
                break accepted;
            }
            AcceptorEvent::Rejected => {
                warn!("rejected during handshake");
                return Ok(None);
            }
        }
    };

    Ok(Some((tls, acceptor, accepted)))
}
