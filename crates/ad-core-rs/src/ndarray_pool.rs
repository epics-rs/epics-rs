use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::error::{ADError, ADResult};
use crate::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use crate::ndarray_handle::{NDArrayHandle, pooled_array};

/// If a free-list buffer is more than this ratio larger than needed, discard
/// it and allocate fresh to avoid wasting memory.
const THRESHOLD_SIZE_RATIO: f64 = 1.5;

/// Process-wide source of pool identities. Each `NDArrayPool` gets a unique
/// non-zero id so `release` can verify an array belongs to it (C++
/// NDArrayPool.cpp:352 checks `pArray->pNDArrayPool == this`).
static NEXT_POOL_ID: AtomicU64 = AtomicU64::new(1);

/// NDArray factory with free-list reuse and memory tracking.
///
/// Mimics C++ ADCore's NDArrayPool: on alloc, checks the free list for a
/// buffer with sufficient capacity. On release, returns the buffer to the
/// free list for future reuse. The free list is sorted by capacity (descending)
/// and excess entries are dropped when max_memory is exceeded.
pub struct NDArrayPool {
    /// Unique identity of this pool, stamped onto every array it allocates.
    id: u64,
    max_memory: usize,
    allocated_bytes: AtomicU64,
    next_unique_id: AtomicI32,
    free_list: Mutex<Vec<NDArray>>,
    num_alloc_buffers: AtomicU32,
    num_free_buffers: AtomicU32,
}

impl NDArrayPool {
    pub fn new(max_memory: usize) -> Self {
        Self {
            id: NEXT_POOL_ID.fetch_add(1, Ordering::Relaxed),
            max_memory,
            allocated_bytes: AtomicU64::new(0),
            next_unique_id: AtomicI32::new(1),
            free_list: Mutex::new(Vec::new()),
            num_alloc_buffers: AtomicU32::new(0),
            num_free_buffers: AtomicU32::new(0),
        }
    }

