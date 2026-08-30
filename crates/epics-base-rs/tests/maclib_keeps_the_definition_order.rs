//! **The macro table's notice order is the order the definitions were
//! made in, and nothing else.**
//!
//! C builds its table one `macPutValue` at a time: `macInstallMacros`
//! walks the `pairs` array `macParseDefns` produced, in the order the
//! operator wrote them (`macUtil.c:250-275`), and `expand` then walks the
//! table in that same order (`macCore.c:655`). So the sequence a load's
//! `macLib:` notices come out in is the sequence of the definitions, and
//! a redefinition does not move its entry — `rawval` writes through the
//! entry `lookup` found (`macCore.c:610-619`).
//!
//! Measured on `softIoc` built from `~/work/epics-base` (`R7.0.10`), with
//! the two definitions delivered raw through a `.substitutions` row so
//! they reach `macLib` unexpanded, and with the alphabetical order and
//! the definition order deliberately opposed:
//!
//! ```text
//! $ cat ordB.sub                     $ cat ordA.sub
//! file ord.db {                      file ord.db {
//! { B="$(NOPEB)", A="$(NOPEA)" }     { A="$(NOPEA)", B="$(NOPEB)" }
//! }                                  }
//!
//! macLib: macro NOPEB is undefined   macLib: macro NOPEA is undefined
//!         (expanding macro B)                (expanding macro A)
//! macLib: macro NOPEA is undefined   macLib: macro NOPEB is undefined
//!         (expanding macro A)                (expanding macro B)
//! ```
//!
//! This port sorted the names instead, which agreed with C on every shape
//! measured so far only because those shapes were already alphabetical.
//! [`MacroDefs`] carries the order, and the sort survives in exactly one
//! place — the conversion from a [`HashMap`], where there is no order to
//! carry and the only thing left to promise is that two runs agree.
//!
//! Unix only: what is captured is the process console, and the only way to
//! capture it is to point fd 2 somewhere else and put it back.

#![cfg(unix)]

use std::collections::HashMap;

use epics_base_rs::server::db_loader::{MacroDefs, MacroExpandOptions, expand_macros};

/// Expand `src` against `defs` with fd 2 pointed at a file, and give back
/// everything `macLib` wrote.
fn notices(src: &str, defs: impl Into<MacroDefs>) -> String {
    let sink = tempfile::NamedTempFile::new().expect("capture file");
    let saved = unsafe { libc::dup(2) };
    assert!(saved >= 0, "dup(2) failed");
    let fd = {
        use std::os::fd::AsRawFd;
        sink.as_file().as_raw_fd()
    };
    assert!(unsafe { libc::dup2(fd, 2) } >= 0, "dup2 onto fd 2 failed");

    let defs = defs.into();
    let ran =
        std::panic::catch_unwind(|| expand_macros(src, &defs, MacroExpandOptions::default()).text);

    // Restore BEFORE anything can panic on the assertion, or the failure
    // report has nowhere to go.
    assert!(unsafe { libc::dup2(saved, 2) } >= 0, "restore fd 2 failed");
    unsafe { libc::close(saved) };
    ran.expect("the expansion");

    strip_ansi(&std::fs::read_to_string(sink.path()).expect("read the capture"))
}

/// C's SGR sequences, dropped — `errlog` strips its own the same way when
/// the console is not a terminal (`errlog.c:672-681`).
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

/// The `.db` line the two shapes above load. It mentions neither macro:
/// the notices under test are the TABLE pass's, raised before the string
/// is looked at, which is why C prints them for a line that refers to
/// nothing.
const LINE: &str = "#ord\n";

const B_THEN_A: &str = "\
macLib: macro NOPEB is undefined (expanding macro B)
macLib: macro NOPEA is undefined (expanding macro A)
";

const A_THEN_B: &str = "\
macLib: macro NOPEA is undefined (expanding macro A)
macLib: macro NOPEB is undefined (expanding macro B)
";

/// **Boundary: definition order against alphabetical order.**
///
/// Both directions, because a port that sorts passes one of them. `B`
/// first is the row a sort gets wrong.
#[test]
#[serial_test::serial(db_load_stderr)]
fn the_notice_order_is_the_definition_order() {
    let mut b_first = MacroDefs::new();
    b_first.put("B", "$(NOPEB)");
    b_first.put("A", "$(NOPEA)");
    assert_eq!(notices(LINE, &b_first), B_THEN_A);

    let mut a_first = MacroDefs::new();
    a_first.put("A", "$(NOPEA)");
    a_first.put("B", "$(NOPEB)");
    assert_eq!(notices(LINE, &a_first), A_THEN_B);
}

/// **Boundary: a redefinition of a name already in the table.**
///
/// C `macPutValue` looks the name up and writes through the entry it
/// finds, so the entry keeps its place and the second definition of `B`
/// does not move it behind `A`. A port that appended would report `A`
/// first here.
#[test]
#[serial_test::serial(db_load_stderr)]
fn a_redefinition_keeps_the_entry_where_it_was() {
    let mut defs = MacroDefs::new();
    defs.put("B", "unset");
    defs.put("A", "$(NOPEA)");
    defs.put("B", "$(NOPEB)");
    assert_eq!(defs.len(), 2, "a redefinition adds no entry");
    assert_eq!(notices(LINE, &defs), B_THEN_A);
}

/// **Boundary: the input that has no order to carry.**
///
/// A [`HashMap`] iterates in whatever order its hasher gives, so the
/// conversion sorts by name — the one surviving sort, and the only thing
/// it promises is that two runs of the same load agree. Both maps below
/// hold the same two definitions and are built in opposite orders.
#[test]
#[serial_test::serial(db_load_stderr)]
fn a_hash_map_falls_back_to_name_order_whichever_way_it_was_built() {
    let mut b_first: HashMap<String, String> = HashMap::new();
    b_first.insert("B".into(), "$(NOPEB)".into());
    b_first.insert("A".into(), "$(NOPEA)".into());

    let mut a_first: HashMap<String, String> = HashMap::new();
    a_first.insert("A".into(), "$(NOPEA)".into());
    a_first.insert("B".into(), "$(NOPEB)".into());

    assert_eq!(notices(LINE, &b_first), A_THEN_B);
    assert_eq!(notices(LINE, &a_first), A_THEN_B);
}
