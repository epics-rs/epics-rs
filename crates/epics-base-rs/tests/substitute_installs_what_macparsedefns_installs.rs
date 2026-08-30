//! **A `substitute` directive makes exactly the `macPutValue` calls C's
//! `macParseDefns` + `macInstallMacros` make for the same text.**
//!
//! The `.db` `substitute` directive is msi's, not dbStatic's — C's `.db`
//! grammar has no such production — so msi is the reference for the whole
//! round trip: `makeSubstitutions` cuts the text out between the outer
//! quotes with `\"` skipped as a pair and without unescaping it
//! (`msi.cpp:305-358`), `addMacroReplacements` hands that to
//! `macParseDefns` and its result straight to `macInstallMacros`
//! (`:256-275`), and `macParseDefns` removes quotes and escapes from
//! NAMES only, leaving values for `trans` to re-read at level 1
//! (`macUtil.c:199-227`).
//!
//! Four rules of that round trip this port did not have. Measured with
//! `msi` built from `~/work/epics-base` (`R7.0.10`):
//!
//! | `substitute` line | msi | this port, before |
//! |---|---|---|
//! | `"'A'=1,B"` with `-M B=preset` | `A=[1] B=[$(B)]` | `A=[$(A)] B=[preset]` |
//! | `"A=\"1\",B=2"` | `A=["1"] B=[2]` | value truncated at the escape |
//! | `"=1,B=2"` | `empty:[1] B=[2]` | the empty name dropped |
//! | `"X=1"` then `"X"` under `-M X=outer` | `after:[$(X)]` | `after:[outer]` |
//!
//! msi suppresses macLib's warnings by default (`msi.cpp:154`), which is
//! why its undefined placeholder is the short `$(X)`; the `.db` reader
//! runs at `dbQuietMacroWarnings`, which defaults to loud, so the same
//! reference comes out here as `$(X,undefined)`. `msi -V` prints that
//! longer form and was used to confirm each row above.
//!
//! Unix only: fd 2 is redirected so the notices these loads raise do not
//! land in the test harness's own output.

#![cfg(unix)]

use std::path::Path;

use epics_base_rs::server::db_loader::{DbFaults, DbLoadConfig, MacroDefs, expand_includes};

/// Flatten a one-file include tree with fd 2 pointed at a temporary file.
fn expanded(path: &Path, macros: &MacroDefs) -> String {
    let sink = tempfile::NamedTempFile::new().expect("capture file");
    let saved = unsafe { libc::dup(2) };
    assert!(saved >= 0, "dup(2) failed");
    let fd = {
        use std::os::fd::AsRawFd;
        sink.as_file().as_raw_fd()
    };
    assert!(unsafe { libc::dup2(fd, 2) } >= 0, "dup2 onto fd 2 failed");

    let config = DbLoadConfig::default();
    let macros = macros.clone();
    let ran = std::panic::catch_unwind(move || {
        expand_includes(path, &macros, &config, &mut DbFaults::default())
    });

    // Restore BEFORE anything can panic on the assertion, or the failure
    // report has nowhere to go.
    assert!(unsafe { libc::dup2(saved, 2) } >= 0, "restore fd 2 failed");
    unsafe { libc::close(saved) };
    ran.expect("the expansion").expect("the flattened text")
}

/// Write `text` as a `.db`, expand it against `macros`, and give back the
/// flattened result. A directive line contributes an empty line, which is
/// how this reader keeps a diagnostic's line number the operator's.
fn through_the_reader(text: &str, macros: &[(&str, &str)]) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sub.db");
    std::fs::write(&path, text).expect("write the .db");
    let mut defs = MacroDefs::new();
    for (name, value) in macros {
        defs.put(*name, *value);
    }
    expanded(&path, &defs)
}

/// **Boundary: a quoted name, and a name with no `=` at all.**
///
/// One line carries both, because the two rules meet in `macParseDefns`'s
/// `del[]` array and a port can pass either alone.
#[test]
#[serial_test::serial(db_load_stderr)]
fn quotes_come_off_a_name_and_a_bare_name_deletes() {
    let got = through_the_reader(
        "substitute \"'A'=1,B\"\nA=[$(A)] B=[$(B)]\n",
        &[("B", "preset")],
    );
    assert_eq!(got, "\nA=[1] B=[$(B,undefined)]\n");
}

/// **Boundary: an escaped quote, which is where the directive scan and
/// the value both have to leave the text alone.**
///
/// msi's closing-quote scan skips `\"` as a pair and copies the escape
/// through, and the value keeps it until `trans` discards it at level 1 —
/// so what comes out is a literal `"1"`.
#[test]
#[serial_test::serial(db_load_stderr)]
fn an_escaped_quote_survives_the_directive_and_the_value() {
    let got = through_the_reader("substitute \"A=\\\"1\\\",B=2\"\nA=[$(A)] B=[$(B)]\n", &[]);
    assert_eq!(got, "\nA=[\"1\"] B=[2]\n");
}

/// **Boundary: an escaped quote in a NAME.**
///
/// The same two characters mean the opposite thing here: C's cleanup loop
/// reaches its escape branch only after the quote branch has declined the
/// character, so `\"A\"` is a macro whose name is `"A"`, quotes included,
/// and `$(A)` finds nothing.
#[test]
#[serial_test::serial(db_load_stderr)]
fn an_escaped_quote_in_a_name_is_a_literal_quote() {
    let got = through_the_reader("substitute \"\\\"A\\\"=1\"\nA=[$(A)]\n", &[]);
    assert_eq!(got, "\nA=[$(A,undefined)]\n");
}

/// **Boundary: the empty name.**
///
/// `=1` is a definition, not a syntax error to drop: `msi` on `substitute
/// "=1,B=2"` resolves `$()` to `1`.
#[test]
#[serial_test::serial(db_load_stderr)]
fn an_empty_name_is_a_name() {
    let got = through_the_reader("substitute \"=1,B=2\"\nempty:[$()] B=[$(B)]\n", &[]);
    assert_eq!(got, "\nempty:[1] B=[2]\n");
}

/// **Boundary: a deletion of a name the caller defined.**
///
/// Both definitions sit at scope level 0, so the deletion removes the
/// entry outright and leaves nothing behind it. Measured with `msi -V -M
/// X=outer`: `after:[$(X,undefined)]`.
#[test]
#[serial_test::serial(db_load_stderr)]
fn a_deletion_removes_the_callers_definition_too() {
    let got = through_the_reader(
        "substitute \"X=1\"\nmid:[$(X)]\nsubstitute \"X\"\nafter:[$(X)]\n",
        &[("X", "outer")],
    );
    assert_eq!(got, "\nmid:[1]\n\nafter:[$(X,undefined)]\n");
}

/// **Boundary: text after the closing quote.**
///
/// msi requires blanks only between the closing quote and the end of the
/// line; anything else and the line was never a command, so it is
/// expanded as ordinary text and no macro is defined by it.
#[test]
#[serial_test::serial(db_load_stderr)]
fn a_line_with_trailing_text_is_not_a_directive() {
    let got = through_the_reader("substitute \"A=1\" junk\nA=[$(A)]\n", &[]);
    assert_eq!(got, "substitute \"A=1\" junk\nA=[$(A,undefined)]\n");
}
