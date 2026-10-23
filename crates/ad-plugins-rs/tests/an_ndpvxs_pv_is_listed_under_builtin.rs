//! `NDPvxsConfigure`'s PV joins the pvxs server's `__builtin` store
//! (`NDPluginPvxs.cpp:41-56`, `ioc::server().addPV`), so `pvxsr 1` lists it
//! there (`sharedpv.cpp:538-545`) and under no other source, before
//! `iocInit` as after; a name the store holds is refused as pvxs and
//! pvDatabase refuse it, and with no pvxs server `addPV` throws.
//!
//! The IOC ends in the iocsh REPL, so it runs in a child (this binary re-run
//! on [`ioc_child`]) whose piped stdin types the command.
#![cfg(all(feature = "ioc", feature = "pva", unix))]

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ad_core_rs::ioc::GenericDriverContext;
use ad_core_rs::ndarray_pool::DriverPool;
use ad_core_rs::plugin::channel::NDArrayOutput;

const CHILD: &str = "NDPVXS_BUILTIN_CHILD_STCMD";
/// Set: the child runs CA only, with no pvxs server registered.
const CA_ONLY: &str = "NDPVXS_BUILTIN_CHILD_CA_ONLY";

/// The child half; a no-op unless the parent set [`CHILD`].
#[test]
fn ioc_child() {
    let Ok(stcmd) = std::env::var(CHILD) else {
        return;
    };
    let ioc = ad_plugins_rs::ioc::AdIoc::new();
    ioc.mgr().set_driver(Arc::new(GenericDriverContext::new(
        DriverPool::standalone(4_000_000),
        Arc::new(parking_lot::Mutex::new(NDArrayOutput::new())),
        "DET",
        ioc.mgr().wiring(),
    )));
    let run = if std::env::var_os(CA_ONLY).is_some() {
        epics_base_rs::runtime::task::test_block_on(ioc.run(&stcmd))
    } else {
        epics_base_rs::runtime::task::test_block_on(ioc.run_with_pva(&stcmd))
    };
    run.expect("the IOC runs");
}

/// stdout and stderr of an IOC on `script`, typed `input`.
fn run_ioc(script: &str, input: &[u8], ca_only: bool) -> (String, String) {
    let dir = tempfile::tempdir().unwrap();
    let stcmd = dir.path().join("st.cmd");
    std::fs::write(&stcmd, script).unwrap();
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    if ca_only {
        cmd.env(CA_ONLY, "1");
    }
    let mut child = cmd
        .args(["ioc_child", "--exact", "--nocapture"])
        .env(CHILD, &stcmd)
        .env("EPICS_CAS_SERVER_PORT", "0")
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_PVAS_SERVER_PORT", "0")
        .env("EPICS_PVAS_BROADCAST_PORT", "0")
        .env("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_PVAS_AUTO_BEACON_ADDR_LIST", "NO")
        .env("EPICS_PVAS_BEACON_ADDR_LIST", "")
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
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The `pvxsr 1` source and PV lines of `stdout`.
fn report_lines(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter(|l| l.starts_with("Source: ") || l.starts_with("    "))
        .collect()
}

#[test]
fn pvxsr_1_lists_the_ndpvxs_pv_under_builtin() {
    let (stdout, stderr) = run_ioc(
        "NDPvxsConfigure(\"PVXS1\", 20, 0, \"DET\", 0, \"TEST:Pvxs\")\niocInit\n",
        b"pvxsr 1\nexit\n",
        false,
    );
    let lines = report_lines(&stdout);
    let context = format!("stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(
        lines.get(..3),
        Some(
            &[
                "Source: __builtin prio=-1 StaticProvider",
                "    TEST:Pvxs",
                "Source: __server prio=-1 Source",
            ][..]
        ),
        "{context}"
    );
    assert_eq!(
        lines.iter().filter(|l| l.trim() == "TEST:Pvxs").count(),
        1,
        "{context}"
    );
}

/// A second `NDPvxsConfigure` of a name throws pvxs's refusal
/// (`sharedpv.cpp:587-588`) and a second `NDPvaConfigure` pvDatabase's
/// (`NDPluginPva.cpp:176-177`); `pvxsr 1` before `iocInit` lists the store
/// of the stopped server (`server.cpp:290-305`).
#[test]
fn a_taken_name_is_refused_and_pvxsr_lists_the_store_before_iocinit() {
    let (stdout, stderr) = run_ioc(
        "NDPvxsConfigure(\"PVXS1\", 20, 0, \"DET\", 0, \"TEST:Pvxs\")\n\
         NDPvxsConfigure(\"PVXS2\", 20, 0, \"DET\", 0, \"TEST:Pvxs\")\n\
         NDPvaConfigure(\"PVA1\", 20, 0, \"DET\", 0, \"TEST:Pva\")\n\
         NDPvaConfigure(\"PVA2\", 20, 0, \"DET\", 0, \"TEST:Pva\")\n\
         pvxsr 1\n\
         iocInit\n",
        b"exit\n",
        false,
    );
    let context = format!("stdout: {stdout}\nstderr: {stderr}");
    for want in [
        "C++ error: add() will not create duplicate PV",
        "C++ error: couldn't add record to master database",
    ] {
        assert_eq!(stderr.matches(want).count(), 1, "`{want}`\n{context}");
    }
    assert_eq!(
        report_lines(&stdout),
        [
            "Source: __builtin prio=-1 StaticProvider",
            "    TEST:Pva",
            "    TEST:Pvxs",
            "Source: __server prio=-1 Source",
        ],
        "{context}"
    );
}

/// With no pvxs server `ioc::server()` throws (`iochooks.cpp:80-87`), so
/// `NDPvxsConfigure` fails; `NDPvaConfigure` adds to pvDatabase's master,
/// which needs none.
#[test]
fn ndpvxs_configure_with_no_pvxs_server_throws_no_instance() {
    let (stdout, stderr) = run_ioc(
        "NDPvxsConfigure(\"PVXS1\", 20, 0, \"DET\", 0, \"TEST:Pvxs\")\n\
         NDPvaConfigure(\"PVA1\", 20, 0, \"DET\", 0, \"TEST:Pva\")\n\
         iocInit\n",
        b"exit\n",
        true,
    );
    let context = format!("stdout: {stdout}\nstderr: {stderr}");
    assert!(stderr.contains("C++ error: No Instance"), "{context}");
    assert_eq!(stderr.matches("C++ error").count(), 1, "{context}");
}
