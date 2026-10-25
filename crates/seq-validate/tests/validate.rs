//! Each validate program run as its `<stem>.t` runs it under the Perl
//! harness: `seqMain -S -t [-d <stem>.db]`, so `iocInit` when there is a
//! database, 0.2 s, then `seq`; and, in [`ioc`], as `<stem>Ioc.t` runs it,
//! against its database in an IOC of its own. The program's TAP must be
//! C's, kept in `c/<stem>.tap` from a run of the C build; a program's `.t`
//! and `Ioc.t` report the same lines.
//!
//! The run-time is process-wide (one `PvSystem`, one IOC), so each case
//! needs a process of its own, which nextest gives it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;
use epics_seq_rs::{ProgramVars, SeqProgram, seq};
use seq_validate::support::exited;
use seq_validate::unit_test::Report;

/// The `.db` beside the program's `.st`.
fn db_path(stem: &str) -> String {
    format!("{}/validate/{stem}.db", env!("CARGO_MANIFEST_DIR"))
}

/// `subThreadSleep.c`, the `sub` routine of `pvGetSync.db`.
fn sub_thread_sleep(rec: &mut dyn Record) -> epics_base_rs::error::CaResult<i64> {
    std::thread::sleep(Duration::from_secs(1));
    let Some(EpicsValue::Double(val)) = rec.get_field("VAL") else {
        panic!("sub VAL is a double");
    };
    rec.put_field("VAL", EpicsValue::Double(val + 1.0))?;
    Ok(0)
}

/// seqMain's `-d <stem>.db` and `iocInit`.
async fn ioc_init(stem: &str) -> Arc<PvDatabase> {
    let ioc = IocBuilder::new()
        .register_subroutine("subThreadSleep", sub_thread_sleep)
        .db_file(&db_path(stem), &HashMap::new())
        .expect("dbLoadRecords")
        .build_ioc()
        .await
        .expect("iocBuild");
    let db = Arc::clone(ioc.database());
    assert!(ioc.run().await, "iocRun");
    db
}

/// A TAP stream's plan, `ok` / `not ok` lines and `# ` diagnostics.
struct Tap {
    plan: i32,
    /// What follows the number: the description after ` - `, or a
    /// `testSkip`'s `# SKIP <why>`.
    results: Vec<(bool, String)>,
    diags: Vec<String>,
}

fn parse_tap(tap: &str) -> Tap {
    let mut plan = None;
    let mut results = Vec::new();
    let mut diags: Vec<String> = Vec::new();
    // Whether the last line was a diagnostic's: `testDiag` is `printf("# ")`,
    // the text, `putchar('\n')`, so a `\n` in the text, a last one too, puts
    // the rest of it on lines of its own.
    let mut in_diag = false;
    for line in tap.lines() {
        if let Some(n) = line.strip_prefix("1..") {
            plan = Some(n.parse().expect("plan"));
            in_diag = false;
        } else if let Some(diag) = line.strip_prefix("# ") {
            diags.push(diag.to_owned());
            in_diag = true;
        } else if let Some((ok, rest)) = line
            .strip_prefix("not ok ")
            .map(|rest| (false, rest))
            .or_else(|| line.strip_prefix("ok ").map(|rest| (true, rest)))
        {
            // `ok %2d - %s` and `testSkip`'s `ok %2d # SKIP %s`.
            let (num, text) = rest.trim_start().split_once(' ').expect("num text");
            let desc = text.strip_prefix("- ").unwrap_or(text);
            assert!(
                text.starts_with("- ") || text.starts_with("# SKIP "),
                "{line}"
            );
            assert_eq!(num.parse::<usize>(), Ok(results.len() + 1), "{line}");
            results.push((ok, desc.to_owned()));
            in_diag = false;
        } else {
            assert!(in_diag, "{line:?} is no TAP line");
            let diag = diags.last_mut().expect("a diagnostic");
            diag.push('\n');
            diag.push_str(line);
        }
    }
    Tap {
        plan: plan.expect("a plan line"),
        results,
        diags,
    }
}

/// `testDiag("a\n")`, `testDiag("b\nc")` and `testSkip(1, "why")` as C
/// prints them.
#[test]
fn parse_tap_reads_multi_line_diagnostics_and_skips() {
    let tap = parse_tap("1..2\n# a\n\nok  1 - x\n# b\nc\nok  2 # SKIP why\n");
    assert_eq!(tap.plan, 2);
    assert_eq!(tap.diags, ["a\n", "b\nc"]);
    assert_eq!(
        tap.results,
        [(true, "x".to_owned()), (true, "# SKIP why".to_owned())]
    );
}

