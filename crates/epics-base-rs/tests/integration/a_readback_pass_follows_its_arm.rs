//! C arms a readback, runs its pass and reconciles under one `dbScanLock`
//! (devAsynInt32.c:627-645): no other pass of the record runs between the arm
//! and the readback pass it armed.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::longout::LongoutRecord;
use epics_base_rs::types::EpicsValue;

/// Logs each arm, readback read and write; its arm hands a put to `put` and
/// holds the record long enough for that put to wait on it.
struct Logged {
    log: Arc<Mutex<Vec<&'static str>>>,
    put: mpsc::Sender<()>,
}

impl DeviceSupport for Logged {
    fn dtyp(&self) -> &str {
        "MockDev"
    }
    fn output_callback_readback(&self) -> bool {
        true
    }
    fn arm_readback_callback(&mut self) -> DeviceUdf {
        self.log.lock().unwrap().push("arm");
        self.put.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        DeviceUdf::Defined
    }
    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        self.log.lock().unwrap().push("readback");
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        self.log.lock().unwrap().push("put");
        Ok(())
    }
}

#[epics_macros_rs::epics_test]
async fn a_put_waiting_on_an_armed_readback_runs_after_its_pass() {
    let db = Arc::new(PvDatabase::new());
    db.add_record("LO", Box::new(LongoutRecord::new(0)))
        .await
        .unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let (put, go) = mpsc::channel();
    {
        let rec = db.get_record("LO").unwrap();
        let mut inst = rec.write();
        inst.common.dtyp = "MockDev".into();
        inst.device = Some(Box::new(Logged {
            log: log.clone(),
            put,
        }));
    }
    let putter = {
        let db = db.clone();
        std::thread::spawn(move || {
            go.recv().unwrap();
            let _gate = db.lock_record("LO");
            db.get_record("LO")
                .unwrap()
                .write()
                .record
                .put_field("VAL", EpicsValue::Long(5))
                .unwrap();
            db.process_record_with_links_already_locked("LO", &mut ProcStack::new())
                .unwrap();
        })
    };
    db.process_record_readback("LO", &mut ProcStack::new())
        .await
        .unwrap();
    putter.join().unwrap();
    assert_eq!(*log.lock().unwrap(), ["arm", "readback", "put"]);
}
