//! A dset read that errs produced no value: C `aiRecord` converts only on a
//! zero `read_ai` (aiRecord.c:158), so VAL keeps the last reading. A refused
//! `read_begin` is devAsynInt32 `processAi`'s refused `queueRequest`
//! (devAsynInt32.c:836-847): it raises `asynStatusToEpicsAlarm(status,
//! READ_ALARM, INVALID)`, whose defaults fill only what the device left unset
//! (asynEpicsUtils.c:234-266).

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{DeviceDoneSource, ProcStack, PvDatabase};
use epics_base_rs::server::device_support::{
    DeviceAlarm, DeviceReadOutcome, DeviceSupport, ReadStart,
};
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;

#[derive(Clone, Copy)]
enum Fails {
    ReadBegin,
    Read,
}

/// A device whose read fails where `fails` says, leaving `reports` in
/// `last_alarm`. A read that got through would return RVAL as it stands.
struct Failing {
    fails: Fails,
    reports: Option<(u16, AlarmSeverity)>,
}

impl DeviceSupport for Failing {
    fn read_begin(
        &mut self,
        _record: &mut dyn Record,
        _done: &DeviceDoneSource<'_>,
    ) -> CaResult<ReadStart> {
        match self.fails {
            Fails::ReadBegin => Err(CaError::Protocol("queueRequest refused".into())),
            Fails::Read => Ok(ReadStart::Synchronous),
        }
    }
    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        match self.fails {
            Fails::ReadBegin => Ok(DeviceReadOutcome::ok()),
            Fails::Read => Err(CaError::Protocol("asynDisconnected".into())),
        }
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "Failing"
    }
    fn last_alarm(&self) -> Option<DeviceAlarm<'_>> {
        let (stat, sevr) = self.reports?;
        Some(DeviceAlarm {
            stat,
            sevr: sevr as u16,
            msg: "",
        })
    }
}

/// One process of an `ai` holding VAL 1.5 over RVAL 7: its VAL and alarm.
async fn process_once(
    fails: Fails,
    reports: Option<(u16, AlarmSeverity)>,
) -> (f64, u16, AlarmSeverity) {
    let db = PvDatabase::new();
    db.add_record("AI", Box::new(AiRecord::new(1.5)))
        .await
        .unwrap();
    {
        let rec = db.get_record("AI").unwrap();
        let mut inst = rec.write();
        inst.record.put_field("RVAL", EpicsValue::Long(7)).unwrap();
        inst.common.dtyp = "Failing".into();
        inst.common.udf = 0;
        inst.device = Some(Box::new(Failing { fails, reports }));
    }
    let mut visited = ProcStack::new();
    let _ = db.process_record_with_links("AI", &mut visited).await;
    let rec = db.get_record("AI").unwrap();
    let inst = rec.read();
    let val = inst.record.get_field("VAL").unwrap().to_f64().unwrap();
    (val, inst.common.stat, inst.common.sevr)
}

#[epics_macros_rs::epics_test]
async fn a_refused_read_keeps_val_and_raises_read_invalid() {
    assert_eq!(
        process_once(Fails::ReadBegin, None).await,
        (1.5, alarm_status::READ_ALARM, AlarmSeverity::Invalid)
    );
}

#[epics_macros_rs::epics_test]
async fn a_refused_read_keeps_the_device_s_own_alarm() {
    let comm = Some((alarm_status::COMM_ALARM, AlarmSeverity::Major));
    assert_eq!(
        process_once(Fails::ReadBegin, comm).await,
        (1.5, alarm_status::COMM_ALARM, AlarmSeverity::Major)
    );
}

#[epics_macros_rs::epics_test]
async fn a_failed_read_keeps_val() {
    let comm = Some((alarm_status::COMM_ALARM, AlarmSeverity::Invalid));
    assert_eq!(
        process_once(Fails::Read, comm).await,
        (1.5, alarm_status::COMM_ALARM, AlarmSeverity::Invalid)
    );
}
