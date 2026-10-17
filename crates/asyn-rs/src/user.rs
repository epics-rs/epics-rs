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
/// (drvAsynIPPort.c:792-794), `VMIN = 1` with no `VTIME` for the serial driver
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
/// request (asynManager.c:1538-1554).
///
/// The two are *not* interchangeable and only one of them is ever waived:
///
/// * `!pport->dpc.enabled → asynDisabled` (:1543-1548) is **unconditional** —
///   no priority, no reason, no op class escapes it. A port disabled with
///   `asynEnable(port,0)` was disabled precisely to keep the IOC off the
///   hardware, so nothing it queues may touch the wire.
/// * `!pport->dpc.connected → asynDisconnected` (:1549-1554) is conditional on
///   `checkPortConnect`, which :1538-1540 clears for a request that is queued at
///   `asynQueuePriorityConnect` **and** either carries no device or carries
///   [`ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED`]. A user has a device only on an
///   `ASYN_MULTIDEVICE` port at `addr >= 0` (`locateDevice`, :576).
///
/// Producing this value is [`AsynUser::connect_check`]'s job and nobody else's:
/// there is deliberately no way to ask the gate to skip the *enabled* half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectCheck {
    /// The port (and device) must be connected — C `checkPortConnect == TRUE`.
    Required,
    /// C `checkPortConnect = FALSE`: queue it on a disconnected port.
    Waived,
    /// C `queueLockPort` on a synchronous port (asynManager.c:1851-1856,
    /// :1913-1916): the port-enabled refusal and nothing else — no connected,
    /// device or auto-connect step.
    SynchronousLock,
}

/// The two halves of C `queueLockPort` (asynManager.c:1834-1916), chosen by
/// the port's `ASYN_CANBLOCK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockPort {
    /// A CANBLOCK port: the call is a Low-priority `queueRequest` whose
    /// never-ran outcomes carry queueLockPort's own text and status
    /// (`AsynError::through_queue_lock_port`).
    Queued,
    /// A synchronous port: `epicsMutexMustLock(pport->synchronousLock)`, with
    /// no queue, no deadline and no text of its own.
    Synchronous,
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
    /// set only for `addr >= 0`, asynManager.c:1351-1354), and renders the
    /// no-device case as -1 at the driver boundary (`getAddr`, :2006-2010;
    /// `connectAttempt`, :754). A non-negative value therefore means a device
    /// was deliberately chosen — `0` is device 0, never "unset".
    pub addr: i32,
    /// I/O timeout. Only meaningful for drivers that perform real I/O.
    ///
    /// `None` is C's negative `pasynUser->timeout`: **wait forever**, the wait
    /// the operator asks for with `@asyn(PORT,0,-1)` or a negative TMOT. Every
    /// driver wait built from this field must be unbounded in that case — that
    /// is what C's `readPollmsec = -1` (drvAsynIPPort.c:792-794) and its
    /// `VMIN = 1` (drvAsynSerialPort.c:906-909) do — and the operations C keys
    /// off the *sign* must read it the same way: `disconnectOnReadTimeout`
    /// fires only for `pasynUser->timeout > 0` (drvAsynIPPort.c:853), so a
    /// forever-wait never tears the socket down.
    ///
    /// `Some(Duration::ZERO)` is a different thing again — C's non-blocking
    /// poll, floored to a 1 ms wait by the drivers that poll.
    pub timeout: Option<Duration>,
    /// How long this request may wait **in the port queue** before it is removed
    /// and reported as never having run — C `queueRequest`'s `timeout` argument
    /// (asynManager.c:1516,1619-1625), which is a different clock from
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
    /// (asynManager.c:657-663). A request that has begun running always completes.
    pub queue_timeout: Option<QueueDeadline>,
    /// `Some` when this request is a SyncIO call's `queueLockPort`
    /// (asynManager.c:1838), not a plain `queueRequest`; which half of it is
    /// the port's class. Set only by [`Self::locking_port`].
    pub lock_port: Option<LockPort>,
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
    /// C `pasynUser->drvUser`, from the record's `drvUserCreate`
    /// ([`crate::port::DrvUserInfo::drv_user`]).
    pub drv_user: Option<crate::port::DrvUser>,
    /// Token for BlockProcess ownership. When a port is blocked, only requests
    /// with a matching block_token (or UnblockProcess) are dequeued.
    pub block_token: Option<u64>,
    /// The port this user is connected to, and that port's trace config.
    ///
    /// C's `pasynUser` reaches the trace through its `userPvt → pport/pdevice`
    /// linkage, which is what lets any layer holding the user call
    /// `asynPrint`/`asynPrintIO` (`findTracePvt`, asynManager.c:548-553) —
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
    /// `asynInterposeCom.c:584-586` and `:600-602` leave "XON/XOFF already set.
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

