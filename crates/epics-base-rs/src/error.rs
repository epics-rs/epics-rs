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

// ECA status constants (originally from protocol.rs, now in epics-ca-rs)
const ECA_TIMEOUT: u32 = 80; // defmsg(CA_K_WARNING, 10)
const ECA_NOWTACCESS: u32 = 376; // defmsg(CA_K_WARNING, 47)
const ECA_PUTFAIL: u32 = 160; // defmsg(CA_K_WARNING, 20)
const ECA_BADTYPE: u32 = 114; // defmsg(CA_K_ERROR, 14)

impl CaError {
    pub fn to_eca_status(&self) -> u32 {
        match self {
            CaError::Timeout => ECA_TIMEOUT,
            CaError::ReadOnlyField(_) => ECA_NOWTACCESS,
            CaError::PutDisabled(_) => ECA_PUTFAIL,
            CaError::TypeMismatch(_) => ECA_BADTYPE,
            CaError::UnsupportedType(_) => ECA_BADTYPE,
            CaError::InvalidValue(_) => ECA_BADTYPE,
            CaError::FieldNotFound(_) => ECA_PUTFAIL,
            _ => ECA_PUTFAIL,
        }
    }
}

pub type CaResult<T> = Result<T, CaError>;
