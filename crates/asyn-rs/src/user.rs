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

/// C `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED` (asynDriver.h:105, aliasing
/// `ASYN_REASON_RESERVED_LOW`): the reason value a caller stamps on its
/// [`AsynUser`] to tell the queue gate that this request must be queued on a
/// port that is **not connected**.
///
/// It is not a driver parameter: the reserved band `0x70000000..=0x7FFFFFFF` is
/// carved out of the reason space precisely so no `drvUser` index can collide
/// with it. The requests that carry it (`asynSetOption` / `asynSetEos` from
/// iocsh, the record's HOSTINFO put and its connect-time option readback) are
/// the ones whose whole point is to reconfigure a dead line — a serial port
/// before the crate is powered on, an IP port aimed at the wrong host — and the
/// ops they run (`setOption`/`getOption`/`setEos`/`getEos`) never look at
/// `reason`.
pub const ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED: usize = 0x7000_0000;

/// Which of the two independent refusals C's `queueRequest` applies to a
/// request (asynManager.c:1536-1552).
///
/// The two are *not* interchangeable and only one of them is ever waived:
///
/// * `!pport->dpc.enabled → asynDisabled` (:1541-1546) is **unconditional** —
///   no priority, no reason, no op class escapes it. A port disabled with
///   `asynEnable(port,0)` was disabled precisely to keep the IOC off the
///   hardware, so nothing it queues may touch the wire.
/// * `!pport->dpc.connected → asynDisconnected` (:1547-1552) is conditional on
///   `checkPortConnect`, which :1536-1538 clears for a request that is queued at
///   `asynQueuePriorityConnect` **and** either carries no device (`addr == -1`)
///   or carries [`ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED`].
///
/// Producing this value is [`AsynUser::connect_check`]'s job and nobody else's:
/// there is deliberately no way to ask the gate to skip the *enabled* half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectCheck {
    /// The port (and device) must be connected — C `checkPortConnect == TRUE`.
    Required,
    /// C `checkPortConnect = FALSE`: queue it on a disconnected port.
    Waived,
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
    /// How long this request may wait **in the port queue** before it is removed
    /// and reported as never having run — C `queueRequest`'s `timeout` argument
    /// (asynManager.c:1514,1617-1623), which is a different clock from
    /// [`Self::timeout`] above: that one bounds the driver's transfer once the
    /// request is *running*, this one bounds the wait *before* it runs.
    ///
    /// `None` is C's `queueRequest(..., 0.0)`: no timer is armed and the request
    /// waits as long as the queue makes it. That is what every device support
    /// does (devAsynInt32.c:838) and it is the default here. `asynRecord` is the
    /// caller that asks for a deadline — `QUEUE_TIMEOUT` = 10 s on both its
    /// process and its special requests (asynRecord.c:71,343,572).
    ///
    /// The deadline is resolved through the request's [`crate::request::CancelToken`]:
    /// only a still-**queued** request can time out, exactly as C's
    /// `queueTimeoutCallback` returns immediately when `!isQueued`
    /// (asynManager.c:655-661). A request that has begun running always completes.
    pub queue_timeout: Option<Duration>,
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
    /// The port this user is connected to, and that port's trace config.
    ///
    /// C's `pasynUser` reaches the trace through its `userPvt → pport/pdevice`
    /// linkage, which is what lets any layer holding the user call
    /// `asynPrint`/`asynPrintIO` (`findTracePvt`, asynManager.c:3040-3060) —
    /// including an *interpose*, which has no other handle on the port
    /// (`asynInterposeCom.c:237-239` prints the unstuffed read at
    /// `ASYN_TRACEIO_FILTER`). `PortActor` is the single
    /// owner of this linkage: it stamps every request's user with the port it is
    /// about to run on. A user built outside a port (a unit test, a driver's own
    /// internal user) carries `None` and its prints are silent.
    pub trace: Option<UserTrace>,
    /// C's `pasynUser->errorMessage`: the message slot every asyn layer writes
    /// its diagnostic into.
    ///
    /// Most of them accompany a failing `asynStatus` and are the `Err` arm here.
    /// This slot exists for the ones that do *not* fail the call —
    /// `asynInterposeCom.c:571-573` and `:587-589` leave "XON/XOFF already set.
    /// Now using RTS/CTS." (and its mirror) in the buffer and carry on — which a
    /// `Result` has no way to carry.
    pub error_message: String,
}

