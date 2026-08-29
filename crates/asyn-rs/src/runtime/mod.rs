//! Runtime module: promoted actors with event emission, shutdown, and supervision.
//!
//! Also re-exports async runtime primitives (`sync`, `task`, `select!`)
//! so that driver authors can use `asyn_rs::runtime::` instead of `tokio::` directly.

pub mod axis;
pub mod config;
pub mod event;
pub mod port;
pub mod supervisor;

/// Async sync primitives (channels, Notify, Mutex, etc.)
pub mod sync {
    pub use std::sync::Arc;
    pub use tokio::sync::{Mutex, Notify, RwLock, broadcast, mpsc, oneshot};
}

/// Async task utilities (spawn, sleep, interval, etc.), over the executor
/// seam rather than over tokio.
///
/// The module header above tells driver authors to reach for
/// `asyn_rs::runtime::` instead of `tokio::`, and until now that bought them
/// nothing: every item here forwarded straight to tokio, so a driver that took
/// the advice still inherited the requirement for an entered tokio runtime and
/// still died with "there is no reactor running" wherever there was none. The
/// abstraction was also unused — not one call site in this workspace, inside
/// asyn-rs or downstream, went through it, while the library spawned through
/// `tokio::spawn` directly.
///
/// The forwarding target is `epics_libcom_rs::runtime::task`, which picks
/// the executor by backend: tokio when the build has one, the process-global
/// callback pool and delayed-callback timer when it does not. Both halves have
/// to move together — a task placed on a callback band whose body then awaits
/// `tokio::time` has not been fixed, only moved — which is why `sleep`,
/// `sleep_until` and `interval` come from the same place as `spawn`.
///
/// Named at its owner rather than through `epics_base_rs`, which is only a
/// `pub use epics_libcom_rs::{net, runtime}` re-export (`lib.rs:123`) and is
/// an OPTIONAL dependency behind `epics`. This module is not gated, so
/// reaching the seam through the re-export made `--no-default-features` —
/// which is the RTEMS and VxWorks target configuration — E0433 at this line.
/// `epics-libcom-rs` is not optional, for the reason its manifest entry
/// already gives about `runtime::socket`.
pub mod task {
    use std::future::Future;

    pub use epics_libcom_rs::runtime::task::{
        Interval, Reactor, TaskHandle, interval, sleep, sleep_until, spawn_blocking, yield_now,
    };

    /// Start a task on the executor this thread is running under.
    ///
    /// `Reactor::current()` is `None` only on the hosted backend and only off
    /// a runtime, which is precisely where the `tokio::spawn` this replaces
    /// panicked from inside itself; the `expect` keeps that outcome and names
    /// the reason instead. On the exec backend the executor is process-global,
    /// so there is no `None` and no panic — that is the whole fix.
    ///
    /// A caller that must survive having no executor holds a [`Reactor`] of
    /// its own and decides for itself; this entry point is for driver code
    /// that has one by construction.
    pub fn spawn<F>(future: F) -> TaskHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        Reactor::current()
            .expect("asyn_rs::runtime::task::spawn is called from a task on an executor")
            .spawn(future)
    }
}

/// Re-export `tokio::select!` macro.
pub use tokio::select;

pub use axis::{
    AxisActions, AxisDelayRequest, AxisMotorCommand, AxisPollDirective, AxisRuntime,
    AxisRuntimeHandle, create_axis_runtime,
};
pub use config::{BackoffConfig, RuntimeConfig, SupervisionPolicy};
pub use event::RuntimeEvent;
pub use port::{PortRuntimeHandle, create_port_runtime, port_runtime_unavailable};
pub use supervisor::{SupervisionOutcome, supervise};

/// asyn-rs library code starts tasks and waits on time through
/// [`task`], never through tokio directly.
///
/// Same rule and same reason as `epics-ca-rs`'s `spawn_capability_guard` and
/// `epics-pva-rs`'s `client_native` guard: a bare `tokio::spawn` reads an
/// executor out of whichever thread happens to be running, and being right on
/// the host says nothing about the placement. The five `enum_property_post_
/// mask` / `time_series_busy_post_mask` failures under `exec_backend` were
/// exactly one such read, in `adapter.rs`.
///
/// Needles are assembled with `concat!` so this module's own text cannot
/// satisfy the check it performs.
#[cfg(test)]
mod tokio_is_not_the_executor {
    use source_guard::{Comments, production};

