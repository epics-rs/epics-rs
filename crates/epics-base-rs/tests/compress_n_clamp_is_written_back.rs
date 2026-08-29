//! The array algorithms write their `N` clamp back into the field.
//!
//! C `compress_array` (`compressRecord.c:171-172`) and `array_average`
//! (`:255-256`) both open with
//!
//! ```c
//! if (prec->n <= 0)
//!     prec->n = 1;
//! n = prec->n;
//! ```
//!
//! — the store is into the FIELD, not a local, so a client that puts `N = 0`
//! reads 1 back after the next array cycle. Dividing by a local 1 while leaving
//! the field at 0 gives the same samples but makes the record report a
//! configuration it is not running.
//!
//! The scalar path is the boundary on the other side: C `compress_scalar`
//! (`:273-304`) has no such line, and with `n == 0` its `inx >= prec->n` test
//! at `:296` is already true on the first sample, so it emits every sample and
//! leaves N at 0. Both are asserted here, because the fix is only correct if it
//! stops at the array algorithms.
//!
//! ALG selects which C routine runs: `Average` is `array_average`, the
//! `N to 1 *` family is `compress_array`, and a scalar INP reaches
//! `compress_scalar` whatever the ALG.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const DB: &str = r#"
record(waveform, "SRC") { field(FTVL, "DOUBLE") field(NELM, "4") }
record(ai, "SCALAR:SRC") { field(VAL, "7") }

record(compress, "AVG") {
    field(INP, "SRC") field(ALG, "Average") field(NSAM, "4") field(N, "0")
}
record(compress, "N21") {
    field(INP, "SRC") field(ALG, "N to 1 Low Value") field(NSAM, "4") field(N, "0")
}
record(compress, "SCALAR") {
    field(INP, "SCALAR:SRC") field(ALG, "N to 1 Low Value") field(NSAM, "4") field(N, "0")
}
"#;

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

async fn build() -> Db {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &Db, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

fn n(db: &Db, rec: &str) -> i64 {
    db.get_record(rec)
        .unwrap()
        .read()
        .record
        .get_field("N")
        .and_then(|v| v.to_f64())
        .map(|v| v as i64)
        .expect("N")
}

async fn feed_array(db: &Db) {
    db.put_pv("SRC", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0]))
        .await
        .unwrap();
}

/// `array_average` — C `compressRecord.c:255-256`.
#[epics_macros_rs::epics_test]
async fn the_average_algorithm_writes_the_n_clamp_back() {
    let db = build().await;
    assert_eq!(
        n(&db, "AVG"),
        0,
        "the DB put N=0 and init does not clamp it"
    );
    feed_array(&db).await;
    process(&db, "AVG").await;
    assert_eq!(n(&db, "AVG"), 1, "C stores the clamp into prec->n");
}

/// `compress_array` — C `compressRecord.c:171-172`.
#[epics_macros_rs::epics_test]
async fn the_n_to_1_algorithms_write_the_n_clamp_back() {
    let db = build().await;
    assert_eq!(n(&db, "N21"), 0);
    feed_array(&db).await;
    process(&db, "N21").await;
    assert_eq!(n(&db, "N21"), 1, "C stores the clamp into prec->n");
}

/// The boundary the fix must NOT cross: `compress_scalar` has no clamp, so N
/// stays where the client put it.
#[epics_macros_rs::epics_test]
async fn the_scalar_algorithm_leaves_n_alone() {
    let db = build().await;
    process(&db, "SCALAR").await;
    assert_eq!(
        n(&db, "SCALAR"),
        0,
        "compressRecord.c:273-304 never touches prec->n"
    );
}
