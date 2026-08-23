//! Typed failures on the per-connection session path.
//!
//! Encode/send helpers used to return `Result<T, ()>`, which made writer
//! close indistinguishable from a cancelled blocking encode. Callers treat
//! [`SessionError::WriterClosed`] as a clean disconnect.

use rdpcore_connector::ConnectorError;

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
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Connector(#[from] ConnectorError),
}

/// Maps a session-loop result onto the accept-task `anyhow::Result`.
/// A closed writer is a normal client disconnect, not a server failure.
pub(crate) fn finish_session(result: Result<(), SessionError>) -> anyhow::Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(SessionError::WriterClosed) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
