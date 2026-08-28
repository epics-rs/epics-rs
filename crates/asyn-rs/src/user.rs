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
/// C carries the timeout as a `double` whose **sign** carries meaning: a
/// negative value is the "wait forever" the operator asked for, and every wait
/// built from it is unbounded — `readPollmsec = -1` for the IP driver
/// (drvAsynIPPort.c:741-743), `VMIN = 1` with no `VTIME` for the serial driver
/// (drvAsynSerialPort.c:906-909). It reaches asyn from `@asyn(PORT,0,-1)`
/// (`strtod`, asynEpicsUtils.c:125) and from a record's TMOT field. `None` is
/// that value here (DRV-42); it used to take [`DEFAULT_TIMEOUT`] instead, so a
/// record told to wait forever timed out after a second and raised an alarm C
/// never raises.
///
/// The values that remain unrepresentable are the ones C has no defined
/// behaviour for either — NaN, infinity, and a magnitude beyond `Duration` —
/// and those still take the bounded [`DEFAULT_TIMEOUT`]. Callers must not
/// construct a `Duration` from an operator-supplied `f64` themselves:
/// `Duration::from_secs_f64` *panics* on exactly the inputs C accepts, so
/// routing the conversion through this function is what makes the panic
/// structurally impossible rather than merely unobserved.
pub fn timeout_from_secs(secs: f64) -> Option<Duration> {
    if secs.is_finite() && secs < 0.0 {
        return None;
    }
    Some(Duration::try_from_secs_f64(secs).unwrap_or(DEFAULT_TIMEOUT))
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
    /// The device this user is connected to, or `-1` when it is connected to
    /// the port itself. `-1` is the default: C decides port-vs-device from
    /// whether `connectDevice` allocated a device at all (`puserPvt->pdevice`,
    /// set only for `addr >= 0`, asynManager.c:1348-1351), and renders the
    /// no-device case as -1 at the driver boundary (`getAddr`, :2004-2008;
    /// `connectAttempt`, :752). A non-negative value therefore means a device
    /// was deliberately chosen — `0` is device 0, never "unset".
    pub addr: i32,
    /// I/O timeout. Only meaningful for drivers that perform real I/O.
    ///
    /// `None` is C's negative `pasynUser->timeout`: **wait forever**, the wait
    /// the operator asks for with `@asyn(PORT,0,-1)` or a negative TMOT. Every
    /// driver wait built from this field must be unbounded in that case — that
    /// is what C's `readPollmsec = -1` (drvAsynIPPort.c:741-743) and its
    /// `VMIN = 1` (drvAsynSerialPort.c:906-909) do — and the operations C keys
    /// off the *sign* must read it the same way: `disconnectOnReadTimeout`
    /// fires only for `pasynUser->timeout > 0` (drvAsynIPPort.c:799), so a
    /// forever-wait never tears the socket down.
    ///
    /// `Some(Duration::ZERO)` is a different thing again — C's non-blocking
    /// poll, floored to a 1 ms wait by the drivers that poll.
    pub timeout: Option<Duration>,
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
    /// `asynPrint`/`asynPrintIO` (`findTracePvt`, asynManager.c:546-551) —
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
            addr: -1,
            timeout: Some(DEFAULT_TIMEOUT),
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
    /// (asynShellCommands.c:121,127), `asynSetEos`/`asynShowEos` (:240,:291),
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

    pub fn with_timeout(self, timeout: Duration) -> Self {
        self.with_timeout_opt(Some(timeout))
    }

    /// Carry a timeout that may already be C's "wait forever" — for a caller
    /// passing on the timeout it was itself given. See [`Self::timeout`].
    pub fn with_timeout_opt(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }

    /// C's negative `pasynUser->timeout`: this request's driver wait has no
    /// bound at all — see [`Self::timeout`].
    pub fn waiting_forever(mut self) -> Self {
        self.timeout = None;
        self
    }

    /// Ask for C's `queueRequest(pasynUser, priority, timeout)` queue-wait
    /// deadline on this request — see [`Self::queue_timeout`].
    pub fn with_queue_timeout(mut self, queue_timeout: Duration) -> Self {
        self.queue_timeout = Some(queue_timeout);
        self
    }

    /// C `asynPrintIO(pasynUser, mask, data, len, format, …)` — `tracePrintIO`
    /// (asynManager.c:3096-3100) into `traceVprintIOSource` (:3123-3133): print
    /// the buffer through the trace config of the port/device this user is
    /// connected to, if that mask is enabled there. Silent for a user with no port.
    ///
    /// `file`/`line` are the caller's, because C's `asynPrintIO` is a macro
    /// that captures `__FILE__`/`__LINE__` at the call site
    /// (asynDriver.h:296-299); passing anything else would attribute the line
    /// to this function rather than to the driver that emitted it.
    pub fn print_io(
        &self,
        mask: crate::trace::TraceMask,
        data: &[u8],
        label: &str,
        file: &str,
        line: u32,
    ) {
        let Some(t) = &self.trace else { return };
        if t.manager.is_enabled(&t.port, mask) {
            // C's `pasynUser->reason` is an `int` and `printPort` writes it
            // with `%d` (asynManager.c:3018); the whole reason domain,
            // `ASYN_REASON_SIGNAL` (-1) through `ASYN_REASON_RESERVED_HIGH`
            // (0x7FFFFFFF), fits an `i32`.
            t.manager.output_device_io(
                &t.port,
                Some(self.addr),
                self.reason as i32,
                mask,
                data,
                label,
                file,
                line,
            );
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
        // A user that has not been connected to a device is port-level. C
        // renders that as -1: `connectDevice` sets `puserPvt->pdevice` only
        // when `addr >= 0` (asynManager.c:1348-1351), and `getAddr` returns -1
        // whenever `pdevice` is NULL (:2004-2008).
        assert_eq!(u.addr, -1);
        assert_eq!(u.timeout, Some(Duration::from_secs(1)));
    }

    #[test]
    fn test_user_builder() {
        let u = AsynUser::new(42)
            .with_addr(3)
            .with_timeout(Duration::from_millis(500));
        assert_eq!(u.reason, 42);
        assert_eq!(u.addr, 3);
        assert_eq!(u.timeout, Some(Duration::from_millis(500)));
    }
}
