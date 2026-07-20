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
//! need a runtime-free home. This module is that home: C-parity facilities
//! built from **plain `std` threads + `Mutex`/`Condvar`**, carrying no tokio
//! dependency.

pub mod callback_executor;
pub mod delayed_timer;
pub mod scan_once;

pub use callback_executor::{
    Callback, CallbackError, CallbackHandle, CallbackPool, CallbackPriority, DEFAULT_QUEUE_SIZE,
    DEFAULT_THREADS_PER_PRIORITY, NUM_CALLBACK_PRIORITIES,
};
pub use delayed_timer::{DelayedTimer, TimerHandle};
pub use scan_once::{
    DEFAULT_ONCE_QUEUE_SIZE, OnceCallback, ScanOnceHandle, ScanOnceOverflow, ScanOnceQueue,
};
