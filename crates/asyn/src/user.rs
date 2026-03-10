use std::any::Any;
use std::time::{Duration, SystemTime};

use crate::port::QueuePriority;

/// Per-request context, equivalent to C asyn's asynUser.
///
/// `timeout` is meaningful only when a driver performs actual I/O synchronously.
/// Cache-based default implementations ignore it (return immediately).
pub struct AsynUser {
    /// Parameter index (called "reason" in C asyn).
    pub reason: usize,
    /// Sub-address for multi-device ports. Always 0 for single-device ports.
    pub addr: i32,
    /// I/O timeout in seconds. Only meaningful for drivers that perform real I/O.
    pub timeout: Duration,
    /// Queue priority.
    pub priority: QueuePriority,
    /// Timestamp set by the driver.
    pub timestamp: Option<SystemTime>,
    /// Alarm status.
    pub alarm_status: u16,
    /// Alarm severity.
    pub alarm_severity: u16,
    /// User-defined data.
    pub user_data: Option<Box<dyn Any + Send>>,
    /// Token for BlockProcess ownership. When a port is blocked, only requests
    /// with a matching block_token (or UnblockProcess) are dequeued.
    pub block_token: Option<u64>,
}

impl Default for AsynUser {
    fn default() -> Self {
        Self {
            reason: 0,
            addr: 0,
            timeout: Duration::from_secs(1),
            priority: QueuePriority::default(),
            timestamp: None,
            alarm_status: 0,
            alarm_severity: 0,
            user_data: None,
            block_token: None,
        }
    }
}

impl AsynUser {
    pub fn new(reason: usize) -> Self {
        Self {
            reason,
            ..Default::default()
        }
    }

    pub fn with_addr(mut self, addr: i32) -> Self {
        self.addr = addr;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_default() {
        let u = AsynUser::default();
        assert_eq!(u.reason, 0);
        assert_eq!(u.addr, 0);
        assert_eq!(u.timeout, Duration::from_secs(1));
    }

    #[test]
    fn test_user_builder() {
        let u = AsynUser::new(42)
            .with_addr(3)
            .with_timeout(Duration::from_millis(500));
        assert_eq!(u.reason, 42);
        assert_eq!(u.addr, 3);
        assert_eq!(u.timeout, Duration::from_millis(500));
    }
}
