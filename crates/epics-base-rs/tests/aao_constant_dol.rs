//! R15-82: a CONSTANT closed-loop DOL is loaded into the aao's buffer at init.
//!
//! C `aaoRecord.c::init_record` pass 1 ends with `return fetchValue(prec, 1);`
//! (line 147), and `fetchValue` (351-377) has exactly one arm for a constant:
//!
//! ```c
//!     if (init && isConst) {
//!         status = dbLoadLinkArray(&prec->dol, prec->ftvl, prec->bptr, &nReq);
//!     } else if (!init && !isConst) {
//!         status = dbGetLink(&prec->dol, prec->ftvl, prec->bptr, 0, &nReq);
//!     } else return 0;
//!     if (!status) { prec->nord = nReq; prec->udf = FALSE; }
//! ```
//!
//! So the constant is applied ONCE at init and never re-fetched, and the port —
//! which had only the per-cycle `!init && !isConst` arm — dropped it entirely:
//! a `field(DOL,"[1,2,3]")` closed-loop aao wrote a zero-filled buffer to OUT
//! forever and stayed UDF.
//!
//! Boundaries: constant array DOL vs constant scalar DOL vs real link DOL; and
//! closed_loop vs supervisory (C returns before the load in supervisory).

use std::collections::HashSet;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(waveform, "TGT") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
}
record(aao, "CL:ARRAY") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
    field(OMSL, "closed_loop")
    field(DOL, "[1, 2, 3]")
    field(OUT, "TGT")
}
record(aao, "CL:SCALAR") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
    field(OMSL, "closed_loop")
    field(DOL, "7.5")
}
record(aao, "SUP:CONST") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
    field(OMSL, "supervisory")
    field(DOL, "[1, 2, 3]")
}
"#;

async fn build() -> std::sync::Arc<epics_base_rs::server::database::PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// The constant array is in VAL before any process, NORD = 3, UDF clear.
#[epics_macros_rs::epics_test]
async fn constant_array_dol_is_loaded_at_init() {
    let db = build().await;

    assert_eq!(
        db.get_pv("CL:ARRAY").unwrap(),
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]),
        "C `dbLoadLinkArray(&prec->dol, ...)` at init_record pass 1"
    );
    assert_eq!(db.get_pv("CL:ARRAY.NORD").unwrap().to_f64(), Some(3.0));
    assert!(
        db.get_record("CL:ARRAY").unwrap().read().common.udf == 0,
        "C `fetchValue`: on a successful load, nord = nReq and udf = FALSE"
    );
}

/// A constant SCALAR DOL is a one-element load (`dbPutConvertJSON` on "7.5"
/// yields nRequest = 1), landing in element 0 — the same rule as R15-79.
#[epics_macros_rs::epics_test]
async fn constant_scalar_dol_is_one_element() {
    let db = build().await;

    assert_eq!(
        db.get_pv("CL:SCALAR").unwrap(),
        EpicsValue::DoubleArray(vec![7.5])
    );
    assert_eq!(db.get_pv("CL:SCALAR.NORD").unwrap().to_f64(), Some(1.0));
}

/// Supervisory mode does not source VAL from DOL at all — C `fetchValue`
/// returns on its first line (`if (prec->omsl != menuOmslclosed_loop) return 0`).
#[epics_macros_rs::epics_test]
async fn supervisory_mode_ignores_the_constant_dol() {
    let db = build().await;

    assert_eq!(
        db.get_pv("SUP:CONST").unwrap(),
        EpicsValue::DoubleArray(vec![]),
        "supervisory: DOL is not a value source"
    );
    assert!(
        db.get_record("SUP:CONST").unwrap().read().common.udf != 0,
        "nothing was loaded, so the record is still UNDEFINED"
    );
}

/// The loaded constant reaches the OUT target on process, and the process
/// cycle does not re-fetch the constant over a client caput to VAL.
#[epics_macros_rs::epics_test]
async fn loaded_constant_writes_out_and_is_not_re_fetched() {
    let db = build().await;

    let mut visited = HashSet::new();
    db.process_record_with_links("CL:ARRAY", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("TGT").unwrap(),
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]),
        "the init-loaded constant is what gets written out"
    );

    db.put_pv("CL:ARRAY", EpicsValue::DoubleArray(vec![9.0, 9.0]))
        .await
        .unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links("CL:ARRAY", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("CL:ARRAY").unwrap(),
        EpicsValue::DoubleArray(vec![9.0, 9.0]),
        "C `!init && !isConst`: a constant DOL is NOT re-fetched per cycle"
    );
    assert_eq!(
        db.get_pv("TGT").unwrap(),
        EpicsValue::DoubleArray(vec![9.0, 9.0])
    );
}