    /// Identity of this pool (the value stamped onto `NDArray::pool_id`).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Allocate an NDArray. Tries to reuse a free-list entry with sufficient capacity.
    ///
    /// Memory accounting tracks the exact requested byte count (`data_size`,
    /// equivalent to C++ `dataSize`), NOT the allocator-rounded `Vec::capacity`.
    /// `allocated_bytes` is the sum of the `data_size` of every live + free array,
    /// so it matches what C++ `getMemorySize()` reports and the `max_memory`
    /// limit is enforced exactly.
    pub fn alloc(&self, dims: Vec<NDDimension>, data_type: NDDataType) -> ADResult<NDArray> {
        let num_elements: usize = dims.iter().map(|d| d.size).product();
        let needed_bytes = num_elements * data_type.element_size();

        // Try to find a reusable buffer in the free list. Selection is by Vec
        // capacity (a buffer big enough to hold the data without reallocating),
        // but accounting always uses the exact `data_size`.
        let reused = {
            let mut free = self.free_list.lock();
            let mut best_idx = None;
            let mut best_cap = usize::MAX;
            for (i, arr) in free.iter().enumerate() {
                let cap = arr.data.capacity_bytes();
                if cap >= needed_bytes && cap < best_cap {
                    best_cap = cap;
                    best_idx = Some(i);
                }
            }
            if let Some(idx) = best_idx {
                if best_cap as f64 > needed_bytes as f64 * THRESHOLD_SIZE_RATIO {
                    // Oversized: discard it, subtract its tracked data_size.
                    let dropped = free.swap_remove(idx);
                    self.num_free_buffers.fetch_sub(1, Ordering::Relaxed);
                    self.allocated_bytes
                        .fetch_sub(dropped.data_size as u64, Ordering::Relaxed);
                    self.num_alloc_buffers.fetch_sub(1, Ordering::Relaxed);
                    None
                } else {
                    let arr = free.swap_remove(idx);
                    self.num_free_buffers.fetch_sub(1, Ordering::Relaxed);
                    Some(arr)
                }
            } else {
                None
            }
        };

        let mut arr = if let Some(mut reused) = reused {
            // Reuse: C parity (NDArrayPool.cpp `alloc`) keeps the buffer's
            // tracked `dataSize` and `memorySize_` unchanged when the existing
            // buffer is already large enough — only a grow (realloc) adjusts
            // accounting. Reusing a 1000-byte buffer for an 800-byte request
            // therefore leaves `allocated_bytes` at 1000.
            let old_size = reused.data_size;
            if reused.data.data_type() != data_type {
                reused.data = NDDataBuffer::zeros(data_type, num_elements);
            } else {
                reused.data.resize(num_elements);
            }
            let effective_size = if needed_bytes > old_size {
                let diff = (needed_bytes - old_size) as u64;
                // CAS loop: the limit check and the increment must be atomic so
                // two threads on the reuse-grow path cannot both pass the check
                // and over-commit past `max_memory`. Mirrors the fresh-allocation
                // path; C++ `NDArrayPool::alloc` is fully mutex-serialized.
                if self.max_memory > 0 {
                    loop {
                        let current = self.allocated_bytes.load(Ordering::Relaxed);
                        if current + diff > self.max_memory as u64 {
                            // Put the array back; the reuse path does not consume
                            // a slot.
                            let mut free = self.free_list.lock();
                            free.push(reused);
                            self.num_free_buffers.fetch_add(1, Ordering::Relaxed);
                            return Err(ADError::PoolExhausted(needed_bytes, self.max_memory));
                        }
                        if self
                            .allocated_bytes
                            .compare_exchange_weak(
                                current,
                                current + diff,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            )
                            .is_ok()
                        {
                            break;
                        }
                    }
                } else {
                    self.allocated_bytes.fetch_add(diff, Ordering::Relaxed);
                }
                needed_bytes
            } else {
                // Buffer already big enough: keep the larger tracked size so
                // accounting matches the byte count added when it was created.
                old_size
            };
            reused.data_size = effective_size;
            reused.dims = dims;
            reused.attributes.clear();
            reused.codec = None;
            reused
        } else {
            // Fresh allocation with CAS loop to avoid TOCTOU race. The reserved
            // amount is exactly `needed_bytes`; no capacity slack is ever added.
            if self.max_memory > 0 {
                loop {
                    let current = self.allocated_bytes.load(Ordering::Relaxed);
                    if current + needed_bytes as u64 > self.max_memory as u64 {
                        let mut freed_enough = false;
                        {
                            let mut free = self.free_list.lock();
                            free.sort_by(|a, b| {
                                b.data.capacity_bytes().cmp(&a.data.capacity_bytes())
                            });
                            let mut reclaimed = 0u64;
                            let over = (current + needed_bytes as u64)
                                .saturating_sub(self.max_memory as u64);
                            while !free.is_empty() && reclaimed < over {
                                let dropped = free.remove(0);
                                let dropped_size = dropped.data_size as u64;
                                self.allocated_bytes
                                    .fetch_sub(dropped_size, Ordering::Relaxed);
                                self.num_free_buffers.fetch_sub(1, Ordering::Relaxed);
                                self.num_alloc_buffers.fetch_sub(1, Ordering::Relaxed);
                                reclaimed += dropped_size;
                            }
                            if reclaimed >= over {
                                freed_enough = true;
                            }
                        }
                        if !freed_enough {
                            return Err(ADError::PoolExhausted(needed_bytes, self.max_memory));
                        }
                        continue;
                    }
                    if self
                        .allocated_bytes
                        .compare_exchange_weak(
                            current,
                            current + needed_bytes as u64,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                    {
                        break;
                    }
                }
            } else {
                self.allocated_bytes
                    .fetch_add(needed_bytes as u64, Ordering::Relaxed);
            }
            self.num_alloc_buffers.fetch_add(1, Ordering::Relaxed);
            NDArray::new(dims, data_type)
        };

        arr.unique_id = self.next_unique_id.fetch_add(1, Ordering::Relaxed);
        // The "Initialize fields" block of C++ NDArrayPool::alloc
        // (NDArrayPool.cpp:187-199) sets neither `epicsTS` nor `timeStamp`: the
        // driver owns both and stamps them together via
        // asynNDArrayDriver::updateTimeStamps (asynNDArrayDriver.cpp:832-836).
        // Stamping only `epicsTS` here left `timeStamp` at 0.0 (fresh buffer) or
        // at the previous frame's value (reused buffer), so the two published
        // timestamps disagreed. See NDArray::update_time_stamps.
        arr.pool_id = self.id;
        // `data_size` is already correct: a fresh `NDArray::new` sets it to
        // `needed_bytes`; the reuse branch keeps the buffer's larger size.
        Ok(arr)
    }

    /// Allocate a copy of an existing NDArray (new unique_id, data cloned).
    /// Tries the free list first (via alloc()), then copies data from source.
    pub fn alloc_copy(&self, source: &NDArray) -> ADResult<NDArray> {
        let dims = source.dims.clone();
        let data_type = source.data.data_type();
        let mut copy = self.alloc(dims, data_type)?;
        copy.data = source.data.clone();
        // C++ NDArrayPool::copy carries BOTH stamps across (NDArrayPool.cpp:284-285).
        // Copying only `time_stamp` left `timestamp` (epicsTS) at whatever the
        // recycled buffer happened to hold.
        copy.time_stamp = source.time_stamp;
        copy.timestamp = source.timestamp;
        copy.attributes = source.attributes.clone();
        copy.codec = source.codec.clone();
        Ok(copy)
    }

    /// Return an array to the free list for future reuse.
    ///
    /// The array must have been allocated from this pool. C++
    /// (NDArrayPool.cpp:352) verifies `pArray->pNDArrayPool == this` and refuses
    /// otherwise; the Rust port checks `pool_id` and drops a foreign array
    /// without touching this pool's free list or accounting.
    pub fn release(&self, array: NDArray) {
        if array.pool_id != self.id {
            // Foreign (or non-pool) array — never touch this pool's accounting.
            // Dropping `array` here frees its memory; it was never part of
            // `allocated_bytes`, so no adjustment is made.
            return;
        }

        let mut free = self.free_list.lock();
        free.push(array);
        self.num_free_buffers.fetch_add(1, Ordering::Relaxed);

        // If total allocated exceeds max_memory, drop largest free entries.
        // Accounting and the loop counter both use the exact `data_size` so the
        // `usize` `excess` can never underflow (max_memory == 0 means unlimited).
        let total = self.allocated_bytes.load(Ordering::Relaxed) as usize;
        if self.max_memory > 0 && total > self.max_memory && !free.is_empty() {
            free.sort_by(|a, b| b.data_size.cmp(&a.data_size));
            let mut excess = total - self.max_memory;
            while excess > 0 && !free.is_empty() {
                let dropped = free.remove(0);
                let dropped_size = dropped.data_size;
                self.allocated_bytes
                    .fetch_sub(dropped_size as u64, Ordering::Relaxed);
                self.num_free_buffers.fetch_sub(1, Ordering::Relaxed);
                self.num_alloc_buffers.fetch_sub(1, Ordering::Relaxed);
                if dropped_size >= excess {
                    break;
                }
                excess -= dropped_size;
            }
        }
    }

    /// Clear all entries from the free list.
    pub fn empty_free_list(&self) {
        let mut free = self.free_list.lock();
        let count = free.len() as u32;
        for arr in free.drain(..) {
            self.allocated_bytes
                .fetch_sub(arr.data_size as u64, Ordering::Relaxed);
            self.num_alloc_buffers.fetch_sub(1, Ordering::Relaxed);
        }
        self.num_free_buffers.fetch_sub(count, Ordering::Relaxed);
    }

    pub fn allocated_bytes(&self) -> u64 {
        self.allocated_bytes.load(Ordering::Relaxed)
    }

    pub fn num_alloc_buffers(&self) -> u32 {
        self.num_alloc_buffers.load(Ordering::Relaxed)
    }

    pub fn num_free_buffers(&self) -> u32 {
        self.num_free_buffers.load(Ordering::Relaxed)
    }

    pub fn max_memory(&self) -> usize {
        self.max_memory
    }

    /// Allocate an NDArray wrapped in a pool-aware handle.
    /// On final drop, the array is returned to this pool's free list.
    pub fn alloc_handle(
        pool: &Arc<Self>,
        dims: Vec<NDDimension>,
        data_type: NDDataType,
    ) -> ADResult<NDArrayHandle> {
        let array = pool.alloc(dims, data_type)?;
        Ok(pooled_array(array, pool))
    }

    /// Copy `src` into a (possibly existing) output array.
    ///
    /// Mirrors C++ `NDArrayPool::copy(pIn, pOut, copyData, copyDimensions,
    /// copyDataType)`:
    /// - `out` is `None`: a fresh array is allocated through this pool with the
    ///   source dimensions/type.
    /// - `copy_dimensions`: copy `dims` from source.
    /// - `copy_data_type`: the output buffer takes the source data type.
    /// - `copy_data`: copy the pixel/codec bytes.
    ///
    /// Attributes are always cleared on the output then copied from the source.
    pub fn copy(
        &self,
        src: &NDArray,
        out: Option<NDArray>,
        copy_data: bool,
        copy_dimensions: bool,
        copy_data_type: bool,
    ) -> ADResult<NDArray> {
        let mut out = match out {
            Some(o) => o,
            None => self.alloc(src.dims.clone(), src.data.data_type())?,
        };

        out.unique_id = src.unique_id;
        out.time_stamp = src.time_stamp;
        out.timestamp = src.timestamp;
        if copy_dimensions {
            out.dims = src.dims.clone();
        }
        out.codec = src.codec.clone();

        if copy_data {
            if copy_data_type && out.data.data_type() != src.data.data_type() {
                // Output adopts the source type: clone the buffer wholesale.
                out.data = src.data.clone();
            } else if out.data.data_type() == src.data.data_type() {
                out.data = src.data.clone();
            } else {
                // Output keeps its own type: convert pixel values.
                out.data = crate::color::convert_data_type(src, out.data.data_type())?.data;
            }
        } else if copy_data_type && out.data.data_type() != src.data.data_type() {
            out.data = NDDataBuffer::zeros(src.data.data_type(), out.data.len());
        }

        out.attributes.clear();
        out.attributes.copy_from(&src.attributes);
        Ok(out)
    }

    /// Allocate `count` copies of `template_array`, then immediately release
    /// them back to the free list (C++ `asynNDArrayDriver::preAllocateBuffers`).
    ///
    /// The net effect is that the pool's free list is warmed with `count`
    /// reusable buffers sized for the template array.
    pub fn pre_allocate_buffers(&self, template_array: &NDArray, count: usize) -> ADResult<()> {
        let mut buffers = Vec::with_capacity(count);
        for _ in 0..count {
            buffers.push(self.copy(template_array, None, true, true, true)?);
        }
        for arr in buffers {
            self.release(arr);
        }
        Ok(())
    }

    /// Convert data type only (no dimension changes).
    /// Allocates the output through the pool, converts data, copies metadata.
    pub fn convert_type(&self, src: &NDArray, target_type: NDDataType) -> ADResult<NDArray> {
        // C parity (NDArrayPool.cpp:620-625): cannot convert compressed data.
        if src.codec.is_some() {
            return Err(ADError::UnsupportedConversion(
                "convert_type: cannot convert compressed (codec) data".into(),
            ));
        }
        if src.data.data_type() == target_type {
            return self.alloc_copy(src);
        }
        // Allocate the output through the pool so it counts against
        // allocated_bytes / num_alloc_buffers.
        let mut out = self.alloc(src.dims.clone(), target_type)?;
        let converted = crate::color::convert_data_type(src, target_type)?;
        out.data = converted.data;
        out.time_stamp = src.time_stamp;
        out.timestamp = src.timestamp;
        out.attributes.copy_from(&src.attributes);
        Ok(out)
    }

    /// Full convert with dimension changes: extract sub-region, bin, reverse.
    ///
    /// The conversion kernel itself lives in [`crate::convert::convert_dims`]
    /// (the single owner of C++ `NDArrayPool::convert` semantics — binning
    /// accumulates in the TARGET type, casts are C casts). This wrapper adds
    /// what only a pool can: allocating the output THROUGH the pool so it
    /// counts against allocated_bytes / num_alloc_buffers and can be reused
    /// via the free list (C parity: C++ convert calls alloc() for its output).
    pub fn convert(
        &self,
        src: &NDArray,
        dims_out: &[NDDimension],
        target_type: NDDataType,
    ) -> ADResult<NDArray> {
        let converted = crate::convert::convert_dims(src, dims_out, target_type)?;

        let mut arr = self.alloc(converted.dims.clone(), target_type)?;
        arr.timestamp = src.timestamp;
        arr.time_stamp = src.time_stamp;
        arr.attributes.copy_from(&src.attributes);
        arr.data = converted.data;

        Ok(arr)
    }

    /// Produce a diagnostic text dump (matching C++ `NDArrayPool::report`).
    ///
    /// `details > 5` additionally lists the free-list entries.
    pub fn report(&self, details: i32) -> String {
        let mut out = String::new();
        out.push('\n');
        out.push_str("NDArrayPool:\n");
        out.push_str(&format!(
            "  numBuffers={}, numFree={}\n",
            self.num_alloc_buffers(),
            self.num_free_buffers()
        ));
        out.push_str(&format!(
            "  memorySize={}, maxMemory={}\n",
            self.allocated_bytes(),
            self.max_memory
        ));
        if details > 5 {
            let free = self.free_list.lock();
            out.push_str("  freeList: (index, dataSize, capacity)\n");
            for (i, arr) in free.iter().enumerate() {
                out.push_str(&format!(
                    "    {} {} {}\n",
                    i,
                    arr.data_size,
                    arr.data.capacity_bytes()
                ));
            }
            if details > 10 {
                for arr in free.iter() {
                    out.push_str(&arr.report(details));
                }
            }
        }
        out
    }
}

// Compile-time check: NDArrayPool is Send + Sync
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<NDArrayPool>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alloc_auto_id() {
        let pool = NDArrayPool::new(1_000_000);
        let a1 = pool
            .alloc(vec![NDDimension::new(10)], NDDataType::UInt8)
            .unwrap();
        let a2 = pool
            .alloc(vec![NDDimension::new(10)], NDDataType::UInt8)
            .unwrap();
        assert_eq!(a1.unique_id, 1);
        assert_eq!(a2.unique_id, 2);
    }

