//! Group-PV (NTTable / multi-record) parity tests, mirroring pvxs
//! `test/testqgroup.cpp::testTable`.
//!
//! Loads a JSON group config containing two member records, exercises
//! [`GroupChannel`] get/put, and verifies the atomic semantics that
//! pvxs's qsrv promises.

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::longin::LonginRecord;
use epics_base_rs::types::EpicsValue;

use epics_bridge_rs::qsrv::{
    BridgeProvider, Channel, ProcessMode, PutOptions, group::GroupChannel,
};
use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};

// members need `+putorder` to be writable. pvxs's
// `MappingInfo::putOrder` default is `i64::MIN` → silently
// not-putable (fieldconfig.h:37 / groupsource.cpp:503). The
// PUT tests below explicitly opt members in.
const GROUP_JSON: &str = r#"{
    "TEST:grp": {
        "+id": "epics:nt/NTGroup:1.0",
        "+atomic": true,
        "level": { "+channel": "TEST:level.VAL", "+type": "plain", "+putorder": 0 },
        "count": { "+channel": "TEST:count.VAL", "+type": "plain", "+putorder": 1 }
    }
}"#;

const GROUP_JSON_NONATOMIC: &str = r#"{
    "TEST:grp_na": {
        "+atomic": false,
        "level": { "+channel": "TEST:level_na.VAL", "+type": "plain", "+putorder": 0 },
        "count": { "+channel": "TEST:count_na.VAL", "+type": "plain", "+putorder": 1 }
    }
}"#;

// members WITHOUT `+putorder` are not putable, so a PUT that
// supplies their fields writes nothing. pvxs returns "No fields changed".
const GROUP_JSON_READONLY: &str = r#"{
    "TEST:grp_ro": {
        "+atomic": false,
        "level": { "+channel": "TEST:level.VAL", "+type": "plain" },
        "count": { "+channel": "TEST:count.VAL", "+type": "plain" }
    }
}"#;

// explicit cross-named `+trigger` graph — `level`
// triggers only `count`, `count` triggers only `level`. Neither is a
// self-trigger and neither is `"*"`, so the group is NOT pure
// self-trigger and a member event must mark only its named target.
const GROUP_JSON_NAMED_TRIGGER: &str = r#"{
    "TEST:grp_trig": {
        "+atomic": false,
        "level": { "+channel": "TEST:level.VAL", "+type": "plain", "+putorder": 0, "+trigger": "count" },
        "count": { "+channel": "TEST:count.VAL", "+type": "plain", "+putorder": 1, "+trigger": "level" }
    }
}"#;

// An *all-const* (channel-less) group: every member is `+type:const`, so
// no member has a backing DB channel and `GroupMonitor::start()` spawns
// zero member tasks. pvxs `groupsource.cpp:240-300` posts the single
// initial value and then leaves the subscription OPEN until the client
// cancels ("maybe post initial here in pathological case with no +channel
// (eg. all const)").
const GROUP_JSON_ALLCONST: &str = r#"{
    "TEST:allconst": {
        "+id": "epics:nt/NTScalar:1.0",
        "value": { "+type": "const", "+const": 42 },
        "label": { "+type": "const", "+const": "static" }
    }
}"#;

fn empty_request() -> PvStructure {
    PvStructure::new("epics:nt/NTRequest:1.0")
}

async fn make_db() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:level", Box::new(AiRecord::new(1.5)))
        .await
        .unwrap();
    db.add_record("TEST:count", Box::new(LonginRecord::new(7)))
        .await
        .unwrap();
    db
}

async fn make_db_na() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:level_na", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    db.add_record("TEST:count_na", Box::new(LonginRecord::new(0)))
        .await
        .unwrap();
    db
}

fn extract_double(s: &PvStructure, field: &str) -> Option<f64> {
    let f = s.fields.iter().find(|(n, _)| n == field).map(|(_, v)| v)?;
    if let PvField::Scalar(ScalarValue::Double(v)) = f {
        Some(*v)
    } else {
        None
    }
}

fn extract_long(s: &PvStructure, field: &str) -> Option<i64> {
    let f = s.fields.iter().find(|(n, _)| n == field).map(|(_, v)| v)?;
    match f {
        PvField::Scalar(ScalarValue::Long(v)) => Some(*v),
        PvField::Scalar(ScalarValue::Int(v)) => Some(*v as i64),
        _ => None,
    }
}

/// pvxs `testTable` parity for atomic groups: GET returns a struct
/// with both members populated from their backing records.
#[tokio::test]
async fn group_get_returns_all_members() {
    let db = make_db().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(GROUP_JSON).expect("load");
    let def = provider
        .groups()
        .get("TEST:grp")
        .cloned()
        .expect("grp registered");

    let ch = GroupChannel::new(db, def);
    let result = ch.get(&empty_request()).await.expect("get");

    assert_eq!(extract_double(&result, "level"), Some(1.5));
    assert_eq!(extract_long(&result, "count"), Some(7));
}

/// pvxs `testTable` PUT path: an atomic put updates both members,
/// and a subsequent GET reads back the new values.
#[tokio::test]
async fn group_atomic_put_updates_all_members() {
    let db = make_db().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(GROUP_JSON).expect("load");
    let def = provider
        .groups()
        .get("TEST:grp")
        .cloned()
        .expect("grp registered");

    let ch = GroupChannel::new(db, def);

    let mut put = PvStructure::new("epics:nt/NTGroup:1.0");
    put.fields
        .push(("level".into(), PvField::Scalar(ScalarValue::Double(42.0))));
    put.fields
        .push(("count".into(), PvField::Scalar(ScalarValue::Long(13))));
    ch.put(&put).await.expect("put");

    let result = ch.get(&empty_request()).await.expect("get-after-put");
    assert_eq!(extract_double(&result, "level"), Some(42.0));
    assert_eq!(extract_long(&result, "count"), Some(13));
}

/// pvxs `testTable` non-atomic path: same end state but the put
/// loop is sequential rather than locker-guarded.
#[tokio::test]
async fn group_nonatomic_put_updates_all_members() {
    let db = make_db_na().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider
        .load_group_config(GROUP_JSON_NONATOMIC)
        .expect("load");
    let def = provider
        .groups()
        .get("TEST:grp_na")
        .cloned()
        .expect("registered");
    let ch = GroupChannel::new(db, def);

    let mut put = PvStructure::new("structure");
    put.fields
        .push(("level".into(), PvField::Scalar(ScalarValue::Double(2.5))));
    put.fields
        .push(("count".into(), PvField::Scalar(ScalarValue::Long(99))));
    ch.put(&put).await.expect("put");

    let result = ch.get(&empty_request()).await.expect("get");
    assert!(matches!(
        extract_double(&result, "level"),
        Some(v) if (v - 2.5).abs() < 1e-9
    ));
    assert_eq!(extract_long(&result, "count"), Some(99));
}

