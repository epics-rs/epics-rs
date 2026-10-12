//! The server-wide QSRV group drain — the port of pvxs's single
//! `"qsrvGroup"` event pump (`ioc/groupsource.cpp:72-96`).
//!
//! # Why one drain
//!
//! Upstream starts ONE `db_start_events(.., "qsrvGroup", ..)` thread per
//! `GroupSource`; every member subscription of every group is a callback on
//! that thread, which locks the trigger set, refreshes the group's
//! `currentValue` and posts the coalesced update
//! (`groupsource.cpp:307-353`). The pre-drain Rust shape instead spawned
//! ~2 forwarder tasks per member onto the shared Medium-band callback pool;
//! a 20-member group at a 10 Hz posting rate put ~400 task wakes/second on
//! the band every other PV's monitor delivery uses, collapsing group
//! delivery to ~0.35 Hz and stalling an unrelated scalar monitor for up to
//! 16.9 s on the RTEMS target. Here every member subscription of every
//! group attaches to the pump's one `event_user`
//! ([`DbEventSource`]), whose one drain thread assembles the atomic
//! snapshot and posts the group update into the subscription's update ring.
//! A scan pass over many members wakes that thread once while it is busy.
//!
//! # Why the drain is a dedicated thread, not a pool task
//!
//! Upstream's pump IS a thread — `db_start_events` creates it — and the
//! first landing of this module learned why the hard way. It spawned the
//! drain with `runtime::task::spawn`, which on the exec backend is a task
//! on the Medium callback band: one cooperative worker, released only when
//! a task's poll RETURNS, and the drain's poll returned only when every
//! member queue was empty. On the target the `.1 second` scans (EPICS 66,
//! above cbMedium's 64) refilled the member queues faster than the drain
//! emptied them, so that poll never returned: the band's only worker never
//! ran another Medium task (all per-PV monitor forwarding dead) and never
//! slept (SCHED_FIFO 64 busy on a single core — every thread below it
//! starved, protocol echo included). One group subscription wedged the
//! whole server, permanently. A dedicated thread removes the failure class:
//! the drain parks on its own stack, occupies no shared worker, and runs
//! BELOW the connection threads so even a saturated drain cannot preempt
//! protocol traffic — see [`QSRV_GROUP_PUMP_PRIORITY`].
//!
//! # Invariant
//!
//! * MUST: only the drain consumes member subscription events and posts
//!   group monitor updates; a member event MUST NOT spawn or wake a
//!   per-member pool task.
//! * MUST: the drain exists exactly while a member subscription does
//!   (each [`DbAttachment`] holds it), so no group subscription means no
//!   drain thread.
//! * MUST: a group subscription's update ring stays open while its
//!   [`GroupMonitor`] holds the [`GroupDelivery`], so a quiet group parks
//!   instead of ending (pvxs keeps an all-const subscription open until
//!   the client cancels, `groupsource.cpp:241-298`).
//!
//! # Update ring (per subscription, C monitor semantics)
//!
//! The drain posts assembled updates into the subscription's
//! [`MonitorRing`](epics_pva_rs::server_native::MonitorRing), bounded by
//! the negotiated `queueSize`: while room remains distinct updates append;
//! once full the newest REPLACES the tail in place, value newest-wins and
//! marked leaf sets unioned (pvxs's monitor FIFO squash). A post never
//! blocks, so one slow subscriber cannot stall the drain for other groups.

// RTEMS-EXEC-MODEL-ALLOW(8): checked - these run and pass in the exec-backend
// suite.

use std::sync::Arc;

use epics_base_rs::runtime::task::{StackSizeClass, ThreadPriority};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::char_view::DollarView;
use epics_base_rs::server::database::db_access::{DbAttachment, DbEventSource, EventDelivery};
use epics_base_rs::server::database::filters::FilterChain;
use epics_base_rs::server::pv::{MonitorEvent, SubscribeRefused};
use epics_base_rs::server::snapshot::PropertySupport;
use epics_pva_rs::pvdata::PvField;
use epics_pva_rs::server_native::{MonitorOutbox, MonitorUpdate};

use super::group::{EventMark, GroupChannel, GroupMonitor, MemberEventKind};
use super::group_config::GroupPvDef;
use super::pvif::FieldMapping;

