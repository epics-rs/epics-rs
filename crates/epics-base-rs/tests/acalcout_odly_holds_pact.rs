//! acalcout ODLY defers the OUT write and holds PACT across the delay.
//!
//! C `aCalcoutRecord.c::process`/`afterCalc` (calc/calcApp/src): when an output
//! is due and `ODLY > 0`, afterCalc sets `dlya=1`, posts it, schedules the
//! delayed `doOutCb`, and `return(ASYNC)` with `pact` still TRUE (lines
//! 338-346) — the record stays ACTIVE across the delay, so a concurrent
//! `dbProcess` bails; the delayed callback re-enters (`pact==TRUE`, `dlya`
//! branch, lines 421-430), clears DLYA/pact, and runs `execOutput`.
//!
//! Before the port acalcout wrote OUT synchronously (ODLY/DLYA were inert), so
//! the OUT write fired immediately instead of after the delay, and nothing held
//! PACT. The port now defers via an async-pending-notify pass (DLYA=1) +
//! `ReprocessAfter`, and the framework holds PACT for that notify (because it
//! carries a `ReprocessAfter`) — so a foreign `dbProcess` during the delay
//! bails at the PACT entry guard instead of firing the deferred OUT early.

// RTEMS-EXEC-MODEL-ALLOW(3): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record};
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::EpicsValue;

/// OUT-link target that records each VAL write (count + last array), so the
/// test can assert exactly when acalcout's deferred OUT fires. A bare DB OUT
/// link is NPP: the framework calls `put_field` but never `process`.
struct OutProbe {
    writes: Arc<AtomicUsize>,
    last: Arc<Mutex<Option<Vec<f64>>>>,
}

impl Record for OutProbe {
    fn record_type(&self) -> &'static str {
        "acalcout_out_probe"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(0.0)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        if name == "VAL" {
            self.writes.fetch_add(1, Ordering::SeqCst);
            let v = match value {
                EpicsValue::DoubleArray(a) => a,
                EpicsValue::Double(d) => vec![d],
                _ => Vec::new(),
            };
            *self.last.lock().unwrap() = Some(v);
        }
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }
}

#[tokio::test]
async fn acalcout_odly_holds_pact_foreign_process_does_not_fire_early() {
    let db = PvDatabase::new();

    let writes = Arc::new(AtomicUsize::new(0));
    let last = Arc::new(Mutex::new(None));
    db.add_record(
        "PROBE",
        Box::new(OutProbe {
            writes: writes.clone(),
            last: last.clone(),
        }),
    )
    .await
    .unwrap();

    // acalcout: CALC="42" → VAL=42, AVAL=[42] (NELM=1 default); OOPT=Every_Time
    // (default 0) → output due; ODLY=100s (the real timer cannot fire within the
    // test); OUT→PROBE (writes AVAL).
    let mut a = AcalcoutRecord::default();
    a.put_field("CALC", EpicsValue::String("42".into()))
        .unwrap();
    a.special("CALC", true).unwrap();
    a.put_field("ODLY", EpicsValue::Double(100.0)).unwrap();
    a.put_field("OUT", EpicsValue::String("PROBE".into()))
        .unwrap();
    db.add_record("AC", Box::new(a)).await.unwrap();

    // Delaying cycle: ODLY>0 defers, sets DLYA=1, OUT not written.
    let mut v1 = HashSet::new();
    db.process_record_with_links("AC", &mut v1, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_record("AC").unwrap().read().record.get_field("DLYA"),
        Some(EpicsValue::UShort(1)),
        "ODLY>0 cycle sets DLYA and defers"
    );
    assert_eq!(
        writes.load(Ordering::SeqCst),
        0,
        "OUT deferred on the ODLY delaying cycle — no write yet"
    );

    // Foreign dbProcess DURING the delay (is_continuation=false): must BAIL at
    // the PACT entry guard, NOT re-enter process() while dlya==1 and fire the
    // deferred OUT early.
    let mut v2 = HashSet::new();
    db.process_record_with_links("AC", &mut v2, 0)
        .await
        .unwrap();
    assert_eq!(
        writes.load(Ordering::SeqCst),
        0,
        "PACT held: a foreign dbProcess during the ODLY delay must NOT fire the \
         deferred OUT early (C aCalcoutRecord.c holds pact across the delay)"
    );

    // Continuation (bypasses the PACT guard): fires the deferred output once.
    let mut v3 = HashSet::new();
    db.process_record_continuation("AC", &mut v3, 0)
        .await
        .unwrap();
    assert_eq!(
        writes.load(Ordering::SeqCst),
        1,
        "continuation fires the deferred OUT exactly once"
    );
    assert_eq!(
        *last.lock().unwrap(),
        Some(vec![42.0]),
        "continuation writes AVAL=[42] to OUT after the ODLY delay"
    );
    assert_eq!(
        db.get_record("AC").unwrap().read().record.get_field("DLYA"),
        Some(EpicsValue::UShort(0)),
        "continuation clears DLYA"
    );
}

