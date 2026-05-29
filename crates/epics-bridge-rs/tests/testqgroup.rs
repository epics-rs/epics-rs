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

use epics_bridge_rs::qsrv::{BridgeProvider, Channel, group::GroupChannel};
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
    assert!(
        msg.contains("TEST:count.VAL"),
        "error must name the denied member channel; got: {msg}"
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