/// The EPICS priority of the `qsrvGroup` drain thread — deliberately ONE
/// BELOW the PVA connection threads, where pvxs puts its pump one ABOVE.
///
/// pvxs starts the pump with `db_start_events(.., "qsrvGroup", ..,
/// epicsThreadPriorityCAServerLow - 1)` (`ioc/groupsource.cpp:96`) = 19 on
/// the ladder this workspace already copied its PVA numbers from: the
/// blocking driver's connection threads sit at `CAServerLow - 2` = 18
/// (`PVA_SERVER_PRIORITY`, `server_native/blocking.rs`) and its UDP search
/// responder at `CAServerLow - 4` = 16. Taking upstream's 19 literally
/// would put the drain ABOVE the connection threads, and the §9.15
/// regression measured exactly what a drain above the protocol threads
/// does to a single-core SCHED_FIFO target when it saturates: echo
/// unanswered, fresh clients timing out, delivery dead server-wide. Per
/// event this drain also does strictly more work than upstream's pump
/// callback budgeted for that slot (a full atomic `read_group` of every
/// member). So the deviation: 17 keeps the pump above the SEARCH responder
/// (preserving pvxs's drain > search ordering — a discovery storm must not
/// starve group delivery) but below established connections, so a
/// saturated drain degrades group update latency (member queues coalesce,
/// newest-wins) instead of protocol liveness.
const QSRV_GROUP_PUMP_PRIORITY: ThreadPriority = ThreadPriority::Custom(17);

/// One group subscription's state on the drain: what every member event
/// of that subscription needs to assemble and post the group.
pub(crate) struct GroupDelivery {
    def: GroupPvDef,
    member_props: Vec<PropertySupport>,
    group_channel: GroupChannel,
    updates: MonitorOutbox<MonitorUpdate>,
    /// Until the first post; touched by the drain alone.
    prime: parking_lot::Mutex<Option<GroupPrime>>,
}

/// pvxs `subscriptionPost`'s `eventsPrimed` (`groupsource.cpp:256-264`):
/// nothing is posted until every member subscription has delivered once,
/// and the first post carries every leaf those events marked.
struct GroupPrime {
    /// C `hadValueEvent` / `hadPropertyEvent` per member; a member with no
    /// such subscription starts seen (`fieldsubscriptionctx.h:51-54`).
    seen: Vec<[bool; 2]>,
    /// What `onSubscribe` assigns before any event: the two
    /// `record._options` leaves and each member's assigned leaves
    /// (`:401-424`).
    marked: Vec<String>,
}

impl GroupDelivery {
    pub(crate) fn new(
        def: GroupPvDef,
        member_props: Vec<PropertySupport>,
        group_channel: GroupChannel,
        updates: MonitorOutbox<MonitorUpdate>,
    ) -> Self {
        let mut marked = vec![
            "record._options.queueSize".to_string(),
            "record._options.atomic".to_string(),
        ];
        let seen = def
            .channels
            .iter()
            .map(|m| {
                let value = m.has_channel();
                if value || m.def.mapping == FieldMapping::Const {
                    marked.extend(super::pvif::assigned_leaf_paths(
                        &m.def.field_name,
                        m.def.mapping,
                        m.is_value_field(),
                    ));
                }
                let property =
                    value && matches!(m.def.mapping, FieldMapping::Scalar | FieldMapping::Meta);
                [!value, !property]
            })
            .collect();
        Self {
            def,
            member_props,
            group_channel,
            updates,
            prime: parking_lot::Mutex::new(Some(GroupPrime { seen, marked })),
        }
    }

    /// A member subscription that was not made, so the first post does not
    /// wait for it.
    pub(crate) fn not_subscribed(&self, member_index: usize, kind: MemberEventKind) {
        let _ = self.primed(member_index, kind, EventMark::Skip);
    }

    /// The leaves to post for one member event, `None` for no post: before
    /// the first post this event's marks are held back and merged.
    fn primed(
        &self,
        member_index: usize,
        kind: MemberEventKind,
        mark: EventMark,
    ) -> Option<Vec<String>> {
        let mut prime = self.prime.lock();
        let Some(p) = prime.as_mut() else {
            return match mark {
                EventMark::Marked(marked) => Some(marked),
                EventMark::Skip => None,
            };
        };
        if let Some(seen) = p.seen.get_mut(member_index) {
            seen[kind as usize] = true;
        }
        if let EventMark::Marked(marked) = mark {
            p.marked.extend(marked);
        }
        if !p.seen.iter().all(|s| s[0] && s[1]) {
            return None;
        }
        let mut marked = prime.take()?.marked;
        marked.sort_unstable();
        marked.dedup();
        Some(marked)
    }
}

