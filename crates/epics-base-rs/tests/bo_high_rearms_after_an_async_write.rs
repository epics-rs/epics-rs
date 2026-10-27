//! A bo's or busy's HIGH one-shot on an asynchronous device (stream's): the
//! first pass returns once the device sets PACT (`boRecord.c:257`,
//! `busyRecord.c:258`), and the completing pass decides the HIGH arm
//! (`:263-268`) from the VAL it then holds. Armed, its `val = 0` reprocess
//! writes a second time; a VAL 0 put during PACT leaves it unarmed, and only
//! the RPRO reprocess writes again.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::{DeviceDoneSource, PvDatabase};
use epics_base_rs::server::device_support::{DeviceSupport, WriteStart};
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::types::EpicsValue;

/// Completes every write `delay` later from a "port" thread, logging the
/// value it wrote.
struct Port {
    writes: Arc<Mutex<Vec<String>>>,
    delay: Duration,
}

impl DeviceSupport for Port {
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        unreachable!("write_begin always submits")
    }
    fn write_begin(
        &mut self,
        record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<WriteStart> {
        let val = record
            .get_field("VAL")
            .map(|v| format!("{v:?}"))
            .unwrap_or_default();
        self.writes.lock().unwrap().push(val);
        let (done, band, delay) = (done.device_done(), done.priority(), self.delay);
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            done.complete(band).unwrap();
        });
        Ok(WriteStart::Pending)
    }
    fn dtyp(&self) -> &str {
        "Port"
    }
}

/// `R`, HIGH 0.2, on a [`Port`] completing `delay` after each write.
async fn on_port(
    record: Box<dyn Record>,
    delay: Duration,
) -> (PvDatabase, Arc<Mutex<Vec<String>>>) {
    let db = PvDatabase::new();
    epics_base_rs::runtime::task::background_init();
    db.add_record("R", record).await.unwrap();
    db.ioc_init().await;
    let writes = Arc::new(Mutex::new(Vec::new()));
    {
        let rec = db.get_record("R").unwrap();
        let mut inst = rec.write();
        inst.record
            .put_field("HIGH", EpicsValue::Double(0.2))
            .unwrap();
        inst.common.udf = 0;
        inst.common.dtyp = "Port".into();
        inst.device = Some(Box::new(Port {
            writes: Arc::clone(&writes),
            delay,
        }));
    }
    (db, writes)
}

/// Until the port has taken `n` writes and R has left PACT after the last:
/// each completes from the port thread, after the put returns.
async fn settled(db: &PvDatabase, writes: &Mutex<Vec<String>>, n: usize) {
    let cell = db.get_record("R").unwrap();
    epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, async {
        while writes.lock().unwrap().len() < n || cell.read().is_processing() {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "R never settled on {n} writes: {:?}",
            writes.lock().unwrap()
        )
    });
}

#[epics_macros_rs::epics_test]
async fn the_high_reset_writes_again() {
    let (db, writes) = on_port(Box::new(BoRecord::default()), Duration::from_millis(20)).await;
    db.put_record_field_from_ca_no_notify("R", "VAL", EpicsValue::Enum(1))
        .await
        .unwrap();
    settled(&db, &writes, 2).await;
    assert_eq!(
        writes.lock().unwrap().len(),
        2,
        "{:?}",
        writes.lock().unwrap()
    );
    assert_eq!(db.get_pv("R.VAL").ok().and_then(|v| v.to_f64()), Some(0.0));
}

/// VAL 1 is written, VAL 0 lands while the write is in flight, and the
/// completing pass sees 0: no HIGH arm, so the writes are the 1 and the RPRO
/// reprocess's 0 — C's two, not a third from a timer the first pass's VAL armed.
async fn a_val_put_during_pact_disarms_high(record: Box<dyn Record>) {
    let (db, writes) = on_port(record, Duration::from_millis(100)).await;
    db.put_record_field_from_ca_no_notify("R", "VAL", EpicsValue::Enum(1))
        .await
        .unwrap();
    epics_base_rs::runtime::task::sleep(Duration::from_millis(30)).await;
    db.put_record_field_from_ca_no_notify("R", "VAL", EpicsValue::Enum(0))
        .await
        .unwrap();
    settled(&db, &writes, 2).await;
    // A timer the first pass armed would fire 0.2 s on; nothing orders after
    // a timer never armed, so a window past HIGH stands for it.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1000)).await;
    assert_eq!(*writes.lock().unwrap(), ["Enum(1)", "Enum(0)"]);
    assert_eq!(db.get_pv("R.VAL").ok().and_then(|v| v.to_f64()), Some(0.0));
}

#[epics_macros_rs::epics_test]
async fn bo_decides_high_on_the_completing_pass() {
    a_val_put_during_pact_disarms_high(Box::new(BoRecord::default())).await;
}

#[epics_macros_rs::epics_test]
async fn busy_decides_high_on_the_completing_pass() {
    a_val_put_during_pact_disarms_high(Box::new(BusyRecord::new())).await;
}
