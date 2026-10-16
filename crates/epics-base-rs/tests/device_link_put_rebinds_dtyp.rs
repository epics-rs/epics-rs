//! A runtime INP/OUT put re-takes the device support from the current DTYP
//! (`dbPutFieldLink`'s `isDevLink` arm); a DTYP put alone does not. Pinned to
//! softIoc R7.0.10 with ASLO 2 / AOFF 1.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ai, "DS") { field(VAL, "7") }
record(ai, "DA") { field(INP, "DS NPP") field(ASLO, "2") field(AOFF, "1") }
record(ai, "DC") { field(INP, "DS NPP") field(ASLO, "2") field(AOFF, "1") }
record(ao, "DO") { field(OUT, "T NPP") field(ASLO, "2") field(AOFF, "1") field(VAL, "9") }
record(ao, "T") {}
record(ao, "T2") {}
"#;

async fn put(db: &PvDatabase, rec: &str, field: &str, text: &str) {
    db.put_record_field_from_ca(rec, field, EpicsValue::String(text.into()))
        .await
        .unwrap();
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

fn get(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn a_device_link_put_binds_the_dtyp_put_before_it_like_softioc() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    put(&db, "DA", "DTYP", "Raw Soft Channel").await;
    put(&db, "DA", "INP", "DS NPP").await;
    process(&db, "DA").await;
    assert_eq!(get(&db, "DA.RVAL"), 7.0, "DA.RVAL");

    put(&db, "DC", "DTYP", "Raw Soft Channel").await;
    process(&db, "DC").await;
    assert_eq!(get(&db, "DC.VAL"), 7.0, "DC.VAL");
    assert_eq!(get(&db, "DC.RVAL"), 0.0, "DC.RVAL");

    put(&db, "DO", "DTYP", "Raw Soft Channel").await;
    put(&db, "DO", "OUT", "T2 NPP").await;
    process(&db, "DO").await;
    assert_eq!(get(&db, "DO.RVAL"), 4.0, "DO.RVAL");
    assert_eq!(get(&db, "T2.VAL"), 4.0, "T2.VAL raw");

    // Back to Soft Channel: still bound Raw until OUT is written again.
    put(&db, "DO", "DTYP", "Soft Channel").await;
    process(&db, "DO").await;
    assert_eq!(get(&db, "T2.VAL"), 4.0, "T2.VAL after DTYP only");
    put(&db, "DO", "OUT", "T2 NPP").await;
    process(&db, "DO").await;
    assert_eq!(get(&db, "T2.VAL"), 9.0, "T2.VAL after OUT put");
}

/// `dbAccess.c:1149-1153`: a device with no extension table refuses the INP
/// put, so the record keeps INP and its soft dset. softIoc: DG.INP stays
/// "DS NPP NMS" and DG.VAL reads 7 with NO_ALARM.
#[epics_macros_rs::epics_test]
async fn a_device_link_put_to_a_device_without_dsxt_is_refused_like_softioc() {
    let db = IocBuilder::new()
        .db_string(
            r#"
record(ai, "DS") { field(VAL, "7") }
record(ai, "DG") { field(INP, "DS NPP") }
"#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    put(&db, "DG", "DTYP", "General Time").await;
    let refused = db
        .put_record_field_from_ca("DG", "INP", EpicsValue::String("@TIME".into()))
        .await;
    assert!(
        matches!(refused, Err(epics_base_rs::error::CaError::NoSupport)),
        "{refused:?}"
    );
    let inp = db.get_pv("DG.INP").unwrap();
    assert_eq!(inp, EpicsValue::String("DS NPP NMS".into()), "DG.INP");
    process(&db, "DG").await;
    assert_eq!(get(&db, "DG.VAL"), 7.0, "DG.VAL");
    assert_eq!(get(&db, "DG.SEVR"), 0.0, "DG.SEVR");
}

/// The same refusal for a device a downstream crate registers: asyn's
/// `device(ai,INST_IO,asynAiInt32,"asynInt32")` has no `devExtend`. Pinned to
/// oracleScalerIoc (base + asyn + scaler): `dbpf DG.DTYP asynInt32` then
/// `dbpf DG.INP "@asyn(scalerPort 0 0)"` prints nothing, DG.INP stays
/// "DS NPP NMS", and a process reads DS with VAL 7, NO_ALARM.
#[epics_macros_rs::epics_test]
async fn a_device_link_put_to_a_registered_device_without_dsxt_is_refused() {
    use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, register_device_menu};
    static ASYN_AI: &[DevSup] = &[DevSup {
        name: "asynAiInt32",
        choice: "asynInt32",
        link_type: DbLinkType::InstIo,
        dset: Dset::new(6, &[2, 3, 4, 5], false),
    }];
    register_device_menu("ai", ASYN_AI);
    let db = IocBuilder::new()
        .db_string(
            r#"
record(ai, "DS") { field(VAL, "7") }
record(ai, "DG") { field(INP, "DS NPP") }
"#,
            &std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    put(&db, "DG", "DTYP", "asynInt32").await;
    let refused = db
        .put_record_field_from_ca(
            "DG",
            "INP",
            EpicsValue::String("@asyn(scalerPort 0 0)".into()),
        )
        .await;
    assert!(
        matches!(refused, Err(epics_base_rs::error::CaError::NoSupport)),
        "{refused:?}"
    );
    let inp = db.get_pv("DG.INP").unwrap();
    assert_eq!(inp, EpicsValue::String("DS NPP NMS".into()), "DG.INP");
    process(&db, "DG").await;
    assert_eq!(get(&db, "DG.VAL"), 7.0, "DG.VAL");
    assert_eq!(get(&db, "DG.SEVR"), 0.0, "DG.SEVR");
    assert_eq!(get(&db, "DG.PACT"), 0.0, "DG.PACT");
}
