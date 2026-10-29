//! pvxs `enable2()` decides once, from `pvxsBaseRegistrar`, for every pvxs
//! IOC (`ioc/iochooks.cpp:412-459,503-507`, pvxs 1.5.3): pvxs7x.dbd's
//! `link("pva", "lsetPVX")` loads whatever it says, `dbpvar` and
//! `pvaLinkNWorkers` register only when it is enabled (`pvalink.cpp:328-333`),
//! and its INFO line prints at registration unless it was told to be quiet.
//!
//! Each IOC ends in the iocsh REPL, so it runs in a child (this binary re-run
//! on [`ioc_child`]) whose piped stdin types the commands: one through
//! `run_pva_ioc_app`, one through `PvaServerBuilder`, which is
//! `pvxs_ioc_builder`'s.
#![cfg(all(feature = "client", unix))]

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CHILD: &str = "QSRV2_DECISION_CHILD_DB";
const ENTRY: &str = "QSRV2_DECISION_CHILD_ENTRY";
const INFO: &str = "INFO: PVXS QSRV2 is loaded, permitted, and ENABLED.\n";

/// The child half; a no-op unless the parent set [`CHILD`].
#[test]
fn ioc_child() {
    let Ok(db) = std::env::var(CHILD) else {
        return;
    };
    let result = match std::env::var(ENTRY).as_deref() {
        Ok("app") => {
            epics_base_rs::runtime::task::test_block_on(epics_pva_rs::server::run_pva_ioc_app(
                epics_base_rs::server::ioc_app::IocApplication::new()
                    .startup_line(&format!("dbLoadRecords(\"{db}\")")),
            ))
        }
        Ok("builder") => epics_base_rs::runtime::task::test_block_on(async move {
            epics_pva_rs::server::PvaServerBuilder::new()
                .port(0)
                .db_file(&db, &Default::default())?
                .build()
                .await?
                .run_with_shell(|_| {})
                .await
        }),
        other => panic!("{ENTRY}={other:?}"),
    };
    result.expect("the IOC runs");
}

/// stdout of an IOC entered by `entry` on a db holding a pva link, with
/// `PVXS_QSRV_ENABLE` as `enable` and `input` typed.
fn run(entry: &str, enable: Option<&str>, input: &[u8]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("x.db");
    std::fs::write(
        &db,
        "record(ai, \"LNK\") { field(INP, {pva:\"NOWHERE\"}) }\n",
    )
    .unwrap();
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["ioc_child", "--exact", "--nocapture"])
        .env(CHILD, &db)
        .env(ENTRY, entry)
        .env_remove("EPICS_IOC_IGNORE_SERVERS")
        .env("EPICS_PVAS_SERVER_PORT", "0")
        .env("EPICS_PVAS_BROADCAST_PORT", "0")
        .env("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_PVAS_AUTO_BEACON_ADDR_LIST", "NO")
        .env("EPICS_PVAS_BEACON_ADDR_LIST", "")
        .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_PVA_ADDR_LIST", "")
        .env_remove("EPICS_PVA_NAME_SERVERS");
    match enable {
        Some(v) => cmd.env("PVXS_QSRV_ENABLE", v),
        None => cmd.env_remove("PVXS_QSRV_ENABLE"),
    };
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the IOC child");
    // The one process this test started; it ends by this handle, never by
    // name.
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(input);
    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    // Both streams in one, as a console shows them.
    let console = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // `dbgf` answers only once the pva link's record loaded.
    assert!(
        out.status.success() && console.contains("DBF_DOUBLE:"),
        "the IOC never served LNK\n{console}"
    );
    console
}

/// softIocPVX 1.5.3 with `PVXS_QSRV_ENABLE=NO`: the link loads, the gated
/// command and variable are absent, and `enable2()` is quiet.
fn refuses_dbpvar_quietly(entry: &str) {
    let console = run(
        entry,
        Some("NO"),
        b"dbgf LNK\ndbpvar\nvar pvaLinkNWorkers\nexit\n",
    );
    for want in [
        "Command 'dbpvar' not registered.",
        "No known vars match 'pvaLinkNWorkers'.",
    ] {
        assert!(console.contains(want), "`{want}` missing\n{console}");
    }
    assert!(
        !console.contains("PVXS QSRV2"),
        "QSRV2 was not quiet\n{console}"
    );
}

/// softIocPVX 1.5.3 with no `PVXS_QSRV_ENABLE`: one INFO line, and the gated
/// variable registered.
fn prints_info_once(entry: &str) {
    let console = run(entry, None, b"dbgf LNK\nvar pvaLinkNWorkers\nexit\n");
    assert_eq!(console.matches(INFO).count(), 1, "{console}");
    assert!(
        !console.contains("No known vars match 'pvaLinkNWorkers'."),
        "{console}"
    );
}

#[test]
fn run_pva_ioc_app_with_qsrv2_off_refuses_dbpvar_quietly() {
    refuses_dbpvar_quietly("app");
}

#[test]
fn run_pva_ioc_app_prints_the_qsrv2_info_line_once() {
    prints_info_once("app");
}

#[test]
fn pva_server_builder_with_qsrv2_off_refuses_dbpvar_quietly() {
    refuses_dbpvar_quietly("builder");
}

#[test]
fn pva_server_builder_prints_the_qsrv2_info_line_once() {
    prints_info_once("builder");
}