/// a PUT that supplies a member field which is not putable
/// (no `+putorder`) writes nothing and must return a "No fields changed"
/// error, matching pvxs `groupsource.cpp:605-608`. An empty PUT (no
/// member field supplied) stays a silent no-op.
#[tokio::test]
async fn br_r60_put_with_no_writable_field_errors() {
    let db = make_db().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider
        .load_group_config(GROUP_JSON_READONLY)
        .expect("load");
    let def = provider
        .groups()
        .get("TEST:grp_ro")
        .cloned()
        .expect("grp_ro registered");
    let ch = GroupChannel::new(db, def);

    // Client supplies `level`, but no member is putable → nothing writes.
    let mut put = PvStructure::new("structure");
    put.fields
        .push(("level".into(), PvField::Scalar(ScalarValue::Double(42.0))));
    let err = ch
        .put(&put)
        .await
        .expect_err("PUT writing nothing must error");
    assert!(
        format!("{err}").contains("No fields changed"),
        "expected 'No fields changed', got: {err}"
    );

    // Empty PUT (no member field supplied) → silent no-op.
    let empty = PvStructure::new("structure");
    ch.put(&empty).await.expect("empty PUT is a silent no-op");
}

/// C-parity for the group PUT preparation pass: pvxs runs
/// `IOCSource::doPreProcessing` over every channeled member
/// (`groupsource.cpp:596-609`) BEFORE any marked/putable filtering and
/// in every process mode, throwing `S_db_putDisabled` when a member's
/// backing record is DISP-disabled. A DISP=1 member therefore rejects
/// the whole group PUT even when the client did not mark it — the prep
/// pass iterates `group.fields`, not just the changed ones.
///
/// Regression: the `Force`/`Inhibit` group routes call `put_pv`, which
/// (as the internal `dbPut` analogue) does not itself gate DISP, so a
/// forced group PUT to a DISP=1 member wrote through the interlock.
#[tokio::test]
async fn group_put_rejected_when_unmarked_member_is_disp_disabled() {
    for mode in [
        ProcessMode::Passive,
        ProcessMode::Force,
        ProcessMode::Inhibit,
    ] {
        let db = make_db().await;
        let provider = Arc::new(BridgeProvider::new(db.clone()));
        provider.load_group_config(GROUP_JSON).expect("load");
        let def = provider
            .groups()
            .get("TEST:grp")
            .cloned()
            .expect("grp registered");

        // DISP=1 on the `count` member's backing record. The PUT below
        // marks only `level`, leaving `count` unmarked — the prep pass
        // must still reject the whole operation.
        db.put_pv("TEST:count.DISP", EpicsValue::Char(1))
            .await
            .expect("set DISP");

        let ch = GroupChannel::new(db.clone(), def);

        let mut put = PvStructure::new("epics:nt/NTGroup:1.0");
        put.fields
            .push(("level".into(), PvField::Scalar(ScalarValue::Double(42.0))));
        let opts = PutOptions {
            process: mode,
            block: false,
        };
        let err = ch
            .put_with_options(&put, opts, None, &Default::default())
            .await
            .expect_err("DISP=1 member must reject the whole group PUT");
        let msg = format!("{err}").to_ascii_lowercase();
        assert!(
            msg.contains("disp") || msg.contains("disabled"),
            "{mode:?}: expected a DISP rejection, got: {err}"
        );

        // The whole PUT is rejected in the prep pass, before any member
        // write — the marked `level` member must be untouched.
        let result = ch.get(&empty_request()).await.expect("get");
        assert_eq!(
            extract_double(&result, "level"),
            Some(1.5),
            "{mode:?}: level must be untouched after a rejected PUT"
        );
    }
}

/// a `DBE_LOG`-only post against a backing record (archive
/// deadband fires without a value change) wakes the group monitor,
/// matching pvxs `groupsource.cpp:389` which subscribes group value
/// events with `DBE_VALUE | DBE_ALARM | DBE_ARCHIVE`.
///
/// Regression: the prior Rust mask was `VALUE|ALARM` only, so log
/// posts dropped silently on group monitors and archiver-like
/// clients tracking a group PV missed samples.
#[tokio::test]
async fn group_monitor_subscribes_archive_log_events() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_bridge_rs::qsrv::group::GroupMonitor;
    use epics_bridge_rs::qsrv::provider::PvaMonitor;
    use std::time::Duration;

    let db = make_db().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(GROUP_JSON).expect("load");
    let def = provider
        .groups()
        .get("TEST:grp")
        .cloned()
        .expect("grp registered");

    let mut mon = GroupMonitor::new(db.clone(), def);
    mon.start().await.expect("start");

    // Post a LOG-ONLY event on `level.VAL`. No VALUE / ALARM bit set;
    // if the bridge had subscribed only with VALUE|ALARM the event
    // would silently drop and the following poll would time out. The
    // group monitor is purely delta-driven (the wire layer owns the
    // initial frame), so this delta must wake poll() directly.
    {
        let rec = db.get_record("TEST:level").await.expect("rec exists");
        let inst = rec.read().await;
        inst.notify_field("VAL", EventMask::LOG);
    }

    let polled = tokio::time::timeout(Duration::from_millis(500), mon.poll()).await;
    let snap = polled
        .expect("LOG event must wake group poll within 500ms")
        .expect("snapshot delivered");
    assert!(
        !snap.value.fields.is_empty(),
        "log-event group snapshot must carry the full group structure, got {snap:?}"
    );

    mon.stop().await;
}

/// An all-const (channel-less) group monitor must treat "no member event
/// sources" as *quiet but open*, not as a closed stream. pvxs keeps such a
/// subscription open after the initial post (`groupsource.cpp:240-300`),
/// whereas the pre-fix Rust `GroupMonitor::poll()` returned `None` the
/// instant its member-event channel closed — and the native PVA server
/// turns that `None` into a premature MONITOR FINISH (`subcmd 0x10`).
///
/// After the keepalive-sender fix, `poll()` parks indefinitely (no member
/// ever posts), so a bounded `poll()` must TIME OUT rather than resolve to
/// `None`.
///
/// FAIL-proof: removing `self.event_tx = Some(tx)` in `GroupMonitor::start`
/// makes the fan-in channel close as soon as `start()` returns, so `poll()`
/// resolves to `None` immediately and `timeout(...)` is `Ok(None)` — the
/// `is_err()` assertion fails.
#[tokio::test]
async fn allconst_group_monitor_poll_parks_instead_of_finishing() {
    use epics_bridge_rs::qsrv::group::GroupMonitor;
    use epics_bridge_rs::qsrv::provider::PvaMonitor;
    use std::time::Duration;

    // No DB records needed — every member is `+const`.
    let db = Arc::new(PvDatabase::new());
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider
        .load_group_config(GROUP_JSON_ALLCONST)
        .expect("all-const group must load");
    let def = provider
        .groups()
        .get("TEST:allconst")
        .cloned()
        .expect("all-const group registered");

    let mut mon = GroupMonitor::new(db.clone(), def);
    mon.start().await.expect("start");

    // poll() must PARK (the wire layer already sent the one initial frame);
    // there is no member event source, so it must NOT resolve to None.
    let polled = tokio::time::timeout(Duration::from_millis(500), mon.poll()).await;
    assert!(
        polled.is_err(),
        "all-const group poll() must park (quiet but open), not resolve to {polled:?} — \
         a resolved None becomes a premature MONITOR FINISH"
    );

    mon.stop().await;
}

