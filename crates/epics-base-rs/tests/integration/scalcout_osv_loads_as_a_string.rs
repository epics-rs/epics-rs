//! `sCalcoutRecord.dbd:784` declares OSV `DBF_STRING`, while bi's OSV of
//! the same name is a `menuAlarmSevr`. A string field is never a menu, so
//! the load stores the text; resolving it by name through the shared menu
//! table refused it.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
use epics_base_rs::types::EpicsValue;

async fn load(db: &str) -> EpicsValue {
    IocBuilder::new()
        .register_record_type("scalcout", || -> Box<dyn Record> {
            Box::new(ScalcoutRecord::default())
        })
        .db_string(db, &HashMap::new())
        .unwrap()
        .load()
        .await
        .unwrap()
        .get_pv("R.OSV")
        .unwrap()
}

#[epics_macros_rs::epics_test]
async fn a_loaded_osv_reads_back_its_text() {
    let new = "record(scalcout, \"R\") {\n  field(OSV, \"busy\")\n}\n";
    let merged = format!("record(scalcout, \"R\") {{\n}}\n{new}");
    for db in [new, merged.as_str()] {
        assert_eq!(load(db).await, EpicsValue::String("busy".into()), "{db}");
    }
}
