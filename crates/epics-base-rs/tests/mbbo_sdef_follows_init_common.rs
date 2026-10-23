//! mbbo SDEF is set by `init_common`, at pass 0 and after the dset's `init_record`
//! (`mbboRecord.c:115,141`), and again by `special()` after a put to a state
//! field (`:286`). softIoc with this DB reads the SDEF values below.

mod probe_rows;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{
    DeviceInitOutcome, DeviceReadOutcome, DeviceSupport, DeviceUdf,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;

const DB: &str = r#"
record(mbbo, "A") { }
record(mbbo, "B") { field(ONVL, "3") }
record(mbbo, "D") { field(ONST, "x") }
"#;

#[epics_macros_rs::epics_test]
async fn a_state_field_put_recomputes_sdef_mbborecord_286() {
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let sdef = |rec: &str| db.get_pv(format!("{rec}.SDEF")).ok();
    let put = |rec: &'static str, f: &'static str, v: EpicsValue| {
        let db = db.clone();
        async move {
            db.put_record_field_from_ca_no_notify(rec, f, v)
                .await
                .unwrap()
        }
    };

    assert_eq!(sdef("A"), Some(EpicsValue::Short(0)));
    assert_eq!(sdef("B"), Some(EpicsValue::Short(1)));
    assert_eq!(sdef("D"), Some(EpicsValue::Short(1)));

    put("A", "ZRVL", EpicsValue::ULong(5)).await;
    assert_eq!(sdef("A"), Some(EpicsValue::Short(1)));
    put("A", "ZRVL", EpicsValue::ULong(0)).await;
    assert_eq!(sdef("A"), Some(EpicsValue::Short(0)));
    put("A", "FFST", EpicsValue::String("y".into())).await;
    assert_eq!(sdef("A"), Some(EpicsValue::Short(1)));
    put("B", "ONVL", EpicsValue::ULong(0)).await;
    assert_eq!(sdef("B"), Some(EpicsValue::Short(0)));
}

/// A dset whose `init_record` writes the state table and the readback, as
/// asyn's `setEnums` from `initCommon` does.
struct Enums;

impl DeviceSupport for Enums {
    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        record.put_field("ONVL", EpicsValue::ULong(7))?;
        record.put_field("RVAL", EpicsValue::ULong(7))?;
        Ok(DeviceInitOutcome::ok())
    }

    fn read(&mut self, _record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        Ok(DeviceReadOutcome::computed(DeviceUdf::Defined))
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "probeLive"
    }
}

/// `mbboRecord.c:141` recomputes SDEF before the readback conversion at
/// `:143-160`, so the RVAL the dset left resolves through the table it
/// wrote: ONVL 7 is state 1, where the pass-0 SDEF of 0 would give VAL 7.
#[epics_macros_rs::epics_test]
async fn a_table_the_dset_writes_converts_its_readback_mbborecord_141() {
    let (db, _h) = IocBuilder::new()
        .register_dynamic_device_support(
            &probe_rows::devices(&["mbbo"]),
            |ctx: &DeviceSupportContext| {
                (ctx.dtyp == "probeLive").then(|| Box::new(Enums) as Box<dyn DeviceSupport>)
            },
        )
        .db_string(
            r#"record(mbbo, "E") { field(DTYP, "probeLive") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    assert_eq!(db.get_pv("E.SDEF").ok(), Some(EpicsValue::Short(1)));
    assert_eq!(db.get_pv("E.VAL").ok(), Some(EpicsValue::Enum(1)));
}

/// asyn's runtime `interruptCallbackEnumMbbo` (devAsynInt32.c:726-737) runs
/// `setEnums` with no `init_common`, so SDEF keeps its init value in C.
#[epics_macros_rs::epics_test]
async fn a_runtime_enum_table_leaves_sdef_as_c_does() {
    use epics_base_rs::server::device_support::PropertyPost;
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let post = |rec: &str, f: &str, v: EpicsValue| {
        db.post_property(
            rec,
            PropertyPost {
                writes: vec![(f.to_string(), v)],
                post_field: "VAL".to_string(),
            },
        )
        .unwrap();
    };
    post("A", "ONVL", EpicsValue::ULong(3));
    assert_eq!(db.get_pv("A.SDEF").ok(), Some(EpicsValue::Short(0)));
    post("B", "ONVL", EpicsValue::ULong(0));
    assert_eq!(db.get_pv("B.SDEF").ok(), Some(EpicsValue::Short(1)));
}
