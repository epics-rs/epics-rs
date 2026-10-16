//! C binds the dset of the row DTYP selects (`iocInit.c:530-533`, and
//! `dbAccess.c:1128` on a device-link put) and never reads the choice's
//! spelling, so a row registered with its device object is served by that
//! object even when its choice reads like a base soft channel — mca's
//! `device(mca,CONSTANT,devMCA_soft,"Soft Channel")`. A row registered without
//! an object stays the framework's soft channel.

use std::collections::HashMap;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{
    DeviceReadOutcome, DeviceSupport, DeviceUdf, SoftDtyp,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record, register_device_row};
use epics_base_rs::types::EpicsValue;

/// A choice base's waveform menu does not list, spelled as a soft channel.
static OBJECT_ROW: DevSup = DevSup {
    name: "devWfProbe",
    choice: "Raw Soft Channel",
    link_type: DbLinkType::Constant,
    dset: Dset::new(5, &[], false),
};

/// The same spelling on aai, as a menu row only.
static PLAIN_ROW: DevSup = DevSup {
    name: "devAaiProbe",
    choice: "Raw Soft Channel",
    link_type: DbLinkType::Constant,
    dset: Dset::new(5, &[], false),
};

struct Probe;

impl DeviceSupport for Probe {
    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "Raw Soft Channel"
    }
}

const DB: &str = r#"
record(waveform, "A") { field(NELM, "2") }
record(waveform, "B") { field(NELM, "2") }
record(waveform, "WO") { field(DTYP, "Raw Soft Channel") field(INP, "A") field(NELM, "2") }
record(aai, "WR") { field(DTYP, "Raw Soft Channel") field(INP, "A") field(NELM, "2") }
"#;

#[epics_macros_rs::epics_test]
async fn the_row_decides_whether_a_soft_spelling_is_served_by_an_object() {
    register_device_row("aai", &PLAIN_ROW);
    let (db, _) = IocBuilder::new()
        .register_device_support("waveform", &OBJECT_ROW, || Box::new(Probe))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let state = |name: &str| {
        let rec = db.get_record(name).unwrap();
        let inst = rec.read();
        (inst.device.is_some(), inst.common.dtyp.soft())
    };
    assert_eq!(state("WO"), (true, None));
    assert_eq!(state("WR"), (false, Some(SoftDtyp::Raw)));

    // A CONSTANT row's dsxt is `devSoft_DSXT`, whose `add_record` binds
    // nothing: the object stays through the put.
    for name in ["WO", "WR"] {
        let bound = state(name);
        db.put_record_field_from_ca_no_notify(name, "INP", EpicsValue::String("B".into()))
            .await
            .unwrap();
        assert_eq!(state(name), bound, "{name}");
    }
}
