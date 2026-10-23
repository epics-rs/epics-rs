//! A CP/CPP input link is a `CA_LINK` in C whatever its target, and
//! `dbCaGetLink` converts its native value with no source address
//! (`dbCa.c:504-506`): a float read as `DBR_STRING` takes 6 digits, not the
//! source's PREC. An ENUM channel still reads its label (`:465-482`).
//! Pinned to oracleStdIoc on this DB verbatim.

mod calink_ioc;

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
    calink_ioc::connect(&db).await;
    db.put_record_field_from_ca("A1", "VAL", EpicsValue::Double(0.5))
        .await
        .unwrap();
    db.put_record_field_from_ca("B", "VAL", EpicsValue::Enum(1))
        .await
        .unwrap();
    // The source posts process the CP/CPP holders; the NPP one is read here.
    db.put_record_field_from_ca("SNPP", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    for (rec, want) in [("SCP", "0.500000"), ("SCPP", "0.500000"), ("SB", "On")] {
        calink_ioc::until(&format!("{rec} reads {want}"), || text(&db, rec) == want).await;
    }

    assert_eq!(text(&db, "SNPP"), "0.5", "a DB link renders at PREC");
}
