//! The record layer's gate on entries into the core: StreamEpics takes the
//! record before the core, and a bus answering inside a request, on the
//! thread that holds the core, must not take it again.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::TestHooks;
use stream_rs::bus::{Client, StreamIoStatus};
use stream_rs::core::StreamCoreLock;
use stream_rs::error::{capture_stderr, take_captured};

#[test]
fn only_an_entry_from_a_thread_without_the_core_passes_the_gate() {
    common::setup();
    let gated = Arc::new(AtomicUsize::new(0));
    let g = gated.clone();
    let core = StreamCoreLock::new_gated(
        "rec",
        |_| TestHooks::default(),
        Box::new(move |lock, entry| {
            g.fetch_add(1, Ordering::SeqCst);
            lock.dispatch(entry)
        }),
    );
    capture_stderr();
    core.timer_callback();
    {
        let _held = core.lock();
        core.timer_callback();
        core.write_callback(StreamIoStatus::StreamIoSuccess);
    }
    std::thread::scope(|s| {
        s.spawn(|| core.exec_callback(StreamIoStatus::StreamIoSuccess));
    });
    let out = String::from_utf8_lossy(&take_captured()).into_owned();
    assert_eq!(gated.load(Ordering::SeqCst), 2);
    // each entry on this thread still reached the core
    assert_eq!(
        out.matches("timerCallback() called unexpectedly").count(),
        2,
        "{out}"
    );
    assert_eq!(
        out.matches("writeCallback(StreamIoSuccess) called unexpectedly")
            .count(),
        1,
        "{out}"
    );
}
