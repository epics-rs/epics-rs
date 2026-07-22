//! scalcout IVOA=Set_to_IVOV drives OVAL (not VAL) on a calc-fail cycle.
//!
//! C `sCalcoutRecord.c` execOutput (lines 786-808): on a `nsev >= INVALID`
//! cycle, IVOA=Set_to_IVOV sets `pcalc->oval = pcalc->ivov` (line 798) and
//! writes OVAL — it never touches VAL. VAL stays at the calc-fail sentinel
//! `-1` (line 361). The Rust record used to set `self.val = ivov` in-record,
//! clobbering VAL and duplicating the OVAL=IVOV write the framework's IVOA
//! gate already performs.
//!
//! The framework path is live for scalcout because `evaluate_alarms`
//! (record_instance.rs:1653) raises CALC_ALARM/INVALID from the CALC_ALARM
//! field, so `sevr == INVALID` and `apply_invalid_output_value` runs.

// RTEMS-EXEC-MODEL-ALLOW(1): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;

/// calc-fail + IVOA=Set_to_IVOV + output due: OVAL = IVOV, VAL = -1 (sentinel).
#[tokio::test]
async fn scalcout_ivov_drives_oval_not_val_on_calc_fail() {
    let db = PvDatabase::new();

    let mut sc = ScalcoutRecord::default();
    // A stack-underflow CALC fails at eval → CALC_ALARM → INVALID severity.
    sc.put_field("CALC", EpicsValue::String("+".into()))
        .unwrap();
    sc.special("CALC", true).unwrap();
    sc.dopt = 0; // Use_VAL
    sc.oopt = 0; // Every_Time: output is due.
    sc.put_field("IVOA", EpicsValue::Short(2)).unwrap(); // Set_to_IVOV
    sc.put_field("IVOV", EpicsValue::Double(99.0)).unwrap();
    db.add_record("SC_IVOV", Box::new(sc)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SC_IVOV", &mut visited, 0)
        .await
        .unwrap();

    let rec = db.get_record("SC_IVOV").unwrap();
    let inst = rec.read();

    // Precondition: the cycle is INVALID and output is due — the IVOA trigger.
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "broken CALC drives the cycle INVALID (CALC_ALARM via evaluate_alarms)"
    );
    assert!(inst.record.should_output(), "Every_Time always outputs");

    // OVAL = IVOV (the framework's IVOA=Set_to_IVOV write, C:798).
    assert_eq!(
        inst.record.get_field("OVAL"),
        Some(EpicsValue::Double(99.0)),
        "IVOA=Set_to_IVOV drives OVAL = IVOV = 99"
    );
    // VAL stays at the calc-fail sentinel -1 — NOT clobbered to IVOV.
    assert_eq!(
        inst.record.get_field("VAL"),
        Some(EpicsValue::Double(-1.0)),
        "IVOA=Set_to_IVOV must leave VAL at the calc-fail sentinel -1 (C:361), \
         not set it to IVOV"
    );
}
