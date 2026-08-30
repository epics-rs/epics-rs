//! **C expands the macro TABLE into cached values before it looks at the
//! caller's string, and a reference COPIES that value.**
//!
//! `macExpandString` calls `expand( handle )` first (`macCore.c:203-204`).
//! That pass walks every `MAC_ENTRY` in the table, clears its `error`, and
//! translates its raw value at level 1 under THAT entry (`:645-679`); only
//! then is the caller's string translated, under a stack entry typed
//! `"string"` whose name is the string itself (`:206-209`). A reference
//! that resolves while the table is clean copies `refentry->value` and
//! merges `refentry->error` without re-scanning anything (`:882-886`).
//!
//! Three things follow, and none of them are wording:
//!
//!   * a macro whose own value is faulty is announced ONCE, seated on the
//!     macro and not on the string, however many times the string refers
//!     to it;
//!   * the value a cycle leaves behind is the one the TABLE pass built,
//!     which is the other member of the pair — not the reference a lazy
//!     expander happens to refuse first;
//!   * anything that changes a raw value — a scoped definition, the scope
//!     pop that follows it, an environment name materialising as an entry
//!     — raises `handle->dirty` (`rawval`, `:610-619`; `delete`,
//!     `:625-643`), and every reference after it in the same string
//!     translates the raw value again, under the STRING's seat.
//!
//! Every expected byte below was captured from `softIoc` built from
//! `~/work/epics-base` (`R7.0.10`), loading a `.substitutions` that carries
//! the macro definitions raw into `dbLoadRecords` and a `.db` whose
//! `field(DESC, …)` is split across lines so the loader echoes back what it
//! refused to set. The strings passed here are those `.db` lines verbatim,
//! newline included, because that is what `macLib` quotes.
//!
//! Unix only: what is captured is the process console, and the only way to
//! capture it is to point fd 2 somewhere else and put it back.

#![cfg(unix)]

use std::collections::HashMap;

use epics_base_rs::server::db_loader::{MacroExpandOptions, MacroTable, expand_macros};