/// The port context an [`AsynUser`] carries so the layers it passes through can
/// trace — C's `pasynUser → pport/pdevice`. Cheap to clone: two `Arc`s.
#[derive(Clone)]
pub struct UserTrace {
    pub manager: std::sync::Arc<crate::trace::TraceManager>,
    pub port: std::sync::Arc<str>,
}

impl Default for AsynUser {
    fn default() -> Self {
        Self {
            reason: 0,
            addr: 0,
            timeout: Duration::from_secs(1),
            queue_timeout: None,
            priority: QueuePriority::default(),
            timestamp: None,
            alarm_status: 0,
            alarm_severity: 0,
            user_data: None,
            block_token: None,
            trace: None,
            error_message: String::new(),
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

    pub fn with_priority(mut self, priority: QueuePriority) -> Self {
        self.priority = priority;
        self
    }

    /// Queue this request even on a port that is not connected — C's
    /// `pasynUser->reason = ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED` paired with
    /// `queueRequest(..., asynQueuePriorityConnect, ...)`.
    ///
    /// Both halves are set here because C's gate reads both (:1536-1538) and
    /// every C caller sets both together: `asynSetOption`
    /// (asynShellCommands.c:121,126), `asynSetEos`/`asynShowEos` (:240,:291),
    /// the record's HOSTINFO put (asynRecord.c:566-569) and its connect-time
    /// option readback (:1277-1280). Setting only the reason would leave the
    /// request at Low priority, where C still refuses it.
    pub fn queue_even_if_not_connected(mut self) -> Self {
        self.reason = ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED;
        self.priority = QueuePriority::Connect;
        self
    }

    /// C `queueRequest`'s `checkPortConnect` (asynManager.c:1536-1538) — the
    /// *only* producer of a [`ConnectCheck`], so the connected refusal is waived
    /// exactly where C waives it and the enabled refusal is waived nowhere.
    pub fn connect_check(&self) -> ConnectCheck {
        if self.priority == QueuePriority::Connect {
            self.connect_check_at_connect_priority()
        } else {
            ConnectCheck::Required
        }
    }

    /// [`Self::connect_check`] for a request that rides the Connect-priority
    /// queue *by construction* — the `asynCommon` connect/disconnect callbacks,
    /// whose C call sites always pass `asynQueuePriorityConnect`
    /// (asynRecord.c:561-563, asynShellCommands.c). Priority being given, only
    /// the second half of C's `checkPortConnect` waiver remains (:1536-1538):
    /// a port-level user (`addr == -1`) or the explicit sentinel. A
    /// device-addressed user with neither — asynRecord's CNCT put, whose
    /// special user is connected at the record's ADDR — is refused
    /// `asynDisconnected` on a disconnected port (W10-D1): a C wart, kept.
    pub fn connect_check_at_connect_priority(&self) -> ConnectCheck {
        if self.addr < 0 || self.reason == ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED {
            ConnectCheck::Waived
        } else {
            ConnectCheck::Required
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Ask for C's `queueRequest(pasynUser, priority, timeout)` queue-wait
    /// deadline on this request — see [`Self::queue_timeout`].
    pub fn with_queue_timeout(mut self, queue_timeout: Duration) -> Self {
        self.queue_timeout = Some(queue_timeout);
        self
    }

    /// C `asynPrintIO(pasynUser, mask, data, len, format, …)`
    /// (asynManager.c:3080-3099): print the buffer through the trace config of
    /// the port/device this user is connected to, if that mask is enabled there.
    /// Silent for a user with no port.
    pub fn print_io(&self, mask: crate::trace::TraceMask, data: &[u8], label: &str) {
        let Some(t) = &self.trace else { return };
        if t.manager.is_enabled(&t.port, mask) {
            t.manager
                .output_device_io(&t.port, Some(self.addr), mask, data, label);
        }
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
