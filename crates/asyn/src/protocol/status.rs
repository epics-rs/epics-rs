pub use crate::error::AsynStatus;

/// Protocol-level reply status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReplyStatus {
    Ok,
    Timeout,
    Overflow,
    Error,
    Disconnected,
    Disabled,
}

impl From<AsynStatus> for ReplyStatus {
    fn from(s: AsynStatus) -> Self {
        match s {
            AsynStatus::Success => Self::Ok,
            AsynStatus::Timeout => Self::Timeout,
            AsynStatus::Overflow => Self::Overflow,
            AsynStatus::Error => Self::Error,
            AsynStatus::Disconnected => Self::Disconnected,
            AsynStatus::Disabled => Self::Disabled,
        }
    }
}

impl From<ReplyStatus> for AsynStatus {
    fn from(s: ReplyStatus) -> Self {
        match s {
            ReplyStatus::Ok => Self::Success,
            ReplyStatus::Timeout => Self::Timeout,
            ReplyStatus::Overflow => Self::Overflow,
            ReplyStatus::Error => Self::Error,
            ReplyStatus::Disconnected => Self::Disconnected,
            ReplyStatus::Disabled => Self::Disabled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_status() {
        let statuses = [
            AsynStatus::Success,
            AsynStatus::Timeout,
            AsynStatus::Overflow,
            AsynStatus::Error,
            AsynStatus::Disconnected,
            AsynStatus::Disabled,
        ];
        for s in statuses {
            let rs: ReplyStatus = s.into();
            let back: AsynStatus = rs.into();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn serde_roundtrip() {
        let rs = ReplyStatus::Timeout;
        let json = serde_json::to_string(&rs).unwrap();
        let back: ReplyStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(rs, back);
    }
}
