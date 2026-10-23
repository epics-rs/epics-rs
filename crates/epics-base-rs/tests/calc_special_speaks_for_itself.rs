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
//!
//! Unix only: the stdout capture redirects fd 1 with `libc::dup2`.

#![cfg(unix)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::PutSplit;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::scalcout::ScalcoutRecord;
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

/// Everything written to fd 1 between `start` and `finish`: scalcout's
/// `sCalcPostfix returns: N` is a C `printf`, outside the errlog.
struct Stdout {
    sink: tempfile::NamedTempFile,
    saved: i32,
}

impl Stdout {
    fn start() -> Self {
        use std::os::fd::AsRawFd;
        let sink = tempfile::NamedTempFile::new().expect("stdout capture");
        // SAFETY: the test is `serial(stdout)`, so nothing else redirects fd 1.
        let saved = unsafe {
            let saved = libc::dup(1);
            libc::dup2(sink.as_file().as_raw_fd(), 1);
            saved
        };
        Self { sink, saved }
    }

    fn so_far(&self) -> String {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::fs::read_to_string(self.sink.path()).expect("stdout capture")
    }

    fn finish(self) -> String {
        let out = self.so_far();
        // SAFETY: restoring the descriptor `start` replaced.
        unsafe {
            libc::dup2(self.saved, 1);
            libc::close(self.saved);
        }
        out
    }
}