/// Expand `src` against `macros` with fd 2 pointed at a file, and give
/// back both the text and everything `macLib` wrote.
fn expand_and_notices(src: &str, macros: &HashMap<String, String>) -> (String, String) {
    let sink = tempfile::NamedTempFile::new().expect("capture file");
    let saved = unsafe { libc::dup(2) };
    assert!(saved >= 0, "dup(2) failed");
    let fd = {
        use std::os::fd::AsRawFd;
        sink.as_file().as_raw_fd()
    };
    assert!(unsafe { libc::dup2(fd, 2) } >= 0, "dup2 onto fd 2 failed");

    let expanded =
        std::panic::catch_unwind(|| expand_macros(src, macros, MacroExpandOptions::default()).text);

    // Restore BEFORE anything can panic on the assertion, or the failure
    // report has nowhere to go.
    assert!(unsafe { libc::dup2(saved, 2) } >= 0, "restore fd 2 failed");
    unsafe { libc::close(saved) };

    let notices = strip_ansi(&std::fs::read_to_string(sink.path()).expect("read the capture"));
    (expanded.expect("the expansion"), notices)
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

fn macros(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

/// **Boundary: a value whose expansion is consumed twice.**
///
/// One notice, two placeholders. The notice comes from the table pass and
/// is seated on `A`; both references copy the value that pass cached, and
/// neither translates `A`'s raw value again. A lazy expander answers with
/// two notices, one per reference, and seats both on the string.
///
/// Measured with `A="$(NOPE)"` and the line `"q[$(A)-$(A)]"`:
///
/// ```text
/// macLib: macro NOPE is undefined (expanding macro A)
/// ERROR: Can't set 'R1.DESC' to 'q[$(NOPE,undefined)-$(NOPE,undefined)]'  : Bad Field value
/// ```
#[test]
#[serial_test::serial(db_load_stderr)]
fn a_faulty_value_is_announced_once_per_table_not_once_per_reference() {
    let (text, notices) = expand_and_notices("\"q[$(A)-$(A)]\"\n", &macros(&[("A", "$(NOPE)")]));
    assert_eq!(text, "\"q[$(NOPE,undefined)-$(NOPE,undefined)]\"\n");
    assert_eq!(
        notices,
        "macLib: macro NOPE is undefined (expanding macro A)\n"
    );
}

/// **Boundary: a clean table versus a dirty one, inside one string.**
///
/// `$(Z,K=1)` pushes a scope, defines `K` in it and pops it again, and
/// both ends of that raise `handle->dirty`. So the first `$(A)` copies the
/// cached value in silence while the second, three characters later,
/// re-translates `A`'s raw value — and it does that under the STRING's
/// seat, because `refer` hands the raw translation `entry` and not
/// `refentry` (`macCore.c:888-892`). The same fault therefore reports
/// twice under two different names, which is the discriminator no
/// single-phase expander can produce.
///
/// Measured with `A="$(NOPE)", Z="z"` and the line
/// `"q[$(A)|$(Z,K=1)|$(A)]"`:
///
/// ```text
/// macLib: macro NOPE is undefined (expanding macro A)
/// macLib: macro NOPE is undefined (expanding string "q[$(A)|$(Z,K=1)|$(A)]"
/// )
/// ERROR: Can't set 'R1.DESC' to 'q[$(NOPE,undefined)|z|$(NOPE,undefined)]'  : Bad Field value
/// ```
#[test]
#[serial_test::serial(db_load_stderr)]
fn a_scope_pop_sends_the_next_reference_back_to_the_raw_value() {
    let src = "\"q[$(A)|$(Z,K=1)|$(A)]\"\n";
    let (text, notices) = expand_and_notices(src, &macros(&[("A", "$(NOPE)"), ("Z", "z")]));
    assert_eq!(text, "\"q[$(NOPE,undefined)|z|$(NOPE,undefined)]\"\n");
    assert_eq!(
        notices,
        format!(
            "macLib: macro NOPE is undefined (expanding macro A)\n\
             macLib: macro NOPE is undefined (expanding string {src})\n"
        )
    );
}

/// **Boundary: a redefinition reached after a completed expansion.**
///
/// `Z="$(A)"` is resolved by the table pass, so the pass reports `A`'s
/// fault twice — once seated on `A` and once on `Z`, which had to follow
/// it. Then the string redefines `A` inside `$(Z,A=one)`: the scoped
/// definition is visible to `Z`'s raw value, so `Z` comes out `one` and
/// not the cached `$(NOPE,undefined)`, and the pop leaves the table dirty
/// for the `$(A)` after it.
///
/// Measured with `A="$(NOPE)", Z="$(A)"` and the line
/// `"q[$(Z,A=one)|$(A)]"`:
///
/// ```text
/// macLib: macro NOPE is undefined (expanding macro A)
/// macLib: macro NOPE is undefined (expanding macro Z)
/// macLib: macro NOPE is undefined (expanding string "q[$(Z,A=one)|$(A)]"
/// )
/// ERROR: Can't set 'R1.DESC' to 'q[one|$(NOPE,undefined)]'  : Bad Field value
/// ```
///
/// The order of the first two lines is the table's order. C's is the order
/// `macPutValue` was called in; this port is handed a `HashMap` and sorts
/// by name, which agrees here and is the only thing that keeps the stream
/// reproducible at all.
#[test]
#[serial_test::serial(db_load_stderr)]
fn a_scoped_redefinition_beats_the_value_the_table_pass_cached() {
    let src = "\"q[$(Z,A=one)|$(A)]\"\n";
    let (text, notices) = expand_and_notices(src, &macros(&[("A", "$(NOPE)"), ("Z", "$(A)")]));
    assert_eq!(text, "\"q[one|$(NOPE,undefined)]\"\n");
    assert_eq!(
        notices,
        format!(
            "macLib: macro NOPE is undefined (expanding macro A)\n\
             macLib: macro NOPE is undefined (expanding macro Z)\n\
             macLib: macro NOPE is undefined (expanding string {src})\n"
        )
    );
}

/// **Boundary: a self-cycle versus a mutual one.**
///
/// Both are refused at C's per-entry `visited` guard, and the difference
/// is which entry the refusal lands in. `A="$(A)"` closes on itself, so one notice
/// and `$(A,recursive)`. `A="$(B)", B="$(A)"` closes one step further out:
/// the pass expanding `A` follows `$(B)` and refuses the `$(A)` inside it,
/// so `A`'s cached value — and therefore the loaded value — names B, and
/// the pass then expands `B` and reports the mirror image.
///
/// Measured, one `softIoc` run each:
///
/// ```text
/// macLib: macro A is recursive (expanding macro A)
/// ERROR: Can't set 'R1.DESC' to 'q[$(A,recursive)]'  : Bad Field value
///
/// macLib: macro A is recursive (expanding macro B)
/// macLib: macro B is recursive (expanding macro A)
/// ERROR: Can't set 'R1.DESC' to 'q[$(B,recursive)]'  : Bad Field value
/// ```
#[test]
#[serial_test::serial(db_load_stderr)]
fn a_cycle_leaves_behind_the_entry_the_table_pass_refused() {
    let (text, notices) = expand_and_notices("\"q[$(A)]\"\n", &macros(&[("A", "$(A)")]));
    assert_eq!(text, "\"q[$(A,recursive)]\"\n");
    assert_eq!(
        notices,
        "macLib: macro A is recursive (expanding macro A)\n"
    );

    let (text, notices) =
        expand_and_notices("\"q[$(A)]\"\n", &macros(&[("A", "$(B)"), ("B", "$(A)")]));
    assert_eq!(text, "\"q[$(B,recursive)]\"\n");
    assert_eq!(
        notices,
        "macLib: macro A is recursive (expanding macro B)\n\
         macLib: macro B is recursive (expanding macro A)\n"
    );
}

/// **Boundary: an undefined name reached through a macro value versus one
/// written in the string.**
///
/// Same fault, same placeholder, different seat — and the seat is the
/// whole of what tells an operator which of the two it is. Measured, one
/// `softIoc` run each:
///
/// ```text
/// macLib: macro NOPE is undefined (expanding macro A)
/// macLib: macro NOPE is undefined (expanding string "q[$(NOPE)]"
/// )
/// ```
///
/// Both loads refuse the same `q[$(NOPE,undefined)]`.
#[test]
#[serial_test::serial(db_load_stderr)]
fn an_undefined_name_names_the_entry_it_was_reached_through() {
    let (text, notices) = expand_and_notices("\"q[$(A)]\"\n", &macros(&[("A", "$(NOPE)")]));
    assert_eq!(text, "\"q[$(NOPE,undefined)]\"\n");
    assert_eq!(
        notices,
        "macLib: macro NOPE is undefined (expanding macro A)\n"
    );

    let src = "\"q[$(NOPE)]\"\n";
    let (text, notices) = expand_and_notices(src, &macros(&[("Z", "z")]));
    assert_eq!(text, "\"q[$(NOPE,undefined)]\"\n");
    assert_eq!(
        notices,
        format!("macLib: macro NOPE is undefined (expanding string {src})\n")
    );
}

/// **Boundary: an unterminated reference in a macro value versus one in
/// the string.**
///
/// The arm copies from the `$` to the end of whatever it was translating
/// and writes no placeholder (`macCore.c:862-875`), so in a macro value it
/// consumes the rest of THAT value and in a string the rest of the line.
///
/// Measured with `A="$(P "` and the line `"q$(A)"`:
///
/// ```text
/// macLib: unterminated macro reference in macro A
///  3 | "q$(P ""
/// ```
///
/// which is also the sharpest confirmation that the copy runs to the end
/// and stops nowhere else: `macParseDefns` had already translated the
/// substitution's own quoting at level 1 and left `A`'s raw value as
/// `$(P "`, trailing quote and all, and every byte of it came through.
/// The raw value is set directly here, so the expected text is that same
/// copy without the quote `macParseDefns` contributed.
///
/// In the string, measured on the line `"q[$(P ]"`:
///
/// ```text
/// macLib: unterminated macro reference in string "q[$(P ]"
///
/// ERROR: Can't set 'R1.DESC' to 'q[$(P ]'  : Bad Field value
/// ```
#[test]
#[serial_test::serial(db_load_stderr)]
fn an_unterminated_reference_names_the_entry_it_was_reached_through() {
    let (text, notices) = expand_and_notices("\"q$(A)\"\n", &macros(&[("A", "$(P ")]));
    assert_eq!(text, "\"q$(P \"\n");
    assert_eq!(notices, "macLib: unterminated macro reference in macro A\n");

    let src = "\"q[$(P ]\"\n";
    let (text, notices) = expand_and_notices(src, &macros(&[("Z", "z")]));
    assert_eq!(text, src);
    assert_eq!(
        notices,
        format!("macLib: unterminated macro reference in string {src}\n")
    );
}

/// **Boundary: one table across many strings.**
///
/// C creates one `MAC_HANDLE` per file and expands it on first use, so a
/// macro whose value is faulty is announced once for the whole file — not
/// once per line that mentions it. [`MacroTable`] is that handle; the
/// per-call [`expand_macros`] above is the same engine for callers that
/// have one string.
///
/// Measured on a two-line `.db` under one `dbLoadRecords`: the notice
/// appears once, ahead of both `WARNING:` lines.
#[test]
#[serial_test::serial(db_load_stderr)]
fn one_table_announces_a_faulty_value_once_however_many_strings_read_it() {
    let sink = tempfile::NamedTempFile::new().expect("capture file");
    let saved = unsafe { libc::dup(2) };
    assert!(saved >= 0, "dup(2) failed");
    let fd = {
        use std::os::fd::AsRawFd;
        sink.as_file().as_raw_fd()
    };
    assert!(unsafe { libc::dup2(fd, 2) } >= 0, "dup2 onto fd 2 failed");

    let mut table = MacroTable::new(&macros(&[("A", "$(NOPE)")]), MacroExpandOptions::default());
    let first = table.expand("\"q[$(A)]\"\n");
    let second = table.expand("\"r[$(A)]\"\n");

    assert!(unsafe { libc::dup2(saved, 2) } >= 0, "restore fd 2 failed");
    unsafe { libc::close(saved) };

    assert_eq!(first.text, "\"q[$(NOPE,undefined)]\"\n");
    assert_eq!(second.text, "\"r[$(NOPE,undefined)]\"\n");
    assert!(first.errored() && second.errored(), "both must report it");
    let notices = strip_ansi(&std::fs::read_to_string(sink.path()).expect("read the capture"));
    assert_eq!(
        notices,
        "macLib: macro NOPE is undefined (expanding macro A)\n"
    );
}
