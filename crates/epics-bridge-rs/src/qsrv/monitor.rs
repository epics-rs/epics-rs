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
use epics_base_rs::server::database::db_access::{DbSubscription, SubscriptionActivation};
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
    /// Bound field (uppercased; defaults to `VAL`).
    field: String,
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
    /// override mask for the value subscription. `None` means
    /// "use the pvxs-parity default VALUE|ALARM". Set by
    /// `with_value_mask` when the client provides
    /// `record._options.DBE` in the INIT pvRequest.
    value_mask_override: Option<u16>,
    /// filter chain to install on the value subscription
    /// when it's opened. Empty chain = no filtering. Sourced from
    /// the pvxs-compatible `PV.VAL{...}` JSON suffix on the
    /// channel name (parsed once by `BridgeChannel::new`).
    filters: std::sync::Arc<epics_base_rs::server::database::filters::FilterChain>,
    /// filter chain to install on the PROPERTY subscription — an
    /// INDEPENDENT re-parse of the same `PV.VAL{...}` suffix (pvxs builds
    /// the property `dbChannel` from the same filtered name,
    /// singlesource.cpp:161 + singlesrcsubscriptionctx.cpp:24). Held apart
    /// from `filters` so a stateful `dbnd`/`dec` on the value stream never
    /// shares state with a DBE_PROPERTY event on the property stream.
    /// Without it, a filtered array (`arr`-sliced) monitor rebuilt the NT
    /// from an UNFILTERED property snapshot and shipped the whole un-sliced
    /// array on every metadata change, corrupting the client's cached slice.
    property_filters: std::sync::Arc<epics_base_rs::server::database::filters::FilterChain>,
    running: bool,
    /// Initial complete snapshot sent on first poll() after start().
    initial_snapshot: Option<PvStructure>,
    /// Number of monitor events lost due to overflow.
    overflow_count: Arc<AtomicU64>,
    /// Access control context for read enforcement on start().
    access: AccessContext,
}

impl BridgeMonitor {
    pub fn new(db: Arc<PvDatabase>, record_name: String, field: String, nt_type: NtType) -> Self {
        Self {
            db,
            record_name,
            field,
            nt_type,
            subscription: None,
            property_subscription: None,
            value_mask_override: None,
            filters: std::sync::Arc::new(
                epics_base_rs::server::database::filters::FilterChain::new(),
            ),
            property_filters: std::sync::Arc::new(
                epics_base_rs::server::database::filters::FilterChain::new(),
            ),
            running: false,
            initial_snapshot: None,
            overflow_count: Arc::new(AtomicU64::new(0)),
            access: AccessContext::allow_all(),
        }
    }

    /// attach the pvxs-compatible channel filter chain
    /// extracted from the `PV.VAL{...}` JSON suffix. Called by
    /// `BridgeChannel::create_monitor_with_value_mask` before
    /// `start()` opens the subscription.
    pub fn with_filters(
        mut self,
        filters: std::sync::Arc<epics_base_rs::server::database::filters::FilterChain>,
    ) -> Self {
        self.filters = filters;
        self
    }

    /// attach the INDEPENDENT filter chain for the PROPERTY subscription.
    /// pvxs's property `dbChannel` re-parses the same `PV.VAL{...}` suffix
    /// (`dbChannel.c:471`), so metadata-change events carry the client's
    /// value reshaping (`arr` slice) with per-channel filter state. Called
    /// by `BridgeChannel::create_monitor` alongside [`Self::with_filters`].
    pub fn with_property_filters(
        mut self,
        filters: std::sync::Arc<epics_base_rs::server::database::filters::FilterChain>,
    ) -> Self {
        self.property_filters = filters;
        self
    }

    /// Inject an access control context. The PVA server (or `BridgeChannel`'s
    /// own create_monitor) calls this to propagate the channel's identity
    /// into the monitor.
    pub fn with_access(mut self, access: AccessContext) -> Self {
        self.access = access;
        self
    }

    /// override the value-subscription DBE mask.
    ///
    /// pvxs reads `record._options.DBE` from the MONITOR INIT
    /// pvRequest (singlesource.cpp:115). The wire layer extracts that
    /// option in `QsrvPvStore::subscribe_checked` and calls this
    /// builder before `start()`. `None` leaves the pvxs-parity
    /// default (`VALUE | ALARM`) in place; `Some(mask)` substitutes
    /// the client-selected mask.
    pub fn with_value_mask(mut self, mask: u16) -> Self {
        self.value_mask_override = Some(mask);
        self
    }

    /// Get the number of overflow events (events lost due to queue full).
    pub fn overflow_count(&self) -> u64 {
        self.overflow_count.load(Ordering::Relaxed)
    }

