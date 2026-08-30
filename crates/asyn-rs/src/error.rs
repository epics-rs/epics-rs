/// Status codes matching C asyn's asynStatus enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsynStatus {
    Success,
    Timeout,
    Overflow,
    Error,
    Disconnected,
    Disabled,
}

/// Error type for asyn-rs operations.
#[derive(Debug, thiserror::Error)]
pub enum AsynError {
    #[error("asyn: {status:?} - {message}")]
    Status { status: AsynStatus, message: String },

    /// An octet read that failed *after* transferring bytes into the
    /// caller's buffer.
    ///
    /// C parity: `asynOctet::read` reports `*nbytesTransfered` and
    /// `*eomReason` **together with** a failing `asynStatus` — the EOS
    /// interpose breaks out of its accumulation loop on a lower-layer error
    /// and still runs the common tail
    /// (`asynInterposeEos.c:242-253`: null-terminate, `*eomReason = eom`,
    /// `*nbytesTransfered = nRead`, `return status`). A device that emits a
    /// partial line and then goes quiet therefore reaches the record as
    /// `asynTimeout` *plus* the bytes it did send; `asynRecord` commits both
    /// (`asynRecord.c:1591,1627`: `eomr` and `nord` are assigned regardless
    /// of status).
    ///
    /// [`AsynError::Status`] alone cannot express that: `?` on a
    /// partially-filled read discards the count and the eom reason, and the
    /// bytes already written into the caller's buffer become unrecoverable
    /// because the interpose's `in_buf_tail` has advanced past them — and
    /// because every dispatch hop above the driver (`port_actor` →
    /// `PortHandle` → device support) owns its own buffer, which `?` drops.
    /// Carrying the bytes *in* the error is what makes the transfer and the
    /// status one value: a consumer cannot take the failure without also
    /// being handed everything the device did send. Build this with
    /// [`AsynError::with_partial_read`] rather than by hand, and read it back
    /// with [`AsynError::partial_read`].
    ///
    /// The carrier **wraps** the failure it decorates instead of copying its
    /// status and message out of it: flattening would erase which *kind* of
    /// failure it was, and callers legitimately ask that question — the
    /// drivers' fatal-transport test ([`AsynError::is_fatal_transport`]) tears
    /// the link down for a real errno ([`AsynError::Io`]) but leaves it up for
    /// a timeout, so a flattened `Io` (a `recv` that returned `ECONNRESET`
    /// after the EOS interpose had already buffered half a line) would silently
    /// stop disconnecting. Every question about the underlying failure —
    /// [`AsynError::status`], [`AsynError::message`],
    /// [`AsynError::is_transport_io`] — is answered *through* the carrier.
    #[error("{source} (after {} partial bytes)", partial.nbytes_transferred())]
    PartialRead {
        /// The failure that ended the transfer, intact.
        source: Box<AsynError>,
        /// The bytes transferred before the failure and the end-of-message
        /// reason accumulated up to that point — the `*nbytesTransfered` /
        /// `*eomReason` pair C writes out alongside the error.
        partial: crate::interpose::PartialOctetRead,
    },

