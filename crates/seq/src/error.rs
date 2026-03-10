use thiserror::Error;

/// Sequencer-level operation outcome (normal flow).
/// Matches C sequencer pvStat semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum PvStat {
    Ok = 0,
    Error = -1,
    Timeout = -2,
    Disconnected = -3,
}

/// Operation completion record for pvGet/pvPut results.
#[derive(Debug, Clone)]
pub struct PvOpResult {
    pub stat: PvStat,
    pub severity: i16,
    pub message: Option<String>,
}

impl Default for PvOpResult {
    fn default() -> Self {
        Self {
            stat: PvStat::Ok,
            severity: 0,
            message: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum SeqError {
    #[error("channel not connected: {0}")]
    NotConnected(String),

    #[error("channel access error: {0}")]
    CaError(#[from] epics_base_rs::error::CaError),

    #[error("invalid channel id: {0}")]
    InvalidChannelId(usize),

    #[error("invalid event flag id: {0}")]
    InvalidEventFlagId(usize),

    #[error("invalid state id: {0}")]
    InvalidStateId(usize),

    #[error("type mismatch for channel {channel}: expected {expected}, got {actual}")]
    TypeMismatch {
        channel: String,
        expected: String,
        actual: String,
    },

    #[error("program shutdown")]
    Shutdown,

    #[error("{0}")]
    Other(String),
}

pub type SeqResult<T> = Result<T, SeqError>;
