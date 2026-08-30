//! **macLib's third error arm: a macro reference whose closing delimiter never
//! matched its opener.**
//!
//! C `refer` (`macCore.c:862-875`) does three things when `*r != close`: it
//! rewinds the output pointer to where the reference began and copies the raw
//! text from the `$` through the last character `trans` scanned — the whole
//! rest of the string — verbatim; it sets `entry->error`; and it writes
//!
//! ```text
//! macLib: unterminated macro reference in <type> <name>
//! ```
//!
//! with `macExpandString`'s fake `"string"` entry, whose `name` is the caller's
//! whole source string (`macCore.c:208-209`). Because `entry->error` is set,
//! `macExpandString` returns a negative length and the `.db` reader's own
//! per-line warning (`dbLexRoutines.c:378-387`) fires under it — two lines, not
//! one.
//!
//! This port had none of it. `refer` returned `None` at the depth check and the
//! caller pushed a bare `$` and carried on scanning, which meant three
//! divergences at once: no `macLib:` line, no error flag and so no `WARNING:`
//! line either, and — because the caller re-entered `trans` one character past
//! the `$` — any `$(…)` in the tail got expanded a second time where C never
//! looks at it again.
//!
//! Every expected byte below was captured from `softIoc` built from
//! `~/work/epics-base` (`R7.0.10`), loading these exact files. ANSI is stripped
//! before comparing, for the reason the sibling gate
//! `db_load_diagnostics_say_where` gives: whether the escapes are emitted is a
//! property of the console predicate and has its own gate.
//!
//! Unix only: what is captured is the process console, and the only way to
//! capture it is to point fd 2 somewhere else and put it back.

#![cfg(unix)]

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::db_loader::{MacroExpandOptions, MacroFault, expand_macros};
use epics_base_rs::server::iocsh::IocShell;

/// The four shapes the unterminated arm has to cover, one per line, each on a
/// `#` comment so the only thing the load can produce is the diagnostics — C
/// expands every line it reads, comments included (`db_yyinput`).
///
///   * line 2 — a plain `$(` with no `)` anywhere after it;
///   * line 3 — the `${` opener, whose `close` is `}`;
///   * line 4 — a MISMATCHED delimiter: `$(A}` closes nothing, and the `)` that
///     follows belongs to the inner `$(Q)`, which the name translation consumes;
///   * line 5 — two openers and no closer at all.
///
/// Lines 2-4 each carry a `$(Q)` in the tail, and `Q` IS defined. That is the
/// discriminator: C never re-scans the tail, so `$(Q)` survives as text.
const FAMILY_DB: &str = "record(ai, \"T1\") {}\n\
                         #x$(A $(Q) y\n\
                         #p${A $(Q) q\n\
                         #m$(A} $(Q) n\n\
                         #$(A$(B z\n";

/// Run one iocsh line with fd 2 pointed at a file, and give back what it wrote.
///
/// The two families this gates reach the operator by two different routes —
/// `errlogPrintf`'s console fallback for macLib's line, `eprintln!` for the
/// loader's — and the only place they are one stream again is the process
/// console, which is also the only place their order is the operator's order.
fn stderr_of(line: &str, db: Arc<PvDatabase>) -> String {
    let sink = tempfile::NamedTempFile::new().expect("capture file");
    let saved = unsafe { libc::dup(2) };
    assert!(saved >= 0, "dup(2) failed");
    let fd = {
        use std::os::fd::AsRawFd;
        sink.as_file().as_raw_fd()
    };
    assert!(unsafe { libc::dup2(fd, 2) } >= 0, "dup2 onto fd 2 failed");

    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let owned = line.to_string();
    let ran = std::thread::spawn(move || IocShell::new(db, bridge).execute_line(&owned)).join();

    // Restore BEFORE anything can panic on the assertion, or the failure
    // report has nowhere to go.
    assert!(unsafe { libc::dup2(saved, 2) } >= 0, "restore fd 2 failed");
    unsafe { libc::close(saved) };
    ran.expect("the shell thread").ok();

    strip_ansi(&std::fs::read_to_string(sink.path()).expect("read the capture"))
}

/// C's SGR sequences, dropped. `errlog` strips its own the same way when the
/// console is not a terminal (`errlog.c:672-681`); this is the test's copy so
/// the gate holds whichever way the harness's console answers.
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

/// Write `FAMILY_DB` where a bare `dbLoadRecords` name will find it, and give
/// back the directory (kept alive by the caller) and its path.
fn family_db_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("fam.db"), FAMILY_DB).expect("write the .db");
    dir
}

/// The whole of what C writes for the four shapes, byte for byte.
///
/// Note the blank line under each `macLib:` line: `entry->name` is the `fgets`
/// line and still carries its own `\n`, and `errlogPrintf`'s format adds a
/// second one. That is why the notice must be raised on the raw line and not on
/// a trimmed copy.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn an_unterminated_reference_names_the_string_and_the_line() {
    let dir = family_db_dir();
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let got = stderr_of(
        "dbLoadRecords(\"fam.db\",\"Q=ZZZ\")",
        Arc::new(PvDatabase::new()),
    );
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = "\
macLib: unterminated macro reference in string #x$(A $(Q) y

