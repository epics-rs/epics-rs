//! Public op-handle surface for custom [`super::source::ChannelSource`]
//! authors.
//!
//! Mirrors the pvxs `Source::onCreate` op-handle types:
//!
//! - [`OpBase`] — common surface (peer credentials, op name, isOpen).
//! - [`ExecOp`] — per-op (Get / Put / RPC) handle with `reply(value)`,
//!   `error(msg)`, `info(msg)`.
//! - [`RemoteLogger`] — `info()` / `warn()` / `error()` message
//!   sender that produces `CMD_MESSAGE` frames (when wired) or
//!   tracing events (when not).
//! - [`ClientCredentials`] — type alias for the existing
//!   [`super::source::ChannelContext`]. Server-side view of the
//!   authenticated client.
//!
//! The runtime today doesn't route op replies through these handles —
//! the existing [`super::source::ChannelSource`] trait owns that
//! responsibility. These types are provided for source authors who
//! want pvxs-shape APIs and for code that translates pvxs sources to
//! Rust. Attaching an [`ExecOp`] to the actual wire dispatch is opt-in:
//! callers wire the underlying [`tokio::sync::mpsc::Sender`] /
//! [`tokio::sync::oneshot::Sender`] themselves.
//!
//! That includes [`RemoteLogger`]: it is the pvxs-shaped *ergonomic*
//! wrapper, not the runtime's diagnostic path. The wired source→client
//! diagnostic channel a [`super::source::ChannelSource`] gets for free
//! is [`super::source::RemoteLog`], carried on every
//! [`super::source::ChannelContext`] and drained by the connection into
//! IOID-tagged `CMD_MESSAGE` frames before the operation's reply.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::pvdata::{FieldDesc, PvField};

pub use super::source::ChannelContext as ClientCredentials;

/// Server→client log-message severity. Maps to the `mtype` byte in the
/// `CMD_MESSAGE` *payload* (info=0, warn=1, error=2), which follows the
/// `ioid` — not the message-header flags/subcommand byte. See pvxs
/// `ServerConn::logRemote` (`serverconn.cpp`): `to_wire(ioid)` then
/// `to_wire(level2mtype(lvl))` then `to_wire(msg)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageLevel {
    Info,
    Warn,
    Error,
}

impl MessageLevel {
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Info => 0,
            Self::Warn => 1,
            Self::Error => 2,
        }
    }
}

/// Server→client log message. Emitted via [`RemoteLogger`] /
/// [`ExecOp::info`] / `warn` / `error`. The runtime drains these onto
/// the connection's outbound writer as `CMD_MESSAGE` frames, whose
/// payload is `ioid` (the operation the message pertains to), then the
/// [`MessageLevel`] `mtype` byte, then the message string.
#[derive(Debug, Clone)]
pub struct OpMessage {
    /// ioid of the operation this message is about — the first field of
    /// the `CMD_MESSAGE` payload. Without it the frame cannot be
    /// associated with a request.
    pub ioid: u32,
    pub level: MessageLevel,
    pub message: String,
}

/// `RemoteLogger` mixin. Source authors construct one with either a
/// real `mpsc::Sender<OpMessage>` (wires to the connection's outbox)
/// or with [`Self::log_only`] (falls back to `tracing` events —
/// useful for tests and for sources that don't have access to the
/// outbound queue).
#[derive(Clone)]
pub struct RemoteLogger {
    sender: Option<tokio::sync::mpsc::Sender<OpMessage>>,
    /// ioid stamped on every [`OpMessage`] this logger emits — the
    /// operation the messages pertain to. One logger per op.
    ioid: u32,
    /// Tag attached to fallback tracing events; usually the PV name
    /// or operation identifier.
    pub tag: Arc<str>,
}