/// End-to-end through the native-PVA `ChannelSource` adapter: an all-const
/// group's MONITOR stream must stay OPEN and quiet after the initial frame
/// (no FINISH), and must tear down promptly when the client cancels (the
/// downstream receiver is dropped).
///
/// Pre-fix, `subscribe` opened the group monitor, `poll()` returned `None`
/// at once, and the forward task dropped its sender — closing the receiver
/// (the wire-level FINISH). After the keepalive fix the stream stays open;
/// after the forward-task `tokio::select!` on `tx.closed()` it still tears
/// down on cancel instead of leaking the parked monitor (and its
/// `Arc<PvDatabase>` clones) forever.
///
/// FAIL-proof (open half): reverting the keepalive closes the receiver, so
/// the first `recv()` resolves to `None` within 500 ms and the `is_err()`
/// assertion fails. FAIL-proof (teardown half): reverting the forward-task
/// `tokio::select!` leaves `poll()` parked forever for an all-const group,
/// so the monitor — and its `Arc<PvDatabase>` clones — never drop after the
/// receiver is dropped, and the strong count never returns to baseline.
#[tokio::test]
async fn allconst_group_subscribe_stays_open_then_tears_down_on_cancel() {
    use epics_bridge_rs::qsrv::QsrvPvStore;
    use epics_pva_rs::server_native::ChannelSource;
    use std::time::Duration;

    let db = Arc::new(PvDatabase::new());
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider
        .load_group_config(GROUP_JSON_ALLCONST)
        .expect("all-const group must load");
    let store = QsrvPvStore::new(provider);

    // Baseline strong count after the provider/store exist but before the
    // monitor opens. The forward task's `GroupMonitor`/`GroupChannel` add
    // `Arc<PvDatabase>` clones on top of this; teardown must release them.
    let baseline = Arc::strong_count(&db);

    let mut rx = ChannelSource::subscribe(&store, "TEST:allconst")
        .await
        .expect("subscribe opens the all-const group monitor");

    // Open half: the stream stays quiet (the wire layer owns the single
    // initial frame) and OPEN — no FINISH/close. A pre-fix close would make
    // recv() resolve to None within the window.
    let recv = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
    assert!(
        recv.is_err(),
        "all-const group monitor stream must stay open and quiet (parked), got {recv:?}"
    );

    // Teardown half: dropping the downstream receiver (client cancel) must
    // tear the forward task + monitor down, releasing the `Arc<PvDatabase>`
    // clones the GroupMonitor/GroupChannel hold, so the strong count returns
    // to baseline. A parked poll() with no `tx.closed()` select would leak
    // it forever.
    drop(rx);
    let mut returned = false;
    for _ in 0..400 {
        if Arc::strong_count(&db) == baseline {
            returned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        returned,
        "dropping the receiver must tear down the parked all-const monitor; \
         db strong-count {} did not return to baseline {baseline}",
        Arc::strong_count(&db)
    );
}

/// a group GET / MONITOR value carries
/// `record._options.queueSize` and `record._options.atomic` at
/// its root. pvxs `groupsource.cpp:359` stamps these into every
/// posted value; strict pvRequest clients and archiver appliances
/// depend on the branch being present.
#[tokio::test]
async fn group_get_carries_record_options_queue_size_and_atomic() {
    let db = make_db().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(GROUP_JSON).expect("load");
    let def = provider
        .groups()
        .get("TEST:grp")
        .cloned()
        .expect("grp registered");

    let ch = GroupChannel::new(db, def);
    let result = ch.get(&empty_request()).await.expect("get");

    let record = result
        .fields
        .iter()
        .find(|(n, _)| n == "record")
        .map(|(_, v)| v)
        .expect("record sub-structure must be present");
    let record_struct = match record {
        PvField::Structure(s) => s,
        other => panic!("expected record to be structure, got {other:?}"),
    };
    let options = record_struct
        .fields
        .iter()
        .find(|(n, _)| n == "_options")
        .map(|(_, v)| v)
        .expect("record._options must be present");
    let options_struct = match options {
        PvField::Structure(s) => s,
        other => panic!("expected _options to be structure, got {other:?}"),
    };
    // queueSize: a GET has no monitor subscription queue, so pvxs leaves
    // the value-template default 0 — `groupsource.cpp:480-485` stamps only
    // `atomic`, and `test/testqgroup.cpp:60-66` shows GET reports
    // `record._options.queueSize int32_t = 0`. The negotiated depth is a
    // monitor-only concern (see br_r33_group_monitor_stamps_negotiated_queue_size).
    let qs = options_struct
        .fields
        .iter()
        .find(|(n, _)| n == "queueSize")
        .map(|(_, v)| v)
        .expect("queueSize must be present");
    match qs {
        PvField::Scalar(ScalarValue::Int(n)) => {
            assert_eq!(
                *n, 0,
                "GET must report the template-default queueSize 0, got {n}"
            )
        }
        other => panic!("expected int queueSize, got {other:?}"),
    }
    // atomic: the group default is atomic=true (GROUP_JSON).
    let atomic = options_struct
        .fields
        .iter()
        .find(|(n, _)| n == "atomic")
        .map(|(_, v)| v)
        .expect("atomic must be present");
    match atomic {
        PvField::Scalar(ScalarValue::Boolean(b)) => {
            assert!(*b, "atomic must reflect group default")
        }
        other => panic!("expected bool atomic, got {other:?}"),
    }
}

/// a group monitor stamps the *per-operation
/// negotiated* `record._options.queueSize` — the value resolved
/// from the MONITOR INIT pvRequest — not a hardcoded constant.
/// pvxs `servermon.cpp:533-540` parses `record._options.queueSize`
/// (kept iff >= 2) into `op->limit`, then `groupsource.cpp:359`
/// stamps `stats.limitQueue` into the monitor value.
#[tokio::test]
async fn br_r33_group_monitor_stamps_negotiated_queue_size() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_bridge_rs::qsrv::group::{
        GROUP_DEFAULT_QUEUE_SIZE, GroupMonitor, negotiated_queue_size,
    };
    use epics_bridge_rs::qsrv::provider::PvaMonitor;
    use std::time::Duration;

    // Build a MONITOR INIT pvRequest carrying
    // `record._options.queueSize = 32`.
    let mk_request = |qsize: i32| {
        let mut opts = PvStructure::new("");
        opts.fields
            .push(("queueSize".into(), PvField::Scalar(ScalarValue::Int(qsize))));
        let mut record = PvStructure::new("");
        record
            .fields
            .push(("_options".into(), PvField::Structure(opts)));
        let mut req = PvStructure::new("epics:nt/NTRequest:1.0");
        req.fields
            .push(("record".into(), PvField::Structure(record)));
        req
    };

    // Negotiation rule (pvxs `servermon.cpp:533`): >= 2 honoured.
    assert_eq!(negotiated_queue_size(&mk_request(32)), 32);
    // < 2 → default kept.
    assert_eq!(
        negotiated_queue_size(&mk_request(1)),
        GROUP_DEFAULT_QUEUE_SIZE
    );
    // Absent `record._options.queueSize` → default.
    assert_eq!(
        negotiated_queue_size(&empty_request()),
        GROUP_DEFAULT_QUEUE_SIZE
    );

    // The monitor stamps the negotiated value into its snapshots.
    let queue_size_in_snapshot =
        |def: epics_bridge_rs::qsrv::GroupPvDef, db: Arc<PvDatabase>, negotiated: i32| async move {
            let mut mon = GroupMonitor::new(db.clone(), def).with_queue_size(negotiated);
            mon.start().await.expect("start");
            // A member post wakes the delta-driven monitor; the emitted
            // value carries the stamped record._options.queueSize.
            for rec_name in ["TEST:level", "TEST:count"] {
                let rec = db.get_record(rec_name).await.expect("rec exists");
                rec.read().await.notify_field("VAL", EventMask::VALUE);
            }
            let snap = tokio::time::timeout(Duration::from_secs(2), mon.poll())
                .await
                .expect("first monitor delta within 2s")
                .expect("snapshot");
            mon.stop().await;
            let record = match snap
                .value
                .fields
                .iter()
                .find(|(n, _)| n == "record")
                .map(|(_, v)| v)
            {
                Some(PvField::Structure(s)) => s.clone(),
                other => panic!("record sub-structure missing: {other:?}"),
            };
            let options = match record
                .fields
                .iter()
                .find(|(n, _)| n == "_options")
                .map(|(_, v)| v)
            {
                Some(PvField::Structure(s)) => s.clone(),
                other => panic!("record._options missing: {other:?}"),
            };
            match options
                .fields
                .iter()
                .find(|(n, _)| n == "queueSize")
                .map(|(_, v)| v)
            {
                Some(PvField::Scalar(ScalarValue::Int(n))) => *n,
                other => panic!("queueSize missing: {other:?}"),
            }
        };

    // Negotiated 32 → snapshot carries 32.
    let db = make_db().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(GROUP_JSON).expect("load");
    let def = provider.groups().get("TEST:grp").cloned().expect("grp");
    let qs = queue_size_in_snapshot(def, db, negotiated_queue_size(&mk_request(32))).await;
    assert_eq!(
        qs, 32,
        "monitor must stamp the negotiated queueSize (32), not a hardcoded default"
    );

    // No negotiation → default GROUP_DEFAULT_QUEUE_SIZE.
    let db2 = make_db().await;
    let provider2 = Arc::new(BridgeProvider::new(db2.clone()));
    provider2.load_group_config(GROUP_JSON).expect("load");
    let def2 = provider2.groups().get("TEST:grp").cloned().expect("grp");
    let qs_default =
        queue_size_in_snapshot(def2, db2, negotiated_queue_size(&empty_request())).await;
    assert_eq!(
        qs_default, GROUP_DEFAULT_QUEUE_SIZE,
        "absent queueSize → monitor stamps the default"
    );
}

