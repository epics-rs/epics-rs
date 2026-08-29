//! R15-80, corrected in R21: what a `NELM=1` array record serves at init.
//!
//! R15-80 read the RECORD's `init_record` pass 0 — `prec->nord = (prec->nelm ==
//! 1)` (waveformRecord.c:100, aaiRecord.c:113, aaoRecord.c:116-120) — and
//! stopped there, so it asserted NORD=1 on all three kinds. That seed is not the
//! last word: the soft DEVICE SUPPORT's `init_record` runs in the same iocInit
//! and overwrites it on two of the three. Measured on the compiled softIoc
//! (7.0.10.1-DEV), bare `record(x,"P"){}`, never processed:
//!
//! ```text
//! $ caget -t P:WF.NORD    -> 0     waveform
//! $ caget -t P:AAI.NORD   -> 1     aai
//! $ caget -t P:AAO.NORD   -> 0     aao
//! $ caget -t P:SA.NORD    -> 0     subArray
//! ```
//!
//! * `devWfSoft.c:39-51` runs `dbLoadLinkArray` on the INP unconditionally and
//!   sets `prec->nord = 0` when it fails — which it does for anything but a
//!   constant (`dbLink.c:253-262`: no `loadArray` lset ⇒ `S_db_noLSET`).
//! * `devAaiSoft.c:55` loads only `if (dbLinkIsConstant(plink))`, so a waveform-
//!   shaped aai keeps the record's seed.
//! * `devAaoSoft.c:43-51` is `if (dbLinkIsConstant(&prec->out)) prec->nord = 0;`
//!   and runs at pass 0 — BEFORE `doResolveLinks` (`iocInit.c::initDatabase`),
//!   when every link still reads as a constant — so it fires on every aao.
//!
//! The full link-shape table is `tests/array_nord_at_init.rs`; this file keeps
//! the NELM boundary (NELM==1 vs NELM>1) per kind, with no record processed.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(waveform, "N1:WF") {
    field(FTVL, "DOUBLE")
    field(NELM, "1")
}
record(aai, "N1:AAI") {
    field(FTVL, "DOUBLE")
    field(NELM, "1")
}
record(aao, "N1:AAO") {
    field(FTVL, "DOUBLE")
    field(NELM, "1")
}
record(subArray, "N1:SUB") {
    field(FTVL, "DOUBLE")
    field(MALM, "8")
    field(NELM, "1")
}
record(waveform, "N8:WF") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
}
record(aai, "N8:AAI") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
}
record(aao, "N8:AAO") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
}
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

async fn nord(db: &PvDatabase, rec: &str) -> f64 {
    db.get_pv(&format!("{rec}.NORD")).unwrap().to_f64().unwrap()
}

/// `aai` is the one kind whose dset leaves the seed alone: its single element IS
/// the value, and a client reads it before the first process.
#[epics_macros_rs::epics_test]
async fn nelm1_seeds_nord1_on_aai_and_serves_one_element() {
    let db = build().await;
    assert_eq!(nord(&db, "N1:AAI").await, 1.0, "devAaiSoft keeps the seed");
    assert_eq!(
        db.get_pv("N1:AAI").unwrap(),
        EpicsValue::DoubleArray(vec![0.0]),
        "a NELM=1 aai serves its single element before first process"
    );
}

/// waveform and aao: the seed does not survive their dsets, so a NELM=1 record
/// serves a zero-length array until something loads elements into it — exactly
/// what C serves.
#[epics_macros_rs::epics_test]
async fn nelm1_keeps_nord_zero_on_waveform_and_aao() {
    let db = build().await;
    for rec in ["N1:WF", "N1:AAO"] {
        assert_eq!(
            nord(&db, rec).await,
            0.0,
            "{rec}: the soft dset's init zeroes the NELM=1 seed"
        );
        assert_eq!(
            db.get_pv(rec).unwrap(),
            EpicsValue::DoubleArray(vec![]),
            "{rec}: NORD=0 serves a zero-length array"
        );
    }
}

#[epics_macros_rs::epics_test]
async fn nelm_above_one_keeps_nord_zero() {
    let db = build().await;
    for rec in ["N8:WF", "N8:AAI", "N8:AAO"] {
        assert_eq!(nord(&db, rec).await, 0.0, "{rec}: NELM>1 must keep NORD=0");
        assert_eq!(
            db.get_pv(rec).unwrap(),
            EpicsValue::DoubleArray(vec![]),
            "{rec}: NORD=0 serves a zero-length array"
        );
    }
}

/// subArray has no NELM==1 seed in C (`subArrayRecord.c:101` sets NORD=0
/// unconditionally) — its NORD comes from the INP slice.
#[epics_macros_rs::epics_test]
async fn subarray_nelm1_keeps_nord_zero() {
    let db = build().await;
    assert_eq!(
        nord(&db, "N1:SUB").await,
        0.0,
        "subArray must NOT take the NELM=1 NORD seed"
    );
}
