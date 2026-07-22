//! acalcout IVOA=Set_output_to_IVOV fires on the RECORD's severity, whatever
//! raised it — not only on a CALC failure (R11-C15).
//!
//! C `aCalcoutRecord.c::execOutput` (:913-940) is reached from `afterCalc`
//! whenever the OOPT decision says the record outputs (:338-352) and from the
//! DLYA continuation (:425-428). Its one severity test is
//! `if (pcalc->nsev < INVALID_ALARM)` (:915) — the record's pending severity,
//! from ANY source: `recGblSetSevr(CALC_ALARM,...)` in `afterCalc` (:305), a
//! HIHI/LOLO limit at INVALID severity in `checkAlarms` (:868-880), UDF_ALARM
//! at UDFS=INVALID (:845-852), or an MS input link raising `nsev` inside
//! `fetch_values`/`dbGetLink`. Any of those with IVOA=Set_output_to_IVOV takes
//! `case menuIvoaSet_output_to_IVOV` and drives IVOV.
//!
//! The port gated its `apply_invalid_output_value` on the record-private
//! `calc_alarm` flag, i.e. on the calc failure alone, so an acalcout driven
//! INVALID by its own HIHI limit drove the CALCULATED value at
//! IVOA=Set_output_to_IVOV. Severity is the framework's — the record may not
//! re-derive it from a private flag.

// RTEMS-EXEC-MODEL-ALLOW(2): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{AlarmSeverity, FieldDesc, ProcessOutcome, Record};
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::types::EpicsValue;

/// OUT-link target that records the last VAL write.
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

/// A LIMIT alarm at INVALID severity — no calc failure anywhere — must still
/// take C's IVOA=Set_output_to_IVOV arm.
#[tokio::test]
async fn r11_c15_a_limit_driven_invalid_still_substitutes_ivov() {
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

    // CALC="10" succeeds (no CALC_ALARM). HIHI=5 with HHSV=INVALID(3) drives the
    // record INVALID through checkAlarms' limit arm. IVOA=2 (Set_output_to_IVOV),
    // IVOV=42, OOPT=Every_Time (default), ODLY=0 → output fires this cycle.
    // DOPT=Use OCAL: C's substitution is `pcalc->oval = pcalc->ivov` alone
    // (aCalcoutRecord.c:924), so IVOV is observable at OUT only through the
    // `&oval` buffer — the Use OCAL / scalar-target branch of
    // devaCalcoutSoft.c:87.
    let mut a = AcalcoutRecord::default();
    a.put_field("CALC", EpicsValue::String("10".into()))
        .unwrap();
    a.special("CALC", true).unwrap();
    a.put_field("DOPT", EpicsValue::Short(1)).unwrap();
    a.put_field("OCAL", EpicsValue::String("7".into())).unwrap();
    a.special("OCAL", true).unwrap();
    a.put_field("HIHI", EpicsValue::Double(5.0)).unwrap();
    a.put_field("HHSV", EpicsValue::Short(3)).unwrap();
    a.put_field("IVOA", EpicsValue::Short(2)).unwrap();
    a.put_field("IVOV", EpicsValue::Double(42.0)).unwrap();
    a.put_field("OUT", EpicsValue::String("PROBE".into()))
        .unwrap();
    db.add_record("AC", Box::new(a)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("AC", &mut v, 0).await.unwrap();

    let rec = db.get_record("AC").unwrap();
    let guard = rec.read();
    assert_eq!(
        guard.common.sevr,
        AlarmSeverity::Invalid,
        "HIHI=5 with HHSV=INVALID and VAL=10 drives the record INVALID"
    );
    drop(guard);

    assert_eq!(writes.load(Ordering::SeqCst), 1, "OUT fires once");
    assert_eq!(
        *last.lock().unwrap(),
        Some(vec![42.0]),
        "C execOutput:915 tests nsev, not the calc status — a limit-driven INVALID \
         with IVOA=Set_output_to_IVOV drives IVOV=42 (via OVAL), not OCAL's 7"
    );
}

/// The output decision remains the record's: OOPT=Never means C never reaches
/// `execOutput`, so the IVOV substitution must not run either (VAL/AVAL keep the
/// calculated value and nothing is written to OUT).
#[tokio::test]
async fn r11_c15_a_non_outputting_cycle_does_not_substitute_ivov() {
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

    // Same INVALID limit, but OOPT=Never (6).
    let mut a = AcalcoutRecord::default();
    a.put_field("CALC", EpicsValue::String("10".into()))
        .unwrap();
    a.special("CALC", true).unwrap();
    a.put_field("HIHI", EpicsValue::Double(5.0)).unwrap();
    a.put_field("HHSV", EpicsValue::Short(3)).unwrap();
    a.put_field("IVOA", EpicsValue::Short(2)).unwrap();
    a.put_field("IVOV", EpicsValue::Double(42.0)).unwrap();
    a.put_field("OOPT", EpicsValue::Short(6)).unwrap();
    a.put_field("OUT", EpicsValue::String("PROBE".into()))
        .unwrap();
    db.add_record("AC", Box::new(a)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("AC", &mut v, 0).await.unwrap();

    assert_eq!(writes.load(Ordering::SeqCst), 0, "OOPT=Never: no OUT write");
    let rec = db.get_record("AC").unwrap();
    let guard = rec.read();
    assert_eq!(
        guard.record.get_field("VAL"),
        Some(EpicsValue::Double(10.0)),
        "OOPT=Never never reaches execOutput, so IVOV is not substituted"
    );
}

/// `CALC_ALARM` is not a field of the C record (it appears in no `.dbd`); it was
/// a leftover of the R9-7 hack. A get of it must miss.
#[test]
fn r11_c15_calc_alarm_is_not_a_field() {
    let a = AcalcoutRecord::default();
    assert_eq!(
        a.get_field("CALC_ALARM"),
        None,
        "aCalcoutRecord.dbd declares no CALC_ALARM field"
    );
}
