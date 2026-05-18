//! BridgeMonitor: bridges DbSubscription to PVA monitor.
//!
//! Corresponds to C++ QSRV's `PDBSingleMonitor` / `BaseMonitor`.
//!
//! Uses `DbSubscription::recv_snapshot()` to receive full Snapshot data
//! (alarm, display, control, enums) — not just the raw value.
//!
//! On `start()`, reads the current record state and stores it as an
//! initial snapshot, matching C++ BaseMonitor::connect() behavior.
//!
//! Tracks overflow events via a counter, corresponding to C++ BaseMonitor's
//! `inoverflow` flag and overflow BitSet.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::recgbl::EventMask;
use epics_pva_rs::pvdata::PvStructure;

use super::provider::{AccessContext, PvaMonitor};
use super::pvif::{NtType, snapshot_to_pv_structure};
use crate::error::{BridgeError, BridgeResult};

/// A PVA monitor backed by a DbSubscription for a single record.
///
/// Tracks overflow statistics: when the internal mpsc channel is full,
/// events are dropped. The `overflow_count` tracks how many events
/// were lost (corresponds to C++ BaseMonitor's overflow BitSet).
///
/// Carries an [`AccessContext`] so monitor read permission is enforced
/// in `start()`. Without this, a downstream client denied via `get()`
/// could still receive value updates by subscribing.
pub struct BridgeMonitor {
    db: Arc<PvDatabase>,
    record_name: String,
    nt_type: NtType,
    /// VALUE | ALARM subscription — matches pvxs QSRV default DBE mask
    /// (singlesource.cpp:142). Replaces the previous VALUE | LOG default
    /// so a no-options PVA monitor sees alarm transitions but not the
    /// archive-only LOG events that PVA clients never asked for.
    subscription: Option<DbSubscription>,
    /// Separate PROPERTY-only subscription — pvxs QSRV creates two
    /// `dbChannel`s per single-record monitor (singlesource.cpp:161): one
    /// for value/alarm and one for display/control/enum metadata changes.
    /// Without the second subscription, downstream PVA clients never see
    /// EGU / HOPR / LOPR / enum-string updates pushed through a monitor.
    property_subscription: Option<DbSubscription>,
    running: bool,
    /// Initial complete snapshot sent on first poll() after start().
    initial_snapshot: Option<PvStructure>,
    /// Number of monitor events lost due to overflow.
    overflow_count: Arc<AtomicU64>,
    /// Access control context for read enforcement on start().
    access: AccessContext,
}

impl BridgeMonitor {
    pub fn new(db: Arc<PvDatabase>, record_name: String, nt_type: NtType) -> Self {
        Self {
            db,
            record_name,
            nt_type,
            subscription: None,
            property_subscription: None,
            running: false,
            initial_snapshot: None,
            overflow_count: Arc::new(AtomicU64::new(0)),
            access: AccessContext::allow_all(),
        }
    }

    /// Inject an access control context. The PVA server (or `BridgeChannel`'s
    /// own create_monitor) calls this to propagate the channel's identity
    /// into the monitor.
    pub fn with_access(mut self, access: AccessContext) -> Self {
        self.access = access;
        self
    }

    /// Get the number of overflow events (events lost due to queue full).
    pub fn overflow_count(&self) -> u64 {
        self.overflow_count.load(Ordering::Relaxed)
    }
}

impl PvaMonitor for BridgeMonitor {
    async fn start(&mut self) -> BridgeResult<()> {
        if self.running {
            return Ok(());
        }

        // Read enforcement: a client without read permission must not be
        // allowed to subscribe to monitor events either.
        if !self.access.can_read(&self.record_name) {
            return Err(BridgeError::PutRejected(format!(
                "monitor read denied for {} (user='{}' host='{}')",
                self.record_name, self.access.user, self.access.host
            )));
        }

        // BR-R36: pvxs QSRV default mask is VALUE | ALARM, not
        // VALUE | LOG. Subscribe explicitly so the Bridge does
        // not inherit DbSubscription::subscribe's CA-leaning
        // VALUE|LOG default, which would deliver archive-LOG
        // events while missing alarm transitions.
        let value_alarm_mask = (EventMask::VALUE | EventMask::ALARM).bits();
        let sub =
            DbSubscription::subscribe_with_mask(&self.db, &self.record_name, 0, value_alarm_mask)
                .await
                .ok_or_else(|| BridgeError::RecordNotFound(self.record_name.clone()))?;

        // BR-R36: pvxs QSRV opens a second subscription with the
        // PROPERTY mask (singlesource.cpp:161) so a PVA monitor
        // is woken when EGU / HOPR / LOPR / enum-string change,
        // not just when VAL changes. The full snapshot is rebuilt
        // on every wake so the property-channel firing alone is
        // enough to push fresh metadata down the wire.
        let property_sub = DbSubscription::subscribe_with_mask(
            &self.db,
            &self.record_name,
            0,
            EventMask::PROPERTY.bits(),
        )
        .await
        .ok_or_else(|| BridgeError::RecordNotFound(self.record_name.clone()))?;

        // The native PVA server emits the initial snapshot via
        // ChannelSource::get_value() at MONITOR INIT time (server_native/
        // tcp.rs build_monitor_payload). Caching another initial snapshot
        // here would deliver it twice — visible to clients tracking
        // event counts (archiver appliance) and surfaced in `pvmonitor`
        // as a duplicate timestamp on the first event. Leave
        // initial_snapshot None.

        self.subscription = Some(sub);
        self.property_subscription = Some(property_sub);
        self.running = true;
        Ok(())
    }

