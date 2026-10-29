//! `IocApplication::run`'s boot console against
//! `$EPICS_BASE/bin/linux-x86_64/softIoc -d x.db` on one `ai`, whose
//! stderr is `Starting iocInit` then `iocRun: All initialization complete`.
//!
//! Unix only: what is asserted is the process console.

#![cfg(unix)]
#![allow(clippy::disallowed_methods, reason = "tests may take stderr()")]

use std::io::Write;
use std::os::fd::AsRawFd;

use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::records::ai::AiRecord;

#[epics_macros_rs::epics_test]
async fn boot_stderr_is_what_softioc_prints() {
    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: this test owns the process, and fd 2 is restored below.
    let saved = unsafe {
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let ran = IocApplication::new()
        .port(0)
        .record("X", AiRecord::default())
        .run(|_| async { Ok(()) })
        .await;
    let _ = std::io::stderr().flush();
    // SAFETY: restoring the descriptor this test replaced.
    unsafe {
        libc::dup2(saved, 2);
        libc::close(saved);
    }
    ran.expect("the IOC booted");
    assert_eq!(
        std::fs::read_to_string(err_sink.path()).expect("stderr capture"),
        "Starting iocInit\niocRun: All initialization complete\n"
    );
}