/// One member subscription's delivery: its group subscription, which
/// member it belongs to and whether it is the value or property channel.
pub(crate) struct MemberDelivery {
    pub(crate) group: Arc<GroupDelivery>,
    pub(crate) member_index: usize,
    pub(crate) kind: MemberEventKind,
}

impl EventDelivery for MemberDelivery {
    /// One member event, end to end: resolve the marked leaves, assemble
    /// the atomic group snapshot, post the update. The body pvxs runs on
    /// its `qsrvGroup` thread per callback (`groupsource.cpp:307-353`).
    async fn deliver(&self, event: MonitorEvent) {
        let group = &*self.group;
        let mark = match self.kind {
            MemberEventKind::Value => GroupMonitor::value_event_mark(
                &group.def,
                &group.member_props,
                self.member_index,
                event.mask(),
            ),
            MemberEventKind::Property => GroupMonitor::property_event_mark(
                &group.def,
                &group.member_props,
                self.member_index,
            ),
        };
        let Some(marked) = group.primed(self.member_index, self.kind, mark) else {
            return;
        };
        // A value event serves its own member from the log; a property
        // event reads only metadata (`groupsource.cpp:338-340`, `:375-377`).
        let log =
            matches!(self.kind, MemberEventKind::Value).then_some((self.member_index, &event));
        let value = match group.group_channel.read_group_logged(log).await {
            Ok(v) => v,
            Err(e) => {
                // pvxs wraps each group refresh in a try/catch that logs and
                // returns from the callback WITHOUT posting, leaving the
                // subscription alive (`groupsource.cpp:350-352`).
                tracing::warn!(
                    group = %group.def.name,
                    error = %e,
                    "qsrv group drain: member read failed; skipping event, subscription kept open"
                );
                return;
            }
        };
        let marked = super::pvif::narrow_enum_value_leaves(marked, &value);
        // A consumer gone mid-teardown refuses the post; its monitor's drop
        // detaches the members.
        group.updates.post(
            MonitorUpdate {
                value: epics_pva_rs::server_native::PostValue::Owned(PvField::Structure(value)),
                marked: Some(marked.into()),
                type_changed: false,
                overrun: Vec::new(),
            },
            false,
        );
    }
}

/// The server-wide group drain. One per `BridgeProvider` (== one per
/// served IOC database — C's one `GroupSource` per server); every member
/// subscription of every group attaches here.
pub(crate) struct GroupPump {
    source: DbEventSource<MemberDelivery>,
}

impl GroupPump {
    pub(crate) fn new() -> Arc<Self> {
        // `Big` stack, not upstream's event-task `Medium` (`dbEvent.c:1117`):
        // the drain runs each `read_group` + NT-assembly state machine on
        // this stack, the same audited future the `Big`-stacked connection
        // threads run for a group GET.
        Arc::new(Self {
            source: DbEventSource::new("qsrvGroup", QSRV_GROUP_PUMP_PRIORITY, StackSizeClass::Big),
        })
    }

    /// Attach one member subscription to the drain.
    pub(crate) fn attach(
        &self,
        db: &PvDatabase,
        pv_name: &str,
        mask: u16,
        filters: &FilterChain,
        dollar: Option<DollarView>,
        delivery: MemberDelivery,
    ) -> Result<DbAttachment, SubscribeRefused> {
        self.source
            .attach(db, pv_name, mask, Some(filters), dollar, delivery)
    }

