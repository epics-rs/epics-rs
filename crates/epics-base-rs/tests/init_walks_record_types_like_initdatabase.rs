//! `iocInit` initialises records the way C's `initDatabase` walks them:
//! `doInitRecord0` over every record, then `doResolveLinks`, then
//! `doInitRecord1` (`iocInit.c:576-578`), each walk record-type-major in
//! `recordTypeList` order and `.db` load order within a type
//! (`iterateRecords`, `iocInit.c:484-506`). softIoc R7.0.10 on the fixture
//! below, stderr:
//!
//! ```text
//! Starting iocInit
//! WARNING: Using deprecated record type "state" for record "T1".
//! This record type will be removed beginning with EPICS 7.1. Please replace it
//! by a stringin record.
//! WARNING: Using deprecated record type "state" for record "T2".
//! This record type will be removed beginning with EPICS 7.1. Please replace it
//! by a stringin record.
//! A1.SNAM ERROR function 'noSuchA1' not found
//! A2.SNAM ERROR function 'noSuchA2' not found
//! S1.SNAM ERROR function 'noSuch1' not found
//! S2.SNAM ERROR function 'noSuch2' not found
//! iocRun: All initialization complete
//! ```
//!
//! `state` reports in pass 0, `sub`/`aSub` in pass 1, so the fixture pins
//! both the pass split and the type order (`aSub` sorts before `state` and
//! `sub`), with the `.db` interleaving the three types.
//!
//! Unix only: what is asserted is the process stderr, reached by pointing fd 2
//! at a file and putting it back.

#![cfg(unix)]

use std::collections::HashMap;
use std::os::fd::AsRawFd;

use epics_base_rs::server::ioc_builder::IocBuilder;

const DB: &str = r#"
record(sub, "S1") { field(SNAM, "noSuch1") }
record(state, "T1") {}
record(aSub, "A1") { field(SNAM, "noSuchA1") }
record(sub, "S2") { field(SNAM, "noSuch2") }
record(state, "T2") {}
record(aSub, "A2") { field(SNAM, "noSuchA2") }
"#;

const SOFTIOC: [&str; 10] = [
    "WARNING: Using deprecated record type \"state\" for record \"T1\".",
    "This record type will be removed beginning with EPICS 7.1. Please replace it",
    "by a stringin record.",
    "WARNING: Using deprecated record type \"state\" for record \"T2\".",
    "This record type will be removed beginning with EPICS 7.1. Please replace it",
    "by a stringin record.",
    "A1.SNAM ERROR function 'noSuchA1' not found",
    "A2.SNAM ERROR function 'noSuchA2' not found",
    "S1.SNAM ERROR function 'noSuch1' not found",
    "S2.SNAM ERROR function 'noSuch2' not found",
];

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

#[epics_macros_rs::epics_test]
async fn init_reports_follow_c_initdatabase_order() {
    let builder = IocBuilder::new().db_string(DB, &HashMap::new()).unwrap();

    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: nextest runs this test in its own process; nothing else holds
    // fd 2 while it is swapped.
    let saved_err = unsafe {
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let built = builder.build().await;
    // SAFETY: restoring the descriptor this test replaced, before anything
    // can panic.
    unsafe {
        libc::dup2(saved_err, 2);
        libc::close(saved_err);
    }
    built.unwrap();

    let err = strip_ansi(&std::fs::read_to_string(err_sink.path()).unwrap());
    let reports: Vec<&str> = err.lines().filter(|l| SOFTIOC.contains(l)).collect();
    assert_eq!(reports, SOFTIOC, "stderr was:\n{err}");
}
