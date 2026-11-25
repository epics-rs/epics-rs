//! `aiRecord.c:121-131` returns from `init_record` right after a dset
//! `init_record` runs, so only a dset without one (devTimestampAI,
//! `devTimestamp.c:44`) reaches `mlst = alst = lalm = val`. devAiSoft has one
//! (`devAiSoft.c:34`), which loads a constant INP into VAL and leaves the
//! trackers at 0, so the first monitor() posts VAL.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

async fn ioc(db_text: &str) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

fn trackers(db: &PvDatabase, name: &str) -> [EpicsValue; 3] {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    ["MLST", "ALST", "LALM"].map(|f| inst.record.get_field(f).unwrap())
}

#[epics_macros_rs::epics_test]
async fn soft_channel_ai_leaves_trackers_at_zero_and_posts_its_first_val() {
    let db = ioc(r#"record(ai, "SOFT") {
    field(INP, "5")
}"#)
    .await;

    assert_eq!(
        trackers(&db, "SOFT"),
        [const { EpicsValue::Double(0.0) }; 3]
    );
    let rec = db.get_record("SOFT").unwrap();
    let mut inst = rec.write();
    assert_eq!(inst.record.get_field("VAL"), Some(EpicsValue::Double(5.0)));
    assert_eq!(inst.check_deadband_ext(), (true, true));
}

#[epics_macros_rs::epics_test]
async fn ai_on_a_dset_without_init_record_seeds_trackers_from_val() {
    let db = ioc(r#"record(ai, "TS") {
    field(DTYP, "Soft Timestamp")
    field(VAL, "5")
}"#)
    .await;

    assert_eq!(trackers(&db, "TS"), [const { EpicsValue::Double(5.0) }; 3]);
    let rec = db.get_record("TS").unwrap();
    let mut inst = rec.write();
    assert_eq!(inst.check_deadband_ext(), (false, false));
}

fn oraw(db: &PvDatabase, name: &str) -> Option<EpicsValue> {
    db.get_record(name).unwrap().read().record.get_field("ORAW")
}

/// `prec->oraw = prec->rval` is on the same branch (`aiRecord.c:132`);
/// softIoc reads ORAW 0 for Soft Channel and 7 for Soft Timestamp.
#[epics_macros_rs::epics_test]
async fn oraw_follows_rval_only_without_dset_init_record() {
    let db = ioc(r#"record(ai, "SOFT") {
    field(DTYP, "Soft Channel")
    field(RVAL, "7")
}
record(ai, "TS") {
    field(DTYP, "Soft Timestamp")
    field(RVAL, "7")
}"#)
    .await;

    assert_eq!(oraw(&db, "SOFT"), Some(EpicsValue::Long(0)));
    assert_eq!(oraw(&db, "TS"), Some(EpicsValue::Long(7)));
}