impl UserTrace {
    /// C `asynPrint(pasynUser, mask, format, …)` — `tracePrintSource`
    /// (asynManager.c:3074-3082): print through the trace config of the
    /// device `addr` on this port, if `mask` is enabled there. `file`/`line`
    /// are the caller's, as for [`AsynUser::print_io`].
    ///
    /// The message is formatted only when the mask is enabled, onto the
    /// stack, so a print on a hot path costs one config lookup while tracing
    /// is off and allocates nothing while it is on.
    pub fn print(
        &self,
        addr: i32,
        reason: i32,
        mask: crate::trace::TraceMask,
        file: &str,
        line: u32,
        args: std::fmt::Arguments<'_>,
    ) {
        self.manager.print_source(
            Some(&self.port),
            Some(addr),
            reason,
            mask,
            (file, line),
            args,
        );
    }
}

/// A queue-wait deadline that arms a timer. C starts one only for
/// `timeout > 0.0` (asynManager.c:1619-1626), so a zero cannot be built: a
/// zero timer would fail the request the moment it queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueDeadline(Duration);

impl QueueDeadline {
    /// `None` for `Duration::ZERO` — C's `queueRequest(..., 0.0)`, no timer.
    pub fn new(timeout: Duration) -> Option<Self> {
        (!timeout.is_zero()).then_some(Self(timeout))
    }

    pub fn get(self) -> Duration {
        self.0
    }
}

/// C `DEFAULT_QUEUE_LOCK_PORT_TIMEOUT` (asynManager.c:50), a new port's
/// `queueLockPortTimeout` (:2071); see [`AsynUser::locking_port`].
pub const QUEUE_LOCK_PORT_TIMEOUT: Duration = Duration::from_secs(2);

