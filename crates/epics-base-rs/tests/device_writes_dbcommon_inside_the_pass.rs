//! C's dset entries take `dbCommon*` and write it in the pass:
//! StreamEpics.cc `Stream::process` clears `proc`, `protocolFinishHook`
//! writes `udf`, and `recGblSetSevr` raises `nsev`. What a device writes
//! through the `_common` forms is the record's when the pass returns.

use std::collections::HashMap;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{
    AlarmSeverity, CommonFields, DbLinkType, DevSup, Dset, Record,
};
use epics_base_rs::types::EpicsValue;

static IN: DevSup = DevSup {
    name: "devLiCommon",
    choice: "CommonIn",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[2, 3, 4], false),
};

static OUT: DevSup = DevSup {
    name: "devLoCommon",
    choice: "CommonOut",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[2, 3, 4], false),
};

/// Writes PROC, and UDF or a raised alarm: a UDF left set raises the
/// record's own UDF alarm, which would hide the other.
struct Writes {
    udf: bool,
}

impl Writes {
    fn touch(&self, common: &mut CommonFields) {
        common.proc_field = 0;
        if self.udf {
            // A raw byte no record rule writes, so it can only be the device's.
            common.udf = 2;
        } else {
            common.nsta = 1; // READ_ALARM
            common.nsev = AlarmSeverity::Major;
        }
    }
}

impl DeviceSupport for Writes {
    fn read_common(
        &mut self,
        _record: &mut dyn Record,
        common: &mut CommonFields,
    ) -> CaResult<DeviceReadOutcome> {
        self.touch(common);
        // No value: the record's own UDF rule stays out of it.
        Ok(DeviceReadOutcome::no_value(DeviceUdf::Untouched))
    }

    fn write_common(
        &mut self,
        _record: &mut dyn Record,
        common: &mut CommonFields,
    ) -> CaResult<()> {
        self.touch(common);
        Ok(())
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        unreachable!("the framework calls write_common")
    }

    fn dtyp(&self) -> &str {
        "Common"
    }
}

const DB: &str = r#"record(longin, "I") { field(DTYP, "CommonIn") field(INP, "@x") }
record(longout, "O") { field(DTYP, "CommonOut") field(OUT, "@x") }
"#;

#[epics_macros_rs::epics_test]
async fn common_writes_in_the_pass_are_the_record_s_after_it() {
    let (db, _) = IocBuilder::new()
        .register_device_support("longin", &IN, || Box::new(Writes { udf: false }))
        .register_device_support("longout", &OUT, || Box::new(Writes { udf: true }))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for name in ["I", "O"] {
        if name == "I" {
            // A read that stores nothing leaves UDF, whose alarm would win.
            db.get_record(name).unwrap().write().common.udf = 0;
        }
        // The read stored no value, so the put answers the pass's ERROR.
        let _ = db
            .put_record_field_from_ca_no_notify(name, "PROC", EpicsValue::String("2".into()))
            .await;
        let rec = db.get_record(name).unwrap();
        let c = &rec.read().common;
        assert_eq!(
            (c.proc_field, c.udf, c.stat, c.sevr),
            if name == "I" {
                (0, 0, 1, AlarmSeverity::Major)
            } else {
                (0, 2, 17, AlarmSeverity::Invalid) // UDF_ALARM
            },
            "{name}: PROC UDF STAT SEVR after the pass"
        );
    }
}