    #[test]
    fn test_r6_65_alloc_stamps_neither_timestamp() {
        // R6-65 / NDArrayPool.cpp:187-199 — alloc's "Initialize fields" block
        // sets neither epicsTS nor timeStamp. Stamping only epicsTS left the two
        // published timestamps disagreeing on every array.
        let pool = NDArrayPool::new(1_000_000);
        let fresh = pool
            .alloc(vec![NDDimension::new(10)], NDDataType::UInt8)
            .unwrap();
        assert_eq!(fresh.timestamp, crate::timestamp::EpicsTimestamp::default());
        assert_eq!(fresh.time_stamp, 0.0);

        // Reuse path: C leaves the recycled buffer's stamps alone too, so
        // whatever is there stays there — but the pair still agrees.
        let mut used = pool
            .alloc(vec![NDDimension::new(10)], NDDataType::UInt8)
            .unwrap();
        used.update_time_stamps(crate::timestamp::EpicsTimestamp {
            sec: 1000,
            nsec: 250_000_000,
        });
        pool.release(used);
        let reused = pool
            .alloc(vec![NDDimension::new(10)], NDDataType::UInt8)
            .unwrap();
        assert_eq!(reused.timestamp.sec, 1000);
        assert_eq!(reused.time_stamp, 1000.25);
    }

    #[test]
    fn test_r6_65_update_time_stamps_derives_the_double() {
        // asynNDArrayDriver.cpp:832-836: timeStamp = epicsTS.secPastEpoch +
        // epicsTS.nsec/1.e9 — derived, so the pair cannot disagree.
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.update_time_stamps(crate::timestamp::EpicsTimestamp {
            sec: 42,
            nsec: 500_000_000,
        });
        assert_eq!(arr.timestamp.sec, 42);
        assert_eq!(arr.timestamp.nsec, 500_000_000);
        assert_eq!(arr.time_stamp, 42.5);
    }

    #[test]
    fn test_r6_65_alloc_copy_carries_both_stamps() {
        // NDArrayPool.cpp:284-285 copies timeStamp AND epicsTS from the source.
        let pool = NDArrayPool::new(1_000_000);
        let mut src = pool
            .alloc(vec![NDDimension::new(8)], NDDataType::UInt8)
            .unwrap();
        src.update_time_stamps(crate::timestamp::EpicsTimestamp {
            sec: 777,
            nsec: 125_000_000,
        });

        let copy = pool.alloc_copy(&src).unwrap();
        assert_eq!(copy.timestamp, src.timestamp);
        assert_eq!(copy.time_stamp, src.time_stamp);
        assert_eq!(copy.time_stamp, 777.125);
    }