/// a group MONITOR stamps `record._options.atomic = true`
/// unconditionally, regardless of the group's `+atomic:false`
/// default, while a GET on the same group reports the actual
/// operation atomicity (`false`). pvxs `groupsource.cpp:401-405`
/// (GroupMonitor::onStart) always sets the monitor value's atomic
/// flag true — a monitor delivers one consistent snapshot, so it
/// reports itself atomic — whereas `groupsource.cpp:480-485`
/// (GroupSource::onOp, the GET path) stamps the per-operation
/// atomicity. Before the fix both paths reported the group default,
/// so a `+atomic:false` group's monitor wrongly advertised `false`.
#[tokio::test]
async fn group_monitor_stamps_atomic_true_while_get_reports_operation_atomicity() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_bridge_rs::qsrv::group::GroupMonitor;
    use epics_bridge_rs::qsrv::provider::PvaMonitor;
    use std::time::Duration;

    // Extract `record._options.atomic` from a posted/returned value.
    fn atomic_of(s: &PvStructure) -> bool {
        let record = match s.fields.iter().find(|(n, _)| n == "record").map(|(_, v)| v) {
            Some(PvField::Structure(r)) => r,
            other => panic!("record sub-structure missing: {other:?}"),
        };
        let options = match record
            .fields
            .iter()
            .find(|(n, _)| n == "_options")
            .map(|(_, v)| v)
        {
            Some(PvField::Structure(o)) => o,
            other => panic!("record._options missing: {other:?}"),
        };
        match options
            .fields
            .iter()
            .find(|(n, _)| n == "atomic")
            .map(|(_, v)| v)
        {
            Some(PvField::Scalar(ScalarValue::Boolean(b))) => *b,
            other => panic!("record._options.atomic missing: {other:?}"),
        }
    }

    // GET path: a `+atomic:false` group reports the operation
    // atomicity, which defaults to the group default (false).
    let db = make_db_na().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider
        .load_group_config(GROUP_JSON_NONATOMIC)
        .expect("load");
    let def = provider
        .groups()
        .get("TEST:grp_na")
        .cloned()
        .expect("grp_na registered");
    assert!(!def.atomic, "fixture must be a non-atomic group");

    let ch = GroupChannel::new(db.clone(), def.clone());
    let get_result = ch.get(&empty_request()).await.expect("get");
    assert!(
        !atomic_of(&get_result),
        "GET on a +atomic:false group must report operation atomicity (false)"
    );

    // MONITOR path: the same group's monitor snapshot reports
    // atomic = true unconditionally.
    let mut mon = GroupMonitor::new(db.clone(), def);
    mon.start().await.expect("start");
    for rec_name in ["TEST:level_na", "TEST:count_na"] {
        let rec = db.get_record(rec_name).await.expect("rec exists");
        rec.read().await.notify_field("VAL", EventMask::VALUE);
    }
    let snap = tokio::time::timeout(Duration::from_secs(2), mon.poll())
        .await
        .expect("first monitor delta within 2s")
        .expect("snapshot");
    mon.stop().await;
    assert!(
        atomic_of(&snap.value),
        "group MONITOR must stamp record._options.atomic = true even for a +atomic:false group"
    );
}

/// a per-member ACF denial fails the group PUT even when the
/// group PV itself is writable. Mirrors pvxs's per-field
/// SecurityClient gating (groupsource.cpp:161 + 515) — "any
/// member denied → operation rejected".
#[tokio::test]
async fn group_put_member_acf_denial_rejects_entire_put() {
    use epics_bridge_rs::qsrv::{AccessContext, AccessControl};

    /// Deny writes to a specific record (matching by full PV name as
    /// stored on the GroupMember.channel — `record.FIELD`).
    struct DenySpecific(String);
    impl AccessControl for DenySpecific {
        fn can_write(&self, channel: &str, _: &str, _: &str) -> bool {
            channel != self.0
        }
    }

    let db = make_db().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(GROUP_JSON).expect("load");
    let def = provider
        .groups()
        .get("TEST:grp")
        .cloned()
        .expect("grp registered");

    // Deny writes to the `count` member's backing record. The group
    // PV itself remains writable.
    let access = AccessContext::anonymous(Arc::new(DenySpecific("TEST:count.VAL".into())));
    let ch = GroupChannel::new(db.clone(), def).with_access(access);

    let mut put = PvStructure::new("epics:nt/NTGroup:1.0");
    put.fields
        .push(("level".into(), PvField::Scalar(ScalarValue::Double(42.0))));
    put.fields
        .push(("count".into(), PvField::Scalar(ScalarValue::Long(13))));
    let result = ch.put(&put).await;
    let err = result.expect_err("group PUT must be rejected");
    let msg = format!("{err}");
    // pvxs's `doFieldPreProcessing` throws the bare contract text
    // (iocsource.cpp:385); the denied member channel is a server-log detail,
    // never part of the Status.message the client receives.
    assert_eq!(
        msg, "put rejected: Put not permitted",
        "member ACF denial must carry pvxs's contract text and no member identity"
    );

    // Verify the allowed member's record was NOT pre-emptively
    // mutated either — pvxs rejects the whole operation before any
    // member apply runs.
    let level = {
        let rec = db.get_record("TEST:level").await.unwrap();
        let inst = rec.read().await;
        inst.snapshot_for_field("VAL").map(|s| s.value)
    };
    assert!(
        matches!(level, Some(epics_base_rs::types::EpicsValue::Double(v)) if (v - 1.5).abs() < 1e-9),
        "allowed member must remain at seed value 1.5 after rejected group PUT, got {level:?}"
    );
}

