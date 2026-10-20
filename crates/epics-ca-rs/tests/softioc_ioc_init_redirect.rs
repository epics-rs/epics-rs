//! `iocInit > f` puts the build's stdout in `f`.
//!
//! `coreRelease` and `asDbLib` write through epicsStdio, so the redirect
//! swaps where they land; `Starting iocInit` and the asInit failure are
//! errlog and stay on the console. Measured on softIoc R7.0.10.

#![cfg(all(unix, tokio_backend))]

use std::process::{Command, Stdio};

/// softioc-rs on `script` with stdin at EOF: the redirect target's text,
/// and the process's stdout. A port taken before the IOC bound it is drawn
/// again.
fn run(script: &str, target: &str) -> (String, String) {
    named_port::on_a_named_port(|port| run_on(script, target, port))
}

fn run_on(script: &str, target: &str, port: u16) -> Option<(String, String)> {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.db"), "record(ai, \"R1\") {\n}\n").unwrap();
    std::fs::write(dir.path().join("st.cmd"), script).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .current_dir(dir.path())
        .arg("st.cmd")
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CA_SERVER_PORT", port.to_string())
        .env(
            "EPICS_CA_REPEATER_PORT",
            named_port::free_for_tcp_and_udp().to_string(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("softioc-rs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    (!named_port::verdict::rs_ioc_port_was_taken(&stderr, port)).then(|| {
        (
            std::fs::read_to_string(dir.path().join(target)).unwrap_or_default(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    })
}

/// The `#####` version banner, VCS stamp aside.
fn banner_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|l| l.starts_with("##"))
        .filter(|l| !l.starts_with("## Rev."))
        .collect()
}

#[test]
fn the_banner_goes_to_the_redirect() {
    let (file, stdout) = run("dbLoadRecords a.db\niocInit > init.txt\n", "init.txt");
    assert_eq!(banner_lines(&file).len(), 3, "{file:?}");
    assert!(banner_lines(&stdout).is_empty(), "{stdout:?}");
}

#[test]
fn the_as_db_lib_line_goes_to_the_redirect() {
    let (file, stdout) = run(
        "dbLoadRecords a.db\nasInit\niocInit > init.txt\n",
        "init.txt",
    );
    assert!(
        file.ends_with(
            "Access security is NOT enabled. Was asSetFilename specified before iocInit?\n"
        ),
        "{file:?}"
    );
    assert!(!stdout.contains("Access security"), "{stdout:?}");
}

/// The lines `afterIocRunning` queued run inside `iocInit`, on its thread,
/// so what they print lands in its redirect; the hook's own echo is a plain
/// `printf` and stays on the console (`afterIocRunning.c:47-53`).
#[test]
fn the_queued_lines_print_into_the_redirect() {
    let (file, stdout) = run(
        "dbLoadRecords a.db\nafterIocRunning \"echo QUEUED\"\niocInit > init.txt\n",
        "init.txt",
    );
    let lines: Vec<&str> = file.lines().filter(|l| !l.starts_with("##")).collect();
    assert_eq!(lines, ["QUEUED"], "{file:?}");
    assert!(
        stdout
            .lines()
            .any(|l| l.contains("afterIocRunning:") && l.ends_with(" echo QUEUED")),
        "{stdout:?}"
    );
    assert!(!stdout.lines().any(|l| l == "QUEUED"), "{stdout:?}");
}

#[test]
fn a_queued_lines_diagnostic_goes_to_the_stderr_redirect() {
    let (file, stdout) = run(
        "dbLoadRecords a.db\nafterIocRunning \"nosuchcmd\"\niocInit 2> err.txt\n",
        "err.txt",
    );
    assert!(
        file.contains("Command 'nosuchcmd' not registered."),
        "{file:?}"
    );
    assert!(
        stdout.contains("afterIocRunning: command 'nosuchcmd' failed to run"),
        "{stdout:?}"
    );
}