impl RemoteLogger {
    /// Wire the logger to a connection-side outbox. Messages are
    /// shipped as PVA `CMD_MESSAGE` frames by the runtime drain task,
    /// each stamped with `ioid` (the operation they belong to).
    pub fn new(
        sender: tokio::sync::mpsc::Sender<OpMessage>,
        ioid: u32,
        tag: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            sender: Some(sender),
            ioid,
            tag: tag.into(),
        }
    }

    /// Construct a logger with no wire path — `info`/`warn`/`error`
    /// surface as `tracing` events instead of `CMD_MESSAGE` frames.
    /// Useful for unit tests and for default-impl source authors who
    /// want pvxs-shape API without re-architecting their op flow.
    /// `ioid` is unused on this path (no frame is built).
    pub fn log_only(tag: impl Into<Arc<str>>) -> Self {
        Self {
            sender: None,
            ioid: 0,
            tag: tag.into(),
        }
    }

    pub fn info(&self, msg: impl Into<String>) {
        self.emit(MessageLevel::Info, msg.into());
    }

    pub fn warn(&self, msg: impl Into<String>) {
        self.emit(MessageLevel::Warn, msg.into());
    }

    pub fn error(&self, msg: impl Into<String>) {
        self.emit(MessageLevel::Error, msg.into());
    }

    fn emit(&self, level: MessageLevel, message: String) {
        if let Some(tx) = &self.sender {
            // Wired path: ship to the connection outbox as a
            // CMD_MESSAGE. On success we're done — a delivered message
            // is not also written to local tracing (pvxs logRemote is
            // wire-only). Only a failed send (closing connection) falls
            // through to tracing so the message isn't lost entirely.
            if tx
                .try_send(OpMessage {
                    ioid: self.ioid,
                    level,
                    message: message.clone(),
                })
                .is_ok()
            {
                return;
            }
        }
        match level {
            MessageLevel::Info => tracing::info!(tag = %self.tag, "{}", message),
            MessageLevel::Warn => tracing::warn!(tag = %self.tag, "{}", message),
            MessageLevel::Error => tracing::error!(tag = %self.tag, "{}", message),
        }
    }
}

/// Common base for op handles. Every Get / Put / RPC op the server
/// hands a source author exposes this surface.
#[derive(Clone)]
pub struct OpBase {
    /// PV name the operation targets.
    pub name: Arc<str>,
    /// Authenticated peer info — Account / method / host / addr.
    pub credentials: ClientCredentials,
    /// Op-level open flag. Goes false on cancel, drop, or completion.
    /// Source authors check before reply()-ing into a handle whose
    /// owner has gone away.
    is_open: Arc<AtomicBool>,
    logger: RemoteLogger,
}

impl OpBase {
    pub fn new(
        name: impl Into<Arc<str>>,
        credentials: ClientCredentials,
        logger: RemoteLogger,
    ) -> Self {
        Self {
            name: name.into(),
            credentials,
            is_open: Arc::new(AtomicBool::new(true)),
            logger,
        }
    }

    pub fn is_open(&self) -> bool {
        self.is_open.load(Ordering::Acquire)
    }

    /// Mark the op closed. Idempotent. Subsequent `reply()` /
    /// `error()` calls become no-ops.
    pub fn close(&self) {
        self.is_open.store(false, Ordering::Release);
    }

    /// Borrow the [`RemoteLogger`] for `info` / `warn` / `error` use.
    pub fn logger(&self) -> &RemoteLogger {
        &self.logger
    }
}

/// Per-operation handle. Mirrors pvxs `ExecOp`. A source author
/// receives one of these per Get / Put / RPC and either:
///
/// - calls [`Self::reply`] with the result value,
/// - calls [`Self::error`] with a message (server emits an op-error
///   reply), or
/// - calls [`Self::info`] / `warn` to send a `CMD_MESSAGE` without
///   finishing the op (status-only feedback).
///
/// The reply path wires through a `tokio::sync::oneshot::Sender` —
/// source authors take the [`Self::take_reply_tx`] handle and feed it
/// directly when they want bypass control of the lifetime.
pub struct ExecOp {
    base: OpBase,
    reply_tx: Option<tokio::sync::oneshot::Sender<ExecResult>>,
}