/// a group root meta member `""` flattens its meta sub-fields
/// (`alarm`, `timeStamp`) into the group root, matching pvxs
/// (test/ntenum.db:6, test/testqgroup.cpp:168). The earlier path
/// silently no-oped on the empty member-path, dropping root meta.
#[tokio::test]
async fn group_root_meta_member_flattens_into_root() {
    const ROOT_META_JSON: &str = r#"{
        "TEST:rootmeta": {
            "+id": "epics:nt/NTEnum:1.0",
            "": { "+channel": "TEST:val.VAL", "+type": "meta" },
            "value": { "+channel": "TEST:val.VAL", "+type": "plain" }
        }
    }"#;

    let db = Arc::new(PvDatabase::new());
    db.add_record("TEST:val", Box::new(AiRecord::new(7.5)))
        .await
        .unwrap();

    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(ROOT_META_JSON).expect("load");
    let def = provider
        .groups()
        .get("TEST:rootmeta")
        .cloned()
        .expect("registered");

    let ch = GroupChannel::new(db, def);
    let result = ch.get(&empty_request()).await.expect("get");

    let names: Vec<&str> = result.fields.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        names.contains(&"value"),
        "root must carry the non-meta member; got {names:?}"
    );
    assert!(
        names.contains(&"alarm"),
        "root meta must flatten `alarm` onto group root; got {names:?}"
    );
    assert!(
        names.contains(&"timeStamp"),
        "root meta must flatten `timeStamp` onto group root; got {names:?}"
    );
    // Root must NOT carry an empty-name sub-structure (the previous
    // silent no-op path would have produced no meta at all; the
    // accidental-merge path would risk creating a `""` child).
    assert!(
        !names.contains(&""),
        "root must not carry an empty-name sub-field; got {names:?}"
    );
}

/// an explicit named `+trigger` graph marks only the
/// named target field on a member event — not the source field, and
/// not the whole group. `level` triggers `count` and vice versa, so a
/// `level` post marks exactly `["count"]` and a `count` post marks
/// exactly `["level"]`. Before the fix every trigger kind re-read the
/// full group and emitted a full request mask, making named triggers
/// behave like `+trigger:"*"`.
#[tokio::test]
async fn br_fr12_named_trigger_marks_only_target() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_bridge_rs::qsrv::group::GroupMonitor;
    use epics_bridge_rs::qsrv::provider::PvaMonitor;
    use std::time::Duration;

    let db = make_db().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider
        .load_group_config(GROUP_JSON_NAMED_TRIGGER)
        .expect("load");
    let def = provider
        .groups()
        .get("TEST:grp_trig")
        .cloned()
        .expect("grp_trig registered");
    assert!(
        !def.is_pure_self_trigger(),
        "a named-trigger group must not be classified pure self-trigger"
    );

    let mut mon = GroupMonitor::new(db.clone(), def);
    mon.start().await.expect("start");

    // The monitor is purely delta-driven: the wire layer owns the full
    // initial frame, so poll() carries only fresh member deltas with
    // their resolved marked set (no priming snapshot precedes them).

    // A `level` post triggers only `count`.
    {
        let rec = db.get_record("TEST:level").await.expect("rec exists");
        rec.read().await.notify_field("VAL", EventMask::VALUE);
    }
    let ev = tokio::time::timeout(Duration::from_millis(500), mon.poll())
        .await
        .expect("level event wakes poll within 500ms")
        .expect("snapshot");
    assert_eq!(
        ev.marked,
        Some(vec!["count".to_string()]),
        "a `level` post must mark only its named trigger target `count`, \
         not itself and not the whole group"
    );

    // A `count` post triggers only `level`.
    {
        let rec = db.get_record("TEST:count").await.expect("rec exists");
        rec.read().await.notify_field("VAL", EventMask::VALUE);
    }
    let ev = tokio::time::timeout(Duration::from_millis(500), mon.poll())
        .await
        .expect("count event wakes poll within 500ms")
        .expect("snapshot");
    assert_eq!(
        ev.marked,
        Some(vec!["level".to_string()]),
        "a `count` post must mark only its named trigger target `level`"
    );

    mon.stop().await;
}

/// A pure self-trigger group keeps the
/// value-diff path — every member defaults `+trigger`, so the monitor
/// derives the changed-bitset (`marked: None`) instead of carrying an
/// explicit set. This guards that the new marked-set path does not
/// regress the existing self-trigger narrowing.
#[tokio::test]
async fn br_fr12_pure_self_trigger_derives_bitset() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_bridge_rs::qsrv::group::GroupMonitor;
    use epics_bridge_rs::qsrv::provider::PvaMonitor;
    use std::time::Duration;

    let db = make_db().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(GROUP_JSON).expect("load");
    let def = provider
        .groups()
        .get("TEST:grp")
        .cloned()
        .expect("grp registered");
    assert!(
        def.is_pure_self_trigger(),
        "the default-trigger group must be pure self-trigger"
    );

    let mut mon = GroupMonitor::new(db.clone(), def);
    mon.start().await.expect("start");

    // Delta-driven monitor: a single `level` post yields one delta. A
    // pure self-trigger group derives its changed-bitset (marked: None)
    // rather than carrying an explicit marked set.
    {
        let rec = db.get_record("TEST:level").await.expect("rec exists");
        rec.read().await.notify_field("VAL", EventMask::VALUE);
    }
    let ev = tokio::time::timeout(Duration::from_millis(500), mon.poll())
        .await
        .expect("level event wakes poll within 500ms")
        .expect("snapshot");
    assert!(
        ev.marked.is_none(),
        "a pure self-trigger group must derive its bitset (marked: None), got {:?}",
        ev.marked
    );

    mon.stop().await;
}

/// a group monitor must forward an active member's
/// update without waiting for a quiet member to change after start.
/// `TEST:grp` has two Plain members (`level`, `count`); only `level`
/// posts, `count` stays quiet. The old per-member priming gate withheld
/// every delta until BOTH members posted their initial events, so
/// `level`'s update was discarded indefinitely. pvxs primes each field
/// from sampled values at start (db_post_single_event,
/// `groupsource.cpp:289-297`), so a quiet member never blocks an active
/// one. The 500ms timeout is the guard: before the fix poll() never
/// returned because `count`'s priming flag stayed unset.
#[tokio::test]
async fn br113_quiet_member_does_not_block_active_member_update() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_bridge_rs::qsrv::group::GroupMonitor;
    use epics_bridge_rs::qsrv::provider::PvaMonitor;
    use std::time::Duration;

    let db = make_db().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(GROUP_JSON).expect("load");
    let def = provider
        .groups()
        .get("TEST:grp")
        .cloned()
        .expect("grp registered");

    let mut mon = GroupMonitor::new(db.clone(), def);
    mon.start().await.expect("start");

    // Only `level` changes after start; `count` never posts.
    {
        let rec = db.get_record("TEST:level").await.expect("rec exists");
        rec.read().await.notify_field("VAL", EventMask::VALUE);
    }
    let ev = tokio::time::timeout(Duration::from_millis(500), mon.poll())
        .await
        .expect("active member's update must flow without waiting for the quiet member")
        .expect("snapshot");
    // The delta carries the full group structure read fresh; `count`'s
    // current value is included even though it never posted.
    assert!(
        !ev.value.fields.is_empty(),
        "active-member delta must carry the full group structure, got {ev:?}"
    );

    mon.stop().await;
}

