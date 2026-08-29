//! R16-81: `histogram`'s input link is SVL, not INP.
//!
//! `histogramRecord.dbd.pod:212` declares `field(SVL,DBF_INLINK)` — "Signal
//! Value Location" — and declares NO `INP` at all. `devHistogramSoft.c` is the
//! only soft path in or out:
//!
//! ```c
//!     static long init_record(dbCommon *pcommon) {              /* :40-48 */
//!         if (recGblInitConstantLink(&prec->svl, DBF_DOUBLE, &prec->sgnl))
//!             prec->udf = FALSE;
//!     }
//!     static long read_histogram(histogramRecord *prec) {       /* :50-54 */
//!         dbGetLink(&prec->svl, DBR_DOUBLE, &prec->sgnl, 0, 0);
//!         return 0;                                             /* add count */
//!     }
//! ```
//!
//! and `process()` then bins whatever SGNL holds (`histogramRecord.c:218-219`).
//! The port had no SVL field and drove the record off the common INP, so
//! `record(histogram){field(SVL,"MYSIG")}` was dropped at load and the record
//! was inert — while `field(INP,...)`, which C's dbd refuses outright, worked.
//!
//! Expectations are from a built softIoc
//! (`/home/stevek/work/epics-base/bin/linux-x86_64/softIoc`) driving these exact
//! records over CA. Its loader rejects the INP form with
//! `ERROR: histogram record 'H:INP' doesn't have a field 'INP'`.

use std::collections::HashSet;

