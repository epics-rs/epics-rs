//! A put made under a gate its caller holds settles once that gate drops: a
//! refused choice prints C's `recGblRecordError` line then, never under the
//! hold, and a link put connects its CP edge then. The line is softIoc's for
//! `dbpf SO zz` with `SO.OUT` = `MS.VAL`, a `dbPut` into `MS` (R7.0.10-146).

mod calink_ioc;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::runtime::log::{errlog_add_listener, errlog_flush};
use epics_base_rs::server::database::PutSplit;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};

const DB: &str = r#"
record(mbbi, "MS") { field(ZRST, "a") field(ONST, "b") }
record(ao, "X") { field(VAL, "0") }
record(calc, "H") { field(CALC, "VAL+1") }
"#;

const MS_ZZ: &str = "recGblRecordError: zz Illegal choice PV: MS";

async fn connected_ioc() -> Arc<PvDatabase> {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.initialize_link_locality().await;
    calink_ioc::connect(&db).await;
    db
}

fn listen() -> impl Fn() -> Vec<String> {
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    errlog_add_listener(move |m| sink.lock().unwrap().push(m.to_string()));
    move || {
        errlog_flush();
        let mut heard = heard.lock().unwrap();
        let said = heard
            .iter()
            .filter(|m| m.contains("Illegal choice PV: MS"))
            .map(|m| m.trim_end().to_string())
            .collect();
        heard.clear();
        said
    }
}

fn zz() -> EpicsValue {
    EpicsValue::String(PvString::from("zz"))
}

fn val(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn a_held_refusal_is_reported_once_its_gate_drops() {
    let db = connected_ioc().await;
    let said = listen();
    said();

    let refused = db.put_many_locked(&["MS"], 2, |puts| {
        let ms = puts.record(0).unwrap();
        assert!(puts.record(1).is_none(), "no handle outside the locked set");
        let refused = (
            puts.put_record_field_from_ca_no_notify(ms, "VAL", zz(), PutSplit::DbPutField)
                .is_err(),
            puts.put_pv(ms, "VAL", zz()).is_err(),
        );
        assert_eq!(
            said(),
            Vec::<String>::new(),
            "nothing is printed under the hold"
        );
        refused
    });
    assert_eq!(refused, (true, true));
    assert_eq!(
        said(),
        [MS_ZZ, MS_ZZ],
        "one line per put, once the gate drops"
    );

    let held = db.lock_for_put("MS", "VAL", &zz());
    assert!(
        db.put_record_field_from_ca_no_notify_already_locked(held, zz())
            .is_err()
    );
    assert_eq!(said(), [MS_ZZ]);

    let held = db.lock_for_put("MS", "VAL", &zz());
    assert!(db.put_pv_already_locked(held, zz()).is_err());
    assert_eq!(said(), [MS_ZZ]);

    assert!(db.put_pv("MS.VAL", zz()).await.is_err());
    assert_eq!(said(), [MS_ZZ]);
    assert_eq!(val(&db, "MS"), 0.0);
}

/// `dbpf H.INPA "X CP"` then `dbpf X 1` on softIoc: H 1, then 2.
#[epics_macros_rs::epics_test]
async fn a_held_link_put_connects_its_cp_edge_once_its_gate_drops() {
    let db = connected_ioc().await;
    let text = EpicsValue::String(PvString::from("X CP"));

    let held = db.lock_for_put("H", "INPA", &text);
    db.put_record_field_from_ca_already_locked(held, text.clone())
        .unwrap();
    calink_ioc::until("the new CP link processes H on connect", || {
        val(&db, "H") == 1.0
    })
    .await;

    db.put_record_field_from_ca("X", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    calink_ioc::until("X drives H through its CP edge", || val(&db, "H") == 2.0).await;
}

/// A panic in `put_many_locked`'s body still settles the puts it made: the
/// `H.INPA` link put connects its CP edge once the gate unwinds.
#[epics_macros_rs::epics_test]
async fn a_held_link_put_settles_when_the_body_panics() {
    let db = connected_ioc().await;
    let text = EpicsValue::String(PvString::from("X CP"));

    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        db.put_many_locked(&["H", "X"], 1, |puts| {
            let h = puts.record(0).unwrap();
            puts.put_record_field_from_ca_no_notify(h, "INPA", text, PutSplit::DbPutField)
                .unwrap();
            panic!("the body fails after its put");
        })
    }));
    assert!(unwound.is_err());
    calink_ioc::until("the new CP link processes H on connect", || {
        val(&db, "H") == 1.0
    })
    .await;

    db.put_record_field_from_ca("X", "VAL", EpicsValue::Double(1.0))
        .await
        .unwrap();
    calink_ioc::until("X drives H through its CP edge", || val(&db, "H") == 2.0).await;
}
