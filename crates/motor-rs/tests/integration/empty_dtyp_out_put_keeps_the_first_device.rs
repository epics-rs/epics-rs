//! An OUT put to a motor record with no DTYP. C's `dbPutFieldLink` takes its
//! new dset from `dbDTYPtoDevSup(rdes, dtyp)` again (`dbAccess.c:1125-1130`),
//! and an unset DTYP is menu index 0, so the put keeps row 0's device exactly
//! as a put to a record naming that choice does. motor's `.dbd` declares no
//! device, so the probe's row is row 0; a CONSTANT row extends with devSoft's
//! dsxt, whose `add_record` binds nothing.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};
use epics_base_rs::types::EpicsValue;

static PROBE: DevSup = DevSup {
    name: "devMotorProbe",
    choice: "Motor Probe",
    link_type: DbLinkType::Constant,
    dset: Dset::new(6, &[2, 4, 5], false),
};

struct ProbeDevice;

impl DeviceSupport for ProbeDevice {
    fn dtyp(&self) -> &str {
        "Motor Probe"
    }
    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::ok())
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}

#[epics_base_rs::epics_test]
async fn an_out_put_keeps_the_row_0_device_of_an_empty_dtyp() {
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
             record(motor, \"Q\") { field(DTYP, \"Motor Probe\") field(OUT, \"1\") }\n",
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
    assert_eq!(made.load(Ordering::SeqCst), 2, "a put rebuilt the device");
}
