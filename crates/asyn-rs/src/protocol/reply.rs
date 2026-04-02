use serde::{Deserialize, Serialize};

use super::status::ReplyStatus;
use super::value::{AlarmMeta, ParamValue, Timestamp};

/// Typed reply payload (minimal set).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReplyPayload {
    Ack,
    Value(ParamValue),
    OctetData { data: Vec<u8>, nbytes: usize },
    Subscribed { subscription_id: u64 },
    Error { code: ReplyStatus, detail: String },
}

/// Reply envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortReply {
    pub request_id: u64,
    pub payload: ReplyPayload,
    pub alarm: Option<AlarmMeta>,
    pub timestamp: Option<Timestamp>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_ack() {
        let reply = PortReply {
            request_id: 1,
            payload: ReplyPayload::Ack,
            alarm: None,
            timestamp: None,
        };
        let json = serde_json::to_string(&reply).unwrap();
        let back: PortReply = serde_json::from_str(&json).unwrap();
        assert_eq!(reply, back);
    }

    #[test]
    fn serde_roundtrip_value() {
        let reply = PortReply {
            request_id: 42,
            payload: ReplyPayload::Value(ParamValue::Int32(100)),
            alarm: Some(AlarmMeta {
                status: 1,
                severity: 2,
            }),
            timestamp: Some(Timestamp(1_000_000)),
        };
        let json = serde_json::to_string(&reply).unwrap();
        let back: PortReply = serde_json::from_str(&json).unwrap();
        assert_eq!(reply, back);
    }

    #[test]
    fn serde_roundtrip_octet_data() {
        let reply = PortReply {
            request_id: 3,
            payload: ReplyPayload::OctetData {
                data: vec![0x48, 0x65, 0x6c, 0x6c, 0x6f],
                nbytes: 5,
            },
            alarm: None,
            timestamp: None,
        };
        let json = serde_json::to_string(&reply).unwrap();
        let back: PortReply = serde_json::from_str(&json).unwrap();
        assert_eq!(reply, back);
    }

    #[test]
    fn serde_roundtrip_error() {
        let reply = PortReply {
            request_id: 99,
            payload: ReplyPayload::Error {
                code: ReplyStatus::Timeout,
                detail: "timed out waiting".into(),
            },
            alarm: None,
            timestamp: None,
        };
        let json = serde_json::to_string(&reply).unwrap();
        let back: PortReply = serde_json::from_str(&json).unwrap();
        assert_eq!(reply, back);
    }

    #[test]
    fn serde_roundtrip_subscribed() {
        let reply = PortReply {
            request_id: 7,
            payload: ReplyPayload::Subscribed {
                subscription_id: 42,
            },
            alarm: None,
            timestamp: None,
        };
        let json = serde_json::to_string(&reply).unwrap();
        let back: PortReply = serde_json::from_str(&json).unwrap();
        assert_eq!(reply, back);
    }

    #[test]
    fn serde_all_payload_variants() {
        let payloads = vec![
            ReplyPayload::Ack,
            ReplyPayload::Value(ParamValue::Float64(2.718)),
            ReplyPayload::OctetData {
                data: vec![1],
                nbytes: 1,
            },
            ReplyPayload::Subscribed { subscription_id: 0 },
            ReplyPayload::Error {
                code: ReplyStatus::Error,
                detail: "fail".into(),
            },
        ];
        for payload in payloads {
            let json = serde_json::to_string(&payload).unwrap();
            let back: ReplyPayload = serde_json::from_str(&json).unwrap();
            assert_eq!(payload, back);
        }
    }
}