    /// An octet write that failed *after* the device accepted bytes.
    ///
    /// C parity: `asynOctet::write` reports `*nbytesTransfered` **together
    /// with** a failing `asynStatus`, at every layer of the write chain —
    /// `drvAsynSerialPort.c::writeIt` (`:843`) assigns
    /// `*nbytesTransfered = numchars - nleft` on the way out of the loop no
    /// matter whether it broke on `asynTimeout` or on a fatal `write()` errno;
    /// `asynInterposeEcho.c::writeIt` (`:83`) and `asynInterposeDelay.c` (`:51`)
    /// assign `*nbytesTransfered = transfered` on every break; and
    /// `asynInterposeEos.c::writeIt` (`:179`) clamps the lower layer's count to
    /// the caller's `numchars` and returns it beside the status. `asynRecord`
    /// commits the result unconditionally — `nawt = nbytesTransfered`
    /// (asynRecord.c:1547) runs *before* the status check at `:1551` — so a
    /// half-written command lands `NAWT=3` next to its `Write error, nout=3`
    /// diagnostic.
    ///
    /// [`AsynError::Status`] alone cannot express that: `?` on a partially
    /// accepted write discards the count, and every hop above the driver
    /// (`port_actor` → `PortHandle` → record/device support) only sees the
    /// status. This is the write-side twin of [`AsynError::PartialRead`], for
    /// the same reason: carrying the count *in* the error makes the transfer
    /// and the status one value, so a consumer cannot take the failure without
    /// being handed how far the device got. Build it with
    /// [`AsynError::with_partial_write`] and read it back with
    /// [`AsynError::partial_write`]. Like [`AsynError::PartialRead`] it wraps
    /// the underlying failure rather than flattening it, so the transport
    /// classifiers still see the errno.
    #[error("{source} (after {nbytes} partial bytes written)")]
    PartialWrite {
        /// The failure that ended the write, intact.
        source: Box<AsynError>,
        /// The bytes the device accepted before the failure — C's
        /// `*nbytesTransfered` on the error path.
        nbytes: usize,
    },

    /// The request waited in the port queue past the deadline its
    /// `queueRequest` was given, so it was removed and **never ran**.
    ///
    /// C `queueTimeoutCallback` (asynManager.c:647-700): the timer
    /// `queueRequest(pasynUser, priority, timeout)` armed at enqueue
    /// (asynManager.c:1617-1623) fires while `isQueued` is still true, the
    /// request is unlinked from the port's queue list, and the caller's
    /// `timeoutUser` runs **instead of** its `processUser`. Only a caller that
    /// asked for a queue deadline can see this — device support passes
    /// `queueRequest(..., 0.0)` (devAsynInt32.c:838) and arms no timer at all,
    /// while `asynRecord` passes `QUEUE_TIMEOUT` = 10 s for both its process and
    /// its special requests (asynRecord.c:71,343,572).
    ///
    /// Distinct from `AsynStatus::Timeout`, which means the driver *did* run and
    /// the device did not answer in time. C reports this one as plain
    /// `asynError` (asynRecord.c:919-927), which is what [`AsynError::status`]'s
    /// default branch gives it.
    #[error("queueRequest timeout on port {port}")]
    QueueTimeout { port: String },

    /// The port's queue gate refused the request, so it was **never queued and
    /// never ran** — C's `queueRequest` returning `asynDisabled` /
    /// `asynDisconnected` (asynManager.c:1541-1552).
    ///
    /// Distinct from an [`AsynError::Status`] carrying the same `AsynStatus`,
    /// and that distinction is the whole point: in C the two arrive by different
    /// routes and mean opposite things. A refusal is `queueRequest`'s *return
    /// value* — the callback never runs, so nothing it implies happened: no
    /// bytes moved, no option was written, no readback, no `monitorStatus`
    /// (asynRecord.c:571-578 reports `pasynUser->errorMessage` and frees the
    /// user). A driver error arrives *inside* the callback, which did run and
    /// whose tail C still executes. Collapsing them into one variant made a
    /// refused `special()` report as a callback that ran (R14-46).
    ///
    /// Built only by the gate owner ([`crate::port::PortDriverBase::check_queue`])
    /// and asked about through [`AsynError::never_ran`].
    #[error("asyn: {status:?} - {message}")]
    QueueRefused { status: AsynStatus, message: String },

    #[error("port not found: {0}")]
    PortNotFound(String),

    #[error("port already registered: {0}")]
    PortAlreadyRegistered(String),

    #[error("param not found: {0}")]
    ParamNotFound(String),

