use std::any::Any;
use std::time::{Duration, SystemTime};

use crate::port::QueuePriority;

/// The timeout every C asyn entry point supplies when the operator gives none:
/// `parseLink` writes `pasynUser->timeout = 1.0` for an `@asyn(port)` or
/// `@asyn(port,addr)` link (asynEpicsUtils.c:109,121), and it is also the
/// bounded value [`timeout_from_secs`] substitutes for a timeout that
/// [`AsynUser::timeout`] cannot represent.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1);

/// The single owner of every `f64 seconds` → [`AsynUser::timeout`] conversion.
///
/// C carries the timeout as a `double` and accepts operator values this type
/// cannot hold: `strtod` parses the negative "wait forever" sentinel
/// (`@asyn(PORT,0,-1)`, asynEpicsUtils.c:125), and a record's TMOT field can
/// hold a negative, NaN or infinite double. [`AsynUser::timeout`] is an
/// unsigned `Duration` — the signed-off framework deviation **DRV-42**, which
/// keeps every blocking driver operation bounded so a stuck device cannot
/// wedge the port actor thread — so none of those values is representable.
///
/// Every such value therefore takes the bounded [`DEFAULT_TIMEOUT`]. Callers
/// must not construct a `Duration` from an operator-supplied `f64` themselves:
/// `Duration::from_secs_f64` *panics* on exactly the inputs C accepts, so
/// routing the conversion through this function is what makes the panic
/// structurally impossible rather than merely unobserved.
pub fn timeout_from_secs(secs: f64) -> Duration {
    Duration::try_from_secs_f64(secs).unwrap_or(DEFAULT_TIMEOUT)
}

/// Per-request context, equivalent to C asyn's asynUser.
///
/// `timeout` is meaningful only when a driver performs actual I/O synchronously.
/// Cache-based default implementations ignore it (return immediately).
pub struct AsynUser {
    /// Parameter index (called "reason" in C asyn).
    pub reason: usize,
    /// Sub-address for multi-device ports. Always 0 for single-device ports.
    pub addr: i32,
    /// I/O timeout. Only meaningful for drivers that perform real I/O.
    ///
    /// This is an unsigned `Duration`, so unlike C asyn's `double`
    /// `pasynUser->timeout` it cannot carry the negative "wait forever"
    /// sentinel (a finite timeout is always supplied). A deliberate
    /// framework-wide divergence: every blocking driver operation is bounded,
    /// so a stuck device cannot wedge the port actor thread indefinitely.
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
