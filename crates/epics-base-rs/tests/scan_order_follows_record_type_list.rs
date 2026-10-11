//! A periodic scan walks same-`PHAS` records in C's `recordTypeList` order,
//! which is name order: an IOC loads its expanded `.dbd`, whose record types
//! `dbdExpand.pl` writes sorted (`DBD/Output.pm:61`). softIoc R7.0.10 on the
//! fixture below, stdout, one period:
//!
//! ```text
//! scan-1: dbProcess of 'X3'
//! scan-1: dbProcess of 'X2'
//! scan-1: dbProcess of 'X1'
//! scan-1: dbProcess of 'X4'
//! ```
//!
//! `aSub` sorts before `aai` and `ai`; `stdRecords.dbd` include order puts it
//! after both. The context is the scan thread's name, which differs, so the pin
//! is the order of the records the lines name.
//!
//! Unix only: what is asserted is the process stdout, reached by pointing fd 1
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

    let out_sink = tempfile::NamedTempFile::new().expect("stdout capture");
    // SAFETY: nextest runs this test in its own process; nothing else holds
    // fd 1 while it is swapped.
    let saved_out = unsafe {
        let saved = libc::dup(1);
        libc::dup2(out_sink.as_file().as_raw_fd(), 1);
        saved
    };
    let owner = epics_base_rs::server::scan::ScanOwner::start(db.clone());
    epics_base_rs::runtime::task::sleep(Duration::from_millis(1500)).await;
    drop(owner);
    // SAFETY: restoring the descriptor this test replaced, before anything
    // can panic.
    unsafe {
        libc::dup2(saved_out, 1);
        libc::close(saved_out);
    }

    let out = std::fs::read_to_string(out_sink.path()).unwrap();
    let scanned: Vec<&str> = out
        .lines()
        .filter_map(|l| l.split_once(": dbProcess of '")?.1.strip_suffix('\''))
        .collect();
    assert!(scanned.len() >= 4, "stdout was:\n{out}");
    assert_eq!(scanned[..4], SOFTIOC, "stdout was:\n{out}");
}
