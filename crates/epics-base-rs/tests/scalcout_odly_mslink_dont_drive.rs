//! scalcout ODLY continuation carries a non-persistent (INP MS-link) INVALID
//! severity across the delay, so IVOA=Don't_drive still suppresses OUT on the
//! delayed cycle — matching C, and confirming the §4.6 IVOA gate covers the
//! ODLY path.
//!
//! C `sCalcoutRecord.c`: the ODLY delaying cycle returns before
//! `recGblResetAlarms` (process 399-432), so the pending `nsev` set by the
//! cycle-1 input read is carried to the delayed (`pact`) cycle, whose
//! `execOutput` reads it. A Don't_drive + `nsev>=INVALID` delayed cycle hits
//! `break` (sCalcoutRecord.c:794) and never writes OUT.
//!
//! The Rust framework mirrors this: the delaying cycle returns `AsyncPending`
//! before `rec_gbl_reset_alarms` (so `common.sevr` is still NoAlarm mid-delay
//! while the carried `nsev` holds INVALID), and the continuation commits the
//! carried `nsev` to `sevr==INVALID`, at which point the §4.6
//! `multi_output_links` IVOA gate suppresses the OUT write. This is exactly the
//! corner R50 flagged for a closer look — verified non-divergent: the carried
//! severity is NOT lost, and neutralizing the gate drives the target to OVAL,
//! proving the suppression is the gate's doing.

// RTEMS-EXEC-MODEL-ALLOW(1): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;

#[tokio::test]
async fn scalcout_odly_mslink_invalid_dont_drive_suppresses_out() {
    let db = PvDatabase::new();

    // SRC: ai VAL=200 over HIHI=100 / HHSV=INVALID → INVALID severity with a
    // FINITE value — so SC's own udf/calc stay clean and the ONLY INVALID
    // source on SC is the MS link (a non-persistent severity not re-read on
    // the ODLY continuation).
    db.add_record("SRC", Box::new(AiRecord::new(200.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("SRC").unwrap();
        let mut inst = rec.write();
        inst.put_common_field("HIHI", EpicsValue::Double(100.0))
            .unwrap();
        inst.put_common_field("HHSV", EpicsValue::Short(AlarmSeverity::Invalid as i16))
            .unwrap();
    }
    // OUT target seeded 0.0 — must not be driven to OVAL while suppressed.
    db.add_record("TGT", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    // SC: CALC="5" (VAL=5, finite, ignores A); INPA="SRC MS" pulls SRC's
    // INVALID severity via MS; ODLY large so the real timer cannot fire;
    // IVOA=Don't_drive; OOPT=Every → output due; OUT→TGT.
    let mut sc = ScalcoutRecord::default();
    sc.put_field("CALC", EpicsValue::String("5".into()))
        .unwrap();
    sc.special("CALC", true).unwrap();
    sc.put_field("INPA", EpicsValue::String("SRC MS".into()))
        .unwrap();
    sc.put_field("ODLY", EpicsValue::Double(100.0)).unwrap();
    sc.oopt = 0;
    sc.put_field("IVOA", EpicsValue::Short(1)).unwrap();
    sc.put_field("OUT", EpicsValue::String("TGT".into()))
        .unwrap();
    db.add_record("SC", Box::new(sc)).await.unwrap();

    // Bring SRC to INVALID (finite VAL=200 over HIHI=100, HHSV=INVALID).
    let mut v0 = HashSet::new();
    db.process_record_with_links("SRC", &mut v0, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_record("SRC").unwrap().read().common.sevr,
        AlarmSeverity::Invalid,
        "SRC must be INVALID with a finite VAL=200 (HIHI=100/HHSV=INVALID)"
    );

    // SC delaying cycle: ODLY>0 defers; OUT must NOT be written yet.
    let mut v1 = HashSet::new();
    db.process_record_with_links("SC", &mut v1, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_record("SC").unwrap().read().record.get_field("DLYA"),
        Some(EpicsValue::Short(1)),
        "ODLY>0 cycle sets DLYA and defers"
    );
    assert_eq!(
        db.get_pv("TGT").unwrap().to_f64(),
        Some(0.0),
        "OUT must not be written on the ODLY delaying cycle"
    );

    // SC continuation (delayed cycle): the carried nsev commits to
    // sevr==INVALID and the §4.6 IVOA=Don't_drive gate suppresses the OUT
    // write — the target keeps its 0.0 sentinel.
    let mut v2 = HashSet::new();
    db.process_record_continuation("SC", &mut v2, 0)
        .await
        .unwrap();
    assert_eq!(
        db.get_record("SC").unwrap().read().common.sevr,
        AlarmSeverity::Invalid,
        "continuation commits the carried MS-link nsev to sevr==INVALID \
         (not lost across the ODLY gap) — matches C carrying nsev to the \
         pact cycle"
    );
    assert_eq!(
        db.get_pv("TGT").unwrap().to_f64(),
        Some(0.0),
        "IVOA=Don't_drive must suppress the OUT write on the ODLY delayed \
         cycle for a non-persistent (MS-link) INVALID source too \
         (C execOutput nsev>=INVALID → break, sCalcoutRecord.c:794)"
    );
}