    /// C parity: `asynParamAlreadyExists` —
    /// `paramList::createParam` (`asynPortDriver.cpp:126-138`) returns
    /// this status when a second `createParam(name, ...)` arrives with
    /// the same name. The `asynPortDriver::createParam` wrapper
    /// (`asynPortDriver.cpp:991-1012`) translates it to `asynError`
    /// with an `asynPrint(ASYN_TRACE_ERROR, ...)` log line. The lax
    /// Rust [`ParamList::create_param`](crate::param::ParamList::create_param) silently returns the existing
    /// index to match the idempotent build pattern used by
    /// `ad-core-rs`/`ad-plugins-rs` (e.g. `ADDriverParams::create`
    /// after `NDArrayDriverParams::create`); use
    /// [`ParamList::create_param_strict`](crate::param::ParamList::create_param_strict) when you need C parity for
    /// the duplicate-name error.
    #[error("param already exists: {0}")]
    ParamAlreadyExists(String),

    #[error("param index out of range: {0}")]
    ParamIndexOutOfRange(usize),

    /// C parity: `asynParamUndefined` —
    /// `paramVal::getInteger/getInteger64/getDouble/getUInt32/getString`
    /// throws `ParamValNotDefined` when the value has never been set, and
    /// `paramList::getInteger/...` translates that to `asynParamUndefined`
    /// (`asynPortDriver/asynPortDriver.cpp:301-403,543-565`). The lax Rust
    /// getters (`ParamList::get_int32` etc.) return the type default
    /// (`0`, `0.0`, `""`) silently — that mirrors many existing call sites
    /// that use `.unwrap_or(...)`. Use the `_strict` variants
    /// (`ParamList::get_int32_strict` etc.) to surface this status the way
    /// C reportGetParamErrors does.
    #[error("param undefined: index {0}")]
    ParamUndefined(usize),

    #[error("type mismatch: expected {expected}, got {actual}")]
    TypeMismatch {
        expected: &'static str,
        actual: &'static str,
    },

    #[error("interface not supported: {0}")]
    InterfaceNotSupported(String),

    #[error("address out of range: {0}")]
    AddressOutOfRange(i32),

    #[error("already subscribed")]
    AlreadySubscribed,

    /// An option key the port does not implement — C's trailing
    /// `else if (epicsStrCaseCmp(key, "") != 0)` arm, in `setOption` and
    /// `getOption` alike (drvAsynSerialPort.c:594-597, :1171;
    /// drvAsynSerialPortWin32.c:341-344; drvAsynIPPort.c:902-905). The text is
    /// C's, verbatim: it is what reaches the operator through
    /// `pasynUser->errorMessage` and lands in the record's ERRS.
    #[error("Unsupported key \"{0}\"")]
    OptionNotFound(String),

    #[error("invalid link syntax: {0}")]
    InvalidLinkSyntax(String),

    #[error("downcast failed: stored type does not match requested type")]
    DowncastFailed,

    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
}

impl AsynError {
    /// The `asynStatus` this error carries — the single owner of the
    /// error → status mapping.
    ///
    /// Every consumer that classifies a failure by status (record alarm
    /// mapping, fatal-transport detection, protocol reply status) MUST go
    /// through this instead of matching [`AsynError::Status`] directly:
    /// a bare match silently misclassifies every other status-carrying
    /// variant (that is exactly how [`AsynError::PartialRead`] would have
    /// downgraded a timeout to a generic error). Variants that carry no
    /// status take C's generic `asynError`, matching the
    /// `asynStatusToEpicsAlarm` default branch (asynEpicsUtils.c:234-266).
    pub fn status(&self) -> AsynStatus {
        match self {
            AsynError::Status { status, .. } | AsynError::QueueRefused { status, .. } => *status,
            AsynError::PartialRead { source, .. } | AsynError::PartialWrite { source, .. } => {
                source.status()
            }
            _ => AsynStatus::Error,
        }
    }

    /// The driver/interpose diagnostic behind this error — C's
    /// `pasynUser->errorMessage`, which every `reportError` call site splices
    /// into `ERRS`. Reads *through* the partial carriers, so a failed transfer
    /// reports the same text whether or not it moved bytes first.
    pub fn message(&self) -> String {
        match self {
            AsynError::Status { message, .. } | AsynError::QueueRefused { message, .. } => {
                message.clone()
            }
            AsynError::PartialRead { source, .. } | AsynError::PartialWrite { source, .. } => {
                source.message()
            }
            other => other.to_string(),
        }
    }

