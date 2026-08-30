//! **`dbl`'s field list is `dbFindField` + `dbGetString`, and nothing else.**
//!
//! C's `printFieldsList` (`dbTest.c:127-147`,
//! R7.0.10-146-g8f5015b663d764ad75df) is three lines of decision per field:
//!
//! ```c
//!     long status = dbFindField(pdbentry, papfields[ifield]);
//!     if (status) {
//!         if (!strcmp(papfields[ifield], "recordType")) pvalue = dbGetRecordTypeName(pdbentry);
//!         else { printf(", "); continue; }
//!     }
//!     else pvalue = dbGetString(pdbentry);
//!     printf(", \"%s\"", (pvalue ? pvalue : ""));
//! ```
//!
//! so there are exactly three outcomes: a bare `, ` for a name the record type
//! does not declare, `, ""` for one it declares that `dbGetString` has no arm
//! for, and `, "<text>"` from the static renderer otherwise.
//!
//! The port asked the RECORD for its own fields — `Record::get_field`, which
//! knows nothing of `dbCommon` — and rendered with Rust's `Display`. Measured
//! against `~/work/epics-base/bin/linux-x86_64/softIoc` on the fixture below,
//! that lost every `dbCommon` field to the bare separator (`DESC`, `SCAN`,
//! `INP`, `EGU`, `PREC`, `FLNK`) and printed an unwritten `waveform.VAL` as
//! `T:WF, "[]"` where C prints `T:WF, ""`.
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;

/// Four record types, chosen for the four renderings: a numeric with a
/// `dbCommon` string and a link (`ai`), a `DBF_NOACCESS` array VAL
/// (`waveform`), an enum VAL (`bo`), and a type with no `EGU`/`PREC` of its
/// own so the bare-separator case has a record (`bo` again, and `mbbi`).
const DB: &str = "\
record(ai, \"T:AI\") {
    field(DESC, \"an analog in\")
    field(EGU, \"V\")
    field(PREC, \"3\")
    field(VAL, \"1.5\")
    field(INP, \"T:BO\")
}
record(waveform, \"T:WF\") {
    field(FTVL, \"DOUBLE\")
    field(NELM, \"4\")
}
record(bo, \"T:BO\") {
    field(ZNAM, \"Off\")
    field(ONAM, \"On\")
    field(VAL, \"1\")
}
record(mbbi, \"T:MB\") {
    field(ZRVL, \"10\")
    field(ZRST, \"zero\")
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

/// Load the fixture, run one `dbl`, and give back only what it listed.
fn dbl(fields: &str) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("t.db"), DB).expect("write the .db");
    let line = format!("dbl \"\" \"{fields}\"");
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

    let out = strip_ansi(&std::fs::read_to_string(sink.path()).expect("stdout capture"));
    out.split_once(&format!("{line}\n"))
        .map(|(_, a)| a.to_string())
        .unwrap_or_else(|| panic!("the shell never echoed {line}; got:\n{out}"))
}

/// One case per rendering `dbGetString` has, plus the two non-`dbGetString`
/// outcomes. Every block is `softIoc`'s own bytes.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn every_field_list_shape_is_cs() {
    // A numeric VAL, an enum VAL rendered as its INDEX (C's `DBF_ENUM` arm
    // is the number, not the choice), and a DBF_NOACCESS array VAL as "".
    assert_eq!(
        dbl("VAL"),
        "T:AI, \"1.5\"\nT:BO, \"1\"\nT:MB, \"0\"\nT:WF, \"\"\n"
    );
    // A dbCommon string: present on every record type, empty where unset.
    assert_eq!(
        dbl("DESC"),
        "T:AI, \"an analog in\"\nT:BO, \"\"\nT:MB, \"\"\nT:WF, \"\"\n"
    );
    // A dbCommon MENU, rendered as its CHOICE (C's `DBF_MENU` arm).
    assert_eq!(
        dbl("SCAN"),
        "T:AI, \"Passive\"\nT:BO, \"Passive\"\nT:MB, \"Passive\"\nT:WF, \"Passive\"\n"
    );
    // A link, rebuilt by `dbGetString`'s DB_LINK arm with its pp/ms words —
    // and `bo`, which declares no INP at all, taking the bare separator.
    assert_eq!(
        dbl("INP"),
        "T:AI, \"T:BO NPP NMS\"\nT:BO, \nT:MB, \"\"\nT:WF, \"\"\n"
    );
    // The pseudo-field C answers from `dbGetRecordTypeName`.
    assert_eq!(
        dbl("recordType"),
        "T:AI, \"ai\"\nT:BO, \"bo\"\nT:MB, \"mbbi\"\nT:WF, \"waveform\"\n"
    );
    // A `dbCommon` DBF_NOACCESS pointer: declared, so `dbFindField` finds it
    // and `dbGetString`'s default arm answers NULL.
    assert_eq!(
        dbl("RSET"),
        "T:AI, \"\"\nT:BO, \"\"\nT:MB, \"\"\nT:WF, \"\"\n"
    );
    // Several fields at once, with one name nothing declares last so the
    // trailing bare separator is visible.
    assert_eq!(
        dbl("VAL DESC NOSUCH"),
        "T:AI, \"1.5\", \"an analog in\", \nT:BO, \"1\", \"\", \n\
         T:MB, \"0\", \"\", \nT:WF, \"\", \"\", \n"
    );
    // Record-own fields, absent from two of the four types.
    assert_eq!(
        dbl("EGU PREC"),
        "T:AI, \"V\", \"3\"\nT:BO, , \nT:MB, , \nT:WF, \"\", \"0\"\n"
    );
}
