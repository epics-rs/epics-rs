//! UI-64 (epics-base#876): only `dbPutField` changes DBF link fields.
//!
//! C `dbPut` refuses a put to an INLINK/OUTLINK/FWDLINK target with
//! `S_db_badDbrtype` (`field_type > DBF_DEVICE`, `dbAccess.c:1340-1347`);
//! `dbPutField` routes link fields through `dbPutFieldLink` instead
//! (`dbAccess.c:1261-1262`). The pre-fix port let every write funnel fall
//! through to `put_common_field`'s INP/OUT/FLNK re-parse arms, so a record's
//! DB OUT link could silently rewire another record's link field on every
//! process cycle.
//!
//! One boundary per put entry: the `dbPut` analogues (`put_pv`,
//! `put_pv_and_post`, the OUT-link route) refuse; the `dbPutField` analogues
//! (`put_record_field_from_ca_no_notify`, the autosave restore's
//! `put_pv_no_process`) still rewire.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use epics_base_rs::error::CaError;
use epics_base_rs::server::autosave::save_file::{SaveEntry, write_save_file};
use epics_base_rs::server::autosave::save_set::{RestoreMode, restore_from_entries_with_mode};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

async fn build(db_text: &str) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

fn link_text(db: &PvDatabase, name: &str) -> String {
    match db.get_pv(name).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        EpicsValue::CharArray(b) => String::from_utf8_lossy(&b)
            .trim_end_matches('\0')
            .to_string(),
        other => panic!("{name} is not string-shaped: {other:?}"),
    }
}

/// The cited defect: a stringout whose OUT names another record's INP. C's
/// `dbPutLink` → `dbDbPutValue` → `dbPut` refuses (`S_db_badDbrtype`) and
/// `setLinkAlarm` puts the WRITER in LINK/INVALID; the target's INP text
/// never changes. Pre-fix the port rewired `TGT.INP` to the stringout's VAL.
#[epics_macros_rs::epics_test]
async fn db_out_link_write_to_a_link_field_is_refused() {
    let db = build(
        r#"
        record(ai, "TGT") { }
        record(stringout, "SRC") { field(VAL, "REWIRED.VAL") field(OUT, "TGT.INP") }
        "#,
    )
    .await;
    let mut v = HashSet::new();
    db.process_record_with_links("SRC", &mut v, 0)
        .await
        .unwrap();

    assert_eq!(
        link_text(&db, "TGT.INP"),
        "",
        "the DB OUT link must not rewire TGT.INP (C dbPut refuses, dbAccess.c:1340)"
    );
    let rec = db.get_record("SRC").unwrap();
    let (stat, sevr) = {
        let inst = rec.read();
        (inst.common.stat, inst.common.sevr)
    };
    assert_eq!(
        (stat, sevr),
        (alarm_status::LINK_ALARM, AlarmSeverity::Invalid),
        "the refused put lands on the writer as LINK/INVALID (C setLinkAlarm)"
    );
}

/// `put_pv` is the public `dbPut` analogue: an INLINK and a FWDLINK target
/// both refuse with the `S_db_badDbrtype` analogue, text unchanged.
#[epics_macros_rs::epics_test]
async fn put_pv_refuses_link_fields_with_bad_dbrtype() {
    let db = build(r#"record(ai, "R1") { }"#).await;
    for field in ["INP", "FLNK"] {
        let err = db
            .put_pv(
                &format!("R1.{field}"),
                EpicsValue::String("OTHER.VAL".into()),
            )
            .await
            .expect_err("put_pv is the dbPut analogue and must refuse a link field");
        assert!(
            matches!(err, CaError::BadDbrType(_)),
            "expected BadDbrType for {field}, got {err:?}"
        );
        assert_eq!(link_text(&db, &format!("R1.{field}")), "");
    }
}

/// `put_pv_and_post` is another `dbPut` body (value + monitor post) and
/// takes the same refusal.
#[epics_macros_rs::epics_test]
async fn put_pv_and_post_refuses_a_link_field() {
    let db = build(r#"record(ai, "R2") { }"#).await;
    let err = db
        .put_pv_and_post("R2.INP", EpicsValue::String("OTHER.VAL".into()))
        .await
        .expect_err("put_pv_and_post shares dbPut's link-field refusal");
    assert!(matches!(err, CaError::BadDbrType(_)), "got {err:?}");
    assert_eq!(link_text(&db, "R2.INP"), "");
}

/// The `dbPutField` analogue still rewires: this is `dbPutFieldLink`
/// (`dbAccess.c:1261`), the one sanctioned link-write path.
#[epics_macros_rs::epics_test]
async fn ca_route_still_rewires_a_link_field() {
    let db = build(
        r#"
        record(ai, "SRC3") { }
        record(ai, "R3") { }
        "#,
    )
    .await;
    db.put_record_field_from_ca_no_notify("R3", "INP", EpicsValue::String("SRC3.VAL".into()))
        .await
        .unwrap();
    assert_eq!(link_text(&db, "R3.INP"), "SRC3.VAL");
}

/// `put_pv_no_process` is the autosave-restore entry; its C analogue
/// (`reboot_restore` → `dbPutField`) permits link-field writes, so it must
/// keep permitting them.
#[epics_macros_rs::epics_test]
async fn put_pv_no_process_still_rewires_for_restore() {
    let db = build(
        r#"
        record(ai, "SRC4") { }
        record(ai, "R4") { }
        "#,
    )
    .await;
    db.put_pv_no_process("R4.INP", EpicsValue::String("SRC4.VAL".into()))
        .await
        .unwrap();
    assert_eq!(link_text(&db, "R4.INP"), "SRC4.VAL");
}

/// An autosave restore in `RestoreMode::Process` must still restore a saved
/// link field: C's restore writes via `dbPutField`, and `dbPutField` on a
/// link field is the no-process `dbPutFieldLink` write — not the `dbPut`
/// that now refuses.
#[epics_macros_rs::epics_test]
async fn autosave_process_mode_restores_a_link_field() {
    let db = build(
        r#"
        record(ai, "SRC5") { }
        record(ai, "R5") { }
        "#,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("links.sav");
    write_save_file(
        &path,
        &[SaveEntry {
            pv_name: "R5.INP".into(),
            value: "SRC5.VAL".into(),
            connected: true,
        }],
    )
    .await
    .unwrap();

    let result = restore_from_entries_with_mode(&db, &path, RestoreMode::Process)
        .await
        .unwrap();
    assert_eq!(
        result.failed_puts.len(),
        0,
        "link-field restore must not fail: {:?}",
        result.failed_puts
    );
    assert_eq!(link_text(&db, "R5.INP"), "SRC5.VAL");
}