    /// Re-stamp a gate refusal as one: the request was rejected by
    /// `queueRequest` and never ran. The gate owner
    /// ([`crate::port::PortDriverBase::check_queue`]) is the only caller — the
    /// same checks reached any other way (a driver's own `check_ready` inside a
    /// request that *is* running) stay [`AsynError::Status`], because there the
    /// callback did run.
    pub(crate) fn into_queue_refusal(self) -> AsynError {
        match self {
            AsynError::Status { status, message } => AsynError::QueueRefused { status, message },
            other => other,
        }
    }

    /// True iff the request never ran at all — the port's queue gate refused it
    /// ([`AsynError::QueueRefused`]) or it sat past its deadline and was removed
    /// ([`AsynError::QueueTimeout`]).
    ///
    /// The single owner of that question, and the one every caller of a queued
    /// request must ask before doing anything a *completed* request implies.
    /// C draws the line structurally: both outcomes are decided before
    /// `processUser` is ever dispatched — `queueRequest` returns the refusal
    /// (asynManager.c:1541-1552) and `queueTimeoutCallback` runs `timeoutUser`
    /// *instead of* `processUser` (:647-700) — so no bytes moved, no option or
    /// EOS was written, no connect was attempted, and none of the follow-up work
    /// a completed request implies (asynRecord's `setOption` → `getOptions`
    /// fall-through, its `monitorStatus` tail) may run.
    pub fn never_ran(&self) -> bool {
        self.is_queue_refused() || self.is_queue_timeout()
    }

    /// True iff the port's queue gate refused the request — see
    /// [`AsynError::QueueRefused`]. Ask [`AsynError::never_ran`] unless you
    /// specifically need to tell a refusal from a queue timeout.
    pub fn is_queue_refused(&self) -> bool {
        match self {
            AsynError::QueueRefused { .. } => true,
            AsynError::PartialRead { source, .. } | AsynError::PartialWrite { source, .. } => {
                source.is_queue_refused()
            }
            _ => false,
        }
    }

    /// True iff the request never ran because it sat in the port queue past its
    /// deadline — C's `queueTimeoutCallback` outcome, see
    /// [`AsynError::QueueTimeout`].
    ///
    /// The single owner of that test. Callers that arm a queue deadline MUST ask
    /// through this rather than matching the variant, and MUST NOT treat the
    /// failure as an I/O result: C runs `timeoutUser` *instead of* `processUser`,
    /// so nothing the request would have done happened — no bytes moved, no
    /// option was written, and none of the follow-up work a completed request
    /// implies (asynRecord's `setOption` → `getOptions` fall-through) may run.
    pub fn is_queue_timeout(&self) -> bool {
        match self {
            AsynError::QueueTimeout { .. } => true,
            AsynError::PartialRead { source, .. } | AsynError::PartialWrite { source, .. } => {
                source.is_queue_timeout()
            }
            _ => false,
        }
    }

    /// True when the failure came from the OS transport itself — a real errno
    /// on the fd/socket, not a timeout and not a higher-layer complaint.
    ///
    /// C's drivers decide this at the errno itself, calling `closeConnection`
    /// right where `read`/`write` failed (drvAsynIPPort.c:692-698,
    /// drvAsynSerialPort.c:836-845) while returning `asynTimeout` with the link
    /// intact on a poll expiry. Rust re-derives the decision one layer up, in
    /// [`AsynError::is_fatal_transport`], so the errno has to survive the trip:
    /// ask *through* the partial carriers rather than matching
    /// [`AsynError::Io`] by variant, or a half-transferred `ECONNRESET` reads
    /// as non-fatal and leaves a dead socket reporting `connected` forever.
    pub fn is_transport_io(&self) -> bool {
        match self {
            AsynError::Io(_) => true,
            AsynError::PartialRead { source, .. } | AsynError::PartialWrite { source, .. } => {
                source.is_transport_io()
            }
            _ => false,
        }
    }

