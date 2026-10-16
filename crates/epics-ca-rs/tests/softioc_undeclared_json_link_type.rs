//! `softioc-rs` declares no `pva` link type, as softIoc's `.dbd` has no
//! `link("pva", …)` line, so `{pva:…}` fails the load the way softIoc
//! R7.0.10-146-g8f5015b66 fails it from the same script, whole stderr:
//!
//! ```text
//! $ softIoc st.cmd </dev/null      # st.cmd: dbLoadRecords("j.db")
//! dbJLinkInit: Link type 'pva' not found
//! ERROR: Can't set 'A3.INP' to '{pva:"REM:Z"}'  : Bad Field value
//! ERROR:  at or before ')' in path "."  file "j.db" line 2
//!
//!  2 | record(ai, "A3") { field(INP, {pva:"REM:Z"}) }
//!
//! ERROR: Failed to load 'j.db'
//! ```
//!
//! From a script `iocsh` has raised the thread to ok-to-block, so C flushes
//! the `dbJLinkInit` errlog line before `dbRecordField` prints (`errlog.c:186`).
//! Under `-d` the load runs on `_main_`, which may not block
//! (`osdThread.c:406`), so the line is left to the errlog thread and races the
//! loader's own `stderr`: measured 36 of 40 runs last, 3 before `Failed to
//! load`, 1 inside the parse error. Only its count is pinned there.

// `softioc-rs` serves through the async CA front-end and refuses at startup
// on `exec_backend`.
#![cfg(tokio_backend)]

use std::process::{Command, Stdio};

#[test]
fn an_undeclared_json_link_type_fails_the_load_as_in_soft_ioc() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("j.db"),
        "record(ai, \"A0\") {}\nrecord(ai, \"A3\") { field(INP, {pva:\"REM:Z\"}) }\n",
    )
    .expect("write db");
    std::fs::write(dir.path().join("st.cmd"), "dbLoadRecords(\"j.db\")\n").expect("write st.cmd");
    let out = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .args(["--port", "0", "st.cmd"])
        .current_dir(dir.path())
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::null())
        .output()
        .expect("run softioc-rs");

    let stderr = String::from_utf8(out.stderr).expect("utf-8 stderr");
    assert_eq!(
        stderr,
        "dbJLinkInit: Link type 'pva' not found\n\
         \x1b[31;1mERROR\x1b[0m: Can't set 'A3.INP' to '{pva:\"REM:Z\"}'  : Bad Field value\n\
         \x1b[31;1mERROR\x1b[0m:  at or before ')' in path \".\"  file \"j.db\" line 2\n\
         \n \
         2 | record(ai, \"A3\") { field(INP, {pva:\"REM:Z\"}) }\n\
         \n\
         \x1b[31;1mERROR\x1b[0m: Failed to load 'j.db'\n"
    );
}

#[test]
fn under_dash_d_the_link_type_line_is_left_to_the_errlog_thread() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("j.db");
    std::fs::write(
        &db,
        "record(ai, \"A0\") {}\nrecord(ai, \"A3\") { field(INP, {pva:\"REM:Z\"}) }\n",
    )
    .expect("write db");
    let out = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .args(["--port", "0", "-d"])
        .arg(&db)
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::null())
        .output()
        .expect("run softioc-rs");

    let stderr = String::from_utf8(out.stderr).expect("utf-8 stderr");
    const LINK: &str = "dbJLinkInit: Link type 'pva' not found\n";
    assert_eq!(stderr.matches(LINK).count(), 1, "{stderr}");
    let db = db.display();
    assert_eq!(
        stderr.replacen(LINK, "", 1),
        format!(
            "\x1b[31;1mERROR\x1b[0m: Can't set 'A3.INP' to '{{pva:\"REM:Z\"}}'  : Bad Field value\n\
             \x1b[31;1mERROR\x1b[0m:  at or before ')' in file \"{db}\" line 2\n\
             \n \
             2 | record(ai, \"A3\") {{ field(INP, {{pva:\"REM:Z\"}}) }}\n\
             \n\
             \x1b[31;1mERROR\x1b[0m: Failed to load '{db}'\n"
        )
    );
}

/// A `.dbd` loaded at run time declares `pva` and binds no support, which is
/// `dbJLinkInit`'s other refusal. softIoc R7.0.10-146-g8f5015b66, whole
/// stderr, with `dbLoadDatabase("pl.dbd")` ahead of the same load:
///
/// ```text
/// dbJLinkInit: Support for Link type 'pva' not loaded
/// ERROR: Can't set 'A3.INP' to '{pva:"REM:Z"}'  : Bad Field value
/// ...
/// ERROR: Failed to load 'j.db'
/// ```
#[test]
fn a_loaded_link_line_declares_a_type_with_no_support() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("pl.dbd"), "link(\"pva\", \"lsetPVX\")\n").expect("write dbd");
    std::fs::write(
        dir.path().join("j.db"),
        "record(ai, \"A0\") {}\nrecord(ai, \"A3\") { field(INP, {pva:\"REM:Z\"}) }\n",
    )
    .expect("write db");
    std::fs::write(
        dir.path().join("st.cmd"),
        "dbLoadDatabase(\"pl.dbd\")\ndbLoadRecords(\"j.db\")\n",
    )
    .expect("write st.cmd");
    let out = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .args(["--port", "0", "st.cmd"])
        .current_dir(dir.path())
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::null())
        .output()
        .expect("run softioc-rs");

    let stderr = String::from_utf8(out.stderr).expect("utf-8 stderr");
    assert_eq!(
        stderr,
        "dbJLinkInit: Support for Link type 'pva' not loaded\n\
         \x1b[31;1mERROR\x1b[0m: Can't set 'A3.INP' to '{pva:\"REM:Z\"}'  : Bad Field value\n\
         \x1b[31;1mERROR\x1b[0m:  at or before ')' in path \".\"  file \"j.db\" line 2\n\
         \n \
         2 | record(ai, \"A3\") { field(INP, {pva:\"REM:Z\"}) }\n\
         \n\
         \x1b[31;1mERROR\x1b[0m: Failed to load 'j.db'\n"
    );
}
