use thiserror::Error;

#[derive(Error, Debug)]
pub enum PvaError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("timeout waiting for response")]
    Timeout,

    /// A `wait` was woken by [`crate::client_native::PvaOperation::interrupt`]
    /// rather than by a deadline. pvxs distinguishes
    /// `Interrupted` from `Timeout`, and conflating them hid the cause
    /// (operator-driven wake-up vs. real deadline) from callers. The
    /// underlying operation keeps running and its result stays
    /// recoverable by a later `wait`.
    #[error("operation wait interrupted")]
    Interrupted,

    #[error("channel not found: {0}")]
    ChannelNotFound(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("connection refused")]
    ConnectionRefused,

    #[error("invalid value: {0}")]
    InvalidValue(String),

    #[error("decode error: {0}")]
    Decode(String),

    // Discrete exception variants — additive companions to the
    // discrete types defined in `errors::*`. New code should prefer
    // returning the discrete struct (then `?` into PvaError via the
    // `From` impls); existing call sites can keep matching the enum
    // until migration finishes. (`Interrupted` already exists above.)
    /// Channel was previously connected but the underlying TCP
    /// virtual circuit dropped. Mirrors pvxs `client::Disconnect`.
    #[error("channel disconnected")]
    Disconnected,

    /// Server emitted a `CMD_MESSAGE` / op-error reply rather than
    /// the expected response payload. Mirrors pvxs `RemoteError`.
    /// Distinct from [`PvaError::Protocol`] (which is reserved for
    /// framing / decoder errors detected on this side).
    #[error("remote error: {0}")]
    RemoteError(String),

    /// Sentinel returned when a long-lived operation (monitor /
    /// streaming RPC) completed normally. Mirrors pvxs `Finished`.
    #[error("operation finished")]
    Finished,

    /// Sentinel for connection-state callbacks (`onConnect`).
    /// Returned only via the discrete [`errors::Connected`] event
    /// type — not produced by client operations directly.
    #[error("channel connected")]
    Connected,
}

pub type PvaResult<T> = Result<T, PvaError>;

/// Discrete exception types matching pvxs's per-cause exception
/// classes — additive companions giving callers fine-grained control
/// without breaking the existing [`PvaError`] enum-based surface.
///
/// New code paths that need to distinguish causes precisely should:
///
/// 1. Return one of the structs in this module (or layer them inside
///    a `Result<T, errors::Disconnect>` etc).
/// 2. Use `?` to propagate; the [`From`] impls below promote each
///    discrete type into the matching [`PvaError`] variant so older
///    `Result<T, PvaError>` consumers keep working unchanged.
///
/// Existing code does NOT need to migrate — every variant of
/// [`PvaError`] still exists; nothing is renamed or removed.
pub mod errors {
    use thiserror::Error;

    /// Channel was previously connected but the underlying TCP
    /// virtual circuit dropped. Carries the optional peer address
    /// for diagnostic logging.
    #[derive(Error, Debug, Clone)]
    #[error("channel disconnected{}", .reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default())]
    pub struct Disconnect {
        pub reason: Option<String>,
    }

    impl Disconnect {
        pub fn new() -> Self {
            Self { reason: None }
        }
        pub fn with_reason(reason: impl Into<String>) -> Self {
            Self {
                reason: Some(reason.into()),
            }
        }
    }

    impl Default for Disconnect {
        fn default() -> Self {
            Self::new()
        }
    }

    /// Server-emitted error (CMD_MESSAGE error-level or op-error
    /// reply). Carries the message verbatim.
    #[derive(Error, Debug, Clone)]
    #[error("remote error: {message}")]
    pub struct RemoteError {
        pub message: String,
    }

    impl RemoteError {
        pub fn new(message: impl Into<String>) -> Self {
            Self {
                message: message.into(),
            }
        }
    }

    /// Cooperative cancel signal from the caller side.
    #[derive(Error, Debug, Clone, Copy, Default)]
    #[error("operation interrupted")]
    pub struct Interrupted;

    /// Sentinel — long-lived op (monitor / streaming RPC) drained
    /// to its natural end without an error.
    #[derive(Error, Debug, Clone, Copy, Default)]
    #[error("operation finished")]
    pub struct Finished;

    /// Sentinel — channel transitioned to Connected state. Used
    /// in `ConnectBuilder::onConnect` callback chains; never
    /// produced by an operation's `Result`.
    #[derive(Error, Debug, Clone, Copy, Default)]
    #[error("channel connected")]
    pub struct Connected;

    /// Op-level timeout. Distinct from io-level timeout (which
    /// surfaces via `std::io::Error`).
    #[derive(Error, Debug, Clone, Copy, Default)]
    #[error("timeout waiting for response")]
    pub struct Timeout;
}

// --- Discrete → PvaError promotions ------------------------------

impl From<errors::Disconnect> for PvaError {
    fn from(_: errors::Disconnect) -> Self {
        // Reason is dropped on enum promotion; callers needing the
        // textual cause should keep the discrete struct in their
        // own Result and only `?`-promote at the public boundary.
        PvaError::Disconnected
    }
}

impl From<errors::RemoteError> for PvaError {
    fn from(e: errors::RemoteError) -> Self {
        PvaError::RemoteError(e.message)
    }
}

impl From<errors::Interrupted> for PvaError {
    fn from(_: errors::Interrupted) -> Self {
        PvaError::Interrupted
    }
}

impl From<errors::Finished> for PvaError {
    fn from(_: errors::Finished) -> Self {
        PvaError::Finished
    }
}

impl From<errors::Connected> for PvaError {
    fn from(_: errors::Connected) -> Self {
        PvaError::Connected
    }
}

impl From<errors::Timeout> for PvaError {
    fn from(_: errors::Timeout) -> Self {
        PvaError::Timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discrete_disconnect_with_reason_promotes() {
        let d = errors::Disconnect::with_reason("server restarted");
        // The textual reason is on the discrete struct...
        assert_eq!(d.reason.as_deref(), Some("server restarted"));
        // ...but the enum form drops it (matches enum size budget).
        let e: PvaError = d.into();
        assert!(matches!(e, PvaError::Disconnected));
    }

    #[test]
    fn discrete_remote_error_carries_message() {
        let r = errors::RemoteError::new("ECA_NOACCESS");
        let e: PvaError = r.into();
        match e {
            PvaError::RemoteError(m) => assert_eq!(m, "ECA_NOACCESS"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn discrete_sentinel_types_round_trip_via_question_mark() {
        // The whole point of the From impls — `?` from a discrete
        // Result lifts cleanly into PvaError.
        fn returns_interrupt() -> Result<(), errors::Interrupted> {
            Err(errors::Interrupted)
        }
        fn outer() -> PvaResult<()> {
            returns_interrupt()?;
            Ok(())
        }
        assert!(matches!(outer(), Err(PvaError::Interrupted)));
    }

    #[test]
    fn finished_and_connected_have_distinct_promotions() {
        let f: PvaError = errors::Finished.into();
        let c: PvaError = errors::Connected.into();
        assert!(matches!(f, PvaError::Finished));
        assert!(matches!(c, PvaError::Connected));
    }

    #[test]
    fn discrete_disconnect_default_has_no_reason() {
        let d = errors::Disconnect::default();
        assert!(d.reason.is_none());
        assert_eq!(d.to_string(), "channel disconnected");
    }

    #[test]
    fn discrete_disconnect_with_reason_displays_reason() {
        let d = errors::Disconnect::with_reason("circuit closed");
        assert_eq!(d.to_string(), "channel disconnected: circuit closed");
    }
}
