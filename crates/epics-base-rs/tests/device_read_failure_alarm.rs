//! A failed device read: the device's own alarm is the failure's.
//!
//! C `aiRecord` `readValue` raises nothing on a failed `read_ai`
//! (aiRecord.c:482); devAsyn maps the status itself (asynDisconnected is
//! COMM, asynEpicsUtils.c:234-266). `recGblSetSevr` keeps the first raise
//! unless a later one is strictly more severe, so a READ/INVALID raised ahead
//! of the device's alarm took the INVALID tie and outranked a MAJOR one.
//! A device that failed and reported none leaves the record unalarmed.
//! The device raises inside `readValue`, ahead of the record body, so a body
//! raise of equal severity in the same cycle loses to it.

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::device_support::{DeviceAlarm, DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, FieldDesc, ProcessOutcome, Record};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// A device whose every read fails, leaving `reports` in `last_alarm`.
struct FailingReader {
    reports: Option<(u16, AlarmSeverity)>,
}

impl DeviceSupport for FailingReader {
    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Err(CaError::Protocol("asynDisconnected".into()))
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "FailingReader"
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

/// One process of `record` over the device, before any commit.
async fn read_into(
    record: Box<dyn Record>,
    reports: Option<(u16, AlarmSeverity)>,
) -> std::sync::Arc<PvDatabase> {
    let db = std::sync::Arc::new(PvDatabase::new());
    db.add_record("AI", record).await.unwrap();
    {
        let rec = db.get_record("AI").unwrap();
        let mut inst = rec.write();
        inst.common.dtyp = "FailingReader".into();
        inst.common.udf = 0;
        inst.device = Some(Box::new(FailingReader { reports }));
    }
    let mut visited = ProcStack::new();
    let _ = db.process_record_with_links("AI", &mut visited).await;
    db
}

/// One process of an `ai` over the device; the alarm it committed.
async fn read_once(reports: Option<(u16, AlarmSeverity)>) -> (u16, AlarmSeverity) {
    let db = read_into(Box::new(AiRecord::new(0.0)), reports).await;
    let rec = db.get_record("AI").unwrap();
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr)
}

static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

/// An input whose body raises SOFT/INVALID and returns before the epilogue,
/// as C `epidRecord.c:195-202` returns on UDF: the raise stays pending.
struct RaisingBody;

impl Record for RaisingBody {
    fn record_type(&self) -> &'static str {
        "raising_body"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete_no_emit_raising(
            alarm_status::SOFT_ALARM,
            AlarmSeverity::Invalid,
        ))
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        (name == "VAL").then_some(EpicsValue::Double(0.0))
    }
    fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
}

#[epics_macros_rs::epics_test]
async fn a_failed_read_keeps_the_device_s_own_invalid_alarm() {
    let alarm = read_once(Some((alarm_status::COMM_ALARM, AlarmSeverity::Invalid))).await;
    assert_eq!(alarm, (alarm_status::COMM_ALARM, AlarmSeverity::Invalid));
}

#[epics_macros_rs::epics_test]
async fn a_failed_read_keeps_the_device_s_own_lesser_alarm() {
    let alarm = read_once(Some((alarm_status::COMM_ALARM, AlarmSeverity::Major))).await;
    assert_eq!(alarm, (alarm_status::COMM_ALARM, AlarmSeverity::Major));
}

#[epics_macros_rs::epics_test]
async fn a_failed_read_with_no_device_alarm_raises_nothing() {
    let alarm = read_once(None).await;
    assert_eq!(alarm, (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm));
}

#[epics_macros_rs::epics_test]
async fn a_failed_read_s_alarm_precedes_the_body_s() {
    let db = read_into(
        Box::new(RaisingBody),
        Some((alarm_status::COMM_ALARM, AlarmSeverity::Invalid)),
    )
    .await;
    let rec = db.get_record("AI").unwrap();
    let inst = rec.read();
    assert_eq!(
        (inst.common.nsta, inst.common.nsev),
        (alarm_status::COMM_ALARM, AlarmSeverity::Invalid)
    );
}
