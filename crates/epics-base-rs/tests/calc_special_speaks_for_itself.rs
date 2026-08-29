//! A refused `dbpf` is silent, and the operator still gets told why.
//!
//! `dbpf` hands `dbPutField`'s status to `iocshSetError`
//! (`dbIocRegister.c:272-273`), which sets a flag and prints nothing, so every
//! word on a refused put comes from inside the put. For a bad CALC that is the
//! record itself, in two `errlogPrintf` records (`calcRecord.c:145-151`):
//!
//! ```c
//! if (postfix(prec->calc, prec->rpcl, &error_number)) {
//!     recGblRecordError(S_db_badField, (void *)prec,
//!                       "calc: Illegal CALC field");
//!     errlogPrintf("%s.CALC: %s in expression \"%s\"\n",
//!                  prec->name, calcErrorStr(error_number), prec->calc);
//!     return S_db_badField;
//! }
//! ```
//!
//! `init_record` (`:105-110`) writes the same pair with `"calc: init_record:
//! Illegal CALC field"` and does NOT refuse the record.
//!
//! Both carry `prec->name`, which is why C prints them from the record and not
//! from `postfix()` — a shared compile helper has no name to print. The port
//! had routed the second line to `tracing` from its shared `calc_compile`,
//! naming the record TYPE, and no IOC binary installs a `tracing` subscriber;
//! the first line it did not write at all.
//!
//! Measured against `softIoc` R7.0.10 on the same command file: all eight
//! stderr lines byte-identical, in order, for `A+`, `@@@`, `(A` and `""`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// Everything the errlog carried, from before the IOC was built. Installed
/// first because `init_record`'s half of the report is written during
/// `build()` and there is no second chance to hear it.
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

fn calc_of(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).expect("the field reads back") {
        EpicsValue::String(s) => s.to_string(),
        other => panic!("expected a DBF_STRING CALC, got {other:?}"),
    }
}

/// The put path. Both records, the real PV name in both, and the put refused.
#[epics_macros_rs::epics_test]
async fn a_refused_calc_put_writes_both_of_cs_errlog_records() {
    let sink = listen();
    let db = ioc("record(calc, \"P:C\") { field(CALC,\"A+B\") }\n").await;

    let refused = db
        .put_record_field_from_ca_no_notify("P:C", "CALC", EpicsValue::String("A+".into()))
        .await;
    assert!(
        refused.is_err(),
        "C `special()` returns S_db_badField, so the client's write FAILS"
    );

    let log = heard(&sink);
    assert!(
        log.contains("recGblRecordError: calc: Illegal CALC field Illegal field value PV: P:C"),
        "C `recGblRecordError(S_db_badField, ...)`, got {log:?}"
    );
    assert!(
        log.contains("P:C.CALC: Incomplete expression, operand missing in expression \"A+\""),
        "C `calcErrorStr(CALC_ERR_INCOMPLETE)` under the record's own NAME, got {log:?}"
    );
}

/// The line names `prec->name`, not the record type — the whole reason this
/// report cannot live in the shared compile helper. Without a second record
/// the assertion above passes on a hard-coded "P:C" too.
#[epics_macros_rs::epics_test]
async fn each_record_is_named_by_its_own_pv_name() {
    let sink = listen();
    let db = ioc("record(calc, \"A:ONE\") { field(CALC,\"A+B\") }\n\
                  record(calc, \"B:TWO\") { field(CALC,\"A+B\") }\n")
    .await;

    for pv in ["A:ONE", "B:TWO"] {
        db.put_record_field_from_ca_no_notify(pv, "CALC", EpicsValue::String("@@@".into()))
            .await
            .expect_err("an uncompilable CALC is refused");
    }

    let log = heard(&sink);
    for pv in ["A:ONE", "B:TWO"] {
        assert!(
            log.contains(&format!("PV: {pv}")),
            "each refusal names its own record, got {log:?}"
        );
        assert!(
            log.contains(&format!(
                "{pv}.CALC: Syntax error, unknown operator/operand in expression \"@@@\""
            )),
            "the expression line carries the PV name too, got {log:?}"
        );
    }
    assert!(
        !log.contains("calc.CALC:"),
        "the record TYPE must not appear where C prints the name, got {log:?}"
    );
}

/// The other side of the boundary. Without this the report could be
/// unconditional and every test above would still pass.
#[epics_macros_rs::epics_test]
async fn a_calc_that_compiles_says_nothing() {
    let sink = listen();
    let db = ioc("record(calc, \"P:OK\") { field(CALC,\"A+B\") }\n").await;

    db.put_record_field_from_ca_no_notify("P:OK", "CALC", EpicsValue::String("A*B".into()))
        .await
        .expect("a CALC that compiles is accepted");

    let log = heard(&sink);
    assert!(
        !log.contains("Illegal CALC field"),
        "a good CALC is silent in C, got {log:?}"
    );
}

/// The `init_record` half: a different `pmessage`, and the record is NOT
/// refused (`calcRecord.c:105-110` falls through to `return 0`).
#[epics_macros_rs::epics_test]
async fn a_bad_calc_from_the_db_file_reports_at_init_without_refusing_the_record() {
    let sink = listen();
    let db = ioc("record(calc, \"P:F\") { field(CALC,\"(A\") }\n").await;

    let log = heard(&sink);
    assert!(
        log.contains(
            "recGblRecordError: calc: init_record: Illegal CALC field Illegal field value PV: P:F"
        ),
        "C's init_record passes a DIFFERENT pmessage than special(), got {log:?}"
    );
    assert!(
        log.contains("P:F.CALC: Parenthesis still open at end of expression in expression \"(A\""),
        "got {log:?}"
    );
    assert!(
        db.get_record("P:F").is_some(),
        "C `init_record` logs and returns 0 — the record still exists"
    );
}

/// The refusal does not roll the field back: C's `special()` runs AFTER
/// `dbPut` stored the string, so `dbgf` reads the expression that was
/// rejected. This is what makes `dbpf`'s closing read-back show `"A+"`.
#[epics_macros_rs::epics_test]
async fn the_refused_expression_stays_stored() {
    let db = ioc("record(calc, \"P:C\") { field(CALC,\"A+B\") }\n").await;

    db.put_record_field_from_ca_no_notify("P:C", "CALC", EpicsValue::String("A+".into()))
        .await
        .expect_err("refused");

    assert_eq!(calc_of(&db, "P:C.CALC"), "A+");
}

/// The empty expression is `CALC_ERR_NULL_ARG` in base `postfix()`, so C
/// reports it like any other failure rather than treating "no expression" as
/// nothing to do.
#[epics_macros_rs::epics_test]
async fn an_emptied_calc_is_reported_like_any_other_failure() {
    let sink = listen();
    let db = ioc("record(calc, \"P:C\") { field(CALC,\"A+B\") }\n").await;

    db.put_record_field_from_ca_no_notify("P:C", "CALC", EpicsValue::String("".into()))
        .await
        .expect_err("C refuses an empty CALC put with S_db_badField");

    let log = heard(&sink);
    assert!(
        log.contains("P:C.CALC: NULL or empty input argument to postfix() in expression \"\""),
        "got {log:?}"
    );
}
