//! TPRO prints C `dbProcess`'s trace lines.
//!
//! softIoc 8f5015b66, `dbpf L.TPRO 1`, `dbpf Z.TPRO 1`, `dbpf L.PROC 1` on
//! `record(longout,"L"){field(OUT,"Z.PROC")}` and
//! `record(longout,"Z"){field(OUT,"L.PROC")}`:
//!
//! ```text
//! _main_: dbProcess of 'L'
//! _main_: dbProcess of 'Z'
//! _main_: dbProcess of Active 'L' with RPRO=0
//! ```
//!
//! then `L.LCNT 1`, `L.SEVR INVALID`, `Z.LCNT 0`. The same softIoc on
//! `record(ai,"PY"){field(PINI,"YES") field(TPRO,"1")}` and
//! `record(ai,"SC"){field(SCAN,"1 second") field(TPRO,"1")}`, one period:
//!
//! ```text
//! _main_: dbProcess of 'PY'
//! scan-1: dbProcess of 'SC'
//! ```
//!
//! The context before the colon is the thread's name. It is compared only for
//! the scan thread: the port runs PINI on its `scan-owner` thread, not on the
//! thread that called `iocInit`.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const CHILD: &str = "TPRO_TRACE_CHILD";

/// This test run again in a child whose stdout is not captured, and the
/// `dbProcess` lines it printed.
fn trace_of(test: &str) -> Vec<String> {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| l.contains(": dbProcess"))
        .map(str::to_string)
        .collect()
}

/// `line` without its context.
fn uncontexted(line: &str) -> &str {
    line.split_once(": ").map_or(line, |(_, r)| r)
}

#[epics_macros_rs::epics_test]
async fn a_longout_loop_through_proc_prints_the_refused_reentry() {
    if std::env::var_os(CHILD).is_none() {
        let trace = trace_of("a_longout_loop_through_proc_prints_the_refused_reentry");
        assert_eq!(
            trace.iter().map(|l| uncontexted(l)).collect::<Vec<_>>(),
            [
                "dbProcess of 'L'",
                "dbProcess of 'Z'",
                "dbProcess of Active 'L' with RPRO=0",
            ]
        );
        return;
    }
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
record(longout, "L") { field(OUT, "Z.PROC") }
record(longout, "Z") { field(OUT, "L.PROC") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    for r in ["L", "Z"] {
        db.put_pv(&format!("{r}.TPRO"), EpicsValue::Char(1))
            .await
            .unwrap();
    }
    db.put_record_field_from_ca("L", "PROC", EpicsValue::Char(1))
        .await
        .unwrap();
    let num = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    assert_eq!(
        [num("L.LCNT"), num("L.SEVR"), num("Z.LCNT")],
        [1.0, 3.0, 0.0]
    );
}

#[epics_macros_rs::epics_test]
async fn a_pini_record_and_a_scanned_one_print_their_dbprocess_lines() {
    if std::env::var_os(CHILD).is_none() {
        let trace = trace_of("a_pini_record_and_a_scanned_one_print_their_dbprocess_lines");
        assert_eq!(trace.len(), 2, "{trace:?}");
        assert_eq!(uncontexted(&trace[0]), "dbProcess of 'PY'");
        assert_eq!(trace[1], "scan-1: dbProcess of 'SC'");
        return;
    }
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
record(ai, "PY") { field(PINI, "YES") field(TPRO, "1") }
record(ai, "SC") { field(SCAN, "1 second") field(TPRO, "1") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    drop(owner);
}
