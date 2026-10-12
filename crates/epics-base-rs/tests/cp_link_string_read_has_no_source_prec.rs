//! A CP/CPP input link is a `CA_LINK` in C whatever its target, and
//! `dbCaGetLink` converts its native value with no source address
//! (`dbCa.c:504-506`): a float read as `DBR_STRING` takes 6 digits, not the
//! source's PREC. An ENUM channel still reads its label (`:465-482`).
//! Pinned to oracleStdIoc on this DB verbatim.

use std::collections::HashMap;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao,"A1"){field(PREC,"1")}
record(stringin,"SCP"){field(INP,"A1 CP")}
record(stringin,"SCPP"){field(INP,"A1 CPP")}
record(stringin,"SNPP"){field(INP,"A1 NPP")}
record(bo,"B"){field(ZNAM,"Off") field(ONAM,"On")}
record(stringin,"SB"){field(INP,"B CP")}
"#;

fn text(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        other => panic!("{pv} = {other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn a_cp_link_reads_a_float_at_six_digits() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.put_record_field_from_ca("A1", "VAL", EpicsValue::Double(0.5))
        .await
        .unwrap();
    db.put_record_field_from_ca("B", "VAL", EpicsValue::Enum(1))
        .await
        .unwrap();
    for rec in ["SCP", "SCPP", "SNPP", "SB"] {
        db.put_record_field_from_ca(rec, "PROC", EpicsValue::Short(1))
            .await
            .unwrap();
    }

    assert_eq!(text(&db, "SCP"), "0.500000");
    assert_eq!(text(&db, "SCPP"), "0.500000");
    assert_eq!(text(&db, "SNPP"), "0.5", "a DB link renders at PREC");
    assert_eq!(text(&db, "SB"), "On");
}
