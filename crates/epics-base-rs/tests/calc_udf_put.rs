//! acalcout/scalcout `UDF` is the framework's raw `dbCommon.udf` byte, owned by
//! `check_alarms` (C's post-calc tail) and NOT re-derived from VAL each cycle.
//!
//! `UDF` is `field(UDF,DBF_UCHAR){ pp(TRUE) }` (`dbCommon.dbd`), so a put stores
//! the raw `epicsUInt8` AND triggers a process. Because these records set
//! `clears_udf() == false`, the framework never collapses the byte to 0/1: C
//! `aCalcoutRecord.c:305-307` / `sCalcoutRecord.c:356-366` clear `udf` only on a
//! successful calc (`else pcalc->udf = FALSE`) and leave it otherwise. The five
//! invariant boundaries below are the oracle's ground truth (C vs the port):
//!
//!   1. fresh (no calc, no put)      → UDF 1   (C `iocInit` udf=TRUE)
//!   2. caput UDF 0                  → 0
//!   3. caput UDF 255               → 255  (served signed as -1 on the CA wire)
//!   4. caput UDF -1                → 255  (negative into the unsigned char)
//!   5. caput UDF 1 on a good CALC  → 0    (the successful calc clears it)
//!
//! Byte fidelity (#3/#4) holds because the empty-CALC record's `pp(TRUE)`
//! re-process FAILS the calc, so `check_alarms` leaves the put byte untouched —
//! exactly C, whose `afterCalc` clears `udf` only in the success arm. The
//! wave-3 boolean shadow-cell collapsed every nonzero put to 1 (oracle: `caput
//! UDF -1/255` → C=-1, port=1); this is verified against the C source, not the
//! running oracle, per the panel's constraints.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::ProcessCompletion;
use epics_base_rs::types::EpicsValue;

// "A"/"S": default (empty CALC → fails every cycle, so a put byte survives).
// "AC"/"SC": a finite CALC, so a triggered process clears UDF.
const DB: &str = r#"
record(acalcout, "A") {}
record(scalcout, "S") {}
record(acalcout, "AC") { field(CALC, "1+1") }
record(scalcout, "SC") { field(CALC, "1+1") }
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// Read the record's raw `dbCommon.udf` byte (what CA serves, signed).
async fn udf_byte(db: &PvDatabase, rec: &str) -> u8 {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    g.common.udf
}

/// A `caput <rec>.UDF <text>` — a string, as `caput` sends; UDF's `pp(TRUE)`
/// drives a process, so await the returned notify before reading back.
async fn caput_udf(db: &PvDatabase, rec: &str, text: &str) {
    if let ProcessCompletion::Async(rx) = db
        .put_record_field_from_ca(rec, "UDF", EpicsValue::String(text.into()))
        .await
        .unwrap()
    {
        let _ = rx.await;
    }
}

#[epics_macros_rs::epics_test]
async fn acalcout_udf_byte_fidelity_across_the_five_boundaries() {
    let db = build().await;

    // #1 fresh empty-CALC record is undefined (C `iocInit` udf=TRUE).
    assert_eq!(udf_byte(&db, "A").await, 1, "#1 fresh A.UDF");

    // #2 caput UDF 0 stands (empty calc fails → the put 0 is left).
    caput_udf(&db, "A", "0").await;
    assert_eq!(udf_byte(&db, "A").await, 0, "#2 caput A.UDF 0");

    // #3 caput UDF 255 keeps the raw byte (byte fidelity — was collapsed to 1).
    caput_udf(&db, "A", "255").await;
    assert_eq!(udf_byte(&db, "A").await, 255, "#3 caput A.UDF 255");

    // #4 caput UDF -1 stores 255 in the signed-served DBF_UCHAR.
    caput_udf(&db, "A", "-1").await;
    assert_eq!(udf_byte(&db, "A").await, 255, "#4 caput A.UDF -1");

    // #5 caput UDF 1 on a finite-CALC record: the pp(TRUE) process runs the
    // calc (1+1=2), which succeeds and clears UDF — C `else pcalc->udf = FALSE`.
    caput_udf(&db, "AC", "1").await;
    assert_eq!(
        udf_byte(&db, "AC").await,
        0,
        "#5 successful calc clears A.UDF"
    );
}

#[epics_macros_rs::epics_test]
async fn scalcout_udf_byte_fidelity_across_the_five_boundaries() {
    let db = build().await;

    assert_eq!(udf_byte(&db, "S").await, 1, "#1 fresh S.UDF");

    caput_udf(&db, "S", "0").await;
    assert_eq!(udf_byte(&db, "S").await, 0, "#2 caput S.UDF 0");

    caput_udf(&db, "S", "255").await;
    assert_eq!(udf_byte(&db, "S").await, 255, "#3 caput S.UDF 255");

    caput_udf(&db, "S", "-1").await;
    assert_eq!(udf_byte(&db, "S").await, 255, "#4 caput S.UDF -1");

    caput_udf(&db, "SC", "1").await;
    assert_eq!(
        udf_byte(&db, "SC").await,
        0,
        "#5 successful calc clears S.UDF"
    );
}