/// Guard: dbLoadGroup → processGroups → groups() exposes the parsed
/// definition with the expected member roster.
#[tokio::test]
async fn group_config_parses_and_finalizes() {
    let db = make_db().await;
    let provider = BridgeProvider::new(db);
    provider.load_group_config(GROUP_JSON).expect("load");
    let n = provider.process_groups();
    assert_eq!(n, 1);
    let groups = provider.groups();
    let def = groups.get("TEST:grp").expect("registered");
    assert!(def.atomic);
    assert_eq!(def.struct_id.as_deref(), Some("epics:nt/NTGroup:1.0"));
    assert_eq!(def.members.len(), 2);
    let names: Vec<&str> = def.members.iter().map(|m| m.field_name.as_str()).collect();
    assert!(names.contains(&"level"));
    assert!(names.contains(&"count"));
}

const GROUP_JSON_STRUCT_MEMBER: &str = r#"{
    "TEST:sgrp": {
        "+id": "epics:nt/NTGroup:1.0",
        "+atomic": true,
        "value": { "+channel": "TEST:level.VAL", "+type": "plain" },
        "meta":  { "+type": "structure", "+id": "alpha/v1" }
    }
}"#;

const GROUP_JSON_STRUCT_MEMBER_NA: &str = r#"{
    "TEST:sgrp_na": {
        "+atomic": false,
        "value": { "+channel": "TEST:level.VAL", "+type": "plain" },
        "meta":  { "+type": "structure", "+id": "alpha/v1" }
    }
}"#;

fn find_field<'a>(s: &'a PvStructure, name: &str) -> Option<&'a PvField> {
    s.fields.iter().find(|(n, _)| n == name).map(|(_, v)| v)
}

/// A `+type:"structure"` member must appear in the GET value as an
/// empty structure carrying its `+id`, matching the advertised
/// descriptor. pvxs adds the empty `Struct(id)` to the value template
/// (groupconfigprocessor.cpp:922-930) and clones it into every GET /
/// MONITOR snapshot (groupsource.cpp:480-518).
#[tokio::test]
async fn structure_member_appears_in_atomic_group_value_and_descriptor() {
    use epics_pva_rs::pvdata::FieldDesc;

    let db = make_db().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider
        .load_group_config(GROUP_JSON_STRUCT_MEMBER)
        .expect("load");
    let def = provider
        .groups()
        .get("TEST:sgrp")
        .cloned()
        .expect("registered");
    let ch = GroupChannel::new(db, def);

    // GET value carries the empty structure branch with the member +id.
    let result = ch.get(&empty_request()).await.expect("get");
    match find_field(&result, "meta") {
        Some(PvField::Structure(s)) => {
            assert!(s.fields.is_empty(), "structure member value must be empty");
            assert_eq!(
                s.struct_id, "alpha/v1",
                "value struct id must carry member +id"
            );
        }
        other => panic!("group value must contain empty `meta` structure, got {other:?}"),
    }
    // The backed member is still present.
    assert_eq!(extract_double(&result, "value"), Some(1.5));

    // Descriptor advertises the same empty structure with the +id.
    let desc = ch.get_field().await.expect("get_field");
    let fields = match &desc {
        FieldDesc::Structure { fields, .. } => fields,
        other => panic!("group descriptor must be a structure, got {other:?}"),
    };
    match fields.iter().find(|(n, _)| n == "meta").map(|(_, d)| d) {
        Some(FieldDesc::Structure { struct_id, fields }) => {
            assert_eq!(struct_id, "alpha/v1");
            assert!(fields.is_empty(), "structure descriptor must be empty");
        }
        other => panic!("descriptor must contain empty `meta` structure with +id, got {other:?}"),
    }
}

/// Same as above for a non-atomic group, exercising the non-atomic
/// read loop which also skipped Structure members before the fix.
#[tokio::test]
async fn structure_member_appears_in_nonatomic_group_value() {
    let db = make_db().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider
        .load_group_config(GROUP_JSON_STRUCT_MEMBER_NA)
        .expect("load");
    let def = provider
        .groups()
        .get("TEST:sgrp_na")
        .cloned()
        .expect("registered");
    let ch = GroupChannel::new(db, def);

    let result = ch.get(&empty_request()).await.expect("get");
    match find_field(&result, "meta") {
        Some(PvField::Structure(s)) => {
            assert!(s.fields.is_empty());
            assert_eq!(s.struct_id, "alpha/v1");
        }
        other => {
            panic!("non-atomic group value must contain empty `meta` structure, got {other:?}")
        }
    }
    assert_eq!(extract_double(&result, "value"), Some(1.5));
}

/// GET_FIELD / CREATE_CHANNEL must advertise the built-in
/// `record._options` branch (queueSize int, atomic boolean) that GET
/// and MONITOR values carry, matching pvxs `group.valueTemplate`
/// (groupconfigprocessor.cpp:499-523).
#[tokio::test]
async fn group_descriptor_advertises_record_options() {
    use epics_pva_rs::pvdata::{FieldDesc, ScalarType};

    let db = make_db().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(GROUP_JSON).expect("load");
    let def = provider
        .groups()
        .get("TEST:grp")
        .cloned()
        .expect("registered");
    let ch = GroupChannel::new(db, def);

    let desc = ch.get_field().await.expect("get_field");
    let root = match &desc {
        FieldDesc::Structure { fields, .. } => fields,
        other => panic!("group descriptor must be a structure, got {other:?}"),
    };
    let opts = match root.iter().find(|(n, _)| n == "record").map(|(_, d)| d) {
        Some(FieldDesc::Structure { fields, .. }) => {
            fields.iter().find(|(n, _)| n == "_options").map(|(_, d)| d)
        }
        other => panic!("descriptor must contain a `record` structure, got {other:?}"),
    };
    match opts {
        Some(FieldDesc::Structure { fields, .. }) => {
            assert!(
                matches!(
                    fields
                        .iter()
                        .find(|(n, _)| n == "queueSize")
                        .map(|(_, d)| d),
                    Some(FieldDesc::Scalar(ScalarType::Int))
                ),
                "record._options.queueSize must be advertised as int"
            );
            assert!(
                matches!(
                    fields.iter().find(|(n, _)| n == "atomic").map(|(_, d)| d),
                    Some(FieldDesc::Scalar(ScalarType::Boolean))
                ),
                "record._options.atomic must be advertised as boolean"
            );
        }
        other => panic!("descriptor must contain `record._options`, got {other:?}"),
    }

    // The GET value conforms: it carries the same record._options branch.
    let val = ch.get(&empty_request()).await.expect("get");
    assert!(
        matches!(find_field(&val, "record"), Some(PvField::Structure(_))),
        "GET value must carry the record._options branch the descriptor advertises"
    );
}

