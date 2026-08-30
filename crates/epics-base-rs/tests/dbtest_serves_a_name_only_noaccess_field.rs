//! **A `dbCommon` `DBF_NOACCESS` internal has an ADDRESS, and every
//! `dbTest.c` command works from it.**
//!
//! C's `dbNameToAddr` resolves any field the `.dbd` declares, so `RSET` — a
//! `struct typed_rset *` behind `special(SPC_NOMOD)` (`dbCommon.dbd:216-221`)
//! — comes back with a full `DBADDR` and every command then just reads it.
//! Measured on `~/work/epics-base/bin/linux-x86_64/softIoc`
//! (R7.0.10-146-g8f5015b663d764ad75df), `dba T:AI.RSET` prints the descriptor
//! block and `dbtgf T:AI.RSET` the option block plus thirteen value lines.
//!
//! This port resolved the name in one place and then asked a second, narrower
//! question in each command — `field_desc`, `snapshot_for_field` — which have
//! no entry for the rows the generator carries by NAME only. `dba T:AI.RSET`
//! printed nothing at all and `dbtgf T:AI.RSET` printed `PV '…' not found`
//! for a name it had just resolved.
//!
//! Three cells of `dba`'s block have no counterpart here and print `(none)`:
//! the field address (this port reads a field through `Record::get_field`,
//! not at a struct offset), and — for these rows only — the descriptor
//! pointer and the width, because the generator carries a descriptor only for
//! the `DBF_NOACCESS` rows whose `extra(...)` names a plain scalar. What is
//! asserted below is everything else, which is C's bytes.
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
    text.split_once(&format!("{line}\n"))
        .map(|(_, after)| after.to_string())
        .unwrap_or_else(|| panic!("the shell never echoed {line}; got:\n{text}"))
}

/// `dba` on a name-only `DBF_NOACCESS` row.
///
/// C's block for `dba T:AI.RSET`, with the run-varying pointers elided:
///
/// ```text
/// Record Address: 0x… Field Address: 0x… Field Description: 0x…
///    No Elements: 1
///    Record Type: ai
///     Field Type: 17 = DBF_NOACCESS
///     Field Size: 8
///        Special: 1
/// DBR Field Type: 17 = DBR_NOACCESS
/// ```
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn dba_prints_the_descriptor_block_for_rset() {
    let out = answer_to("dba T:AI.RSET");
    let (head, rest) = out.split_once('\n').expect("dba printed nothing");
    assert!(
        head.starts_with("Record Address: 0x")
            && head.ends_with(" Field Address: (none) Field Description: (none)"),
        "header was {head:?}"
    );
    assert_eq!(
        rest,
        "   No Elements: 1\n\
         \x20  Record Type: ai\n\
         \x20   Field Type: 17 = DBF_NOACCESS\n\
         \x20   Field Size: (none)\n\
         \x20      Special: 1\n\
         DBR Field Type: 17 = DBR_NOACCESS\n"
    );
}

/// `dbtgf` on the same row — `softIoc`'s bytes, with the one substitution
/// this port makes on purpose.
///
/// C's native header reads `DBF_CHAR[0]`, not `DBF_NOACCESS[0]`: `dbr[]` is
/// declared `[DBR_ENUM+2]` (`dbTest.c:82-85`) while `DBR_NOACCESS` is 17, so
/// `dbr[17]` reads past the end and R7.0.10 prints the adjacent `dbf[1]`.
/// The port prints the type C meant.
///
/// The option block is C's own, line for line: `getOptions` copies the alarm
/// off `paddr->precord`, leaves `units` a set-but-empty option for a class
/// that is neither float nor integer, clears `DBR_PRECISION`, and every limit
/// falls to `recGblGetGraphicDouble`'s zeros because `aiRecord`'s
/// `get_graphic_double` answers HOPR/LOPR only for `VAL`.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn dbtgf_prints_the_option_block_and_thirteen_values_for_rset() {
    assert_eq!(
        answer_to("dbtgf T:AI.RSET"),
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
         alDouble: 0 < 0 .. 0 < 0\n\
         DBF_NOACCESS[0]: (empty)      Bad DBR type 17     \n\
         DBF_STRING:         failed.   \n\
         DBF_CHAR:           failed.   \n\
         DBF_UCHAR:          failed.   \n\
         DBF_SHORT:          failed.   \n\
         DBF_USHORT:         failed.   \n\
         DBF_LONG:           failed.   \n\
         DBF_ULONG:          failed.   \n\
         DBF_INT64:          failed.   \n\
         DBF_UINT64:         failed.   \n\
         DBF_FLOAT:          failed.   \n\
         DBF_DOUBLE:         failed.   \n\
         DBF_ENUM:           failed.   \n"
    );
}

/// The control: a readable field still takes the typed path, so the
/// zero-element native header stays bare — C's `default:` arm is the only one
/// that speaks under it.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_readable_field_keeps_its_bare_zero_element_header() {
    let out = answer_to("dbtgf T:AI.VAL");
    assert!(
        out.contains("\nDBF_DOUBLE[0]: (empty)        \nDBF_STRING:         \"1.500\"   \n"),
        "native header was not bare in:\n{out}"
    );
}
