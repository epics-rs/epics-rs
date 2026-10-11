//! `stateRecord.c:84-90` warns through `errlogPrintf`, so `eltc 0` before
//! `iocInit` silences it. softIoc R7.0.10 on `record(state, "T1") {}`:
//! with `eltc 1`, stderr carries the three lines pinned below; with `eltc 0`,
//! stderr is empty.
//!
//! Unix only: the console is the process stderr, reached by pointing fd 2 at a
//! file and putting it back.

#![cfg(unix)]

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::ioc_builder::IocBuilder;

const SOFTIOC: &str = "WARNING: Using deprecated record type \"state\" for record \"T1\".\n\
This record type will be removed beginning with EPICS 7.1. Please replace it\n\
by a stringin record.\n";

#[epics_macros_rs::epics_test]
async fn eltc_0_keeps_the_warning_off_the_console_and_on_the_errlog() {
    let heard = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&heard);
    epics_base_rs::runtime::log::errlog_add_listener(move |m| {
        sink.lock().expect("sink").push(m.to_string());
    });
    epics_base_rs::runtime::log::eltc(false);

    let builder = IocBuilder::new()
        .db_string("record(state, \"T1\") {}\n", &HashMap::new())
        .unwrap();
    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: nextest runs this test in its own process; nothing else holds
    // fd 2 while it is swapped.
    let saved_err = unsafe {
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let built = builder.build().await;
    epics_base_rs::runtime::log::errlog_flush();
    // SAFETY: restoring the descriptor this test replaced, before anything
    // can panic.
    unsafe {
        libc::dup2(saved_err, 2);
        libc::close(saved_err);
    }
    built.unwrap();

    let err = std::fs::read_to_string(err_sink.path()).unwrap();
    assert!(!err.contains("deprecated"), "stderr was:\n{err}");
    let log = heard.lock().expect("sink").concat();
    assert!(log.contains(SOFTIOC), "errlog heard:\n{log}");
}
