//! `IocBuilder::db_file` is `softIoc -d`, i.e. `dbLoadRecords`: the file is
//! opened through `dbOpenFile` on the include path list and its `include`s
//! resolve there (`dbLexRoutines.c:245-286`). softIoc R7.0.10.1 (8f5015b66)
//! loads `top2.db`'s included record and fails a missing file's load with
//! `ERROR: Can't open file 'nope.db'` then `ERROR: Failed to load 'nope.db'`.
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]

use std::collections::HashMap;
use std::os::fd::AsRawFd;

use epics_base_rs::server::ioc_builder::IocBuilder;

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn db_file_resolves_its_includes_on_the_path_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("top2.db"), "include \"ok.db\"\n").expect("write");
    std::fs::write(
        dir.path().join("ok.db"),
        "record(ai, \"OK\") { field(DESC, \"ok\") }\n",
    )
    .expect("write");
    // SAFETY: the process-global env is why this test is `serial`.
    unsafe { std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path()) };
    let builder = IocBuilder::new().db_file("top2.db", &HashMap::new());
    // SAFETY: as above.
    unsafe { std::env::remove_var("EPICS_DB_INCLUDE_PATH") };
    let (db, _) = builder
        .expect("top2.db is found on the path")
        .build()
        .await
        .expect("top2.db loads");
    let rec = db.get_record("OK").expect("the included record loads");
    assert_eq!(rec.read().resolve_field("DESC").unwrap().text_g(), "ok");
}

#[epics_macros_rs::epics_test]
#[serial_test::serial(db_load_stderr)]
async fn db_file_names_a_file_it_cannot_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: the process-global env and fd 2 are why this test is `serial`.
    let saved_err = unsafe {
        std::env::set_var("EPICS_DB_INCLUDE_PATH", dir.path());
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let built = IocBuilder::new()
        .db_file("nope.db", &HashMap::new())
        .expect("an unopened file is the load's failure, not the read's")
        .build()
        .await;
    // SAFETY: restoring what this test replaced.
    unsafe {
        libc::dup2(saved_err, 2);
        libc::close(saved_err);
        std::env::remove_var("EPICS_DB_INCLUDE_PATH");
    }
    assert!(built.is_err());
    let err = std::fs::read_to_string(err_sink.path()).expect("stderr capture");
    // softIoc colours the prefix on a terminal only; strip what this port adds.
    let err = err.replace("\x1b[31;1mERROR\x1b[0m", "ERROR");
    assert_eq!(
        err,
        "ERROR: Can't open file 'nope.db'\nERROR: Failed to load 'nope.db'\n"
    );
}
