//! `PVXS_QSRV_ENABLE=NO` as softIocPVX 1.5.3 takes it: `addSingleSrc` and
//! `addGroupSrc` run at `initHookAfterIocBuilt` and pva links resolve
//! whatever `enable2()` said (`iochooks.cpp:333-372`); the decision gates
//! only what `single_enable`, `group_enable` and `pvalink_enable` register
//! (`iochooks.cpp:503-507`): `pvxsl`, `pvxgl`, `dbLoadGroup`, `dbpvar` and
//! `pvaLinkNWorkers`.
//!
//! The IOC ends in the iocsh REPL, so it runs in a child (this binary re-run
//! on [`ioc_child`]) whose piped stdin types the commands.
#![cfg(all(feature = "qsrv", unix))]

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CHILD: &str = "QSRV2_OFF_CHILD_STCMD";

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

/// stdout and stderr of a QSRV2-off IOC on the probe db, typed `input`.
fn run_disabled(input: &[u8]) -> (String, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("x.db");
    std::fs::write(
        &db,
        r#"record(ao, "ZB") { info(Q:group, {"GRPB":{"v":{+channel:"VAL"}}}) }
record(ao, "XA") { alias("XC") field(VAL, "5") field(PINI, "YES") info(Q:group, {"GRPA":{"v":{+channel:"VAL"}}}) }
record(ai, "LNK") { field(INP, {pva:"XA"}) field(SCAN, "1 second") }
"#,
    )
    .unwrap();
    let json = dir.path().join("g.json");
    std::fs::write(&json, "{}").unwrap();
    let stcmd = dir.path().join("st.cmd");
    std::fs::write(
        &stcmd,
        format!(
            "dbLoadGroup(\"{}\")\ndbLoadRecords(\"{}\")\niocInit\n",
            json.display(),
            db.display()
        ),
    )
    .unwrap();
    // The link reaches the IOC's own server by name server, so its TCP port
    // is fixed.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
        .to_string();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["ioc_child", "--exact", "--nocapture"])
        .env(CHILD, &stcmd)
        .env("PVXS_QSRV_ENABLE", "NO")
        .env("EPICS_PVAS_SERVER_PORT", &port)
        .env("EPICS_PVA_NAME_SERVERS", format!("127.0.0.1:{port}"))
        .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_PVA_ADDR_LIST", "")
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
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    );
    // The gated commands register at iocInit's link-set install and in the
    // protocol runner, so their absence means nothing unless both ran.
    assert!(
        out.status.success() && stderr.contains("iocRun: All initialization complete\n"),
        "the IOC never served\nstdout: {stdout}\nstderr: {stderr}"
    );
    (stdout, stderr)
}

/// softIocPVX 1.5.3 with `PVXS_QSRV_ENABLE=NO` on the same db, `pvxsr 1`.
#[test]
fn both_qsrv_sources_serve_with_qsrv2_off() {
    let (stdout, stderr) = run_disabled(b"pvxsr 1\nexit\n");
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|l| l.starts_with("Source: ") || l.starts_with("    "))
        .collect();
    assert_eq!(
        lines,
        [
            "Source: __builtin prio=-1 StaticProvider",
            "Source: __server prio=-1 Source",
            "Source: qsrvSingle prio=0 IOC",
            "    LNK",
            "    XA",
            "    XC",
            "    ZB",
            "Source: qsrvGroup prio=1 IOC",
            "    GRPA",
            "    GRPB",
        ],
        "stdout: {stdout}\nstderr: {stderr}"
    );
}

/// The commands the decision gates are absent, the st.cmd's `dbLoadGroup`
/// included, as softIocPVX answers them.
#[test]
fn qsrv2_off_registers_none_of_the_gated_commands() {
    let (stdout, stderr) = run_disabled(b"pvxsl\npvxgl\ndbpvar LNK\nvar pvaLinkNWorkers\nexit\n");
    for want in [
        "Command 'dbLoadGroup' not registered.",
        "Command 'pvxsl' not registered.",
        "Command 'pvxgl' not registered.",
        "Command 'dbpvar' not registered.",
        "No known vars match 'pvaLinkNWorkers'.",
    ] {
        assert!(
            stderr.contains(want),
            "`{want}` missing\nstdout: {stdout}\nstderr: {stderr}"
        );
    }
}

/// softIocPVX reads 5 NO_ALARM through `{pva:"XA"}` with QSRV2 off.
#[test]
fn a_pva_link_resolves_with_qsrv2_off() {
    let (stdout, stderr) = run_disabled(b"epicsThreadSleep 4\ndbgf LNK\ndbgf LNK.SEVR\nexit\n");
    let got: Vec<&str> = stdout
        .lines()
        .filter_map(|l| l.split_once("DBF_").map(|(_, v)| v))
        .map(str::trim_end)
        .collect();
    assert_eq!(
        got,
        ["DOUBLE:         5", "STRING:         \"NO_ALARM\""],
        "stdout: {stdout}\nstderr: {stderr}"
    );
}
