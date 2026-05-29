use thiserror::Error;

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("channel not found: {0}")]
    ChannelNotFound(String),

    #[error("record not found: {0}")]
    RecordNotFound(String),

    #[error("field not found: {field} on record {record}")]
    FieldNotFound { record: String, field: String },

    #[error("type mismatch: expected {expected}, got {got}")]
    TypeMismatch { expected: String, got: String },

    #[error("put rejected: {0}")]
    PutRejected(String),

    #[error("monitor not started")]
    MonitorNotStarted,

    #[error("group config parse error: {0}")]
    GroupConfigError(String),

    #[error("channel filter parse error: {0}")]
    ChannelFilterError(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type BridgeResult<T> = Result<T, BridgeError>;
