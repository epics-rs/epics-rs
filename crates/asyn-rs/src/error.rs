/// Status codes matching C asyn's asynStatus enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsynStatus {
    Success,
    Timeout,
    Overflow,
    Error,
    Disconnected,
    Disabled,
}

/// Error type for asyn-rs operations.
#[derive(Debug, thiserror::Error)]
pub enum AsynError {
    #[error("asyn: {status:?} - {message}")]
    Status { status: AsynStatus, message: String },

    #[error("port not found: {0}")]
    PortNotFound(String),

    #[error("param not found: {0}")]
    ParamNotFound(String),

    #[error("param index out of range: {0}")]
    ParamIndexOutOfRange(usize),

    #[error("type mismatch: expected {expected}, got {actual}")]
    TypeMismatch {
        expected: &'static str,
        actual: &'static str,
    },

    #[error("interface not supported: {0}")]
    InterfaceNotSupported(String),

    #[error("address out of range: {0}")]
    AddressOutOfRange(i32),

    #[error("already subscribed")]
    AlreadySubscribed,

    #[error("option not found: {0}")]
    OptionNotFound(String),

    #[error("invalid link syntax: {0}")]
    InvalidLinkSyntax(String),

    #[error("downcast failed: stored type does not match requested type")]
    DowncastFailed,

    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
}

pub type AsynResult<T> = Result<T, AsynError>;
