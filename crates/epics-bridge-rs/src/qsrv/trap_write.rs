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

// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.

use epics_base_rs::server::access_security::{self, TrapWriteFields, TrapWriteGuard};
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

    // One RAII guard owns the Before/After pair: BeforeWrite on
    // construction, AfterWrite on `complete` (normal path) OR on Drop
    // (this future cancelled mid-write — the QSRV connection/RPC torn
    // down while `write(...).await` is parked). The Drop arm is what
    // keeps the put-log balanced; the pre-guard code dispatched the
    // AfterWrite from an explicit call below the await, which a
    // cancellation skipped, leaving a BeforeWrite with no match. The C
    // invariant pairs every `asTrapWriteWithData` with one
    // `asTrapWriteAfter` on all exit paths; pvxs `SecurityLogger`'s
    // destructor enforces the same.
    let mut guard = TrapWriteGuard::begin(TrapWriteFields {
        pv_name: meta.pv_name.to_string(),
        user: meta.user.to_string(),
        host: meta.host.to_string(),
        peer: meta.peer.to_string(),
        value_str,
        dbr_type: meta.dbr_type,
        no_elements,
        event_id,
        rule_was_trap: true,
        cancel_status: "cancel".to_string(),
    });

    let result = write(value).await;

    guard.complete(if result.is_ok() { "ok" } else { "fail" });

    result
}

/// [`put_with_trap`]'s synchronous twin, for a write already holding the
/// member-record advisory gate (the QSRV atomic group PUT, `already_locked`
/// entries). C's `SecurityLogger` bracket (`pvxs/ioc/securitylogger.h:23-59`)
/// is plain synchronous C++ with no `async` concept at all — this is that
/// shape. `write` cannot itself suspend (its `_already_locked` callees are
/// synchronous post-H6), so there is no cancellation-mid-write case as in
/// [`put_with_trap`]; the same RAII guard still balances the trap log on a
/// panic unwinding through `write`.
pub(crate) fn put_with_trap_already_locked<F>(
    grant: WriteGrant,
    meta: TrapWriteMeta<'_>,
    value: EpicsValue,
    write: F,
) -> BridgeResult<()>
where
    F: FnOnce(EpicsValue) -> BridgeResult<()>,
{
    if !(grant.rule_was_trap && access_security::has_trap_write_listeners()) {
        return write(value);
    }

    let value_str = value.display_truncated(64);
    let no_elements = value.count();
    let event_id = access_security::next_trap_write_event_id();

    let mut guard = TrapWriteGuard::begin(TrapWriteFields {
        pv_name: meta.pv_name.to_string(),
        user: meta.user.to_string(),
        host: meta.host.to_string(),
        peer: meta.peer.to_string(),
        value_str,
        dbr_type: meta.dbr_type,
        no_elements,
        event_id,
        rule_was_trap: true,
        cancel_status: "cancel".to_string(),
    });

    let result = write(value);

    guard.complete(if result.is_ok() { "ok" } else { "fail" });

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_base_rs::server::access_security::{
        TrapWriteListenerHandle, TrapWriteOp, register_trap_write_listener,
    };
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    type Captured = Arc<Mutex<Vec<(TrapWriteOp, Option<String>)>>>;

    /// Capture (op, owned-status) for events on `pv` only, so the
    /// process-global listener registry shared with other tests does not
    /// pollute the assertion.
    fn capture(pv: &'static str) -> (Captured, TrapWriteListenerHandle) {
        let events: Captured = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let handle = register_trap_write_listener(Arc::new(move |msg| {
            if msg.pv_name == pv {
                sink.lock()
                    .unwrap()
                    .push((msg.op, msg.status.map(str::to_owned)));
            }
        }));
        (events, handle)
    }

    fn meta(pv: &str) -> TrapWriteMeta<'_> {
        TrapWriteMeta {
            pv_name: pv,
            user: "u",
            host: "h",
            peer: "h:5075",
            dbr_type: 5,
        }
    }

    /// A trapped put whose backing write is cancelled mid-`.await` (the
    /// QSRV connection/RPC torn down) must still emit AfterWrite — the
    /// guard's Drop arm fires it with the cancel status. Pre-guard this
    /// path emitted only BeforeWrite, leaving the put-log unbalanced.
    #[tokio::test]
    async fn after_fires_when_write_future_cancelled() {
        let (events, _handle) = capture("trap:cancel");
        let grant = WriteGrant {
            allowed: true,
            rule_was_trap: true,
        };
        let fut = put_with_trap(
            grant,
            meta("trap:cancel"),
            EpicsValue::Long(42),
            |_v| async {
                // never completes — models record processing still running
                // when the client disconnects.
                std::future::pending::<BridgeResult<()>>().await
            },
        );
        // `timeout` polls `fut` (firing BeforeWrite) then drops it when
        // the timer elapses (the inner write never resolves), running
        // the guard's Drop.
        let elapsed = tokio::time::timeout(Duration::from_millis(20), fut).await;
        assert!(elapsed.is_err(), "write should not have completed");

        let got = events.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![
                (TrapWriteOp::BeforeWrite, None),
                (TrapWriteOp::AfterWrite, Some("cancel".to_string())),
            ]
        );
    }

    /// The normal path still emits exactly one AfterWrite carrying the
    /// real put status (here "ok"), and no extra cancel AfterWrite when
    /// the guard drops after `complete`.
    #[tokio::test]
    async fn after_fires_once_ok_on_normal_completion() {
        let (events, _handle) = capture("trap:ok");
        let grant = WriteGrant {
            allowed: true,
            rule_was_trap: true,
        };
        put_with_trap(grant, meta("trap:ok"), EpicsValue::Long(7), |_v| async {
            Ok(())
        })
        .await
        .unwrap();

        let got = events.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![
                (TrapWriteOp::BeforeWrite, None),
                (TrapWriteOp::AfterWrite, Some("ok".to_string())),
            ]
        );
    }
}