    /// Detachable enable/disable handles for this monitor's backing
    /// subscriptions — the value (VALUE|ALARM) and PROPERTY `dbChannel`s
    /// pvxs QSRV opens per single-record monitor. Used by the per-op
    /// MONITOR START/STOP gate: on a client STOP the gate
    /// calls `set_active(false)` on each, the in-process equivalent of
    /// pvxs `onStart(false)` ⇒ `db_event_disable` on both `dbChannel`s.
    /// Valid after [`PvaMonitor::start`]; empty before.
    pub fn activation_handles(&self) -> Vec<SubscriptionActivation> {
        let mut handles = Vec::new();
        if let Some(sub) = &self.subscription {
            handles.push(sub.activation_handle());
        }
        if let Some(sub) = &self.property_subscription {
            handles.push(sub.activation_handle());
        }
        handles
    }
}

impl PvaMonitor for BridgeMonitor {
    async fn start(&mut self) -> BridgeResult<()> {
        if self.running {
            return Ok(());
        }

        // Read enforcement: a client without read permission must not be
        // allowed to subscribe to monitor events either.
        if !self.access.can_read(&self.record_name).await {
            return Err(BridgeError::PutRejected(format!(
                "monitor read denied for {} (user='{}' host='{}')",
                self.record_name, self.access.user, self.access.host
            )));
        }

        // pvxs QSRV default mask is VALUE | ALARM, not
        // VALUE | LOG. Subscribe explicitly so the Bridge does
        // not inherit DbSubscription::subscribe's CA-leaning
        // VALUE|LOG default, which would deliver archive-LOG
        // events while missing alarm transitions.
        //
        // subscribe to the bound field (`record.FIELD`), not
        // unconditionally to `VAL`. `DbSubscription::subscribe_with_mask`
        // parses the PV name via `parse_pv_name`, so passing
        // `record.FIELD` binds the subscriber slot to that field's
        // subscribers vector.
        let pv_name = format!("{}.{}", self.record_name, self.field);
        let value_mask = self
            .value_mask_override
            .unwrap_or_else(|| (EventMask::VALUE | EventMask::ALARM).bits());
        // attach the channel-filter chain to the value subscription.
        let filters_opt = if self.filters.is_empty() {
            None
        } else {
            Some(self.filters.as_ref())
        };
        let sub = DbSubscription::subscribe_with_mask_and_filters(
            &self.db,
            &pv_name,
            0,
            value_mask,
            filters_opt,
        )
        .await
        .ok_or_else(|| BridgeError::RecordNotFound(self.record_name.clone()))?;

        // pvxs QSRV opens a second subscription with the
        // PROPERTY mask (singlesource.cpp:161) so a PVA monitor
        // is woken when EGU / HOPR / LOPR / enum-string change,
        // not just when VAL changes. The full snapshot is rebuilt
        // on every wake so the property-channel firing alone is
        // enough to push fresh metadata down the wire.
        //
        // The property `dbChannel` carries the SAME channel filter as the
        // value channel (pvxs builds `pPropertiesChannel` from
        // `dbChannelName(sInfo->chan)`, singlesrcsubscriptionctx.cpp:24),
        // so a DBE_PROPERTY event's snapshot is reshaped (e.g. `arr`
        // sliced) before `poll()` rebuilds the NT. Use the INDEPENDENT
        // `property_filters` re-parse — a stateful `dbnd`/`dec` here must
        // not perturb the value stream's baseline/counter. Without this the
        // metadata event carried the whole un-sliced array and corrupted
        // the client's cached slice.
        let property_filters_opt = if self.property_filters.is_empty() {
            None
        } else {
            Some(self.property_filters.as_ref())
        };
        let property_sub = DbSubscription::subscribe_with_mask_and_filters(
            &self.db,
            &pv_name,
            0,
            EventMask::PROPERTY.bits(),
            property_filters_opt,
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

    async fn poll(&mut self) -> Option<super::provider::MonitorPoll> {
        // Return initial snapshot on first poll (C++ BaseMonitor::connect behavior)
        if let Some(initial) = self.initial_snapshot.take() {
            return Some(super::provider::MonitorPoll::derive(initial));
        }

        // wake on either the VALUE|ALARM subscription or
        // the PROPERTY subscription — whichever the record posts
        // first. snapshot_to_pv_structure rebuilds the full NT
        // structure (with display/control/enums) on every wake,
        // so either firing pushes fresh metadata to the client.
        //
        // a single-record monitor has no `+trigger`
        // graph, so each update derives its own changed-bitset
        // (`marked: None`).
        match (
            self.subscription.as_mut(),
            self.property_subscription.as_mut(),
        ) {
            (Some(value_sub), Some(prop_sub)) => {
                let snapshot = tokio::select! {
                    snap = value_sub.recv_snapshot() => snap?,
                    snap = prop_sub.recv_snapshot() => snap?,
                };
                Some(super::provider::MonitorPoll::derive(
                    snapshot_to_pv_structure(&snapshot, self.nt_type),
                ))
            }
            (Some(value_sub), None) => {
                let snapshot = value_sub.recv_snapshot().await?;
                Some(super::provider::MonitorPoll::derive(
                    snapshot_to_pv_structure(&snapshot, self.nt_type),
                ))
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

        let mut mon = BridgeMonitor::new(
            db.clone(),
            "MON_LIFECYCLE".into(),
            "VAL".into(),
            NtType::Scalar,
        );
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
        let mut mon2 = BridgeMonitor::new(
            db.clone(),
            "MON_LIFECYCLE".into(),
            "VAL".into(),
            NtType::Scalar,
        );
        mon2.start().await.expect("re-subscribe ok");
        assert!(mon2.running);
        mon2.stop().await;
    }

    /// a PROPERTY-only post must wake `poll()` even when no
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

        let mut mon = BridgeMonitor::new(
            db.clone(),
            "MON_PROPERTY".into(),
            "VAL".into(),
            NtType::Scalar,
        );
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
        // a single-record monitor carries no marked set.
        assert!(
            snap.marked.is_none(),
            "single-record monitor must not carry an explicit marked set"
        );
        assert!(
            !snap.value.fields.is_empty(),
            "PROPERTY-event snapshot must carry the full NT structure"
        );

        mon.stop().await;
    }

    /// Regression (Q37): a DBE_PROPERTY event on an `arr`-sliced array
    /// monitor must deliver the SLICED value, not the whole array. pvxs
    /// builds the property `dbChannel` from the same filtered channel name
    /// (`singlesrcsubscriptionctx.cpp:24`), so both streams carry the
    /// client's `arr` filter. Pre-fix the property subscription was
    /// unfiltered, so `poll()` rebuilt the NT from the full un-sliced array
    /// on every metadata change and shipped a wrong-length value that
    /// corrupted the client's cached slice.
    #[tokio::test]
    async fn property_event_delivers_filtered_slice() {
        use epics_base_rs::server::database::filters::try_parse_filter_chain;
        use epics_base_rs::server::records::waveform::WaveformRecord;
        use epics_base_rs::types::{DbFieldType, EpicsValue};
        use epics_pva_rs::pvdata::PvField;

        let db = Arc::new(PvDatabase::new());
        db.add_record(
            "WF_ARR",
            Box::new(WaveformRecord::new(4, DbFieldType::Double)),
        )
        .await
        .unwrap();
        db.put_pv(
            "WF_ARR.VAL",
            EpicsValue::DoubleArray(vec![10.0, 20.0, 30.0, 40.0]),
        )
        .await
        .unwrap();

        // `[1:2]` slice → elements 1 and 2 → length 2. Parse the value and
        // property chains independently, mirroring `BridgeChannel::new`.
        let value_chain =
            Arc::new(try_parse_filter_chain(r#"{"arr":{"s":1,"e":2}}"#).expect("parse value arr"));
        let property_chain =
            Arc::new(try_parse_filter_chain(r#"{"arr":{"s":1,"e":2}}"#).expect("parse prop arr"));

        let mut mon = BridgeMonitor::new(
            db.clone(),
            "WF_ARR".into(),
            "VAL".into(),
            NtType::ScalarArray,
        )
        .with_filters(value_chain)
        .with_property_filters(property_chain);
        mon.start().await.expect("start ok");

        // A PROPERTY-only post (metadata change) carries the current VAL.
        {
            let rec = db.get_record("WF_ARR").await.expect("rec exists");
            let instance = rec.read().await;
            instance.notify_field("VAL", EventMask::PROPERTY);
        }

        let snap = tokio::time::timeout(Duration::from_millis(500), mon.poll())
            .await
            .expect("PROPERTY event must wake poll within 500ms")
            .expect("snapshot delivered");

        let value = snap
            .value
            .fields
            .iter()
            .find(|(n, _)| n == "value")
            .map(|(_, v)| v)
            .expect("NTScalarArray has a value field");
        match value {
            PvField::ScalarArray(v) => assert_eq!(
                v.len(),
                2,
                "PROPERTY event must ship the arr-sliced value (len 2), not the full array (len 4)"
            ),
            other => panic!("value must be a ScalarArray, got {other:?}"),
        }

        mon.stop().await;
    }
}
