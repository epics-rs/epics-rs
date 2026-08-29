//! R16-76: subArray is C's documented exception to the load-once constant-INP
//! rule (R15-78) — `devSASoft.c::read_sa` (92-123) re-reads its INP on EVERY
//! process:
//!
//! ```c
//!     rt.nRequest = prec->indx + prec->nelm;
//!     if (rt.nRequest > prec->malm) rt.nRequest = prec->malm;
//!     if (dbLinkIsConstant(&prec->inp)) {
//!         status = dbLoadLinkArray(&prec->inp, prec->ftvl, prec->bptr, &rt.nRequest);
//!         if (status == S_db_badField) {  /* INP was empty */
//!             rt.nRequest = prec->nord;
//!             status = 0;
//!         }
//!     }
//!     else { ... dbGetLink ... }
//!     if (!status) subset(prec, rt.nRequest);   /* shift by INDX, set NORD */
//! ```
//!
//! So a constant INP is re-loaded and re-sliced each cycle, and an EMPTY INP
//! still subsets — with `nRequest = NORD`, i.e. the record re-slices the array
//! a client wrote into VAL. In the port, `set_val` was the only slicing site and
//! ran only on link delivery, and a constant INP delivers nothing at process
//! (R15-78) — so INDX was inert on exactly the two configurations C documents.
//!
//! Every expectation below was taken from a built softIoc
//! (`/home/stevek/work/epics-base/bin/linux-x86_64/softIoc`) driving the same
//! record definitions over CA.

use std::collections::HashSet;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(waveform, "SRC") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
}
record(subArray, "SA:CONST") {
    field(FTVL, "DOUBLE")
    field(INP, "[1, 2, 3, 4]")
    field(MALM, "8")
    field(NELM, "3")
    field(INDX, "0")
}
record(subArray, "SA:EMPTY") {
    field(FTVL, "DOUBLE")
    field(MALM, "8")
    field(NELM, "3")
    field(INDX, "1")
}
record(subArray, "SA:LINK") {
    field(FTVL, "DOUBLE")
    field(INP, "SRC")
    field(MALM, "8")
    field(NELM, "3")
    field(INDX, "1")
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

async fn process(db: &epics_base_rs::server::database::PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

async fn nord(db: &epics_base_rs::server::database::PvDatabase, rec: &str) -> f64 {
    db.get_pv(&format!("{rec}.NORD")).unwrap().to_f64().unwrap()
}

async fn udf(db: &epics_base_rs::server::database::PvDatabase, rec: &str) -> bool {
    db.get_record(rec).unwrap().read().common.udf != 0
}

/// C: `dbLoadLinkArray` + `subset` in `devSASoft.c::init_record` (58-73) — the
/// INDX window of the constant is in VAL before any process. softIoc:
/// `SA:CONST` reads `1 2 3`, NORD=3, UDF=0.
#[epics_macros_rs::epics_test]
async fn constant_inp_slice_is_loaded_at_init() {
    let db = build().await;

    assert_eq!(
        db.get_pv("SA:CONST").unwrap(),
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0])
    );
    assert_eq!(nord(&db, "SA:CONST").await, 3.0);
    assert!(!udf(&db, "SA:CONST").await);
}

/// C: `read_sa` re-runs `dbLoadLinkArray` on the constant EVERY process, so a
/// client's caput to VAL is gone on the next cycle. softIoc: after
/// `caput -a SA:CONST 5 9 9 9 9 9` + process, VAL is `1 2 3` again.
///
/// This is the R15-78 rule INVERTED for this one record type, so the aai/wf
/// half of that rule is pinned alongside it (they must still NOT re-load).
#[epics_macros_rs::epics_test]
async fn constant_inp_is_re_loaded_and_re_sliced_at_process() {
    let db = build().await;

    db.put_pv(
        "SA:CONST",
        EpicsValue::DoubleArray(vec![9.0, 9.0, 9.0, 9.0, 9.0]),
    )
    .await
    .unwrap();
    process(&db, "SA:CONST").await;

    assert_eq!(
        db.get_pv("SA:CONST").unwrap(),
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]),
        "a constant INP subArray restores the INDX window of the constant every process"
    );
    assert_eq!(nord(&db, "SA:CONST").await, 3.0);
}

