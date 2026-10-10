//! qsrv-rs on a `--group-file` that cannot be loaded, against softIocPVX
//! 1.5.3 given the same file through `dbLoadGroup`:
//!
//! ```text
//! malformed  exit 134 (SIGABRT), stderr
//!              Starting iocInit
//!              terminate called after throwing an instance of 'std::runtime_error'
//!                what():  Error reading group definition file "bad.json"
//!              Error while completing parsing
//! missing    Error opening "nosuch.json", then iocInit completes and serves
//! ```
//!
//! The last malformed line is the JSON parser's own message, so the lines
//! before it are what is pinned.
#![cfg(all(feature = "qsrv-bin", unix))]

use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// softIocPVX's last boot line, on stderr.
const READY: &str = "iocRun: All initialization complete\n";

fn qsrv_rs(group_file: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_qsrv-rs"));
    cmd.arg("--port")
        .arg("0")
        .arg("--group-file")
        .arg(group_file)
        .env("EPICS_PVAS_BROADCAST_PORT", "0");
    cmd
}

#[test]
fn a_malformed_group_file_aborts_with_pvxs_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.json");
    std::fs::write(&path, "{\"G\":{\"a\":{\"+channel\":\"A.VAL\"}}\n").unwrap();

    // One file for both streams, so the lines land in write order.
    let log = dir.path().join("out");
    let out = std::fs::File::create(&log).unwrap();
    let mut child = qsrv_rs(&path)
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .expect("spawn qsrv-rs");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("qsrv-rs kept serving a malformed group file");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.signal(), Some(6), "{status:?}");
    let output = std::fs::read_to_string(&log).unwrap();
    let framing = format!(
        "terminate called after throwing an instance of 'std::runtime_error'\n  \
         what():  Error reading group definition file \"{}\"\n",
        path.display()
    );
    let at = output.find(&framing).unwrap_or_else(|| panic!("{output}"));
    let parser_line = &output[at + framing.len()..];
    assert_eq!(parser_line.lines().count(), 1, "{output}");
    assert!(output[..at].contains("Starting iocInit\n"), "{output}");
    assert!(
        !output[..at].contains("iocRun"),
        "scans started before the group file was read: {output}"
    );
}

#[test]
fn a_missing_group_file_says_so_before_iocrun_and_keeps_serving() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nosuch.json");
    let log = dir.path().join("out");
    let out = std::fs::File::create(&log).unwrap();
    let mut child = qsrv_rs(&missing)
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .expect("spawn qsrv-rs");

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let output = loop {
        let output = std::fs::read_to_string(&log).unwrap();
        if output.contains(READY) || std::time::Instant::now() > deadline {
            break output;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let alive = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        output.contains(READY) && alive,
        "qsrv-rs stopped on a missing file: {output}"
    );
    let line = format!("Error opening \"{}\"\n", missing.display());
    let at = output.find(&line).unwrap_or_else(|| panic!("{output}"));
    assert!(!output[..at].contains("iocRun"), "{output}");
    assert!(
        output[at..].contains("iocRun: All initialization complete\n"),
        "{output}"
    );
}

