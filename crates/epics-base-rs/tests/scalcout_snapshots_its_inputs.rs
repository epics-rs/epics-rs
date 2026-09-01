//! `scalcout` copies A..L into PA..PL and AA..LL into PAA..PLL at the top of
//! every process cycle, and PAA..PLL are FORTY one-byte channels.
//!
//! C `sCalcoutRecord.c:340-353`, inside the `!pact` arm and before
//! `fetch_values`:
//!
//! ```c
//! for (i=0, pcurr=&pcalc->a, pprev=&pcalc->pa; i<MAX_FIELDS; i++, pcurr++, pprev++) {
//!     *pprev = *pcurr;
//! }
//! for (i=0, pscurr=pcalc->strs, psprev=&pcalc->paa; i<STRING_MAX_FIELDS; i++, pscurr++, psprev++) {
//!     strNcpy(*psprev, *pscurr, STRING_SIZE);
//! }
//! ```
//!
//! `monitor` (`:855-861`) reads them back to decide what to post. The port had
//! neither snapshot, so PA..PL stayed 0 and PAA..PLL stayed empty for the life
//! of the record where the compiled softIoc tracks the inputs.
//!
//! The channel over PAA..PLL is the second half. `cvt_dbaddr`
//! (`sCalcoutRecord.c:588-596`) sets `no_elements = STRING_SIZE` with
//! `field_type = DBF_STRING` and leaves `field_size` at 1, so
//! `getStringString` copies ONE byte per element: the field is forty
//! one-character strings over the 40-byte buffer, not one 40-character string.
//! `cainfo` reports `DBF_STRING`/`40` against C and reported `1` here.
//!
//! Two consequences of `strNcpy` (`:146-153`) that a `String`-shaped port field
//! cannot express, both measured against `softIoc` R7.0.10 with the calc module
//! at `f207871`:
//!
//! ```text
//!   AA="hello", process   caget -#8 PAA  ->  8 h e l l o
//!   AA="hi",    process   caget -#8 PAA  ->  8 h i   l o      <- stale tail
//!   caput PAA zz          caget -#8 PAA  ->  8   i   l o      <- put CLEARS
//! ```
//!
//! The tail survives because `strNcpy` terminates without clearing, and the put
//! clears because `putStringString` writes `pdst[size-1] = 0` over the one byte
//! it just copied.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;
use std::sync::Arc;

const DB: &str = r#"
record(scalcout, "SCO") { field(CALC, "A+1") }
"#;

type Db = Arc<PvDatabase>;

async fn build() -> Db {
    IocBuilder::new()
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn put(db: &Db, field: &str, value: EpicsValue) {
    db.put_record_field_from_ca_no_notify("SCO", field, value)
        .await
        .unwrap_or_else(|e| panic!("put SCO.{field}: {e}"));
}

async fn process(db: &Db) {
    db.put_record_field_from_ca_no_notify("SCO", "PROC", EpicsValue::Long(1))
        .await
        .expect("PROC");
}

fn num(db: &Db, field: &str) -> f64 {
    db.get_pv(&format!("SCO.{field}"))
        .unwrap_or_else(|e| panic!("SCO.{field}: {e}"))
        .to_f64()
        .expect("numeric")
}

/// The channel exactly as a client reads it: a read-only scalar `DBF_STRING`.
fn prev_str(db: &Db, field: &str) -> String {
    match db.get_pv(&format!("SCO.{field}")).expect("prev string") {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("SCO.{field} is {other:?}"),
    }
}

/// The numeric half of the snapshot.
#[epics_macros_rs::epics_test]
async fn processing_copies_the_numeric_inputs_into_pa_to_pl() {
    let db = build().await;
    assert_eq!(num(&db, "PA"), 0.0, "nothing processed yet");

    put(&db, "A", EpicsValue::Double(5.0)).await;
    process(&db).await;
    assert_eq!(num(&db, "PA"), 5.0);

    put(&db, "B", EpicsValue::Double(3.0)).await;
    process(&db).await;
    assert_eq!(num(&db, "PA"), 5.0, "A did not change");
    assert_eq!(num(&db, "PB"), 3.0);
}

/// The string half, served as a read-only scalar `DBF_STRING` (calc#42).
#[epics_macros_rs::epics_test]
async fn processing_copies_the_string_inputs_into_paa_to_pll() {
    let db = build().await;
    put(&db, "AA", EpicsValue::String("hi".into())).await;
    process(&db).await;

    assert_eq!(
        prev_str(&db, "PAA"),
        "hi",
        "PAA is the previous cycle's AA, a scalar string"
    );
}

/// `strNcpy` terminates without clearing the tail, but a scalar `DBF_STRING`
/// reads only up to the NUL, so a shorter value no longer exposes the previous
/// one's tail (calc#42 — released C served the raw 40 bytes and did leak it).
#[epics_macros_rs::epics_test]
async fn a_shorter_value_reads_clean_over_a_longer_predecessor() {
    let db = build().await;
    put(&db, "AA", EpicsValue::String("hello".into())).await;
    process(&db).await;
    assert_eq!(prev_str(&db, "PAA"), "hello");

    put(&db, "AA", EpicsValue::String("hi".into())).await;
    process(&db).await;
    assert_eq!(
        prev_str(&db, "PAA"),
        "hi",
        "the NUL after `hi` hides `hello`'s uncleared tail"
    );
}

/// PAA..PLL are `special(SPC_NOMOD)` (calc#42): a client `caput` is refused
/// before it reaches the record, and the snapshot is untouched.
#[epics_macros_rs::epics_test]
async fn a_put_into_a_prev_string_is_refused() {
    let db = build().await;
    put(&db, "AA", EpicsValue::String("hi".into())).await;
    process(&db).await;

    let err = db
        .put_record_field_from_ca_no_notify("SCO", "PAA", EpicsValue::String("zz".into()))
        .await
        .expect_err("PAA is SPC_NOMOD; the put must be refused");
    assert!(
        matches!(err, epics_base_rs::error::CaError::ReadOnlyField(ref f) if f == "PAA"),
        "expected S_db_noMod (ReadOnlyField), got {err:?}"
    );
    assert_eq!(prev_str(&db, "PAA"), "hi", "the snapshot is unchanged");
}

/// The snapshot belongs to every one of the twelve pairs, not just the first.
#[epics_macros_rs::epics_test]
async fn the_snapshot_covers_all_twelve_pairs() {
    let db = build().await;
    put(&db, "L", EpicsValue::Double(9.0)).await;
    put(&db, "LL", EpicsValue::String("z".into())).await;
    process(&db).await;
    assert_eq!(num(&db, "PL"), 9.0);
    assert_eq!(prev_str(&db, "PLL"), "z");
}
