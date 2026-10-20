//! A record with no DTYP binds its record type's first device. C's `dtyp`
//! is a device-menu index that defaults to 0, and `dbDTYPtoDevSup` returns
//! `ellNth(&prdes->devList, dtyp+1)` (`dbAccess.c:1052-1054`) for both the
//! init dset (`iocInit.c:528`) and the link resolution's `add_record`
//! (`:548`). Base types read Soft Channel there only because `devSoft` is the
//! first `device()` line of their `.dbd`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, SoftDtyp};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, FieldDesc, ProcessOutcome, Record};
use epics_base_rs::types::EpicsValue;

/// A record type whose `.dbd` would declare one device, not a soft one.
struct Probe;

impl Record for Probe {
    fn record_type(&self) -> &'static str {
        "firstdevprobe"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        (name == "VAL").then_some(EpicsValue::Double(0.0))
    }
    fn put_field(&mut self, _name: &str, _value: EpicsValue) -> CaResult<()> {
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }
}

static PROBE_DEVICE: DevSup = DevSup {
    name: "devFirstProbe",
    choice: "First Probe",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[], false),
};

struct ProbeDevice;

impl DeviceSupport for ProbeDevice {
    fn dtyp(&self) -> &str {
        "First Probe"
    }
    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::ok())
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}

#[epics_macros_rs::epics_test]
async fn a_record_with_no_dtyp_gets_the_first_device_of_its_type() {
    let made = Arc::new(AtomicUsize::new(0));
    let counted = made.clone();
    let (db, _) = IocBuilder::new()
        .register_record_type("firstdevprobe", || Box::new(Probe))
        .register_device_support("firstdevprobe", &PROBE_DEVICE, move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::new(ProbeDevice)
        })
        .db_string(
            "record(firstdevprobe, \"P\") { }\nrecord(ai, \"A\") { }\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let p = db.get_record("P").unwrap();
    let p = p.read();
    assert!(p.device.is_some(), "no dset bound for an empty DTYP");
    assert_eq!(p.common.dtyp.soft(), None);
    assert_eq!(made.load(Ordering::SeqCst), 1);

    // devSoft is ai's first device, so its empty DTYP is Soft Channel.
    let a = db.get_record("A").unwrap();
    let a = a.read();
    assert!(a.device.is_none());
    assert_eq!(a.common.dtyp.soft(), Some(SoftDtyp::Plain));
}
