//! The record layer's gate on entries into the core: StreamEpics takes the
//! record before the core, and a bus answering inside a request, on the
//! thread that holds the core, must not take it again. An entry made in a
//! run is dropped once that run ended; one made between runs is not.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use common::{Core, TestHooks, Value};
use stream_rs::bus::{Client, ReadInput, StreamIoStatus};
use stream_rs::core::{ProtocolResult, StartMode, StreamCoreLock};
use stream_rs::error::{capture_stderr, set_stream_debug, take_captured};
use stream_rs::protocol;

#[test]
fn only_an_entry_from_a_thread_without_the_core_passes_the_gate() {
    common::setup();
    let gated = Arc::new(AtomicUsize::new(0));
    let g = gated.clone();
    let core = StreamCoreLock::new_gated(
        "rec",
        |_| TestHooks::default(),
        Box::new(move |lock, run, entry| {
            g.fetch_add(1, Ordering::SeqCst);
            lock.dispatch(run, entry)
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

/// Where the core stands when an entry is made.
#[derive(Clone, Copy, Debug)]
enum Made {
    BeforeAnyRun,
    /// In a run waiting for its input.
    InRun,
    /// After a run that ended on its input.
    AfterRun,
    AfterAbort,
}

/// The core's debug output for `entry` made on a thread without the core
/// where `made`, with the next run started while the gate holds it when
/// `restart`.
fn through_the_gate(made: Made, restart: bool, entry: fn(&Core)) -> String {
    common::setup();
    // one per test process: another writing it would truncate it under us
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("gate-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("g.proto"), "p { in \"%d\"; }\n").unwrap();
    protocol::set_path(Some(dir.to_str().unwrap()));
    let gate_restart = Arc::new(AtomicBool::new(false));
    let g = gate_restart.clone();
    let core = StreamCoreLock::new_gated(
        "rec",
        |_| TestHooks {
            value: Value::Longin(0),
            ..Default::default()
        },
        Box::new(move |lock, run, entry| {
            if g.swap(false, Ordering::SeqCst) {
                lock.start_protocol(StartMode::StartNormal);
            }
            lock.dispatch(run, entry)
        }),
    );
    assert!(core.attach_bus("P", 0, ""));
    assert!(common::parse(&core, "g.proto", b"p"));
    capture_stderr();
    match made {
        Made::BeforeAnyRun => {}
        Made::InRun => assert!(core.start_protocol(StartMode::StartNormal)),
        Made::AfterRun => {
            assert!(core.start_protocol(StartMode::StartNormal));
            core.read_callback(StreamIoStatus::StreamIoEnd, ReadInput::Bytes(b"1"));
            assert!(!core.lock().running());
        }
        Made::AfterAbort => {
            assert!(core.start_protocol(StartMode::StartNormal));
            core.lock().finish_protocol(ProtocolResult::Abort);
        }
    }
    take_captured();
    gate_restart.store(restart, Ordering::SeqCst);
    set_stream_debug(1);
    let out = std::thread::scope(|s| {
        s.spawn(|| {
            capture_stderr();
            entry(&core);
            take_captured()
        })
        .join()
        .unwrap()
    });
    set_stream_debug(0);
    String::from_utf8_lossy(&out).into_owned()
}

/// An entry's name, what makes it, and the core's debug line for it.
type EntryCase = (&'static str, fn(&Core), &'static str);

const ENTRIES: [EntryCase; 2] = [
    (
        "timer",
        |core| core.timer_callback(),
        "StreamCore::timerCallback(rec)",
    ),
    (
        "read",
        |core| {
            core.read_callback(StreamIoStatus::StreamIoSuccess, ReadInput::Bytes(b"7"));
        },
        "StreamCore::readCallback(rec, StreamIoSuccess",
    ),
];

#[test]
fn an_entry_made_in_a_run_is_dropped_once_the_run_ended() {
    for (what, entry, line) in ENTRIES {
        let out = through_the_gate(Made::InRun, false, entry);
        assert_eq!(out.matches(line).count(), 1, "{what} in its run: {out}");
        let out = through_the_gate(Made::InRun, true, entry);
        assert_eq!(out.matches(line).count(), 0, "{what} after its run: {out}");
    }
}

/// C drops what comes after an abort (`Aborted`, StreamCore.cc:954), so
/// the next run does not get it either.
#[test]
fn an_entry_made_after_an_abort_is_dropped() {
    for (what, entry, line) in ENTRIES {
        for restart in [false, true] {
            let out = through_the_gate(Made::AfterAbort, restart, entry);
            assert_eq!(
                out.matches(line).count(),
                0,
                "{what}, restart {restart}: {out}"
            );
        }
    }
}

/// What comes between runs belongs to none, as an I/O Intr record's
/// early input (StreamEpics.cc:1031-1035): a run starting meanwhile does
/// not drop it.
#[test]
fn an_entry_made_between_runs_passes_a_run_start_at_the_gate() {
    for made in [Made::BeforeAnyRun, Made::AfterRun] {
        for (what, entry, line) in ENTRIES {
            for restart in [false, true] {
                let out = through_the_gate(made, restart, entry);
                assert_eq!(
                    out.matches(line).count(),
                    1,
                    "{what} {made:?}, restart {restart}: {out}"
                );
            }
        }
    }
}

/// A connect no request asked for belongs to no run
/// (AsynDriverInterface.cc:1283-1284), even made in one: a run starting
/// meanwhile does not drop it.
#[test]
fn an_asynchronous_connect_passes_a_run_start_at_the_gate() {
    let connect = |core: &Core| core.async_connect_callback(StreamIoStatus::StreamIoSuccess);
    for restart in [false, true] {
        let out = through_the_gate(Made::InRun, restart, connect);
        assert_eq!(
            out.matches("StreamCore::connectCallback(rec, StreamIoSuccess)")
                .count(),
            1,
            "restart {restart}: {out}"
        );
    }
}