/// a `+type:"any"` group member is advertised as a PVA `any`
/// slot (FieldDesc::Variant), its GET value is a Variant wrapping the
/// concrete scalar/array payload (tagged with that payload's descriptor),
/// and a PUT of a Variant payload is dereferenced and written. pvxs builds
/// the descriptor with `Member(TypeCode::Any,…)`
/// (groupconfigprocessor.cpp:904-910), fills the slot via
/// `anyType.cloneEmpty()` + `node.from(value)` (iocsource.cpp:335-349),
/// and dereferences `node["->"]` on PUT (iocsource.cpp:575-586). Before
/// the fix the member was advertised/served as a fixed scalar and a
/// Variant PUT was rejected by `pv_field_to_epics`.
#[tokio::test]
async fn br76_any_member_is_variant_descriptor_value_and_put() {
    use epics_base_rs::server::records::waveform::WaveformRecord;
    use epics_base_rs::types::{DbFieldType, EpicsValue};
    use epics_pva_rs::pvdata::{FieldDesc, ScalarType, VariantValue};

    let db = Arc::new(PvDatabase::new());
    db.add_record("B76:sc", Box::new(AiRecord::new(2.5)))
        .await
        .unwrap();
    db.add_record(
        "B76:wf",
        Box::new(WaveformRecord::new(8, DbFieldType::Double)),
    )
    .await
    .unwrap();
    db.put_pv("B76:wf", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
        .await
        .unwrap();

    let json = r#"{
        "B76:grp": {
            "+atomic": true,
            "sc": { "+channel": "B76:sc.VAL", "+type": "any", "+putorder": 0 },
            "wf": { "+channel": "B76:wf.VAL", "+type": "any", "+putorder": 1 }
        }
    }"#;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(json).expect("load");
    let def = provider
        .groups()
        .get("B76:grp")
        .cloned()
        .expect("registered");
    let ch = GroupChannel::new(db.clone(), def);

    // Descriptor: both `any` members advertise FieldDesc::Variant.
    let desc = ch.get_field().await.expect("get_field");
    let root = match &desc {
        FieldDesc::Structure { fields, .. } => fields,
        other => panic!("group descriptor must be a structure, got {other:?}"),
    };
    for name in ["sc", "wf"] {
        let d = root.iter().find(|(n, _)| n == name).map(|(_, d)| d);
        assert!(
            matches!(d, Some(FieldDesc::Variant)),
            "`+type:any` member `{name}` must advertise FieldDesc::Variant, got {d:?}"
        );
    }

    // GET value: each member is a Variant carrying the concrete payload
    // descriptor (scalar double / double array), not a bare scalar.
    let val = ch.get(&empty_request()).await.expect("get");
    match find_field(&val, "sc") {
        Some(PvField::Variant(v)) => {
            assert_eq!(
                v.desc,
                Some(FieldDesc::Scalar(ScalarType::Double)),
                "scalar any payload must tag the concrete scalar descriptor"
            );
            assert!(
                matches!(&v.value, PvField::Scalar(ScalarValue::Double(d)) if (*d - 2.5).abs() < 1e-9),
                "scalar any value must carry the record value, got {:?}",
                v.value
            );
        }
        other => panic!("scalar any member must be a Variant, got {other:?}"),
    }
    match find_field(&val, "wf") {
        Some(PvField::Variant(v)) => {
            assert_eq!(
                v.desc,
                Some(FieldDesc::ScalarArray(ScalarType::Double)),
                "array any payload must tag the concrete array descriptor"
            );
            assert!(
                matches!(
                    &v.value,
                    PvField::ScalarArray(_) | PvField::ScalarArrayTyped(_)
                ),
                "array any value must carry the array payload, got {:?}",
                v.value
            );
        }
        other => panic!("array any member must be a Variant, got {other:?}"),
    }

    // PUT: a Variant payload for the scalar member is dereferenced and written.
    let mut put = PvStructure::new("epics:nt/NTGroup:1.0");
    put.fields.push((
        "sc".into(),
        PvField::Variant(Box::new(VariantValue {
            desc: Some(FieldDesc::Scalar(ScalarType::Double)),
            value: PvField::Scalar(ScalarValue::Double(7.25)),
        })),
    ));
    ch.put(&put).await.expect("variant put accepted");

    let after = ch.get(&empty_request()).await.expect("get-after-put");
    match find_field(&after, "sc") {
        Some(PvField::Variant(v)) => assert!(
            matches!(&v.value, PvField::Scalar(ScalarValue::Double(d)) if (*d - 7.25).abs() < 1e-9),
            "variant PUT must update the backing record, got {:?}",
            v.value
        ),
        other => panic!("scalar any member must be a Variant after put, got {other:?}"),
    }
}

/// A group member whose field path uses `[N]` index notation
/// (`a[0].x`) must build a PVA `StructureArray`, not a plain nested
/// structure. pvxs wraps each indexed path component in `StructA(...)`
/// when assembling the group type (groupconfigprocessor.cpp:1005-1035)
/// and lands the runtime value in element `[N]` of that structure array
/// (groupsource.cpp:414-425). Before the fix the value/descriptor
/// builders ignored `comp.index` and produced a plain `Structure`, so
/// the `[N]` notation in the config was silently dropped.
///
/// Homogeneous case: `a[0].x` and `a[1].x` share one element schema.
#[tokio::test]
async fn br52_indexed_member_builds_homogeneous_structure_array() {
    use epics_pva_rs::pvdata::FieldDesc;

    let db = Arc::new(PvDatabase::new());
    db.add_record("B52:r0", Box::new(AiRecord::new(10.0)))
        .await
        .unwrap();
    db.add_record("B52:r1", Box::new(AiRecord::new(20.0)))
        .await
        .unwrap();

    let json = r#"{
        "B52:grp": {
            "+atomic": true,
            "a[0].x": { "+channel": "B52:r0.VAL", "+type": "plain" },
            "a[1].x": { "+channel": "B52:r1.VAL", "+type": "plain" }
        }
    }"#;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(json).expect("load");
    let def = provider
        .groups()
        .get("B52:grp")
        .cloned()
        .expect("registered");
    let ch = GroupChannel::new(db, def);

    // Descriptor: `a` is a StructureArray whose element carries `x`.
    let desc = ch.get_field().await.expect("get_field");
    let root = match &desc {
        FieldDesc::Structure { fields, .. } => fields,
        other => panic!("group descriptor must be a structure, got {other:?}"),
    };
    match root.iter().find(|(n, _)| n == "a").map(|(_, d)| d) {
        Some(FieldDesc::StructureArray { fields, .. }) => {
            assert_eq!(
                fields.len(),
                1,
                "homogeneous element schema must hold exactly the shared `x` field"
            );
            assert_eq!(fields[0].0, "x");
        }
        other => panic!("`a[N].x` must advertise a StructureArray descriptor, got {other:?}"),
    }

    // Value: `a` is a StructureArray with element [0].x=10, [1].x=20.
    let val = ch.get(&empty_request()).await.expect("get");
    match find_field(&val, "a") {
        Some(PvField::StructureArray(items)) => {
            assert_eq!(items.len(), 2, "two indexed members → two array elements");
            assert_eq!(
                extract_double(items[0].as_ref().expect("a[0]"), "x"),
                Some(10.0)
            );
            assert_eq!(
                extract_double(items[1].as_ref().expect("a[1]"), "x"),
                Some(20.0)
            );
        }
        other => panic!("`a[N].x` value must be a StructureArray, got {other:?}"),
    }
}

