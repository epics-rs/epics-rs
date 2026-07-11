use serde::{Deserialize, Serialize};

use super::status::ReplyStatus;

/// Protocol-level error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProtocolError {
    pub status: ReplyStatus,
    pub message: String,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.status, self.message)
    }
}

impl std::error::Error for ProtocolError {}

impl From<crate::error::AsynError> for ProtocolError {
    fn from(e: crate::error::AsynError) -> Self {
        use crate::error::AsynError;
        // Take the status from the single owner (`AsynError::status()`), not
        // from a variant match: every status-carrying variant — including
        // `PartialRead`, which reports a genuine timeout/disconnect alongside
        // the bytes already transferred — must reach the wire with its real
        // status, not a flattened `Error`.
        let status = e.status().into();
        let message = match e {
            AsynError::Status { message, .. } | AsynError::PartialRead { message, .. } => message,
            other => other.to_string(),
        };
        Self { status, message }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip() {
        let err = ProtocolError {
            status: ReplyStatus::Timeout,
            message: "connection timed out".into(),
        };
        let json = serde_json::to_string(&err).unwrap();
        let back: ProtocolError = serde_json::from_str(&json).unwrap();
        assert_eq!(err, back);
    }

    #[test]
    fn display() {
        let err = ProtocolError {
            status: ReplyStatus::Error,
            message: "bad thing".into(),
        };
        assert_eq!(err.to_string(), "Error: bad thing");
    }
}
