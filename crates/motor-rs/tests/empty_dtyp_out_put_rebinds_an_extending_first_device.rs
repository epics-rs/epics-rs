//! [`empty_dtyp_out_put_keeps_the_first_device`]'s put where row 0's dset
//! extends with a dsxt of its own: `dbPutFieldLink` runs that dsxt's
//! `add_record` (`dbAccess.c:1196`), so a record with no DTYP gets row 0's
//! device for its new link as one naming the choice does. Its own binary, as
//! the device menu is process-wide and row 0 differs.
//!
//! [`empty_dtyp_out_put_keeps_the_first_device`]: ./empty_dtyp_out_put_keeps_the_first_device.rs

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};
use epics_base_rs::types::EpicsValue;

static PROBE: DevSup = DevSup {
    name: "devMotorExtProbe",
    choice: "Motor Ext Probe",
    link_type: DbLinkType::Constant,
    dset: Dset::new(6, &[2, 4, 5], true),
};

struct ProbeDevice;

impl DeviceSupport for ProbeDevice {
    fn dtyp(&self) -> &str {
        "Motor Ext Probe"
    }
    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::ok())
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}

#[epics_base_rs::epics_test]
async fn an_out_put_rebinds_the_row_0_device_of_an_empty_dtyp() {
    let made = Arc::new(AtomicUsize::new(0));
    let counted = made.clone();
    let (name, factory) = motor_rs::motor_record_factory();
    let (db, _) = IocBuilder::new()
        .register_device_support("motor", &PROBE, move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::new(ProbeDevice)
        })
        .register_record_type(name, factory)
        .db_string(
            "record(motor, \"P\") { field(OUT, \"1\") }\n\
             record(motor, \"Q\") { field(DTYP, \"Motor Ext Probe\") field(OUT, \"1\") }\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    assert_eq!(made.load(Ordering::SeqCst), 2);

    for record in ["Q", "P"] {
        db.put_record_field_from_ca_no_notify(record, "OUT", EpicsValue::String("2".into()))
            .await
            .unwrap();
        let rec = db.get_record(record).unwrap();
        assert!(rec.read().device.is_some(), "{record} lost its device");
    }
    assert_eq!(made.load(Ordering::SeqCst), 4, "each put binds a device");
}
