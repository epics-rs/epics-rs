//! A runtime put to swait's `OUTN` re-points the output.
//!
//! `OUTN` is a `DBF_STRING` with `special(SPC_MOD)` (`swaitRecord.dbd:158-165`):
//! `dbPut` stores the name, then `special(after)` (`swaitRecord.c:507-559`)
//! sets `OUTV` and re-issues `recDynLinkAddOutput`, so the next output goes
//! to the new name.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::EpicsValue;

const PV_OK: u16 = 0;
const NO_PV: u16 = 2;

const DB: &str = r#"
record(ai, "T1") {}
record(ai, "T2") {}
record(swait, "W") {
    field(CALC, "A")
    field(A, "5")
    field(OUTN, "T1")
}
"#;

async fn loaded() -> Arc<PvDatabase> {
    // swait is a synApps record, registered as an application would.
    IocBuilder::new()
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &PvDatabase) {
    db.process_record_with_links("W", &mut ProcStack::new())
        .await
        .unwrap();
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

async fn settle_outv(db: &PvDatabase, want: u16) {
    for _ in 0..400 {
        if db.get_pv("W.OUTV").ok() == Some(EpicsValue::Enum(want)) {
            return;
        }
        epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("W.OUTV never reached {want}: {:?}", db.get_pv("W.OUTV"));
}

#[epics_macros_rs::epics_test]
async fn a_runtime_outn_put_stores_the_name_and_moves_the_output() {
    let db = loaded().await;
    process(&db).await;
    assert_eq!(num(&db, "T1.VAL"), 5.0, "the loaded OUTN");

    db.put_pv_no_process("W.OUTN", EpicsValue::String("T2".into()))
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("W.OUTN").unwrap(),
        EpicsValue::String("T2".into())
    );
    settle_outv(&db, PV_OK).await;
    db.put_pv_no_process("W.A", EpicsValue::Double(7.0))
        .await
        .unwrap();
    process(&db).await;
    assert_eq!(num(&db, "T2.VAL"), 7.0, "the output follows OUTN");
    assert_eq!(num(&db, "T1.VAL"), 5.0, "the old target is left");

    // A blank name is no output at all: `special` sets NO_PV and clears the
    // dynamic link.
    db.put_pv_no_process("W.OUTN", EpicsValue::String("".into()))
        .await
        .unwrap();
    settle_outv(&db, NO_PV).await;
    db.put_pv_no_process("W.A", EpicsValue::Double(9.0))
        .await
        .unwrap();
    process(&db).await;
    assert_eq!(num(&db, "T2.VAL"), 7.0, "a blank OUTN writes nothing");
}

/// `dbPut` stores a string field as `dbPutField` does; a `DBR_DOUBLE` is
/// converted to the name first (`dbConvert.c` `putDoubleString`).
#[epics_macros_rs::epics_test]
async fn a_db_put_and_a_numeric_put_store_outn_too() {
    let db = loaded().await;
    db.put_pv_and_post("W.OUTN", EpicsValue::String("T2".into()))
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("W.OUTN").unwrap(),
        EpicsValue::String("T2".into()),
        "dbPut"
    );
    settle_outv(&db, PV_OK).await;
    process(&db).await;
    assert_eq!(num(&db, "T2.VAL"), 5.0, "dbPut moves the output");

    db.put_pv_no_process("W.OUTN", EpicsValue::Double(3.0))
        .await
        .unwrap();
    assert_eq!(
        db.get_pv("W.OUTN").unwrap(),
        EpicsValue::String("3".into()),
        "DBR_DOUBLE"
    );
}
