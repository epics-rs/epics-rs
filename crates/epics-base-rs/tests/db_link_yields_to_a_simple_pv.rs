//! A simple PV registered under a name that spells a record's field
//! (`REC.FLD`) takes an OUT link's put and answers an input link's read ahead
//! of the field, as `put_pv` and `get_pv` give it precedence — including one
//! registered after the link already cached the record's address.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;

const DB: &str = r#"
record(longout, "SRC") { field(VAL, "7") field(OUT, "DST.A NPP") }
record(calc, "DST") { field(CALC, "A") }
record(ai, "RD") { field(INP, "DST.B NPP") }
"#;

#[epics_macros_rs::epics_test]
async fn an_out_link_puts_to_the_simple_pv_that_spells_its_target() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let f64_of = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    db.process_record("SRC").await.unwrap();
    assert_eq!(f64_of("DST.A"), 7.0, "no shadow yet: the record field");

    db.add_pv("DST.A", EpicsValue::Long(0)).await.unwrap();
    db.put_pv("SRC", EpicsValue::Long(9)).await.unwrap();
    db.process_record("SRC").await.unwrap();
    assert_eq!(f64_of("DST.A"), 9.0, "the simple PV took the put");
    let field = db
        .get_record("DST")
        .unwrap()
        .read()
        .resolve_field("A")
        .unwrap();
    assert_eq!(
        field.to_f64().unwrap(),
        7.0,
        "the shadowed field is untouched"
    );
}

#[epics_macros_rs::epics_test]
async fn an_input_link_reads_the_simple_pv_that_spells_its_target() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let f64_of = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    db.put_pv("DST.B", EpicsValue::Double(3.0)).await.unwrap();
    db.process_record("RD").await.unwrap();
    assert_eq!(f64_of("RD"), 3.0, "no shadow yet: the record field");

    db.add_pv("DST.B", EpicsValue::Double(5.0)).await.unwrap();
    db.process_record("RD").await.unwrap();
    assert_eq!(f64_of("RD"), 5.0, "the simple PV answered the read");
}
