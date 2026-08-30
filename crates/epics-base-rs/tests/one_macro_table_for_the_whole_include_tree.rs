//! **One macro table for a whole `.db` read, includes and all — and every
//! line goes through it before anything reads what the line says.**
//!
//! C `dbReadCOM` creates a single `MAC_HANDLE`, installs the caller's
//! macros into it and reads `dbQuietMacroWarnings` into it once, at open
//! (`dbLexRoutines.c:256-300`). `dbIncludeNew` pushes the included file
//! onto `inputFileList` and never touches the handle (`:443-461`), and
//! `db_yyinput` expands each `fgets` line through it before the lexer
//! sees a character (`:375-391`). msi is built the same way: one
//! `macCreateHandle` for the run (`msi.cpp:111`), no scope around an
//! `include`.
//!
//! This port built a `MacroTable` per LINE inside `expand_includes_inner`
//! and kept a per-file `HashMap` copy of the macros beside it. Four things
//! followed, all measured against `softIoc`/`msi` built from
//! `~/work/epics-base` (`R7.0.10`):
//!
//! | shape | C | this port, before |
//! |---|---|---|
//! | a faulty definition, 3 lines referring to it | one `macLib:` notice | three |
//! | `substitute` inside an included file | outlives the include | reverted at its end |
//! | `substitute "X=$(Y)"` then `substitute "Y=2"` | `X` is `2` | `X` is `1` |
//! | `include "$(NOPE)b.db"` | notice AND `WARNING:` for the line | notice only |
//!
//! Unix only for the notice half: what is captured is the process console,
//! and the only way to capture it is to point fd 2 somewhere else and put
//! it back.

#![cfg(unix)]

use std::collections::HashMap;
use std::path::Path;

use epics_base_rs::server::db_loader::{DbFaults, DbLoadConfig, MacroDefs, expand_includes};

/// Expand an include tree with fd 2 pointed at a file, and give back both
/// the flattened text and everything the read wrote to the console.
fn expand_and_notices(path: &Path, macros: impl Into<MacroDefs>) -> (String, String) {
    let sink = tempfile::NamedTempFile::new().expect("capture file");
    let saved = unsafe { libc::dup(2) };
    assert!(saved >= 0, "dup(2) failed");
    let fd = {
        use std::os::fd::AsRawFd;
        sink.as_file().as_raw_fd()
    };
    assert!(unsafe { libc::dup2(fd, 2) } >= 0, "dup2 onto fd 2 failed");

    let config = DbLoadConfig {
        include_paths: vec![path.parent().expect("a parent directory").to_path_buf()],
        ..DbLoadConfig::default()
    };
    let macros = macros.into();
    let ran = std::panic::catch_unwind(move || {
        expand_includes(path, &macros, &config, &mut DbFaults::default())
    });

    // Restore BEFORE anything can panic on the assertion, or the failure
    // report has nowhere to go.
    assert!(unsafe { libc::dup2(saved, 2) } >= 0, "restore fd 2 failed");
    unsafe { libc::close(saved) };

    let text = ran.expect("the expansion").expect("the flattened text");
    let notices = strip_ansi(&std::fs::read_to_string(sink.path()).expect("read the capture"));
    (text, notices)
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

fn write(dir: &tempfile::TempDir, name: &str, text: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).expect("write the fixture");
    path
}

/// **Boundary: one faulty definition against many lines that read it.**
///
/// The notice belongs to the table pass, which runs once; the `WARNING:`
/// belongs to the line, which is read three times. Measured on `softIoc`
/// with `A="$(NOPE)"` delivered raw through a `.substitutions` row:
///
/// ```text
/// macLib: macro NOPE is undefined (expanding macro A)
/// WARNING: 'once.db' line 2 has undefined macros
/// WARNING: 'once.db' line 3 has undefined macros
/// WARNING: 'once.db' line 4 has undefined macros
/// ```
#[test]
#[serial_test::serial(db_load_stderr)]
fn a_faulty_definition_is_announced_once_for_the_file_and_the_lines_warn_each() {
    let dir = tempfile::tempdir().expect("tempdir");
    let main = write(
        &dir,
        "once.db",
        "record(ai, \"T1\") {}\n#$(A)\n#$(A)\n#$(A)\n",
    );
    let mut defs = MacroDefs::new();
    defs.put("A", "$(NOPE)");

    let (_, notices) = expand_and_notices(&main, &defs);
    let want = format!(
        "\
macLib: macro NOPE is undefined (expanding macro A)
WARNING: '{f}' line 2 has undefined macros
WARNING: '{f}' line 3 has undefined macros
WARNING: '{f}' line 4 has undefined macros
",
        f = main.display()
    );
    assert_eq!(
        notices, want,
        "\n--- got ---\n{notices}\n--- want ---\n{want}"
    );
}

