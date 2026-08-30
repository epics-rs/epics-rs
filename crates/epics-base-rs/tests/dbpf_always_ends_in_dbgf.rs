//! **`dbpf` prints no diagnostic of its own, and always ends in `dbgf`.**
//!
//! C's whole tail is three statements (`dbTest.c:431-434`,
//! R7.0.10-146-g8f5015b663d764ad75df):
//!
//! ```c
//!     status = dbPutField(&addr, dbrType, pvalue, n);
//!     free(array);
//!     dbgf(pname);
//!     return status;
//! ```
//!
//! so the read-back runs whatever the put returned, and every word an operator
//! sees comes from INSIDE the put — `recGblDbaddrError`, a record's own
//! `special()` — never from `dbpf`. C also never parses the scalar text: it
//! hands it to `dbPutField` as `DBR_STRING` and lets the conversion fail there.
//!
//! The port broke that on both sides. Text no `EpicsValue::parse` took raised
//! `ERROR <file> line <n>: cannot parse 'notanumber' as Double` and returned
//! before the read-back; and the read-back itself was an inline copy of only
//! `dbgf`'s successful half, so a put whose read then failed printed nothing.
//! Measured against `~/work/epics-base/bin/linux-x86_64/softIoc` on the
//! fixture below, C printed `DBF_DOUBLE:         1.5` and
//! `DBF_CHAR:           failed.` for those two.
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

const DB: &str = "record(ai, \"T:AI\") {\n    field(PREC, \"3\")\n    field(VAL, \"1.5\")\n}\n";

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

/// Load the fixture, run `line`, and give back what it printed on stdout
/// after the shell's echo of the line, plus everything on stderr.
fn answer_to(line: &str) -> (String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("t.db"), DB).expect("write the .db");
    let script = dir.path().join("t.cmd");
    std::fs::write(
        &script,
        format!("dbLoadRecords(\"t.db\")\niocInit\n{line}\n"),
    )
    .expect("write the script");

    let out_sink = tempfile::NamedTempFile::new().expect("stdout capture");
    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: the shell writes through fds 1 and 2 for the length of one
    // script and this test is `serial`, so nothing else holds either.
    let saved = unsafe {
        std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path());
        let saved = (libc::dup(1), libc::dup(2));
        libc::dup2(out_sink.as_file().as_raw_fd(), 1);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };

    let db = Arc::new(PvDatabase::new());
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let path = script.display().to_string();
    let ran = std::thread::spawn(move || IocShell::new(db, bridge).execute_script(&path)).join();

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // SAFETY: restoring the two descriptors this call replaced.
    unsafe {
        libc::dup2(saved.0, 1);
        libc::dup2(saved.1, 2);
        libc::close(saved.0);
        libc::close(saved.1);
        std::env::remove_var("EPICS_DB_INCLUDE_PATH");
    }
    assert!(ran.is_ok(), "the shell panicked running {line}");

    let out = strip_ansi(&std::fs::read_to_string(out_sink.path()).expect("stdout capture"));
    // C's build says two errlog lines of its own around the script's `iocInit`
    // (`iocInit.c:129` and `:273`); they are the shell's, not the command's,
    // so they are not part of what `dbpf` said.
    let err = strip_ansi(&std::fs::read_to_string(err_sink.path()).expect("stderr capture"))
        .replace("Starting iocInit\n", "")
        .replace("iocRun: All initialization complete\n", "");
    let after = out
        .split_once(&format!("{line}\n"))
        .map(|(_, a)| a.to_string())
        .unwrap_or_else(|| panic!("the shell never echoed {line}; got:\n{out}"));
    (after, err)
}

/// Text no conversion takes is a FAILED PUT, not a parse error: C leaves the
/// field alone, says nothing, and still echoes what the record kept.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn unconvertible_text_is_a_failed_put_and_still_echoes() {
    let (out, err) = answer_to("dbpf T:AI, \"notanumber\"");
    assert_eq!(out, "DBF_DOUBLE:         1.5       \n");
    assert_eq!(err, "", "C's dbpf writes nothing to stderr of its own");
}

/// A put whose read-back fails still prints `dbgf`'s failure line. `DBF_CHAR`
/// is what C prints here, off the end of its 13-entry `dbr[]` table for
/// `DBR_NOACCESS` (17); this port names the type it actually has, a deviation
/// signed off with the rest of that out-of-bounds read.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_put_whose_readback_fails_still_prints_dbgfs_failure_line() {
    let (out, _) = answer_to("dbpf T:AI.RSET, \"1\"");
    assert_eq!(out, "DBF_NOACCESS:       failed.   \n");
}

/// The successful case is unchanged, and still goes through the same printer.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_put_that_lands_echoes_the_new_value() {
    let (out, err) = answer_to("dbpf T:AI, \"2.5\"");
    assert_eq!(out, "DBF_DOUBLE:         2.5       \n");
    assert_eq!(err, "");
}
