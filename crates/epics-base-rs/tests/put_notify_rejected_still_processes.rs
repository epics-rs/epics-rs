//! Cause B: a put-NOTIFY whose conversion is rejected must still process.
//!
//! `caput -c REC.VAL 'notanumber'` — the value cannot be coerced to the field's
//! native type, so BOTH C and the port report the put FAILED to the client
//! (`put_accepted == False` on both). But C `db_put_process`
//! (db_access.c:1025-1043) ALWAYS `return 1` (didPut) even when the internal
//! `dbChannelPut` fails (it sets `ppn->status = notifyError` and still returns
//! 1), so `processNotifyCommon` (dbNotify.c:243-246) still runs `dbProcess`
//! when the gate passes. STAT/SEVR then recompute → NO_ALARM.
//!
//! The port previously returned `Err` from the conversion before the process
//! gate, leaving the record on its stale UDF/INVALID (~378 oracle cases).
//!
//! CRITICAL — notify path ONLY. A plain `ca_put` (`want_notify == false`) MUST
//! keep the current behavior: return `Err` and process NOTHING, matching C
//! `dbPutField` (dbAccess.c:1263 processes only when `dbPut` status==0).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::*;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// Passive record: `VAL` is `pp(TRUE)` and `DBF_DOUBLE`, so a text value that
/// cannot parse as a number is rejected by the conversion. `process()` bumps a
/// counter so the test can say whether the rejected put still processed.
struct CountingRecord {
    val: f64,
    process_count: Arc<AtomicU32>,
}

static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

impl Record for CountingRecord {
    fn record_type(&self) -> &'static str {
        "counting"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.process_count.fetch_add(1, Ordering::Relaxed);
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::Double(v) => {
                    self.val = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }

    fn process_passive_fields(&self) -> &'static [&'static str] {
        &["VAL"]
    }
}

async fn record_with(count: Arc<AtomicU32>) -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.add_record(
        "REC",
        Box::new(CountingRecord {
            val: 0.0,
            process_count: count,
        }),
    )
    .await
    .unwrap();
    db
}

/// The bad value cannot be coerced to `DBF_DOUBLE`, so `dbput_request` rejects
/// it before any field write.
const BAD: fn() -> EpicsValue = || EpicsValue::String("notanumber".into());

/// Notify path: the rejected conversion still drives exactly one process cycle,
/// and the client still receives the failure.
#[epics_macros_rs::epics_test]
async fn rejected_notify_put_still_processes() {
    let count = Arc::new(AtomicU32::new(0));
    let db = record_with(count.clone()).await;

    let result = db.put_record_field_from_ca("REC", "VAL", BAD()).await;

    assert!(
        result.is_err(),
        "the conversion is rejected — the client is told the put FAILED"
    );
    assert_eq!(
        count.load(Ordering::Relaxed),
        1,
        "C db_put_process returns didPut=1 despite the failure, so the record still processes"
    );
    assert_eq!(
        db.get_pv("REC.VAL").unwrap(),
        EpicsValue::Double(0.0),
        "no field was written — dbChannelPut wrote nothing on the rejected conversion"
    );
}

/// Plain `ca_put` path: the rejected conversion returns Err and processes
/// NOTHING — the notify-only fix must not touch the fire-and-forget path.
#[epics_macros_rs::epics_test]
async fn rejected_plain_put_does_not_process() {
    let count = Arc::new(AtomicU32::new(0));
    let db = record_with(count.clone()).await;

    let result = db
        .put_record_field_from_ca_no_notify("REC", "VAL", BAD())
        .await;

    assert!(result.is_err(), "the conversion is rejected");
    assert_eq!(
        count.load(Ordering::Relaxed),
        0,
        "C dbPutField processes only when dbPut status==0: a rejected plain put drives no cycle"
    );
}

/// Sanity: an ACCEPTED put on the notify path processes exactly once and writes
/// the value — the fix only adds a cycle on the FAILING path.
#[epics_macros_rs::epics_test]
async fn accepted_notify_put_processes_and_writes() {
    let count = Arc::new(AtomicU32::new(0));
    let db = record_with(count.clone()).await;

    db.put_record_field_from_ca("REC", "VAL", EpicsValue::Double(3.5))
        .await
        .expect("a valid put is accepted");

    assert_eq!(
        count.load(Ordering::Relaxed),
        1,
        "the accepted put processes once"
    );
    assert_eq!(
        db.get_pv("REC.VAL").unwrap(),
        EpicsValue::Double(3.5),
        "and the value landed"
    );
}