/// Outcome of an [`ExecOp`]. Value-bearing for Get / RPC; for Put
/// the value can be empty (server only cares about the error tag).
#[derive(Debug)]
pub enum ExecResult {
    Ok {
        introspection: Option<FieldDesc>,
        value: PvField,
    },
    Empty,
    Err(String),
}

impl ExecOp {
    pub fn new(base: OpBase, reply_tx: tokio::sync::oneshot::Sender<ExecResult>) -> Self {
        Self {
            base,
            reply_tx: Some(reply_tx),
        }
    }

    pub fn base(&self) -> &OpBase {
        &self.base
    }

    pub fn name(&self) -> &str {
        &self.base.name
    }

    pub fn credentials(&self) -> &ClientCredentials {
        &self.base.credentials
    }

    pub fn is_open(&self) -> bool {
        self.base.is_open()
    }

    /// Respond with a value. Idempotent: subsequent calls are no-ops.
    pub fn reply(&mut self, introspection: Option<FieldDesc>, value: PvField) {
        if let Some(tx) = self.reply_tx.take() {
            self.base.close();
            let _ = tx.send(ExecResult::Ok {
                introspection,
                value,
            });
        }
    }

    /// Respond with no value (Put-style ack).
    pub fn reply_empty(&mut self) {
        if let Some(tx) = self.reply_tx.take() {
            self.base.close();
            let _ = tx.send(ExecResult::Empty);
        }
    }

    /// Respond with an error string. The server packages this into the
    /// op-error reply payload (the `Status` of the operation's reply) —
    /// matching pvxs `ExecOp::error`, which resolves the op and does
    /// **not** also send a separate `CMD_MESSAGE`. A source author who
    /// wants an out-of-band log line as well calls [`Self::warn`] /
    /// the logger explicitly. Idempotent.
    pub fn error(&mut self, msg: impl Into<String>) {
        if let Some(tx) = self.reply_tx.take() {
            self.base.close();
            let _ = tx.send(ExecResult::Err(msg.into()));
        }
    }

    /// Send an info-level `CMD_MESSAGE` without finishing the op.
    pub fn info(&self, msg: impl Into<String>) {
        self.base.logger.info(msg);
    }

    /// Send a warn-level `CMD_MESSAGE` without finishing the op.
    pub fn warn(&self, msg: impl Into<String>) {
        self.base.logger.warn(msg);
    }

    /// Take the reply oneshot so the caller can hand it elsewhere
    /// (e.g. spawn a background task that resolves the op). The handle
    /// is now consumed — subsequent `reply` / `error` calls become
    /// no-ops.
    pub fn take_reply_tx(&mut self) -> Option<tokio::sync::oneshot::Sender<ExecResult>> {
        self.reply_tx.take()
    }
}