    /// True while the drain thread is alive.
    #[cfg(test)]
    pub(crate) fn has_live_drain(&self) -> bool {
        self.source.live_drains() > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use epics_base_rs::server::recgbl::EventMask;
    use epics_base_rs::server::records::ai::AiRecord;

    use crate::qsrv::group_config::parse_group_config;
    use crate::qsrv::provider::PvaMonitor;

    async fn wait_drain_stopped(pump: &Arc<GroupPump>) {
        for _ in 0..500 {
            if !pump.has_live_drain() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the drain did not terminate after the last member detached");
    }

    async fn one_member_group(name: &str, rec: &str) -> (Arc<PvDatabase>, GroupPvDef) {
        let db = Arc::new(PvDatabase::new());
        db.add_record(rec, Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        let cfg =
            format!(r#"{{ "{name}": {{ "v": {{"+type": "plain", "+channel": "{rec}.VAL"}} }} }}"#);
        let mut defs = parse_group_config(&cfg).unwrap();
        (db, defs.pop().unwrap())
    }

    fn post_value(db: &PvDatabase, rec: &str) {
        let rec = db.get_record(rec).expect("record exists");
        rec.write()
            .notify_field("VAL", EventMask::VALUE | EventMask::ALARM);
    }

    /// A value event serves its own member from its log, as pvxs reads the
    /// triggering member through `pDbFieldLog` (`groupsource.cpp:338-340`):
    /// a write made after the post and before the drain reads stays out of it.
    #[tokio::test]
    async fn a_value_event_serves_its_member_from_its_log() {
        use epics_base_rs::types::EpicsValue;
        use epics_pva_rs::pvdata::ScalarValue;

        let (db, def) = one_member_group("TL:GRP", "TL:a").await;
        let pump = GroupPump::new();
        let mut mon = GroupMonitor::new(db.clone(), def).with_pump(pump.clone());
        mon.start().await.expect("start");
        {
            let rec = db.get_record("TL:a").unwrap();
            let mut held = rec.write();
            held.record
                .put_field("VAL", EpicsValue::Double(5.0))
                .unwrap();
            held.notify_field("VAL", EventMask::VALUE | EventMask::ALARM);
            held.record
                .put_field("VAL", EpicsValue::Double(6.0))
                .unwrap();
        }
        let polled = tokio::time::timeout(Duration::from_secs(2), mon.poll())
            .await
            .expect("the post is delivered")
            .expect("update");
        let v = polled.value.fields.iter().find(|(name, _)| name == "v");
        assert_eq!(
            v.map(|(_, f)| f),
            Some(&PvField::Scalar(ScalarValue::Double(5.0)))
        );
        mon.stop().await;
        wait_drain_stopped(&pump).await;
    }

    /// Boundary zero subscribers: nothing attaches, so no drain thread
    /// exists — before, during and after member records post. Starting one
    /// subscription spawns the drain; stopping it detaches the last member
    /// and ends the drain.
    #[tokio::test]
    async fn zero_group_subscriptions_run_no_drain() {
        let (db, def) = one_member_group("T0:GRP", "T0:a").await;
        let pump = GroupPump::new();
        assert!(!pump.has_live_drain(), "no drain before any subscription");

        post_value(&db, "T0:a");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !pump.has_live_drain(),
            "member posts without a subscription must start no drain work"
        );

        let mut mon = GroupMonitor::new(db.clone(), def).with_pump(pump.clone());
        mon.start().await.expect("start");
        assert!(
            pump.has_live_drain(),
            "the first attachment spawns the drain"
        );

        mon.stop().await;
        wait_drain_stopped(&pump).await;
    }

    /// Boundary cross-group fairness: two groups share the one drain; group
    /// A's subscriber never polls while A floods, and group B's delivery
    /// must still proceed — the update ring coalesces instead of blocking,
    /// so a slow subscriber cannot stall the drain for a sibling group.
    #[tokio::test]
    async fn two_groups_share_one_drain_slow_consumer_does_not_block_the_other() {
        let db = Arc::new(PvDatabase::new());
        db.add_record("TF:a", Box::new(AiRecord::new(1.0)))
            .await
            .unwrap();
        db.add_record("TF:b", Box::new(AiRecord::new(2.0)))
            .await
            .unwrap();
        let cfg = r#"{
            "TF:GA": { "v": {"+type": "plain", "+channel": "TF:a.VAL"} },
            "TF:GB": { "v": {"+type": "plain", "+channel": "TF:b.VAL"} }
        }"#;
        let defs = parse_group_config(cfg).unwrap();
        let def_a = defs.iter().find(|d| d.name == "TF:GA").unwrap().clone();
        let def_b = defs.iter().find(|d| d.name == "TF:GB").unwrap().clone();

        let pump = GroupPump::new();
        let mut mon_a = GroupMonitor::new(db.clone(), def_a).with_pump(pump.clone());
        let mut mon_b = GroupMonitor::new(db.clone(), def_b).with_pump(pump.clone());
        mon_a.start().await.expect("start A");
        mon_b.start().await.expect("start B");

        // Flood A without ever polling its monitor. The short sleeps let
        // the drain assemble per event rather than EvQue-coalescing the
        // whole burst, so A's update ring genuinely overflows into
        // replace-in-place while its consumer stays stalled.
        for _ in 0..12 {
            post_value(&db, "TF:a");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // B must deliver regardless of A's backlog.
        post_value(&db, "TF:b");
        let polled = tokio::time::timeout(Duration::from_secs(2), mon_b.poll())
            .await
            .expect("B's delivery must not be blocked by A's stalled subscriber")
            .expect("B receives an assembled update");
        assert!(!polled.value.fields.is_empty());

        // A's backlog is intact (coalesced, not lost or torn down).
        let polled_a = tokio::time::timeout(Duration::from_secs(2), mon_a.poll())
            .await
            .expect("A still delivers after the flood")
            .expect("A's update stream is alive");
        assert!(!polled_a.value.fields.is_empty());

        mon_a.stop().await;
        assert!(
            pump.has_live_drain(),
            "the drain survives while B is still subscribed"
        );
        mon_b.stop().await;
        wait_drain_stopped(&pump).await;
    }

    /// Boundary last-unsubscribe-while-drain-mid-cycle: tear the only
    /// subscription down while a posted burst is still draining. The last
    /// member's detach ends the drain, and a fresh subscription afterwards
    /// restarts it cleanly
    /// (resubscribe-after-teardown boundary).
    #[tokio::test]
    async fn last_unsubscribe_mid_burst_terminates_the_drain() {
        let (db, def) = one_member_group("TM:GRP", "TM:a").await;
        let pump = GroupPump::new();
        let mut mon = GroupMonitor::new(db.clone(), def.clone()).with_pump(pump.clone());
        mon.start().await.expect("start");

        // Burst, then stop immediately — the drain is mid-cycle on the
        // backlog (or about to be) when the members detach.
        for _ in 0..8 {
            post_value(&db, "TM:a");
        }
        mon.stop().await;
        wait_drain_stopped(&pump).await;

        // Resubscribe-after-teardown restarts cleanly on the same pump.
        let mut mon2 = GroupMonitor::new(db.clone(), def).with_pump(pump.clone());
        mon2.start().await.expect("restart");
        assert!(pump.has_live_drain(), "re-attaching spawns a new drain");
        post_value(&db, "TM:a");
        let polled = tokio::time::timeout(Duration::from_secs(2), mon2.poll())
            .await
            .expect("the restarted drain delivers")
            .expect("update after resubscribe");
        assert!(!polled.value.fields.is_empty());
        mon2.stop().await;
        wait_drain_stopped(&pump).await;
    }

    /// Boundary event-arriving-during-teardown, consumer side first: the
    /// update ring's reader drops BEFORE the monitor, and a member event
    /// lands in that window. The drain's post is refused and nothing else
    /// happens; the monitor's teardown then detaches the member and ends
    /// the drain.
    #[tokio::test]
    async fn event_after_the_reader_drops_is_refused_and_teardown_ends_the_drain() {
        let (db, def) = one_member_group("TT:GRP", "TT:a").await;
        let pump = GroupPump::new();
        let mut mon = GroupMonitor::new(db.clone(), def).with_pump(pump.clone());
        mon.start().await.expect("start");
        assert!(pump.has_live_drain());

        drop(mon.take_updates().expect("the ring before it is taken"));
        post_value(&db, "TT:a");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(pump.has_live_drain(), "the member keeps the drain");

        mon.stop().await;
        wait_drain_stopped(&pump).await;
    }

    /// The §9.15 fatal-regression test — group delivery MUST NOT depend on
    /// the shared callback pool's Medium band.
    ///
    /// The first pump landing spawned its drain with
    /// `runtime::task::spawn`, which on the exec backend is a task on the
    /// Medium callback band: ONE cooperative worker, released only when a
    /// task's poll RETURNS — and the drain's poll returned only when every
    /// member queue was empty. On
    /// the target the `.1 second` scans (EPICS 66, above cbMedium's 64)
    /// refilled the member queues faster than the drain emptied them, so the
    /// poll never returned, the band's only worker never ran another Medium
    /// task or slept again, and every thread below 64 starved: one group
    /// subscription wedged ALL monitor delivery server-wide, killed protocol
    /// echo and timed out fresh clients, permanently (measured, §9.15). The
    /// same class was caught independently on the host: a name-server dial
    /// occupying cbMedium for its 40 s connect timeout froze delivery
    /// server-wide.
    ///
    /// So: occupy the Medium band's only worker for the WHOLE test, and
    /// require the drain to deliver anyway. On the band-task shape this
    /// fails deterministically (the pump task is queued behind the pin and
    /// can never run); on the dedicated-thread shape (pvxs's `qsrvGroup`
    /// thread, `db_start_events` at `groupsource.cpp:96`) it passes without
    /// the band ever becoming free. Exec backend only: on the tokio backend
    /// `spawn_blocking` goes to a large pool and pins nothing.
    #[cfg(exec_backend)]
    #[tokio::test]
    async fn drain_delivers_while_the_medium_band_worker_is_occupied() {
        // Pin the Medium band's ONLY worker (DEFAULT_THREADS_PER_PRIORITY
        // = 1): the closure holds it until released, so nothing spawned onto
        // the band can run for the duration of the test.
        let (pinned_tx, pinned_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let pin = epics_base_rs::runtime::task::spawn_blocking(move || {
            let _ = pinned_tx.send(());
            let _ = release_rx.recv_timeout(Duration::from_secs(60));
        });
        pinned_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the pin closure reached the band worker");

        let (db, def) = one_member_group("TP:GRP", "TP:a").await;
        let pump = GroupPump::new();
        let mut mon = GroupMonitor::new(db.clone(), def).with_pump(pump.clone());
        mon.start().await.expect("start");
        post_value(&db, "TP:a");

        let polled = tokio::time::timeout(Duration::from_secs(3), mon.poll()).await;
        // Release the band before asserting, so a failure does not leave the
        // worker wedged for the rest of the process.
        let _ = release_tx.send(());
        let polled = polled
            .expect(
                "group delivery must not depend on the shared Medium callback band \
                 (§9.15: the band-task pump wedged the whole target)",
            )
            .expect("assembled update");
        assert!(!polled.value.fields.is_empty());

        mon.stop().await;
        wait_drain_stopped(&pump).await;
        drop(pin);
    }

    /// Delivery-keeps-up regression (host-scale stand-in for the on-target
    /// §9.14 rerun): a 20-member group posting fast, with a live group
    /// subscriber, must keep delivering assembled updates promptly — every
    /// member tick is O(1) woken tasks through the shared drain, and the
    /// update stream never wedges under the flood.
    #[tokio::test]
    async fn twenty_member_flood_keeps_delivering() {
        let db = Arc::new(PvDatabase::new());
        let mut members = String::new();
        for i in 0..20 {
            let rec = format!("TB:m{i:02}");
            db.add_record(&rec, Box::new(AiRecord::new(f64::from(i))))
                .await
                .unwrap();
            if i > 0 {
                members.push(',');
            }
            members.push_str(&format!(
                r#""f{i:02}": {{"+type": "plain", "+channel": "{rec}.VAL"}}"#
            ));
        }
        let cfg = format!(r#"{{ "TB:BIG": {{ {members} }} }}"#);
        let mut defs = parse_group_config(&cfg).unwrap();
        let def = defs.pop().unwrap();

        let pump = GroupPump::new();
        let mut mon = GroupMonitor::new(db.clone(), def).with_pump(pump.clone());
        mon.start().await.expect("start");

        // Flood: 5 rounds over all 20 members, polling in between.
        for _ in 0..5 {
            for i in 0..20 {
                post_value(&db, &format!("TB:m{i:02}"));
            }
            let polled = tokio::time::timeout(Duration::from_secs(2), mon.poll())
                .await
                .expect("delivery keeps up under a 20-member flood")
                .expect("assembled update");
            assert!(
                polled.value.get_field("f00").is_some() && polled.value.get_field("f19").is_some(),
                "the drained update is the complete assembled group"
            );
        }

        mon.stop().await;
        wait_drain_stopped(&pump).await;
    }
    /// softIocPVX 1.5.3's first update of `G` (`pvxmonitor -F delta G`, the
    /// same with any `record[DBE=..]`) on `A` (ai, VAL 1.5, EGU mm, PREC 2)
    /// and `B` (longin, VAL 3), the group below: `m` and `p` trigger nothing,
    /// so no event marks them, and `a`'s value leaves come from `b`'s trigger.
    const SEEDED_G: &str = "a.alarm.message a.alarm.severity a.alarm.status \
        a.control.limitHigh a.control.limitLow a.display.description \
        a.display.form.choices a.display.form.index a.display.limitHigh \
        a.display.limitLow a.display.precision a.display.units \
        a.timeStamp.nanoseconds a.timeStamp.secondsPastEpoch a.timeStamp.userTag \
        a.value a.valueAlarm.highAlarmLimit a.valueAlarm.highWarningLimit \
        a.valueAlarm.lowAlarmLimit a.valueAlarm.lowWarningLimit b.alarm.message \
        b.alarm.severity b.alarm.status b.control.limitHigh b.control.limitLow \
        b.display.description b.display.form.choices b.display.form.index \
        b.display.limitHigh b.display.limitLow b.display.units \
        b.timeStamp.nanoseconds b.timeStamp.secondsPastEpoch b.timeStamp.userTag \
        b.value b.valueAlarm.highAlarmLimit b.valueAlarm.highWarningLimit \
        b.valueAlarm.lowAlarmLimit b.valueAlarm.lowWarningLimit \
        record._options.atomic record._options.queueSize";

    /// START enables every member subscription, and the first post waits
    /// for all of them and marks what their events assign
    /// (`groupsource.cpp:256-297`).
    #[tokio::test]
    async fn the_first_group_update_marks_what_its_member_seeds_assign_as_softiocpvx() {
        use epics_base_rs::server::records::longin::LonginRecord;
        use epics_base_rs::types::EpicsValue;

        let db = Arc::new(PvDatabase::new());
        db.add_record("A", Box::new(AiRecord::new(1.5)))
            .await
            .unwrap();
        db.add_record("B", Box::new(LonginRecord::new(3)))
            .await
            .unwrap();
        for (field, v) in [
            ("EGU", EpicsValue::String("mm".into())),
            ("PREC", EpicsValue::Short(2)),
        ] {
            db.get_record("A")
                .unwrap()
                .write()
                .record
                .put_field(field, v)
                .unwrap();
        }
        let cfg = r#"{ "G": {
            "a": {"+channel": "A.VAL"},
            "m": {"+channel": "A.VAL", "+type": "meta"},
            "p": {"+channel": "A.VAL", "+type": "plain"},
            "b": {"+channel": "B.VAL", "+trigger": "a,b"}
        } }"#;
        let def = parse_group_config(cfg).unwrap().pop().unwrap();
        let pump = GroupPump::new();
        let mut mon = GroupMonitor::new(db.clone(), def).with_pump(pump.clone());
        mon.start().await.expect("start");
        let mut ring = mon.take_updates().expect("the ring");
        for h in mon.activation_handles() {
            h.enable(&db).await;
        }
        let first = tokio::time::timeout(Duration::from_secs(2), ring.recv())
            .await
            .expect("the first update")
            .expect("ring open");
        // A marked structure is every leaf under it on the wire.
        fn leaves(at: &PvField, path: String, out: &mut Vec<String>) {
            match at {
                PvField::Structure(s) => {
                    for (name, f) in &s.fields {
                        leaves(f, format!("{path}.{name}"), out);
                    }
                }
                _ => out.push(path),
            }
        }
        let PvField::Structure(root) = &*first.value else {
            panic!("a group is a structure");
        };
        let mut marked = Vec::new();
        for path in first.marked.expect("a marked update").into_vec() {
            let mut at = root
                .get_field(path.split('.').next().unwrap())
                .expect("marked");
            for name in path.split('.').skip(1) {
                let PvField::Structure(s) = at else {
                    panic!("{path}")
                };
                at = s.get_field(name).expect("marked");
            }
            leaves(at, path, &mut marked);
        }
        marked.sort_unstable();
        let want: Vec<&str> = SEEDED_G.split_whitespace().collect();
        assert_eq!(marked, want);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), ring.recv())
                .await
                .is_err(),
            "the seeds post once"
        );
        drop(mon);
        wait_drain_stopped(&pump).await;
    }
}
