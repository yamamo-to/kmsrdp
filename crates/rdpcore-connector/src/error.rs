use core::fmt;

#[derive(Debug)]
pub enum ConnectorError {
    Decode(rdpcore_pdu::DecodeError),
    /// `Acceptor::step` was called again after reaching `Accepted`/`Rejected`.
    AlreadyFinished,
    /// `Acceptor::begin_resize` was called before the connection first
    /// reached `Accepted`, or while a previous resize is still in flight.
    NotReady,
}

impl fmt::Display for ConnectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(e) => write!(f, "PDU decode error: {e}"),
            Self::AlreadyFinished => write!(f, "Acceptor::step called after the connection sequence finished"),
            Self::NotReady => write!(f, "Acceptor is not ready for this operation"),
        }
    }
}

impl core::error::Error for ConnectorError {}

impl From<rdpcore_pdu::DecodeError> for ConnectorError {
    fn from(e: rdpcore_pdu::DecodeError) -> Self {
        Self::Decode(e)
    }
}
