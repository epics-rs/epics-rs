//! Cause 3: mbbo RVAL is `DBF_ULONG`, and on an UNDEFINED record a `caput RVAL`
//! stores the value verbatim — C's `process()` skips `convert()` while UDF=1.
//!
//! RVAL is `field(RVAL,DBF_ULONG){ pp(TRUE) }` (mbboRecord.dbd.pod), so a put
//! processes the record. On a bare `record(mbbo,"M"){}` VAL is undefined
//! (UDF=1), and C's `process()` takes the early exit before `convert()`:
//!
//! ```c
//! /* mbboRecord.c:199-217 */
//! if (!pact) {
//!     if (!dbLinkIsConstant(&prec->dol) && omsl == closed_loop) { ... }
//!     else if (prec->udf) {
//!         recGblSetSevr(prec, UDF_ALARM, prec->udfs);
//!         goto CONTINUE;          /* skip udf=FALSE AND convert() */
//!     }
//!     prec->udf = FALSE;
//!     convert(prec);              /* VAL -> RVAL */
//! }
//! ```
//!
//! So the put value survives — `convert` never recomputes `RVAL = VAL(=0)`.
//! Verified on the compiled softIoc: a bare `caput RVAL 1` reads back 1; after a
//! VAL put clears UDF, the next RVAL put IS overwritten. The port ran `convert()`
//! unconditionally and clobbered RVAL to 0; opting mbbo into the framework's
//! undefined-skip mirrors C's `goto CONTINUE`.

// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::mbbo::MbboRecord;
use epics_base_rs::types::EpicsValue;

/// `caput REC.FIELD <text>` over the CA put path the oracle drives.
async fn caput(db: &PvDatabase, field: &str, text: &str) -> Result<(), String> {
    db.put_record_field_from_ca("REC", field, EpicsValue::String(text.into()))
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The raw stored field value (not the CA-served projection).
async fn stored(db: &PvDatabase, field: &str) -> EpicsValue {
    let rec = db.get_record("REC").unwrap();
    let inst = rec.read();
    inst.record.get_field(field).unwrap()
}

async fn db_with(record: Box<dyn Record>) -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("REC", record).await.unwrap();
    db
}

/// On a bare mbbo (UDF=1) a `caput RVAL v` stores `v`. `4294967295` is
/// `u32::MAX`; `-1` reaches the unsigned field as `u32::MAX` too (C `strtoul`).
#[tokio::test]
async fn mbbo_rval_put_stores_on_undefined_record() {
    let db = db_with(Box::new(MbboRecord::new(0))).await;
    caput(&db, "RVAL", "1").await.unwrap();
    assert_eq!(stored(&db, "RVAL").await, EpicsValue::ULong(1));

    caput(&db, "RVAL", "4294967295").await.unwrap();
    assert_eq!(stored(&db, "RVAL").await, EpicsValue::ULong(u32::MAX));

    caput(&db, "RVAL", "-1").await.unwrap();
    assert_eq!(
        stored(&db, "RVAL").await,
        EpicsValue::ULong(u32::MAX),
        "negative into unsigned wraps to u32::MAX, as C strtoul does"
    );
}

/// The convert-skip is gated on UDF, exactly as C's `goto CONTINUE` is: once a
/// VAL put clears UDF, the next process cycle DOES run `convert()` and RVAL is
/// recomputed from VAL. Guards the fix against becoming an unconditional skip.
#[tokio::test]
async fn mbbo_rval_is_overwritten_by_convert_once_val_is_defined() {
    let db = db_with(Box::new(MbboRecord::new(0))).await;
    // Define VAL (clears UDF). VAL=0 on a bare mbbo -> convert() sets RVAL=0.
    caput(&db, "VAL", "0").await.unwrap();
    // Now a RVAL put IS clobbered: the process cycle runs convert() (UDF=0),
    // recomputing RVAL = VAL(=0).
    caput(&db, "RVAL", "5").await.unwrap();
    assert_eq!(
        stored(&db, "RVAL").await,
        EpicsValue::ULong(0),
        "with VAL defined, process() runs convert() and RVAL follows VAL"
    );
}
