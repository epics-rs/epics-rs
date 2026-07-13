//! R15-80: `NELM=1` must yield `NORD=1` at init on waveform/aai/aao.
//!
//! C `init_record` pass 0 seeds `prec->nord = (prec->nelm == 1)`
//! (waveformRecord.c:100, aaiRecord.c:113, aaoRecord.c:116-120): a
//! one-element array record is fully populated by construction. The port
//! seeded NORD=0 unconditionally and `get_field("VAL")` truncates to NORD, so
//! a NELM=1 record served a ZERO-length array to every client until its first
//! process. subArray keeps NORD=0 (subArrayRecord.c:101 has no such seed).
//!
//! Boundaries: NELM==1 vs NELM>1, per kind; no record is processed — the seed
//! must come from init alone.

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
    db.get_pv(&format!("{rec}.NORD"))
        .await
        .unwrap()
        .to_f64()
        .unwrap()
}

#[tokio::test]
async fn nelm1_seeds_nord1_and_serves_one_element() {
    let db = build().await;
    for rec in ["N1:WF", "N1:AAI", "N1:AAO"] {
        assert_eq!(nord(&db, rec).await, 1.0, "{rec}: NELM=1 must seed NORD=1");
        assert_eq!(
            db.get_pv(rec).await.unwrap(),
            EpicsValue::DoubleArray(vec![0.0]),
            "{rec}: a NELM=1 record must serve its single element before first process"
        );
    }
}

#[tokio::test]
async fn nelm_above_one_keeps_nord_zero() {
    let db = build().await;
    for rec in ["N8:WF", "N8:AAI", "N8:AAO"] {
        assert_eq!(nord(&db, rec).await, 0.0, "{rec}: NELM>1 must keep NORD=0");
        assert_eq!(
            db.get_pv(rec).await.unwrap(),
            EpicsValue::DoubleArray(vec![]),
            "{rec}: NORD=0 serves a zero-length array"
        );
    }
}

/// subArray has no NELM==1 seed in C (`subArrayRecord.c:101` sets NORD=0
/// unconditionally) — its NORD comes from the INP slice at process time.
#[tokio::test]
async fn subarray_nelm1_keeps_nord_zero() {
    let db = build().await;
    assert_eq!(
        nord(&db, "N1:SUB").await,
        0.0,
        "subArray must NOT take the NELM=1 NORD seed"
    );
}
