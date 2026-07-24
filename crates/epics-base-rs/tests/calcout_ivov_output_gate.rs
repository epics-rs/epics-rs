//! calcout IVOA=Set_to_IVOV must clobber OVAL only on an *output* cycle.
//!
//! C `calcoutRecord.c`: the `oval = ivov` substitution lives inside
//! `execOutput` (calcoutRecord.c:646), which `process` calls ONLY under the
//! `if (doOutput)` gate (calcoutRecord.c:276). So when the OOPT condition is
//! not met (`doOutput == 0`) on an INVALID cycle, C leaves OVAL at its
//! retained value. The Rust framework IVOA=2 block called
//! `apply_invalid_output_value` whenever `sevr == INVALID && ivoa == 2`,
//! independent of `should_output()`, clobbering OVAL→IVOV and posting a
//! spurious OVAL monitor on a non-output cycle (D3). The fix gates the OVAL
//! write on the record's `cached_should_output`.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::calcout::CalcoutRecord;
use epics_base_rs::types::EpicsValue;

/// Non-output INVALID cycle: OVAL must NOT become IVOV.
#[epics_macros_rs::epics_test]
async fn calcout_ivov_not_applied_on_non_output_cycle() {
    let db = PvDatabase::new();

    let mut co = CalcoutRecord::default();
    // CALC="0/0" → VAL = NaN → UDF → INVALID severity.
    co.put_field("CALC", EpicsValue::String("0/0".into()))
        .unwrap();
    co.special("CALC", true).unwrap();
    co.dopt = 0; // Use_VAL
    co.oopt = 2; // When_Zero: doOutput = (VAL == 0); NaN != 0 → NO output.
    co.put_field("IVOA", EpicsValue::Short(2)).unwrap(); // Set_to_IVOV
    co.put_field("IVOV", EpicsValue::Double(99.0)).unwrap();
    db.add_record("CO_NOOUT", Box::new(co)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("CO_NOOUT", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("CO_NOOUT").unwrap();
    let inst = rec.read();

    // Precondition: this is genuinely an INVALID, non-output cycle.
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "NaN VAL must drive the cycle INVALID (UDF) — the IVOA block's trigger"
    );
    assert!(
        !inst.record.should_output(),
        "When_Zero with a NaN VAL must NOT request output (doOutput == 0)"
    );

    // The fix: OVAL stays at its retained default (0.0), NOT clobbered to IVOV.
    assert_eq!(
        inst.record.get_field("OVAL"),
        Some(EpicsValue::Double(0.0)),
        "C execOutput (oval=ivov) runs only under doOutput; a non-output \
         INVALID cycle must leave OVAL untouched, not set it to IVOV=99"
    );
}

/// Control: when output IS due, IVOA=Set_to_IVOV still clobbers OVAL→IVOV.
/// Pins that the gate suppresses only non-output cycles, not all of them.
#[epics_macros_rs::epics_test]
async fn calcout_ivov_applied_on_output_cycle() {
    let db = PvDatabase::new();

    let mut co = CalcoutRecord::default();
    co.put_field("CALC", EpicsValue::String("0/0".into()))
        .unwrap();
    co.special("CALC", true).unwrap();
    co.dopt = 0; // Use_VAL
    co.oopt = 0; // Every_Time: doOutput = 1 always.
    co.put_field("IVOA", EpicsValue::Short(2)).unwrap(); // Set_to_IVOV
    co.put_field("IVOV", EpicsValue::Double(99.0)).unwrap();
    db.add_record("CO_OUT", Box::new(co)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("CO_OUT", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("CO_OUT").unwrap();
    let inst = rec.read();

    assert_eq!(inst.common.sevr, AlarmSeverity::Invalid);
    assert!(inst.record.should_output(), "Every_Time always outputs");
    assert_eq!(
        inst.record.get_field("OVAL"),
        Some(EpicsValue::Double(99.0)),
        "on an output cycle IVOA=Set_to_IVOV must drive OVAL = IVOV = 99"
    );
}
