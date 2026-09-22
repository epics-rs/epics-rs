//! A device support that finished the write inside `write_begin` says so
//! with `WriteStart::Completed`, and the framework runs no `write()` after
//! it. `Ok(None)` used to mean both "completed here" and "no async path",
//! so a completed write was followed by a second, synchronous one.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{DeviceSupport, WriteStart};
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

struct CompletesInBegin {
    writes: Arc<AtomicUsize>,
}

impl DeviceSupport for CompletesInBegin {
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        panic!("write() after WriteStart::Completed is the second write");
    }
    fn write_begin(&mut self, _record: &mut dyn Record) -> CaResult<WriteStart> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(WriteStart::Completed)
    }
    fn dtyp(&self) -> &str {
        "CompletesInBegin"
    }
}

#[epics_macros_rs::epics_test]
async fn a_write_completed_in_write_begin_is_not_written_again() {
    let writes = Arc::new(AtomicUsize::new(0));
    let db = PvDatabase::new();
    db.add_record("AO", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("AO").unwrap();
        let mut inst = rec.write();
        inst.common.dtyp = "CompletesInBegin".into();
        inst.common.udf = 0;
        inst.device = Some(Box::new(CompletesInBegin {
            writes: writes.clone(),
        }));
    }
    db.put_record_field_from_ca_no_notify("AO", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    for _ in 0..500 {
        {
            let rec = db.get_record("AO").unwrap();
            let inst = rec.read();
            if !inst.is_processing() {
                assert_eq!(inst.common.sevr, AlarmSeverity::NoAlarm);
                assert_eq!(inst.common.stat, alarm_status::NO_ALARM);
                assert_eq!(writes.load(Ordering::SeqCst), 1);
                return;
            }
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("the cycle never ended");
}
