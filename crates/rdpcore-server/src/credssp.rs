//! CredSSP (NLA) exchange after TLS when the negotiation selected
//! `PROTOCOL_HYBRID`. Uses the `sspi` crate for NTLMv2 + TSRequest framing;
//! this module only wires it to the TLS stream and our configured credentials.

use std::io;

use sspi::credssp::{
    CredSspServer, CredentialsProxy, ServerError, ServerMode, ServerState, TsRequest,
};
use sspi::generator::{Generator, GeneratorState, NetworkRequest};
use sspi::ntlm::NtlmConfig;
use sspi::{AuthIdentity, Username};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::credentials::{Credentials, normalize_client_identity};

type CredsspProcessGenerator<'a> =
    Generator<'a, NetworkRequest, sspi::Result<Vec<u8>>, Result<ServerState, ServerError>>;

/// Looks up the single configured account for NTLM verification.
struct ConfiguredCredentials {
    expected: Credentials,
}

impl CredentialsProxy for ConfiguredCredentials {
    type AuthenticationData = AuthIdentity;

    fn auth_data_by_user(&mut self, username: &Username) -> io::Result<Self::AuthenticationData> {
        let (account, domain) = match username.parts() {
            sspi::UsernameParts::DownLevelLogonName(parts) => {
                (parts.account_name(), parts.netbios_domain().unwrap_or(""))
            }
            sspi::UsernameParts::UserPrincipalName(parts) => (parts.account_name(), parts.suffix()),
        };
        let (client_domain, client_user) = normalize_client_identity(account, domain);

        if !client_user.eq_ignore_ascii_case(&self.expected.username) {
            return Err(io::Error::other(format!(
                "unknown username {account:?} (domain {domain:?})"
            )));
        }
        if let Some(expected_domain) = &self.expected.domain
            && !client_domain.eq_ignore_ascii_case(expected_domain)
        {
            return Err(io::Error::other(format!(
                "domain mismatch for user {account:?}"
            )));
        }

        // Preserve the client's Username (domain form) so NTLM challenge
        // verification uses the same identity the client authenticated as.
        Ok(AuthIdentity {
            username: username.clone(),
            password: self.expected.password.clone().into(),
        })
    }

    fn auth_data(&mut self) -> io::Result<Vec<Self::AuthenticationData>> {
        let username = Username::parse(&self.expected.username)
            .map_err(|e| io::Error::other(format!("invalid configured username: {e}")))?;
        Ok(vec![AuthIdentity {
            username,
            password: self.expected.password.clone().into(),
        }])
    }
}

/// Drive CredSSP until [`ServerState::Finished`] or an error.
///
/// Returns the authenticated username (account name) on success.
pub async fn run_credssp_nla<S>(
    stream: &mut S,
    public_key: Vec<u8>,
    expected: Credentials,
    computer_name: &str,
) -> anyhow::Result<String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let proxy = ConfiguredCredentials { expected };
    let mut server = CredSspServer::new(
        public_key,
        proxy,
        ServerMode::Ntlm(NtlmConfig::new(computer_name.to_owned())),
    )
    .map_err(|e| anyhow::anyhow!("CredSSP server init failed: {e}"))?;

    loop {
        let request = read_ts_request(stream).await?;
        let mut generator = server.process(request);
        let result = resolve_credssp_generator(&mut generator)
            .map_err(|e| anyhow::anyhow!("CredSSP failed: {}", e.error))?;

        match result {
            ServerState::ReplyNeeded(ts_request) => {
                write_ts_request(stream, &ts_request).await?;
            }
            ServerState::Finished(identity) => {
                return Ok(identity.username.account_name().to_owned());
            }
        }
    }
}

fn resolve_credssp_generator(
    generator: &mut CredsspProcessGenerator<'_>,
) -> Result<ServerState, ServerError> {
    // NTLM CredSSP never yields NetworkRequest (no KDC); resolve synchronously.
    let state = generator.start();
    match state {
        GeneratorState::Suspended(_) => Err(ServerError {
            ts_request: None,
            error: sspi::Error::new(
                sspi::ErrorKind::UnsupportedFunction,
                "CredSSP NTLM unexpectedly requested a network round-trip",
            ),
        }),
        GeneratorState::Completed(result) => result,
    }
}

async fn read_ts_request<S: AsyncRead + Unpin>(stream: &mut S) -> anyhow::Result<TsRequest> {
    let mut buf = Vec::with_capacity(256);
    let total = loop {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await?;
        buf.push(byte[0]);
        match TsRequest::read_length(&buf[..]) {
            Ok(len) => break len,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => continue,
            Err(e) => {
                return Err(anyhow::anyhow!("CredSSP TSRequest length: {e}"));
            }
        }
    };
    if buf.len() < total {
        let already = buf.len();
        buf.resize(total, 0);
        stream.read_exact(&mut buf[already..]).await?;
    }
    TsRequest::from_buffer(&buf[..]).map_err(|e| anyhow::anyhow!("CredSSP TSRequest decode: {e}"))
}

async fn write_ts_request<S: AsyncWrite + Unpin>(
    stream: &mut S,
    ts_request: &TsRequest,
) -> anyhow::Result<()> {
    let mut buf = Vec::with_capacity(usize::from(ts_request.buffer_len()));
    ts_request
        .encode_ts_request(&mut buf)
        .map_err(|e| anyhow::anyhow!("CredSSP TSRequest encode: {e}"))?;
    stream.write_all(&buf).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sspi::Username;

    #[test]
    fn proxy_accepts_matching_username_case_insensitive() {
        let mut proxy = ConfiguredCredentials {
            expected: Credentials {
                username: "kmsrdp".into(),
                password: "secret".into(),
                domain: None,
            },
        };
        let user = Username::parse("KMSRDP").unwrap();
        let identity = proxy.auth_data_by_user(&user).unwrap();
        assert_eq!(identity.username.account_name(), "KMSRDP");
        assert_eq!(identity.password.as_ref().as_str(), "secret");
    }

    #[test]
    fn proxy_rejects_unknown_username() {
        let mut proxy = ConfiguredCredentials {
            expected: Credentials {
                username: "kmsrdp".into(),
                password: "secret".into(),
                domain: None,
            },
        };
        let user = Username::parse("other").unwrap();
        assert!(proxy.auth_data_by_user(&user).is_err());
    }
}
