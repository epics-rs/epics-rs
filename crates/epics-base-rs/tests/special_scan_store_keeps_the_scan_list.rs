//! `Record::take_scan_store` is sscan's `psscan->scan = 0` from `special()`
//! (`sscanRecord.c:1571`): SCAN reads the stored value while the record stays
//! on, and is still processed from, the periodic list it was on — C's
//! `scanList` never reads SCAN (`dbScan.c:1014-1019`). A later SCAN put finds
//! nothing to delete for Passive (`dbScan.c:318`), so the record ends up on
//! both lists.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record, ScanType};
use epics_base_rs::types::{DbFieldType, EpicsValue};

static FIELDS: &[FieldDesc] = &[
    FieldDesc::new("VAL", DbFieldType::Double, false),
    FieldDesc::new("CMND", DbFieldType::Short, false),
];

struct Probe {
    processed: Arc<AtomicUsize>,
    store: Option<ScanType>,
}

impl Record for Probe {
    fn record_type(&self) -> &'static str {
        "scan_store_probe"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.processed.fetch_add(1, Ordering::Relaxed);
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(0.0)),
            "CMND" => Some(EpicsValue::Short(0)),
            _ => None,
        }
    }
    fn put_field(&mut self, name: &str, _value: EpicsValue) -> CaResult<()> {
        match name {
            "CMND" => Ok(()),
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }
    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if after && field == "CMND" {
            self.store = Some(ScanType::Passive);
        }
        Ok(())
    }
    fn take_scan_store(&mut self) -> Option<ScanType> {
        self.store.take()
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

async fn ioc() -> (Arc<PvDatabase>, Arc<AtomicUsize>) {
    let processed = Arc::new(AtomicUsize::new(0));
    let count = processed.clone();
    let ioc = IocBuilder::new()
        .register_record_type("scan_store_probe", move || {
            Box::new(Probe {
                processed: count.clone(),
                store: None,
            })
        })
        .db_string(
            "record(scan_store_probe, \"R\") {\n    field(SCAN, \".1 second\")\n}\n",
            &HashMap::new(),
        )
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = Arc::clone(ioc.database());
    assert!(ioc.run().await, "the IOC runs");
    (db, processed)
}

async fn on(db: &PvDatabase, scan: ScanType) -> bool {
    db.records_for_scan(scan)
        .await
        .iter()
        .any(|n| n.as_bytes() == b"R")
}

fn scan_of(db: &PvDatabase) -> u16 {
    db.get_pv("R.SCAN").unwrap().to_f64().unwrap() as u16
}

#[epics_macros_rs::epics_test]
async fn a_stored_scan_leaves_the_record_on_its_periodic_list() {
    let (db, processed) = ioc().await;
    assert!(on(&db, ScanType::SEC01).await);

    db.put_pv("R.CMND", EpicsValue::Short(1)).await.unwrap();
    assert_eq!(scan_of(&db), 0, "SCAN reads Passive");
    assert!(on(&db, ScanType::SEC01).await, "no scanDelete ran");

    let before = processed.load(Ordering::Relaxed);
    epics_base_rs::runtime::task::sleep(Duration::from_millis(350)).await;
    assert!(
        processed.load(Ordering::Relaxed) > before,
        "the .1 second list still processes it"
    );
}

#[epics_macros_rs::epics_test]
async fn a_scan_put_after_the_store_leaves_it_on_both_lists() {
    let (db, _) = ioc().await;
    db.put_pv("R.CMND", EpicsValue::Short(1)).await.unwrap();
    db.put_pv("R.SCAN", EpicsValue::String("1 second".into()))
        .await
        .unwrap();
    assert!(on(&db, ScanType::SEC1).await);
    assert!(on(&db, ScanType::SEC01).await);
}

#[epics_macros_rs::epics_test]
async fn a_scan_put_moves_the_record_off_its_list() {
    let (db, _) = ioc().await;
    db.put_pv("R.SCAN", EpicsValue::String("Passive".into()))
        .await
        .unwrap();
    assert_eq!(scan_of(&db), 0, "SCAN reads Passive");
    assert!(!on(&db, ScanType::SEC01).await);
}
