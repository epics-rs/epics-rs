use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::sync::Arc;

use crate::ndarray::NDArray;
use crate::ndarray_pool::NDArrayPool;

/// Pool-aware wrapper. On final drop, returns array to pool.
pub struct PooledNDArray {
    array: ManuallyDrop<NDArray>,
    pool: Arc<NDArrayPool>,
}

impl Deref for PooledNDArray {
    type Target = NDArray;
    fn deref(&self) -> &NDArray {
        &self.array
    }
}

impl Drop for PooledNDArray {
    fn drop(&mut self) {
        // SAFETY: only taken once in drop, never accessed after
        let array = unsafe { ManuallyDrop::take(&mut self.array) };
        self.pool.release(array);
    }
}

/// Cloneable handle. Inner Arc ensures pool return on last clone drop.
pub type NDArrayHandle = Arc<PooledNDArray>;

/// Create a pool-aware handle wrapping an NDArray.
pub fn pooled_array(array: NDArray, pool: &Arc<NDArrayPool>) -> NDArrayHandle {
    Arc::new(PooledNDArray {
        array: ManuallyDrop::new(array),
        pool: Arc::clone(pool),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ndarray::{NDDataType, NDDimension};

    #[test]
    fn test_pooled_array_returns_to_pool_on_drop() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let arr = pool
            .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
            .unwrap();
        assert_eq!(pool.num_free_buffers(), 0);

        let handle = pooled_array(arr, &pool);
        drop(handle);

        assert_eq!(pool.num_free_buffers(), 1);
    }

    #[test]
    fn test_clone_keeps_alive_drop_both_returns() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let arr = pool
            .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
            .unwrap();

        let handle = pooled_array(arr, &pool);
        let handle2 = handle.clone();

        drop(handle);
        assert_eq!(pool.num_free_buffers(), 0, "still one clone alive");

        drop(handle2);
        assert_eq!(pool.num_free_buffers(), 1, "both dropped, returned to pool");
    }

    #[test]
    fn test_deref_access() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let arr = pool
            .alloc(vec![NDDimension::new(50)], NDDataType::Float64)
            .unwrap();
        let id = arr.unique_id;

        let handle = pooled_array(arr, &pool);
        assert_eq!(handle.unique_id, id);
        assert_eq!(handle.data.len(), 50);
        assert_eq!(handle.dims[0].size, 50);
    }

    #[test]
    fn test_alloc_handle_via_pool() {
        let pool = Arc::new(NDArrayPool::new(1_000_000));
        let handle =
            NDArrayPool::alloc_handle(&pool, vec![NDDimension::new(64)], NDDataType::UInt16)
                .unwrap();
        assert_eq!(handle.data.len(), 64);
        let alloc_before = pool.num_alloc_buffers();

        drop(handle);
        assert_eq!(pool.num_free_buffers(), 1);
        assert_eq!(pool.num_alloc_buffers(), alloc_before);
    }
}
