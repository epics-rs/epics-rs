//! A device that writes PREC from its cycle `read()` — no lend, no field
//! writer — is served the PREC it wrote, as C's `dbGet` reads
//! `get_precision` off the record on every call.

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;

struct WritesPrec;

impl DeviceSupport for WritesPrec {
    fn dtyp(&self) -> &str {
        "MockDev"
    }
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        record.put_field("PREC", EpicsValue::Short(5))?;
        Ok(DeviceReadOutcome::ok())
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}

fn served_precision(db: &PvDatabase) -> Option<i16> {
    let cell = db.get_record("AI").unwrap();
    db.channel_snapshot_for_field(&cell, "VAL", false)
        .unwrap()
        .precision()
}

#[epics_base_rs::epics_test]
async fn a_prec_its_device_writes_in_the_cycle_is_served() {
    let db = PvDatabase::new();
    db.add_record("AI", Box::new(AiRecord::default()))
        .await
        .unwrap();
    {
        let rec = db.get_record("AI").unwrap();
        let mut inst = rec.write();
        inst.common.dtyp = "MockDev".into();
        inst.device = Some(Box::new(WritesPrec));
    }
    assert_eq!(served_precision(&db), Some(0));
    db.process_record("AI").await.unwrap();
    assert_eq!(served_precision(&db), Some(5));
}