    /// This failure means the link is dead and the driver must tear the
    /// connection down — C's `closeConnection` contract: a real errno on the
    /// fd/socket, or an explicit disconnect, but **not** a timeout (C returns
    /// `asynTimeout` with the link intact and lets the next transfer retry).
    ///
    /// The single owner of that test for every octet driver (`drvAsynIPPort`,
    /// `drvAsynSerialPort`, its Win32 twin), which each used to keep a private
    /// copy — and each copy independently proxied "real errno" through
    /// `matches!(e, AsynError::Io(_))`, a variant match that the
    /// partial-transfer carriers defeat.
    pub fn is_fatal_transport(&self) -> bool {
        matches!(self.status(), AsynStatus::Disconnected) || self.is_transport_io()
    }

    /// The partial octet transfer delivered before this error, if any —
    /// C's `*nbytesTransfered` / `*eomReason` / caller-buffer contents on the
    /// failure path.
    ///
    /// Every consumer of an octet read MUST consult this on the error path:
    /// C `asynRecord::performOctetIO` (asynRecord.c:1591-1629) and
    /// `devAsynOctet::readIt` (devAsynOctet.c:693-717) both publish the
    /// transfer regardless of the returned status, so an error branch that
    /// looks only at the status silently drops device data C delivers.
    pub fn partial_read(&self) -> Option<&crate::interpose::PartialOctetRead> {
        match self {
            AsynError::PartialRead { partial, .. } => Some(partial),
            _ => None,
        }
    }

    /// Attach a partial octet transfer to a failing read. This is the only way
    /// to build [`AsynError::PartialRead`], so the underlying failure can never
    /// be lost in the conversion — it is wrapped, not copied.
    ///
    /// Re-attaching overwrites the count: in a stacked interpose chain the
    /// outermost layer is the one that filled the caller's buffer, so its count
    /// is the authoritative `*nbytesTransfered`. The original source is kept —
    /// re-wrapping must not bury it one layer deeper each hop.
    pub fn with_partial_read(self, partial: crate::interpose::PartialOctetRead) -> Self {
        let source = match self {
            AsynError::PartialRead { source, .. } => source,
            other => Box::new(other),
        };
        AsynError::PartialRead { source, partial }
    }

    /// The bytes the device accepted before this write failed — C's
    /// `*nbytesTransfered` on a failing `asynOctet::write`. `None` means the
    /// layer reported no transfer, which is C's pre-call
    /// `nbytesTransfered = 0` (asynRecord.c:1527) left untouched: zero bytes.
    ///
    /// Every consumer of an octet write MUST consult this on the error path:
    /// C `asynRecord::performOctetIO` publishes `nawt = nbytesTransfered`
    /// before it looks at the status (asynRecord.c:1547-1551), so an error
    /// branch that reports only "0 written" contradicts what the device
    /// actually received.
    pub fn partial_write(&self) -> Option<usize> {
        match self {
            AsynError::PartialWrite { nbytes, .. } => Some(*nbytes),
            _ => None,
        }
    }

    /// Attach the accepted-byte count to a failing write. This is the only way
    /// to build [`AsynError::PartialWrite`], so the underlying failure can
    /// never be lost in the conversion — it is wrapped, not copied.
    ///
    /// Re-attaching overwrites the count: in a stacked interpose chain the
    /// outermost layer is the one that owns the caller's `numchars` (the EOS
    /// interpose must not report its appended terminator bytes), so its count
    /// is the authoritative `*nbytesTransfered`. The original source is kept.
    pub fn with_partial_write(self, nbytes: usize) -> Self {
        let source = match self {
            AsynError::PartialWrite { source, .. } => source,
            other => Box::new(other),
        };
        AsynError::PartialWrite { source, nbytes }
    }
}

pub type AsynResult<T> = Result<T, AsynError>;
