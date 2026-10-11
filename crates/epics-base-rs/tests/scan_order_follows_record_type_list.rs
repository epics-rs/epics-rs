//! A periodic scan walks same-`PHAS` records in C's `recordTypeList` order,
//! which is name order: an IOC loads its expanded `.dbd`, whose record types
//! `dbdExpand.pl` writes sorted (`DBD/Output.pm:61`). softIoc R7.0.10 on the
//! fixture below, stderr, one period:
//!
//! ```text
//! scan-1: dbProcess of 'X3'
//! scan-1: dbProcess of 'X2'
//! scan-1: dbProcess of 'X1'
//! scan-1: dbProcess of 'X4'
//! ```
//!
//! `aSub` sorts before `aai` and `ai`; `stdRecords.dbd` include order puts it
//! after both. The port's TPRO line is not C's text, so the pin is the order of
//! the records it names.
//!
//! Unix only: what is asserted is the process stderr, reached by pointing fd 2
//! at a file and putting it back.

#![cfg(unix)]

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &str = r#"
record(ai, "X1") { field(SCAN, "1 second") field(TPRO, "1") }
record(aai, "X2") { field(SCAN, "1 second") field(TPRO, "1") }
record(aSub, "X3") { field(SCAN, "1 second") field(TPRO, "1") }
record(ai, "X4") { field(SCAN, "1 second") field(TPRO, "1") }
"#;

const SOFTIOC: [&str; 4] = ["X3", "X2", "X1", "X4"];

#[epics_macros_rs::epics_test]
async fn same_phas_records_scan_in_record_type_list_order() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: nextest runs this test in its own process; nothing else holds
    // fd 2 while it is swapped.
    let saved_err = unsafe {
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1500)).await;
    drop(owner);
    // SAFETY: restoring the descriptor this test replaced, before anything
    // can panic.
    unsafe {
        libc::dup2(saved_err, 2);
        libc::close(saved_err);
    }

    let err = std::fs::read_to_string(err_sink.path()).unwrap();
    let scanned: Vec<&str> = err
        .lines()
        .filter_map(|l| l.strip_prefix("[TPRO] ")?.split(':').next())
        .collect();
    assert!(scanned.len() >= 4, "stderr was:\n{err}");
    assert_eq!(scanned[..4], SOFTIOC, "stderr was:\n{err}");
}
