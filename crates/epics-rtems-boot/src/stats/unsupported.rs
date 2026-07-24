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

use super::{FdUsage, MemUsage};

pub(super) fn fd_usage() -> Option<FdUsage> {
    None
}

pub(super) fn mem_usage() -> Option<MemUsage> {
    None
}
