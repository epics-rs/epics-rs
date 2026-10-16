//! An output's device raises its alarm inside `writeValue`
//! (`aoRecord.c:196-197`): after `checkAlarms` and after the IVOA decision on
//! `nsev`, and in the cycle whose write produced it. The alarm used to be
//! applied before the output stage, so it was the previous write's, and an
//! INVALID one vetoed the write under IVOA "Don't drive outputs".

mod declared_rows;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceAlarm, DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{DbLinkType, Record};
use epics_base_rs::types::EpicsValue;

/// Each write leaves the alarm of that write: WRITE at the written VAL as
/// severity, so the record shows which write it came from.
struct AlarmingWrite {
    writes: Arc<AtomicUsize>,
    sevr: u16,
}

impl DeviceSupport for AlarmingWrite {
    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::ok())
    }

    fn write(&mut self, record: &mut dyn Record) -> CaResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.sevr = record
            .get_field("VAL")
            .and_then(|v| v.to_f64())
            .unwrap_or(0.0) as u16;
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "alarmingWrite"
    }

    fn last_alarm(&self) -> Option<DeviceAlarm<'_>> {
        (self.sevr != 0).then_some(DeviceAlarm {
            stat: alarm_status::WRITE_ALARM,
            sevr: self.sevr,
            msg: "",
        })
    }
}

#[epics_macros_rs::epics_test]
async fn an_output_raises_the_alarm_of_this_cycles_write() {
    let writes = Arc::new(AtomicUsize::new(0));
    let w = Arc::clone(&writes);
    let devices = [(
        "ao",
        declared_rows::rows(&["alarmingWrite"], DbLinkType::Constant),
    )];
    let (db, _) = IocBuilder::new()
        .register_dynamic_device_support(&devices, move |ctx: &DeviceSupportContext| {
            (ctx.dtyp == "alarmingWrite").then(|| {
                Box::new(AlarmingWrite {
                    writes: Arc::clone(&w),
                    sevr: 0,
                }) as Box<dyn DeviceSupport>
            })
        })
        .db_string(
            r#"record(ao, "AO") { field(DTYP, "alarmingWrite") field(IVOA, "Don't drive outputs") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    // MINOR from this write, then INVALID, then back to none: each cycle
    // shows its own write's alarm, and the INVALID left by the second write
    // vetoes nothing, as IVOA reads `nsev` before `writeValue`.
    for (val, sevr) in [(1.0, 1i16), (3.0, 3), (0.0, 0)] {
        db.put_record_field_from_ca("AO", "VAL", EpicsValue::Double(val))
            .await
            .unwrap();
        assert_eq!(
            db.get_pv("AO.SEVR").unwrap(),
            EpicsValue::Short(sevr),
            "VAL {val}"
        );
    }
    assert_eq!(writes.load(Ordering::SeqCst), 3);
    assert_eq!(
        db.get_pv("AO.STAT").unwrap(),
        EpicsValue::Short(alarm_status::NO_ALARM as i16)
    );
}
