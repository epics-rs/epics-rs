//! table's `init_record(1)` reads a link only where C's `GetReadback`,
//! `ReadEncoders` and `GetMotorLimits` read it (tableRecord.c:1094-1136,
//! :1012-1035), one `dbGetLink` at a time in C's order. A PP read processes
//! its passive target (`dbDbLink.c:175-176`), so a skipped read is visible
//! as a target left unprocessed; a self-link sees what pass 1 already wrote.
//! `dbgf` on oracleTableIoc (optics a750b958) after `iocInit`: G:A, G:H and
//! G:C are 0, S:E and S.E0X are 1, S.R0X and S.R0Y are 1.5.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use optics_rs::records::table::TableRecord;

const DB: &str = r#"
record(calc, "G:A") { field(CALC, "VAL+1") }
record(calc, "G:H") { field(CALC, "VAL+1") }
record(ao, "G:M0") { }
record(stringin, "G:S") { field(VAL, "abc") }
record(ao, "G:M1") { }
record(calc, "G:C") { field(CALC, "VAL+1") }
record(table, "G") {
  field(R0XI, "G:A PP")
  field(H0XL, "G:H PP")
  field(M0YL, "G:M0 PP") field(R0YI, "G:S")
  field(M1YL, "G:M1 PP") field(R1YI, "G:C PP")
}
record(ao, "S:M0") { field(VAL, "1.5") }
record(ao, "S:M1") { }
record(calc, "S:E") { field(CALC, "VAL+1") }
record(table, "S") {
  field(R0X, "5")
  field(M0XL, "S:M0 PP") field(R0XI, "S:M0")
  field(M0YL, "S:M1 PP") field(R0YI, "S.R0X")
  field(E0XI, "S:E PP")
}
"#;

#[epics_base_rs::epics_test]
async fn table_init_processes_only_the_pp_targets_c_reads() {
    let (db, _) = IocBuilder::new()
        .register_record_type("table", || Box::new(TableRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_record_type("stringin", || Box::new(StringinRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let val = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    // M0XL unset: no can_RW_drive, so R0XI is never read.
    assert_eq!(val("G:A"), 0.0, "G:A processed");
    // L0XL unset: no can_read_limits, so H0XL is never read.
    assert_eq!(val("G:H"), 0.0, "G:H processed");
    // R0YI fails, and GetReadback returns before R1YI.
    assert_eq!(val("G:C"), 0.0, "G:C processed");
    // A PP link C does read processes its target once.
    assert_eq!((val("S:E"), val("S.E0X")), (1.0, 1.0));
}

#[epics_base_rs::epics_test]
async fn a_table_self_link_reads_the_value_pass_1_wrote() {
    let (db, _) = IocBuilder::new()
        .register_record_type("table", || Box::new(TableRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_record_type("stringin", || Box::new(StringinRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let val = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    // R0YI reads S.R0X after GetReadback read S:M0 into it, not the loaded 5.
    assert_eq!((val("S.R0X"), val("S.R0Y")), (1.5, 1.5));
}
