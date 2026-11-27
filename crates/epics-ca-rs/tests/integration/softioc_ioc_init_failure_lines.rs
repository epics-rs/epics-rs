//! softMain's own `iocInit()` and its `ERROR during iocInit()` line.
//!
//! C calls `iocInit()` after the script only on `-d`/`-x` (`softMain.cpp:239`),
//! and prints the line only for that call; a script's failed `iocInit` prints
//! none. When the script already took the IOC past `iocVoid`, `iocBuild_1`
//! refuses softMain's call (`iocInit.c:118-120`). Measured on softIoc
//! R7.0.10, stdin `exit`; the refusal is errlog, the other line `std::cerr`,
//! so their order is the errlog thread's and only the lines are compared.

#![cfg(all(unix, tokio_backend))]

use std::io::Write;
use std::process::{Command, Stdio};

const AS_FAILED: [&str; 4] = [
    "Starting iocInit",
    "\x1b[31;1mERROR\x1b[0m asInitFile: Can't open file '/no/such/x.acf'",
    "ERROR iocBuild: asInit Failed.",
    " The IOC has not been started.",
];
const DURING: &str = "\x1b[31;1mERROR\x1b[0m during iocInit()";
const REFUSED: &str =
    "iocBuild: ERROR IOC can only be initialized from uninitialized or stopped state";

/// softioc-rs `args` then `script`, stdin `exit`: its stderr lines, sorted.
/// A port taken before the IOC bound it is drawn again.
fn stderr_lines(args: &[&str], script: &str) -> Vec<String> {
    named_port::on_a_named_port(|port| stderr_lines_on(args, script, port))
}

fn stderr_lines_on(args: &[&str], script: &str, port: u16) -> Option<Vec<String>> {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.db"), "record(ai, \"R1\") {\n}\n").unwrap();
    std::fs::write(dir.path().join("st.cmd"), script).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .current_dir(dir.path())
        .args(args)
        .arg("st.cmd")
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CA_SERVER_PORT", port.to_string())
        .env(
            "EPICS_CA_REPEATER_PORT",
            named_port::free_for_tcp_and_udp().to_string(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn softioc-rs");
    child.stdin.take().unwrap().write_all(b"exit\n").unwrap();
    let out = child.wait_with_output().expect("softioc-rs output");
    let stderr = String::from_utf8_lossy(&out.stderr);
    if named_port::verdict::rs_ioc_port_was_taken(&stderr, port) {
        return None;
    }
    let mut lines: Vec<String> = stderr.lines().map(str::to_owned).collect();
    lines.sort();
    Some(lines)
}

fn sorted(lines: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    v.sort();
    v
}

const FAIL: &str = "asSetFilename(\"/no/such/x.acf\")\niocInit\n";

#[test]
fn a_script_s_failed_ioc_init_prints_no_soft_main_line() {
    assert_eq!(stderr_lines(&[], FAIL), sorted(&AS_FAILED));
}

#[test]
fn soft_main_s_failed_ioc_init_prints_its_line() {
    let mut want = AS_FAILED.to_vec();
    want.push(DURING);
    assert_eq!(
        stderr_lines(&["-d", "a.db"], "asSetFilename(\"/no/such/x.acf\")\n"),
        sorted(&want)
    );
}

#[test]
fn soft_main_s_ioc_init_after_a_failed_script_one_is_refused() {
    let mut want = AS_FAILED.to_vec();
    want.extend([REFUSED, DURING]);
    assert_eq!(stderr_lines(&["-d", "a.db"], FAIL), sorted(&want));
}

#[test]
fn soft_main_s_ioc_init_after_a_running_script_one_is_refused() {
    assert_eq!(
        stderr_lines(&["-d", "a.db"], "iocInit\n"),
        sorted(&[
            "Starting iocInit",
            "iocRun: All initialization complete",
            REFUSED,
            DURING,
        ])
    );
}
