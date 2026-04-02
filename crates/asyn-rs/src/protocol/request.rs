use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::command::PortCommand;
use super::types::QueuePriority;

/// Serializable queue priority for the protocol layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtocolPriority {
    Low,
    Medium,
    High,
    Connect,
}

impl From<QueuePriority> for ProtocolPriority {
    fn from(p: QueuePriority) -> Self {
        match p {
            QueuePriority::Low => Self::Low,
            QueuePriority::Medium => Self::Medium,
            QueuePriority::High => Self::High,
            QueuePriority::Connect => Self::Connect,
        }
    }
}

impl From<ProtocolPriority> for QueuePriority {
    fn from(p: ProtocolPriority) -> Self {
        match p {
            ProtocolPriority::Low => Self::Low,
            ProtocolPriority::Medium => Self::Medium,
            ProtocolPriority::High => Self::High,
            ProtocolPriority::Connect => Self::Connect,
        }
    }
}

/// Protocol-level request metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestMeta {
    pub request_id: u64,
    pub port_name: String,
    pub addr: i32,
    pub reason: usize,
    pub timeout_ms: u64,
    pub priority: ProtocolPriority,
    pub block_token: Option<u64>,
}

impl RequestMeta {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
}

/// Full protocol request envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortRequest {
    pub meta: RequestMeta,
    pub command: PortCommand,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip() {
        let req = PortRequest {
            meta: RequestMeta {
                request_id: 1,
                port_name: "test_port".into(),
                addr: 0,
                reason: 3,
                timeout_ms: 5000,
                priority: ProtocolPriority::High,
                block_token: None,
            },
            command: PortCommand::Int32Read,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: PortRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn priority_roundtrip() {
        for p in [
            QueuePriority::Low,
            QueuePriority::Medium,
            QueuePriority::High,
            QueuePriority::Connect,
        ] {
            let proto: ProtocolPriority = p.into();
            let back: QueuePriority = proto.into();
            assert_eq!(p, back);
        }
    }
}