WARNING: 'fam.db' line 2 has undefined macros
macLib: unterminated macro reference in string #p${A $(Q) q

WARNING: 'fam.db' line 3 has undefined macros
macLib: unterminated macro reference in string #m$(A} $(Q) n

WARNING: 'fam.db' line 4 has undefined macros
macLib: unterminated macro reference in string #$(A$(B z

WARNING: 'fam.db' line 5 has undefined macros
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

/// `dbQuietMacroWarnings` drops the `macLib:` line and keeps the loader's.
///
/// C sets `entry->error` BEFORE it consults `FLAG_SUPPRESS_WARNINGS`
/// (`macCore.c:869-874`), so `macExpandString` still returns a negative length
/// and `db_yyinput` still warns. Measured on `softIoc` with `var
/// dbQuietMacroWarnings 1` over the same file: the four `macLib:` notices go,
/// all four `WARNING:` lines stay. A suppression that swallowed both would
/// leave a `.db` whose text is raw `$(` with nothing said about it at all.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn suppressing_the_notice_does_not_clear_the_error_flag() {
    let dir = family_db_dir();
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    epics_base_rs::server::db_loader::set_db_quiet_macro_warnings(true);
    let got = stderr_of(
        "dbLoadRecords(\"fam.db\",\"Q=ZZZ\")",
        Arc::new(PvDatabase::new()),
    );
    epics_base_rs::server::db_loader::set_db_quiet_macro_warnings(false);
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = "\
WARNING: 'fam.db' line 2 has undefined macros
WARNING: 'fam.db' line 3 has undefined macros
WARNING: 'fam.db' line 4 has undefined macros
WARNING: 'fam.db' line 5 has undefined macros
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

/// The tail of an unterminated reference is text, not input.
///
/// This is the half the diagnostics cannot show. C's copy loop runs from the
/// `$` to the end of the string and nothing after it is looked at again, so a
/// perfectly resolvable `$(Q)` sitting in the tail comes out as the four
/// characters `$(Q)`. Measured on `softIoc` with `Q=ZZZ` by putting each shape
/// in a `field(DESC, …)` split across lines so the `.db` line itself carries no
/// `)`, and reading back what the loader refused to set:
///
/// ```text
/// ERROR: Can't set 'T1.DESC' to 'x$(A $(Q) y'  : Bad Field value
/// ERROR: Can't set 'T2.DESC' to 'p${A $(Q) q'  : Bad Field value
/// ERROR: Can't set 'T3.DESC' to 'm$(A} $(Q) n'  : Bad Field value
/// ERROR: Can't set 'T4.DESC' to '$(A$(B x'  : Bad Field value
/// ```
///
/// Before the fix this port wrote `x$(A ZZZ y` for the first of those: the
/// caller re-scanned from one character past the `$`, and the `$(Q)` it found
/// there had already been consumed as part of the unterminated reference's
/// name.
#[test]
fn the_tail_after_an_unterminated_reference_is_copied_not_expanded() {
    let macros = HashMap::from([("Q".to_string(), "ZZZ".to_string())]);
    for raw in [
        "\"x$(A $(Q) y\"\n",
        "\"p${A $(Q) q\"\n",
        "\"m$(A} $(Q) n\"\n",
        "\"$(A$(B x\"\n",
    ] {
        let got = expand_macros(
            raw,
            &macros,
            MacroExpandOptions {
                suppress_warnings: true,
                ..MacroExpandOptions::default()
            },
        );
        assert_eq!(got.text, raw, "the whole tail must survive verbatim");
        let copied = &raw[raw.find('$').expect("a $ in the fixture")..];
        assert_eq!(
            got.fault(),
            Some(MacroFault::Unterminated(copied)),
            "the fault must name the text that was copied through"
        );
    }
}

/// A reference that IS terminated must not be dragged into the new arm, and a
/// `$` that is not a reference at all must still be a plain character.
///
/// The depth counter is what decides, and it counts `$(`/`${` openers against
/// `)`/`}` closers — so `$(A$(B))` is balanced and resolves, while `$(A$(B)` is
/// not and is copied. The pair is here because the fix moved the unterminated
/// case from "return `None`, let the caller cope" to "consume the rest of the
/// input", and that is a much bigger hammer to hold to the correct side of the
/// boundary.
#[test]
fn a_balanced_reference_is_still_expanded() {
    let macros = HashMap::from([
        ("A".to_string(), "outer".to_string()),
        ("B".to_string(), "A".to_string()),
        ("Q".to_string(), "ZZZ".to_string()),
    ]);
    let opts = MacroExpandOptions {
        suppress_warnings: true,
        ..MacroExpandOptions::default()
    };
    for (raw, want) in [
        ("$($(B)) tail $(Q)", "outer tail ZZZ"),
        ("${A} tail $(Q)", "outer tail ZZZ"),
        ("no reference here: $ ( ) $", "no reference here: $ ( ) $"),
        ("trailing dollar $", "trailing dollar $"),
    ] {
        let got = expand_macros(raw, &macros, opts);
        assert_eq!(got.text, want, "expanding {raw:?}");
        assert!(!got.errored(), "{raw:?} must not report a fault");
    }
}