impl Default for AsynUser {
    fn default() -> Self {
        Self {
            reason: 0,
            addr: -1,
            timeout: Some(DEFAULT_TIMEOUT),
            queue_timeout: None,
            lock_port: None,
            priority: QueuePriority::default(),
            timestamp: None,
            alarm_status: 0,
            alarm_severity: 0,
            user_data: None,
            drv_user: None,
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
    /// Both halves are set here because C's gate reads both (:1538-1540) and
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

    /// C `queueRequest`'s `checkPortConnect` (asynManager.c:1538-1540) on a port
    /// that is (`multi_device`) or is not `ASYN_MULTIDEVICE` — the *only*
    /// producer of a [`ConnectCheck`], so the connected refusal is waived
    /// exactly where C waives it and the enabled refusal is waived nowhere.
    pub fn connect_check(&self, multi_device: bool) -> ConnectCheck {
        if self.lock_port == Some(LockPort::Synchronous) {
            ConnectCheck::SynchronousLock
        } else if self.priority == QueuePriority::Connect {
            self.connect_check_at_connect_priority(multi_device)
        } else {
            ConnectCheck::Required
        }
    }

    /// [`Self::connect_check`] for a request that rides the Connect-priority
    /// queue *by construction* — the `asynCommon` connect/disconnect callbacks,
    /// whose C call sites always pass `asynQueuePriorityConnect`
    /// (asynRecord.c:561-563, asynShellCommands.c). Priority being given, only
    /// the second half of C's `checkPortConnect` waiver remains (:1538-1540):
    /// a user with no device or the explicit sentinel. C's `addr` there is the
    /// device's (`pdevice ? pdevice->addr : -1`, :1522), and `connectDevice`
    /// finds no device on a port without `ASYN_MULTIDEVICE` (:576, :1352), so
    /// only a multi-device port refuses a device-addressed CNCT put on a
    /// disconnected port.
    pub fn connect_check_at_connect_priority(&self, multi_device: bool) -> ConnectCheck {
        if crate::port::dp_common_key(multi_device, self.addr).is_none()
            || self.reason == ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED
        {
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

    pub fn with_drv_user(mut self, drv_user: Option<crate::port::DrvUser>) -> Self {
        self.drv_user = drv_user;
        self
    }

    /// C's negative `pasynUser->timeout`: this request's driver wait has no
    /// bound at all — see [`Self::timeout`].
    pub fn waiting_forever(mut self) -> Self {
        self.timeout = None;
        self
    }

    /// Mark this request as a SyncIO call's C `queueLockPort` on `port`.
    ///
    /// On a CANBLOCK port it queues with a deadline (asynManager.c:1894-1896):
    /// the port's `queueLockPortTimeout` (see
    /// `PortHandle::queue_lock_port_timeout`), or this user's own timeout when
    /// longer. It fails at it (`queueLockPortTimeoutCallback`, :972-990)
    /// instead of waiting out a port that went down; a maximum of zero arms no
    /// timer, as in C. A synchronous port only takes `synchronousLock`
    /// (:1913-1916), so there is no deadline.
    pub fn locking_port(mut self, port: &crate::port_handle::PortHandle) -> Self {
        if !port.can_block() {
            self.lock_port = Some(LockPort::Synchronous);
            return self;
        }
        let timeout = self.timeout.unwrap_or_default();
        self.lock_port = Some(LockPort::Queued);
        self.with_queue_timeout(timeout.max(port.queue_lock_port_timeout()))
    }

    /// Ask for C's `queueRequest(pasynUser, priority, timeout)` queue-wait
    /// deadline on this request — see [`field@Self::queue_timeout`].
    pub fn with_queue_timeout(mut self, queue_timeout: Duration) -> Self {
        self.queue_timeout = QueueDeadline::new(queue_timeout);
        self
    }

    /// The queue-wait deadline, if this request arms one.
    pub fn queue_timeout(&self) -> Option<Duration> {
        self.queue_timeout.map(QueueDeadline::get)
    }

    /// C `asynPrint(pasynUser, mask, format, …)`: print through the trace
    /// config of the port/device this user is connected to, if `mask` is
    /// enabled there. Silent for a user with no port. `file`/`line` are the
    /// caller's — see [`Self::print_io`].
    pub fn print(
        &self,
        mask: crate::trace::TraceMask,
        file: &str,
        line: u32,
        args: std::fmt::Arguments<'_>,
    ) {
        if let Some(t) = &self.trace {
            t.print(self.addr, self.reason as i32, mask, file, line, args);
        }
    }

    /// C `asynPrintIO(pasynUser, mask, data, len, format, …)` — `tracePrintIO`
    /// (asynManager.c:3121-3125) into `traceVprintIOSource` (:3148-3158): print
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
        if t.manager.is_enabled_device(&t.port, self.addr, mask) {
            // C's `pasynUser->reason` is an `int` and `printPort` writes it
            // with `%d` (asynManager.c:3043); the whole reason domain,
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
        // when `addr >= 0` (asynManager.c:1351-1354), and `getAddr` returns -1
        // whenever `pdevice` is NULL (:2006-2010).
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
