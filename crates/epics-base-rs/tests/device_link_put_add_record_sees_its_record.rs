//! `dbPutFieldLink` calls the new dset's `add_record(precord)` under the
//! record's lock, after `dset = new_dset` (`dbAccess.c:1192-1196`), so an
//! extending device such as devEtherIP reads its record (name, SCAN, NELM)
//! there to bind the new link.

use std::collections::HashMap;
use std::sync::Mutex;

use epics_base_rs::error::{CaResult, LinkRefusal};
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record, ScanType};
use epics_base_rs::types::EpicsValue;

static EXT: [DevSup; 1] = [DevSup {
    name: "devAiExtProbe",
    choice: "Ext Probe",
    link_type: DbLinkType::InstIo,
    dset: Dset::new(6, &[2, 4], true),
}];

/// Each `add_record`'s (record name, PREC, INP the object was built with).
static ADDED: Mutex<Vec<(String, Option<EpicsValue>, String)>> = Mutex::new(Vec::new());

struct Probe {
    inp: String,
    name: String,
}

impl DeviceSupport for Probe {
    fn dtyp(&self) -> &str {
        "Ext Probe"
    }
    fn set_record_info(&mut self, name: &str, _scan: ScanType) {
        self.name = name.to_string();
    }
    fn add_record(&mut self, record: &mut dyn Record) -> Result<(), LinkRefusal> {
        ADDED.lock().unwrap().push((
            self.name.clone(),
            record.get_field("PREC"),
            self.inp.clone(),
        ));
        Ok(())
    }
    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}

#[epics_macros_rs::epics_test]
async fn a_device_link_put_runs_add_record_on_its_record() {
    let (db, _) = IocBuilder::new()
        .register_dynamic_device_support(&[("ai", &EXT)], |ctx| {
            (ctx.dtyp == "Ext Probe").then(|| {
                Box::new(Probe {
                    inp: ctx.inp.to_string(),
                    name: String::new(),
                }) as Box<dyn DeviceSupport>
            })
        })
        .db_string(
            r#"record(ai, "AX") { field(DTYP, "Ext Probe") field(INP, "@a") field(PREC, "3") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    ADDED.lock().unwrap().clear();
    db.put_record_field_from_ca("AX", "INP", EpicsValue::String("@b".into()))
        .await
        .unwrap();
    let added = ADDED.lock().unwrap().clone();
    assert_eq!(
        added,
        [(
            "AX".to_string(),
            Some(EpicsValue::Short(3)),
            "@b".to_string()
        )]
    );
}