    /// The shapes that name one executor rather than asking for the one in
    /// force. `port_handle.rs` is deliberately not guarded: its two
    /// `try_current` calls ask tokio-only questions the seam does not answer —
    /// which runtime flavor is entered, and whether a tokio timer driver
    /// exists — and both already read `false` correctly off a tokio runtime.
    const NEEDLES: [&str; 7] = [
        concat!("tokio", "::spawn("),
        concat!("tokio::task", "::spawn("),
        concat!("tokio::time", "::sleep("),
        concat!("tokio::time", "::sleep_until("),
        concat!("tokio::time", "::interval("),
        concat!("tokio::time", "::timeout("),
        concat!("tokio::runtime::Handle", "::try_current"),
    ];

    #[test]
    fn library_tasks_go_through_the_runtime_task_seam() {
        let files: [(&str, &str); 6] = [
            ("adapter.rs", include_str!("../adapter.rs")),
            ("asyn_record/mod.rs", include_str!("../asyn_record/mod.rs")),
            (
                "transport/in_process.rs",
                include_str!("../transport/in_process.rs"),
            ),
            ("runtime/axis.rs", include_str!("axis.rs")),
            ("runtime/supervisor.rs", include_str!("supervisor.rs")),
            ("runtime/mod.rs", include_str!("mod.rs")),
        ];
        // Fail closed: if the slicer stops covering the guarded code, say so
        // rather than reporting a vacuous pass.
        let anchors = [
            ("adapter.rs", "fn property_post_receiver"),
            ("asyn_record/mod.rs", "fn register_exception_callback"),
            ("transport/in_process.rs", "fn subscribe("),
            ("runtime/axis.rs", "async fn handle_command("),
            ("runtime/supervisor.rs", "pub async fn supervise<"),
            ("runtime/mod.rs", "pub fn spawn<F>("),
        ];

        for (name, src) in files {
            let prod = production(src, Comments::Strip);
            let anchor = anchors.iter().find(|(n, _)| *n == name).unwrap().1;
            assert!(
                prod.contains(anchor),
                "{name}: production slice no longer contains `{anchor}` — the \
                 slicer stopped covering the guarded code"
            );
            for needle in NEEDLES {
                assert_eq!(
                    prod.matches(needle).count(),
                    0,
                    "{name}: library code starts tasks and waits on time through \
                     `crate::runtime::task`; found bare `{needle}`"
                );
            }
        }
    }

    /// The same rule for the integration tests, stated over the attribute
    /// rather than over a file list so a new test file is covered the day it
    /// is written.
    ///
    /// `#[epics_test]` places the body on whichever executor the build has;
    /// `#[tokio::test]` mints a tokio runtime of its own and may therefore
    /// name `tokio::time` freely. Both live in `tests/`, so the discriminator
    /// has to be read out of each file rather than declared once here — the
    /// five `enum_property_post_mask` / `time_series_busy_post_mask` failures
    /// survived the library fix precisely because the wait was on this side of
    /// the line.
    #[test]
    fn epics_test_files_wait_through_the_seam_too() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
        let mut checked = 0usize;
        for entry in std::fs::read_dir(&dir).expect("asyn-rs/tests") {
            let path = entry.expect("directory entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("test source");
            if !src.contains("epics_test") {
                continue;
            }
            checked += 1;
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            for needle in NEEDLES {
                assert_eq!(
                    src.matches(needle).count(),
                    0,
                    "tests/{name}: an `#[epics_test]` body runs on the seam's \
                     executor, which is not tokio on every backend; found bare \
                     `{needle}`"
                );
            }
        }
        assert!(checked > 0, "no #[epics_test] file found under {dir:?}");
    }
}
