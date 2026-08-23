//! Typed failures on the per-connection session path.
//!
//! Encode/send helpers used to return `Result<T, ()>`, which made writer
//! close indistinguishable from a cancelled blocking encode. Callers treat
//! [`SessionError::WriterClosed`] as a clean disconnect.

use rdpcore_connector::{AcceptorEvent, ConnectorError};

#[derive(Debug, thiserror::Error)]
pub enum DisplayError {
    #[error("display update stream closed or unavailable")]
    Closed,
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("connection writer closed")]
    WriterClosed,
    #[error("bitmap encode task cancelled or panicked")]
    EncodeJoin,
    #[error("GFX encode task cancelled or panicked")]
    GfxEncodeJoin,
    #[error("GFX dynamic channel is not available")]
    GfxChannelMissing,
    #[error(transparent)]
    Display(#[from] DisplayError),
    #[error(transparent)]
    Pdu(#[from] rdpcore_pdu::DecodeError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Connector(#[from] ConnectorError),
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Connector(#[from] ConnectorError),
    #[error(transparent)]
    Display(#[from] DisplayError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("unexpected acceptor event before TLS upgrade: {0:?}")]
    UnexpectedAcceptorEvent(AcceptorEvent),
    #[error("{0}")]
    Other(String),
}

/// Maps a session-loop result onto `Result<(), ServerError>`.
/// A closed writer is a normal client disconnect, not a server failure.
pub(crate) fn finish_session(result: Result<(), SessionError>) -> Result<(), ServerError> {
    match result {
        Ok(()) => Ok(()),
        Err(SessionError::WriterClosed) => Ok(()),
        Err(e) => Err(ServerError::Session(e)),
    }
}
