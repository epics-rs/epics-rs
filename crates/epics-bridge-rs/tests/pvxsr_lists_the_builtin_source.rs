//! `pvxsr` on a QSRV IOC lists the server's sources as softIocPVX 1.5.3 does
//! on the same db:
//!
//! ```text
//! Source: __builtin prio=-1
//! Source: __server prio=-1
//! Source: qsrvSingle prio=0
//! Source: qsrvGroup prio=1
//! ```
//!
//! each line ending in a space. pvxs's `Server::Pvt` constructor puts its
//! `addPV` store at `(-1, "__builtin")` beside `__server`
//! (`server.cpp:542-547`).
//!
//! From detail 1 each line carries the source's `show` (`server.cpp:299-302`)
//! and its names, sorted, one per line under it.
//!
//! The IOC ends in the iocsh REPL, so it runs in a child (this binary re-run
//! on [`ioc_child`]) whose piped stdin types the command.
#![cfg(all(feature = "qsrv", unix))]

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CHILD: &str = "PVXSR_BUILTIN_CHILD_STCMD";

/// The child half; a no-op unless the parent set [`CHILD`].
#[test]
fn ioc_child() {
    let Ok(stcmd) = std::env::var(CHILD) else {
        return;
    };
    epics_base_rs::runtime::task::test_block_on(epics_bridge_rs::qsrv::run_ca_pva_qsrv_ioc_app(
        epics_base_rs::server::ioc_app::IocApplication::new()
            .port(0)
            .startup_script(&stcmd),
    ))
    .expect("the IOC runs");
}

/// The `pvxsr` report lines of an IOC on `db` whose script ends in
/// `after_init` and that is typed `input`: each `Source:` line and each name
/// under one.
fn report_lines(db_text: &str, after_init: &str, input: &[u8]) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("x.db");
    std::fs::write(&db, db_text).unwrap();
    let stcmd = dir.path().join("st.cmd");
    std::fs::write(
        &stcmd,
        format!("dbLoadRecords(\"{}\")\niocInit\n{after_init}", db.display()),
    )
    .unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["ioc_child", "--exact", "--nocapture"])
        .env(CHILD, &stcmd)
        .env("EPICS_PVAS_SERVER_PORT", "0")
        .env("EPICS_PVAS_BROADCAST_PORT", "0")
        .env("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_PVAS_AUTO_BEACON_ADDR_LIST", "NO")
        .env("EPICS_PVAS_BEACON_ADDR_LIST", "")
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the IOC child");
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
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<String> = stdout
        .lines()
        .filter(|l| l.starts_with("Source: ") || l.starts_with("    "))
        .map(str::to_owned)
        .collect();
    assert!(
        !lines.is_empty(),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    lines
}

#[test]
fn pvxsr_lists_builtin_and_server_ahead_of_qsrv() {
    assert_eq!(
        report_lines("record(ao, \"X\") { }\n", "", b"pvxsr\nexit\n"),
        [
            "Source: __builtin prio=-1 ",
            "Source: __server prio=-1 ",
            "Source: qsrvSingle prio=0 ",
            "Source: qsrvGroup prio=1 ",
        ],
    );
}

/// softIocPVX 1.5.3 on the same db, `pvxsr 1`.
const PVXSR_1: [&str; 9] = [
    "Source: __builtin prio=-1 StaticProvider",
    "Source: __server prio=-1 Source",
    "Source: qsrvSingle prio=0 IOC",
    "    XA",
    "    XC",
    "    ZB",
    "Source: qsrvGroup prio=1 IOC",
    "    GRPA",
    "    GRPB",
];

const GROUPS_DB: &str = r#"record(ao, "ZB") { info(Q:group, {"GRPB":{"v":{+channel:"VAL"}}}) }
record(ao, "XA") { alias("XC") info(Q:group, {"GRPA":{"v":{+channel:"VAL"}}}) }
"#;

#[test]
fn pvxsr_1_shows_each_source() {
    assert_eq!(report_lines(GROUPS_DB, "", b"pvxsr 1\nexit\n"), PVXSR_1);
}

/// `iocInit` returns once every server layer is up (`iocInit.c:265`), so the
/// script's next line finds both PVA sources mounted.
#[test]
fn pvxsr_1_on_the_line_after_ioc_init_shows_each_source() {
    assert_eq!(report_lines(GROUPS_DB, "pvxsr 1\n", b"exit\n"), PVXSR_1);
}
