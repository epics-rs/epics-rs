use thiserror::Error;

#[derive(Error, Debug)]
pub enum CaError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("timeout waiting for response")]
    Timeout,

    #[error("channel not found: {0}")]
    ChannelNotFound(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("unsupported DBR type: {0}")]
    UnsupportedType(u16),

    #[error("write failed: ECA status {0:#06x}")]
    WriteFailed(u32),

    #[error("field not found: {0}")]
    FieldNotFound(String),

    #[error("field is read-only: {0}")]
    ReadOnlyField(String),

    #[error("type mismatch for field {0}")]
    TypeMismatch(String),

    #[error("invalid value: {0}")]
    InvalidValue(String),

    #[error("put disabled (DISP=1) for field {0}")]
    PutDisabled(String),

    #[error("link error: {0}")]
    LinkError(String),

    #[error("DB parse error at line {line}, column {column}: {message}")]
    DbParseError {
        line: usize,
        column: usize,
        message: String,
    },

    #[error("calc error: {0}")]
    CalcError(String),

    #[error("channel disconnected")]
    Disconnected,

    #[error("client shut down")]
    Shutdown,

    /// CA WRITE_NOTIFY arrived while a previous async put on the same
    /// record is still in flight. Mirrors C EPICS `S_db_Blocked`; the
    /// CA server replies `ECA_PUTCBINPROG`.
    #[error("put callback in progress for record {0}")]
    PutCallbackInProgress(String),

    /// Server-emitted ECA status carried out-of-band on an otherwise
    /// data-shaped frame — used by libca `cac::eventAddRespAction`
    /// (`cac.cpp:973-977`) when a monitor frame's `m_cid` is non-
    /// NORMAL (e.g. `ECA_NORDACCESS` from `no_read_access_event`
    /// after an ACF reload). Routed to the per-subscription
    /// callback as `Err(CaError::ServerError(eca_status))` so the
    /// subscriber surfaces the status instead of seeing the bogus
    /// zeroed payload that travels with the frame.
    #[error("server reported ECA status {0:#06x}")]
    ServerError(u32),

    /// Request cannot be framed for the peer: it needs the extended
    /// (24-byte) CA header, and the peer's protocol version predates
    /// CA_V49, or the element count exceeds what the peer can carry.
    /// libca raises this locally — `comQueSend::insertRequestHeader`
    /// throws `cacChannel::outOfBounds()` (`comQueSend.cpp:299,313`)
    /// and `ca_array_get`/`ca_array_put` return `ECA_TOLARGE` — so no
    /// byte reaches the wire.
    #[error("request too large for the peer's CA protocol version (ECA_TOLARGE)")]
    TooLarge,

    /// Element count out of bounds for the request C would build. libca's
    /// put path throws `cacChannel::outOfBounds()` for an array that cannot
    /// fit the peer's message-body limit (`comQueSend.cpp:361`) or that
    /// needs an extended header the peer cannot parse
    /// (`comQueSend.cpp:313`); `oldChannelNotify.cpp:309,378,453` map that
    /// to `ECA_BADCOUNT`. Raised locally — no byte reaches the wire.
    #[error("element count out of bounds for this CA circuit (ECA_BADCOUNT)")]
    BadCount,
}

// ECA status constants (originally from protocol.rs, now in epics-ca-rs)
const ECA_TIMEOUT: u32 = 80; // defmsg(CA_K_WARNING, 10)
const ECA_NOWTACCESS: u32 = 376; // defmsg(CA_K_WARNING, 47)
const ECA_PUTFAIL: u32 = 160; // defmsg(CA_K_WARNING, 20)
const ECA_BADTYPE: u32 = 114; // defmsg(CA_K_ERROR, 14)
const ECA_DISCONN: u32 = 192; // defmsg(CA_K_WARNING, 24)
const ECA_PUTCBINPROG: u32 = 366; // defmsg(CA_K_ERROR, 45)
const ECA_TOLARGE: u32 = 72; // defmsg(CA_K_WARNING, 9)
const ECA_BADCOUNT: u32 = 176; // defmsg(CA_K_WARNING, 22)

impl CaError {
    pub fn to_eca_status(&self) -> u32 {
        match self {
            CaError::Timeout => ECA_TIMEOUT,
            CaError::ReadOnlyField(_) => ECA_NOWTACCESS,
            CaError::PutDisabled(_) => ECA_PUTFAIL,
            CaError::TypeMismatch(_) => ECA_BADTYPE,
            CaError::UnsupportedType(_) => ECA_BADTYPE,
            CaError::InvalidValue(_) => ECA_BADTYPE,
            CaError::FieldNotFound(_) => ECA_PUTFAIL,
            // Disconnection / shutdown are surfaced as ECA_DISCONN so a
            // downstream client (e.g. caput on a CA gateway whose
            // upstream just dropped) sees the actionable
            // "CA channel disconnected" message rather than the
            // catch-all "Put fail".
            CaError::Disconnected | CaError::Shutdown => ECA_DISCONN,
            // I/O errors usually mean the upstream connection is
            // wedged; mapping to ECA_DISCONN matches operator
            // expectations from C ca-gateway.
            CaError::Io(_) => ECA_DISCONN,
            CaError::WriteFailed(code) => *code,
            CaError::PutCallbackInProgress(_) => ECA_PUTCBINPROG,
            CaError::ServerError(code) => *code,
            CaError::TooLarge => ECA_TOLARGE,
            CaError::BadCount => ECA_BADCOUNT,
            _ => ECA_PUTFAIL,
        }
    }
}

pub type CaResult<T> = Result<T, CaError>;
