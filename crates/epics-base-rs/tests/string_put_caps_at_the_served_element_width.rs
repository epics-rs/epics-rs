//! **A string put into a `cvt_dbaddr` string element is cut at 39, whatever
//! the declared member's width.**
//!
//! `AS.A`..`AS.U` are `DBF_NOACCESS` pointers (`sizeof` 8) that `cvt_dbaddr`
//! re-types to the `FTx` element: for `FTx=STRING` `field_size` becomes
//! `dbValueSize(DBF_STRING)` = 40, and `putStringString` stores 39 bytes.
//! Expected text is `softIoc` R7.0.10 (`dbpf AS.B "<62 chars>"` then `dbgf`,
//! and `dba AS.B`'s `Field Size: 40`).
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

const DB: &str = "\
record(aSub, \"AS\") {
    field(FTA, \"STRING\")
    field(FTB, \"STRING\")
    field(FTU, \"STRING\")
    field(NOA, \"1\")
    field(NOB, \"1\")
    field(NOU, \"1\")
}
record(stringout, \"SO\") {
    field(VAL, \"abc\")
}
record(lso, \"LSO\") {
    field(SIZV, \"20\")
}
record(lsi, \"LSI\") {
    field(SIZV, \"20\")
}
record(printf, \"PF\") {
    field(SIZV, \"20\")
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

/// Load the fixture, run `line`, and give back what it put on stdout after
/// the shell's own echo of that line.
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
    // The answer is what follows the echo of the script's FIRST line.
    let last = line.lines().next().unwrap_or(line);
    text.split_once(&format!("{last}\n"))
        .map(|(_, after)| after.to_string())
        .unwrap_or_else(|| panic!("the shell never echoed {last}; got:\n{text}"))
}

const LONG: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_string_put_to_a_served_string_element_stores_39_bytes() {
    // One IOC per process: all three fields in one script.
    let script = ["A", "B", "U"]
        .map(|f| format!("dbpf AS.{f} \"{LONG}\"\ndbgf AS.{f}"))
        .join("\n");
    let out = answer_to(&script);
    let want = format!("DBF_STRING:         \"{}\"", &LONG[..39]);
    // Each field: the `dbpf` echo and the `dbgf`, both at 39 bytes.
    assert_eq!(out.matches(&want).count(), 6, "{out:?}");
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn dba_reports_the_served_element_width_not_the_pointer() {
    let out = answer_to("dba AS.B");
    assert!(out.contains("Field Size: 40\n"), "{out:?}");
}

/// `cvt_dbaddr` of lso/lsi/printf sets `field_size = SIZV`
/// (`lsoRecord.c:186`, `lsiRecord.c:144`, `printfRecord.c:420`), per record.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn dba_reports_sizv_for_the_long_string_records() {
    let out = answer_to("dba LSO.VAL\ndba LSI.VAL\ndba PF.VAL");
    assert_eq!(out.matches("    Field Size: 20\n").count(), 3, "{out:?}");
}

/// The put cap and the record's own truncation are one width: SIZV - 1.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_long_string_put_is_cut_at_sizv_minus_one() {
    let out = answer_to(&format!("dbpf LSO.VAL \"{LONG}\"\ndbgf LSO.VAL"));
    let want = format!("DBF_STRING:         \"{}\"", &LONG[..19]);
    assert_eq!(out.matches(&want).count(), 2, "{out:?}");
}

/// `AS.B$`: the count is `get_array_info`'s (`NOB` = 1), so `dbgf` prints one
/// `DBF_CHAR`, while `dba` keeps the buffer's `No Elements: 40`; the plain
/// string fields keep their own widths.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn the_dollar_view_of_an_asub_string_element_counts_nob() {
    let out = answer_to("dbpf AS.B abc\ndbgf AS.B$\ndba AS.B$\ndba SO.VAL$\ndba SO.DESC$");
    assert!(
        out.contains("DBF_CHAR:           97 = 0x61 = 'a'"),
        "{out:?}"
    );
    let counts: Vec<&str> = out
        .lines()
        .filter(|l| l.starts_with("   No Elements:"))
        .collect();
    assert_eq!(
        counts,
        [
            "   No Elements: 40",
            "   No Elements: 40",
            "   No Elements: 41"
        ],
        "{out:?}"
    );
}
