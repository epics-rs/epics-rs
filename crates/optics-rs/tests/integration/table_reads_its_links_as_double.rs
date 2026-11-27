//! table reads every input link at `DBR_DOUBLE`, as each of its `dbGetLink`
//! calls asks (`tableRecord.c:978`, `:1023-1028`, `:1106`, `:1130`), so a
//! stringin holding "5" reads as 5. oracleTableIoc, with this db and `caput
//! T.READ 1`, posts XRB and EX 5.28149 with no alarm.

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the exec
// backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread`); no CA server is built.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::types::EpicsValue;
use optics_rs::records::table::TableRecord;

const DB: &str = r#"
record(stringin, "T:S") {
  field(VAL, "5")
}
record(ao, "T:MX") {
}
record(table, "T") {
  field(LX, "100")
  field(LZ, "200")
  field(M0XL, "T:MX")
  field(R0XI, "T:S")
  field(E0XI, "T:S")
}
"#;

#[tokio::test]
async fn table_reads_a_string_source_as_a_double() {
    let (db, _) = IocBuilder::new()
        .register_record_type("table", || Box::new(TableRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_record_type("stringin", || Box::new(StringinRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db.put_record_field_from_ca("T", "READ", EpicsValue::String("1".into()))
        .await
        .unwrap();
    assert_eq!(db.get_pv("T.R0X").unwrap(), EpicsValue::Double(5.0));
    assert_eq!(db.get_pv("T.E0X").unwrap(), EpicsValue::Double(5.0));
    assert_eq!(db.get_pv("T.SEVR").unwrap().to_f64(), Some(0.0));
}
