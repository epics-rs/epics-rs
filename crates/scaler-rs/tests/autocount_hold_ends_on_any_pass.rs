//! The autocount hold ends on the scaler's next pass, whatever processed it.
//!
//! C arms the autocount callback and sets `ss = SCALER_STATE_WAITING` "to
//! start next time"; a pass that finds `ss == WAITING` starts counting
//! (`scalerRecord.c:492-536`), with no test of how long it waited.
//! oracleScalerIoc, Passive `SC` on drvScalerSoft with `TP1 100 DLY1 5`:
//! `CONT 1`, 0.3 s later `SS 1`; `PROC 1`, 0.1 s later `SS 2`.

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — it ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p
// scaler-rs --all-features`); nothing here builds a CA server, and the
// reactor it sleeps on comes from `#[tokio::test]` itself.

use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;
use scaler_rs::records::scaler::ScalerRecord;

#[tokio::test]
async fn a_pass_during_the_autocount_hold_starts_the_count() {
    let db = PvDatabase::new();
    let mut rec = ScalerRecord::default();
    rec.freq = 1e7;
    rec.tp = 1.0;
    rec.tp1 = 100.0;
    rec.dly1 = 5.0;
    rec.nch = 4;
    rec.init_record(1).unwrap();
    db.add_record("SC", Box::new(rec)).await.unwrap();
    let ss = || db.get_record("SC").unwrap().read().record.get_field("SS");

    db.put_record_field_from_ca("SC", "CONT", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(ss(), Some(EpicsValue::Short(1)), "holding before the count");

    db.put_record_field_from_ca("SC", "PROC", EpicsValue::Char(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        ss(),
        Some(EpicsValue::Short(2)),
        "the pass started the count"
    );
}
