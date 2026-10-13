//! A record type that stores a field whose name dbCommon's arms also answer
//! (sel HYST, acalcout OUT, dfanout OUTN, busy HIGH) reads back its own
//! field after a put, not the instance's unused copy. Pinned to oracleIoc /
//! oracleBusyIoc `dbpf` then `dbgf`.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::{acalcout::AcalcoutRecord, busy::BusyRecord};
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(sel, "S") {}
record(acalcout, "AC") {}
record(dfanout, "DF") {}
record(busy, "BZ") {}
"#;

async fn put(db: &PvDatabase, rec: &str, field: &str, value: EpicsValue) {
    db.put_record_field_from_ca(rec, field, value)
        .await
        .unwrap_or_else(|e| panic!("{rec}.{field}: {e:?}"));
}

#[epics_macros_rs::epics_test]
async fn an_own_named_common_field_reads_back_its_put() {
    // acalcout and busy are module types, registered as the oracle IOCs' dbds do.
    let db = IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    put(&db, "S", "HYST", EpicsValue::Double(5.0)).await;
    put(&db, "S", "HIHI", EpicsValue::Double(7.0)).await;
    put(&db, "AC", "HYST", EpicsValue::Double(6.0)).await;
    put(&db, "AC", "OUT", EpicsValue::String("Y.VAL".into())).await;
    put(&db, "DF", "OUTN", EpicsValue::String("X.VAL".into())).await;
    put(&db, "BZ", "HIGH", EpicsValue::Double(2.5)).await;
    let want = [
        ("S.HYST", EpicsValue::Double(5.0)),
        ("S.HIHI", EpicsValue::Double(7.0)),
        ("AC.HYST", EpicsValue::Double(6.0)),
        ("AC.OUT", EpicsValue::String("Y.VAL NPP NMS".into())),
        ("DF.OUTN", EpicsValue::String("X.VAL NPP NMS".into())),
        ("BZ.HIGH", EpicsValue::Double(2.5)),
    ];
    let got = want.clone().map(|(pv, _)| (pv, db.get_pv(pv).unwrap()));
    assert_eq!(got, want);
}
