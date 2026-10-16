//! aoRecord's `process` tests IVOA on the alarm `checkAlarms` left
//! (aoRecord.c:196-217); the device raises its own alarm, and a failed
//! write's WRITE/INVALID, inside `writeValue` (devAsynFloat64.c:666-680), so
//! neither can veto or replace the output. `Set output to IVOV` stores IVOV
//! only on the first pass. The pass that completes an asynchronous write
//! used to raise both alarms ahead of the test, so under IVOA "Set output to
//! IVOV" a device's INVALID turned VAL into IVOV.

mod declared_rows;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::database::{DeviceDoneSource, PvDatabase};
use epics_base_rs::server::device_support::{DeviceAlarm, DeviceSupport, WriteStart};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{DbLinkType, Record};
use epics_base_rs::types::EpicsValue;

type Writes = Arc<Mutex<HashMap<String, Vec<f64>>>>;

/// What leaves the cycle INVALID.
#[derive(Clone, Copy, Debug)]
enum Invalid {
    /// The device's alarm after the write.
    DeviceAlarm,
    /// The write fails.
    WriteFails,
    /// The record's own HIHI, before `writeValue`.
    Hihi,
}

struct Dev {
    key: String,
    writes: Writes,
    pending: bool,
    invalid: Invalid,
    wrote: bool,
}

impl Dev {
    fn record_write(&mut self, record: &dyn Record) -> CaResult<()> {
        let val = record.get_field("VAL").and_then(|v| v.to_f64()).unwrap();
        self.writes
            .lock()
            .unwrap()
            .entry(self.key.clone())
            .or_default()
            .push(val);
        self.wrote = true;
        match self.invalid {
            Invalid::WriteFails => Err(CaError::Protocol("asynError".into())),
            _ => Ok(()),
        }
    }
}

impl DeviceSupport for Dev {
    fn write(&mut self, record: &mut dyn Record) -> CaResult<()> {
        self.record_write(record)
    }

    fn write_begin(
        &mut self,
        record: &mut dyn Record,
        done: &DeviceDoneSource<'_>,
    ) -> CaResult<WriteStart> {
        if !self.pending {
            return Ok(WriteStart::Synchronous);
        }
        let outcome = self.record_write(record);
        done.device_done()
            .complete(done.priority(), outcome)
            .unwrap();
        Ok(WriteStart::Pending)
    }

    fn dtyp(&self) -> &str {
        "ivoaDev"
    }

    fn last_alarm(&self) -> Option<DeviceAlarm<'_>> {
        (self.wrote && matches!(self.invalid, Invalid::DeviceAlarm)).then_some(DeviceAlarm {
            stat: alarm_status::WRITE_ALARM,
            sevr: 3,
            msg: "",
        })
    }
}

fn name(pending: bool, invalid: Invalid, ivoa: u16) -> String {
    format!(
        "{}_{invalid:?}_{ivoa}",
        if pending { "ASYNC" } else { "SYNC" }
    )
}

const CASES: [Invalid; 3] = [Invalid::DeviceAlarm, Invalid::WriteFails, Invalid::Hihi];

async fn idle(db: &PvDatabase, name: &str) {
    for _ in 0..400 {
        if !db.get_record(name).unwrap().read().is_processing() {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("{name} never completed");
}

#[epics_macros_rs::epics_test]
async fn the_ivoa_test_reads_the_alarm_before_the_write() {
    let writes: Writes = Arc::default();
    let mut records = String::new();
    for pending in [false, true] {
        for invalid in CASES {
            for ivoa in 0..3 {
                records += &format!(
                    r#"record(ao, "{}") {{
                        field(DTYP, "ivoaDev") field(OUT, "@{pending} {invalid:?}")
                        field(IVOA, "{ivoa}") field(IVOV, "99")
                        field(HIHI, "10") field(HHSV, "INVALID")
                    }}
                    "#,
                    name(pending, invalid, ivoa)
                );
            }
        }
    }
    let w = Arc::clone(&writes);
    let devices = [("ao", declared_rows::rows(&["ivoaDev"], DbLinkType::InstIo))];
    let (db, _) = IocBuilder::new()
        .register_dynamic_device_support(&devices, move |ctx: &DeviceSupportContext| {
            (ctx.dtyp == "ivoaDev").then(|| {
                let (pending, invalid) = ctx.out.trim_start_matches('@').split_once(' ').unwrap();
                Box::new(Dev {
                    key: ctx.out.to_string(),
                    writes: Arc::clone(&w),
                    pending: pending == "true",
                    invalid: match invalid {
                        "DeviceAlarm" => Invalid::DeviceAlarm,
                        "WriteFails" => Invalid::WriteFails,
                        _ => Invalid::Hihi,
                    },
                    wrote: false,
                }) as Box<dyn DeviceSupport>
            })
        })
        .db_string(&records, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();

    for pending in [false, true] {
        for invalid in CASES {
            for ivoa in 0..3u16 {
                let rec = name(pending, invalid, ivoa);
                writes.lock().unwrap().clear();
                let put = match invalid {
                    Invalid::Hihi => 20.0,
                    _ => 5.0,
                };
                db.put_record_field_from_ca(&rec, "VAL", EpicsValue::Double(put))
                    .await
                    .unwrap();
                idle(&db, &rec).await;

                // The device's own INVALID comes after the test: every IVOA
                // writes VAL and keeps it. The record's HIHI comes before it.
                let (val, written, stat) = match (invalid, ivoa) {
                    (Invalid::Hihi, 0) => (20.0, vec![20.0], alarm_status::HIHI_ALARM),
                    (Invalid::Hihi, 1) => (20.0, vec![], alarm_status::HIHI_ALARM),
                    (Invalid::Hihi, _) => (99.0, vec![99.0], alarm_status::HIHI_ALARM),
                    _ => (5.0, vec![5.0], alarm_status::WRITE_ALARM),
                };
                assert_eq!(
                    db.get_pv(&rec).unwrap(),
                    EpicsValue::Double(val),
                    "{rec} VAL"
                );
                assert_eq!(
                    writes
                        .lock()
                        .unwrap()
                        .get(&format!("@{pending} {invalid:?}"))
                        .cloned()
                        .unwrap_or_default(),
                    written,
                    "{rec} writes"
                );
                assert_eq!(
                    db.get_pv(format!("{rec}.SEVR")).unwrap(),
                    EpicsValue::Short(3),
                    "{rec} SEVR"
                );
                assert_eq!(
                    db.get_pv(format!("{rec}.STAT")).unwrap(),
                    EpicsValue::Short(stat as i16),
                    "{rec} STAT"
                );
            }
        }
    }
}
