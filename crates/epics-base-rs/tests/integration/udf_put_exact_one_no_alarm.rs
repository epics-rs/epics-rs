//! Cause 1 (Family B): a direct `caput .UDF 255` (or `-1`) must NOT raise
//! UDF_ALARM on `bo`/`stringout`.
//!
//! `UDF` is `field(UDF,DBF_UCHAR){ pp(TRUE) }` (dbCommon.dbd:265-271), so a put
//! PROCESSES the record. On `bo`/`stringout` the record does not re-derive
//! `udf` from VAL (`clears_udf() == false`), so the put byte survives to
//! `checkAlarms`. There C tests `if (prec->udf == TRUE)` — exact-one, `TRUE`
//! is `1` (`boRecord.c:366`, `stringoutRecord.c:146`). A byte of `255` (what
//! `caput 255` stores, and what `-1` stores in the signed-served `DBF_UCHAR`
//! field) satisfies `255 != 1`, so NO UDF_ALARM is raised; processing then
//! moves STAT from its `initial("UDF")` down to `NO_ALARM`.
//!
//! The port previously raised UDF_ALARM/INVALID here because the framework's
//! `rec_gbl_check_udf` tested `udf != 0` (truthy) for every record. Verified
//! against the C source (not the running oracle) per the panel's constraints.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::server::records::stringout::StringoutRecord;
use epics_base_rs::types::EpicsValue;

const NO_ALARM: u16 = 0;

async fn db_with(record: Box<dyn Record>) -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("REC", record).await.unwrap();
    db
}

async fn caput(db: &PvDatabase, field: &str, text: &str) {
    db.put_record_field_from_ca("REC", field, EpicsValue::String(text.into()))
        .await
        .unwrap();
}

/// `(SEVR, STAT, udf byte)` after the put.
async fn state(db: &PvDatabase) -> (AlarmSeverity, u16, u8) {
    let inst = db.get_record("REC").unwrap();
    let g = inst.read();
    (g.common.sevr, g.common.stat, g.common.udf)
}

#[epics_macros_rs::epics_test]
async fn bo_udf_put_type_max_raises_no_alarm() {
    let db = db_with(Box::new(BoRecord::new(0))).await;
    caput(&db, "UDF", "255").await;
    let (sevr, stat, udf) = state(&db).await;
    assert_eq!(udf, 255, "the UDF byte is stored verbatim");
    assert_eq!(
        (sevr, stat),
        (AlarmSeverity::NoAlarm, NO_ALARM),
        "bo udf==255 is not TRUE(==1), so C raises no UDF_ALARM"
    );
}

#[epics_macros_rs::epics_test]
async fn bo_udf_put_negative_raises_no_alarm() {
    let db = db_with(Box::new(BoRecord::new(0))).await;
    // `-1` into the signed-served DBF_UCHAR reaches the field as 255.
    caput(&db, "UDF", "-1").await;
    let (sevr, stat, udf) = state(&db).await;
    assert_eq!(udf, 255, "negative into unsigned char stores 255");
    assert_eq!((sevr, stat), (AlarmSeverity::NoAlarm, NO_ALARM));
}

#[epics_macros_rs::epics_test]
async fn stringout_udf_put_type_max_raises_no_alarm() {
    let db = db_with(Box::new(StringoutRecord::default())).await;
    caput(&db, "UDF", "255").await;
    let (sevr, stat, udf) = state(&db).await;
    assert_eq!(udf, 255, "the UDF byte is stored verbatim");
    assert_eq!(
        (sevr, stat),
        (AlarmSeverity::NoAlarm, NO_ALARM),
        "stringout udf==255 is not TRUE(==1), so C raises no UDF_ALARM"
    );
}

#[epics_macros_rs::epics_test]
async fn stringout_udf_put_negative_raises_no_alarm() {
    let db = db_with(Box::new(StringoutRecord::default())).await;
    caput(&db, "UDF", "-1").await;
    let (sevr, stat, udf) = state(&db).await;
    assert_eq!(udf, 255, "negative into unsigned char stores 255");
    assert_eq!((sevr, stat), (AlarmSeverity::NoAlarm, NO_ALARM));
}

/// The exact-one gate must NOT suppress the ordinary undefined alarm: a fresh
/// record with `udf == 1` still raises UDF_ALARM/INVALID (C `1 == TRUE`).
#[epics_macros_rs::epics_test]
async fn bo_udf_one_still_raises() {
    let db = db_with(Box::new(BoRecord::new(0))).await;
    // A UDF put of exactly 1 keeps the record undefined and must alarm.
    caput(&db, "UDF", "1").await;
    let (sevr, stat, udf) = state(&db).await;
    assert_eq!(udf, 1);
    assert_eq!(
        (sevr, stat),
        (AlarmSeverity::Invalid, 17),
        "udf==1 is TRUE, so UDF_ALARM(=17)/INVALID is raised"
    );
}
