//! **Every `dbTest.c` command resolves a name through the same rule.**
//!
//! C routes all seven — `dba`, `dbgf`, `dbpf`, `dbpr`, `dbtr`, `dbtgf`,
//! `dbtpf` — through one `nameToAddr` (`dbTest.c:787-795`), so what
//! `<record>[.<FIELD>]` selects, and when `PV '%s' not found` is printed,
//! cannot differ between them.
//!
//! The port had seven spellings of the test, and measured against
//! `~/work/epics-base/bin/linux-x86_64/softIoc`
//! (R7.0.10-146-g8f5015b663d764ad75df) on the fixture below they disagreed in
//! three directions at once: `dbpr`/`dbtr` never split off the field, so
//! `dbpr T:AI.VAL` answered not-found where C printed the record;
//! `dba`/`dbpf`/`dbtgf` read "declared but unreadable" as unresolved, so
//! `dbpf T:AI.RSET` answered not-found where C reached the put; and `dbtpf`
//! asked only for the record, so `dbtpf T:AI.NOSUCHFLD` printed twelve
//! `Put as DBR_… Failed.` lines where C printed not-found.
//!
//! The boundary is the same three values for every command, so it is tested
//! that way rather than by command: a field the type declares AND serves, a
//! field it declares and does NOT serve (`DBF_NOACCESS`), and a name it does
//! not declare at all. What each command prints once the name HAS resolved is
//! not this file's subject — only whether it resolved.
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

const DB: &str = "\
record(ai, \"T:AI\") {
    field(DESC, \"an analog in\")
    field(EGU, \"V\")
    field(PREC, \"3\")
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

/// Load the fixture, run `line`, and give back what it put on stdout —
/// without the shell's own echo of the line, which C prints too and which
/// says nothing about the resolution.
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
    // Drop the echoed script lines: the two setup lines, this line, and
    // whatever `iocInit` announced.
    text.split_once(&format!("{line}\n"))
        .map(|(_, after)| after.to_string())
        .unwrap_or_else(|| panic!("the shell never echoed {line}; got:\n{text}"))
}

/// The line C's `nameToAddr` prints, and the only thing the caller then
/// prints (`dbTest.c:789-791`).
fn not_found(pname: &str) -> String {
    format!("PV '{pname}' not found\n")
}

/// Every command must resolve a field the record type declares and serves.
///
/// `DESC` rather than `VAL` on purpose: `VAL` is what a bare record name
/// already defaults to (`parse_pv_name`), so it cannot tell a command that
/// splits the name from one that ignores everything after the dot.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_served_field_resolves_for_every_command() {
    for line in [
        "dba T:AI.DESC",
        "dbgf T:AI.DESC",
        "dbpf T:AI.DESC, \"x\"",
        "dbpr T:AI.DESC",
        "dbtr T:AI.DESC",
        "dbtgf T:AI.DESC",
        "dbtpf T:AI.DESC, \"x\"",
    ] {
        let got = answer_to(line);
        assert!(
            !got.contains("not found"),
            "`{line}` must resolve, as C's dbNameToAddr does; got:\n{got}"
        );
        assert!(!got.is_empty(), "`{line}` printed nothing at all");
    }
}

/// A `DBF_NOACCESS` field is DECLARED, so C resolves it and the read or write
/// fails afterwards, inside `dbGet`/`dbPut` — `dbgf T:AI.RSET` prints a type
/// header and `failed.`, never the not-found line.
///
/// `dba` is not in this list: it resolves the name here too, but the block it
/// then prints needs a per-field descriptor the port's generated tables do
/// not carry for a `dbCommon` `DBF_NOACCESS` row. `dbtgf` is not in it for the
/// same reason — it needs a readable snapshot to print its option block.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_declared_but_unreadable_field_resolves_too() {
    for line in ["dbgf T:AI.RSET", "dbpf T:AI.RSET, \"1\"", "dbpr T:AI.RSET"] {
        let got = answer_to(line);
        assert!(
            !got.contains("not found"),
            "`{line}`: a declared DBF_NOACCESS field resolves in C; got:\n{got}"
        );
    }
}

/// A name the record type does not declare resolves nowhere, and every
/// command says so with C's one line and nothing else.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn an_undeclared_field_is_not_found_for_every_command() {
    for (line, pname) in [
        ("dba T:AI.NOSUCHFLD", "T:AI.NOSUCHFLD"),
        ("dbgf T:AI.NOSUCHFLD", "T:AI.NOSUCHFLD"),
        ("dbpf T:AI.NOSUCHFLD, \"1\"", "T:AI.NOSUCHFLD"),
        ("dbpr T:AI.NOSUCHFLD", "T:AI.NOSUCHFLD"),
        ("dbtr T:AI.NOSUCHFLD", "T:AI.NOSUCHFLD"),
        ("dbtgf T:AI.NOSUCHFLD", "T:AI.NOSUCHFLD"),
        ("dbtpf T:AI.NOSUCHFLD, \"1\"", "T:AI.NOSUCHFLD"),
        ("dba NOSUCH", "NOSUCH"),
        ("dbgf NOSUCH", "NOSUCH"),
        ("dbpf NOSUCH, \"1\"", "NOSUCH"),
        ("dbpr NOSUCH", "NOSUCH"),
        ("dbtr NOSUCH", "NOSUCH"),
        ("dbtgf NOSUCH", "NOSUCH"),
        ("dbtpf NOSUCH, \"1\"", "NOSUCH"),
    ] {
        assert_eq!(answer_to(line), not_found(pname), "`{line}`");
    }
}

/// `dbpr` prints the WHOLE record whatever field the address landed on —
/// C's `dbpr_report` walks the record's field table and never looks at
/// `paddr->pfldDes` — so these three lines are one report, byte for byte.
///
/// The block is `softIoc`'s own, measured at level 0 on the fixture above.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn dbpr_ignores_the_field_the_name_selected() {
    const LEVEL0: &str = "\
AMSG:               ASG :               DESC: an analog in  DISA: 0             
DISV: 1             NAME: T:AI          NAMSG:              RVAL: 0             
SEVR: NO_ALARM      STAT: UDF           SVAL: 0             TPRO: 0             
VAL : 1.5           
";
    for line in ["dbpr T:AI", "dbpr T:AI.VAL", "dbpr T:AI.DESC"] {
        assert_eq!(answer_to(line), LEVEL0, "`{line}`");
    }
}
