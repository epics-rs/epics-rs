//! A failed async device write ends its cycle in `WRITE_ALARM`/`INVALID`,
//! as the synchronous `write()` branch does and as C's `processCallbackOutput`
//! reports through `pPvt->result.status` (devAsynFloat64.c:668-671).
//!
//! The completion task used to drop the write's result and call
//! `complete_async_record` with no outcome, so a write the driver refused on an
//! `ASYN_CANBLOCK` port completed `NO_ALARM`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{DeviceDoneSource, PvDatabase};
use epics_base_rs::server::device_support::{DeviceSupport, WriteStart};
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

/// A blocking-port device support: every write goes out asynchronously.
struct AsyncDevice {
    fails: Arc<AtomicBool>,
}

impl DeviceSupport for AsyncDevice {
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        unreachable!("write_begin always submits")
    }
    /// The write lands at once; its outcome reaches the record through the
    /// completion, as `pPvt->result.status` does.
    fn write_begin(
        &mut self,
        _record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<WriteStart> {
        let outcome = if self.fails.load(Ordering::SeqCst) {
            Err(CaError::Protocol("asynError".into()))
        } else {
            Ok(())
        };
        done.device_done()
            .complete(done.priority(), outcome)
            .unwrap();
        Ok(WriteStart::Pending)
    }
    fn dtyp(&self) -> &str {
        "AsyncDev"
    }
}

async fn build(fails: bool) -> PvDatabase {
    let db = PvDatabase::new();
    db.add_record("AO", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let rec = db.get_record("AO").unwrap();
    let mut inst = rec.write();
    inst.common.dtyp = "AsyncDev".into();
    inst.common.udf = 0;
    inst.device = Some(Box::new(AsyncDevice {
        fails: Arc::new(AtomicBool::new(fails)),
    }));
    drop(inst);
    db
}

/// Drive one put and wait for the completion task to end the cycle.
async fn put_and_complete(db: &PvDatabase) -> (u16, AlarmSeverity) {
    db.put_record_field_from_ca_no_notify("AO", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    for _ in 0..500 {
        {
            let rec = db.get_record("AO").unwrap();
            let inst = rec.read();
            if !inst.is_processing() {
                return (inst.common.stat, inst.common.sevr);
            }
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("the async write never completed");
}

#[epics_macros_rs::epics_test]
async fn a_failed_async_write_completes_in_write_alarm() {
    let db = build(true).await;
    let (stat, sevr) = put_and_complete(&db).await;
    assert_eq!(sevr, AlarmSeverity::Invalid);
    assert_eq!(stat, alarm_status::WRITE_ALARM);
}

#[epics_macros_rs::epics_test]
async fn a_successful_async_write_completes_no_alarm() {
    let db = build(false).await;
    let (stat, sevr) = put_and_complete(&db).await;
    assert_eq!(sevr, AlarmSeverity::NoAlarm);
    assert_eq!(stat, alarm_status::NO_ALARM);
}
