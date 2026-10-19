//! A bo's HIGH one-shot on an asynchronous device (stream's): the first pass
//! returns once the device sets PACT (`boRecord.c:257`), and the completing
//! pass arms the HIGH callback (`:263-268`), whose `val = 0` reprocess
//! writes a second time.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::{DeviceDoneSource, PvDatabase};
use epics_base_rs::server::device_support::{DeviceSupport, WriteStart};
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::bo::BoRecord;
use epics_base_rs::types::EpicsValue;

/// Completes every write from a "port" thread, logging the value it wrote.
struct Port(Arc<Mutex<Vec<String>>>);

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
        self.0.lock().unwrap().push(val);
        let (done, band) = (done.device_done(), done.priority());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            done.complete(band).unwrap();
        });
        Ok(WriteStart::Pending)
    }
    fn dtyp(&self) -> &str {
        "Port"
    }
}

#[epics_macros_rs::epics_test]
async fn the_high_reset_writes_again() {
    let db = PvDatabase::new();
    epics_base_rs::runtime::task::background_init();
    db.add_record("BO", Box::new(BoRecord::default()))
        .await
        .unwrap();
    db.ioc_init().await;
    let writes = Arc::new(Mutex::new(Vec::new()));
    {
        let rec = db.get_record("BO").unwrap();
        let mut inst = rec.write();
        inst.record
            .put_field("HIGH", EpicsValue::Double(0.2))
            .unwrap();
        inst.common.udf = 0;
        inst.common.dtyp = "Port".into();
        inst.device = Some(Box::new(Port(Arc::clone(&writes))));
    }
    db.put_record_field_from_ca_no_notify("BO", "VAL", EpicsValue::Enum(1))
        .await
        .unwrap();
    epics_base_rs::runtime::task::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        writes.lock().unwrap().len(),
        2,
        "{:?}",
        writes.lock().unwrap()
    );
    assert_eq!(db.get_pv("BO.VAL").ok().and_then(|v| v.to_f64()), Some(0.0));
}
