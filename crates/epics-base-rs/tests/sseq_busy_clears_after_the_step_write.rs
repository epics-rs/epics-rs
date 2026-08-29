//! `SSEQ.BUSY` reads 0 only once the step's `LNKn` write has landed.
//!
//! C `sseqRecord.c` does the put and the clear under one `dbScanLock`:
//! `processCallback` writes `LNKn` (`:714-792`) and `asyncFinish` clears
//! `busy` (`:498-505`), both inside the lock `dbProcess` holds for the record.
//! `dbGetField` takes that lock, so a `caget SSEQ.BUSY` returns either before
//! the whole region or after it — never `BUSY == 0` with `LNKn` unwritten.
//!
//! A no-wait step's put is a queued [`ProcessAction::WriteDbLink`]
//! (`sseq.rs::fire_current_step`) that the framework executes after
//! `process()` returns, so the clear had to stop being a `process()` store:
//! `finish` emits the whole `busy`/`abort`/`aborting`/`waiting` group as
//! `ProcessOutcome::post_write_fields` and the drain applies it once the
//! writes have run.
//!
//! The reader here is the step's own target: it samples `SS_ORD.BUSY` from
//! inside the put the step is making, which is the one instant a `caget` could
//! land between `process()` and the drain. `dbGetField` and this sample answer
//! from the same `get_field("BUSY")`.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::*;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// No sample taken yet — distinct from every value `BUSY` can hold.
const UNSAMPLED: i32 = -1;

/// The step's `LNKn` target. Reads `SS_ORD.BUSY` while the step's put is being
/// made to it.
struct BusyProbe {
    val: f64,
    db: PvDatabase,
    observed: Arc<AtomicI32>,
}

static PROBE_FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

impl Record for BusyProbe {
    fn record_type(&self) -> &'static str {
        "busyprobe"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
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
            "VAL" => {
                let busy = self
                    .db
                    .get_record("SS_ORD")
                    .and_then(|r| r.read().record.get_field("BUSY"))
                    .and_then(|v| v.to_f64())
                    .map(|v| v as i32)
                    .unwrap_or(UNSAMPLED);
                self.observed.store(busy, Ordering::SeqCst);
                self.val = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("VAL".into()))?;
                Ok(())
            }
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        PROBE_FIELDS
    }
}

#[epics_macros_rs::epics_test]
async fn busy_is_still_set_while_the_step_write_is_being_made() {
    let db = PvDatabase::new();
    let observed = Arc::new(AtomicI32::new(UNSAMPLED));

    db.add_record(
        "SS_ORD_TGT",
        Box::new(BusyProbe {
            val: 0.0,
            db: db.clone(),
            observed: observed.clone(),
        }),
    )
    .await
    .unwrap();

    // One no-wait step: `fire_current_step` queues the `LNK1` write and
    // `finish` runs in the same `process()` call — the whole window.
    let mut sseq = SseqRecord::new();
    sseq.put_field("SELM", EpicsValue::Short(0)).unwrap(); // All
    sseq.put_field("DO1", EpicsValue::Double(33.0)).unwrap();
    sseq.put_field("LNK1", EpicsValue::String("SS_ORD_TGT".into()))
        .unwrap();
    db.add_record("SS_ORD", Box::new(sseq)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SS_ORD", &mut visited, 0)
        .await
        .unwrap();

    for _ in 0..400 {
        if matches!(db.get_pv("SS_ORD.BUSY"), Ok(EpicsValue::Short(0))) {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }

    assert_eq!(
        db.get_pv("SS_ORD_TGT.VAL").unwrap(),
        EpicsValue::Double(33.0),
        "the step must write its target"
    );
    assert_eq!(
        observed.load(Ordering::SeqCst),
        1,
        "BUSY must still read 1 while the step's put is being made — C clears \
         it in `asyncFinish` under the same `dbScanLock` as the `dbPutLink`"
    );
    assert_eq!(
        db.get_pv("SS_ORD.BUSY").unwrap(),
        EpicsValue::Short(0),
        "and the sequence must then finish"
    );
}
