//! sseq `special()` classifies only the link that was put
//! (sseqRecord.c:987-1027 DOLn, :1028-1079 LNKn), and a WAITn put only arms
//! `checkLinks` (:1081-1095), which leaves DOLnV/LNKnV to the CA pass. Two puts under one
//! record lock must therefore each keep the status their own put classified.

use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::EpicsValue;

const LOCAL: i16 = 2;

fn local() -> EpicsValue {
    EpicsValue::String("L".into())
}

fn status(db: &PvDatabase, pv: &str) -> i16 {
    db.get_pv(pv).unwrap().to_f64().unwrap() as i16
}

/// Runs each `(field, value)` put and its `special()` under one hold of S's
/// write lock, as the put path does, so every refresh token is issued before
/// any classification posts.
async fn puts_under_one_lock(puts: &[(&str, EpicsValue)]) -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("L", Box::new(AiRecord::new(3.0)))
        .await
        .unwrap();
    db.add_record("S", Box::new(SseqRecord::new()))
        .await
        .unwrap();
    db.ioc_init().await;
    // S's init classification must have posted before the puts.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    {
        let rec = db.get_record("S").unwrap();
        let mut guard = rec.write();
        for (field, value) in puts {
            guard.record.put_field(field, value.clone()).unwrap();
            guard.record.special(field, true).unwrap();
        }
    }
    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
    db
}

#[epics_macros_rs::epics_test]
async fn a_dol_put_keeps_its_status_past_a_put_to_another_dol() {
    let db = puts_under_one_lock(&[("DOL1", local()), ("DOL2", local())]).await;
    assert_eq!(status(&db, "S.DOL1V"), LOCAL);
    assert_eq!(status(&db, "S.DOL2V"), LOCAL);
}

#[epics_macros_rs::epics_test]
async fn a_dol_put_keeps_its_status_past_a_wait_put() {
    let db = puts_under_one_lock(&[("DOL1", local()), ("WAIT1", EpicsValue::Short(1))]).await;
    assert_eq!(status(&db, "S.DOL1V"), LOCAL);
}
