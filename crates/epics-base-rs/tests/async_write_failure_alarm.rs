//! A failed async device write ends its cycle in the device's alarm, as the
//! synchronous `write()` branch does and as C's `processCallbackOutput`
//! reports through `pPvt->result.status` (devAsynFloat64.c:668-671).
//!
//! The completion task used to drop the write's result and call
//! `complete_async_record` with no outcome, so a write the driver refused on an
//! `ASYN_CANBLOCK` port completed `NO_ALARM`.
//!
//! The alarm is the device's alone: C `aoRecord` `writeValue` raises nothing
//! on a failed `write_ao` (aoRecord.c:565), so a device that reports none
//! leaves the record unalarmed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{DeviceDoneSource, PvDatabase};
use epics_base_rs::server::device_support::{DeviceAlarm, DeviceSupport, WriteStart};
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

/// A blocking-port device support: every write goes out asynchronously.
struct AsyncDevice {
    fails: Arc<AtomicBool>,
    /// The alarm a failed write leaves in `last_alarm`, as devAsyn does.
    reports: Option<u16>,
    async_write: bool,
    failed: bool,
}

impl DeviceSupport for AsyncDevice {
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        self.failed = self.fails.load(Ordering::SeqCst);
        if self.failed {
            Err(CaError::Protocol("asynDisconnected".into()))
        } else {
            Ok(())
        }
    }
    /// The write lands at once; its outcome reaches the record through the
    /// completion, as `pPvt->result.status` does.
    fn write_begin(
        &mut self,
        _record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<WriteStart> {
        if !self.async_write {
            return Ok(WriteStart::Synchronous);
        }
        self.failed = self.fails.load(Ordering::SeqCst);
        done.device_done().complete(done.priority()).unwrap();
        Ok(WriteStart::Pending)
    }
    fn dtyp(&self) -> &str {
        "AsyncDev"
    }
    fn last_alarm(&self) -> Option<DeviceAlarm<'_>> {
        let stat = self.reports.filter(|_| self.failed)?;
        Some(DeviceAlarm {
            stat,
            sevr: AlarmSeverity::Invalid as u16,
            msg: "",
        })
    }
}

async fn build(fails: bool) -> PvDatabase {
    build_with(fails, None, true).await
}

async fn build_with(fails: bool, reports: Option<u16>, async_write: bool) -> PvDatabase {
    let db = PvDatabase::new();
    // C `callbackInit`: no build arms the bands for a bare database.
    epics_base_rs::runtime::task::background_init();
    db.add_record("AO", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    let rec = db.get_record("AO").unwrap();
    let mut inst = rec.write();
    inst.common.dtyp = "AsyncDev".into();
    inst.common.udf = 0;
    inst.device = Some(Box::new(AsyncDevice {
        fails: Arc::new(AtomicBool::new(fails)),
        reports,
        async_write,
        failed: false,
    }));
    drop(inst);
    db
}

/// Drive one put and wait for the completion task to end the cycle. A
/// failed synchronous write fails the put itself; the alarm is what counts.
async fn put_and_complete(db: &PvDatabase) -> (u16, AlarmSeverity) {
    let _ = db
        .put_record_field_from_ca_no_notify("AO", "VAL", EpicsValue::Double(1.0))
        .await;
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
async fn a_failed_async_write_completes_in_the_device_s_alarm() {
    let db = build_with(true, Some(alarm_status::WRITE_ALARM), true).await;
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

#[epics_macros_rs::epics_test]
async fn a_failed_async_write_keeps_the_device_s_own_alarm() {
    let db = build_with(true, Some(alarm_status::COMM_ALARM), true).await;
    let (stat, sevr) = put_and_complete(&db).await;
    assert_eq!(sevr, AlarmSeverity::Invalid);
    assert_eq!(stat, alarm_status::COMM_ALARM);
}

#[epics_macros_rs::epics_test]
async fn a_failed_sync_write_keeps_the_device_s_own_alarm() {
    let db = build_with(true, Some(alarm_status::COMM_ALARM), false).await;
    let (stat, sevr) = put_and_complete(&db).await;
    assert_eq!(sevr, AlarmSeverity::Invalid);
    assert_eq!(stat, alarm_status::COMM_ALARM);
}

#[epics_macros_rs::epics_test]
async fn a_failed_write_with_no_device_alarm_raises_nothing() {
    for async_write in [false, true] {
        let db = build_with(true, None, async_write).await;
        let alarm = put_and_complete(&db).await;
        assert_eq!(
            alarm,
            (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm),
            "async_write={async_write}"
        );
    }
}
