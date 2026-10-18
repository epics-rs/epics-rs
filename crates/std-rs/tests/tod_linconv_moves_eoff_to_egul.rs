//! `devAiTodSeconds` fills `special_linconv` with `no_op`
//! (devTimeOfDay.c:93, :106-112), so a LINEAR "Sec Past Epoch" ai still
//! gets the record's EOFF = EGUL on a LINR/EGUL put (aiRecord.c special).
//! Expected values are oracleIoc (8f5015b66) with the same records and puts.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ai, "TOD:LIN") { field(DTYP, "Sec Past Epoch") field(LINR, "LINEAR") field(EGUL, "5") field(EGUF, "10") field(ESLO, "2") field(EOFF, "3") }
record(ai, "TOD:NOC") { field(DTYP, "Sec Past Epoch") field(LINR, "NO CONVERSION") field(EGUL, "5") field(EGUF, "10") field(ESLO, "2") field(EOFF, "3") }
"#;

async fn ioc() -> Arc<PvDatabase> {
    let mut builder = IocBuilder::new();
    for (record_type, devsup, factory) in std_rs::std_device_supports() {
        builder = builder.register_device_support(record_type, devsup, factory);
    }
    builder
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn eslo_eoff(db: &PvDatabase, name: &str) -> (f64, f64) {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    let f = |field| {
        inst.record
            .get_field(field)
            .and_then(|v| v.to_f64())
            .unwrap()
    };
    (f("ESLO"), f("EOFF"))
}

async fn put(db: &PvDatabase, name: &str, field: &str, value: EpicsValue) {
    db.put_record_field_from_ca(name, field, value)
        .await
        .unwrap();
}

#[epics_base_rs::epics_test]
async fn linear_egul_and_linr_puts_move_eoff_to_egul() {
    let db = ioc().await;
    for r in ["TOD:LIN", "TOD:NOC"] {
        assert_eq!(eslo_eoff(&db, r), (2.0, 3.0), "{r} after init");
    }
    for r in ["TOD:LIN", "TOD:NOC"] {
        put(&db, r, "EGUL", EpicsValue::Double(7.0)).await;
    }
    assert_eq!(
        eslo_eoff(&db, "TOD:LIN"),
        (2.0, 7.0),
        "TOD:LIN after EGUL 7"
    );
    assert_eq!(
        eslo_eoff(&db, "TOD:NOC"),
        (2.0, 3.0),
        "TOD:NOC after EGUL 7"
    );
    put(&db, "TOD:NOC", "LINR", EpicsValue::Short(2)).await;
    assert_eq!(
        eslo_eoff(&db, "TOD:NOC"),
        (2.0, 7.0),
        "TOD:NOC after LINR LINEAR"
    );
}
