use thiserror::Error;

#[derive(Error, Debug)]
pub enum PvaError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("timeout waiting for response")]
    Timeout,

    #[error("channel not found: {0}")]
    ChannelNotFound(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("unsupported type code: {0:#04x}")]
    UnsupportedType(u8),

    #[error("connection refused")]
    ConnectionRefused,

    #[error("invalid value: {0}")]
    InvalidValue(String),
}

pub type PvaResult<T> = Result<T, PvaError>;
