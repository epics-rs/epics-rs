//! A group put whose member record is active prints pvxs
//! `doPostProcessing`'s RPRO line under `var dbAccessDebugPUTF 1`
//! (`iocsource.cpp:404-406`, reached from `groupsource.cpp:570`), atomic or
//! not, Passive or forced.
#![cfg(feature = "qsrv-core")]

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;
use epics_bridge_rs::qsrv::{BridgeProvider, ProcessMode, PutOptions, group::GroupChannel};
use epics_pva_rs::pvdata::{PvField, PvStructure, ScalarValue};

const CHILD: &str = "GROUP_PUTF_CHILD";
const TEST: &str = "a_group_put_to_an_active_record_prints_the_pvxs_line";

// RTEMS-EXEC-MODEL-ALLOW(1): checked - runs and passes in the exec-backend
// suite.
#[tokio::test]
async fn a_group_put_to_an_active_record_prints_the_pvxs_line() {
    if std::env::var_os(CHILD).is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        let lines: Vec<&str> = stdout
            .lines()
            .filter_map(|l| l.split_once(": single source onPut").map(|(_, r)| r))
            .collect();
        assert_eq!(lines, [" to Active 'D', setting RPRO=1"; 3], "{stdout}");
        return;
    }
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"record(calcout, "D") { field(TPRO, "1") field(CALC, "1") field(ODLY, "0.3") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let shell_db = db.clone();
    std::thread::spawn(move || {
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        let shell = IocShell::new(shell_db, bridge);
        shell.execute_line("var dbAccessDebugPUTF 1").unwrap();
        shell.execute_line("dbpf D.PROC 1").unwrap();
    })
    .join()
    .unwrap();

    let provider = BridgeProvider::new(db.clone());
    for (atomic, process) in [
        (true, ProcessMode::Passive),
        (true, ProcessMode::Force),
        (false, ProcessMode::Force),
    ] {
        provider
            .load_group_config(&format!(
                r#"{{ "G{atomic}": {{
                    "+atomic": {atomic},
                    "p": {{ "+channel": "D.PROC", "+type": "plain", "+putorder": 0 }}
                }} }}"#
            ))
            .expect("load");
        provider.process_groups().await;
        let def = provider
            .groups()
            .get(&format!("G{atomic}"))
            .cloned()
            .expect("group");
        let mut put = PvStructure::new("epics:nt/NTGroup:1.0");
        put.fields
            .push(("p".into(), PvField::Scalar(ScalarValue::Int(1))));
        GroupChannel::new(db.clone(), def)
            .put_with_options(
                &put,
                PutOptions {
                    process,
                    block: false,
                },
                None,
                &Default::default(),
            )
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(1000)).await;
}
