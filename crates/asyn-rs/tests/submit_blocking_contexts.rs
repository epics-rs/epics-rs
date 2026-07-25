//! Regression coverage for [`PortHandle::submit_blocking`] across caller
//! contexts.
//!
//! The port actor and the ad-core driver threads both run their futures on a
//! **current-thread** tokio runtime. A sync API that reaches for
//! `tokio::task::block_in_place` panics there ("can call blocking only when
//! running on the multi-threaded runtime"), so every blocking device-I/O call
//! made from one of the framework's own runtimes aborted its thread.
//!
//! Two contexts, deliberately distinguished:
//!
//! (a) a current-thread runtime that is **not** the target port's actor —
//!     e.g. an `ad_core_rs::runtime::run_thread_named` acquisition task.
//!     The reply is produced by the actor's own OS thread, so parking the
//!     caller is safe: it stalls only that driver's private runtime. This
//!     must SUCCEED.
//!
//! (b) the target port's **own actor thread** — a `PortDriver` method calling
//!     back into its own port. Parking here can never be serviced: the actor
//!     is the thread that would have to run the request. This must return an
//!     ERROR rather than deadlock (and rather than panic).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::port_handle::PortHandle;
use asyn_rs::request::RequestOp;
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime;
use asyn_rs::user::AsynUser;

/// Every test blocks a thread on purpose; a regression must surface as a
/// failure, never as a hung CI job.
const WATCHDOG: Duration = Duration::from_secs(10);

/// Run `f` on a scratch thread and fail if it does not finish within
/// [`WATCHDOG`]. Returns whether `f` panicked.
fn run_with_watchdog<F>(what: &str, f: F) -> Result<(), String>
where
    F: FnOnce() + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel::<std::thread::Result<()>>();
    std::thread::Builder::new()
        .name(format!("watchdog-{what}"))
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            let _ = tx.send(outcome);
        })
        .expect("spawn");
    match rx.recv_timeout(WATCHDOG) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(format!("{what} panicked")),
        Err(_) => Err(format!("{what} hung (no result within {WATCHDOG:?})")),
    }
}

// ── (a) caller on a foreign current-thread runtime ───────────────────────────

struct PlainDriver {
    base: PortDriverBase,
}

impl PlainDriver {
    fn new(name: &str) -> Self {
        let mut base = PortDriverBase::new(name, 1, PortFlags::default());
        base.create_param("VAL", ParamType::Int32).unwrap();
        Self { base }
    }
}

impl PortDriver for PlainDriver {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

/// (a) An ad-core-style driver task owns a private **current-thread** runtime
/// (`ad_core_rs::runtime::run_thread_named`) and does its device I/O through
/// the blocking API. The actor lives on its own OS thread and answers, so the
/// call must simply succeed.
///
/// On unfixed main this panics the caller's thread:
/// `can call blocking only when running on the multi-threaded runtime`.
#[test]
fn submit_blocking_from_foreign_current_thread_runtime_succeeds() {
    let (handle, _join) = create_port_runtime(PlainDriver::new("ctx_a"), RuntimeConfig::default())
        .expect("the port runtime thread must start");
    let port: PortHandle = handle.port_handle().clone();

    run_with_watchdog("ctx-a", move || {
        // Exactly what ad-core's run_thread_named builds for a driver task.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            port.write_int32_blocking(0, 0, 42)
                .expect("write from a foreign current-thread runtime must succeed");
            assert_eq!(
                port.read_int32_blocking(0, 0)
                    .expect("read from a foreign current-thread runtime must succeed"),
                42
            );
        });
    })
    .unwrap();
}

// ── (b) caller is the target port's own actor thread ─────────────────────────

/// A driver that calls back into its own port from inside a `PortDriver`
/// method — i.e. from the actor thread that would have to service the very
/// request it is submitting.
struct SelfCallingDriver {
    base: PortDriverBase,
    /// Handle to this driver's own port, installed after the runtime is built.
    self_handle: Arc<parking_lot::Mutex<Option<PortHandle>>>,
    /// Set iff the re-entrant `submit_blocking` returned an error instead of
    /// hanging or panicking.
    got_error: Arc<AtomicBool>,
}

impl PortDriver for SelfCallingDriver {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn io_read_int32(&mut self, _user: &AsynUser) -> AsynResult<i32> {
        let me = self.self_handle.lock().clone().expect("handle installed");
        // Re-entrant call into our own port, from our own actor thread.
        match me.submit_blocking(RequestOp::Int32Read, AsynUser::new(0)) {
            Ok(_) => panic!("re-entrant submit_blocking must not report success"),
            Err(_) => {
                self.got_error.store(true, Ordering::SeqCst);
                Ok(7)
            }
        }
    }
}

/// (b) A `PortDriver` method that calls `submit_blocking` on its own port must
/// get an error back. Blocking would deadlock forever (the actor cannot
/// service the request it is waiting on) and today's `block_in_place` panics
/// the actor thread instead. Neither is acceptable: the call must fail fast.
///
/// On unfixed main the actor thread panics, the reply channel is dropped, and
/// the outer call fails with "actor dropped reply channel" — the port is dead
/// from that point on.
#[test]
fn submit_blocking_from_own_actor_thread_returns_error() {
    let self_handle = Arc::new(parking_lot::Mutex::new(None));
    let got_error = Arc::new(AtomicBool::new(false));

    let mut base = PortDriverBase::new("ctx_b", 1, PortFlags::default());
    base.create_param("VAL", ParamType::Int32).unwrap();
    let driver = SelfCallingDriver {
        base,
        self_handle: self_handle.clone(),
        got_error: got_error.clone(),
    };

    let (handle, _join) = create_port_runtime(driver, RuntimeConfig::default())
        .expect("the port runtime thread must start");
    let port: PortHandle = handle.port_handle().clone();
    *self_handle.lock() = Some(port.clone());

    let got_error_probe = got_error.clone();
    run_with_watchdog("ctx-b", move || {
        // Drives io_read_int32 on the actor thread, which re-enters the port.
        let v = port
            .read_int32_blocking(0, 0)
            .expect("the outer request itself must still complete");
        assert_eq!(v, 7, "driver returned its sentinel after the inner error");
        assert!(
            got_error_probe.load(Ordering::SeqCst),
            "re-entrant submit_blocking must return an error to the driver"
        );
    })
    .unwrap();

    // The actor must still be alive and serving after refusing the re-entrant
    // call — an error, unlike a panic, does not take the port down.
    let port2: PortHandle = handle.port_handle().clone();
    run_with_watchdog("ctx-b-alive", move || {
        port2
            .write_int32_blocking(0, 0, 5)
            .expect("port must still serve requests after refusing a re-entrant call");
    })
    .unwrap();
}
