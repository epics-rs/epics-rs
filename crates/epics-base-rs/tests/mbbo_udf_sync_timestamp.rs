//! A soft (synchronous) mbbo / mbboDirect that is still UNDEFINED must leave
//! TIME at the EPICS epoch ("never processed"), matching C. The port used to
//! stamp wall-clock TIME on every sync process, so a UDF seed/update showed a
//! live timestamp the C IOC never sets.
//!
//! C `mbboRecord.c:210-221` / `mbboDirectRecord.c:190-202`:
//!
//! ```c
//! if (!pact) {
//!     ...
//!     else if (prec->udf) {
//!         recGblSetSevr(prec, UDF_ALARM, prec->udfs);
//!         goto CONTINUE;                       /* skips the stamp below */
//!     }
//!     prec->udf = FALSE;
//!     convert(prec);
//!     recGblGetTimeStampSimm(prec, prec->simm, NULL);   /* pre-output stamp */
//! }
//! CONTINUE:
//!     ...
//!     if (pact) {                              /* async completion ONLY */
//!         recGblGetTimeStampSimm(prec, prec->simm, NULL);
//!     }
//! ```
//!
//! A soft (pact never set) UDF record hits neither stamp, so TIME stays at the
//! epoch. ao/bo/longout stamp UNCONDITIONALLY in their `if (!pact)` block, so
//! they are NOT in this family and must keep stamping while undefined.

// RTEMS-EXEC-MODEL-ALLOW(4): checked - these run and pass in the feature-ON suite.

use epics_base_rs::runtime::general_time::epics_epoch;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::mbbo::MbboRecord;
use epics_base_rs::server::records::mbbo_direct::MbboDirectRecord;
use epics_base_rs::types::EpicsValue;

async fn add(db: &PvDatabase, name: &str, record: Box<dyn Record>) {
    db.add_record(name, record).await.unwrap();
}

async fn time_of(db: &PvDatabase, name: &str) -> std::time::SystemTime {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    inst.common.time
}

async fn udf_of(db: &PvDatabase, name: &str) -> u8 {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    inst.common.udf
}

/// A soft UDF mbbo, processed synchronously, keeps TIME at the epoch — C's
/// `goto CONTINUE` skips the pre-output stamp and the only other stamp is
/// `if (pact)`-guarded (never taken on a soft record).
#[tokio::test]
async fn sync_udf_mbbo_keeps_epoch_time() {
    let db = PvDatabase::new();
    add(&db, "M", Box::new(MbboRecord::new(0))).await;

    // Bare record is UNDEFINED.
    assert_eq!(udf_of(&db, "M").await, 1, "bare mbbo is UDF");

    db.process_record("M").await.unwrap();

    assert_eq!(
        udf_of(&db, "M").await,
        1,
        "sync process leaves a bare mbbo UDF"
    );
    assert_eq!(
        time_of(&db, "M").await,
        epics_epoch(),
        "sync UDF mbbo must leave TIME at the epoch, not wall-clock now"
    );
}

/// Same for mbboDirect: bit-derived VAL, but it shares the `goto CONTINUE`
/// timestamp-skip while undefined.
#[tokio::test]
async fn sync_udf_mbbo_direct_keeps_epoch_time() {
    let db = PvDatabase::new();
    add(&db, "MD", Box::new(MbboDirectRecord::default())).await;

    assert_eq!(udf_of(&db, "MD").await, 1, "bare mbboDirect is UDF");

    db.process_record("MD").await.unwrap();

    assert_eq!(
        udf_of(&db, "MD").await,
        1,
        "sync process leaves a bare mbboDirect UDF"
    );
    assert_eq!(
        time_of(&db, "MD").await,
        epics_epoch(),
        "sync UDF mbboDirect must leave TIME at the epoch, not wall-clock now"
    );
}

/// Once a VAL put clears UDF, the sync process DOES stamp wall-clock TIME —
/// the skip is gated on UDF exactly as C's `goto CONTINUE` is, so defined
/// updates keep stamping. Guards the fix against becoming an unconditional
/// skip.
#[tokio::test]
async fn defined_mbbo_stamps_wall_clock_time() {
    let db = PvDatabase::new();
    add(&db, "M2", Box::new(MbboRecord::new(0))).await;

    // Define VAL (clears UDF). The put processes the record.
    db.put_record_field_from_ca("M2", "VAL", EpicsValue::Enum(0))
        .await
        .unwrap();

    assert_eq!(udf_of(&db, "M2").await, 0, "VAL put clears UDF");
    assert!(
        time_of(&db, "M2").await > epics_epoch(),
        "a DEFINED mbbo must stamp wall-clock TIME (> epoch) on process"
    );
}

/// mbboDirect too: a VAL put clears UDF and the sync process stamps.
#[tokio::test]
async fn defined_mbbo_direct_stamps_wall_clock_time() {
    let db = PvDatabase::new();
    add(&db, "MD2", Box::new(MbboDirectRecord::default())).await;

    db.put_record_field_from_ca("MD2", "VAL", EpicsValue::Long(1))
        .await
        .unwrap();

    assert_eq!(udf_of(&db, "MD2").await, 0, "VAL put clears UDF");
    assert!(
        time_of(&db, "MD2").await > epics_epoch(),
        "a DEFINED mbboDirect must stamp wall-clock TIME (> epoch) on process"
    );
}
