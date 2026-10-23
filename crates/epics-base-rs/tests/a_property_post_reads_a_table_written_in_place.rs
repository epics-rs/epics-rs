//! devOpcua's `updateEnumInfos` writes an mbbi's ZRVL/ZRST through `prec`
//! inside its read, then posts `db_post_events(prec, &prec->val,
//! DBE_PROPERTY)` (devOpcua.cpp:710-732). A client that re-reads
//! `DBR_GR_ENUM` on that event gets the labels just written.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::{AsyncDbHandle, PvDatabase};
use epics_base_rs::server::device_support::{
    DeviceReadOutcome, DeviceSupport, IoIntrAction, PropertyPost,
};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record, ScanType};
use epics_base_rs::types::{DbFieldType, EpicsValue};

struct InPlace {
    name: String,
    db: Option<AsyncDbHandle>,
}

impl DeviceSupport for InPlace {
    fn bind_database(&mut self, db: AsyncDbHandle) {
        self.db = Some(db);
    }
    fn set_record_info(&mut self, name: &str, _: ScanType) {
        self.name = name.to_owned();
    }
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        record.put_field("ZRVL", EpicsValue::ULong(3))?;
        record.put_field("ZRST", EpicsValue::String("Idle".into()))?;
        self.db
            .as_ref()
            .unwrap()
            .run_io_intr_callback_with(&self.name, |held| {
                held.put_property(PropertyPost {
                    writes: Vec::new(),
                    post_field: "VAL".into(),
                })
                .unwrap();
                IoIntrAction::Skip
            });
        Ok(DeviceReadOutcome::ok())
    }
    fn write(&mut self, _: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
    fn dtyp(&self) -> &str {
        "inplace"
    }
}

static ROWS: [DevSup; 1] = [DevSup {
    name: "devInPlace",
    choice: "inplace",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(5, &[2, 4], false),
}];

async fn ioc() -> std::sync::Arc<PvDatabase> {
    let devices: Vec<(&str, &[DevSup])> = vec![("mbbi", &ROWS[..])];
    let ioc = IocBuilder::new()
        .register_dynamic_device_support(&devices, |ctx: &DeviceSupportContext| {
            (ctx.dtyp == "inplace").then(|| {
                Box::new(InPlace {
                    name: String::new(),
                    db: None,
                }) as Box<dyn DeviceSupport>
            })
        })
        .db_string(
            r#"record(mbbi, "E") { field(DTYP, "inplace") field(INP, "@x") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    db
}

fn labels(db: &PvDatabase) -> Vec<String> {
    let cell = db.get_record("E").unwrap();
    db.channel_snapshot_for_field(&cell, "VAL", false)
        .and_then(|s| s.enums)
        .map(|e| e.strings.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default()
}

#[epics_macros_rs::epics_test]
async fn the_labels_a_property_post_announces_are_the_ones_read_on_it() {
    let db = ioc().await;
    assert!(labels(&db).iter().all(String::is_empty));
    let mut reader = db
        .get_record("E")
        .unwrap()
        .write()
        .add_subscriber("VAL", 1, DbFieldType::Enum, EventMask::PROPERTY.bits())
        .unwrap();
    while reader.try_recv().is_ok() {}
    db.put_record_field_from_ca_no_notify("E", "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    for _ in 0..400 {
        if reader.try_recv().is_ok() {
            assert_eq!(labels(&db).first().map(String::as_str), Some("Idle"));
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("no PROPERTY event");
}
