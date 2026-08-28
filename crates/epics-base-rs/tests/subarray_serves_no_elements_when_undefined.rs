//! C `subArrayRecord.c::get_array_info` (176-187) is the only one of the four
//! array records that gates its element count on UDF:
//!
//! ```c
//! if (prec->udf)
//!    *no_elements = 0;
//! else
//!    *no_elements = prec->nord;
//! ```
//!
//! and `process` (148) fills that UDF from `readValue`'s status, which is the
//! INP read status OR the empty-slice test (313-319). `devSASoft.c::read_sa`
//! (118-120) runs `subset()` only when the read succeeded, so a broken INP
//! leaves the buffer and NORD at their pre-failure contents — which is exactly
//! why the count has to be suppressed instead.
//!
//! `waveformRecord.c:196` and `aaiRecord.c:221` return NORD flat, so the rule
//! is subArray's alone. Every case below asserts the ELEMENT COUNT: STAT/SEVR
//! agree between C and the port either way (both land LINK/INVALID from
//! `setLinkAlarm`), so the count is the only signal an operator gets.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const DB: &str = r#"
record(subArray, "SA:DEAD") {
    field(FTVL, "DOUBLE") field(MALM, "10") field(NELM, "5") field(INDX, "0")
    field(INP, "NOSUCHREC CA")
}
record(subArray, "SA:NOLINK") {
    field(FTVL, "DOUBLE") field(MALM, "10") field(NELM, "5") field(INDX, "0")
}
record(waveform, "WF:DEAD") {
    field(FTVL, "DOUBLE") field(NELM, "5")
    field(INP, "NOSUCHREC CA")
}
"#;

const SEED: [f64; 5] = [10.0, 20.0, 30.0, 40.0, 50.0];

struct Probe {
    udf: u8,
    nord: i64,
    served: u32,
}

async fn build() -> Arc<epics_base_rs::server::database::PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn probe(db: &epics_base_rs::server::database::PvDatabase, rec: &str) -> Probe {
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    let nord = match g.record.get_field("NORD") {
        Some(v) => v.to_f64().unwrap() as i64,
        None => panic!("{rec}: no NORD"),
    };
    let served = g.record.get_field("VAL").unwrap().count();
    Probe {
        udf: g.common.udf,
        nord,
        served,
    }
}

async fn seed_and_process(db: &epics_base_rs::server::database::PvDatabase, rec: &str) {
    db.put_pv(rec, EpicsValue::DoubleArray(SEED.to_vec()))
        .await
        .unwrap();
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// Boundary: read failed, buffer non-empty. C: UDF 1, NORD 5, count 0.
#[epics_macros_rs::epics_test]
async fn a_subarray_whose_inp_read_failed_serves_zero_elements() {
    let db = build().await;
    seed_and_process(&db, "SA:DEAD").await;
    let p = probe(&db, "SA:DEAD");
    assert_eq!(p.udf, 1, "a failed INP read makes a subArray UNDEFINED");
    assert_eq!(p.nord, 5, "C leaves NORD at its pre-failure value");
    assert_eq!(
        p.served, 0,
        "get_array_info must suppress the stale slice, not hand it out as data"
    );
}

/// Boundary: no INP at all, buffer non-empty. The empty-INP subArray subsets
/// its own buffer (`S_db_badField` -> `nRequest = prec->nord; status = 0`), so
/// the read never failed and the count is NORD.
#[epics_macros_rs::epics_test]
async fn a_subarray_with_no_input_link_still_serves_its_slice() {
    let db = build().await;
    seed_and_process(&db, "SA:NOLINK").await;
    let p = probe(&db, "SA:NOLINK");
    assert_eq!(p.udf, 0, "a subset that ran is a status of 0");
    assert_eq!(p.served, 5, "INDX 0 NELM 5 over a 5-element buffer");
}

/// Boundary: the same failure on a waveform. `waveformRecord.c:196` has no UDF
/// gate and `process` clears UDF on the line after `readValue` regardless, so
/// the stale buffer IS what C serves here. The subArray rule must not leak.
#[epics_macros_rs::epics_test]
async fn a_waveform_whose_inp_read_failed_still_serves_its_buffer() {
    let db = build().await;
    seed_and_process(&db, "WF:DEAD").await;
    let p = probe(&db, "WF:DEAD");
    assert_eq!(p.udf, 0, "waveform clears UDF unconditionally");
    assert_eq!(p.served, 5, "waveform get_array_info returns NORD flat");
}

/// Boundary: the failure is remembered, not latched. C's UDF has two clears —
/// `process`'s re-derive and `dbPut`'s `if (isValueField) precord->udf = FALSE`
/// (`dbAccess.c:1410`) — so a put that re-establishes VAL makes the record
/// defined again even while its INP stays broken.
#[epics_macros_rs::epics_test]
async fn a_put_to_val_re_establishes_a_failed_subarray() {
    let db = build().await;
    seed_and_process(&db, "SA:DEAD").await;
    assert_eq!(probe(&db, "SA:DEAD").served, 0);

    db.put_pv("SA:DEAD", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
        .await
        .unwrap();
    let p = probe(&db, "SA:DEAD");
    assert_eq!(p.udf, 0, "dbPut clears UDF on a value field");
    assert_eq!(p.served, 3, "the freshly written buffer is servable data");
}