/// D1: IVOA=Set_output_to_IVOV substitution must run on the ODLY *continuation*
/// (C `aCalcoutRecord.c` execOutput line 924, reached on the continuation at
/// line 428), NOT on the delaying cycle. A direct get of VAL during the ODLY
/// window must still show the calc-fail value, not IVOV.
#[tokio::test]
async fn acalcout_odly_ivov_substitutes_on_continuation_not_delaying_cycle() {
    let db = PvDatabase::new();

    let writes = Arc::new(AtomicUsize::new(0));
    let last = Arc::new(Mutex::new(None));
    db.add_record(
        "PROBE",
        Box::new(OutProbe {
            writes: writes.clone(),
            last: last.clone(),
        }),
    )
    .await
    .unwrap();

    // acalcout: CALC="1e300*1e300" → non-finite → calc_failed → INVALID; IVOA=2
    // (Set_output_to_IVOV), IVOV=99; OOPT=Every → output due; ODLY=100; OUT→PROBE.
    // R8-7: this used to be "1/0" on the belief that the array engine divides in
    // IEEE. C answers myMAXFLOAT/st=0 for aCalc `1/0` (aCalcPerform.c:690-696),
    // so it cannot drive the INVALID cycle this case needs; `1e300*1e300` is the
    // C-verified non-finite one (compiled aCalcPerform: st=-1 d=inf).
    // DOPT=Use OCAL (W10-E3): C's substitution is `oval = ivov` alone
    // (aCalcoutRecord.c:924), observable at OUT only through the `&oval`
    // buffer a scalar target selects (devaCalcoutSoft.c:87).
    let mut a = AcalcoutRecord::default();
    a.put_field("CALC", EpicsValue::String("1e300*1e300".into()))
        .unwrap();
    a.special("CALC", true).unwrap();
    a.put_field("DOPT", EpicsValue::Short(1)).unwrap();
    a.put_field("OCAL", EpicsValue::String("3".into())).unwrap();
    a.special("OCAL", true).unwrap();
    a.put_field("IVOA", EpicsValue::Short(2)).unwrap();
    a.put_field("IVOV", EpicsValue::Double(99.0)).unwrap();
    a.put_field("ODLY", EpicsValue::Double(100.0)).unwrap();
    a.put_field("OUT", EpicsValue::String("PROBE".into()))
        .unwrap();
    db.add_record("AC", Box::new(a)).await.unwrap();

    // Delaying cycle: IVOA=Set + OOPT-fires + ODLY>0 still defers (DLYA=1), and
    // IVOV must NOT be substituted into VAL yet.
    let mut v1 = HashSet::new();
    db.process_record_with_links("AC", &mut v1, 0)
        .await
        .unwrap();
    {
        let rec = db.get_record("AC").unwrap();
        let guard = rec.read();
        assert_eq!(
            guard.record.get_field("DLYA"),
            Some(EpicsValue::UShort(1)),
            "IVOA=Set + OOPT-fires + ODLY>0 must still defer"
        );
        assert_ne!(
            guard.record.get_field("OVAL"),
            Some(EpicsValue::Double(99.0)),
            "IVOV must NOT be substituted on the ODLY delaying cycle (C substitutes \
             oval=ivov inside execOutput, on the continuation)"
        );
    }
    assert_eq!(writes.load(Ordering::SeqCst), 0, "OUT deferred");

    // Continuation: the framework IVOA dispatch substitutes IVOV and OUT fires
    // with it.
    let mut v3 = HashSet::new();
    db.process_record_continuation("AC", &mut v3, 0)
        .await
        .unwrap();
    assert_eq!(
        writes.load(Ordering::SeqCst),
        1,
        "OUT fires once on the continuation"
    );
    assert_eq!(
        *last.lock().unwrap(),
        Some(vec![99.0]),
        "continuation writes IVOV=99 to OUT (substituted on the continuation, not early)"
    );
}

