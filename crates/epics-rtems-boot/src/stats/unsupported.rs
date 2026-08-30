//! The backend for every target with no statistics source wired up — a host
//! build, and any OS this crate has not been taught to read.
//!
//! It reports [`None`] rather than zero, and that distinction is the whole
//! reason this file is a backend instead of a `#[cfg]` arm inside the funnel:
//! **zero free descriptors and zero free heap are both real readings** on a
//! target, and they are precisely the ones an operator most needs to believe.
//! devIocStats cannot express the difference — its record graph substitutes a
//! sentinel (`FD_FREE`'s `CALC` is `B>0?B-A:C` with `C = 1000`,
//! `iocStats/iocAdmin/Db/ioc.template:118-123`), so an IOC whose descriptor
//! support is missing publishes "1000 free" rather than "unknown". An `Option`
//! says the thing the sentinel cannot.

use super::{FdUsage, HeapSpace, MemUsage};

pub(super) fn fd_usage() -> Option<FdUsage> {
    None
}

pub(super) fn mem_usage() -> MemUsage {
    MemUsage::default()
}

pub(super) fn heap_space() -> Option<HeapSpace> {
    None
}

/// Silence, not a placeholder line.
///
/// The console census is read by a log scraper looking for `TASKDUMP` /
/// `STACKUSE` / `FDCENSUS` blocks. A build with no backend has nothing to put
/// in one, and printing an empty block would let a scraper count a census that
/// never happened — the same "sentinel that reads like a measurement" mistake
/// the `Option` above exists to avoid, one layer up.
pub(super) fn dump_tasks(_tag: &str) {}

pub(super) fn stack_report(_tag: &str) {}

pub(super) fn fd_census(_tag: &str) {}

/// Nothing to record for a census that prints nothing.
///
/// This one is called on every build, not only on a target — it rides
/// `enter_ioc_thread`, so a host `cargo test` reaches it once per IOC thread.
/// Empty is the whole implementation and it must stay cheap enough to be
/// uninteresting there.
pub(super) fn register_task() {}
