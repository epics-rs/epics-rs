//! `var dbAccessDebugPUTF 1` prints the line each C put-driven process
//! prints when it finds its TPRO record active and defers to RPRO:
//!
//! ```text
//! _main_: dbPutField to Active 'D', setting RPRO=1
//! ```
//!
//! from `dbPutField` (`dbAccess.c:1272-1274`) and `db_process`
//! (`db_access.c:1049-1051`, a CP link's `CA_DBPROCESS`), and
//! `single source onPut to Active` from pvxs `doPostProcessing`
//! (`iocsource.cpp:404-406`). Clear (C's default) it prints nothing.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::{FieldPut, ProcessMode, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::types::EpicsValue;

const CHILD: &str = "PUTF_TRACE_CHILD";

/// A TPRO calcout that stays active for its ODLY after each process.
const DELAYED: &str =
    r#"record(calcout, "D") { field(TPRO, "1") field(CALC, "1") field(ODLY, "0.3") }"#;

/// This test run again in a child whose stdout is not captured, and the
/// lines it printed that contain `what`.
fn lines_of(test: &str, what: &str) -> Vec<String> {
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
        .filter(|l| l.contains(what))
        .map(|l| {
            l.trim_start_matches(&format!("test {test} ... "))
                .to_string()
        })
        .collect()
}

/// `line` without its context.
fn uncontexted(line: &str) -> &str {
    line.split_once(": ").map_or(line, |(_, r)| r)
}

/// Run `lines` in an iocsh on the `_main_` thread.
async fn shell(db: Arc<PvDatabase>, lines: &'static [&'static str]) {
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    epics_base_rs::runtime::task::run_on_main_thread(move || {
        let shell = IocShell::new(db, bridge);
        for line in lines {
            shell.execute_line(line).unwrap();
        }
    })
    .unwrap()
    .await
    .unwrap();
}

async fn delayed() -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DELAYED, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// Long enough for the RPRO reprocess and its ODLY to end.
async fn settle() {
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1000)).await;
}

#[epics_macros_rs::epics_test]
async fn a_shell_dbpf_to_an_active_record_prints_the_dbputfield_line() {
    if std::env::var_os(CHILD).is_none() {
        let name = "a_shell_dbpf_to_an_active_record_prints_the_dbputfield_line";
        assert_eq!(
            lines_of(name, "to Active"),
            ["_main_: dbPutField to Active 'D', setting RPRO=1"]
        );
        assert_eq!(
            lines_of(name, "dbProcess of 'D'").len(),
            2,
            "RPRO reprocessed"
        );
        return;
    }
    let db = delayed().await;
    shell(
        db,
        &["var dbAccessDebugPUTF 1", "dbpf D.PROC 1", "dbpf D.PROC 1"],
    )
    .await;
    settle().await;
}

#[epics_macros_rs::epics_test]
async fn with_the_knob_clear_the_rpro_deferral_prints_nothing() {
    if std::env::var_os(CHILD).is_none() {
        let name = "with_the_knob_clear_the_rpro_deferral_prints_nothing";
        assert_eq!(lines_of(name, "to Active"), Vec::<String>::new());
        assert_eq!(
            lines_of(name, "dbProcess of 'D'").len(),
            2,
            "RPRO reprocessed"
        );
        return;
    }
    let db = delayed().await;
    shell(db, &["dbpf D.PROC 1", "dbpf D.PROC 1"]).await;
    settle().await;
}

/// C's context is `dbCaLink`, the `dbCaTask` thread; the port runs a CP
/// process on the link reactor, so the context is not compared.
#[epics_macros_rs::epics_test]
async fn a_cp_update_to_an_active_record_prints_the_db_process_line() {
    if std::env::var_os(CHILD).is_none() {
        let name = "a_cp_update_to_an_active_record_prints_the_db_process_line";
        let lines = lines_of(name, "to Active");
        assert_eq!(
            lines.iter().map(|l| uncontexted(l)).collect::<Vec<_>>(),
            ["dbPutField to Active 'T', setting RPRO=1"]
        );
        return;
    }
    let ioc = IocBuilder::new()
        .db_string(
            r#"
record(ao, "S") { }
record(calcout, "T") { field(INPA, "S CP") field(CALC, "A") field(ODLY, "0.3") field(TPRO, "1") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    let pact = |db: &PvDatabase| db.get_pv("T.PACT").unwrap().to_f64().unwrap();
    // The first event's process ends before the knob goes on.
    settle().await;
    assert_eq!(pact(&db), 0.0);
    shell(db.clone(), &["var dbAccessDebugPUTF 1"]).await;
    db.put_pv_and_post("S", EpicsValue::Double(1.0))
        .await
        .unwrap();
    while pact(&db) == 0.0 {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    db.put_pv_and_post("S", EpicsValue::Double(2.0))
        .await
        .unwrap();
    db.ca_sync().await;
    settle().await;
}

/// pvxs prints its own line from both the Passive and the Force routes.
#[epics_macros_rs::epics_test]
async fn a_client_put_to_an_active_record_prints_the_pvxs_line() {
    if std::env::var_os(CHILD).is_none() {
        let name = "a_client_put_to_an_active_record_prints_the_pvxs_line";
        let lines = lines_of(name, "to Active");
        assert_eq!(
            lines.iter().map(|l| uncontexted(l)).collect::<Vec<_>>(),
            [
                "single source onPut to Active 'D', setting RPRO=1",
                "single source onPut to Active 'D', setting RPRO=1",
            ]
        );
        return;
    }
    let db = delayed().await;
    shell(db.clone(), &["var dbAccessDebugPUTF 1", "dbpf D.PROC 1"]).await;
    for process in [ProcessMode::Passive, ProcessMode::Force] {
        db.put_field_from_client("D", "PROC", EpicsValue::Char(1), process, false)
            .await
            .unwrap();
    }
    settle().await;
}

/// A device's `dbPutField` from inside its own record's hold is
/// `dbPutField`'s, line and all.
#[epics_macros_rs::epics_test]
async fn a_held_db_put_field_to_an_active_record_prints_the_dbputfield_line() {
    if std::env::var_os(CHILD).is_none() {
        let name = "a_held_db_put_field_to_an_active_record_prints_the_dbputfield_line";
        let lines = lines_of(name, "to Active");
        assert_eq!(
            lines.iter().map(|l| uncontexted(l)).collect::<Vec<_>>(),
            ["dbPutField to Active 'D', setting RPRO=1"]
        );
        return;
    }
    let db = IocBuilder::new()
        .db_string(
            &format!("{DELAYED}\nrecord(ao, \"H\") {{ }}"),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    shell(db.clone(), &["var dbAccessDebugPUTF 1", "dbpf D.PROC 1"]).await;
    let h = db.async_handle();
    let proc_ = h.name_to_dbaddr("D.PROC", None).unwrap();
    let put = h.with_held("H", |held| held.db_put_field(&proc_, EpicsValue::Char(1)));
    assert!(matches!(put, Some(FieldPut::Done(Ok(())))), "{put:?}");
    settle().await;
}
