//! QSRV PUT trap-write (`asTrapWrite`) emission.
//!
//! The single write-owner that brackets every QSRV record PUT with the
//! EPICS access-security put-logging hook. caPutLog and site put-loggers
//! attach to this hook (registered via
//! [`epics_base_rs::server::access_security::register_trap_write_listener`]).
//!
//! Parity reference — pvxs wraps each QSRV put in a `SecurityLogger`
//! whose constructor calls
//! `asTrapWriteWithData(cred.account, cred.peer, pChan, type, count, pvalue)`
//! and whose destructor calls `asTrapWriteAfterWrite`
//! (`pvxs/ioc/securitylogger.h:23-59`), built once per put in
//! `IOCSource::put` (`pvxs/ioc/singlesource.cpp:354-360`,
//! `pvxs/ioc/iocsource.cpp:363-374`) and once per group field in
//! `OnPut::onPut` (`pvxs/ioc/groupsource.cpp:594-602`). One logger maps
//! to one Before/After pair around one backing write.
//!
//! Gating — C `asTrapWriteWithData` (`libcom/src/as/asLib.h:57-60`)
//! dispatches the listeners only when the matched ACF/ASG rule's
//! `trapMask` is set:
//!
//! ```c
//! #define asTrapWriteWithData(asClientPvt, user, host, addr, type, count, data) \
//!     ((asActive && (asClientPvt)->trapMask) \
//!     ? asTrapWriteBeforeWithData(...) : 0)
//! #define asTrapWriteAfter(pvt) \
//!     if (pvt) asTrapWriteAfterWrite(pvt)
//! ```
//!
//! A non-trapped (or `NOTRAPWRITE`) put dispatches nothing — the Before
//! returns NULL, so `asTrapWriteAfter` is also a no-op. That trap
//! decision is the [`WriteGrant::rule_was_trap`] flag, resolved once by
//! the access layer; this helper reads only that flag and never
//! re-derives the trap mask at the emission site.

use epics_base_rs::server::access_security::{self, TrapWriteMessage, TrapWriteOp};
use epics_base_rs::types::EpicsValue;

use super::provider::WriteGrant;
use crate::error::BridgeResult;

/// Per-write trap-log identity that does not depend on the value being
/// written. Borrows the caller's identity strings (matching the C
/// `asTrapWriteMessage` by-reference lifetime, `asLib.h:34-56`).
pub(crate) struct TrapWriteMeta<'a> {
    /// The channel (record.FIELD) being written — pvxs passes
    /// `dbChannelName(pChan)`.
    pub pv_name: &'a str,
    /// Authenticated account name (pvxs `cred->account`).
    pub user: &'a str,
    /// Client host (pvxs `cred->host`).
    pub host: &'a str,
    /// Client peer ("ip:port"). The QSRV [`super::provider::AccessContext`]
    /// does not carry the socket peer separately, so callers pass the
    /// connection host — the closest available identity.
    pub peer: &'a str,
    /// Final field DBR type of the channel (pvxs `dbChannelFinalCAType`).
    pub dbr_type: u16,
}

/// Bracket one backing record PUT with the EPICS `asTrapWrite`
/// put-logging hook, then run and return the write's result.
///
/// `grant` is the SINGLE source of "is this a trapped write" — the
/// matched ACF/ASG rule's `TRAPWRITE` flag, decided once by the access
/// layer (see [`WriteGrant`]). When the grant is not trapped, or no
/// listener is registered, the write runs unbracketed and nothing is
/// dispatched (the C `asActive && trapMask` gate, `asLib.h:57`).
///
/// On a trapped write this emits exactly one `BeforeWrite` (before the
/// put) and exactly one `AfterWrite` (after the put completes, on every
/// exit path) carrying the same `event_id`, value string, and `ok`/`fail`
/// status. The value is rendered once (truncated to 64 elements, like
/// the CA dispatcher) only when actually emitting.
pub(crate) async fn put_with_trap<F, Fut>(
    grant: WriteGrant,
    meta: TrapWriteMeta<'_>,
    value: EpicsValue,
    write: F,
) -> BridgeResult<()>
where
    F: FnOnce(EpicsValue) -> Fut,
    Fut: std::future::Future<Output = BridgeResult<()>>,
{
    // The grant alone decides trap; `has_trap_write_listeners` only
    // skips the value render + dispatch cost when nothing would consume
    // the event (mirrors the C `asActive` half of the gate).
    if !(grant.rule_was_trap && access_security::has_trap_write_listeners()) {
        return write(value).await;
    }

    let value_str = value.display_truncated(64);
    let no_elements = value.count();
    let event_id = access_security::next_trap_write_event_id();

    // `TrapWriteMessage` is `Copy`; reuse one record for the Before/After
    // pair, flipping only `op`/`status`, so the pair carries identical
    // identity and the shared `event_id` by construction.
    let mut msg = TrapWriteMessage {
        op: TrapWriteOp::BeforeWrite,
        pv_name: meta.pv_name,
        user: meta.user,
        host: meta.host,
        peer: meta.peer,
        value_str: &value_str,
        dbr_type: meta.dbr_type,
        no_elements,
        event_id,
        status: None,
        rule_was_trap: true,
    };
    access_security::dispatch_trap_write(&msg);

    let result = write(value).await;

    msg.op = TrapWriteOp::AfterWrite;
    msg.status = Some(if result.is_ok() { "ok" } else { "fail" });
    access_security::dispatch_trap_write(&msg);

    result
}
