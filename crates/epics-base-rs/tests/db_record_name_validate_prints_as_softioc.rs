//! C `dbRecordNameValidate` (`dbLexRoutines.c:1085-1119`) as softIoc
//! R7.0.10.1 (8f5015b66) prints it, `dbLoadRecords("<file>")` then `dbl`.
//! The empty name is `yyerrorAbort` given its own `ERROR:` text, a bad
//! character an `fprintf` before `yyerrorAbort(NULL)`, and the two warnings
//! plain stderr lines; an abort keeps every record above it.
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::CommandOutcome;

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

/// Write `body` as `name` in a fresh directory, load it into a fresh
/// database, and give back that database, the load's stderr, and the
/// directory C would have echoed in its ` in path "…"` clause.
fn load(name: &str, body: &str) -> (Arc<PvDatabase>, String, String) {
    let (db, err, dir, _) = load_status(name, body);
    (db, err, dir)
}

/// [`load`], plus whether `dbLoadRecords` failed, the status C's
/// `iocshSetError` hands `on error` (`dbIocRegister.c:73`).
fn load_status(name: &str, body: &str) -> (Arc<PvDatabase>, String, String, bool) {
    use std::os::fd::AsRawFd;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join(name), body).expect("write the .db");
    let found_under = dir.path().display().to_string();
    let db = Arc::new(PvDatabase::new());

    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: the process-global env and fd 2 are why this test is `serial`;
    // nothing else holds either while they are swapped.
    let saved_err = unsafe {
        std::env::set_var("EPICS_DB_INCLUDE_PATH", &found_under);
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };

    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let line = format!("dbLoadRecords(\"{name}\")");
    let shell_db = Arc::clone(&db);
    let ran = std::thread::spawn(move || {
        // A script line runs inside `iocshBody`, which may block (`iocsh.cpp:1122`).
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        IocShell::new(shell_db, bridge).execute_line(&line)
    })
    .join();

    // Restore BEFORE anything can panic, or the failure report has nowhere
    // to go.
    // SAFETY: restoring the descriptor this call replaced.
    unsafe {
        libc::dup2(saved_err, 2);
        libc::close(saved_err);
        std::env::remove_var("EPICS_DB_INCLUDE_PATH");
    }
    let failed = matches!(
        ran.expect("the shell thread"),
        Err(_) | Ok(CommandOutcome::Failed)
    );

    (
        db,
        strip_ansi(&std::fs::read_to_string(err_sink.path()).expect("stderr capture")),
        found_under,
        failed,
    )
}

/// The names `dbl` would list, in load order.
async fn listed(db: &PvDatabase) -> Vec<String> {
    let mut names: Vec<String> = db
        .all_record_names()
        .iter()
        .map(|n| n.to_string())
        .collect();
    names.sort();
    names
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn an_unresolved_macro_in_a_record_name_is_a_bad_character() {
    let (db, err, dir) = load(
        "dollar.db",
        "record(ai,\"U:A0\"){ }\nrecord(stringin,\"U:S$(IDX)\"){ }\nrecord(ai,\"U:A9\"){ }\n",
    );
    assert_eq!(
        err,
        format!(
            "macLib: macro IDX is undefined (expanding string record(stringin,\"U:S$(IDX)\"){{ }}
)
WARNING: 'dollar.db' line 2 has undefined macros
ERROR: Bad character '$' in Record/Alias name \"U:S$(IDX,undefined)\"
ERROR:  at or before ')' in path \"{dir}\"  file \"dollar.db\" line 2

 2 | record(stringin,\"U:S$(IDX,undefined)\"){{ }}

ERROR: Failed to load 'dollar.db'
"
        )
    );
    assert_eq!(listed(&db).await, ["U:A0"]);
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn an_empty_record_name_is_refused_with_its_own_error_text() {
    let (db, err, dir) = load(
        "empty.db",
        "record(ai,\"U:A0\"){ }\nrecord(ai,\"\")\nrecord(ai,\"U:A9\"){ }\n",
    );
    assert_eq!(
        err,
        format!(
            "ERROR: ERROR: Record/Alias name can't be empty
 at or before ')' in path \"{dir}\"  file \"empty.db\" line 2

 2 | record(ai,\"\")

ERROR: Failed to load 'empty.db'
"
        )
    );
    assert_eq!(listed(&db).await, ["U:A0"]);
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_bad_alias_name_keeps_the_record_it_is_in() {
    let (db, err, dir) = load(
        "alias.db",
        "record(ai,\"U:A0\"){ }\nrecord(ai,\"q\"){ alias(\"x.y\") }\nrecord(ai,\"U:A9\"){ }\n",
    );
    assert_eq!(
        err,
        format!(
            "ERROR: Bad character '.' in Record/Alias name \"x.y\"
ERROR:  at or before ')' in path \"{dir}\"  file \"alias.db\" line 2

 2 | record(ai,\"q\"){{ alias(\"x.y\") }}

ERROR: Failed to load 'alias.db'
"
        )
    );
    assert_eq!(listed(&db).await, ["U:A0", "q"]);
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_name_c_warns_about_still_loads() {
    let (db, err, _) = load(
        "warn.db",
        "record(ai,\"-X\"){ }\nrecord(ai,\"A\u{1}B\"){ }\nrecord(ai,\"U:A9\"){ }\n",
    );
    assert_eq!(
        err,
        "WARNING: Record/Alias name '-X' should not begin with '-'
WARNING: Record/Alias name 'A\u{1}B' contains non-printable 0x01
"
    );
    assert_eq!(listed(&db).await, ["-X", "A\u{1}B", "U:A9"]);
}

/// `dbAlias` validates the new name, then the target, stopping at the first
/// refusal (`dbLexRoutines.c:1505`). softIoc with `on error break` stops the
/// script after this load and exits 2.
#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn a_file_alias_validates_its_target_after_its_new_name() {
    let (db, err, dir, failed) = load_status(
        "fa.db",
        "record(ai,\"U:A0\"){ }\nalias(\"a b\",\"Z\")\nrecord(ai,\"U:A9\"){ }\n",
    );
    assert_eq!(
        err,
        format!(
            "ERROR: Bad character ' ' in Record/Alias name \"a b\"
ERROR:  at or before ')' in path \"{dir}\"  file \"fa.db\" line 2

 2 | alias(\"a b\",\"Z\")

ERROR: Failed to load 'fa.db'
"
        )
    );
    assert!(failed, "dbLoadRecords reports the failure to `on error`");
    assert_eq!(listed(&db).await, ["U:A0"]);

    let (db, err, dir, failed) = load_status(
        "fa2.db",
        "record(ai,\"-T\"){ }\nalias(\"-T\",\"+Z\")\nalias(\"x.y\",\"q z\")\n",
    );
    assert_eq!(
        err,
        format!(
            "WARNING: Record/Alias name '-T' should not begin with '-'
WARNING: Record/Alias name '+Z' should not begin with '+'
WARNING: Record/Alias name '-T' should not begin with '-'
ERROR: Bad character ' ' in Record/Alias name \"q z\"
ERROR:  at or before ')' in path \"{dir}\"  file \"fa2.db\" line 3

 3 | alias(\"x.y\",\"q z\")

ERROR: Failed to load 'fa2.db'
"
        )
    );
    assert!(failed);
    assert!(db.get_record("+Z").is_some(), "softIoc's dbl lists +Z");
}
