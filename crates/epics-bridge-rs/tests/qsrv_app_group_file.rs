//! `run_ca_pva_qsrv_ioc_app` over an st.cmd whose `dbLoadGroup` names a file
//! that does not parse, against softIocPVX 1.5.3 given the same script:
//!
//! ```text
//! exit 134 (SIGABRT), stdout (line-buffered) after the command's echo
//!   ##########…  (coreRelease, as `iocInit` runs; nothing from dbLoadGroup)
//! stderr
//!   Starting iocInit
//!   terminate called after throwing an instance of 'std::runtime_error'
//!     what():  Error reading group definition file "bad.json"
//!   Error while completing parsing
//! ```
//!
//! pvxs throws from `processGroups` at `initHookAfterInitDatabase`, inside
//! `iocBuild`, so no scan or PINI has run and `iocRun` never prints.
//!
//! The abort takes the process with it, so the IOC runs in a child: this test
//! binary re-run on [`ioc_child`] with `QSRV_APP_CHILD_STCMD` set.
#![cfg(all(feature = "qsrv", unix))]

use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::time::Duration;

const CHILD: &str = "QSRV_APP_CHILD_STCMD";

/// The child half; a no-op unless the parent set [`CHILD`].
#[test]
fn ioc_child() {
    let Ok(stcmd) = std::env::var(CHILD) else {
        return;
    };
    let _ = epics_base_rs::runtime::task::test_block_on(
        epics_bridge_rs::qsrv::run_ca_pva_qsrv_ioc_app(
            epics_base_rs::server::ioc_app::IocApplication::new()
                .port(0)
                .startup_script(&stcmd),
        ),
    );
}

#[test]
fn a_malformed_group_file_aborts_inside_iocbuild_with_pvxs_text() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "{\"G\":{\"a\":{\"+channel\":\"A.VAL\"}}\n").unwrap();
    let stcmd = dir.path().join("st.cmd");
    std::fs::write(&stcmd, format!("dbLoadGroup(\"{}\")\n", bad.display())).unwrap();

    let log = dir.path().join("stderr");
    let out = std::fs::File::create(&log).unwrap();
    let stdout_log = dir.path().join("stdout");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["ioc_child", "--exact", "--nocapture"])
        .env(CHILD, &stcmd)
        .env("EPICS_PVAS_SERVER_PORT", "0")
        .env("EPICS_PVAS_BROADCAST_PORT", "0")
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(&stdout_log).unwrap())
        .stderr(out)
        .spawn()
        .expect("spawn the IOC child");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the IOC kept running on a malformed group file");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let output = std::fs::read_to_string(&log).unwrap();
    let stdout = std::fs::read_to_string(&stdout_log).unwrap();
    assert_eq!(status.signal(), Some(6), "{status:?}\n{output}");
    let framing = format!(
        "terminate called after throwing an instance of 'std::runtime_error'\n  \
         what():  Error reading group definition file \"{}\"\n",
        bad.display()
    );
    let at = output.find(&framing).unwrap_or_else(|| panic!("{output}"));
    assert_eq!(output[at + framing.len()..].lines().count(), 1, "{output}");
    assert_eq!(&output[..at], "Starting iocInit\n", "{output}");
    // softIocPVX (stdout line-buffered) prints nothing for the command
    // itself: the echo is followed by the next command's echo, then
    // coreRelease's rule.
    let echo = format!("\x1b[1mdbLoadGroup(\"{}\")\x1b[0m\n", bad.display());
    let after = stdout.find(&echo).unwrap_or_else(|| panic!("{stdout}")) + echo.len();
    assert!(
        stdout[after..].starts_with(&format!("{}\n", "#".repeat(76))),
        "{stdout}"
    );
}
