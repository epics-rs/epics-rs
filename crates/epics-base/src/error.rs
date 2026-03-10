use thiserror::Error;

#[derive(Error, Debug)]
pub enum CaError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("timeout waiting for response")]
    Timeout,

    #[error("channel not found: {0}")]
    ChannelNotFound(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("unsupported DBR type: {0}")]
    UnsupportedType(u16),

    #[error("write failed: ECA status {0:#06x}")]
    WriteFailed(u32),

    #[error("field not found: {0}")]
    FieldNotFound(String),

    #[error("field is read-only: {0}")]
    ReadOnlyField(String),

    #[error("type mismatch for field {0}")]
    TypeMismatch(String),

    #[error("invalid value: {0}")]
    InvalidValue(String),

    #[error("put disabled (DISP=1) for field {0}")]
    PutDisabled(String),

    #[error("link error: {0}")]
    LinkError(String),

    #[error("DB parse error at line {line}, column {column}: {message}")]
    DbParseError {
        line: usize,
        column: usize,
        message: String,
    },

    #[error("calc error: {0}")]
    CalcError(String),

    #[error("channel disconnected")]
    Disconnected,

    #[error("client shut down")]
    Shutdown,
}

impl CaError {
    pub fn to_eca_status(&self) -> u32 {
        match self {
            CaError::Timeout => crate::protocol::ECA_TIMEOUT,
            CaError::ReadOnlyField(_) => crate::protocol::ECA_NOWTACCESS,
            CaError::PutDisabled(_) => crate::protocol::ECA_PUTFAIL,
            CaError::TypeMismatch(_) => crate::protocol::ECA_BADTYPE,
            CaError::UnsupportedType(_) => crate::protocol::ECA_BADTYPE,
            CaError::InvalidValue(_) => crate::protocol::ECA_BADTYPE,
            CaError::FieldNotFound(_) => crate::protocol::ECA_PUTFAIL,
            _ => crate::protocol::ECA_PUTFAIL,
        }
    }
}

pub type CaResult<T> = Result<T, CaError>;