use epics_base_rs::error::CaError;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ai, "SRC") {
    field(VAL, "0")
}
record(histogram, "H:CONST") {
    field(SVL, "2.5")
    field(NELM, "4")
    field(LLIM, "0")
    field(ULIM, "4")
}
record(histogram, "H:LINK") {
    field(SVL, "SRC")
    field(NELM, "4")
    field(LLIM, "0")
    field(ULIM, "4")
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

async fn bins(db: &epics_base_rs::server::database::PvDatabase, rec: &str) -> Vec<u32> {
    match db.get_pv(rec).unwrap() {
        // C `cvt_dbaddr` declares the bins DBF_ULONG (histogramRecord.c:304).
        EpicsValue::ULongArray(v) => v,
        other => panic!("{rec}.VAL must be a ULongArray, got {other:?}"),
    }
}

/// C `recGblInitConstantLink(&prec->svl, DBF_DOUBLE, &prec->sgnl)`: a constant
/// SVL seeds SGNL at init and clears UDF — and does NOT bin anything (add_count
/// runs only from `process()` / the SGNL `special()`). softIoc: `SGNL = 2.5`,
/// `UDF = 0`, `VAL = 0 0 0 0`.
#[epics_macros_rs::epics_test]
async fn constant_svl_seeds_sgnl_at_init_without_counting() {
    let db = build().await;

    assert_eq!(
        db.get_pv("H:CONST.SGNL").unwrap().to_f64(),
        Some(2.5),
        "field(SVL,\"2.5\") must seed SGNL at init"
    );
    assert_eq!(bins(&db, "H:CONST").await, vec![0, 0, 0, 0]);
    assert!(
        db.get_record("H:CONST").unwrap().read().common.udf == 0,
        "a histogram seeded from a constant SVL is DEFINED"
    );
}

/// Each process bins the seeded SGNL once. softIoc, NELM=4 LLIM=0 ULIM=4
/// (WDTH=1), SGNL=2.5 → bucket 2: `0 0 1 0` after one process, `0 0 2 0` after
/// two.
#[epics_macros_rs::epics_test]
async fn constant_svl_bins_once_per_process() {
    let db = build().await;

    process(&db, "H:CONST").await;
    assert_eq!(bins(&db, "H:CONST").await, vec![0, 0, 1, 0]);

    process(&db, "H:CONST").await;
    assert_eq!(bins(&db, "H:CONST").await, vec![0, 0, 2, 0]);
}

/// A real SVL link is read into SGNL every process (`dbGetLink`) and binned
/// ONCE. softIoc: `SRC = 3.5`, process → `SGNL = 3.5`, `VAL = 0 0 0 1`.
///
/// The single count is the load-bearing part: `dbGetLink` writes `prec->sgnl`
/// directly and never runs `special()`, so the SPC_MOD `add_count` that a SGNL
/// *caput* triggers must not fire on the link delivery — otherwise every linked
/// sample would be counted twice.
#[epics_macros_rs::epics_test]
async fn real_svl_link_is_read_into_sgnl_and_binned_once_per_process() {
    let db = build().await;

    db.put_pv("SRC", EpicsValue::Double(3.5)).await.unwrap();
    process(&db, "H:LINK").await;

    assert_eq!(db.get_pv("H:LINK.SGNL").unwrap().to_f64(), Some(3.5));
    assert_eq!(
        bins(&db, "H:LINK").await,
        vec![0, 0, 0, 1],
        "one process = one sample in bucket 3"
    );

    db.put_pv("SRC", EpicsValue::Double(0.5)).await.unwrap();
    process(&db, "H:LINK").await;
    assert_eq!(bins(&db, "H:LINK").await, vec![1, 0, 0, 1]);
}

/// The SGNL `special()` path is unchanged: a caput to SGNL counts the sample
/// itself (`histogramRecord.c:334`, SPC_MOD → `add_count`) and SGNL is not
/// `pp(TRUE)`, so no process runs and the sample is counted exactly once.
/// softIoc: `caput H:LINK.SGNL 0.5` → `VAL` bucket 0 += 1.
#[epics_macros_rs::epics_test]
async fn sgnl_caput_still_counts_through_special() {
    let db = build().await;

    db.put_record_field_from_ca("H:LINK", "SGNL", EpicsValue::Double(0.5))
        .await
        .unwrap();
    assert_eq!(bins(&db, "H:LINK").await, vec![1, 0, 0, 0]);
}

/// C's dbd has no INP on a histogram: softIoc's loader answers
/// `ERROR: histogram record 'H:INP' doesn't have a field 'INP'` and the record
/// is inert. The port must refuse it at the same gate rather than driving the
/// record from a field the C record type does not have.
#[epics_macros_rs::epics_test]
async fn field_inp_is_refused_on_a_histogram() {
    const WITH_INP: &str = r#"
record(ai, "SRC2") { field(VAL, "1.5") }
record(histogram, "H:INP") {
    field(INP, "SRC2")
    field(NELM, "4")
    field(LLIM, "0")
    field(ULIM, "4")
}
"#;
    // Refusing the field also fails the load's status, which is what C's
    // `dbLoadRecords` returns (`dbAccess.c:795-813`) and what makes
    // `softIoc -d` exit 2 (`softMain.cpp:198,274-278`).
    let Err(err) = IocBuilder::new()
        .db_string(WITH_INP, &std::collections::HashMap::new())
        .expect("the refusal is recoverable; the load's status settles at build")
        .build()
        .await
    else {
        panic!("field(INP) on a histogram must fail the load");
    };
    assert!(
        matches!(err, CaError::DbLoadFailed(_)),
        "expected a failed load status, got {err:?}"
    );

    // C's iocsh keeps the record and drops only the offending field, so the
    // record that survives such a load is the one below: no INP anywhere.
    let db = IocBuilder::new()
        .db_string(
            r#"
record(ai, "SRC2") { field(VAL, "1.5") }
record(histogram, "H:INP") {
    field(NELM, "4")
    field(LLIM, "0")
    field(ULIM, "4")
}
"#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    let inp = db.get_record("H:INP").unwrap().read().common.inp.clone();
    assert!(inp.is_empty(), "field(INP) must not land on a histogram");

    // And it drives nothing. A process still bins the current SGNL — that is
    // C's `process` → `add_count` — but SGNL is the untouched 0.0 default
    // (bucket 0), not SRC2's 1.5 (which would land in bucket 1).
    process(&db, "H:INP").await;
    assert_eq!(
        db.get_pv("H:INP.SGNL").unwrap().to_f64(),
        Some(0.0),
        "INP must not feed SGNL"
    );
    assert_eq!(bins(&db, "H:INP").await, vec![1, 0, 0, 0]);
}

/// R17-83: the dbd is the whole namespace, not just the loader. A field the C
/// record type does not declare has no `.FIELD` channel either — softIoc, on a
/// loaded histogram:
///
/// ```text
/// dbgf HI.INP  -> PV 'HI.INP' not found
/// dbgf HI.SVL  -> DBF_STRING: ""
/// ```
///
/// The port refused `field(INP,...)` at load and at `dbPut`, but `.INP` still
/// resolved as a readable (empty) channel. Both routes now consult the same
/// `declares_inp_link()` declaration.
#[epics_macros_rs::epics_test]
async fn histogram_inp_does_not_resolve_as_a_channel() {
    let db = IocBuilder::new()
        .db_string(
            r#"
record(histogram, "H:NS") {
    field(NELM, "4")
    field(LLIM, "0")
    field(ULIM, "4")
}
"#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    let err = db
        .get_pv("H:NS.INP")
        .expect_err("histogram declares no INP — the channel must not exist");
    assert!(
        matches!(err, CaError::FieldNotFound(_) | CaError::ChannelNotFound(_)),
        "expected a not-found error for H:NS.INP, got {err:?}"
    );

    // A caput is refused on the same declaration...
    let err = db
        .put_record_field_from_ca("H:NS", "INP", EpicsValue::String("SRC2".into()))
        .await
        .expect_err("histogram declares no INP — a put must not land");
    assert!(
        matches!(err, CaError::FieldNotFound(_)),
        "expected FieldNotFound on a put to H:NS.INP, got {err:?}"
    );

    // ...while SVL, the DBF_INLINK a histogram DOES declare, resolves.
    assert_eq!(
        db.get_pv("H:NS.SVL").unwrap(),
        EpicsValue::String("".into()),
        "SVL (histogramRecord.dbd.pod:212) is the histogram's input link"
    );

    // The gate is per record type: an ai declares INP, so its channel resolves.
    let db2 = IocBuilder::new()
        .db_string(
            r#"record(ai, "A:INP") { field(INP, "1.5") }"#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    assert_eq!(
        db2.get_pv("A:INP.INP").unwrap(),
        EpicsValue::String("1.5".into())
    );
}
