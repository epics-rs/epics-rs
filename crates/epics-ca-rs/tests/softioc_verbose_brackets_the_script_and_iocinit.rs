//! `-v` writes C's closing `# End <script>` and its `iocInit()` line.
//!
//! C `softMain.cpp:230-241` brackets the script in blue remarks and then,
//! *inside* the `loadedDb` gate, announces the `iocInit()` it is about to
//! call. This port could write neither: the script ran inside
//! `run_phased`, so the binary got no turn between it and the serving
//! phase, and it had no `loadedDb` gate to put the second line inside.
//! Both now come from `IocApplication::before_ioc_init`, which is that
//! turn.
//!
//! Each arm exits on its own: stdin is at EOF, so the interactive
//! `iocsh(NULL)` returns as soon as there is nothing left to do.

// Every case here drives the `softioc-rs` binary as a subprocess, and that
// binary serves through the async CA front-end, so on `exec_backend` it
// refuses at startup instead of running. `realtime-ca-ioc` is the entry
// point that brings a CA IOC up on that backend, through the blocking
// thread-per-client driver; it is a different binary with a different
// command line, so these cases follow `softioc-rs` rather than move.
#![cfg(tokio_backend)]

use std::process::{Command, Stdio};

struct Fixture {
    _dir: tempfile::TempDir,
    db: String,
    script: String,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("v.db");
    std::fs::write(&db, "record(ai, \"V:PV\") { field(VAL, \"1\") }\n").expect("write db");
    let script = dir.path().join("st.cmd");
    std::fs::write(&script, "epicsEnvSet(\"V\",\"1\")\n").expect("write script");
    Fixture {
        db: db.to_str().expect("utf-8 path").to_string(),
        script: script.to_str().expect("utf-8 path").to_string(),
        _dir: dir,
    }
}

/// Everything the binary wrote to stdout, with argv as given plus `-v`.
fn verbose_run(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .arg("-v")
        .args(args)
        // Off the network for the arm that does start a server.
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("run softioc-rs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_loaded_ioc_closes_the_script_bracket_then_announces_ioc_init() {
    let f = fixture();
    let out = verbose_run(&["--port", "0", "-d", &f.db, &f.script]);

    let begin = out
        .find(&format!("# Begin {}", f.script))
        .unwrap_or_else(|| panic!("no opening bracket in {out:?}"));
    let end = out
        .find(&format!("# End {}", f.script))
        .unwrap_or_else(|| panic!("no closing bracket in {out:?}"));
    let init = out
        .find("iocInit()")
        .unwrap_or_else(|| panic!("no iocInit() line in {out:?}"));
    assert!(begin < end, "the bracket must close after it opens");
    assert!(
        end < init,
        "C writes `# End` at `:233` and `iocInit()` at `:240`"
    );
}

/// C's `iocInit()` line lives inside the `loadedDb` gate, so a script-only
/// run announces a call it does not make.
#[test]
fn a_script_without_a_database_closes_the_bracket_and_announces_nothing() {
    let f = fixture();
    let out = verbose_run(&[&f.script]);

    assert!(
        out.contains(&format!("# End {}", f.script)),
        "the script ran, so its bracket must close: {out:?}"
    );
    assert!(
        !out.contains("iocInit()"),
        "no `-d`/`-x`, so C never calls iocInit: {out:?}"
    );
}

/// The other boundary: no script means no bracket at all, opening or
/// closing (C `softMain.cpp:229` — the whole block is under `optind<argc`).
#[test]
fn a_database_without_a_script_announces_ioc_init_and_no_bracket() {
    let f = fixture();
    let out = verbose_run(&["--port", "0", "-d", &f.db]);

    assert!(out.contains("iocInit()"), "argv loaded a database: {out:?}");
    assert!(!out.contains("# End"), "there was no script: {out:?}");
    assert!(!out.contains("# Begin"), "there was no script: {out:?}");
}
