//! The boot of `softioc-rs -d <db>` puts on each stream what `softIoc` does.
//!
//! Measured on the compiled `softIoc` (R7.0.10) with the same one-record
//! database and `iocInit`, `exit` on stdin, ports free: stdout is the `#####` version
//! banner and the prompts; stderr is `Starting iocInit`, `iocRun: All
//! initialization complete`, and the second `iocInit`'s `iocBuild: ERROR`.
//! C prints no record-count line and no `CA server:` line when the port is
//! configured, so neither may appear on either stream. The banner is C's
//! with each build's own VCS stamp.

#![cfg(unix)]

use std::io::Write;
use std::process::{Command, Stdio};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

/// Replace the describe and the date after its `Git:` / `build date/time:`
/// label, the two arms `genVersionHeader.pl` has.
fn mask_vcs_stamp(lines: &[&str]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            if let Some(date) = l.strip_prefix("## Rev. Date ") {
                let arm = ["Git: ", "build date/time: "]
                    .into_iter()
                    .find(|arm| date.starts_with(arm) && date.len() > arm.len());
                format!("## Rev. Date {}", arm.map_or("?", |_| "<vcs date>"))
            } else if let Some(rev) = l.strip_prefix("## Rev. ") {
                format!(
                    "## Rev. {}",
                    if rev.is_empty() { "?" } else { "<describe>" }
                )
            } else {
                l.to_string()
            }
        })
        .collect()
}

#[test]
fn stdout_carries_the_banner_and_stderr_only_cs_boot_lines() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("a.db");
    std::fs::write(&db, "record(ai, \"R1\") {\n}\n").expect("write the .db");
    let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .arg("-d")
        .arg(&db)
        .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CA_SERVER_PORT", free_port().to_string())
        .env("EPICS_CA_REPEATER_PORT", free_port().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn softioc-rs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"iocInit\nexit\n")
        .unwrap();
    let out = child.wait_with_output().expect("softioc-rs output");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    // softIoc 8f5015b66's banner, its VCS stamp masked: each build carries its
    // own tree's describe and date, by the same genVersionHeader rule.
    let c_banner = "\
############################################################################
## EPICS R7.0.10.1-DEV
## Rev. R7.0.10-146-g8f5015b663d764ad75df
## Rev. Date Git: 2026-08-12 07:48:22 +0900
############################################################################";
    let rs_banner: Vec<&str> = stdout.lines().take(5).collect();
    assert_eq!(
        mask_vcs_stamp(&rs_banner),
        mask_vcs_stamp(&c_banner.lines().collect::<Vec<_>>()),
        "stdout: {stdout:?}"
    );
    for line in ["Starting iocInit", "iocRun:", "records,", "CA server:"] {
        assert!(!stdout.contains(line), "{line:?} on stdout: {stdout:?}");
    }
    assert_eq!(
        stderr,
        "Starting iocInit\n\
         iocRun: All initialization complete\n\
         iocBuild: ERROR IOC can only be initialized from uninitialized or stopped state\n"
    );
}
