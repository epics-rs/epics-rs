//! `caput REC.VAL ""` into a numeric SCALAR field stores 0; into an ARRAY field
//! it is refused. The port refused both.
//!
//! `dbPut` picks the conversion table on shape, not on direction
//! (`dbAccess.c:1345`): `nRequest > 1` or a `special(SPC_DBADDR)` field takes
//! `dbPutConvertRoutine` (`:1357`, the `putString*` array row), everything else
//! takes `dbFastPutConvertRoutine` (`:1370`/`:1381`, the `cvt_st_*` scalar row).
//! The two rows disagree about the empty string, and only about the empty
//! string:
//!
//! ```c
//! /* dbFastLinkConv.c:147, cvt_st_l — the SCALAR row */
//! if (*from == 0) {
//!      *to = 0;
//!      return 0;
//! }
//! return epicsParseInt32(from, to, dbConvertBase, &end);
//!
//! /* dbConvert.c:1017, putStringLong — the ARRAY row, no such arm */
//! long status = epicsParseInt32(psrc, pdst++, dbConvertBase, &end);
//! if (status)
//!     return status;
//! ```
//!
//! Every `cvt_st_*` carries the arm — `cvt_st_c` at `dbFastLinkConv.c:91`,
//! `cvt_st_l` at `:147`, `cvt_st_d` at `:233` — and the DBR_STRING row of
//! `dbFastPutConvertRoutine` (`:1698`) is built from exactly those functions.
//!
//! The mis-filing this replaces came from a doc comment in `c_parse.rs` that
//! named `putStringDouble` — the ARRAY row — as "the put row", which made the
//! refusal look like parity. The real asymmetry runs the other way: three of
//! the four rows accept the empty string, and the array PUT row is the only one
//! that does not.
//!
//! ```text
//!            scalar                     array
//!   get      cvt_st_*     empty -> 0    getStringDouble  (dbConvert.c:392)  empty -> 0
//!   put      cvt_st_*     empty -> 0    putStringDouble  (dbConvert.c:1130) REFUSED
//! ```
//!
//! A whitespace-only string is not empty by C's `*from == 0` test, so it falls
//! through to `epicsParse*`, whose `strtol` finds no digits and returns
//! `S_stdlib_noConversion`. That boundary is asserted here because the scalar
//! common-field caller used to `.trim()` before handing the string over, which
//! would have turned `"   "` into the accepted empty case.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;
use std::sync::Arc;

const DB: &str = r#"
record(longout, "LO") { field(VAL, "7") }
record(ao,      "AO") { field(VAL, "2.5") }
record(ai,      "AI") { field(PREC, "3") field(HOPR, "10") }
record(waveform,"WF") { field(FTVL, "DOUBLE") field(NELM, "4") }
"#;

type Db = Arc<PvDatabase>;

async fn build() -> Db {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// `dbPutField` with a DBR_STRING, which is what `caput` sends (`caput.c:528`).
async fn caput(db: &Db, rec: &str, field: &str, text: &str) -> epics_base_rs::error::CaResult<()> {
    db.put_record_field_from_ca_no_notify(rec, field, EpicsValue::String(text.into()))
        .await
        .map(|_| ())
}

fn read(db: &Db, rec: &str, field: &str) -> f64 {
    db.get_record(rec)
        .unwrap()
        .read()
        .record
        .get_field(field)
        .and_then(|v| v.to_f64())
        .unwrap_or_else(|| panic!("{rec}.{field}"))
}

/// The headline: a DBF_LONG scalar VAL, C's `cvt_st_l`.
#[epics_macros_rs::epics_test]
async fn an_empty_put_into_a_long_val_stores_zero() {
    let db = build().await;
    assert_eq!(read(&db, "LO", "VAL"), 7.0);
    caput(&db, "LO", "VAL", "")
        .await
        .expect("dbFastLinkConv.c:147 returns 0 for the empty string");
    assert_eq!(read(&db, "LO", "VAL"), 0.0);
}

/// A DBF_DOUBLE scalar VAL, C's `cvt_st_d`.
#[epics_macros_rs::epics_test]
async fn an_empty_put_into_a_double_val_stores_zero() {
    let db = build().await;
    assert_eq!(read(&db, "AO", "VAL"), 2.5);
    caput(&db, "AO", "VAL", "")
        .await
        .expect("dbFastLinkConv.c:233 returns 0 for the empty string");
    assert_eq!(read(&db, "AO", "VAL"), 0.0);
}

/// A record data field that is not VAL — same row, different caller
/// (`coerce_put_value`).
#[epics_macros_rs::epics_test]
async fn an_empty_put_into_a_non_val_double_field_stores_zero() {
    let db = build().await;
    assert_eq!(read(&db, "AI", "HOPR"), 10.0);
    caput(&db, "AI", "HOPR", "").await.expect("cvt_st_d");
    assert_eq!(read(&db, "AI", "HOPR"), 0.0);
}

/// A numeric `dbCommon` field, which reaches the row through the OTHER scalar
/// caller (`coerce_common_field`). This is the one that used to `.trim()`.
#[epics_macros_rs::epics_test]
async fn an_empty_put_into_a_common_field_stores_zero() {
    let db = build().await;
    assert_eq!(read(&db, "AI", "PREC"), 3.0);
    caput(&db, "AI", "PREC", "").await.expect("cvt_st_s");
    assert_eq!(read(&db, "AI", "PREC"), 0.0);
}

/// The boundary on one side: whitespace is not the empty string to C.
#[epics_macros_rs::epics_test]
async fn a_whitespace_only_put_is_still_refused() {
    let db = build().await;
    for (rec, field) in [("LO", "VAL"), ("AO", "VAL"), ("AI", "PREC")] {
        assert!(
            caput(&db, rec, field, "   ").await.is_err(),
            "{rec}.{field}: `*from == 0` is false for a space, so epicsParse* runs and refuses"
        );
    }
    assert_eq!(
        read(&db, "LO", "VAL"),
        7.0,
        "the refused put changed nothing"
    );
    assert_eq!(read(&db, "AI", "PREC"), 3.0);
}

/// The boundary on the other side: a waveform VAL is `special(SPC_DBADDR)`, so
/// `dbPut` takes the array row even for this one-element string put, and
/// `putStringDouble` (`dbConvert.c:1130`) has no empty-string arm.
#[epics_macros_rs::epics_test]
async fn an_empty_put_into_a_waveform_is_still_refused() {
    let db = build().await;
    assert!(
        caput(&db, "WF", "VAL", "").await.is_err(),
        "the array row refuses what the scalar row accepts"
    );
}
