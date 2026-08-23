//! QSRV PUT trap-write (`asTrapWrite`) emission.
//!
//! The QSRV-typed face of the workspace's one put-log write-owner,
//! [`epics_base_rs::server::access_security::put_with_trap`]: this module
//! only reads the trap flag out of a [`WriteGrant`] and hands the write
//! down. caPutLog and site put-loggers attach to the hook underneath
//! (registered via
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

use epics_base_rs::server::access_security;
use epics_base_rs::types::EpicsValue;

pub(crate) use epics_base_rs::server::access_security::TrapWriteMeta;

use super::provider::WriteGrant;
use crate::error::BridgeResult;

/// Bracket one backing record PUT with the EPICS `asTrapWrite`
/// put-logging hook, then run and return the write's result.
///
/// `grant` is the SINGLE source of "is this a trapped write" — the
/// matched ACF/ASG rule's `TRAPWRITE` flag, decided once by the access
/// layer (see [`WriteGrant`]). Reading that one flag out of the grant is
/// all this wrapper does; the bracket itself lives in
/// [`access_security::put_with_trap`], shared with the native PVA source
/// so both servers emit the same put-log for the same write.
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
    access_security::put_with_trap(grant.rule_was_trap, meta, value, write).await
}

/// [`put_with_trap`]'s synchronous twin, for a write already holding the
/// member-record advisory gate (the QSRV atomic group PUT,
/// `already_locked` entries).
pub(crate) fn put_with_trap_already_locked<F>(
    grant: WriteGrant,
    meta: TrapWriteMeta<'_>,
    value: EpicsValue,
    write: F,
) -> BridgeResult<()>
where
    F: FnOnce(EpicsValue) -> BridgeResult<()>,
{
    access_security::put_with_trap_blocking(grant.rule_was_trap, meta, value, write)
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
