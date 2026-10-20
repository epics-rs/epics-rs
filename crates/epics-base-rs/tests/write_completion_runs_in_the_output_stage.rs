//! C's completing pass calls the device's write a second time, from the
//! IVOA-gated output stage (`boRecord.c:241-257`), and a trigger bo's device
//! drops VAL to 0 there (`devMW100_bo.c:194-197`). So VAL reads as written
//! while PACT holds, a put during PACT is zeroed before `monitor()` compares
//! it, and a pass whose IVOA skips the write leaves VAL alone.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::{DeviceDone, DeviceDoneSource, PvDatabase};
use epics_base_rs::server::device_support::{DeviceSupport, WriteStart};
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// Holds each write until the test completes it, and zeroes VAL on the
/// completing pass.
#[derive(Clone, Default)]
struct Trigger {
    held: Arc<Mutex<Option<DeviceDone>>>,
    completions: Arc<AtomicUsize>,
}

impl Trigger {
    fn release(&self) {
        let done = self.held.lock().unwrap().take().expect("a write in flight");
        std::thread::spawn(move || done.complete(done.priority()).unwrap())
            .join()
            .unwrap();
    }

    fn completions(&self) -> usize {
        self.completions.load(Ordering::SeqCst)
    }
}

impl DeviceSupport for Trigger {
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        unreachable!("write_begin always submits")
    }
    fn write_begin(
        &mut self,
        _record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<WriteStart> {
        *self.held.lock().unwrap() = Some(done.device_done());
        Ok(WriteStart::Pending)
    }
    fn async_write_completed(&mut self, record: &mut dyn Record) {
        self.completions.fetch_add(1, Ordering::SeqCst);
        record.put_field("VAL", EpicsValue::Enum(0)).unwrap();
    }
    fn dtyp(&self) -> &str {
        "Trigger"
    }
}

async fn trigger_bo(osv: i16, ivoa: i16) -> (PvDatabase, Trigger) {
    let db = PvDatabase::new();
    epics_base_rs::runtime::task::background_init();
    db.add_record("R", Box::new(BoRecord::default()))
        .await
        .unwrap();
    db.ioc_init().await;
    let dev = Trigger::default();
    {
        let rec = db.get_record("R").unwrap();
        let mut inst = rec.write();
        inst.record
            .put_field("OSV", EpicsValue::Short(osv))
            .unwrap();
        inst.record
            .put_field("IVOA", EpicsValue::Short(ivoa))
            .unwrap();
        inst.common.udf = 0;
        inst.common.dtyp = "Trigger".into();
        inst.device = Some(Box::new(dev.clone()));
    }
    (db, dev)
}

/// Whether `R` leaves PACT within two seconds.
async fn idle(db: &PvDatabase) -> bool {
    for _ in 0..400 {
        if !db.get_record("R").unwrap().read().is_processing() {
            return true;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    false
}

fn val(db: &PvDatabase) -> Option<f64> {
    db.get_pv("R.VAL").ok().and_then(|v| v.to_f64())
}

#[epics_macros_rs::epics_test]
async fn val_reads_as_written_until_the_completing_pass() {
    let (db, dev) = trigger_bo(0, 0).await;
    db.put_record_field_from_ca_no_notify("R", "VAL", EpicsValue::Enum(1))
        .await
        .unwrap();
    assert_eq!(val(&db), Some(1.0), "VAL while the write holds PACT");
    assert_eq!(dev.completions(), 0);
    dev.release();
    assert!(idle(&db).await, "the write never completed");
    assert_eq!(dev.completions(), 1);
    assert_eq!(val(&db), Some(0.0), "VAL after the completing pass");
}

/// The put lands as VAL 1 (no post: VAL is `pp`), the completing pass
/// zeroes it before `monitor()`, and the RPRO reprocess writes 0: C posts
/// VAL on neither pass.
#[epics_macros_rs::epics_test]
async fn a_put_during_pact_posts_nothing() {
    let (db, dev) = trigger_bo(0, 0).await;
    db.put_record_field_from_ca_no_notify("R", "VAL", EpicsValue::Enum(1))
        .await
        .unwrap();
    let mut rx = db
        .get_record("R")
        .unwrap()
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Enum, EventMask::VALUE.bits())
        .expect("subscriber");
    while rx.try_recv().is_ok() {}

    db.put_record_field_from_ca_no_notify("R", "VAL", EpicsValue::Enum(1))
        .await
        .unwrap();
    dev.release();
    for _ in 0..400 {
        if dev.held.lock().unwrap().is_some() {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    dev.release();
    assert!(idle(&db).await, "the RPRO write never completed");
    assert_eq!(dev.completions(), 2);
    assert_eq!(val(&db), Some(0.0));
    let mut posted = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        posted.push(ev.value().as_value().cloned());
    }
    assert!(posted.is_empty(), "VAL posts: {posted:?}");
}

/// VAL 0 drives the write; VAL 1 put during PACT raises OSV's INVALID on
/// the completing pass, so IVOA "Don't drive outputs" skips `writeValue` and
/// the device never sees that pass — nor the RPRO reprocess, skipped the same.
#[epics_macros_rs::epics_test]
async fn an_ivoa_skip_never_reaches_the_device() {
    let (db, dev) = trigger_bo(3, 1).await;
    db.put_record_field_from_ca_no_notify("R", "VAL", EpicsValue::Enum(0))
        .await
        .unwrap();
    db.put_record_field_from_ca_no_notify("R", "VAL", EpicsValue::Enum(1))
        .await
        .unwrap();
    dev.release();
    let settled = idle(&db).await;
    assert_eq!(dev.completions(), 0, "the skipped pass reached the device");
    assert!(settled, "R stayed PACT");
    assert_eq!(val(&db), Some(1.0));
    assert_eq!(db.get_pv("R.SEVR").unwrap(), EpicsValue::Short(3));
}