/// C: the INDX window moves with INDX because the constant is re-loaded whole
/// (`nRequest = min(INDX+NELM, MALM)`) before the shift. softIoc: `caput
/// SA:CONST.INDX 1` → VAL `2 3 4`, NORD=3.
#[epics_macros_rs::epics_test]
async fn constant_inp_indx_selects_the_window() {
    let db = build().await;

    db.put_pv("SA:CONST.INDX", EpicsValue::Long(1))
        .await
        .unwrap();
    process(&db, "SA:CONST").await;
    assert_eq!(
        db.get_pv("SA:CONST").unwrap(),
        EpicsValue::DoubleArray(vec![2.0, 3.0, 4.0])
    );
    assert_eq!(nord(&db, "SA:CONST").await, 3.0);

    // INDX past the constant's length: `ecount = nRequest - indx <= 0` → NORD=0,
    // and `readValue`'s `nord <= 0 → status = -1` makes the record UDF.
    db.put_pv("SA:CONST.INDX", EpicsValue::Long(6))
        .await
        .unwrap();
    process(&db, "SA:CONST").await;
    assert_eq!(nord(&db, "SA:CONST").await, 0.0);
    assert!(
        udf(&db, "SA:CONST").await,
        "empty slice → C `prec->udf = !!status` with status = -1"
    );
}

/// C: an EMPTY INP still subsets, with `nRequest = prec->nord` — the record
/// slices the array the client wrote into VAL. The shift is `memmove` in place,
/// so each further process eats INDX more elements. softIoc, INDX=1 NELM=3
/// MALM=8 after `caput -a SA:EMPTY 5 10 20 30 40 50`:
///
/// | cycle | VAL        | NORD |
/// |-------|------------|------|
/// | put   | `20 30 40` | 3    |
/// | +1    | `30 40`    | 2    |
/// | +2    | `40`       | 1    |
/// | +3    | (empty)    | 0 → UDF/INVALID |
#[epics_macros_rs::epics_test]
async fn empty_inp_re_slices_the_client_written_val() {
    let db = build().await;

    db.put_pv(
        "SA:EMPTY",
        EpicsValue::DoubleArray(vec![10.0, 20.0, 30.0, 40.0, 50.0]),
    )
    .await
    .unwrap();
    // The client's 5 elements survive the put: a subArray's buffer is MALM
    // (=8) wide, not NELM (=3) — C `cvt_dbaddr` `no_elements = prec->malm`.
    assert_eq!(nord(&db, "SA:EMPTY").await, 5.0);

    process(&db, "SA:EMPTY").await;
    assert_eq!(
        db.get_pv("SA:EMPTY").unwrap(),
        EpicsValue::DoubleArray(vec![20.0, 30.0, 40.0]),
        "empty INP: process slices VAL[INDX .. INDX+NELM]"
    );
    assert_eq!(nord(&db, "SA:EMPTY").await, 3.0);

    process(&db, "SA:EMPTY").await;
    assert_eq!(
        db.get_pv("SA:EMPTY").unwrap(),
        EpicsValue::DoubleArray(vec![30.0, 40.0])
    );
    assert_eq!(nord(&db, "SA:EMPTY").await, 2.0);

    process(&db, "SA:EMPTY").await;
    assert_eq!(
        db.get_pv("SA:EMPTY").unwrap(),
        EpicsValue::DoubleArray(vec![40.0])
    );
    assert_eq!(nord(&db, "SA:EMPTY").await, 1.0);

    process(&db, "SA:EMPTY").await;
    assert_eq!(nord(&db, "SA:EMPTY").await, 0.0);
    assert!(
        udf(&db, "SA:EMPTY").await,
        "NORD == 0 → readValue status -1 → UDF (softIoc: SEVR INVALID, STAT UDF)"
    );
}

/// The DB-link path is unchanged: `read_sa`'s `else` arm reads INDX+NELM
/// elements from the source and subsets them, and the source is re-read every
/// cycle (so no in-place eating).
#[epics_macros_rs::epics_test]
async fn db_link_inp_re_reads_the_source_every_cycle() {
    let db = build().await;

    db.put_pv(
        "SRC",
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0, 5.0]),
    )
    .await
    .unwrap();
    process(&db, "SA:LINK").await;
    assert_eq!(
        db.get_pv("SA:LINK").unwrap(),
        EpicsValue::DoubleArray(vec![2.0, 3.0, 4.0]),
        "INDX=1 NELM=3 of the source"
    );

    // Idempotent across cycles — the source, not the record's own buffer, is
    // the thing being sliced.
    process(&db, "SA:LINK").await;
    assert_eq!(
        db.get_pv("SA:LINK").unwrap(),
        EpicsValue::DoubleArray(vec![2.0, 3.0, 4.0])
    );
    assert_eq!(nord(&db, "SA:LINK").await, 3.0);
}