impl Drop for ExecOp {
    fn drop(&mut self) {
        // If the op was never resolved, surface as an Err so the
        // caller's awaiter doesn't deadlock waiting for a reply that
        // will never come.
        if let Some(tx) = self.reply_tx.take() {
            let _ = tx.send(ExecResult::Err(
                "ExecOp dropped without reply()".to_string(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn dummy_creds() -> ClientCredentials {
        ClientCredentials {
            peer: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1),
            creds: std::sync::Arc::new(crate::server_native::config::ClientCredentials {
                account: "anonymous".into(),
                method: "anonymous".into(),
                host: "localhost".into(),
                authority: String::new(),
                roles: Vec::new(),
            }),
            pv_request: None,
            log: Default::default(),
        }
    }

    #[epics_macros_rs::epics_test]
    async fn exec_op_reply_delivers_value() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let logger = RemoteLogger::log_only("TEST:PV");
        let base = OpBase::new("TEST:PV", dummy_creds(), logger);
        let mut op = ExecOp::new(base, tx);
        let value = PvField::Scalar(crate::pvdata::ScalarValue::Int(42));
        op.reply(None, value);
        match rx.await.unwrap() {
            ExecResult::Ok { value, .. } => {
                assert!(matches!(
                    value,
                    PvField::Scalar(crate::pvdata::ScalarValue::Int(42))
                ));
            }
            other => panic!("unexpected reply: {other:?}"),
        }
        assert!(!op.is_open());
    }

    #[epics_macros_rs::epics_test]
    async fn exec_op_error_closes_and_delivers_message() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let logger = RemoteLogger::log_only("TEST:PV");
        let base = OpBase::new("TEST:PV", dummy_creds(), logger);
        let mut op = ExecOp::new(base, tx);
        op.error("nope");
        match rx.await.unwrap() {
            ExecResult::Err(msg) => assert_eq!(msg, "nope"),
            other => panic!("unexpected reply: {other:?}"),
        }
        assert!(!op.is_open());
    }

    #[epics_macros_rs::epics_test]
    async fn exec_op_drop_without_reply_emits_err() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let logger = RemoteLogger::log_only("TEST:PV");
        let base = OpBase::new("TEST:PV", dummy_creds(), logger);
        let op = ExecOp::new(base, tx);
        drop(op);
        match rx.await.unwrap() {
            ExecResult::Err(msg) => assert!(msg.contains("dropped")),
            other => panic!("unexpected reply: {other:?}"),
        }
    }

    #[epics_macros_rs::epics_test]
    async fn remote_logger_with_sender_delivers_to_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<OpMessage>(8);
        let logger = RemoteLogger::new(tx, 77, "TEST:PV");
        logger.info("a");
        logger.warn("b");
        logger.error("c");
        let m1 = rx.recv().await.unwrap();
        let m2 = rx.recv().await.unwrap();
        let m3 = rx.recv().await.unwrap();
        assert_eq!(m1.level, MessageLevel::Info);
        assert_eq!(m2.level, MessageLevel::Warn);
        assert_eq!(m3.level, MessageLevel::Error);
        assert_eq!(m1.message, "a");
        // Each frame carries the op's ioid (first CMD_MESSAGE payload
        // field) so the client can associate it with the request.
        assert_eq!(m1.ioid, 77);
        assert_eq!(m3.ioid, 77);
    }

    #[epics_macros_rs::epics_test]
    async fn exec_op_error_does_not_emit_side_channel_message() {
        // ExecOp::error resolves the op via its reply (Status), and must
        // NOT also push a CMD_MESSAGE — pvxs sends only the op-error
        // reply. The wired logger's channel therefore stays empty.
        let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel::<OpMessage>(8);
        let logger = RemoteLogger::new(msg_tx, 5, "TEST:PV");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let base = OpBase::new("TEST:PV", dummy_creds(), logger);
        let mut op = ExecOp::new(base, tx);
        op.error("nope");
        assert!(matches!(rx.await.unwrap(), ExecResult::Err(m) if m == "nope"));
        assert!(
            msg_rx.try_recv().is_err(),
            "error() must not emit a separate CMD_MESSAGE"
        );
    }

    #[epics_macros_rs::epics_test]
    async fn remote_logger_log_only_drops_silently() {
        // Should not panic. Tracing output is implementation detail.
        let logger = RemoteLogger::log_only("TEST:PV");
        logger.info("hi");
        logger.warn("warning");
        logger.error("oops");
    }

    #[test]
    fn message_level_byte_codes_match_wire() {
        assert_eq!(MessageLevel::Info.as_u8(), 0);
        assert_eq!(MessageLevel::Warn.as_u8(), 1);
        assert_eq!(MessageLevel::Error.as_u8(), 2);
    }
}
