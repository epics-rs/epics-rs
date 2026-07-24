//! Cause A: a put to the `dbCommon` `UDF` field must drive processing.
//!
//! C `processNotifyCommon` (dbNotify.c:243-246) and `dbPutField`
//! (dbAccess.c:1263-1268) process a record on a put when the field is `PROC`
//! **or** it is `pp(TRUE)` and `SCAN == Passive`. `PROC` and `UDF` are the ONLY
//! two `dbCommon` `pp(TRUE)` fields (`dbCommon.dbd.pod`: PROC line 243, UDF line
//! 552). `UDF` is an ordinary `pp` field — it processes only on a Passive
//! record, unlike `PROC`'s force-process on any SCAN.
//!
//! Oracle symptom: `caput -c REC.UDF <v>` ends STAT/SEVR = NO_ALARM in C
//! (the record processed and recomputed its alarms); the port kept the stale
//! UDF/INVALID because the process-decision gate special-cased PROC and omitted
//! UDF, so a UDF put stored the field but drove no process cycle.

// RTEMS-EXEC-MODEL-ALLOW(3): checked - these run and pass in the feature-ON suite.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::*;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// Minimal Passive record: `VAL` is `pp(TRUE)`, `process()` bumps a counter so a
/// test can say exactly how many cycles a put drove.
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
            // UDF/DESC/etc. are `dbCommon` fields — the framework's
            // `put_common_field` owns them, so defer via `FieldNotFound`.
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

/// A put to `UDF` on a Passive record drives exactly one process cycle — the
/// `dbCommon` `pp(TRUE)` field that the gate previously omitted.
#[tokio::test]
async fn put_to_udf_drives_processing() {
    let count = Arc::new(AtomicU32::new(0));
    let db = record_with(count.clone()).await;

    db.put_record_field_from_ca_no_notify("REC", "UDF", EpicsValue::Char(0))
        .await
        .expect("a UDF put is accepted");

    assert_eq!(
        count.load(Ordering::Relaxed),
        1,
        "UDF is a dbCommon pp(TRUE) field: a put to it processes the Passive record"
    );
}

/// The `-c` (callback / put-notify) route drives the same cycle — the gate is
/// shared between the plain and notify paths.
#[tokio::test]
async fn put_notify_to_udf_drives_processing() {
    let count = Arc::new(AtomicU32::new(0));
    let db = record_with(count.clone()).await;

    db.put_record_field_from_ca("REC", "UDF", EpicsValue::Char(0))
        .await
        .expect("a UDF put-notify is accepted");

    assert_eq!(
        count.load(Ordering::Relaxed),
        1,
        "caput -c REC.UDF processes the record, matching C ending STAT/SEVR=NO_ALARM"
    );
}

/// Boundary: the fix adds UDF specifically, not a blanket "every dbCommon put
/// processes". A put to a non-`pp` common field (DESC) drives NO cycle.
#[tokio::test]
async fn put_to_a_non_pp_common_field_does_not_process() {
    let count = Arc::new(AtomicU32::new(0));
    let db = record_with(count.clone()).await;

    db.put_record_field_from_ca_no_notify("REC", "DESC", EpicsValue::String("hello".into()))
        .await
        .expect("a DESC put is accepted");

    assert_eq!(
        count.load(Ordering::Relaxed),
        0,
        "DESC is not pp(TRUE): a put to it stores the field but drives no process"
    );
}
