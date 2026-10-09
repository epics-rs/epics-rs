//! What `devAiSoftCallback.c::add_record` (`:76-127`) leaves an `ai` reading.
//! softIoc R7.0.10.1, this `.db`, `dbpf <rec>.PROC 1` on each, then `dbgf`:
//!
//! ```text
//! recGblRecordError: devAiSoftCallback (add_record) link target not found Process Variable Not Found PV: A2
//! recGblRecordError: devAiSoftCallback (add_record) link target not found Process Variable Not Found PV: A3
//! recGblRecordError: devAiSoftCallback (add_record) Illegal INP field Illegal field value PV: A4
//! A1 VAL 5 NO_ALARM UDF 0    A2 VAL 0 NO_ALARM UDF 0    A3 VAL 0 NO_ALARM UDF 0
//! A4 VAL 0 NO_ALARM UDF 0    A5 VAL 5                   A6 VAL 5
//! ```
//!
//! A refused record keeps a NULL `dpvt`, so `read_ai` returns 2 unread and
//! `aiRecord.c:161` derives UDF from the untouched VAL. A PV name is opened as
//! a local `dbChannel` whatever its CA flag.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ai, "SRC") { field(VAL, "5") }
record(ai, "A1") { field(DTYP, "Async Soft Channel") field(INP, "SRC CA") }
record(ai, "A2") { field(DTYP, "Async Soft Channel") field(INP, "NOWHERE CA") }
record(ai, "A3") { field(DTYP, "Async Soft Channel") field(INP, "NOWHERE") }
record(ai, "A4") { field(DTYP, "Async Soft Channel") field(INP, {calc:{expr:"7"}}) }
record(ai, "A5") { field(DTYP, "Async Soft Channel") field(INP, "SRC") }
record(ai, "A6") { field(DTYP, "Async Soft Channel") field(INP, "SRC CP") }
"#;

async fn processed(names: &[&str]) -> (Arc<PvDatabase>, String) {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    epics_base_rs::runtime::log::errlog_add_listener(move |m| {
        sink.lock().expect("sink").push(m.to_string());
    });
    let (db, _h) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    for name in names {
        db.put_record_field_from_ca(name, "PROC", EpicsValue::Char(1))
            .await
            .unwrap();
    }
    epics_base_rs::runtime::log::errlog_flush();
    let log = heard.lock().expect("sink").join("\n");
    (db, log)
}

/// `(VAL, SEVR, STAT, UDF)`.
fn state(db: &PvDatabase, name: &str) -> (f64, f64, f64, f64) {
    let get = |f: &str| {
        db.get_pv(&format!("{name}.{f}"))
            .ok()
            .and_then(|v| v.to_f64())
            .unwrap_or_else(|| panic!("{name}.{f}"))
    };
    (get("VAL"), get("SEVR"), get("STAT"), get("UDF"))
}

fn refusal(dset_error: &str, name: &str) -> String {
    format!("recGblRecordError: devAiSoftCallback (add_record) {dset_error} PV: {name}")
}

#[epics_macros_rs::epics_test]
async fn a_refused_inp_is_never_read_devaisoftcallback_c_171() {
    let (db, log) = processed(&["A4", "A5", "A6"]).await;
    assert!(
        log.contains(&refusal("Illegal INP field Illegal field value", "A4")),
        "{log:?}"
    );
    for name in ["A5", "A6"] {
        assert!(!log.contains(&format!("PV: {name}")), "{log:?}");
    }
    assert_eq!(state(&db, "A4"), (0.0, 0.0, 0.0, 0.0));
    assert_eq!(state(&db, "A5"), (5.0, 0.0, 0.0, 0.0));
    assert_eq!(state(&db, "A6"), (5.0, 0.0, 0.0, 0.0));
}

#[epics_macros_rs::epics_test]
#[ignore = "needs the add_record local-record lookup armed by the init caller"]
async fn a_pv_name_opens_a_local_channel_devaisoftcallback_c_95() {
    let (db, log) = processed(&["A1", "A2", "A3"]).await;
    for name in ["A2", "A3"] {
        assert!(
            log.contains(&refusal(
                "link target not found Process Variable Not Found",
                name
            )),
            "{log:?}"
        );
    }
    assert!(!log.contains("PV: A1"), "{log:?}");
    assert_eq!(state(&db, "A1"), (5.0, 0.0, 0.0, 0.0));
    assert_eq!(state(&db, "A2"), (0.0, 0.0, 0.0, 0.0));
    assert_eq!(state(&db, "A3"), (0.0, 0.0, 0.0, 0.0));
}
