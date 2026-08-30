//! **`dbtgf`'s six limit lines are numbers, never `nan`.**
//!
//! Each of them is a plain `double` field of the C record — `LOLO`, `LOW`,
//! `HIGH`, `HIHI`, `LOPR`, `HOPR`, `DRVL`, `DRVH` — read straight into the
//! option block by `getOptions` (`dbAccess.c:249-311`,
//! R7.0.10-146-g8f5015b663d764ad75df) and 0.0 until something sets it. There
//! is no encoding in C for "the record states no limit".
//!
//! This port's `Snapshot` has one — NaN — and it was reaching the renderer.
//! Measured against `~/work/epics-base/bin/linux-x86_64/softIoc` on a record
//! with no alarm limits, C prints
//!
//! ```text
//! alLong: 0 < 0 .. 0 < 0
//! alDouble: 0 < 0 .. 0 < 0
//! ```
//!
//! and this port printed `alDouble: nan < nan .. nan < nan`. The long line was
//! already right only because C's own `finite(x) ? (epicsInt32)x : 0`
//! (`dbAccess.c:303-310`) had been ported onto it; the rule belongs ahead of
//! both renderers, not on the arm that happened to be measured.
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

/// `HOPR`/`LOPR` are set so the graphic and control lines carry real numbers;
/// no alarm limit is, so those four fields are the ones with no value.
const DB: &str = "\
record(ai, \"T:AI\") {
    field(DESC, \"an analog in\")
    field(EGU, \"V\")
    field(PREC, \"3\")
    field(HOPR, \"10\")
    field(LOPR, \"0\")
    field(VAL, \"1.5\")
}
";

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('\x1b') {
        out.push_str(&rest[..at]);
        rest = match rest[at..].find('m') {
            Some(end) => &rest[at + end + 1..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn answer_to(line: &str) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("t.db"), DB).expect("write the .db");
    let script = dir.path().join("t.cmd");
    std::fs::write(
        &script,
        format!("dbLoadRecords(\"t.db\")\niocInit\n{line}\n"),
    )
    .expect("write the script");

    let sink = tempfile::NamedTempFile::new().expect("stdout capture");
    // SAFETY: the shell writes through fd 1 for the length of one script and
    // this test is `serial`, so nothing else holds it while it is swapped.
    let saved = unsafe {
        std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path());
        let saved = libc::dup(1);
        libc::dup2(sink.as_file().as_raw_fd(), 1);
        saved
    };

    let db = Arc::new(PvDatabase::new());
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let path = script.display().to_string();
    let ran = std::thread::spawn(move || IocShell::new(db, bridge).execute_script(&path)).join();

    let _ = std::io::stdout().flush();
    // SAFETY: restoring the descriptor this call replaced.
    unsafe {
        libc::dup2(saved, 1);
        libc::close(saved);
        std::env::remove_var("EPICS_DB_INCLUDE_PATH");
    }
    assert!(ran.is_ok(), "the shell panicked running {line}");

    let text = strip_ansi(&std::fs::read_to_string(sink.path()).expect("stdout capture"));
    text.split_once(&format!("{line}\n"))
        .map(|(_, after)| after.to_string())
        .unwrap_or_else(|| panic!("the shell never echoed {line}; got:\n{text}"))
}

/// `DESC` is a `DBF_STRING`, so no limit reaches the block from the record
/// and all six lines are C's zeros — the whole option block, byte for byte.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn an_unset_limit_prints_zero_on_both_lines() {
    let out = answer_to("dbtgf T:AI.DESC");
    assert!(
        out.starts_with(
            "status = 17, severity = 0\n\
             units = \"\"\n\
             precision not returned\n\
             time = <undefined>\n\
             enum strings not returned\n\
             grLong: 0 .. 0\n\
             grDouble: 0 .. 0\n\
             ctrlLong: 0 .. 0\n\
             ctrlDouble: 0 .. 0\n\
             alLong: 0 < 0 .. 0 < 0\n\
             alDouble: 0 < 0 .. 0 < 0\n"
        ),
        "option block was:\n{out}"
    );
}

/// The control: a limit the record DOES state still prints its own value, so
/// the rule cannot be read as "zero every double".
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_stated_limit_still_prints_itself() {
    let out = answer_to("dbtgf T:AI.VAL");
    assert!(
        out.contains("\ngrDouble: 0 .. 10\nctrlLong: 0 .. 10\nctrlDouble: 0 .. 10\n"),
        "option block was:\n{out}"
    );
    assert!(!out.contains("nan"), "a limit printed nan:\n{out}");
}
