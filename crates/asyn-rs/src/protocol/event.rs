use serde::{Deserialize, Serialize};

use super::value::{ParamValue, Timestamp};
use crate::exception::AsynException;

/// Protocol-level event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EventPayload {
    ValueChanged {
        reason: usize,
        addr: i32,
        value: ParamValue,
    },
    Exception {
        exception: ProtocolException,
        port_name: String,
        addr: i32,
    },
}

/// Serializable mirror of `AsynException`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProtocolException {
    Connect,
    Enable,
    AutoConnect,
    TraceMask,
    TraceIoMask,
    TraceInfoMask,
    TraceFile,
    TraceIoTruncateSize,
    Shutdown,
}

impl From<AsynException> for ProtocolException {
    fn from(e: AsynException) -> Self {
        match e {
            AsynException::Connect => Self::Connect,
            AsynException::Enable => Self::Enable,
            AsynException::AutoConnect => Self::AutoConnect,
            AsynException::TraceMask => Self::TraceMask,
            AsynException::TraceIoMask => Self::TraceIoMask,
            AsynException::TraceInfoMask => Self::TraceInfoMask,
            AsynException::TraceFile => Self::TraceFile,
            AsynException::TraceIoTruncateSize => Self::TraceIoTruncateSize,
            AsynException::Shutdown => Self::Shutdown,
        }
    }
}

impl From<ProtocolException> for AsynException {
    fn from(e: ProtocolException) -> Self {
        match e {
            ProtocolException::Connect => Self::Connect,
            ProtocolException::Enable => Self::Enable,
            ProtocolException::AutoConnect => Self::AutoConnect,
            ProtocolException::TraceMask => Self::TraceMask,
            ProtocolException::TraceIoMask => Self::TraceIoMask,
            ProtocolException::TraceInfoMask => Self::TraceInfoMask,
            ProtocolException::TraceFile => Self::TraceFile,
            ProtocolException::TraceIoTruncateSize => Self::TraceIoTruncateSize,
            ProtocolException::Shutdown => Self::Shutdown,
        }
    }
}

/// Event filter for subscriptions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EventFilter {
    pub reason: Option<usize>,
    pub addr: Option<i32>,
}

/// Full protocol event envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortEvent {
    pub port_name: String,
    pub payload: EventPayload,
    pub timestamp: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_value_changed() {
        let evt = PortEvent {
            port_name: "myport".into(),
            payload: EventPayload::ValueChanged {
                reason: 0,
                addr: 1,
                value: ParamValue::Float64(2.5),
            },
            timestamp: Timestamp(1_000_000),
        };
        let json = serde_json::to_string(&evt).unwrap();
        let back: PortEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(evt, back);
    }

    #[test]
    fn serde_exception() {
        let evt = PortEvent {
            port_name: "p".into(),
            payload: EventPayload::Exception {
                exception: ProtocolException::Connect,
                port_name: "p".into(),
                addr: -1,
            },
            timestamp: Timestamp(0),
        };
        let json = serde_json::to_string(&evt).unwrap();
        let back: PortEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(evt, back);
    }

    #[test]
    fn exception_roundtrip() {
        let exceptions = [
            AsynException::Connect,
            AsynException::Enable,
            AsynException::AutoConnect,
            AsynException::Shutdown,
        ];
        for e in exceptions {
            let proto: ProtocolException = e.into();
            let back: AsynException = proto.into();
            assert_eq!(e, back);
        }
    }

    #[test]
    fn event_filter_serde() {
        let filter = EventFilter {
            reason: Some(3),
            addr: Some(0),
        };
        let json = serde_json::to_string(&filter).unwrap();
        let back: EventFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(filter, back);
    }
}
