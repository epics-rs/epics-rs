//! A record in a database past `finishDevSup` for each state a harness
//! binds, to see each `dbProcess` a callback runs and what it read; the
//! crate's unit tests have the same in `device_state::passes`.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use asyn_rs::interfaces::motor::MotorStatus;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::record::{Record, ScanType};
use epics_base_rs::server::records::ai::AiRecord;
use motor_rs::device_state::SharedDeviceState;
use motor_rs::trace::MotorTrace;

type Read = Arc<Mutex<Vec<Option<MotorStatus>>>>;

/// One record's processes, each with its mailbox's new status, if it
/// read one. Holds the record's database.
pub struct Reads(Read, Arc<PvDatabase>);

impl Reads {
    pub fn take(&self) -> Vec<Option<MotorStatus>> {
        std::mem::take(&mut self.0.lock().unwrap())
    }

    /// The processes so far, waiting up to `within` for one.
    pub async fn wait(&self, within: std::time::Duration) -> Vec<Option<MotorStatus>> {
        let until = std::time::Instant::now() + within;
        loop {
            let read = self.take();
            if !read.is_empty() || std::time::Instant::now() >= until {
                return read;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }
}

/// Takes the mailbox as the motor record's callback pass does.
struct ReadsMailbox(Option<SharedDeviceState>, Read);

impl DeviceSupport for ReadsMailbox {
    fn dtyp(&self) -> &str {
        "readsMailbox"
    }
    fn read(&mut self, _: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let read = self
            .0
            .as_ref()
            .and_then(|state| state.lock().unwrap().update_values());
        self.1.lock().unwrap().push(read);
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }
    fn write(&mut self, _: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}

const NAME: &str = "R";

/// Record [`NAME`] reading `mailbox`, past the gate.
fn record(mailbox: Option<SharedDeviceState>) -> Reads {
    let db = Arc::new(PvDatabase::new());
    // A thread of its own, which may block whatever runtime the test runs.
    std::thread::scope(|s| {
        s.spawn(|| {
            epics_base_rs::runtime::task::block_on_sync(
                db.add_record(NAME, Box::new(AiRecord::new(0.0))),
            )
        })
        .join()
    })
    .unwrap()
    .unwrap()
    .unwrap();
    let read = Read::default();
    {
        let rec = db.get_record(NAME).unwrap();
        let mut inst = rec.write();
        inst.common.dtyp = "readsMailbox".into();
        inst.device = Some(Box::new(ReadsMailbox(mailbox, read.clone())));
    }
    db.scan_control().finish_dev_sup();
    Reads(read, db)
}

/// `state` past the gate, its callbacks processing a record named into
/// `trace`.
pub fn bind(state: &SharedDeviceState, trace: &MotorTrace) -> Reads {
    let reads = record(Some(state.clone()));
    trace.set_record(NAME);
    assert_eq!(trace.record(), NAME, "one record per trace");
    let mut ds = state.lock().unwrap();
    ds.bind_database(reads.1.async_handle());
    ds.accept();
    drop(ds);
    reads
}

/// `support`'s callbacks processing a record that reads `mailbox`.
pub fn bind_support(support: &mut dyn DeviceSupport, mailbox: Option<SharedDeviceState>) -> Reads {
    let reads = record(mailbox);
    support.set_record_info(NAME, ScanType::Passive);
    support.bind_database(reads.1.async_handle());
    reads
}
