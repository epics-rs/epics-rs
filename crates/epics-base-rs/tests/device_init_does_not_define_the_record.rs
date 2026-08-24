//! Attaching device support does not define a record; producing a value does.
//!
//! In C, whether `init_record` defines the record is a property of the
//! individual dset. `devTimestamp.c` declares NO `init_record` in either dset
//! (`:44` `devTimestampAI`, `:69` `devTimestampSI` — slot 4 is NULL), and its
//! only two `prec->udf = FALSE` sites are `:40` and `:65`, both inside
//! `read_ai`/`read_stringin`. `iocInit.c::doInitRecord0` (`:508-533`) never
//! writes `udf`; it only reads it, to derive the initial severity
//! (`:524-525`).
//!
//! The port cleared UDF in the framework, at the device-wiring boundary, under
//! a `Record::val().is_some()` gate that is vacuous — `val()` is
//! `get_field(primary_field())` and returns `Some` for any record that HAS a
//! VAL field, populated or not. So every record with a non-soft DTYP came out
//! of the build defined, and reported `UDF = 0` while still carrying the
//! `UDF_ALARM` its own init had raised.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;

async fn ioc(db_text: &str) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

/// BOUNDARY: a hardware DTYP whose device support has produced nothing. The
/// record is undefined, and its alarm state agrees with that.
///
/// This is the brief's softIoc 7.0.10 probe — `caget TS.UDF TS.SEVR TS.STAT`
/// straight after `iocInit`, on a Passive record with `PINI` NO, reads
/// `1 INVALID UDF`. The port read `0 INVALID UDF`: defined and undefined at
/// once.
#[epics_macros_rs::epics_test]
async fn device_wiring_alone_leaves_the_record_undefined() {
    let db = ioc(r#"record(stringin, "TS") {
    field(DTYP, "Soft Timestamp")
    field(INP, "@%Y-%m-%d %H:%M:%S")
}"#)
    .await;

    let rec = db.get_record("TS").unwrap();
    let inst = rec.read();
    assert_eq!(inst.common.udf, 1, "no dset read has run yet");
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "doInitRecord0 derives the severity FROM udf"
    );
    assert_eq!(
        inst.common.stat,
        alarm_status::UDF_ALARM,
        "and the record's alarm state must agree with its UDF"
    );
}

/// BOUNDARY: the same record after one cycle. `read_stringin` produced a
/// value, so now it is defined — the clear belongs to the read, not the wiring.
#[epics_macros_rs::epics_test]
async fn a_device_read_defines_the_record() {
    let db = ioc(r#"record(stringin, "TS") {
    field(DTYP, "Soft Timestamp")
    field(INP, "@%Y-%m-%d %H:%M:%S")
}"#)
    .await;

    let mut visited = std::collections::HashSet::new();
    db.process_record_with_links("TS", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("TS").unwrap();
    let inst = rec.read();
    assert_eq!(inst.common.udf, 0, "devTimestamp.c:65 read_stringin");
    assert_eq!(inst.common.sevr, AlarmSeverity::NoAlarm);
}

/// BOUNDARY: the framework clear that IS C's. A constant INP on a soft record
/// is `recGblInitConstantLink` (`devAiSoft.c`: `if (recGblInitConstantLink(…))
/// prec->udf = FALSE;`), and the port owns it in `seed_constant_links` — a
/// per-record decision made because a link actually loaded, not because a
/// device was attached. Removing the wiring-time clear must not touch it.
#[epics_macros_rs::epics_test]
async fn a_constant_input_link_still_defines_the_record() {
    let db = ioc(r#"record(ai, "C") { field(INP, "7") }"#).await;

    let rec = db.get_record("C").unwrap();
    let inst = rec.read();
    assert_eq!(inst.common.udf, 0, "the constant loaded into VAL");
    assert_eq!(
        inst.record.get_field("VAL").and_then(|v| v.to_f64()),
        Some(7.0)
    );
}
