//! RTEMS-side background-execution infrastructure (CA sans-io refactor,
//! increment W3a — the seam backend).
//!
//! # Why this exists (decision A2)
//!
//! The CA server is being made runnable on RTEMS (armv7-rtems-eabihf) with
//! **one** async engine. On a hosted target, async *tails* — PACT device
//! completion, FLNK/scanOnce chains, SDLY/ODLY/watchdog timers, WRITE_NOTIFY
//! completion — run as tokio tasks via [`crate::runtime::task::spawn`]. RTEMS
//! has no tokio runtime (`tokio::spawn`/`tokio::time` need one), so those tails
//! need a runtime-free home. This module is that home: three C-parity
//! facilities built from **plain `std` threads + `Mutex`/`Condvar`**, carrying
//! no tokio dependency.
//!
//! | Facility | C source | Rust |
//! |----------|----------|------|
//! | [`callback_executor`] | `callback.c` `callbackQueue[]`/`callbackTask` | priority-banded worker pool |
//! | [`delayed_timer`] | `callback.c` `callbackRequestDelayed`/`timerQueue` | one deadline-ordered timer thread |
//! | [`scan_once`] | `dbScan.c` `onceQ`/`scanOnce`/`onceTask` | bounded ring + one worker |
//!
//! # The seam handle
//!
//! [`BackgroundExecutor`] owns the three facilities and their threads;
//! [`BackgroundExecutor::handle`] hands out a cheap, clonable
//! [`BackgroundHandle`] that the future seam wiring routes synchronous-tail
//! hand-offs through. The hosted (tokio) build keeps calling
//! [`crate::runtime::task::spawn`] — this module is **only** the RTEMS route,
//! and this increment adds the infrastructure without switching any call site
//! over to it (that is a later increment).

pub mod callback_executor;
pub mod delayed_timer;
// `pub`, not `pub(crate)`: the record system's periodic-scan threads
// (`epics_base_rs::server::scan`) and the database write gate run under the
// same poison-recovery and panic-isolation rules as the facilities here, and
// they call [`facility::recover`] / [`facility::run_isolated`] to get them.
// Those call sites were inside this crate before the runtime layer was split
// out; the split moved the callers across a crate boundary, and one shared
// helper reached through a wider door is what keeps the rule single-sourced
// rather than restated in the crate above.
pub mod facility;
pub mod future_exec;
pub mod scan_once;
pub mod timer_sleep;

use std::time::Duration;

pub use callback_executor::{
    Callback, CallbackError, CallbackHandle, CallbackPool, CallbackPriority, DEFAULT_QUEUE_SIZE,
    DEFAULT_THREADS_PER_PRIORITY, NUM_CALLBACK_PRIORITIES,
};
pub use delayed_timer::{DelayedTimer, TimerHandle};
pub use future_exec::{
    AbortHandle, DEFAULT_SPAWN_PRIORITY, JoinError, JoinFuture, spawn_blocking_on, spawn_future,
};
pub use timer_sleep::{Sleep, TimerInterval};

pub use scan_once::{
    DEFAULT_ONCE_QUEUE_SIZE, OnceCallback, ScanOnceHandle, ScanOnceOverflow, ScanOnceQueue,
};

/// Owns the three background facilities (callback pool, delayed timer, scanOnce
/// worker) and every thread backing them. Constructing it starts the threads;
/// dropping it stops and joins them.
///
/// The delayed timer fires its due callbacks into the callback pool, exactly as
/// C's `notify` routes an expired `epicsTimer` back through `callbackRequest`
/// (`callback.c:404-419`).
pub struct BackgroundExecutor {
    callbacks: CallbackPool,
    timer: DelayedTimer,
    scan_once: ScanOnceQueue,
}

impl BackgroundExecutor {
    /// Start all three facilities with their C-default sizing.
    pub fn new() -> Self {
        let callbacks = CallbackPool::new();
        let timer = DelayedTimer::new(callbacks.handle());
        let scan_once = ScanOnceQueue::new();
        BackgroundExecutor {
            callbacks,
            timer,
            scan_once,
        }
    }

    /// A cheap, clonable handle over all three facilities — the seam route for
    /// RTEMS synchronous-tail hand-offs.
    pub fn handle(&self) -> BackgroundHandle {
        BackgroundHandle {
            callbacks: self.callbacks.handle(),
            timer: self.timer.handle(),
            scan_once: self.scan_once.handle(),
        }
    }

    /// The callback executor pool.
    pub fn callbacks(&self) -> &CallbackPool {
        &self.callbacks
    }

    /// The delayed-callback timer.
    pub fn timer(&self) -> &DelayedTimer {
        &self.timer
    }

    /// The `scanOnce` facility.
    pub fn scan_once(&self) -> &ScanOnceQueue {
        &self.scan_once
    }
}

impl Default for BackgroundExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// Cheap, clonable submission side of a [`BackgroundExecutor`]. Every method
/// enqueues and returns immediately; the work runs on a background thread.
///
/// This is the type the sans-io seam routes RTEMS tail hand-offs into — the
/// runtime-free counterpart of [`crate::runtime::task::spawn`].
#[derive(Clone)]
pub struct BackgroundHandle {
    callbacks: CallbackHandle,
    timer: TimerHandle,
    scan_once: ScanOnceHandle,
}

impl BackgroundHandle {
    /// Enqueue an immediate callback on `priority` — C `callbackRequest`
    /// (`callback.c:341`).
    pub fn callback(&self, priority: CallbackPriority, cb: Callback) -> Result<(), CallbackError> {
        self.callbacks.request(priority, cb)
    }

    /// Enqueue a callback to run after `delay` on `priority` — C
    /// `callbackRequestDelayed` (`callback.c:410`).
    pub fn callback_delayed(&self, delay: Duration, priority: CallbackPriority, cb: Callback) {
        self.timer.schedule(delay, priority, cb);
    }

    /// Enqueue a one-shot record-process tail — C `scanOnce` (`dbScan.c:664`).
    pub fn scan_once(&self, cb: OnceCallback) -> Result<(), ScanOnceOverflow> {
        self.scan_once.scan_once(cb)
    }

    /// The callback-submission handle alone.
    pub fn callbacks(&self) -> &CallbackHandle {
        &self.callbacks
    }

    /// The delayed-timer handle alone.
    pub fn timer(&self) -> &TimerHandle {
        &self.timer
    }

    /// The scanOnce-submission handle alone.
    pub fn scan_once_handle(&self) -> &ScanOnceHandle {
        &self.scan_once
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const T: Duration = Duration::from_secs(5);

    #[test]
    fn executor_routes_all_three_facilities() {
        let exec = BackgroundExecutor::new();
        let h = exec.handle();

        let (tx, rx) = mpsc::channel();
        let tx_cb = tx.clone();
        h.callback(
            CallbackPriority::Medium,
            Box::new(move || tx_cb.send("cb").unwrap()),
        )
        .unwrap();
        let tx_once = tx.clone();
        h.scan_once(Box::new(move || tx_once.send("once").unwrap()))
            .unwrap();
        h.callback_delayed(
            Duration::from_millis(20),
            CallbackPriority::High,
            Box::new(move || tx.send("delayed").unwrap()),
        );

        let mut seen = std::collections::HashSet::new();
        for _ in 0..3 {
            seen.insert(rx.recv_timeout(T).unwrap());
        }
        assert!(seen.contains("cb"));
        assert!(seen.contains("once"));
        assert!(seen.contains("delayed"));
    }
}