    #[test]
    fn test_alloc_tracks_bytes() {
        let pool = NDArrayPool::new(1_000_000);
        let _ = pool
            .alloc(vec![NDDimension::new(100)], NDDataType::Float64)
            .unwrap();
        assert!(pool.allocated_bytes() >= 800);
    }

    #[test]
    fn test_alloc_exceeds_max() {
        let pool = NDArrayPool::new(100);
        let result = pool.alloc(vec![NDDimension::new(200)], NDDataType::UInt8);
        assert!(result.is_err());
    }

    #[test]
    fn test_alloc_copy_preserves_data() {
        let pool = NDArrayPool::new(1_000_000);
        let mut source = pool
            .alloc(vec![NDDimension::new(4)], NDDataType::UInt8)
            .unwrap();
        if let NDDataBuffer::U8(ref mut v) = source.data {
            v[0] = 1;
            v[1] = 2;
            v[2] = 3;
            v[3] = 4;
        }

        let copy = pool.alloc_copy(&source).unwrap();
        assert_ne!(copy.unique_id, source.unique_id);
        assert_eq!(copy.dims.len(), source.dims.len());
        if let NDDataBuffer::U8(ref v) = copy.data {
            assert_eq!(v, &[1, 2, 3, 4]);
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_alloc_copy_tracks_bytes() {
        let pool = NDArrayPool::new(1_000_000);
        let source = pool
            .alloc(vec![NDDimension::new(10)], NDDataType::UInt16)
            .unwrap();
        assert_eq!(pool.allocated_bytes(), 20);
        let _ = pool.alloc_copy(&source).unwrap();
        assert!(pool.allocated_bytes() >= 40);
    }

    #[test]
    fn test_alloc_copy_exceeds_max() {
        let pool = NDArrayPool::new(60);
        let source = pool
            .alloc(vec![NDDimension::new(50)], NDDataType::UInt8)
            .unwrap();
        assert!(pool.alloc_copy(&source).is_err());
    }

    // --- Free-list reuse tests ---

    #[test]
    fn test_release_and_reuse() {
        let pool = NDArrayPool::new(1_000_000);
        let arr = pool
            .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
            .unwrap();
        let _alloc_bytes_after_first = pool.allocated_bytes();
        assert_eq!(pool.num_alloc_buffers(), 1);

        // Release back to free list
        pool.release(arr);
        assert_eq!(pool.num_free_buffers(), 1);

        // Alloc again — reuse within 1.5x ratio
        let arr2 = pool
            .alloc(vec![NDDimension::new(80)], NDDataType::UInt8)
            .unwrap();
        assert_eq!(arr2.data.len(), 80);
    }

    #[test]
    fn test_free_list_prefers_smallest_sufficient() {
        let pool = NDArrayPool::new(10_000_000);
        let small = pool
            .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
            .unwrap();
        let large = pool
            .alloc(vec![NDDimension::new(10000)], NDDataType::UInt8)
            .unwrap();
        let medium = pool
            .alloc(vec![NDDimension::new(1000)], NDDataType::UInt8)
            .unwrap();

        pool.release(large);
        pool.release(medium);
        pool.release(small);
        assert_eq!(pool.num_free_buffers(), 3);

        // Request 900 bytes — medium (1000 cap) is within 1.5x ratio
        let reused = pool
            .alloc(vec![NDDimension::new(900)], NDDataType::UInt8)
            .unwrap();
        assert!(reused.data.capacity_bytes() >= 900);
    }

    #[test]
    fn test_empty_free_list() {
        let pool = NDArrayPool::new(1_000_000);
        let a1 = pool
            .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
            .unwrap();
        let a2 = pool
            .alloc(vec![NDDimension::new(200)], NDDataType::UInt8)
            .unwrap();
        pool.release(a1);
        pool.release(a2);
        assert_eq!(pool.num_free_buffers(), 2);

        pool.empty_free_list();
        assert_eq!(pool.num_free_buffers(), 0);
        assert_eq!(pool.num_alloc_buffers(), 0);
    }

    #[test]
    fn test_num_free_buffers_tracking() {
        let pool = NDArrayPool::new(1_000_000);
        assert_eq!(pool.num_free_buffers(), 0);

        let a = pool
            .alloc(vec![NDDimension::new(10)], NDDataType::UInt8)
            .unwrap();
        assert_eq!(pool.num_free_buffers(), 0);

        pool.release(a);
        assert_eq!(pool.num_free_buffers(), 1);

        let _ = pool
            .alloc(vec![NDDimension::new(10)], NDDataType::UInt8)
            .unwrap();
        assert_eq!(pool.num_free_buffers(), 0);
    }

    #[test]
    fn test_concurrent_alloc_release() {
        use std::sync::Arc;
        use std::thread;

        let pool = Arc::new(NDArrayPool::new(10_000_000));
        let mut handles = Vec::new();

        for _ in 0..4 {
            let pool = pool.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let arr = pool
                        .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
                        .unwrap();
                    pool.release(arr);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // All should be released back
        assert!(pool.num_free_buffers() > 0);
    }

    #[test]
    fn test_max_memory() {
        let pool = NDArrayPool::new(42);
        assert_eq!(pool.max_memory(), 42);
    }

    // --- convert_type tests ---

    #[test]
    fn test_convert_type_same_type() {
        let pool = NDArrayPool::new(1_000_000);
        let mut src = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        if let NDDataBuffer::U8(ref mut v) = src.data {
            v[0] = 10;
            v[1] = 20;
            v[2] = 30;
            v[3] = 40;
        }

        let out = pool.convert_type(&src, NDDataType::UInt8).unwrap();
        assert_eq!(out.data.data_type(), NDDataType::UInt8);
        if let NDDataBuffer::U8(ref v) = out.data {
            assert_eq!(v, &[10, 20, 30, 40]);
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_convert_type_u8_to_f32() {
        let pool = NDArrayPool::new(1_000_000);
        let mut src = NDArray::new(vec![NDDimension::new(3)], NDDataType::UInt8);
        if let NDDataBuffer::U8(ref mut v) = src.data {
            v[0] = 0;
            v[1] = 128;
            v[2] = 255;
        }

        let out = pool.convert_type(&src, NDDataType::Float32).unwrap();
        assert_eq!(out.data.data_type(), NDDataType::Float32);
        if let NDDataBuffer::F32(ref v) = out.data {
            assert_eq!(v[0], 0.0);
            assert_eq!(v[1], 128.0);
            assert_eq!(v[2], 255.0);
        } else {
            panic!("wrong type");
        }
    }

    /// C++ `convertType` (NDArrayPool.cpp:378-388) is a plain C cast:
    /// `(epicsUInt8)300 == 44` — narrowing truncates to the low bits and
    /// wraps. It does NOT clamp to 255.
    #[test]
    fn test_convert_type_u16_to_u8_wraps_like_a_c_cast() {
        let pool = NDArrayPool::new(1_000_000);
        let mut src = NDArray::new(vec![NDDimension::new(2)], NDDataType::UInt16);
        if let NDDataBuffer::U16(ref mut v) = src.data {
            v[0] = 100;
            v[1] = 300;
        }

        let out = pool.convert_type(&src, NDDataType::UInt8).unwrap();
        if let NDDataBuffer::U8(ref v) = out.data {
            assert_eq!(v[0], 100);
            assert_eq!(v[1], 44, "300 % 256 == 44, not a clamp to 255");
        } else {
            panic!("wrong type");
        }
    }

    // --- convert tests ---

    /// Helper: create a 4x4 UInt8 array with values 0..15.
    fn make_4x4_u8() -> NDArray {
        let mut arr = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = arr.data {
            for i in 0..16 {
                v[i] = i as u8;
            }
        }
        arr
    }

    #[test]
    fn test_convert_identity() {
        // Identity conversion: no offset, no binning, no reverse
        let pool = NDArrayPool::new(1_000_000);
        let src = make_4x4_u8();
        let dims_out = vec![
            NDDimension {
                size: 4,
                offset: 0,
                binning: 1,
                reverse: false,
            },
            NDDimension {
                size: 4,
                offset: 0,
                binning: 1,
                reverse: false,
            },
        ];

        let out = pool.convert(&src, &dims_out, NDDataType::UInt8).unwrap();
        assert_eq!(out.dims[0].size, 4);
        assert_eq!(out.dims[1].size, 4);
        if let NDDataBuffer::U8(ref v) = out.data {
            for i in 0..16 {
                assert_eq!(v[i], i as u8);
            }
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_convert_offset_extraction() {
        // Extract 2x2 sub-region starting at offset (1, 1)
        let pool = NDArrayPool::new(1_000_000);
        let src = make_4x4_u8();
        let dims_out = vec![
            NDDimension {
                size: 2,
                offset: 1,
                binning: 1,
                reverse: false,
            },
            NDDimension {
                size: 2,
                offset: 1,
                binning: 1,
                reverse: false,
            },
        ];

        let out = pool.convert(&src, &dims_out, NDDataType::UInt8).unwrap();
        assert_eq!(out.dims[0].size, 2);
        assert_eq!(out.dims[1].size, 2);
        // Source layout (row-major, dim0=x fastest):
        //   row0: [0,1,2,3], row1: [4,5,6,7], row2: [8,9,10,11], row3: [12,13,14,15]
        // offset (1,1) -> src[1+1*4]=5, src[2+1*4]=6, src[1+2*4]=9, src[2+2*4]=10
        if let NDDataBuffer::U8(ref v) = out.data {
            assert_eq!(v[0], 5);
            assert_eq!(v[1], 6);
            assert_eq!(v[2], 9);
            assert_eq!(v[3], 10);
        } else {
            panic!("wrong type");
        }

        // Verify cumulative offset tracking
        assert_eq!(out.dims[0].offset, 1); // src offset 0 + dims_out offset 1
        assert_eq!(out.dims[1].offset, 1);
    }

    #[test]
    fn test_convert_binning_2x2() {
        // 4x4 -> 2x2 with 2x2 binning (sum)
        let pool = NDArrayPool::new(1_000_000);
        let src = make_4x4_u8();
        let dims_out = vec![
            NDDimension {
                size: 4,
                offset: 0,
                binning: 2,
                reverse: false,
            },
            NDDimension {
                size: 4,
                offset: 0,
                binning: 2,
                reverse: false,
            },
        ];

        let out = pool.convert(&src, &dims_out, NDDataType::UInt8).unwrap();
        assert_eq!(out.dims[0].size, 2);
        assert_eq!(out.dims[1].size, 2);
        // top-left 2x2: sum = 0+1+4+5 = 10
        // top-right 2x2: sum = 2+3+6+7 = 18
        // bottom-left 2x2: sum = 8+9+12+13 = 42
        // bottom-right 2x2: sum = 10+11+14+15 = 50
        if let NDDataBuffer::U8(ref v) = out.data {
            assert_eq!(v[0], 10);
            assert_eq!(v[1], 18);
            assert_eq!(v[2], 42);
            assert_eq!(v[3], 50);
        } else {
            panic!("wrong type");
        }

        // Verify cumulative binning
        assert_eq!(out.dims[0].binning, 2); // src binning 1 * dims_out binning 2
        assert_eq!(out.dims[1].binning, 2);
    }

    // Build a 2x2 array of `data_type` with every element set to `val`
    // (`val` is `as`-cast to the element type).
    fn make_2x2_filled(data_type: NDDataType, val: i128) -> NDArray {
        let mut src = NDArray::new(vec![NDDimension::new(2), NDDimension::new(2)], data_type);
        macro_rules! fill {
            ($variant:ident, $t:ty) => {
                if let NDDataBuffer::$variant(ref mut v) = src.data {
                    for e in v.iter_mut() {
                        *e = val as $t;
                    }
                }
            };
        }
        match data_type {
            NDDataType::Int8 => fill!(I8, i8),
            NDDataType::UInt8 => fill!(U8, u8),
            NDDataType::Int16 => fill!(I16, i16),
            NDDataType::UInt16 => fill!(U16, u16),
            NDDataType::Int32 => fill!(I32, i32),
            NDDataType::UInt32 => fill!(U32, u32),
            NDDataType::Int64 => fill!(I64, i64),
            NDDataType::UInt64 => fill!(U64, u64),
            NDDataType::Float32 => fill!(F32, f32),
            NDDataType::Float64 => fill!(F64, f64),
        }
        src
    }

    fn bin_2x2_to_one() -> Vec<NDDimension> {
        // 2x2 -> 1x1, summing the whole 2x2 window.
        vec![
            NDDimension {
                size: 2,
                offset: 0,
                binning: 2,
                reverse: false,
            },
            NDDimension {
                size: 2,
                offset: 0,
                binning: 2,
                reverse: false,
            },
        ]
    }

    /// ADC-8 case (b) — widening target. C `convertDim` sums in the
    /// OUTPUT type: four u8 200s binned into a u16 give 800. The old f64
    /// accumulator cast the sum to the SOURCE type first (200*4=800 -> u8
    /// saturates to 255 -> widen to 255), losing the high bits.
    #[test]
    fn test_convert_binning_widening_target_keeps_full_sum() {
        let pool = NDArrayPool::new(1_000_000);
        let src = make_2x2_filled(NDDataType::UInt8, 200);
        let out = pool
            .convert(&src, &bin_2x2_to_one(), NDDataType::UInt16)
            .unwrap();
        match out.data {
            NDDataBuffer::U16(ref v) => assert_eq!(v[0], 800, "C sums in u16: 200*4"),
            ref other => panic!("expected U16, got {:?}", other.data_type()),
        }
    }

    /// ADC-8 case (a) — integer overflow wraps, it does not saturate. C
    /// accumulates in the u8 output type, so four 100s = 400 wrap to
    /// 400 % 256 = 144. The old f64 accumulator saturated to 255.
    #[test]
    fn test_convert_binning_integer_overflow_wraps() {
        let pool = NDArrayPool::new(1_000_000);
        let src = make_2x2_filled(NDDataType::UInt8, 100);
        let out = pool
            .convert(&src, &bin_2x2_to_one(), NDDataType::UInt8)
            .unwrap();
        match out.data {
            NDDataBuffer::U8(ref v) => assert_eq!(v[0], 144, "400 wraps mod 256 in u8"),
            ref other => panic!("expected U8, got {:?}", other.data_type()),
        }
    }

    /// ADC-8 case (c) — i64 magnitudes above 2^53 survive. At binning == 1
    /// (the wired ROI path) C copies `(i64)*pDIn` exactly. The old f64
    /// accumulator round-tripped through f64 and dropped the low bit of
    /// 2^53 + 1 even when source and target type are identical.
    #[test]
    fn test_convert_binning1_int64_above_2pow53_exact() {
        let pool = NDArrayPool::new(1_000_000);
        let value: i64 = (1i64 << 53) + 1;
        let mut src = NDArray::new(
            vec![NDDimension::new(1), NDDimension::new(1)],
            NDDataType::Int64,
        );
        if let NDDataBuffer::I64(ref mut v) = src.data {
            v[0] = value;
        }
        let dims_out = vec![
            NDDimension {
                size: 1,
                offset: 0,
                binning: 1,
                reverse: false,
            },
            NDDimension {
                size: 1,
                offset: 0,
                binning: 1,
                reverse: false,
            },
        ];
        let out = pool.convert(&src, &dims_out, NDDataType::Int64).unwrap();
        match out.data {
            NDDataBuffer::I64(ref v) => assert_eq!(v[0], value, "2^53+1 kept exactly"),
            ref other => panic!("expected I64, got {:?}", other.data_type()),
        }
    }

    #[test]
    fn test_convert_reverse_x() {
        // 4x1 with X-reverse
        let pool = NDArrayPool::new(1_000_000);
        let mut src = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(1)],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = src.data {
            v[0] = 10;
            v[1] = 20;
            v[2] = 30;
            v[3] = 40;
        }

        let dims_out = vec![
            NDDimension {
                size: 4,
                offset: 0,
                binning: 1,
                reverse: true,
            },
            NDDimension {
                size: 1,
                offset: 0,
                binning: 1,
                reverse: false,
            },
        ];

        let out = pool.convert(&src, &dims_out, NDDataType::UInt8).unwrap();
        if let NDDataBuffer::U8(ref v) = out.data {
            assert_eq!(v[0], 40);
            assert_eq!(v[1], 30);
            assert_eq!(v[2], 20);
            assert_eq!(v[3], 10);
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_convert_reverse_y() {
        // 2x2 with Y-reverse
        let pool = NDArrayPool::new(1_000_000);
        let mut src = NDArray::new(
            vec![NDDimension::new(2), NDDimension::new(2)],
            NDDataType::UInt16,
        );
        if let NDDataBuffer::U16(ref mut v) = src.data {
            // row0: [1, 2], row1: [3, 4]
            v[0] = 1;
            v[1] = 2;
            v[2] = 3;
            v[3] = 4;
        }

        let dims_out = vec![
            NDDimension {
                size: 2,
                offset: 0,
                binning: 1,
                reverse: false,
            },
            NDDimension {
                size: 2,
                offset: 0,
                binning: 1,
                reverse: true,
            },
        ];

        let out = pool.convert(&src, &dims_out, NDDataType::UInt16).unwrap();
        if let NDDataBuffer::U16(ref v) = out.data {
            // Y reversed: row0 now has row1 data, row1 has row0 data
            assert_eq!(v[0], 3);
            assert_eq!(v[1], 4);
            assert_eq!(v[2], 1);
            assert_eq!(v[3], 2);
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_convert_with_type_change() {
        // Convert 4x4 UInt8 -> Float32, with 2x2 binning
        let pool = NDArrayPool::new(1_000_000);
        let src = make_4x4_u8();
        let dims_out = vec![
            NDDimension {
                size: 4,
                offset: 0,
                binning: 2,
                reverse: false,
            },
            NDDimension {
                size: 4,
                offset: 0,
                binning: 2,
                reverse: false,
            },
        ];

        let out = pool.convert(&src, &dims_out, NDDataType::Float32).unwrap();
        assert_eq!(out.data.data_type(), NDDataType::Float32);
        assert_eq!(out.dims[0].size, 2);
        assert_eq!(out.dims[1].size, 2);
        if let NDDataBuffer::F32(ref v) = out.data {
            assert_eq!(v[0], 10.0); // 0+1+4+5
            assert_eq!(v[1], 18.0); // 2+3+6+7
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_convert_cumulative_offset_and_binning() {
        // Source with existing offset=10, binning=2
        let pool = NDArrayPool::new(1_000_000);
        let mut src = NDArray::new(
            vec![NDDimension::new(4), NDDimension::new(4)],
            NDDataType::UInt8,
        );
        src.dims[0].offset = 10;
        src.dims[0].binning = 2;
        src.dims[1].offset = 20;
        src.dims[1].binning = 3;
        if let NDDataBuffer::U8(ref mut v) = src.data {
            for i in 0..16 {
                v[i] = i as u8;
            }
        }

        let dims_out = vec![
            NDDimension {
                size: 2,
                offset: 1,
                binning: 2,
                reverse: false,
            },
            NDDimension {
                size: 2,
                offset: 1,
                binning: 2,
                reverse: false,
            },
        ];

        let out = pool.convert(&src, &dims_out, NDDataType::UInt8).unwrap();
        // Cumulative offset: src.offset + dims_out.offset
        assert_eq!(out.dims[0].offset, 10 + 1);
        assert_eq!(out.dims[1].offset, 20 + 1);
        // Cumulative binning: src.binning * dims_out.binning
        assert_eq!(out.dims[0].binning, 2 * 2);
        assert_eq!(out.dims[1].binning, 3 * 2);
    }

    #[test]
    fn test_convert_1d() {
        // 1D: 8 elements, offset=2, size=4, binning=2 -> 2 output elements
        let pool = NDArrayPool::new(1_000_000);
        let mut src = NDArray::new(vec![NDDimension::new(8)], NDDataType::UInt16);
        if let NDDataBuffer::U16(ref mut v) = src.data {
            for i in 0..8 {
                v[i] = (i * 10) as u16;
            }
            // [0, 10, 20, 30, 40, 50, 60, 70]
        }

        let dims_out = vec![NDDimension {
            size: 4,
            offset: 2,
            binning: 2,
            reverse: false,
        }];

        let out = pool.convert(&src, &dims_out, NDDataType::UInt16).unwrap();
        assert_eq!(out.dims.len(), 1);
        assert_eq!(out.dims[0].size, 2);
        if let NDDataBuffer::U16(ref v) = out.data {
            // offset=2: src[2]=20, src[3]=30 -> sum=50
            // next: src[4]=40, src[5]=50 -> sum=90
            assert_eq!(v[0], 50);
            assert_eq!(v[1], 90);
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_convert_3d() {
        // 3D: 2x2x2 with identity dims -> should copy exactly
        let pool = NDArrayPool::new(1_000_000);
        let mut src = NDArray::new(
            vec![
                NDDimension::new(2),
                NDDimension::new(2),
                NDDimension::new(2),
            ],
            NDDataType::UInt8,
        );
        if let NDDataBuffer::U8(ref mut v) = src.data {
            for i in 0..8 {
                v[i] = (i + 1) as u8;
            }
        }

        let dims_out = vec![
            NDDimension {
                size: 2,
                offset: 0,
                binning: 1,
                reverse: false,
            },
            NDDimension {
                size: 2,
                offset: 0,
                binning: 1,
                reverse: false,
            },
            NDDimension {
                size: 2,
                offset: 0,
                binning: 1,
                reverse: false,
            },
        ];

        let out = pool.convert(&src, &dims_out, NDDataType::UInt8).unwrap();
        if let NDDataBuffer::U8(ref v) = out.data {
            for i in 0..8 {
                assert_eq!(v[i], (i + 1) as u8);
            }
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_convert_dim_mismatch_error() {
        let pool = NDArrayPool::new(1_000_000);
        let src = make_4x4_u8();
        // Wrong number of dims_out
        let dims_out = vec![NDDimension {
            size: 4,
            offset: 0,
            binning: 1,
            reverse: false,
        }];

        let result = pool.convert(&src, &dims_out, NDDataType::UInt8);
        assert!(result.is_err());
    }

    #[test]
    fn test_convert_offset_out_of_bounds_error() {
        let pool = NDArrayPool::new(1_000_000);
        let src = make_4x4_u8();
        let dims_out = vec![
            NDDimension {
                size: 4,
                offset: 2,
                binning: 1,
                reverse: false,
            }, // 2+4 > 4
            NDDimension {
                size: 4,
                offset: 0,
                binning: 1,
                reverse: false,
            },
        ];

        let result = pool.convert(&src, &dims_out, NDDataType::UInt8);
        assert!(result.is_err());
    }

    #[test]
    fn test_convert_preserves_metadata() {
        let pool = NDArrayPool::new(1_000_000);
        let mut src = make_4x4_u8();
        src.time_stamp = 12345.678;

        let dims_out = vec![
            NDDimension {
                size: 4,
                offset: 0,
                binning: 1,
                reverse: false,
            },
            NDDimension {
                size: 4,
                offset: 0,
                binning: 1,
                reverse: false,
            },
        ];

        let out = pool.convert(&src, &dims_out, NDDataType::UInt8).unwrap();
        assert_eq!(out.time_stamp, 12345.678);
    }

    #[test]
    fn test_convert_binning_and_reverse_combined() {
        // 4x1, binning=2, reverse=true
        let pool = NDArrayPool::new(1_000_000);
        let mut src = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt16);
        if let NDDataBuffer::U16(ref mut v) = src.data {
            v[0] = 1;
            v[1] = 2;
            v[2] = 3;
            v[3] = 4;
        }

        let dims_out = vec![NDDimension {
            size: 4,
            offset: 0,
            binning: 2,
            reverse: true,
        }];

        let out = pool.convert(&src, &dims_out, NDDataType::UInt16).unwrap();
        assert_eq!(out.dims[0].size, 2);
        if let NDDataBuffer::U16(ref v) = out.data {
            // Without reverse: [1+2, 3+4] = [3, 7]
            // With reverse: output[0] reads from high end, output[1] from low end
            // eff_coords[0] for out_coord=0 with reverse => size-1-0 = 1 -> src[2..3] = 3+4 = 7
            // eff_coords[0] for out_coord=1 with reverse => size-1-1 = 0 -> src[0..1] = 1+2 = 3
            assert_eq!(v[0], 7);
            assert_eq!(v[1], 3);
        } else {
            panic!("wrong type");
        }
    }

    // --- Regression tests for review fixes ---

    /// B1: output `reverse` must be cumulative — `dims_out.reverse XOR src.reverse`.
    #[test]
    fn test_convert_reverse_flag_cumulative() {
        let pool = NDArrayPool::new(1_000_000);
        let mut src = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        src.dims[0].reverse = true; // source already reversed

        // dims_out also requests reverse: true XOR true = false.
        let dims_out = vec![NDDimension {
            size: 4,
            offset: 0,
            binning: 1,
            reverse: true,
        }];
        let out = pool.convert(&src, &dims_out, NDDataType::UInt8).unwrap();
        assert!(!out.dims[0].reverse, "true XOR true must be false");

        // dims_out reverse false: false XOR true = true.
        let dims_out2 = vec![NDDimension {
            size: 4,
            offset: 0,
            binning: 1,
            reverse: false,
        }];
        let out2 = pool.convert(&src, &dims_out2, NDDataType::UInt8).unwrap();
        assert!(out2.dims[0].reverse, "false XOR true must be true");
    }

    /// B3: pool accounting tracks the EXACT requested byte count, not Vec capacity.
    #[test]
    fn test_alloc_tracks_exact_data_size() {
        let pool = NDArrayPool::new(0); // unlimited
        let a = pool
            .alloc(vec![NDDimension::new(333)], NDDataType::UInt16)
            .unwrap();
        // 333 * 2 = 666 — exact, no allocator capacity slack.
        assert_eq!(a.data_size, 666);
        assert_eq!(pool.allocated_bytes(), 666);
    }

    /// B3: a single allocation cannot push `allocated_bytes` past `max_memory`.
    #[test]
    fn test_alloc_strict_max_memory_enforcement() {
        let pool = NDArrayPool::new(1000);
        // 600 bytes — fits.
        let _a = pool
            .alloc(vec![NDDimension::new(600)], NDDataType::UInt8)
            .unwrap();
        assert_eq!(pool.allocated_bytes(), 600);
        // 500 more would total 1100 > 1000 — must be rejected.
        let r = pool.alloc(vec![NDDimension::new(500)], NDDataType::UInt8);
        assert!(r.is_err());
        assert!(pool.allocated_bytes() <= 1000);
    }

    /// B6/B7: `convert` output is pool-tracked; releasing it accounts consistently.
    #[test]
    fn test_convert_output_is_pool_tracked() {
        let pool = NDArrayPool::new(1_000_000);
        let src = make_4x4_u8();
        let before_alloc = pool.num_alloc_buffers();
        let dims_out = vec![
            NDDimension {
                size: 4,
                offset: 0,
                binning: 1,
                reverse: false,
            },
            NDDimension {
                size: 4,
                offset: 0,
                binning: 1,
                reverse: false,
            },
        ];
        let out = pool.convert(&src, &dims_out, NDDataType::UInt8).unwrap();
        assert_eq!(out.pool_id, pool.id());
        assert_eq!(out.data_size, 16);
        // convert allocated a fresh buffer through the pool.
        assert_eq!(pool.num_alloc_buffers(), before_alloc + 1);
        let bytes_with_out = pool.allocated_bytes();
        assert_eq!(bytes_with_out, 16);
        // Releasing it returns the exact data_size to the free list — no drift.
        pool.release(out);
        assert_eq!(pool.num_free_buffers(), 1);
        assert_eq!(pool.allocated_bytes(), 16);
    }

    /// B8: `convert` / `convert_type` reject compressed input.
    #[test]
    fn test_convert_rejects_compressed_input() {
        let pool = NDArrayPool::new(1_000_000);
        let mut src = make_4x4_u8();
        src.codec = Some(crate::codec::Codec {
            name: crate::codec::CodecName::LZ4,
            compressed_size: 4,
            level: 0,
            shuffle: 0,
            compressor: 0,
            original_data_type: crate::ndarray::NDDataType::UInt8,
        });
        let dims_out = vec![
            NDDimension {
                size: 4,
                offset: 0,
                binning: 1,
                reverse: false,
            },
            NDDimension {
                size: 4,
                offset: 0,
                binning: 1,
                reverse: false,
            },
        ];
        assert!(pool.convert(&src, &dims_out, NDDataType::UInt8).is_err());
        assert!(pool.convert_type(&src, NDDataType::UInt16).is_err());
    }

    /// G1: `release` of a foreign array must not corrupt this pool's accounting.
    #[test]
    fn test_release_foreign_array_rejected() {
        let pool_a = NDArrayPool::new(1_000_000);
        let pool_b = NDArrayPool::new(1_000_000);
        let arr = pool_a
            .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
            .unwrap();
        let bytes_b_before = pool_b.allocated_bytes();
        let free_b_before = pool_b.num_free_buffers();
        // Release pool A's array into pool B — must be rejected.
        pool_b.release(arr);
        assert_eq!(pool_b.allocated_bytes(), bytes_b_before);
        assert_eq!(pool_b.num_free_buffers(), free_b_before);
    }

    /// G1: a non-pool array (pool_id == 0) is also rejected by `release`.
    #[test]
    fn test_release_non_pool_array_rejected() {
        let pool = NDArrayPool::new(1_000_000);
        let arr = NDArray::new(vec![NDDimension::new(10)], NDDataType::UInt8);
        assert_eq!(arr.pool_id, 0);
        pool.release(arr);
        assert_eq!(pool.num_free_buffers(), 0);
        assert_eq!(pool.allocated_bytes(), 0);
    }

    /// G2: `copy` into a fresh array copies data, dims, type.
    #[test]
    fn test_copy_allocates_and_copies() {
        let pool = NDArrayPool::new(1_000_000);
        let mut src = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        if let NDDataBuffer::U8(ref mut v) = src.data {
            v.copy_from_slice(&[9, 8, 7, 6]);
        }
        let out = pool.copy(&src, None, true, true, true).unwrap();
        assert_eq!(out.pool_id, pool.id());
        assert_eq!(out.dims.len(), 1);
        if let NDDataBuffer::U8(ref v) = out.data {
            assert_eq!(v, &[9, 8, 7, 6]);
        } else {
            panic!("wrong type");
        }
    }

    /// G2: `pre_allocate_buffers` warms the free list with reusable buffers.
    #[test]
    fn test_pre_allocate_buffers_warms_free_list() {
        let pool = NDArrayPool::new(10_000_000);
        let template = pool
            .alloc(vec![NDDimension::new(256)], NDDataType::UInt16)
            .unwrap();
        pool.pre_allocate_buffers(&template, 3).unwrap();
        assert_eq!(pool.num_free_buffers(), 3);
    }

    /// BUG 1 regression: concurrent reuse-grow must not overshoot
    /// `max_memory`.
    ///
    /// Many threads each take a small free-list buffer and grow it (a
    /// reuse-grow). The non-atomic load+fetch_add this test guards against
    /// let two threads both pass the limit check on the same `current`
    /// reading and both increment, pushing `allocated_bytes` past
    /// `max_memory`. With the CAS loop, the invariant
    /// `allocated_bytes <= max_memory` must hold after every successful
    /// alloc.
    #[test]
    fn test_concurrent_reuse_grow_does_not_overshoot_max_memory() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::thread;

        const N: usize = 16;
        // Repeat to exercise the race window.
        for _ in 0..50 {
            // N free buffers, each tracked at data_size = 100 bytes:
            // allocated_bytes = 1600. max_memory = 2000 leaves only 400 bytes
            // of headroom. Each reuse-grow from 100 -> 200 bytes adds 100, so
            // at most 4 of the N grows may succeed; the rest must be rejected.
            // With a non-atomic load+fetch_add, more than 4 succeed and
            // allocated_bytes overshoots 2000.
            let pool = Arc::new(NDArrayPool::new(2000));
            // Build N genuine reuse-grow candidates: each buffer is allocated
            // and tracked at 100 bytes (data_size = 100) but its backing Vec is
            // reserved to >= 200 bytes of capacity. A later 200-byte request
            // then selects it (capacity 200 >= 200, within the 1.5x threshold)
            // and takes the reuse-GROW branch (200 > old data_size 100). This
            // makes the reuse-grow path deterministic regardless of allocator
            // slack.
            let mut warm = Vec::with_capacity(N);
            for _ in 0..N {
                let mut a = pool
                    .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
                    .unwrap();
                // data_size stays 100 (pool accounting); only the Vec capacity
                // grows. swap the buffer for one with len 100 but capacity 200.
                if let NDDataBuffer::U8(ref mut v) = a.data {
                    let mut big = Vec::with_capacity(200);
                    big.resize(100, 0u8);
                    *v = big;
                }
                assert_eq!(a.data_size, 100);
                assert!(a.data.capacity_bytes() >= 200);
                warm.push(a);
            }
            for a in warm {
                pool.release(a);
            }
            assert_eq!(pool.allocated_bytes(), 1600);
            assert_eq!(pool.num_free_buffers(), N as u32);

            let overshoot = Arc::new(AtomicBool::new(false));
            let mut handles = Vec::new();
            for _ in 0..N {
                let pool = pool.clone();
                let overshoot = overshoot.clone();
                handles.push(thread::spawn(move || {
                    // Reuse a 100-byte free buffer, grow it to 200 bytes.
                    let res = pool.alloc(vec![NDDimension::new(200)], NDDataType::UInt8);
                    if res.is_ok() && pool.allocated_bytes() > pool.max_memory() as u64 {
                        overshoot.store(true, Ordering::Relaxed);
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }

            assert!(
                !overshoot.load(Ordering::Relaxed),
                "allocated_bytes overshot max_memory during concurrent reuse-grow"
            );
            assert!(
                pool.allocated_bytes() <= pool.max_memory() as u64,
                "final allocated_bytes {} > max_memory {}",
                pool.allocated_bytes(),
                pool.max_memory()
            );
        }
    }

    /// G11: `report` produces a non-empty diagnostic dump.
    #[test]
    fn test_pool_report_nonempty() {
        let pool = NDArrayPool::new(1_000_000);
        let _ = pool
            .alloc(vec![NDDimension::new(10)], NDDataType::UInt8)
            .unwrap();
        let r = pool.report(10);
        assert!(r.contains("NDArrayPool"));
        assert!(r.contains("numBuffers"));
    }
}
