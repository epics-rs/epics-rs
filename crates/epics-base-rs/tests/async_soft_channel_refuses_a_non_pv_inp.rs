//! `"Async Soft Channel"` on an input record demands a PV link in INP.
//!
//! `devAiSoftCallback.c:76-93` and its six input twins report
//! `S_db_badField` from `add_record` when `plink->type != PV_LINK`. Measured
//! on `softIoc` R7.0.10 over `asyncSoftTest.db`: seven lines, one per input
//! record whose INP is `{const:N}`, and none for the `ai0`-style records whose
//! INP names a PV or for any of the output records — the ten output
//! `dev*SoftCallback.c` files declare no `dsxt` at all.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;

fn listen() -> Arc<Mutex<Vec<String>>> {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    epics_base_rs::runtime::log::errlog_add_listener(move |m| {
        sink.lock().expect("sink").push(m.to_string());
    });
    heard
}

fn heard(sink: &Arc<Mutex<Vec<String>>>) -> String {
    epics_base_rs::runtime::log::errlog_flush();
    sink.lock().expect("sink").join("\n")
}

async fn ioc(db_text: &str) -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// The three boundaries of C's one live test, on one input record type:
/// a JSON constant refuses, a PV name passes, an unset INP refuses.
#[epics_macros_rs::epics_test]
async fn only_a_pv_link_passes_add_record() {
    let sink = listen();
    let db = ioc(
        "record(ai, \"T:CONST\") { field(DTYP,\"Async Soft Channel\") field(INP,{const:9}) }\n\
         record(ai, \"T:PV\")    { field(DTYP,\"Async Soft Channel\") field(INP,\"T:CONST\") }\n\
         record(ai, \"T:EMPTY\") { field(DTYP,\"Async Soft Channel\") }\n",
    )
    .await;

    let log = heard(&sink);
    for name in ["T:CONST", "T:EMPTY"] {
        assert!(
            log.contains(&format!(
                "recGblRecordError: devAiSoftCallback (add_record) Illegal INP field \
                 Illegal field value PV: {name}"
            )),
            "{name} has no PV link, got {log:?}"
        );
    }
    assert!(
        !log.contains("PV: T:PV"),
        "a PV_LINK INP is what add_record is looking for, got {log:?}"
    );
    assert!(
        db.get_pv("T:CONST.VAL").is_ok(),
        "`doResolveLinks` discards the status, so the record is still built"
    );
}

/// An OUTPUT `"Async Soft Channel"` record has no `add_record` in C, and a
/// non-Async soft channel has no callback dset at all. Neither may be
/// reported.
#[epics_macros_rs::epics_test]
async fn nothing_else_is_checked() {
    let sink = listen();
    let _db = ioc(
        "record(ao, \"T:AO\") { field(DTYP,\"Async Soft Channel\") field(OUT,{const:1}) }\n\
         record(ai, \"T:PLAIN\") { field(DTYP,\"Soft Channel\") field(INP,{const:9}) }\n",
    )
    .await;

    let log = heard(&sink);
    assert!(
        !log.contains("add_record"),
        "devAoSoftCallback.c declares no dsxt and devAiSoft.c has no add_record, got {log:?}"
    );
}
