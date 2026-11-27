//! `errlog_flush` is C `errlogFlush` (`errlog.c:614-622`): it returns only
//! after the worker has handled every message logged before it, whoever logged
//! it. Each thread waits on an event of its own, created on its first flush,
//! so a flush after that allocates nothing.
#![cfg(target_os = "linux")]

use std::sync::Mutex;
use std::time::Duration;

use alloc_count::thread;
use epics_libcom_rs::runtime::log::{
    errlog_add_listener, errlog_flush, errlog_init, errlog_vprintf,
};
use epics_libcom_rs::runtime::task::{in_test_ioc_thread, test_ioc_thread};

static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn seen(text: &str) -> bool {
    SEEN.lock().unwrap().iter().any(|m| m == text)
}

#[test]
fn a_flush_returns_after_the_pass_that_handled_what_came_before_it() {
    errlog_init(0);
    // A slow listener: a flush that did not wait would find its message unseen.
    errlog_add_listener(|m| {
        std::thread::sleep(Duration::from_millis(50));
        SEEN.lock().unwrap().push(m.to_owned());
    });

    // A thread that never posted flushes for another thread's message.
    std::thread::spawn(|| errlog_vprintf(format_args!("from a poster\n")))
        .join()
        .unwrap();
    std::thread::spawn(|| {
        errlog_flush();
        assert!(seen("from a poster\n"), "a non-posting thread's flush");
    })
    .join()
    .unwrap();

    // A posting thread's flush waits for its own message.
    test_ioc_thread(|| {
        errlog_vprintf(format_args!("own 0\n"));
        errlog_flush();
        assert!(seen("own 0\n"), "the poster's own flush");
        let ((), counts) = thread::counted(|| {
            for i in 1..=100 {
                errlog_vprintf(format_args!("own {i}\n"));
                errlog_flush();
            }
        });
        assert!(in_test_ioc_thread(), "counted off an IOC thread");
        let n = counts.allocs;
        assert!(seen("own 100\n"), "the poster's last flush");
        assert_eq!(n, 0, "allocations in 100 posts and flushes after the first");
    });
}
