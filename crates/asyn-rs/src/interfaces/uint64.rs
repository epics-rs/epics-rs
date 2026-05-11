//! `asynUInt64` / `asynUInt64Array` interface traits.
//!
//! Mirrors asyn upstream issue #231 — adds an unsigned 64-bit integer
//! parameter type alongside the existing signed Int64. Drivers that
//! handle large unsigned counters (transfer counts, hardware tick
//! registers, big timestamps) should implement these instead of
//! casting through Int64 and risking sign-bit truncation.

use crate::error::AsynResult;
use crate::user::AsynUser;

/// Unsigned 64-bit integer I/O (asynUInt64 equivalent).
pub trait AsynUInt64: Send + Sync {
    fn read_uint64(&mut self, user: &AsynUser) -> AsynResult<u64>;
    fn write_uint64(&mut self, user: &mut AsynUser, value: u64) -> AsynResult<()>;
    /// Driver-reported low/high bounds. Default returns the full
    /// u64 range — drivers with a known span override.
    fn get_bounds(&self, _user: &AsynUser) -> AsynResult<(u64, u64)> {
        Ok((u64::MIN, u64::MAX))
    }
}

/// Unsigned 64-bit integer array I/O (asynUInt64Array equivalent).
pub trait AsynUInt64Array: Send + Sync {
    fn read_uint64_array(&mut self, user: &AsynUser, max_elements: usize) -> AsynResult<Vec<u64>>;
    fn write_uint64_array(&mut self, user: &mut AsynUser, value: &[u64]) -> AsynResult<()>;
}