/// **Boundary: a `substitute` made inside an included file, after the
/// include returns.**
///
/// One handle means the definition survives. Measured with `msi -M
/// X=outer -I .` over an outer file that reads `$(X)` before and after
/// including a file whose only content is `substitute "X=inner"`:
///
/// ```text
/// before:[outer]
/// in:[inner]
/// after:[inner]
/// ```
#[test]
#[serial_test::serial(db_load_stderr)]
fn a_substitute_inside_an_include_outlives_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir, "inner.db", "substitute \"X=inner\"\nin:[$(X)]\n");
    let main = write(
        &dir,
        "outer.db",
        "before:[$(X)]\ninclude \"inner.db\"\nafter:[$(X)]\n",
    );
    let mut defs = MacroDefs::new();
    defs.put("X", "outer");

    let (text, _) = expand_and_notices(&main, &defs);
    // The `substitute` line contributes an empty line, which is how this
    // reader keeps a diagnostic's line number the operator's; the
    // `include` line contributes the included file instead.
    assert_eq!(text, "before:[outer]\n\nin:[inner]\nafter:[inner]\n");
}

/// **Boundary: a `substitute` value that mentions a macro redefined
/// later.**
///
/// msi `addMacroReplacements` installs what `macParseDefns` cut out and
/// nothing more (`msi.cpp:256-275`), so the value stays RAW and follows
/// the later definition. Measured with `msi -M Y=1`:
///
/// ```text
/// substitute "X=$(Y)"
/// substitute "Y=2"
/// X=[$(X)]        ->   X=[2]
/// ```
///
/// This port expanded the value at the directive, which froze `X` to `1`.
#[test]
#[serial_test::serial(db_load_stderr)]
fn a_substitute_value_is_installed_raw_and_stays_live() {
    let dir = tempfile::tempdir().expect("tempdir");
    let main = write(
        &dir,
        "live.db",
        "substitute \"X=$(Y)\"\nsubstitute \"Y=2\"\nX=[$(X)]\n",
    );
    let mut defs = MacroDefs::new();
    defs.put("Y", "1");

    let (text, _) = expand_and_notices(&main, &defs);
    assert_eq!(text, "\n\nX=[2]\n");
}

/// **Boundary: the directive keyword itself comes from a macro.**
///
/// C expands the line and hands the result to the lexer, so `include` is
/// read from the EXPANDED text. Measured on `softIoc` with
/// `dbLoadRecords("ka.db","INC=include")` over a file whose second line is
/// `$(INC) "kb.db"`: byte-identical to the same file written with a
/// literal `include`, down to the diagnostics raised inside `kb.db`.
#[test]
#[serial_test::serial(db_load_stderr)]
fn the_directive_keyword_is_read_from_the_expanded_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(&dir, "kb.db", "included\n");
    let from_macro = write(&dir, "ka.db", "record(ai, \"T1\") {}\n$(INC) \"kb.db\"\n");
    let literal = write(&dir, "kl.db", "record(ai, \"T1\") {}\ninclude \"kb.db\"\n");
    let mut defs = MacroDefs::new();
    defs.put("INC", "include");

    let (from_macro, _) = expand_and_notices(&from_macro, &defs);
    let (literal, _) = expand_and_notices(&literal, &defs);
    assert_eq!(from_macro, "record(ai, \"T1\") {}\nincluded\n");
    assert_eq!(from_macro, literal);
}

/// **Boundary: an `include` line whose own expansion failed.**
///
/// It is a line like any other, so it carries both per-line diagnostics.
/// Measured on `softIoc` with `dbLoadRecords("inca.db")` over a file whose
/// second line is `include "$(NOPE)b.db"`:
///
/// ```text
/// macLib: macro NOPE is undefined (expanding string include "$(NOPE)b.db"
/// )
/// WARNING: 'inca.db' line 2 has undefined macros
/// ```
///
/// This port matched the directive on the raw line and expanded only the
/// filename, so the notice named the fragment `$(NOPE)b.db` and no
/// `WARNING:` was raised at all.
#[test]
#[serial_test::serial(db_load_stderr)]
fn an_include_line_carries_the_per_line_warning() {
    let dir = tempfile::tempdir().expect("tempdir");
    let main = write(
        &dir,
        "inca.db",
        "record(ai, \"T1\") {}\ninclude \"$(NOPE)b.db\"\n",
    );

    let (_, notices) = expand_and_notices(&main, &HashMap::new());
    let head = format!(
        "\
macLib: macro NOPE is undefined (expanding string include \"$(NOPE)b.db\"
)
WARNING: '{f}' line 2 has undefined macros
",
        f = main.display()
    );
    assert!(
        notices.starts_with(&head),
        "\n--- got ---\n{notices}\n--- want prefix ---\n{head}"
    );
}