/// Run `prog` as `<stem>.t` does and hold its report to `golden`.
async fn validate<V: ProgramVars>(
    prog: &'static SeqProgram<V>,
    stem: &str,
    golden: &str,
    timeout: Duration,
) {
    let db = if std::path::Path::new(&db_path(stem)).exists() {
        let db = ioc_init(stem).await;
        sleep(Duration::from_millis(200)).await;
        db
    } else {
        // No `iocInit` in the program's process, so no PINI pass to wait for.
        let db = Arc::new(PvDatabase::new());
        db.mark_pini_done();
        db
    };
    seq(prog, "", 0, &db).expect("seq starts the program");
    check(stem, &wait_for_exit(stem, timeout).await, golden);
}

/// The report once the program has ended the process.
async fn wait_for_exit(stem: &str, timeout: Duration) -> Report {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(report) = exited() {
            return report;
        }
        assert!(
            Instant::now() < deadline,
            "{stem} did not exit in {timeout:?}: {:?}",
            seq_validate::unit_test::report()
        );
        sleep(Duration::from_millis(20)).await;
    }
}

/// C's text, in a description or a diagnostic, where the rs run-time fixes
/// the C defect that printed it: `(stem, C's, rs's)`.
const C_DEFECTS: &[(&str, &str, &str)] = &[
    // seq_if.c `completion_timeout` picks its message with `pvEventGet ?`,
    // a test of the constant 0, so a get's timeout says "put".
    (
        "pvGetSync",
        "put completion failure",
        "get completion failure",
    ),
];

fn check(stem: &str, report: &Report, golden: &str) {
    let Tap {
        plan,
        results: mut c,
        diags: mut c_diags,
    } = parse_tap(golden);
    for (_, c_text, rs_text) in C_DEFECTS.iter().filter(|(s, ..)| *s == stem) {
        let texts = c.iter_mut().map(|(_, desc)| desc).chain(&mut c_diags);
        for text in texts {
            *text = text.replace(c_text, rs_text);
        }
    }
    assert_eq!(report.planned, plan, "{stem}: testPlan");
    let rs: Vec<_> = report.results.iter().map(|r| r.ok).collect();
    let c_ok: Vec<_> = c.iter().map(|(ok, _)| *ok).collect();
    assert_eq!(rs, c_ok, "{stem}: results {:?}", report.results);
    // Descriptions as a multiset: state sets' results interleave as their
    // threads are scheduled, in C too (evflag's first round varies run to
    // run). C's `testOk1` description is the generated C expression, which
    // has no rs counterpart, so it only counts.
    let mut c_descs: Vec<&str> = c.iter().map(|(_, d)| d.as_str()).collect();
    for desc in report.results.iter().filter_map(|r| r.desc.as_deref()) {
        let desc = desc.lines().next().unwrap_or("");
        let at = c_descs.iter().position(|d| *d == desc);
        let at = at.unwrap_or_else(|| panic!("{stem}: {desc:?} is none of C's {c_descs:?}"));
        c_descs.swap_remove(at);
    }
    let testok1 = report.results.iter().filter(|r| r.desc.is_none()).count();
    assert_eq!(testok1, c_descs.len(), "{stem}: testOk1 results");
    // Diagnostics as a multiset for the same reason; each is the same in
    // every C run, rand()'s too, as nothing seeds it.
    let mut rs_diags = report.diags.clone();
    rs_diags.sort();
    c_diags.sort();
    assert_eq!(rs_diags, c_diags, "{stem}: testDiag");
    assert!(report.done.is_some(), "{stem}: testDone");
}

macro_rules! validate {
    ($case:ident, $module:ident :: $prog:ident, $stem:literal, $secs:literal) => {
        #[epics_base_rs::epics_test]
        async fn $case() {
            validate(
                &seq_validate::$module::$prog,
                $stem,
                include_str!(concat!("c/", $stem, ".tap")),
                Duration::from_secs($secs),
            )
            .await;
        }
    };
}

validate!(assign, assign::assignTest, "assign", 30);
validate!(bittypes, bittypes::bittypesTest, "bittypes", 30);
validate!(change, change::changeTest, "change", 30);
validate!(
    comma_operator,
    comma_operator::commaOperatorTest,
    "commaOperator",
    30
);
validate!(entry_opte, entry_opte::entryOpteTest, "entryOpte", 30);
validate!(evflag, evflag::evflagTest, "evflag", 60);
validate!(evflag_ext, evflag_ext::evflagExtTest, "evflagExt", 30);
validate!(exit_optx, exit_optx::exitOptxTest, "exitOptx", 30);
validate!(local, local::localTest, "local", 30);
validate!(
    monitor_evflag,
    monitor_evflag::monitorEvflagTest,
    "monitorEvflag",
    120
);
validate!(norace, norace::noraceTest, "norace", 60);
validate!(optt_var, optt_var::opttVarTest, "opttVar", 30);
validate!(
    pv_assign_stress,
    pv_assign_stress::pvAssignStressTest,
    "pvAssignStress",
    60
);
validate!(
    pv_assign_subst,
    pv_assign_subst::pvAssignSubstTest,
    "pvAssignSubst",
    30
);
validate!(pv_get_async, pv_get_async::pvGetAsyncTest, "pvGetAsync", 30);
validate!(
    pv_put_and_monitor,
    pv_put_and_monitor::pvPutAndMonitorTest,
    "pvPutAndMonitor",
    60
);
validate!(pv_put_async, pv_put_async::pvPutAsyncTest, "pvPutAsync", 60);
validate!(pv_sync_db, pv_sync_db::pvSyncDbTest, "pvSyncDb", 60);
validate!(
    pv_sync_no_db,
    pv_sync_no_db::pvSyncNoDbTest,
    "pvSyncNoDb",
    60
);
validate!(reassign, reassign::reassignTest, "reassign", 30);
validate!(
    safe_mode_not_assigned,
    safe_mode_not_assigned::safeModeNotAssignedTest,
    "safeModeNotAssigned",
    30
);
validate!(
    safe_monitor,
    safe_monitor::safeMonitorTest,
    "safeMonitor",
    30
);
validate!(stop, stop::stopTest, "stop", 30);
validate!(userfunc, userfunc::userfuncTest, "userfunc", 30);
validate!(userfunc_ef, userfunc_ef::userfuncEfTest, "userfuncEf", 30);

