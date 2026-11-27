//! R18-113: `clear_histogram` ends on `prec->udf = FALSE`, and the port stopped
//! one statement short.
//!
//! C `histogramRecord.c:354-364` at `R7.0.10`:
//!
//! ```c
//! static long clear_histogram(histogramRecord *prec)
//! {
//!     int i;
//!
//!     for (i = 0; i < prec->nelm; i++)
//!         prec->bptr[i] = 0;
//!     prec->mcnt = prec->mdel + 1;
//!     prec->udf = FALSE;
//!
//!     return 0;
//! }
//! ```
//!
//! The port zeroed the bins and set `mcnt`, then returned. A never-processed
//! histogram is born `UDF=1` and histogram's `process()` never clears it (that
//! is R17-63, and it is correct), so `clear_histogram` is the record's ONLY
//! runtime route out of undefined: without the third statement `caput HG.CMD 1`
//! left `UDF=1` standing forever.
//!
//! Two puts reach the clear, and both must move the flag —
//! `CMD <= 1` (SPC_CALC, `:246-259`) and a ULIM/LLIM write (SPC_RESET,
//! `:266-273`). The clear latches for both, so they cannot drift apart.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::histogram::HistogramRecord;
use epics_base_rs::types::EpicsValue;

async fn db_with_histogram() -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("HG", Box::new(HistogramRecord::default()))
        .await
        .unwrap();
    db
}

async fn udf(db: &PvDatabase) -> u8 {
    let rec = db.get_record("HG").unwrap();
    let inst = rec.read();
    inst.common.udf
}

/// A fresh histogram is undefined — the premise the row rests on.
#[epics_macros_rs::epics_test]
async fn a_fresh_histogram_is_undefined() {
    let db = db_with_histogram().await;
    assert_eq!(
        udf(&db).await,
        1,
        "a never-processed histogram is born UDF=1"
    );
}

/// `caput HG.CMD 1` — the Clear arm. C's `clear_histogram` ends on
/// `prec->udf = FALSE`; this is the assertion that failed before the fix.
#[epics_macros_rs::epics_test]
async fn cmd_clear_clears_udf() {
    let db = db_with_histogram().await;
    assert_eq!(udf(&db).await, 1);

    db.put_record_field_from_ca("HG", "CMD", EpicsValue::Short(1))
        .await
        .unwrap();

    assert_eq!(
        udf(&db).await,
        0,
        "CMD=Clear runs clear_histogram, which ends on prec->udf = FALSE"
    );
}

/// `CMD = 0` (Read) takes the same `cmd <= 1` arm in C, so it clears too.
#[epics_macros_rs::epics_test]
async fn cmd_read_takes_the_same_arm_and_clears_udf() {
    let db = db_with_histogram().await;

    db.put_record_field_from_ca("HG", "CMD", EpicsValue::Short(0))
        .await
        .unwrap();

    assert_eq!(
        udf(&db).await,
        0,
        "C's arm is `cmd <= 1`, so Read clears exactly as Clear does"
    );
}

/// The other route into the clear: a ULIM/LLIM write is `SPC_RESET`, which
/// recomputes WDTH and calls `clear_histogram`. Latching inside the clear is
/// what makes this arm agree with the CMD arm.
#[epics_macros_rs::epics_test]
async fn ulim_write_clears_udf_through_the_same_clear() {
    let db = db_with_histogram().await;
    assert_eq!(udf(&db).await, 1);

    db.put_record_field_from_ca("HG", "ULIM", EpicsValue::Double(100.0))
        .await
        .unwrap();

    assert_eq!(
        udf(&db).await,
        0,
        "SPC_RESET on ULIM calls clear_histogram, which clears UDF"
    );
}

/// A put that reaches no clear must leave UDF alone — the flag is cleared by
/// `clear_histogram`, not by "any put to a histogram".
#[epics_macros_rs::epics_test]
async fn cmd_start_does_not_clear_udf() {
    let db = db_with_histogram().await;

    db.put_record_field_from_ca("HG", "CMD", EpicsValue::Short(2))
        .await
        .unwrap();

    assert_eq!(
        udf(&db).await,
        1,
        "CMD=Start sets CSTA and returns; C never touches udf on that arm"
    );
}

/// The latch is consumed by the put that set it: a later unrelated put must not
/// re-clear a UDF that has since been re-raised.
#[epics_macros_rs::epics_test]
async fn the_clear_does_not_survive_its_own_put() {
    let db = db_with_histogram().await;

    db.put_record_field_from_ca("HG", "CMD", EpicsValue::Short(1))
        .await
        .unwrap();
    assert_eq!(udf(&db).await, 0);

    // Re-raise UDF the way a direct `caput HG.UDF 1` does, then take an arm
    // that performs no clear.
    {
        let rec = db.get_record("HG").unwrap();
        rec.write().common.udf = 1;
    }
    db.put_record_field_from_ca("HG", "CMD", EpicsValue::Short(3))
        .await
        .unwrap();

    assert_eq!(
        udf(&db).await,
        1,
        "the Stop arm clears nothing — a stale latch would have cleared it"
    );
}

/// What the alarm does, measured rather than assumed. C's `clear_histogram`
/// writes `udf` and nothing else — STAT/SEVR are not touched, and histogram has
/// no `checkAlarms` UDF test to re-derive them (R17-63), so whatever the record
/// was born with stands until something else moves it.
#[epics_macros_rs::epics_test]
async fn the_clear_moves_udf_only_not_stat_or_sevr() {
    let db = db_with_histogram().await;
    let born = {
        let rec = db.get_record("HG").unwrap();
        let inst = rec.read();
        (inst.common.stat, inst.common.sevr)
    };

    db.put_record_field_from_ca("HG", "CMD", EpicsValue::Short(1))
        .await
        .unwrap();

    let rec = db.get_record("HG").unwrap();
    let inst = rec.read();
    assert_eq!(inst.common.udf, 0);
    assert_eq!(
        (inst.common.stat, inst.common.sevr),
        born,
        "C's clear_histogram writes udf and nothing else"
    );
}
