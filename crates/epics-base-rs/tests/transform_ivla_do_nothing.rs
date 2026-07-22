//! R9-61 — transform IVLA="Do Nothing" abandons the WHOLE cycle when an
//! MS-class input link carries an INVALID severity.
//!
//! C `transformRecord.c:554-560`, right after the input-link fetch loop:
//!
//! ```c
//! if ((ptran->nsev >= INVALID_ALARM) && (ptran->ivla == transformIVLA_DO_NOTHING)) {
//!     recGblGetTimeStamp(ptran);
//!     checkAlarms(ptran);
//!     recGblResetAlarms(ptran);   /* monitor normally would do this */
//!     ptran->pact = FALSE;
//!     return (0);
//! }
//! ```
//!
//! No calc for any of the 16 channels, none of the 16 `OUTx` `dbPutLink`
//! writes, no `monitor()`, no `recGblFwdLink()` — only the timestamp and the
//! alarm commit. The input values themselves ARE already in A..P (the fetch
//! loop precedes the test); they are simply not published or acted on.
//!
//! Before the fix the port used IVLA only as a per-channel calc-error policy,
//! so this cycle recomputed CLCB and drove OUTB.

// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::types::EpicsValue;

/// SRC: ai VAL=200 over HIHI=100 / HHSV=INVALID — an INVALID severity with a
/// FINITE value, so the only INVALID source reaching TR is the MS input link.
async fn invalid_source(db: &PvDatabase) {
    db.add_record("SRC", Box::new(AiRecord::new(200.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("SRC").unwrap();
        let mut inst = rec.write();
        inst.put_common_field("HIHI", EpicsValue::Double(100.0))
            .unwrap();
        inst.put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Invalid as i16))
            .unwrap();
    }
    let mut v = HashSet::new();
    db.process_record_with_links("SRC", &mut v, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_record("SRC").unwrap().read().common.sevr,
        AlarmSeverity::Invalid,
        "SRC must be INVALID with a finite VAL=200 (HIHI=100/HHSV=INVALID)"
    );
}

async fn add_transform(db: &PvDatabase, ivla: i16) {
    let mut tr = TransformRecord::new();
    tr.put_field("INPA", EpicsValue::String("SRC MS".into()))
        .unwrap();
    tr.put_field("CLCB", EpicsValue::String("A+100".into()))
        .unwrap();
    tr.put_field("OUTB", EpicsValue::String("TGT".into()))
        .unwrap();
    tr.put_field("IVLA", EpicsValue::Short(ivla)).unwrap();
    db.add_record("TR", Box::new(tr)).await.unwrap();
}

async fn tr_field(db: &PvDatabase, field: &str) -> Option<EpicsValue> {
    db.get_record("TR").unwrap().read().record.get_field(field)
}

#[tokio::test]
async fn r9_61_ivla_do_nothing_skips_calc_and_every_output_link() {
    let db = PvDatabase::new();
    invalid_source(&db).await;
    // OUT target seeded 0.0 — must not be driven while the record is frozen.
    db.add_record("TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    add_transform(&db, 1).await; // IVLA = "Do Nothing"

    let mut v = HashSet::new();
    db.process_record_with_links("TR", &mut v, 0).await.unwrap();

    // C reads the input links BEFORE the IVLA test, so A carries the fresh
    // value even on the abandoned cycle.
    assert_eq!(
        tr_field(&db, "A").await,
        Some(EpicsValue::Double(200.0)),
        "the input-link fetch precedes the IVLA test in C — A is updated"
    );
    assert_eq!(
        tr_field(&db, "B").await,
        Some(EpicsValue::Double(0.0)),
        "CLCB must NOT be evaluated: C returns before the calc loop \
         (transformRecord.c:554-560)"
    );
    assert_eq!(
        db.get_pv("TGT").unwrap().to_f64(),
        Some(0.0),
        "OUTB must NOT be written: C returns before the output loop \
         (transformRecord.c:608-619)"
    );
    // The alarm commit is the one thing C still runs on this cycle.
    assert_eq!(
        db.get_record("TR").unwrap().read().common.sevr,
        AlarmSeverity::Invalid,
        "recGblResetAlarms still runs on the abandoned cycle — the MS link's \
         INVALID severity commits to SEVR"
    );
}

/// Same wiring, IVLA="Ignore error" (0): the identical INVALID input must NOT
/// freeze the record — proving the freeze above is IVLA's doing and not a
/// side effect of the INVALID severity itself.
#[tokio::test]
async fn r9_61_ivla_ignore_error_still_calcs_and_drives_outputs() {
    let db = PvDatabase::new();
    invalid_source(&db).await;
    db.add_record("TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    add_transform(&db, 0).await; // IVLA = "Ignore error"

    let mut v = HashSet::new();
    db.process_record_with_links("TR", &mut v, 0).await.unwrap();

    assert_eq!(
        tr_field(&db, "B").await,
        Some(EpicsValue::Double(300.0)),
        "IVLA=Ignore error: CLCB = A+100 = 300 runs despite the INVALID input"
    );
    assert_eq!(
        db.get_pv("TGT").unwrap().to_f64(),
        Some(300.0),
        "IVLA=Ignore error: OUTB drives the target"
    );
}