/// `<stem>Ioc.t`: `seqMain -S -d <stem>.db` serves the database over CA, and
/// `seqMain -S -t`, with no database, runs the program against it.
#[cfg(tokio_backend)]
mod ioc {
    use super::*;

    use epics_base_rs::runtime::task::Reactor;
    use epics_base_rs::server::access_security::new_acf_cell_watching;
    use epics_ca_rs::calink::install_calink_resolver;
    use epics_ca_rs::server::CaServerBuilder;

    async fn validate_ioc<V: ProgramVars>(
        prog: &'static SeqProgram<V>,
        stem: &str,
        golden: &str,
        timeout: Duration,
    ) {
        let ioc_db = ioc_init(stem).await;
        let acf = new_acf_cell_watching(None, &ioc_db);
        let server = CaServerBuilder::serving(ioc_db, acf, None, None)
            .port(0)
            .build()
            .await
            .expect("the IOC's CA server");
        let port = server.udp_port();
        tokio::spawn(async move { server.run().await });

        // SAFETY: the process is this case's alone, and nothing has read the
        // CA environment yet.
        unsafe {
            std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
            std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        }
        // The program's process: no records, no `iocInit`, every name remote.
        let db = Arc::new(PvDatabase::new());
        install_calink_resolver(&db, Reactor::current().expect("test runtime")).await;
        db.mark_pini_done();
        seq(prog, "", 0, &db).expect("seq starts the program");
        check(stem, &wait_for_exit(stem, timeout).await, golden);
    }

    macro_rules! validate_ioc {
        ($case:ident, $module:ident :: $prog:ident, $stem:literal, $secs:literal) => {
            // Multi-thread, as `pvGetSync.db`'s `sub` blocks the server's
            // processing thread for a second.
            #[tokio::test(flavor = "multi_thread")]
            async fn $case() {
                validate_ioc(
                    &seq_validate::$module::$prog,
                    $stem,
                    include_str!(concat!("c/", $stem, ".tap")),
                    Duration::from_secs($secs),
                )
                .await;
            }
        };
    }

    validate_ioc!(bittypes, bittypes::bittypesTest, "bittypes", 30);
    validate_ioc!(evflag, evflag::evflagTest, "evflag", 60);
    validate_ioc!(
        monitor_evflag,
        monitor_evflag::monitorEvflagTest,
        "monitorEvflag",
        120
    );
    validate_ioc!(norace, norace::noraceTest, "norace", 60);
    validate_ioc!(
        pv_assign_stress,
        pv_assign_stress::pvAssignStressTest,
        "pvAssignStress",
        60
    );
    validate_ioc!(
        pv_assign_subst,
        pv_assign_subst::pvAssignSubstTest,
        "pvAssignSubst",
        30
    );
    // 100000 gets over CA, and 40000 puts and gets: C takes 9 and 8 s, rs
    // optimized 11 and 8 s, unoptimized near a minute.
    validate_ioc!(
        pv_get_async,
        pv_get_async::pvGetAsyncTest,
        "pvGetAsync",
        240
    );
    validate_ioc!(
        pv_get_complete,
        pv_get_complete::pvGetCompleteTest,
        "pvGetComplete",
        240
    );
    validate_ioc!(pv_get_sync, pv_get_sync::pvGetSyncTest, "pvGetSync", 30);
    validate_ioc!(
        pv_put_and_monitor,
        pv_put_and_monitor::pvPutAndMonitorTest,
        "pvPutAndMonitor",
        60
    );
    validate_ioc!(pv_put_async, pv_put_async::pvPutAsyncTest, "pvPutAsync", 60);
    validate_ioc!(pv_sync_db, pv_sync_db::pvSyncDbTest, "pvSyncDb", 60);
    validate_ioc!(reassign, reassign::reassignTest, "reassign", 30);
}
