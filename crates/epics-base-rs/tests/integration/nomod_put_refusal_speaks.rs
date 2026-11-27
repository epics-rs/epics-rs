//! A `dbPut` refused for `SPC_NOMOD` REPORTS — `dbpf` prints nothing itself,
//! so this line is the whole diagnostic.
//!
//! ```c
//! /* dbAccess.c:122-127, reached from dbPut via dbPutSpecial(paddr, 0) */
//! if ((special == SPC_NOMOD) && (pass == 0)) {
//!     status = S_db_noMod;
//!     recGblDbaddrError(status, paddr, "dbPut");
//!     return status;
//! }
//! ```
//!
//! softIoc @`R7.0.10`, with `record(sub,"T:SUB"){field(INAM,"initSub")}` and
//! `record(sel,"T:SEL"){}` — stdout carries the unchanged read-back, stderr
//! carries the refusal:
//!
//! ```text
//! epics> dbpf T:SUB.INAM zzz
//! DBF_STRING:         "initSub"
//! epics> dbpf T:SEL.VAL 3
//! DBF_DOUBLE:         0
//! ...
//! recGblDbaddrError: dbPut Attempt to modify noMod field PV: T:SUB.INAM
//! recGblDbaddrError: dbPut Attempt to modify noMod field PV: T:SEL.VAL
//! ```
//!
//! The port returned `S_db_noMod` and said nothing at all, so a refused put was
//! indistinguishable from an accepted one on the console.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const DB: &str = r#"
record(sub, "T:SUB") { field(INAM, "initSub") }
record(sel, "T:SEL") { }
"#;

async fn build() -> Arc<epics_base_rs::server::database::PvDatabase> {
    IocBuilder::new()
        .register_subroutine("initSub", |_: &mut dyn Record| Ok(0))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

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
    sink.lock().expect("sink").join("")
}

fn forget(sink: &Arc<Mutex<Vec<String>>>) {
    epics_base_rs::runtime::log::errlog_flush();
    sink.lock().expect("sink").clear();
}

/// The CA / `dbpf` route: `dbPutField` → `dbPut` → `dbPutSpecial(paddr, 0)`.
/// The line names the record AND the field, and the value does not change.
#[epics_macros_rs::epics_test]
async fn a_refused_ca_put_writes_the_line_dbpf_relies_on() {
    let sink = listen();
    let db = build().await;
    forget(&sink);

    let err = db
        .put_record_field_from_ca("T:SUB", "INAM", EpicsValue::String("zzz".into()))
        .await
        .expect_err("INAM is special(SPC_NOMOD)");
    assert!(
        matches!(err, epics_base_rs::error::CaError::ReadOnlyField(ref f) if f == "INAM"),
        "expected S_db_noMod, got {err:?}"
    );
    assert_eq!(
        heard(&sink),
        "recGblDbaddrError: dbPut Attempt to modify noMod field PV: T:SUB.INAM\n",
        "dbAccess.c:125 through recGbl.c:87-90, verbatim and once"
    );
    assert_eq!(
        db.get_pv("T:SUB.INAM").unwrap(),
        EpicsValue::String("initSub".into())
    );
}

/// The internal route: `put_pv` is the port's `dbPut` analogue and C's
/// `dbPutLink` reaches the same `dbPutSpecial`, so it speaks too. `sel.VAL` is
/// the `SPC_NOMOD` the oracle's monitor phase already excludes.
#[epics_macros_rs::epics_test]
async fn a_refused_internal_put_speaks_from_the_same_gate() {
    let sink = listen();
    let db = build().await;
    forget(&sink);

    db.put_pv("T:SEL.VAL", EpicsValue::Double(3.0))
        .await
        .expect_err("sel.VAL is special(SPC_NOMOD)");
    assert_eq!(
        heard(&sink),
        "recGblDbaddrError: dbPut Attempt to modify noMod field PV: T:SEL.VAL\n"
    );
}

/// The boundary the gate must NOT cross: pvxs `doPreProcessing`
/// (`iocsource.cpp:363-375`) refuses ABOVE `dbPut`, so the QSRV precondition
/// check reports the same status with no console line — otherwise a QSRV put
/// would print C's `dbPut` line twice, once before `dbPut` and once inside it.
#[epics_macros_rs::epics_test]
async fn the_pre_put_precondition_check_stays_silent() {
    let sink = listen();
    let db = build().await;
    forget(&sink);

    db.check_external_put_preconditions("T:SUB", "INAM")
        .await
        .expect_err("the precondition check still refuses");
    assert_eq!(heard(&sink), "", "doPreProcessing writes no errlog line");
}

/// An accepted put says nothing — the line belongs to the refusal, not to the
/// route.
#[epics_macros_rs::epics_test]
async fn an_accepted_put_writes_no_line() {
    let sink = listen();
    let db = build().await;
    forget(&sink);

    db.put_record_field_from_ca("T:SUB", "DESC", EpicsValue::String("ok".into()))
        .await
        .unwrap();
    assert_eq!(heard(&sink), "");
}
