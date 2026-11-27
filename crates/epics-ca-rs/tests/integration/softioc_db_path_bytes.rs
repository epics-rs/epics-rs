//! `-d <f>` is `dbLoadRecords(optarg, …)` in C (`softMain.cpp:197`), so the
//! loader reports the path argv held, byte for byte. Here it crosses the
//! startup shell as text, through macro expansion and the splitter; every
//! byte either pass reads as structure must come out as itself. `$(X)` is
//! left out: C's loader expands a macro in the filename itself (measured,
//! softIoc R7.0.10), as this port's does.

// `softioc-rs` serves through the async CA front-end and refuses at startup
// on `exec_backend`. Unix only: Windows filenames cannot hold `"` or `<`, and
// its backslash path is `softioc_undeclared_json_link_type`'s.
#![cfg(all(tokio_backend, unix))]

use std::process::{Command, Stdio};

#[test]
fn a_db_path_reaches_the_loader_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join(r#"a\b "c" 'd' $,(e)<f>.db"#);
    std::fs::write(&db, "record(nosuchtype, \"A\") {}\n").expect("write db");
    let out = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
        .args(["--port", "0", "-d"])
        .arg(&db)
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::null())
        .output()
        .expect("run softioc-rs");

    let stderr = String::from_utf8(out.stderr).expect("utf-8 stderr");
    let db = db.display();
    assert!(
        stderr.contains(&format!(" in file \"{db}\" line 1\n")),
        "{stderr}"
    );
    assert!(
        stderr.ends_with(&format!("Failed to load '{db}'\n")),
        "{stderr}"
    );
}
