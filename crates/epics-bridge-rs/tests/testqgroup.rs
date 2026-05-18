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

// BR-R30: members need `+putorder` to be writable. pvxs's
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

/// BR-R17: a per-member ACF denial fails the group PUT even when the
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
    let err = result.expect_err("group PUT must be rejected per BR-R17");
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

/// BR-R25: a group root meta member `""` flattens its meta sub-fields
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