/// Heterogeneous case: `a[0].x` and `a[1].y` differ in their leaf
/// field. The element descriptor is the union of both leaves (`x`,
/// `y`); each value element carries only its own field, and the wire
/// encoder fills the absent sibling with a default
/// (encode.rs `encode_pv_field`). This confirms the StructureArray
/// builder accumulates a shared element schema across indices.
#[tokio::test]
async fn br52_indexed_member_builds_heterogeneous_structure_array() {
    use epics_pva_rs::pvdata::FieldDesc;

    let db = Arc::new(PvDatabase::new());
    db.add_record("B52:hx", Box::new(AiRecord::new(3.0)))
        .await
        .unwrap();
    db.add_record("B52:hy", Box::new(AiRecord::new(4.0)))
        .await
        .unwrap();

    let json = r#"{
        "B52:hgrp": {
            "+atomic": true,
            "a[0].x": { "+channel": "B52:hx.VAL", "+type": "plain" },
            "a[1].y": { "+channel": "B52:hy.VAL", "+type": "plain" }
        }
    }"#;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(json).expect("load");
    let def = provider
        .groups()
        .get("B52:hgrp")
        .cloned()
        .expect("registered");
    let ch = GroupChannel::new(db, def);

    // Descriptor element schema is the union {x, y}.
    let desc = ch.get_field().await.expect("get_field");
    let root = match &desc {
        FieldDesc::Structure { fields, .. } => fields,
        other => panic!("group descriptor must be a structure, got {other:?}"),
    };
    match root.iter().find(|(n, _)| n == "a").map(|(_, d)| d) {
        Some(FieldDesc::StructureArray { fields, .. }) => {
            let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
            assert!(
                names.contains(&"x") && names.contains(&"y"),
                "heterogeneous element schema must union both leaves, got {names:?}"
            );
        }
        other => panic!("`a[N]` must advertise a StructureArray descriptor, got {other:?}"),
    }

    // Each value element carries only its own configured leaf.
    let val = ch.get(&empty_request()).await.expect("get");
    match find_field(&val, "a") {
        Some(PvField::StructureArray(items)) => {
            assert_eq!(items.len(), 2);
            assert_eq!(
                extract_double(items[0].as_ref().expect("a[0]"), "x"),
                Some(3.0)
            );
            assert_eq!(
                extract_double(items[1].as_ref().expect("a[1]"), "y"),
                Some(4.0)
            );
        }
        other => panic!("`a[N]` value must be a StructureArray, got {other:?}"),
    }
}

/// A client STOP on a group monitor disables every
/// member `DbSubscription` it opened — pvxs `groupsource.cpp` `onStart`
/// toggles each member `dbChannel` via `db_event_disable`/`enable`. While
/// stopped, a post on any member is not delivered; RESUME restores
/// delivery on the same handles.
#[tokio::test]
async fn group_monitor_stop_disables_member_subscriptions() {
    use epics_base_rs::server::recgbl::EventMask;
    use epics_bridge_rs::qsrv::group::GroupMonitor;
    use epics_bridge_rs::qsrv::provider::PvaMonitor;
    use std::time::Duration;

    let db = make_db().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(GROUP_JSON).expect("load");
    let def = provider
        .groups()
        .get("TEST:grp")
        .cloned()
        .expect("grp registered");

    let mut mon = GroupMonitor::new(db.clone(), def);
    mon.start().await.expect("start");
    let handles = mon.activation_handles();
    assert!(
        !handles.is_empty(),
        "a started group monitor must expose its member subscription gate handles"
    );

    async fn post(db: &Arc<PvDatabase>) {
        let rec = db.get_record("TEST:level").await.expect("rec exists");
        rec.read().await.notify_field("VAL", EventMask::VALUE);
    }

    // Active (post-START): a member post wakes the group poll.
    post(&db).await;
    tokio::time::timeout(Duration::from_secs(2), mon.poll())
        .await
        .expect("event must be delivered while the group monitor is active")
        .expect("snapshot");

    // STOP: disable every member subscription. A member post is NOT
    // delivered.
    for h in &handles {
        h.set_active(false).await;
    }
    post(&db).await;
    let stopped = tokio::time::timeout(Duration::from_millis(300), mon.poll()).await;
    assert!(
        stopped.is_err(),
        "no event may be delivered while the group monitor is stopped"
    );

    // RESUME: re-enable. A member post is delivered again.
    for h in &handles {
        h.set_active(true).await;
    }
    post(&db).await;
    tokio::time::timeout(Duration::from_secs(2), mon.poll())
        .await
        .expect("event must be delivered after the group monitor resumes")
        .expect("snapshot");

    mon.stop().await;
}

/// A trapped group PUT fires one asTrapWrite Before/After pair per
/// backing member write — pvxs builds one `SecurityLogger` per group
/// field (ioc/groupsource.cpp:594-602), not one for the outer group PV.
#[tokio::test]
async fn trapped_group_put_emits_per_member_astrapwrite() {
    use std::sync::Mutex;

    use epics_base_rs::server::access_security::{
        TrapWriteMessage, TrapWriteOp, register_trap_write_listener,
    };
    use epics_bridge_rs::qsrv::{AccessContext, AccessControl, ClientCreds, WriteGrant};

    /// Grant every write with the `TRAPWRITE` flag set.
    struct TrapAll;
    impl AccessControl for TrapAll {
        fn write_grant(&self, _channel: &str, _creds: &ClientCreds) -> WriteGrant {
            WriteGrant {
                allowed: true,
                rule_was_trap: true,
            }
        }
    }

    let db = make_db().await;
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(GROUP_JSON).expect("load");
    let def = provider
        .groups()
        .get("TEST:grp")
        .cloned()
        .expect("grp registered");
    let ch = GroupChannel::new(db.clone(), def).with_access(AccessContext::with_identity(
        Arc::new(TrapAll),
        "op".into(),
        "h1".into(),
    ));

    // Capture (op, pv, status) for both member channels AND the group PV
    // name, so a stray emission on the outer group PV would be observed.
    let sink: Arc<Mutex<Vec<(TrapWriteOp, String, Option<String>)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let cap = sink.clone();
    let _handle = register_trap_write_listener(Arc::new(move |msg: &TrapWriteMessage<'_>| {
        if matches!(
            msg.pv_name,
            "TEST:level.VAL" | "TEST:count.VAL" | "TEST:grp"
        ) {
            cap.lock().unwrap().push((
                msg.op,
                msg.pv_name.to_string(),
                msg.status.map(|s| s.to_string()),
            ));
        }
    }));

    let mut put = PvStructure::new("epics:nt/NTGroup:1.0");
    put.fields
        .push(("level".into(), PvField::Scalar(ScalarValue::Double(42.0))));
    put.fields
        .push(("count".into(), PvField::Scalar(ScalarValue::Long(13))));
    ch.put(&put).await.expect("trapped group put");

    let events = sink.lock().unwrap().clone();
    assert_eq!(
        events.len(),
        4,
        "two members × (Before + After), none for the group PV: {events:?}"
    );
    assert!(
        events.iter().all(|(_, pv, _)| pv != "TEST:grp"),
        "the outer group PV must not emit asTrapWrite — only backing members do: {events:?}"
    );
    for member in ["TEST:level.VAL", "TEST:count.VAL"] {
        let pairs: Vec<_> = events.iter().filter(|(_, pv, _)| pv == member).collect();
        assert_eq!(
            pairs.len(),
            2,
            "{member}: one Before + one After: {pairs:?}"
        );
        assert_eq!(pairs[0].0, TrapWriteOp::BeforeWrite);
        assert_eq!(pairs[0].2, None);
        assert_eq!(pairs[1].0, TrapWriteOp::AfterWrite);
        assert_eq!(pairs[1].2, Some("ok".to_string()));
    }
}
