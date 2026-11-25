//! `--db-file` and `--macro` reach `dbLoadRecords` byte for byte, though they
//! cross the startup shell as text through macro expansion and the splitter.
//! Unix only: Windows filenames cannot hold `"` or `<`.
#![cfg(all(feature = "qsrv-bin", unix))]

use std::process::{Command, Stdio};

#[test]
fn a_db_path_and_its_macros_reach_the_loader_unchanged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join(r#"a\b "c" 'd' $,(e)<f>.db"#);
    // A space in a record name is the loader's error, which names the file.
    std::fs::write(&db, "record(ai, \"$(P)A\") {}\n").expect("write db");
    let out = Command::new(env!("CARGO_BIN_EXE_qsrv-rs"))
        .args(["--port", "0", "--db-file"])
        .arg(&db)
        .args(["--macro", "P=x y"])
        .stdin(Stdio::null())
        .output()
        .expect("run qsrv-rs");

    let stderr = String::from_utf8(out.stderr).expect("utf-8 stderr");
    assert!(stderr.contains("Record/Alias name \"x yA\""), "{stderr}");
    assert!(
        stderr.contains(&format!(" in file \"{}\" line 1\n", db.display())),
        "{stderr}"
    );
}
