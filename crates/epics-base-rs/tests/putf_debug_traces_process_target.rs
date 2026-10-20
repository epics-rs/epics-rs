//! `var dbAccessDebugPUTF 1` prints C `processTarget`'s line for each link
//! target a traced lock set reaches (`dbDbLink.c:462-494`): the PUTF it
//! hands an idle target, the RPRO it sets on an active one, or that it left
//! an active one alone. softIoc 8f5015b66, after `var dbAccessDebugPUTF 1`,
//! on the records each test names.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;

const CHILD: &str = "PROCESS_TARGET_TRACE_CHILD";

/// This test run again in a child whose stdout is not captured, and the
/// trace lines it printed.
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
        .filter(|l| l.contains(": dbProcess") || l.contains(" -> "))
        .map(|l| {
            l.trim_start_matches(&format!("test {test} ... "))
                .to_string()
        })
        .collect()
}

/// Load `records` and run `lines` in an iocsh on the `_main_` thread.
async fn run(records: &str, lines: &'static [&'static str]) {
    let (db, _): (Arc<PvDatabase>, _) = IocBuilder::new()
        .db_string(records, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
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

const CHAIN: &str = r#"
record(calc, "A") { field(TPRO, "1") field(FLNK, "B") }
record(calc, "B") { field(FLNK, "C") }
record(calc, "C") { }
"#;

/// Every source in the traced set prints, `B` as well as the TPRO `A`.
#[epics_macros_rs::epics_test]
async fn an_idle_target_prints_the_putf_it_takes() {
    if std::env::var_os(CHILD).is_none() {
        assert_eq!(
            trace_of("an_idle_target_prints_the_putf_it_takes"),
            [
                "_main_: dbProcess of 'A'",
                "_main_: 'A' -> 'B' with PUTF=1",
                "_main_: dbProcess of 'B'",
                "_main_: 'B' -> 'C' with PUTF=1",
                "_main_: dbProcess of 'C'",
            ]
        );
        return;
    }
    run(CHAIN, &["var dbAccessDebugPUTF 1", "dbpf A.PROC 1"]).await;
}

#[epics_macros_rs::epics_test]
async fn with_the_knob_clear_process_target_prints_nothing() {
    if std::env::var_os(CHILD).is_none() {
        assert_eq!(
            trace_of("with_the_knob_clear_process_target_prints_nothing"),
            [
                "_main_: dbProcess of 'A'",
                "_main_: dbProcess of 'B'",
                "_main_: dbProcess of 'C'",
            ]
        );
        return;
    }
    run(CHAIN, &["dbpf A.PROC 1"]).await;
}

/// A target on the chain is active and not claimed: `done`.
#[epics_macros_rs::epics_test]
async fn a_target_on_the_chain_prints_done() {
    if std::env::var_os(CHILD).is_none() {
        assert_eq!(
            trace_of("a_target_on_the_chain_prints_done"),
            [
                "_main_: dbProcess of 'L1'",
                "_main_: 'L1' -> 'L2' with PUTF=1",
                "_main_: dbProcess of 'L2'",
                "_main_: 'L2' -> Active 'L1', done",
                "_main_: dbProcess of Active 'L1' with RPRO=0",
            ]
        );
        return;
    }
    run(
        r#"
record(calc, "L1") { field(TPRO, "1") field(FLNK, "L2") }
record(calc, "L2") { field(FLNK, "L1") }
"#,
        &["var dbAccessDebugPUTF 1", "dbpf L1.PROC 1"],
    )
    .await;
}

/// A target busy in its ODLY takes RPRO from a put-driven source.
#[epics_macros_rs::epics_test]
async fn an_async_target_prints_the_rpro_it_takes() {
    if std::env::var_os(CHILD).is_none() {
        assert_eq!(
            trace_of("an_async_target_prints_the_rpro_it_takes"),
            [
                "_main_: dbProcess of 'S'",
                "_main_: 'S' -> Active 'D', setting RPRO=1",
                "_main_: dbProcess of Active 'D' with RPRO=1",
            ]
        );
        return;
    }
    run(
        r#"
record(calc, "S") { field(TPRO, "1") field(FLNK, "D") }
record(calcout, "D") { field(CALC, "1") field(ODLY, "0.5") }
"#,
        &["var dbAccessDebugPUTF 1", "dbpf D.PROC 1", "dbpf S.PROC 1"],
    )
    .await;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1500)).await;
}

/// The knob alone prints nothing: the source's set must be traced.
#[epics_macros_rs::epics_test]
async fn an_untraced_set_prints_nothing() {
    if std::env::var_os(CHILD).is_none() {
        assert_eq!(
            trace_of("an_untraced_set_prints_nothing"),
            Vec::<String>::new()
        );
        return;
    }
    run(
        r#"
record(calc, "U") { field(FLNK, "V") }
record(calc, "V") { }
"#,
        &["var dbAccessDebugPUTF 1", "dbpf U.PROC 1"],
    )
    .await;
}
