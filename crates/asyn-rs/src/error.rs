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

pub type AsynResult<T> = Result<T, AsynError>;
