//! C `errlogExitHandler` (`errlog.c:523-531`) waits for the worker to write
//! out what was queued, so a line posted by a thread that may not block is on
//! the console even when the process exits right after posting it.
#![cfg(unix)]

use std::process::Command;
use std::time::Duration;

use epics_libcom_rs::runtime::log::{errlog_add_listener, errlog_init, errlog_vprintf};

const CHILD: &str = "ERRLOG_EXIT_DRAIN_CHILD";

/// The child: the worker writes the first line and then sits in the listener,
/// so the second is still queued when `exit` runs.
fn post_and_exit() -> ! {
    errlog_init(0);
    errlog_add_listener(|m| {
        if m.contains("first") {
            std::thread::sleep(Duration::from_millis(300));
        }
    });
    errlog_vprintf(format_args!("errlog exit drain: first\n"));
    errlog_vprintf(format_args!("errlog exit drain: second\n"));
    std::process::exit(0);
}

#[test]
fn a_queued_line_reaches_the_console_at_exit() {
    if std::env::var_os(CHILD).is_some() {
        post_and_exit();
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_queued_line_reaches_the_console_at_exit",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .expect("spawn child");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "child failed: {stderr}");
    assert!(
        stderr.contains("errlog exit drain: second"),
        "the exit handler must drain the queue before the process ends; stderr:\n{stderr}"
    );
}
