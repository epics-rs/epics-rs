//! A device read raises its alarm inside `readValue`, ahead of `checkAlarms`
//! (`aiRecord.c:156-164`), so on an equal-severity tie the device's STAT is
//! the one that sticks. softIoc ai whose INP read fails (LINK INVALID from
//! `readValue`) with HHSV INVALID and VAL over HIHI reports STAT=LINK.

use std::collections::HashMap;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{
    DeviceAlarm, DeviceReadOutcome, DeviceSupport, DeviceUdf,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

struct AlarmingDevice;

impl DeviceSupport for AlarmingDevice {
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        record.put_field("VAL", EpicsValue::Double(42.0))?;
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "alarmingRead"
    }

    fn last_alarm(&self) -> Option<DeviceAlarm<'_>> {
        Some(DeviceAlarm {
            stat: alarm_status::READ_ALARM,
            sevr: 3,
            msg: "",
        })
    }
}

#[epics_macros_rs::epics_test]
async fn the_device_alarm_wins_an_equal_severity_tie() {
    let (db, _) = IocBuilder::new()
        .register_dynamic_device_support(|ctx: &DeviceSupportContext| {
            (ctx.dtyp == "alarmingRead").then(|| Box::new(AlarmingDevice) as Box<dyn DeviceSupport>)
        })
        .db_string(
            r#"record(ai, "AI") { field(DTYP, "alarmingRead") field(HIHI, "10") field(HHSV, "INVALID") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("AI", &mut visited)
        .await
        .unwrap();
    assert_eq!(db.get_pv("AI.SEVR").unwrap(), EpicsValue::Short(3));
    assert_eq!(
        db.get_pv("AI.STAT").unwrap(),
        EpicsValue::Short(alarm_status::READ_ALARM as i16)
    );
}

/// softIoc: Soft Channel ai, INP "NOPE:Y", HIHI 10 / HHSV INVALID, VAL 20 —
/// the failed read's LINK/INVALID comes first, so STAT=LINK.
#[epics_macros_rs::epics_test]
async fn a_soft_input_link_failure_wins_the_tie_too() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(ai, "AM") {
                field(DTYP, "Soft Channel") field(INP, "NOPE:Y CA") field(VAL, "20")
                field(HIHI, "10") field(HHSV, "INVALID")
            }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links("AM", &mut visited)
        .await
        .unwrap();
    assert_eq!(db.get_pv("AM.SEVR").unwrap(), EpicsValue::Short(3));
    assert_eq!(
        db.get_pv("AM.STAT").unwrap(),
        EpicsValue::Short(alarm_status::LINK_ALARM as i16)
    );
}
