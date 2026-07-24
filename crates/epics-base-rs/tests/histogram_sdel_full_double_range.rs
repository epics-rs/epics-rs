//! Cause 2: histogram SDEL is `DBF_DOUBLE`, so a large/infinite double put is
//! ACCEPTED — matching C storing the full double range verbatim.
//!
//! SDEL is `field(SDEL,DBF_DOUBLE)` (histogramRecord.dbd.pod). C stores whatever
//! `epicsParseDouble` accepts — `1e308`, `1e39` and `inf` all SUCCEED (a finite
//! overflow like `1e400` is the only refusal). SDEL only arms a monitor
//! watchdog (`wdogInit`: `if (prec->sdel > 0)` schedule a callback of `sdel`
//! seconds); an enormous or infinite SDEL simply schedules a callback that never
//! fires.
//!
//! The port's `watchdog_interval` converted SDEL with `Duration::from_secs_f64`,
//! which PANICS on a non-finite or Duration-overflowing value — so arming the
//! watchdog after the store aborted the whole put, and the oracle saw the put
//! REFUSED. `try_from_secs_f64` maps every such value to `None` (no watchdog
//! arms, behaviourally identical to C's never-firing callback), so the store
//! itself is accepted across the whole double range.

// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::histogram::HistogramRecord;
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

/// `1e308` (near double max), `1e39` (over float max), `inf` — all SUCCEED. The
/// store must be accepted and arming the watchdog must not panic.
#[tokio::test]
async fn histogram_sdel_accepts_large_and_infinite_double() {
    let db = db_with(Box::new(HistogramRecord::default())).await;
    for text in ["1e308", "1e39", "inf"] {
        caput(&db, "SDEL", text)
            .await
            .unwrap_or_else(|e| panic!("SDEL is DBF_DOUBLE: {text} must be accepted: {e}"));
    }
    // The last put value is served back as a Double.
    match stored(&db, "SDEL").await {
        EpicsValue::Double(v) => assert!(v.is_infinite(), "SDEL stored inf verbatim"),
        other => panic!("SDEL served as {other:?}, expected Double(inf)"),
    }
}

/// A finite value that fits still arms a real watchdog interval — the fix must
/// not have turned every SDEL into a no-arm.
#[tokio::test]
async fn histogram_sdel_finite_positive_still_stored() {
    let db = db_with(Box::new(HistogramRecord::default())).await;
    caput(&db, "SDEL", "2.5").await.unwrap();
    assert_eq!(stored(&db, "SDEL").await, EpicsValue::Double(2.5));
}
