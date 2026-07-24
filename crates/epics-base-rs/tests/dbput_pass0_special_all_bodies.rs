//! C `dbPut` runs `dbPutSpecial(paddr, 0)` on EVERY entry path —
//! `dbPutField` (CA/PVA client) and `dbPutLink` (record OUT links)
//! alike (dbAccess.c). The port has three `dbPut` bodies; only the
//! external-put one called the record's pass-0 `special()` hook, so a
//! put-link delivery skipped every pass-0 side effect. The first live
//! consumer is motor's drive-field DMOV blink (motorRecord.cc:
//! 2582-2608): without pass-0 on the internal bodies, a same-value
//! put-link into `motor.VAL` refused at the move-block entry gate
//! instead of pulsing DMOV like C.
//!
//! This pins pass-0 (and pass-1) `special()` on the two internal
//! bodies: `put_pv` (`dbPutLink` route, via `put_pv_already_locked`)
//! and `put_pv_and_post` (gateway/sequencer route).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{FieldDesc, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

struct Pass0TrackingRecord {
    val: f64,
    pass0_count: Arc<AtomicU32>,
    pass1_count: Arc<AtomicU32>,
}

impl Record for Pass0TrackingRecord {
    fn record_type(&self) -> &'static str {
        "test_pass0"
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
                if let EpicsValue::Double(v) = value {
                    self.val = v;
                    Ok(())
                } else {
                    Err(CaError::InvalidValue("bad type".into()))
                }
            }
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];
        FIELDS
    }
    fn special(&mut self, _field: &str, after: bool) -> CaResult<()> {
        if after {
            self.pass1_count.fetch_add(1, Ordering::SeqCst);
        } else {
            self.pass0_count.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

fn tracking_record() -> (Pass0TrackingRecord, Arc<AtomicU32>, Arc<AtomicU32>) {
    let pass0 = Arc::new(AtomicU32::new(0));
    let pass1 = Arc::new(AtomicU32::new(0));
    let rec = Pass0TrackingRecord {
        val: 0.0,
        pass0_count: pass0.clone(),
        pass1_count: pass1.clone(),
    };
    (rec, pass0, pass1)
}

/// `put_pv` is the C `dbPut` sitting under `dbPutLink`
/// (`write_db_link_value` → `put_pv_already_locked` → `put_pv_inner`):
/// pass-0 must run before the write, pass-1 after, exactly like the
/// external-put body.
#[epics_macros_rs::epics_test]
async fn put_pv_runs_pass0_special() {
    let db = PvDatabase::new();
    let (rec, pass0, pass1) = tracking_record();
    db.add_record("T0", Box::new(rec)).await.unwrap();

    db.put_pv("T0.VAL", EpicsValue::Double(1.5)).await.unwrap();

    assert_eq!(pass0.load(Ordering::SeqCst), 1, "dbPutSpecial pass 0");
    assert_eq!(pass1.load(Ordering::SeqCst), 1, "dbPutSpecial pass 1");
}

/// `put_pv_and_post` is the third `dbPut` body (gateway / sequencer
/// route) and must match the other two.
#[epics_macros_rs::epics_test]
async fn put_pv_and_post_runs_pass0_special() {
    let db = PvDatabase::new();
    let (rec, pass0, pass1) = tracking_record();
    db.add_record("T1", Box::new(rec)).await.unwrap();

    db.put_pv_and_post("T1.VAL", EpicsValue::Double(2.5))
        .await
        .unwrap();

    assert_eq!(pass0.load(Ordering::SeqCst), 1, "dbPutSpecial pass 0");
    assert_eq!(pass1.load(Ordering::SeqCst), 1, "dbPutSpecial pass 1");
}