/// D2: IVOA=Don't_drive must NOT cancel the ODLY defer. C `aCalcoutRecord.c`
/// gates the defer on the OOPT-only `doOutput` (afterCalc:338); the Don't_drive
/// veto is checked separately inside `execOutput` (:920-921) and removes only
/// the OUT write. So with Don't_drive + OOPT-fires + ODLY>0 the record still
/// pulses DLYA and holds PACT across the delay; the OUT write simply never
/// fires (delaying cycle or continuation).
#[tokio::test]
async fn acalcout_odly_dont_drive_still_defers() {
    let db = PvDatabase::new();

    let writes = Arc::new(AtomicUsize::new(0));
    let last = Arc::new(Mutex::new(None));
    db.add_record(
        "PROBE",
        Box::new(OutProbe {
            writes: writes.clone(),
            last: last.clone(),
        }),
    )
    .await
    .unwrap();

    // acalcout: CALC="1e300*1e300" → non-finite → calc_failed → INVALID; IVOA=1
    // (Don't_drive); OOPT=Every → output due; ODLY=100; OUT→PROBE.
    // R8-7: was "1/0", which C evaluates to myMAXFLOAT with st=0 — see the
    // sibling case above.
    let mut a = AcalcoutRecord::default();
    a.put_field("CALC", EpicsValue::String("1e300*1e300".into()))
        .unwrap();
    a.special("CALC", true).unwrap();
    a.put_field("IVOA", EpicsValue::Short(1)).unwrap();
    a.put_field("ODLY", EpicsValue::Double(100.0)).unwrap();
    a.put_field("OUT", EpicsValue::String("PROBE".into()))
        .unwrap();
    db.add_record("AC", Box::new(a)).await.unwrap();

    // Delaying cycle: must STILL defer (DLYA=1) even though the OUT write is
    // vetoed — C gates the defer on the OOPT decision, not IVOA.
    let mut v1 = HashSet::new();
    db.process_record_with_links("AC", &mut v1, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_record("AC").unwrap().read().record.get_field("DLYA"),
        Some(EpicsValue::UShort(1)),
        "IVOA=Don't_drive + OOPT-fires + ODLY>0 must STILL defer (C gates the \
         defer on doOutput, the Don't_drive veto is inside execOutput)"
    );
    assert_eq!(
        writes.load(Ordering::SeqCst),
        0,
        "OUT not written on the delaying cycle"
    );

    // Continuation: completes (DLYA cleared); Don't_drive suppresses the OUT
    // write, so it never fires.
    let mut v3 = HashSet::new();
    db.process_record_continuation("AC", &mut v3, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_record("AC").unwrap().read().record.get_field("DLYA"),
        Some(EpicsValue::UShort(0)),
        "continuation clears DLYA"
    );
    assert_eq!(
        writes.load(Ordering::SeqCst),
        0,
        "IVOA=Don't_drive: OUT never written, even on the continuation"
    );
}