/// softIocPVX 1.5.3 on this db and group file prints these lines, in this
/// order: `createGroups`' first pass for every group, then its second.
#[test]
fn refused_groups_print_what_softiocpvx_prints() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("h.db");
    std::fs::write(&db, "record(ai, \"A\") {}\nrecord(calcout, \"C\") {}\n").unwrap();
    let groups = dir.path().join("h.json");
    std::fs::write(
        &groups,
        r#"{"H1":{"a":{"+channel":"A.VAL"},"v":{"+channel":"NOPE.VAL"}},
 "H2":{"l":{"+channel":"C.INPA"},"z":{"+channel":"NOPE.VAL"}},
 "H3":{"l":{"+channel":"C.INPA"}}}"#,
    )
    .unwrap();
    let log = dir.path().join("out");
    let out = std::fs::File::create(&log).unwrap();
    let mut child = qsrv_rs(&groups)
        .arg("--db-file")
        .arg(&db)
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .expect("spawn qsrv-rs");

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let output = loop {
        let output = std::fs::read_to_string(&log).unwrap();
        if output.contains(READY) || std::time::Instant::now() > deadline {
            break output;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = child.kill();
    let _ = child.wait();
    let refused: Vec<&str> = output
        .lines()
        .filter(|l| l.contains("Error Group not created"))
        .collect();
    assert_eq!(
        refused,
        [
            "H1: Error Group not created: Invalid PV: NOPE.VAL",
            "H2: Error Group not created: Invalid PV: NOPE.VAL",
            "H1: Error Group not created: field not found in group: \"v\"",
            "H2: Error Group not created: field not found in group: \"z\"",
            "H3: Error Group not created: Link fields not allowed in this context",
        ],
        "{output}"
    );
}

/// softIocPVX 1.5.3's stderr on the db and group file above, line for line
/// with each WARN's timestamp elided: `Starting iocInit`, then
/// `resolveTriggerReferences`' WARN for every group in name order, then
/// `createGroups`' two passes, then `iocRun`.
#[test]
fn startup_stderr_is_what_softiocpvx_prints() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("h.db");
    std::fs::write(&db, "record(ai, \"A\") {}\nrecord(calcout, \"C\") {}\n").unwrap();
    let groups = dir.path().join("h.json");
    std::fs::write(
        &groups,
        r#"{"H1":{"a":{"+channel":"A.VAL"},"v":{"+channel":"NOPE.VAL"}},
 "H2":{"l":{"+channel":"C.INPA"},"z":{"+channel":"NOPE.VAL"}},
 "H3":{"l":{"+channel":"C.INPA"}}}"#,
    )
    .unwrap();
    let log = dir.path().join("err");
    let mut child = qsrv_rs(&groups)
        .arg("--db-file")
        .arg(&db)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .expect("spawn qsrv-rs");

    let last = READY;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let output = loop {
        let output = std::fs::read_to_string(&log).unwrap();
        if output.contains(last) || std::time::Instant::now() > deadline {
            break output;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = child.kill();
    let _ = child.wait();
    let end = output.find(last).unwrap_or_else(|| panic!("{output}")) + last.len();
    // `YYYY-MM-DDTHH:MM:SS.nnnnnnnnn WARN …` → `<ts> WARN …`.
    let elided: Vec<String> = output[..end]
        .lines()
        .map(|l| match l.split_once(' ') {
            Some((ts, rest))
                if ts.len() == 29
                    && ts.as_bytes()[10] == b'T'
                    && ts[20..].bytes().all(|b| b.is_ascii_digit()) =>
            {
                format!("<ts> {rest}")
            }
            _ => l.to_string(),
        })
        .collect();
    let warn = |g: &str| {
        [
            "<ts> WARN pvxs.ioc.group.processor".to_string(),
            format!(
                "     Group {g} defines no +trigger mappings.  \
                 Default to individual/split monitor updates."
            ),
        ]
    };
    let mut want = vec!["Starting iocInit".to_string()];
    for g in ["H1", "H2", "H3"] {
        want.extend(warn(g));
    }
    want.extend(
        [
            "H1: Error Group not created: Invalid PV: NOPE.VAL",
            "H2: Error Group not created: Invalid PV: NOPE.VAL",
            "H1: Error Group not created: field not found in group: \"v\"",
            "H2: Error Group not created: field not found in group: \"z\"",
            "H3: Error Group not created: Link fields not allowed in this context",
            "iocRun: All initialization complete",
        ]
        .map(String::from),
    );
    assert_eq!(elided, want, "{output}");
}

/// softIocPVX 1.5.3's stdout up to the banner's closing rule, with the
/// tree's own revision and date masked:
///
/// ```text
/// INFO: PVXS QSRV2 is loaded, permitted, and ENABLED.
/// ############################################################################
/// ## EPICS R7.0.10.1-DEV
/// ## Rev. R7.0.10-146-g8f5015b663d764ad75df
/// ## Rev. Date Git: 2026-08-12 07:48:22 +0900
/// ############################################################################
/// ```
#[test]
fn stdout_carries_the_corerelease_banner() {
    let dir = tempfile::tempdir().unwrap();
    let groups = dir.path().join("g.json");
    std::fs::write(&groups, "{}").unwrap();
    let mut child = qsrv_rs(&groups)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn qsrv-rs");
    let rule = "#".repeat(76);
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    let closing = rule.clone();
    std::thread::spawn(move || {
        let mut lines = Vec::new();
        let mut rules = 0;
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            rules += usize::from(line == closing);
            lines.push(line);
            if rules == 2 || lines.len() == 8 {
                break;
            }
        }
        let _ = tx.send(lines);
    });
    let lines = rx.recv_timeout(Duration::from_secs(20));
    let _ = child.kill();
    let _ = child.wait();
    let lines = lines.expect("qsrv-rs printed no banner");
    let masked: Vec<String> = lines
        .iter()
        .map(|l| {
            if l.starts_with("## Rev. Date ") {
                "## Rev. Date <date>".to_string()
            } else if l.starts_with("## Rev. ") {
                "## Rev. <rev>".to_string()
            } else {
                l.clone()
            }
        })
        .collect();
    assert_eq!(
        masked,
        [
            "INFO: PVXS QSRV2 is loaded, permitted, and ENABLED.",
            &rule,
            "## EPICS R7.0.10.1-DEV",
            "## Rev. <rev>",
            "## Rev. Date <date>",
            &rule,
        ],
        "{lines:?}"
    );
}

