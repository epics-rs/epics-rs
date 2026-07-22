//! scalcout IVOA=Don't_drive suppresses the OUT write on ANY INVALID cycle,
//! not just a calc-fail one.
//!
//! C `sCalcoutRecord.c` execOutput (lines 780-795): when `nsev >= INVALID`
//! and `IVOA == menuIvoaDon_t_drive_outputs`, execOutput hits `break` (line
//! 794) — the OUT-link `writeValue` never runs — for EVERY INVALID source,
//! not only a failed sCalcPerform. The Rust record's in-record veto keyed on
//! the `calc_failed` proxy, so a non-calc-fail INVALID (NaN-VAL UDF, INP
//! LINK_ALARM, SIMM, …) left `cached_should_output == true` and the generic
//! `multi_output_links` writeback drove OUT where C suppresses. The framework
//! §4.6 dispatch now applies the same IVOA=Don't_drive gate the single-OUT
//! `skip_out` path already enforced, closing the family for all INVALID
//! sources (and for both `scalcout` and `acalcout`).

// RTEMS-EXEC-MODEL-ALLOW(1): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;

/// Non-calc-fail INVALID (NaN VAL → UDF) + IVOA=Don't_drive + output due:
/// the OUT link must NOT be written.
#[tokio::test]
async fn scalcout_dont_drive_suppresses_out_on_noncalc_invalid() {
    let db = PvDatabase::new();

    // OUT target seeded with a sentinel the suppressed write must not overwrite.
    db.add_record("SC_TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    let mut sc = ScalcoutRecord::default();
    // CALC="0/0" → VAL = NaN → UDF → INVALID severity, but the calc itself
    // evaluated cleanly (calc_failed == false) — this is the non-calc-fail
    // INVALID source the in-record veto did NOT cover.
    sc.put_field("CALC", EpicsValue::String("0/0".into()))
        .unwrap();
    sc.special("CALC", true).unwrap();
    // DOPT=Use_OVAL with a clean OCAL so OVAL=5 is a defined value, decoupling
    // the suppression check from the NaN VAL.
    sc.put_field("DOPT", EpicsValue::Short(1)).unwrap();
    sc.put_field("OCAL", EpicsValue::String("5".into()))
        .unwrap();
    sc.special("OCAL", true).unwrap();
    sc.oopt = 0; // Every_Time → output is due.
    sc.put_field("IVOA", EpicsValue::Short(1)).unwrap(); // Don't_drive_outputs
    sc.put_field("OUT", EpicsValue::String("SC_TGT".into()))
        .unwrap();
    db.add_record("SC_DD", Box::new(sc)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("SC_DD", &mut visited, 0)
        .await
        .unwrap();

    let sc_rec = db.get_record("SC_DD").unwrap();
    {
        let sc_inst = sc_rec.read();

        // Preconditions: INVALID cycle, output is due, OVAL is the defined 5 — so
        // the only thing standing between OVAL and the target is the IVOA gate.
        assert_eq!(
            sc_inst.common.sevr,
            AlarmSeverity::Invalid,
            "NaN VAL must drive the cycle INVALID via UDF (a non-calc-fail source)"
        );
        assert!(
            sc_inst.record.should_output(),
            "Every_Time always requests output"
        );
        assert_eq!(
            sc_inst.record.get_field("OVAL"),
            Some(EpicsValue::Double(5.0)),
            "OCAL=5 under DOPT=Use_OVAL computes OVAL=5 regardless of the IVOA gate"
        );
    }

    // The gate: IVOA=Don't_drive on an INVALID cycle suppresses the OUT write,
    // so the target keeps its 0.0 sentinel — it is NOT driven to OVAL=5.
    let tgt = db.get_record("SC_TGT").unwrap();
    assert_eq!(
        tgt.read().record.get_field("VAL"),
        Some(EpicsValue::Double(0.0)),
        "IVOA=Don't_drive must suppress the OUT write on a non-calc-fail \
         INVALID cycle (C execOutput nsev>=INVALID → break, sCalcoutRecord.c:794)"
    );
}