/// `(record, field, expr)` puts made together under one hold, each accepted;
/// nothing reaches the errlog, or `stdout` when given, until the hold is down.
fn put_under_hold(
    db: &PvDatabase,
    sink: &Arc<Mutex<Vec<String>>>,
    stdout: Option<&Stdout>,
    puts: &[(&str, &str, &str)],
) {
    let printed = stdout.map(Stdout::so_far);
    let before = heard(sink);
    let mut names: Vec<&str> = puts.iter().map(|p| p.0).collect();
    names.dedup();
    db.put_many_locked(&names, puts.len(), |held| {
        for &(pv, field, expr) in puts {
            let rec = held
                .record(names.iter().position(|n| *n == pv).unwrap())
                .unwrap();
            held.put_record_field_from_ca_no_notify(
                rec,
                field,
                EpicsValue::String(expr.into()),
                PutSplit::DbPutField,
            )
            .expect("the put is accepted");
        }
        assert_eq!(heard(sink), before, "nothing is printed under the hold");
        assert_eq!(stdout.map(Stdout::so_far), printed, "nor on stdout");
    });
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
///
/// A `.db` CALC that does not compile is refused by the LOAD first
/// (`dbPutString` runs `postfix()`, `dbStaticLib.c:2593`), which fails
/// `dbLoadRecords` but leaves the record and its stored text, so a startup
/// script goes on to `iocInit` and hears this line as well. softIoc
/// R7.0.10-146-g8f5015b66 prints both, in that order, for `CALC="A+"`.
#[epics_macros_rs::epics_test]
async fn a_bad_calc_from_the_db_file_reports_at_init_without_refusing_the_record() {
    use epics_base_rs::server::iocsh::IocShell;

    let sink = listen();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.db");
    std::fs::write(&path, "record(calc, \"P:F\") { field(CALC,\"(A\") }\n").unwrap();
    let db = Arc::new(PvDatabase::new());
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let line = format!("dbLoadRecords(\"{}\")", path.display());
    let shell_db = Arc::clone(&db);
    // The load itself fails, which is the point: the status is not asserted.
    let _ = std::thread::spawn(move || IocShell::new(shell_db, bridge).execute_line(&line))
        .join()
        .expect("the shell thread");
    db.ioc_init().await;

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

/// calcout `init_record` writes the same pair for CALC and, only under DOPT
/// "Use OCAL", for OCAL (`calcoutRecord.c:191-205`). softIoc 8f5015b66, this
/// db through a `dbLoadRecords` whose failure the script ignores: the load's
/// three `dbPutString` lines (`dbStaticLib.c:2599`), these four at iocInit,
/// then `dbgf` of CLCV/OCLV. OB0's bad OCAL is silent at init yet still
/// OCLV -1.
#[epics_macros_rs::epics_test]
async fn calcout_init_reports_a_bad_calc_and_a_used_bad_ocal() {
    let sink = listen();
    let db = IocBuilder::new()
        .boot_past_failed_loads()
        .db_string(
            r#"
record(calcout, "COB") { field(CALC, "A+") }
record(calcout, "OB0") { field(CALC, "A") field(OCAL, "B+") }
record(calcout, "OB1") { field(CALC, "A") field(OCAL, "B+") field(DOPT, "Use OCAL") }
record(calcout, "GOOD") { field(CALC, "A") field(OCAL, "B") field(DOPT, "Use OCAL") }
record(calcout, "DEF") {}
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    assert_eq!(
        heard(&sink),
        [
            "Incomplete expression, operand missing in CALC expression 'A+'\n",
            "Incomplete expression, operand missing in CALC expression 'B+'\n",
            "Incomplete expression, operand missing in CALC expression 'B+'\n",
            "recGblRecordError: calcout: init_record: Illegal CALC field Illegal field value PV: COB\n",
            "COB.CALC: Incomplete expression, operand missing in expression \"A+\"\n",
            "recGblRecordError: calcout: init_record: Illegal OCAL field Illegal field value PV: OB1\n",
            "OB1.OCAL: Incomplete expression, operand missing in expression \"B+\"\n",
        ]
        .join("\n")
    );
    let lv = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    let got: Vec<_> = ["COB", "OB0", "OB1", "GOOD", "DEF"]
        .iter()
        .map(|r| (lv(&format!("{r}.CLCV")), lv(&format!("{r}.OCLV"))))
        .collect();
    assert_eq!(
        got,
        [
            (-1.0, 0.0),
            (0.0, -1.0),
            (0.0, -1.0),
            (0.0, 0.0),
            (0.0, 0.0)
        ]
    );
}

/// scalcout `init_record` reports a bad CALC and a bad OCAL whatever DOPT says
/// (`sCalcoutRecord.c:290-302`), with no `calcErrorStr` line: its second line
/// is `sCalcPostfix returns: N` on stdout, outside the errlog. oracleSwaitIoc,
/// this db: these four errlog lines at iocInit, `sCalcPostfix returns: 8`
/// three times then `6` on stdout, then CLCV/OCLV by `dbgf`.
#[epics_macros_rs::epics_test]
#[serial_test::serial(stdout)]
async fn scalcout_init_reports_a_bad_calc_and_any_bad_ocal() {
    let sink = listen();
    let stdout = Stdout::start();
    let db = IocBuilder::new()
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .db_string(
            r#"
record(scalcout, "SB0") { field(CALC, "A+") }
record(scalcout, "SB1") { field(CALC, "A") field(OCAL, "B*") field(DOPT, "Use OCAL") }
record(scalcout, "SB2") { field(CALC, "A") field(OCAL, "B*") }
record(scalcout, "SG") { field(CALC, "A+1") field(OCAL, "B") }
record(scalcout, "SE") {}
record(scalcout, "SB3") { field(CALC, "(A") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    assert_eq!(
        stdout.finish(),
        "sCalcPostfix returns: 8\n".repeat(3) + "sCalcPostfix returns: 6\n"
    );
    assert_eq!(
        heard(&sink),
        [
            "recGblRecordError: scalcout: init_record: Illegal CALC field Illegal field value PV: SB0\n",
            "recGblRecordError: scalcout: init_record: Illegal OCAL field Illegal field value PV: SB1\n",
            "recGblRecordError: scalcout: init_record: Illegal OCAL field Illegal field value PV: SB2\n",
            "recGblRecordError: scalcout: init_record: Illegal CALC field Illegal field value PV: SB3\n",
        ]
        .join("\n")
    );
    let lv = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    let got: Vec<_> = ["SB0", "SB1", "SB2", "SG", "SE", "SB3"]
        .iter()
        .map(|r| (lv(&format!("{r}.CLCV")), lv(&format!("{r}.OCLV"))))
        .collect();
    assert_eq!(
        got,
        [
            (-1.0, 0.0),
            (0.0, -1.0),
            (0.0, -1.0),
            (0.0, 0.0),
            (0.0, 0.0),
            (-1.0, 0.0)
        ]
    );
}

/// acalcout `init_record` and `special()` report a bad CALC and a bad OCAL
/// whatever DOPT says (`aCalcoutRecord.c:245-260`, `:471-490`), the errlog
/// line alone: the `printf` after it needs `aCalcoutRecordDebug >= 10`.
/// oracleIoc (calc f207871), this db then `dbpf AC.CALC "A+"` and
/// `dbpf AC.OCAL "(B"`: these four stderr lines and nothing on stdout; CLCV/OCLV
/// by `dbgf`.
#[epics_macros_rs::epics_test]
async fn acalcout_reports_a_bad_calc_and_ocal_at_init_and_on_a_put() {
    use epics_base_rs::server::records::acalcout::AcalcoutRecord;
    let sink = listen();
    let db = IocBuilder::new()
        .register_record_type("acalcout", || Box::new(AcalcoutRecord::default()))
        .db_string(
            r#"
record(acalcout, "ACB") { field(CALC, "A+") }
record(acalcout, "AO0") { field(CALC, "A") field(OCAL, "B+") }
record(acalcout, "ADEF") {}
record(acalcout, "AC") { field(CALC, "A") field(OCAL, "B") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    put_under_hold(
        &db,
        &sink,
        None,
        &[("AC", "CALC", "A+"), ("AC", "OCAL", "(B")],
    );

    assert_eq!(
        heard(&sink),
        [
            "recGblRecordError: acalcout: init_record: Illegal CALC field Illegal field value PV: ACB\n",
            "recGblRecordError: acalcout: init_record: Illegal OCAL field Illegal field value PV: AO0\n",
            "recGblRecordError: acalcout: special(): Illegal CALC field Illegal field value PV: AC\n",
            "recGblRecordError: acalcout: special(): Illegal OCAL field Illegal field value PV: AC\n",
        ]
        .join("\n")
    );
    let lv = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    let got: Vec<_> = ["ACB", "AO0", "ADEF"]
        .iter()
        .map(|r| (lv(&format!("{r}.CLCV")), lv(&format!("{r}.OCLV"))))
        .collect();
    assert_eq!(got, [(-1.0, 0.0), (0.0, -1.0), (0.0, 0.0)]);
}

/// scalcout `special()` reports a refused CALC/OCAL put like its
/// `init_record` does, whatever DOPT says (`sCalcoutRecord.c:465-480`).
/// oracleIoc (calc f207871), `dbpf SC.CALC "A+"` then `dbpf SC.OCAL "@@"`:
/// these two stderr lines, each followed by `sCalcPostfix returns: 8` on stdout.
#[epics_macros_rs::epics_test]
#[serial_test::serial(stdout)]
async fn scalcout_special_reports_a_refused_calc_and_ocal_put() {
    let sink = listen();
    let db = IocBuilder::new()
        .register_record_type("scalcout", || Box::new(ScalcoutRecord::default()))
        .db_string(
            "record(scalcout, \"SC\") { field(CALC, \"A\") field(OCAL, \"B\") }\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let stdout = Stdout::start();
    put_under_hold(
        &db,
        &sink,
        Some(&stdout),
        &[("SC", "CALC", "A+"), ("SC", "OCAL", "@@")],
    );

    assert_eq!(stdout.finish(), "sCalcPostfix returns: 8\n".repeat(2));
    assert_eq!(
        heard(&sink),
        [
            "recGblRecordError: scalcout: special(): Illegal CALC field Illegal field value PV: SC\n",
            "recGblRecordError: scalcout: special(): Illegal OCAL field Illegal field value PV: SC\n",
        ]
        .join("\n")
    );
}

/// transform reports each refused non-empty CLCx, from `init_record`
/// (`transformRecord.c:481-487`) and from `special()` (`:684-689`); an empty
/// CLCx is valid and silent. oracleIoc (calc f207871), this db then
/// `dbpf TR.CLCA "Q+"`, `dbpf TR.CLCB "B+"`, `dbpf TR.CLCC ""`: these four
/// stderr lines and nothing on stdout.
#[epics_macros_rs::epics_test]
async fn transform_reports_each_refused_clc_at_init_and_on_a_put() {
    use epics_base_rs::server::records::transform::TransformRecord;
    let sink = listen();
    let db = IocBuilder::new()
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .db_string(
            r#"
record(transform, "TRB") { field(CLCA, "B+") field(CLCB, "") field(CLCC, "C+") field(CLCD, "A") }
record(transform, "TDEF") {}
record(transform, "TR") { field(CLCA, "A") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    put_under_hold(
        &db,
        &sink,
        None,
        &[
            ("TR", "CLCA", "Q+"),
            ("TR", "CLCB", "B+"),
            ("TR", "CLCC", ""),
        ],
    );

    assert_eq!(
        heard(&sink),
        [
            "recGblRecordError: transform: init_record: Illegal CALC field Illegal field value PV: TRB\n",
            "recGblRecordError: transform: init_record: Illegal CALC field Illegal field value PV: TRB\n",
            "recGblRecordError: transform:special: Illegal CALC field Illegal field value PV: TR\n",
            "recGblRecordError: transform:special: Illegal CALC field Illegal field value PV: TR\n",
        ]
        .join("\n")
    );
}

/// calcout `special()` writes the init-time pair on a refused put, for OCAL
/// only under DOPT "Use OCAL" (`calcoutRecord.c:325-343`). oracleIoc, this db
/// then `dbpf CO0.CALC "A+"`, `dbpf CO0.OCAL "B+"`, `dbpf CO1.OCAL "(B"`:
/// exactly these four stderr lines.
#[epics_macros_rs::epics_test]
async fn calcout_special_reports_a_refused_calc_and_a_used_ocal_put() {
    let sink = listen();
    let db = ioc(r#"
record(calcout, "CO0") { field(CALC, "A") field(OCAL, "B") }
record(calcout, "CO1") { field(CALC, "A") field(OCAL, "B") field(DOPT, "Use OCAL") }
"#)
    .await;
    put_under_hold(
        &db,
        &sink,
        None,
        &[
            ("CO0", "CALC", "A+"),
            ("CO0", "OCAL", "B+"),
            ("CO1", "OCAL", "(B"),
        ],
    );

    assert_eq!(
        heard(&sink),
        [
            "recGblRecordError: calcout: special(): Illegal CALC field Illegal field value PV: CO0\n",
            "CO0.CALC: Incomplete expression, operand missing in expression \"A+\"\n",
            "recGblRecordError: calcout: special(): Illegal OCAL field Illegal field value PV: CO1\n",
            "CO1.OCAL: Parenthesis still open at end of expression in expression \"(B\"\n",
        ]
        .join("\n")
    );
}

/// CAV..CPV hold each CLCx's `sCalcPostfix()` status, 0 for an empty one,
/// set by `init_record` (`transformRecord.c:476-482`) and by `special()`
/// (`:676-692`); a client may write them (no `special` in the `.dbd`).
/// oracleIoc (calc f207871), `dbgf` after each step of this db and puts.
#[epics_macros_rs::epics_test]
async fn transform_cav_holds_each_clc_compile_status() {
    use epics_base_rs::server::records::transform::TransformRecord;
    let db = IocBuilder::new()
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .db_string(
            "record(transform, \"TR\") { field(CLCA, \"A\") field(CLCB, \"B+\") }\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let lv = |f: &str| db.get_pv(format!("TR.{f}")).unwrap().to_f64().unwrap();
    assert_eq!([lv("CAV"), lv("CBV"), lv("CCV")], [0.0, -1.0, 0.0]);

    let mut got = Vec::new();
    for (field, value) in [
        ("CLCA", EpicsValue::String("Q+".into())),
        ("CLCA", EpicsValue::String("A+1".into())),
        ("CLCB", EpicsValue::String("".into())),
        ("CAV", EpicsValue::Long(5)),
    ] {
        db.put_record_field_from_ca_no_notify("TR", field, value)
            .await
            .unwrap();
        got.push([lv("CAV"), lv("CBV")]);
    }
    assert_eq!(got, [[-1.0, -1.0], [0.0, -1.0], [0.0, 0.0], [5.0, 0.0]]);
}

/// A CLCx put is cut to `size(120)` - 1 bytes by `dbPut` before `special()`
/// compiles it, which is why `pclcbuf[INFIX_SIZE - 1] = 0`
/// (`transformRecord.c:679`) never changes anything. oracleIoc (calc f207871),
/// `caput -a TP.CLCA$ 120 <"A+"*59 "1+">`: CLCA reads back the first 119
/// bytes, CAV 0, nothing on stderr.
#[epics_macros_rs::epics_test]
async fn transform_over_long_clc_put_compiles_its_first_119_bytes() {
    use epics_base_rs::server::records::transform::TransformRecord;
    let sink = listen();
    let db = IocBuilder::new()
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .db_string("record(transform, \"TP\") {}\n", &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let long = format!("{}1+", "A+".repeat(59));
    assert_eq!(long.len(), 120);
    db.put_record_field_from_ca_no_notify("TP", "CLCA", EpicsValue::String(long.clone().into()))
        .await
        .unwrap();

    assert_eq!(calc_of(&db, "TP.CLCA"), long[..119]);
    assert_eq!(db.get_pv("TP.CAV").unwrap().to_f64(), Some(0.0));
    assert_eq!(heard(&sink), "");
}

/// swait `special()` reports a refused CALC put and returns 0
/// (`swaitRecord.c:561-568`). oracleSwaitIoc (calc f207871), this db then
/// `dbpf SW.CALC "A+"`: this one stderr line, nothing on stdout, CLCV -1.
#[epics_macros_rs::epics_test]
#[serial_test::serial(stdout)]
async fn swait_special_reports_a_refused_calc_put() {
    use epics_base_rs::server::records::swait::SwaitRecord;
    let sink = listen();
    let db = IocBuilder::new()
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(
            "record(swait, \"SW\") { field(CALC, \"A\") }\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let stdout = Stdout::start();
    put_under_hold(&db, &sink, Some(&stdout), &[("SW", "CALC", "A+")]);

    assert_eq!(stdout.finish(), "");
    assert_eq!(
        heard(&sink),
        "recGblRecordError: swaitRecord:special: Illegal CALC field Illegal field value PV: SW\n"
    );
    assert_eq!(db.get_pv("SW.CLCV").unwrap().to_f64(), Some(-1.0));
}
