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
    /// because the interpose's `in_buf_tail` has advanced past them. Build
    /// this with [`AsynError::with_partial_read`] rather than by hand, and
    /// read it back with [`AsynError::partial_read`].
    #[error("asyn: {status:?} - {message} (after {} partial bytes)", partial.nbytes_transferred)]
    PartialRead {
        status: AsynStatus,
        message: String,
        /// How much of the caller's buffer was filled before the failure,
        /// and the end-of-message reason accumulated up to that point. The
        /// bytes themselves are already in the caller's buffer — this is the
        /// `*nbytesTransfered` / `*eomReason` pair C writes out alongside the
        /// error.
        partial: crate::interpose::OctetReadResult,
    },

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
    /// (`asynPortDriver.cpp:991-1011`) translates it to `asynError`
    /// with an `asynPrint(ASYN_TRACE_ERROR, ...)` log line. The lax
    /// Rust [`ParamList::create_param`] silently returns the existing
    /// index to match the idempotent build pattern used by
    /// `ad-core-rs`/`ad-plugins-rs` (e.g. `ADDriverParams::create`
    /// after `NDArrayDriverParams::create`); use
    /// [`ParamList::create_param_strict`] when you need C parity for
    /// the duplicate-name error.
    #[error("param already exists: {0}")]
    ParamAlreadyExists(String),

    #[error("param index out of range: {0}")]
    ParamIndexOutOfRange(usize),

    /// C parity: `asynParamUndefined` —
    /// `paramVal::getInteger/getInteger64/getDouble/getUInt32/getString`
    /// throws `ParamValNotDefined` when the value has never been set, and
    /// `paramList::getInteger/...` translates that to `asynParamUndefined`
    /// (`asynPortDriver/asynPortDriver.cpp:301-401,543-566`). The lax Rust
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

    #[error("option not found: {0}")]
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
            AsynError::Status { status, .. } | AsynError::PartialRead { status, .. } => *status,
            _ => AsynStatus::Error,
        }
    }

    /// The partial octet transfer delivered before this error, if any —
    /// C's `*nbytesTransfered` / `*eomReason` on the failure path.
    pub fn partial_read(&self) -> Option<&crate::interpose::OctetReadResult> {
        match self {
            AsynError::PartialRead { partial, .. } => Some(partial),
            _ => None,
        }
    }

    /// Attach a partial octet transfer to a failing read, preserving the
    /// status. This is the only way to build [`AsynError::PartialRead`], so
    /// the status can never be lost in the conversion: a non-status variant
    /// (e.g. [`AsynError::Io`]) folds into C's generic `asynError` with its
    /// `Display` text as the message.
    ///
    /// Re-attaching overwrites: in a stacked interpose chain the outermost
    /// layer is the one that filled the caller's buffer, so its count is the
    /// authoritative `*nbytesTransfered`.
    pub fn with_partial_read(self, partial: crate::interpose::OctetReadResult) -> Self {
        let status = self.status();
        let message = match self {
            AsynError::Status { message, .. } | AsynError::PartialRead { message, .. } => message,
            other => other.to_string(),
        };
        AsynError::PartialRead {
            status,
            message,
            partial,
        }
    }
}

pub type AsynResult<T> = Result<T, AsynError>;
