use thiserror::Error;

#[derive(Error, Debug)]
pub enum PvaError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("timeout waiting for response")]
    Timeout,

    /// A `wait` was woken by [`crate::client_native::PvaOperation::interrupt`]
    /// rather than by a deadline. pvxs distinguishes
    /// `Interrupted` from `Timeout`, and conflating them hid the cause
    /// (operator-driven wake-up vs. real deadline) from callers. The
    /// underlying operation keeps running and its result stays
    /// recoverable by a later `wait`.
    #[error("operation wait interrupted")]
    Interrupted,

    #[error("channel not found: {0}")]
    ChannelNotFound(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("connection refused")]
    ConnectionRefused,

    #[error("invalid value: {0}")]
    InvalidValue(String),

    #[error("decode error: {0}")]
    Decode(String),
}

pub type PvaResult<T> = Result<T, PvaError>;
