use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConnectorError {
    #[error("PDU decode error: {0}")]
    Decode(#[from] rdpcore_pdu::DecodeError),
    /// `Acceptor::step` was called again after reaching `Accepted`/`Rejected`.
    #[error("Acceptor::step called after the connection sequence finished")]
    AlreadyFinished,
    /// `Acceptor::begin_resize` was called before the connection first
    /// reached `Accepted`, or while a previous resize is still in flight.
    #[error("Acceptor is not ready for this operation")]
    NotReady,
}
