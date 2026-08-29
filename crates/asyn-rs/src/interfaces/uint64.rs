//! `asynUInt64` / `asynUInt64Array` interface traits — **Rust extension**.
//!
//! **Not in upstream asyn C.** As of 2026-05 the upstream tree only
//! ships `asynInt64.h` / `asynInt64Array.h`; the proposed `asynUInt64`
//! addition (upstream issue #231, "asynUInt64 / asynUInt64Array
//! support") is unmerged. `find $EPICS_MODULES/asyn -iname
//! 'asynUInt64*'` returns nothing.
//!
//! This module exists as a forward-looking Rust extension so drivers
//! that talk to hardware with unsigned 64-bit registers (transfer
//! counters, tick clocks, large bitmaps) avoid casting through the
//! signed `Int64` path and the resulting sign-bit truncation. Use
//! these traits when you control both sides of the wire. **Do not
//! advertise these traits to a C asyn client** — there is no
//! corresponding upstream interface ID, so any client looking up
//! `asynUInt64` against a real C `asynManager` will not find it.
//!
//! Migration plan: if upstream merges issue #231, re-align this
//! module's trait shape to the merged header.

use crate::error::AsynResult;
use crate::user::AsynUser;

/// Unsigned 64-bit integer I/O — Rust extension. See module docs.
pub trait AsynUInt64: Send + Sync {
    fn read_uint64(&mut self, user: &AsynUser) -> AsynResult<u64>;
    fn write_uint64(&mut self, user: &mut AsynUser, value: u64) -> AsynResult<()>;
    /// Driver-reported low/high bounds. Default returns the full
    /// u64 range — drivers with a known span override.
    fn get_bounds(&self, _user: &AsynUser) -> AsynResult<(u64, u64)> {
        Ok((u64::MIN, u64::MAX))
    }
}

/// Unsigned 64-bit integer array I/O — Rust extension. See module docs.
pub trait AsynUInt64Array: Send + Sync {
    fn read_uint64_array(&mut self, user: &AsynUser, max_elements: usize) -> AsynResult<Vec<u64>>;
    fn write_uint64_array(&mut self, user: &mut AsynUser, value: &[u64]) -> AsynResult<()>;
}
