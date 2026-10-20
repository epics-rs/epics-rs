//! TPRO's context names the PVA client a put came from: QSRV2's `dbServer`
//! answers `qClient` (`ioc/singlesourcehooks.cpp:108-136`) from the
//! `CurrentOp` a non-waiting single put and a group put set
//! (`ioc/singlesource.cpp:371`, `ioc/groupsource.cpp:591`). softIocPVX 1.5.3
//! on the records below, then `pvxput X.PROC 1`, `pvxput -r
//! 'record[block=true]' X.PROC 1` and `pvxput grp a.value=5`:
//!
//! ```text
//! q2:root@127.0.0.1:34472: dbProcess of 'X'
//! q2:root@127.0.0.1:34472: dbProcess of 'Y'
//! PVXTCP: dbProcess of 'X'
//! PVXTCP: dbProcess of 'Y'
//! q2:root@127.0.0.1:34482: dbProcess of 'G1'
//! ```
//!
//! The blocking put goes to `dbProcessNotify` before the `CurrentOp`, so it
//! names the thread.

#![cfg(feature = "qsrv")]
#![cfg(tokio_backend)]

use std::sync::Arc;
use std::time::Duration;

use epics_bridge_rs::qsrv::{BridgeProvider, QsrvMount, QsrvPvStore};
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::pv_request::PvRequestExpr;
use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};

const CHILD: &str = "TPRO_PVA_CLIENT_CHILD";
const TEST: &str = "a_pva_put_traces_with_the_peer_s_name";

const DB: &str = r#"
record(calc, "X") { field(TPRO, "1") field(CALC, "VAL+1") field(FLNK, "Y") }
record(calc, "Y") { field(CALC, "VAL+1") }
record(ao, "G1") { field(TPRO, "1") }
"#;

const GROUP: &str = r#"{"grp": {"a": {"+channel": "G1.VAL", "+trigger": "*", "+putorder": 0}}}"#;

/// The `dbProcess` lines this test printed when run again in a child whose
/// stdout is not captured.
fn trace() -> Vec<String> {
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
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| l.contains(": dbProcess"))
        .map(|l| {
            l.trim_start_matches(&format!("test {TEST} ... "))
                .to_string()
        })
        .collect()
}

/// `line` as `q2:u@127.0.0.1:<port>: dbProcess of '<record>'`, its port.
fn peer_port(line: &str, record: &str) -> u16 {
    line.strip_prefix("q2:u@127.0.0.1:")
        .and_then(|l| l.strip_suffix(&format!(": dbProcess of '{record}'")))
        .and_then(|port| port.parse().ok())
        .unwrap_or_else(|| panic!("{line:?} is not the q2 client's trace of {record}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pva_put_traces_with_the_peer_s_name() {
    if std::env::var_os(CHILD).is_none() {
        let trace = trace();
        assert_eq!(trace.len(), 5, "{trace:#?}");
        let port = peer_port(&trace[0], "X");
        assert_eq!(peer_port(&trace[1], "Y"), port, "{trace:#?}");
        for (line, record) in trace[2..4].iter().zip(["X", "Y"]) {
            assert!(
                !line.starts_with("q2:") && line.ends_with(&format!(": dbProcess of '{record}'")),
                "{trace:#?}"
            );
        }
        assert_ne!(peer_port(&trace[4], "G1"), port, "{trace:#?}");
        return;
    }
    let (db, _) = epics_base_rs::server::ioc_builder::IocBuilder::new()
        .db_string(DB, &Default::default())
        .unwrap()
        .build()
        .await
        .unwrap();
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(GROUP).expect("load group");
    assert_eq!(provider.process_groups().await, 1);
    let store = QsrvPvStore::new(
        epics_base_rs::runtime::task::Reactor::current().expect("the test enters a runtime"),
        provider,
    );
    let composite = epics_pva_rs::server_native::CompositeSource::new();
    QsrvMount {
        store: Arc::new(store),
        acf: epics_base_rs::server::access_security::new_acf_cell(None),
    }
    .install_channel_sources(&db, &composite)
    .expect("qsrv sources mount");
    let server =
        PvaServer::start(composite, PvaServerConfig::isolated()).expect("qsrv server starts");
    // What pvxs's `initHookAtBeginning` and `iocRun` do for QSRV2.
    epics_pva_rs::server::iocsh::register_qsrv2_db_server();
    epics_base_rs::server::db_server::db_run_servers();

    let client = || {
        PvaClient::builder()
            .server_addr(server.tcp_addr())
            .user("u")
            .timeout(Duration::from_secs(5))
            .build()
    };
    client().pvput("X.PROC", "1").await.expect("put X.PROC");
    let block = PvRequestExpr::parse("record[block=true]").unwrap();
    client()
        .pvput_with_request("X.PROC", &block, "1")
        .await
        .expect("blocking put X.PROC");
    client()
        .pvput_args("grp", &["a.value=5".into()], None)
        .await
        .expect("put grp");
}