    async fn poll(&mut self) -> Option<PvStructure> {
        // Return initial snapshot on first poll (C++ BaseMonitor::connect behavior)
        if let Some(initial) = self.initial_snapshot.take() {
            return Some(initial);
        }

        // BR-R36: wake on either the VALUE|ALARM subscription or
        // the PROPERTY subscription — whichever the record posts
        // first. snapshot_to_pv_structure rebuilds the full NT
        // structure (with display/control/enums) on every wake,
        // so either firing pushes fresh metadata to the client.
        match (
            self.subscription.as_mut(),
            self.property_subscription.as_mut(),
        ) {
            (Some(value_sub), Some(prop_sub)) => {
                let snapshot = tokio::select! {
                    snap = value_sub.recv_snapshot() => snap?,
                    snap = prop_sub.recv_snapshot() => snap?,
                };
                Some(snapshot_to_pv_structure(&snapshot, self.nt_type))
            }
            (Some(value_sub), None) => {
                let snapshot = value_sub.recv_snapshot().await?;
                Some(snapshot_to_pv_structure(&snapshot, self.nt_type))
            }
            _ => None,
        }
    }

    async fn stop(&mut self) {
        self.subscription = None;
        self.property_subscription = None;
        self.running = false;
        self.initial_snapshot = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_base_rs::server::records::ai::AiRecord;
    use std::time::Duration;

    /// Lifecycle invariants:
    /// - `start()` opens a subscription. The native PVA server emits
    ///   the initial snapshot via ChannelSource::get_value() at
    ///   MONITOR INIT — BridgeMonitor::poll() only surfaces fresh
    ///   record updates so the client doesn't see a duplicate
    ///   initial event.
    /// - `stop()` drops the subscription so the underlying
    ///   DbSubscription is released (poll returns None — the
    ///   broadcast sender was dropped).
    /// - Stopping is idempotent and leaves no spawned task lingering.
    #[tokio::test]
    async fn monitor_stop_releases_subscription() {
        let db = Arc::new(PvDatabase::new());
        db.add_record("MON_LIFECYCLE", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();

        let mut mon = BridgeMonitor::new(db.clone(), "MON_LIFECYCLE".into(), NtType::Scalar);
        mon.start().await.expect("start ok");
        assert!(mon.running);

        // No cached initial snapshot — the PVA server provides it via
        // get_value(). poll() blocks waiting for a fresh update.
        let polled = tokio::time::timeout(Duration::from_millis(100), mon.poll()).await;
        assert!(
            polled.is_err(),
            "poll() should time out without a fresh update"
        );

        // Drop the underlying record's only owner of the broadcast
        // sender. After `stop()` the subscription is None, so subsequent
        // polls short-circuit; the broadcast subscriber is also released
        // (verified indirectly: a fresh subscribe must succeed without
        // contention).
        mon.stop().await;
        assert!(!mon.running);
        assert!(mon.subscription.is_none());
        assert!(mon.property_subscription.is_none());

        // A second `stop()` is idempotent.
        mon.stop().await;
        assert!(!mon.running);

        // After stop, a fresh BridgeMonitor against the same record
        // re-subscribes cleanly (regression for "leaked sender keeps
        // the broadcast at saturated subscriber count" issues).
        let mut mon2 = BridgeMonitor::new(db.clone(), "MON_LIFECYCLE".into(), NtType::Scalar);
        mon2.start().await.expect("re-subscribe ok");
        assert!(mon2.running);
        mon2.stop().await;
    }

    /// BR-R36: a PROPERTY-only post must wake `poll()` even when no
    /// value/alarm event ever fires. Regression for the prior
    /// behaviour where `BridgeMonitor` opened only one VALUE|LOG
    /// subscription and so PROPERTY-class metadata changes (EGU /
    /// HOPR / LOPR / enum strings) were never visible on the PVA
    /// wire until the next unrelated VAL post.
    #[tokio::test]
    async fn monitor_property_event_wakes_poll() {
        let db = Arc::new(PvDatabase::new());
        db.add_record("MON_PROPERTY", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();

        let mut mon = BridgeMonitor::new(db.clone(), "MON_PROPERTY".into(), NtType::Scalar);
        mon.start().await.expect("start ok");

        // Manually post a PROPERTY-only event for the VAL field — no
        // value change, just a metadata-update notification. The
        // VALUE|ALARM subscription should NOT see this (mask
        // mismatch); the PROPERTY subscription must.
        {
            let rec = db.get_record("MON_PROPERTY").await.expect("rec exists");
            let instance = rec.read().await;
            instance.notify_field("VAL", EventMask::PROPERTY);
        }

        let polled = tokio::time::timeout(Duration::from_millis(500), mon.poll()).await;
        let snap = polled
            .expect("PROPERTY event must wake poll within 500ms")
            .expect("snapshot delivered");
        // Snapshot is a full NT structure; sanity-check that we got one.
        assert!(
            !snap.fields.is_empty(),
            "PROPERTY-event snapshot must carry the full NT structure"
        );

        mon.stop().await;
    }
}