/// softIocPVX 1.5.3's console, stdout and stderr on one unbuffered stream:
/// `enable2()` prints the QSRV2 INFO line from the registrar, before the
/// st.cmd, so it comes ahead of `Starting iocInit` and the banner. The
/// banner's `## ` lines are elided (`core_release_block` owns their text).
#[test]
fn qsrv2_info_precedes_iocinit_as_softiocpvx_prints() {
    let dir = tempfile::tempdir().unwrap();
    let groups = dir.path().join("g.json");
    std::fs::write(&groups, "{}").unwrap();
    let log = dir.path().join("console");
    let file = std::fs::File::create(&log).unwrap();
    let mut child = qsrv_rs(&groups)
        .stdout(file.try_clone().unwrap())
        .stderr(file)
        .spawn()
        .expect("spawn qsrv-rs");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let output = loop {
        let output = std::fs::read_to_string(&log).unwrap();
        if output.contains(READY) || std::time::Instant::now() > deadline {
            break output;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = child.kill();
    let _ = child.wait();
    let end = output.find(READY).unwrap_or_else(|| panic!("{output}")) + READY.len();
    let lines: Vec<&str> = output[..end]
        .lines()
        .filter(|l| !l.starts_with("## "))
        .collect();
    let rule = "#".repeat(76);
    assert_eq!(
        lines,
        [
            "INFO: PVXS QSRV2 is loaded, permitted, and ENABLED.",
            "Starting iocInit",
            &rule,
            &rule,
            "iocRun: All initialization complete",
        ],
        "{output}"
    );
}

/// softIocPVX 1.5.3 prints nothing between `iocRun: All initialization
/// complete` and its exit on SIGTERM.
#[test]
fn sigterm_prints_nothing_after_iocrun() {
    let dir = tempfile::tempdir().unwrap();
    let groups = dir.path().join("g.json");
    std::fs::write(&groups, "{}").unwrap();
    let log = dir.path().join("console");
    let file = std::fs::File::create(&log).unwrap();
    let mut child = qsrv_rs(&groups)
        .stdout(file.try_clone().unwrap())
        .stderr(file)
        .spawn()
        .expect("spawn qsrv-rs");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !std::fs::read_to_string(&log).unwrap().contains(READY) {
        assert!(
            std::time::Instant::now() < deadline,
            "qsrv-rs never reached iocRun"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let killed = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("run kill");
    assert!(killed.success());
    let _ = child.wait();
    let output = std::fs::read_to_string(&log).unwrap();
    let tail = &output[output.find(READY).unwrap() + READY.len()..];
    assert_eq!(tail, "", "{output}");
}
