//! A readback cycle's UDF belongs to the device: only what the device says
//! at arm time (devAsyn's `pr->udf = 0` before `dbProcess`,
//! devAsynInt32.c:631) reaches `checkAlarms`. A device whose C dset clears UDF
//! inside its write, after `checkAlarms` (devOpcua's `readScalar`,
//! DataElementOpen62541Leaf.h:718), leaves a UDF stringout at UDF/INVALID on
//! that pass (stringoutRecord.c:146-148).

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::recgbl::alarm_status::UDF_ALARM;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::server::records::stringout::StringoutRecord;
use epics_base_rs::types::EpicsValue;

/// Takes an update in its read, as devOpcua's write dset does, and says
/// `read_udf` about UDF there; nothing at arm time.
struct ReadsInItsWrite {
    read_udf: DeviceUdf,
}

impl DeviceSupport for ReadsInItsWrite {
    fn dtyp(&self) -> &str {
        "MockDev"
    }
    fn output_callback_readback(&self) -> bool {
        true
    }
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        if self.read_udf == DeviceUdf::Defined {
            record.put_field("VAL", EpicsValue::String("x".into()))?;
        }
        Ok(DeviceReadOutcome::computed(self.read_udf))
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}

async fn after_one_readback(read_udf: DeviceUdf) -> (u8, u16, AlarmSeverity) {
    let db = PvDatabase::new();
    db.add_record("SO", Box::new(StringoutRecord::new("")))
        .await
        .unwrap();
    {
        let rec = db.get_record("SO").unwrap();
        let mut inst = rec.write();
        assert_eq!(inst.common.udf, 1);
        inst.common.dtyp = "MockDev".into();
        inst.device = Some(Box::new(ReadsInItsWrite { read_udf }));
    }
    db.process_record_readback("SO", &mut ProcStack::new())
        .await
        .unwrap();
    let rec = db.get_record("SO").unwrap();
    let inst = rec.read();
    (inst.common.udf, inst.common.stat, inst.common.sevr)
}

#[epics_base_rs::epics_test]
async fn a_value_landing_after_the_alarm_check_leaves_the_udf_alarm() {
    assert_eq!(
        after_one_readback(DeviceUdf::Defined).await,
        (0, UDF_ALARM, AlarmSeverity::Invalid)
    );
}

#[epics_base_rs::epics_test]
async fn an_update_without_a_value_leaves_udf_set() {
    assert_eq!(
        after_one_readback(DeviceUdf::Untouched).await,
        (1, UDF_ALARM, AlarmSeverity::Invalid)
    );
}
