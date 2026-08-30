//! **`FLAG_SUPPRESS_WARNINGS` is a region in C `refer`, not a caller's knob.**
//!
//! C `refer` raises and lowers `handle->flags & FLAG_SUPPRESS_WARNINGS` three
//! times inside one reference, and swaps the `MAC_ENTRY` under one of them:
//!
//!   * the reference NAME is translated quiet, through the caller's own `entry`
//!     (`macCore.c:795-800`) — so a fault inside the name still fails the
//!     expansion, but writes no notice and leaves the SHORT `$(X)` placeholder;
//!   * the default's discarding first pass is quiet, through a throwaway
//!     `MAC_ENTRY dflt` (`:805-816`) — this port has no first pass, it slices
//!     the default out instead, so there is nothing to quiet;
//!   * the scoped definitions are translated quiet AND through a separate
//!     `MAC_ENTRY subs` whose `error` starts `FALSE` and is never merged back
//!     (`:820-860`) — so a fault in `,K=$(UNDEF)` neither warns nor fails.
//!
//! Everything else — the used default, a resolved value's re-scan — runs at the
//! caller's own setting through the caller's own `entry`.
//!
//! This port had one flag read straight off [`MacroExpandOptions`], so all three
//! regions were loud and all three merged their faults. Measured against
//! `softIoc` built from `~/work/epics-base` (`R7.0.10`) on the file below, with
//! `P=pval`:
//!
//! | line | C stderr | this port, before |
//! |---|---|---|
//! | `#a$(P,K=$(UNDEF))b` | nothing, and no error | one notice, expansion FAILED |
//! | `#c$(K,K=$(UNDEF))d` | one notice | two notices |
//! | `#g$($(NAMEREF))h` | one notice, names `$(NAMEREF)` | two, names `NAMEREF` then `$(NAMEREF,undefined)` |
//!
//! The first row is the one that matters beyond wording: a scoped definition
//! that mentions an undefined macro made the whole line fail, and every consumer
//! reading [`MacroExpansion::errored`] — the `.db` reader's per-line `WARNING:`,
//! `expand_includes`, iocsh, autosave's hard error — acted on a failure C does
//! not have.
//!
//! ANSI is stripped before comparing; whether the escapes are emitted has its
//! own gate. Unix only: what is captured is the process console.
//!
//! [`MacroExpandOptions`]: epics_base_rs::server::db_loader::MacroExpandOptions
//! [`MacroExpansion::errored`]: epics_base_rs::server::db_loader::MacroExpansion::errored

#![cfg(unix)]

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::db_loader::{MacroExpandOptions, expand_macros};
use epics_base_rs::server::iocsh::IocShell;

/// One line per region, each on a `#` comment so the only thing the load can
/// produce is the diagnostics. `P` is defined; `UNDEF`, `UNDEF3`, `NAMEREF` and
/// `NOPE` are not.
///
///   * line 2 — a scoped definition that is never looked up: silent in C;
///   * line 3 — the same definition, looked up. The stored value is the quiet
///     `$(UNDEF)`, and it is the OUTER lookup re-scanning it that warns once;
///   * line 4 — a USED default, which C translates loud through the outer
///     entry, so this one was never in question;
///   * line 5 — a reference name that is itself an unresolved reference;
///   * line 6 — two scoped definitions, to show the quiet is the region and not
///     the first entry in it.
const REGIONS_DB: &str = "record(ai, \"T1\") {}\n\
                          #a$(P,K=$(UNDEF))b\n\
                          #c$(K,K=$(UNDEF))d\n\
                          #e$(NOPE=$(UNDEF3))f\n\
                          #g$($(NAMEREF))h\n\
                          #i$(P,K=$(UNDEF),L=2)j\n";

/// Run one iocsh line with fd 2 pointed at a file, and give back what it wrote.
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

/// C's SGR sequences, dropped — `errlog` strips its own the same way when the
/// console is not a terminal (`errlog.c:672-681`).
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

/// C's whole answer for the five lines, byte for byte. Two of them say nothing
/// at all, and that silence is the assertion.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn only_the_regions_c_quiets_are_quiet() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("regions.db"), REGIONS_DB).expect("write the .db");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path().display().to_string()) };
    let got = stderr_of(
        "dbLoadRecords(\"regions.db\",\"P=pval\")",
        Arc::new(PvDatabase::new()),
    );
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };

    let want = "\
macLib: macro UNDEF is undefined (expanding string #c$(K,K=$(UNDEF))d
)
WARNING: 'regions.db' line 3 has undefined macros
macLib: macro UNDEF3 is undefined (expanding string #e$(NOPE=$(UNDEF3))f
)
WARNING: 'regions.db' line 4 has undefined macros
macLib: macro $(NAMEREF) is undefined (expanding string #g$($(NAMEREF))h
)
WARNING: 'regions.db' line 5 has undefined macros
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

/// The half the stderr cannot show: which expansions C calls failures, and what
/// text the quiet placeholder leaves behind.
///
/// `$(P,K=$(UNDEF))` is the row that changes a decision rather than a message.
/// C loads it — `V1.DESC` is set to `apvalb` with no complaint — where this
/// port reported a fault and every `errored()` consumer refused the line.
#[test]
fn a_fault_inside_a_scoped_definition_does_not_fail_the_expansion() {
    let macros = HashMap::from([("P".to_string(), "pval".to_string())]);
    for (raw, text, errored) in [
        // Region: scoped definition, never looked up. C: silent, loads.
        ("a$(P,K=$(UNDEF))b", "apvalb", false),
        // Two definitions — the quiet covers the region, not just the first.
        ("i$(P,K=$(UNDEF),L=2)j", "ipvalj", false),
        // Looked up: the stored value is the QUIET `$(UNDEF)`, and the outer
        // re-scan of it is what fails, with the loud placeholder.
        ("c$(K,K=$(UNDEF))d", "c$(UNDEF,undefined)d", true),
        // A used default is translated through the outer entry, loud.
        ("e$(NOPE=$(UNDEF3))f", "e$(UNDEF3,undefined)f", true),
        // A name that is itself unresolved: the inner placeholder is the SHORT
        // form, because the name translation is the quiet region.
        ("g$($(NAMEREF))h", "g$($(NAMEREF),undefined)h", true),
    ] {
        let got = expand_macros(raw, &macros, MacroExpandOptions::default());
        assert_eq!(got.text, text, "text of {raw:?}");
        assert_eq!(got.errored(), errored, "errored() of {raw:?}");
    }
}
