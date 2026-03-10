use crate::protocol::ProtocolError;

/// Transport-level error.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    #[error("channel closed")]
    ChannelClosed,

    #[error("not connected")]
    NotConnected,

    #[error("request cancelled")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

impl From<crate::error::AsynError> for TransportError {
    fn from(e: crate::error::AsynError) -> Self {
        Self::Protocol(ProtocolError::from(e))
    }
}
