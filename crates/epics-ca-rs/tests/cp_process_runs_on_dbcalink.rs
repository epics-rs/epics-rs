//! A CP link's holder is processed by `dbCaTask` (`dbCa.c:1314-1321`), on
//! the `dbCaLink` thread `dbCaLinkInit` creates (`dbCa.c:355`), so its TPRO
//! lines carry that name. softIoc R7.0.10.1 (8f5015b66):
//!
//! ```text
//! $ printf 'dbpf SRC 1\nepicsThreadSleep 0.5\n' | stdbuf -o0 softIoc -d loop.db 2>&1
//! ...
//! dbCaLink: dbProcess of 'DST'
//! dbCaLink: dbProcess of 'LOOP'
//! dbCaLink: dbProcess of Active 'DST' with RPRO=0
//! iocRun: All initialization complete
//! epics> DBF_DOUBLE:         1
//! epics> dbCaLink: dbProcess of 'DST'
//! dbCaLink: dbProcess of 'LOOP'
//! dbCaLink: dbProcess of Active 'DST' with RPRO=0
//! ```

// `softioc-rs` serves through the async CA front-end.
#![cfg(tokio_backend)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};

const LOOP_DB: &str = "\
record(ao, \"SRC\") {}
record(calc, \"DST\") { field(INPA, \"SRC CP\") field(TPRO, \"1\") field(CALC, \"A\") field(FLNK, \"LOOP\") }
record(calc, \"LOOP\") { field(CALC, \"1\") field(FLNK, \"DST\") }
";

/// `softioc-rs -d` on `db` fed `dbpf SRC 1`: its `dbProcess` trace lines.
fn traced(db: &str) -> Vec<String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cp.db");
    std::fs::write(&path, db).expect("write db");
    let (mut output, writer) = std::io::pipe().expect("pipe");
    let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .args(["--port", "0", "-d"])
        .arg(&path)
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(writer.try_clone().expect("clone the pipe"))
        .stderr(writer)
        .spawn()
        .expect("spawn softioc-rs");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"dbpf SRC 1\nepicsThreadSleep 0.5\n")
        .expect("write the shell lines");
    let mut out = String::new();
    output.read_to_string(&mut out).expect("read the output");
    child.wait().expect("reap softioc-rs");

    out.lines()
        .map(|l| l.rsplit_once("epics> ").map_or(l, |(_, rest)| rest))
        .map(|l| l.trim_start_matches("\u{1b}[0m"))
        .filter(|l| l.contains("dbProcess of"))
        .map(str::to_string)
        .collect()
}

#[test]
fn a_local_cp_holder_traces_as_dbcalink_like_soft_ioc() {
    let pass = [
        "dbCaLink: dbProcess of 'DST'",
        "dbCaLink: dbProcess of 'LOOP'",
        "dbCaLink: dbProcess of Active 'DST' with RPRO=0",
    ];
    assert_eq!(traced(LOOP_DB), [pass, pass].concat());
}

/// Two holders of one source are processed in link order, at init and on
/// a post: their monitors are posted in that order and `dbCaTask` takes
/// its `workList` in turn. softIoc, three runs of three:
///
/// ```text
/// dbCaLink: dbProcess of 'A'
/// dbCaLink: dbProcess of 'B'
/// epics> DBF_DOUBLE:         1
/// epics> dbCaLink: dbProcess of 'A'
/// dbCaLink: dbProcess of 'B'
/// ```
#[test]
fn two_cp_holders_of_one_source_trace_in_link_order() {
    let db = "\
record(ao, \"SRC\") {}
record(calc, \"A\") { field(INPA, \"SRC CP\") field(TPRO, \"1\") field(CALC, \"A\") }
record(calc, \"B\") { field(INPA, \"SRC CP\") field(TPRO, \"1\") field(CALC, \"A\") }
";
    let pass = ["dbCaLink: dbProcess of 'A'", "dbCaLink: dbProcess of 'B'"];
    assert_eq!(traced(db), [pass, pass].concat());
}

/// A remote channel's CP holder runs on `dbCaLink` too: `dbCaTask` serves
/// every link's `CA_DBPROCESS`, whichever IOC its channel is on.
#[cfg(feature = "client-core")]
#[tokio::test(flavor = "multi_thread")]
#[serial_test::serial(epics_env)]
async fn a_remote_cp_holder_processes_on_dbcalink() {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use epics_base_rs::server::ioc_builder::IocBuilder;
    use epics_base_rs::types::EpicsValue;
    use epics_ca_rs::calink::install_calink_resolver;
    use epics_ca_rs::server::CaServer;

    static RAN_ON: Mutex<Vec<String>> = Mutex::new(Vec::new());

    let server = Arc::new(
        CaServer::builder()
            .port(0)
            .pv("REMOTE:SRC", EpicsValue::Double(0.0))
            .build()
            .await
            .expect("CA server"),
    );
    let port = server.udp_port();
    let serving = server.clone();
    let _server = tokio::spawn(async move { serving.run().await });
    // SAFETY: `#[serial(epics_env)]` keeps every env-touching test apart.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }

    let (db, _h) = IocBuilder::new()
        .register_context_subroutine(
            "whereAmI",
            Box::new(|_rec, _ctx| {
                let name = std::thread::current().name().unwrap_or("").to_string();
                RAN_ON.lock().unwrap().push(name);
                Ok(0)
            }),
        )
        .db_string(
            r#"record(aSub, "HOLDER") { field(INPA, "REMOTE:SRC CP") field(SNAM, "whereAmI") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let resolver = install_calink_resolver(
        &db,
        epics_base_rs::runtime::task::Reactor::current().expect("test runtime"),
    )
    .await;
    db.initialize_link_locality().await;
    // The build ran with no `ca` lset: its `dbCaAddLink` pass is this one.
    db.setup_external_link_opens().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    assert!(
        resolver
            .wait_for_link_connected("REMOTE:SRC", fact_budget::FACT_BUDGET)
            .await,
        "the remote link connects"
    );

    server
        .put("REMOTE:SRC", EpicsValue::Double(1.0))
        .await
        .expect("post the remote change");
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while RAN_ON.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let ran = RAN_ON.lock().unwrap().clone();
    assert!(ran.len() >= 2, "the first event and the change: {ran:?}");
    assert!(ran.iter().all(|t| t == "dbCaLink"), "{ran:?}");
}
