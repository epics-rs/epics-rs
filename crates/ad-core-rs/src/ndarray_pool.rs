use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::error::{ADError, ADResult};
use crate::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension};
use crate::ndarray_handle::{NDArrayHandle, pooled_array};
use crate::timestamp::EpicsTimestamp;

/// NDArray factory with free-list reuse and memory tracking.
///
/// Mimics C++ ADCore's NDArrayPool: on alloc, checks the free list for a
/// buffer with sufficient capacity. On release, returns the buffer to the
/// free list for future reuse. The free list is sorted by capacity (descending)
/// and excess entries are dropped when max_memory is exceeded.
pub struct NDArrayPool {
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
            max_memory,
            allocated_bytes: AtomicU64::new(0),
            next_unique_id: AtomicI32::new(1),
            free_list: Mutex::new(Vec::new()),
            num_alloc_buffers: AtomicU32::new(0),
            num_free_buffers: AtomicU32::new(0),
        }
    }

    /// Allocate an NDArray. Tries to reuse a free-list entry with sufficient capacity.
    pub fn alloc(&self, dims: Vec<NDDimension>, data_type: NDDataType) -> ADResult<NDArray> {
        let num_elements: usize = dims.iter().map(|d| d.size).product();
        let needed_bytes = num_elements * data_type.element_size();

        // Try to find a reusable buffer in the free list
        let reused = {
            let mut free = self.free_list.lock();
            // Find smallest buffer that is large enough (free list sorted descending by capacity)
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
                let arr = free.swap_remove(idx);
                self.num_free_buffers.fetch_sub(1, Ordering::Relaxed);
                Some(arr)
            } else {
                None
            }
        };

        let mut arr = if let Some(mut reused) = reused {
            // Reuse: retype the buffer if needed, resize to match
            if reused.data.data_type() != data_type {
                // Must reallocate with new type, but we keep the allocation tracked
                let old_cap = reused.data.capacity_bytes();
                reused.data = NDDataBuffer::zeros(data_type, num_elements);
                let new_cap = reused.data.capacity_bytes();
                // Adjust allocated_bytes for the difference
                if new_cap > old_cap {
                    let diff = new_cap - old_cap;
                    let current = self.allocated_bytes.load(Ordering::Relaxed);
                    if current + diff as u64 > self.max_memory as u64 {
                        return Err(ADError::PoolExhausted(needed_bytes, self.max_memory));
                    }
                    self.allocated_bytes
                        .fetch_add(diff as u64, Ordering::Relaxed);
                } else {
                    let diff = old_cap - new_cap;
                    self.allocated_bytes
                        .fetch_sub(diff as u64, Ordering::Relaxed);
                }
            } else {
                reused.data.resize(num_elements);
            }
            reused.dims = dims;
            reused.attributes.clear();
            reused.codec = None;
            reused
        } else {
            // Fresh allocation
            let current = self.allocated_bytes.load(Ordering::Relaxed);
            if current + needed_bytes as u64 > self.max_memory as u64 {
                return Err(ADError::PoolExhausted(needed_bytes, self.max_memory));
            }
            self.allocated_bytes
                .fetch_add(needed_bytes as u64, Ordering::Relaxed);
            self.num_alloc_buffers.fetch_add(1, Ordering::Relaxed);
            NDArray::new(dims, data_type)
        };

        arr.unique_id = self.next_unique_id.fetch_add(1, Ordering::Relaxed);
        arr.timestamp = EpicsTimestamp::now();
        Ok(arr)
    }

    /// Allocate a copy of an existing NDArray (new unique_id, data cloned).
    pub fn alloc_copy(&self, source: &NDArray) -> ADResult<NDArray> {
        let bytes = source.data.total_bytes();
        let current = self.allocated_bytes.load(Ordering::Relaxed);
        if current + bytes as u64 > self.max_memory as u64 {
            return Err(ADError::PoolExhausted(bytes, self.max_memory));
        }
        self.allocated_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
        self.num_alloc_buffers.fetch_add(1, Ordering::Relaxed);

        let mut copy = source.clone();
        copy.unique_id = self.next_unique_id.fetch_add(1, Ordering::Relaxed);
        copy.timestamp = EpicsTimestamp::now();
        Ok(copy)
    }

    /// Return an array to the free list for future reuse.
    pub fn release(&self, array: NDArray) {
        let cap = array.data.capacity_bytes();
        let mut free = self.free_list.lock();
        free.push(array);
        self.num_free_buffers.fetch_add(1, Ordering::Relaxed);

        // If total allocated exceeds max_memory, drop largest free entries
        let total = self.allocated_bytes.load(Ordering::Relaxed) as usize;
        if total > self.max_memory && !free.is_empty() {
            // Sort descending by capacity so we drop largest first
            free.sort_by(|a, b| b.data.capacity_bytes().cmp(&a.data.capacity_bytes()));
            let mut excess = total.saturating_sub(self.max_memory);
            while excess > 0 && !free.is_empty() {
                let dropped = free.remove(0);
                let dropped_cap = dropped.data.capacity_bytes();
                self.allocated_bytes
                    .fetch_sub(dropped_cap.min(total) as u64, Ordering::Relaxed);
                self.num_free_buffers.fetch_sub(1, Ordering::Relaxed);
                self.num_alloc_buffers.fetch_sub(1, Ordering::Relaxed);
                if dropped_cap >= excess {
                    break;
                }
                excess -= dropped_cap;
            }
        }
        let _ = cap;
    }

    /// Clear all entries from the free list.
    pub fn empty_free_list(&self) {
        let mut free = self.free_list.lock();
        let count = free.len() as u32;
        for arr in free.drain(..) {
            let cap = arr.data.capacity_bytes();
            self.allocated_bytes
                .fetch_sub(cap as u64, Ordering::Relaxed);
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
    fn test_alloc_tracks_bytes() {
        let pool = NDArrayPool::new(1_000_000);
        let _ = pool
            .alloc(vec![NDDimension::new(100)], NDDataType::Float64)
            .unwrap();
        assert_eq!(pool.allocated_bytes(), 800);
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
        assert_eq!(pool.allocated_bytes(), 40);
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
        let alloc_bytes_after_first = pool.allocated_bytes();
        assert_eq!(pool.num_alloc_buffers(), 1);

        // Release back to free list
        pool.release(arr);
        assert_eq!(pool.num_free_buffers(), 1);

        // Alloc again — should reuse the freed buffer
        let arr2 = pool
            .alloc(vec![NDDimension::new(50)], NDDataType::UInt8)
            .unwrap();
        assert_eq!(pool.num_free_buffers(), 0);
        // allocated_bytes should be unchanged (reused buffer)
        assert_eq!(pool.allocated_bytes(), alloc_bytes_after_first);
        assert_eq!(arr2.data.len(), 50);
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

        // Request 500 bytes — should pick medium (1000 cap), not large (10000 cap)
        let reused = pool
            .alloc(vec![NDDimension::new(500)], NDDataType::UInt8)
            .unwrap();
        assert_eq!(pool.num_free_buffers(), 2);
        // The reused buffer should have capacity >= 1000 (from medium)
        assert!(reused.data.capacity_bytes() >= 1000);
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
            .alloc(vec![NDDimension::new(5)], NDDataType::UInt8)
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
}
