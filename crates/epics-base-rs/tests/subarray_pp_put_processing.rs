//! R16-80: subArray VAL/NELM/INDX are `pp(TRUE)` (`subArrayRecord.dbd.pod`
//! :328, :382, :388) — a `dbPutField` to any of them PROCESSES the record, and
//! `read_sa` re-slices. NELM is not a buffer size on a subArray, it is the SLICE
//! LENGTH: C's `bptr` is MALM elements wide, allocated once at `init_record`, and
//! a NELM put only changes how many of them the next `subset()` keeps.
//!
//! The port reallocated a zeroed NELM-wide buffer on every NELM put, so a
//! `caput SA.NELM 2` EMPTIED VAL instead of narrowing the slice.
//!
//! Expectations are from a built softIoc
//! (`/home/stevek/work/epics-base/bin/linux-x86_64/softIoc`) driving these exact
//! records over CA.

// RTEMS-EXEC-MODEL-ALLOW(5): checked - these run and pass in the feature-ON suite.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
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

async fn field(
    db: &epics_base_rs::server::database::PvDatabase,
    rec: &str,
    f: &str,
) -> Option<EpicsValue> {
    db.get_pv(&format!("{rec}.{f}")).ok()
}

/// C, `caput SA:CONST.NELM 2` on `INP="[1,2,3,4]" MALM=8 INDX=0`:
/// `VAL = 1 2`, `NORD = 2`. The port emptied VAL (`NORD = 0`).
#[tokio::test]
async fn nelm_put_narrows_the_slice_it_does_not_empty_val() {
    let db = build().await;

    db.put_record_field_from_ca("SA:CONST", "NELM", EpicsValue::Long(2))
        .await
        .expect("subArray NELM is pp(TRUE), not SPC_NOMOD — the caput must land");

    assert_eq!(
        db.get_pv("SA:CONST").unwrap(),
        EpicsValue::DoubleArray(vec![1.0, 2.0]),
        "NELM=2 keeps the first two elements of the constant, it does not wipe VAL"
    );
    assert_eq!(
        field(&db, "SA:CONST", "NORD").await,
        Some(EpicsValue::Long(2))
    );
    assert_eq!(
        field(&db, "SA:CONST", "NELM").await,
        Some(EpicsValue::ULong(2))
    );
}

/// C `readValue` (310-311) clamps NELM to MALM at process, so a NELM put above
/// MALM lands as MALM. softIoc: `caput SA:CONST.NELM 9` → `NELM = 8`, and the
/// slice is the whole 4-element constant (`NORD = 4`).
#[tokio::test]
async fn nelm_put_above_malm_clamps_at_process() {
    let db = build().await;

    db.put_record_field_from_ca("SA:CONST", "NELM", EpicsValue::Long(9))
        .await
        .unwrap();

    assert_eq!(
        field(&db, "SA:CONST", "NELM").await,
        Some(EpicsValue::ULong(8))
    );
    assert_eq!(
        field(&db, "SA:CONST", "NORD").await,
        Some(EpicsValue::Long(4))
    );
    assert_eq!(
        db.get_pv("SA:CONST").unwrap(),
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0])
    );
}

/// C: `caput SA:CONST.INDX 1` → the window moves to `2 3 4` (the constant is
/// re-loaded whole, then shifted). The port stored INDX and left a stale window.
#[tokio::test]
async fn indx_put_moves_the_window() {
    let db = build().await;

    db.put_record_field_from_ca("SA:CONST", "INDX", EpicsValue::Long(1))
        .await
        .expect("subArray INDX is pp(TRUE) — the caput must land");

    assert_eq!(
        db.get_pv("SA:CONST").unwrap(),
        EpicsValue::DoubleArray(vec![2.0, 3.0, 4.0])
    );
    assert_eq!(
        field(&db, "SA:CONST", "NORD").await,
        Some(EpicsValue::Long(3))
    );
}

/// C: an INDX past the data leaves an empty slice — `subset`'s `ecount <= 0` →
/// `NORD = 0`, and `readValue`'s `nord <= 0 → status = -1` makes the record UDF
/// (softIoc: `SEVR INVALID`). MALM=8 keeps INDX=5 un-clamped, so this is the
/// genuine "past the source" path, not the MALM clamp.
#[tokio::test]
async fn indx_put_past_the_data_empties_the_slice_and_alarms() {
    let db = build().await;

    db.put_record_field_from_ca("SA:CONST", "INDX", EpicsValue::Long(5))
        .await
        .unwrap();

    assert_eq!(
        field(&db, "SA:CONST", "NORD").await,
        Some(EpicsValue::Long(0))
    );
    assert!(
        db.get_record("SA:CONST").unwrap().read().common.udf != 0,
        "an empty subArray slice is UNDEFINED (C `prec->udf = !!status`)"
    );
}

/// VAL is `pp(TRUE)` too, so the client's put processes the record and comes
/// back already sliced. softIoc, `SA:EMPTY` (INDX=1 NELM=3 MALM=8) after
/// `caput -a SA:EMPTY 5 10 20 30 40 50`: `VAL = 20 30 40`, `NORD = 3`. A
/// following `caput SA:EMPTY.NELM 2` re-subsets the buffer in place: `30 40`.
#[tokio::test]
async fn empty_inp_val_put_processes_and_nelm_put_re_subsets() {
    let db = build().await;

    db.put_record_field_from_ca(
        "SA:EMPTY",
        "VAL",
        EpicsValue::DoubleArray(vec![10.0, 20.0, 30.0, 40.0, 50.0]),
    )
    .await
    .unwrap();
    assert_eq!(
        db.get_pv("SA:EMPTY").unwrap(),
        EpicsValue::DoubleArray(vec![20.0, 30.0, 40.0]),
        "VAL is pp(TRUE): the put itself processes and slices"
    );

    db.put_record_field_from_ca("SA:EMPTY", "NELM", EpicsValue::Long(2))
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("SA:EMPTY").unwrap(),
        EpicsValue::DoubleArray(vec![30.0, 40.0])
    );
    assert_eq!(
        field(&db, "SA:EMPTY", "NORD").await,
        Some(EpicsValue::Long(2))
    );
}
