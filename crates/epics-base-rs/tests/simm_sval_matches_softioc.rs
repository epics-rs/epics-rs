//! SIOL simulation reads through SVAL on mbbi, mbbiDirect, bi, event and
//! stringin, both on the SDLY continuation and synchronously, against softIoc
//! (R7.0.10).
//! `fixtures/softioc/simm_sval.out` is softIoc's output for
//! `simm_sval.cmds` over `simm_sval.db`.

use std::collections::HashMap;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;

const DB: &str = include_str!("fixtures/softioc/simm_sval.db");
const CMDS: &str = include_str!("fixtures/softioc/simm_sval.cmds");
const C_OUT: &str = include_str!("fixtures/softioc/simm_sval.out");

#[epics_macros_rs::epics_test]
async fn simm_reads_siol_into_sval_like_softioc() {
    let (db, _) = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    // The shell blocks on the runtime, so it runs on a thread of its own.
    let rs = std::thread::spawn(move || {
        let shell = IocShell::new(db, bridge);
        let out = tempfile::NamedTempFile::new().unwrap();
        let mut rs = String::new();
        for line in CMDS.lines() {
            shell
                .execute_line(&format!("{line} > {}", out.path().display()))
                .unwrap();
            rs.push_str(&std::fs::read_to_string(out.path()).unwrap());
        }
        rs
    })
    .join()
    .unwrap();
    let diff: Vec<_> = rs
        .lines()
        .zip(C_OUT.lines())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, (a, b))| format!("{}: rs {a:?}\n    c  {b:?}", i + 1))
        .collect();
    assert!(
        diff.is_empty() && rs.lines().count() == C_OUT.lines().count(),
        "{} rs lines vs {} C lines\n{}",
        rs.lines().count(),
        C_OUT.lines().count(),
        diff.join("\n")
    );
}
