use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;

use crate::attributes::NDAttributeList;
use crate::codec::Codec;
use crate::driver::ndarray_driver::ArrayOwner;
use crate::error::{ADError, ADResult};
use crate::ndarray::{NDArray, NDDataBuffer, NDDataType, NDDimension, PoolTie};

/// If a free-list buffer is more than this ratio larger than needed, discard
/// it and allocate fresh to avoid wasting memory.
const THRESHOLD_SIZE_RATIO: f64 = 1.5;

/// Process-wide source of pool identities. Each `NDArrayPool` gets a unique
/// non-zero id so `release` can verify an array belongs to it (C++
/// NDArrayPool.cpp:352 checks `pArray->pNDArrayPool == this`).
static NEXT_POOL_ID: AtomicU64 = AtomicU64::new(1);

/// Leave to change what only the pool sets on an [`NDDataBuffer`]: its
/// element type ([`NDDataBuffer::retype`]) and, with the array's tie to the
/// pool, its charge ([`NDArray::tie`]). Only this module makes one, as C's
/// pool is the one writer of `dataType` and `dataSize` on a buffer it hands
/// out.
pub(crate) struct PoolOnly(());

/// A recycled buffer waiting in the free list, with the `data_size` the pool
/// charged for it when it was allocated (C++ keeps the whole `NDArray`; only
/// its buffer and its accounting are worth keeping).
struct FreeBuffer {
    data: NDDataBuffer,
    data_size: usize,
}

/// NDArray factory with free-list reuse and memory tracking.
///
/// Mimics C++ ADCore's NDArrayPool: on alloc, checks the free list for a
/// buffer with sufficient capacity. Every array it allocates carries a link
/// back to the pool and returns its buffer to the free list when dropped —
/// the C++ `NDArray::release` at refcount zero — so nothing has to call
/// [`release`](Self::release) by hand. The free list is sorted by capacity
/// (descending) and excess entries are dropped when max_memory is exceeded.
pub struct NDArrayPool {
    /// The link every allocated array carries; `Weak` so the arrays never keep
    /// a pool alive past its owner.
    this: Weak<Self>,
    /// Unique identity of this pool, stamped onto every array it allocates.
    id: u64,
    /// The driver that built the pool (C++ `pDriver_`), whose queued count
    /// every array from it is counted on wherever it is queued
    /// (NDPluginDriver.cpp:462, :538).
    owner: Weak<ArrayOwner>,
    max_memory: usize,
    allocated_bytes: AtomicU64,
    next_unique_id: AtomicI32,
    free_list: Mutex<Vec<FreeBuffer>>,
    num_alloc_buffers: AtomicU32,
    num_free_buffers: AtomicU32,
}

impl NDArrayPool {
    /// A pool is always shared: its arrays refer back to it from wherever
    /// they end up, so it lives behind an `Arc` from the start.
    ///
    /// No driver owns it, so its arrays are queued uncounted.
    pub fn new(max_memory: usize) -> Arc<Self> {
        Self::with_owner(max_memory, Weak::new())
    }

    /// C++ `NDArrayPool(pDriver, maxMemory)`: the pool of the driver `owner`.
    /// Reached from outside as [`DriverPool::for_driver`].
    pub(crate) fn for_driver(max_memory: usize, owner: &Arc<ArrayOwner>) -> Arc<Self> {
        Self::with_owner(max_memory, Arc::downgrade(owner))
    }

    fn with_owner(max_memory: usize, owner: Weak<ArrayOwner>) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            this: this.clone(),
            id: NEXT_POOL_ID.fetch_add(1, Ordering::Relaxed),
            owner,
            max_memory,
            allocated_bytes: AtomicU64::new(0),
            next_unique_id: AtomicI32::new(1),
            free_list: Mutex::new(Vec::new()),
            num_alloc_buffers: AtomicU32::new(0),
            num_free_buffers: AtomicU32::new(0),
        })
    }

    /// Wrap a buffer in an array that belongs to this pool.
    fn adopt(&self, dims: Vec<NDDimension>, data: NDDataBuffer, data_size: usize) -> NDArray {
        // The "Initialize fields" block of C++ NDArrayPool::alloc
        // (NDArrayPool.cpp:187-199) sets neither `epicsTS` nor `timeStamp`,
        // and neither does `with_data`: the driver owns both and stamps them
        // together via asynNDArrayDriver::updateTimeStamps
        // (asynNDArrayDriver.cpp:832-836). Stamping only `epicsTS` here left
        // `timeStamp` at 0.0 (fresh buffer) or at the previous frame's value
        // (reused buffer), so the two published timestamps disagreed. See
        // NDArray::update_time_stamps.
        let mut array = NDArray::with_data(dims, data);
        array.unique_id = self.next_unique_id.fetch_add(1, Ordering::Relaxed);
        array.data_size = data_size;
        array.tie(PoolOnly(()), PoolTie::Charged(self.this.clone()));
        array
    }

    /// The driver that owns the pool, `None` for an unowned pool or a driver
    /// that is gone.
    pub fn owner(&self) -> Option<Arc<ArrayOwner>> {
        self.owner.upgrade()
    }

    /// Identity of this pool (what `NDArray::pool_id` reports for its arrays).
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
        self.alloc_sized(dims, data_type, num_elements * data_type.element_size())
    }

    /// [`alloc`](Self::alloc) with an explicit payload size, C++
    /// `NDArrayPool::alloc`'s `dataSize` argument: the buffer holds
    /// `data_size` bytes of `data_type` elements whatever `dims` say, as a
    /// codec output does (NDPluginCodec.cpp:64-84). `data_size` must be a
    /// whole number of elements.
    pub fn alloc_sized(
        &self,
        dims: Vec<NDDimension>,
        data_type: NDDataType,
        data_size: usize,
    ) -> ADResult<NDArray> {
        let element_size = data_type.element_size();
        if data_size % element_size != 0 {
            return Err(ADError::InvalidDimensions(format!(
                "{data_size} bytes is not a whole number of {data_type:?} elements"
            )));
        }
        let num_elements = data_size / element_size;
        let needed_bytes = data_size;

        // C++ NDArrayPool::alloc (NDArrayPool.cpp:161-183): the smallest free
        // buffer whose tracked `dataSize` holds the request. A buffer over
        // THRESHOLD_SIZE_RATIO of the request is freed and allocated afresh. A reused buffer keeps its `dataSize`,
        // which is never below the request, so reuse charges nothing.
        let reused = {
            let mut free = self.free_list.lock();
            let found = free
                .iter()
                .enumerate()
                .filter(|(_, b)| b.data_size >= needed_bytes)
                .min_by_key(|(_, b)| b.data_size)
                .map(|(i, _)| i);
            match found {
                Some(idx)
                    if free[idx].data_size as f64 > needed_bytes as f64 * THRESHOLD_SIZE_RATIO =>
                {
                    // Its buffer is freed; the fresh allocation below
                    // stands in for the C array object kept.
                    let dropped = free.remove(idx);
                    self.num_free_buffers.fetch_sub(1, Ordering::Relaxed);
                    self.allocated_bytes
                        .fetch_sub(dropped.data_size as u64, Ordering::Relaxed);
                    self.num_alloc_buffers.fetch_sub(1, Ordering::Relaxed);
                    None
                }
                Some(idx) => {
                    self.num_free_buffers.fetch_sub(1, Ordering::Relaxed);
                    Some(free.remove(idx))
                }
                None => None,
            }
        };

        if let Some(reused) = reused {
            let mut array = self.adopt(dims, reused.data, reused.data_size);
            array.data.retype(PoolOnly(()), data_type, num_elements);
            return Ok(array);
        }

        // Fresh allocation with CAS loop to avoid TOCTOU race. The reserved
        // amount is exactly `needed_bytes`; no capacity slack is ever added.
        if self.max_memory > 0 {
            loop {
                let current = self.allocated_bytes.load(Ordering::Relaxed);
                if current + needed_bytes as u64 > self.max_memory as u64 {
                    let mut freed_enough = false;
                    {
                        let mut free = self.free_list.lock();
                        let mut reclaimed = 0u64;
                        let over =
                            (current + needed_bytes as u64).saturating_sub(self.max_memory as u64);
                        // Largest `dataSize` first, the end of C++'s multiset
                        // (NDArrayPool.cpp:207-218).
                        while let Some(idx) = free
                            .iter()
                            .enumerate()
                            .max_by_key(|(_, b)| b.data_size)
                            .map(|(i, _)| i)
                            && reclaimed < over
                        {
                            let dropped = free.remove(idx);
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
                        // C counts the array it was about to fill.
                        self.report_limit(self.num_alloc_buffers() + 1);
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
        let data = NDDataBuffer::zeros(data_type, num_elements);
        Ok(self.adopt(dims, data, needed_bytes))
    }

    /// The refusal line C prints on the owning driver's `pasynUserSelf`
    /// (NDArrayPool.cpp:230-232).
    fn report_limit(&self, buffers: u32) {
        if let Some(port) = self.owner().and_then(|o| o.port()) {
            port.trace().output(
                port.port_name(),
                asyn_rs::trace::TraceMask::ERROR,
                &format!(
                    "NDArrayPool::alloc:: error: reached limit of {} memory ({buffers} buffers)\n",
                    self.max_memory
                ),
            );
        }
    }

    /// Allocate a copy of an existing NDArray: data, both stamps, attributes,
    /// the codec as [`Codec::copied`] and `unique_id` carry across (C++
    /// NDArrayPool.cpp:283-302).
    /// Tries the free list first (via alloc()), then copies data from source.
    pub fn alloc_copy(&self, source: &NDArray) -> ADResult<NDArray> {
        let mut copy = self.alloc_for(source)?;
        crate::convert::convert_buffer_into(&source.data, &mut copy.data);
        copy.unique_id = source.unique_id;
        copy.copy_time_stamps_from(source);
        copy.attributes = source.attributes.clone();
        copy.codec = source.codec.as_ref().map(Codec::copied);
        Ok(copy)
    }

    /// An output for a copy of `source`: C's `alloc` of its dims and type
    /// (NDArrayPool.cpp:280), for a codec frame the type it was compressed
    /// from. C then cuts a frame larger than that (:298), as a codec's
    /// overhead can make one (NDPluginCodec.cpp:466); that defect is not
    /// copied, so such a frame is charged its payload.
    fn alloc_for(&self, source: &NDArray) -> ADResult<NDArray> {
        let dims = source.dims.clone();
        let data_type = source.data.data_type();
        let Some(codec) = &source.codec else {
            return self.alloc(dims, data_type);
        };
        let elements: usize = dims.iter().map(|d| d.size).product();
        let uncompressed = elements * codec.original_data_type.element_size();
        self.alloc_sized(dims, data_type, uncompressed.max(source.data.total_bytes()))
    }

    /// Give an array back to the pool now.
    ///
    /// Dropping does the same: every array this pool allocated returns its
    /// buffer to the free list from its destructor. An array from another
    /// pool goes back to *that* pool, and an array from no pool is simply
    /// freed — this pool's free list and accounting are never touched by
    /// either (C++ NDArrayPool.cpp:352 refuses a foreign array outright).
    pub fn release(&self, array: NDArray) {
        drop(array);
    }

    /// The destructor's half of [`release`](Self::release): put a buffer this
    /// pool charged for back on the free list. C++ release never trims it
    /// (NDArrayPool.cpp:360-366); only the next alloc evicts.
    pub(crate) fn recycle(&self, data: NDDataBuffer, data_size: usize) {
        self.free_list.lock().push(FreeBuffer { data, data_size });
        self.num_free_buffers.fetch_add(1, Ordering::Relaxed);
    }

    /// Clear all entries from the free list.
    pub fn empty_free_list(&self) {
        let mut free = self.free_list.lock();
        let count = free.len() as u32;
        for buf in free.drain(..) {
            self.allocated_bytes
                .fetch_sub(buf.data_size as u64, Ordering::Relaxed);
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
            None => self.alloc_for(src)?,
        };

        out.unique_id = src.unique_id;
        out.copy_time_stamps_from(src);
        if copy_dimensions {
            out.dims = src.dims.clone();
        }
        out.codec = src.codec.as_ref().map(Codec::copied);

        if copy_data_type {
            let len = if copy_data {
                src.data.len()
            } else {
                out.data.len()
            };
            out.data.retype(PoolOnly(()), src.data.data_type(), len);
        }
        if copy_data {
            // Output keeps its own type: convert pixel values.
            crate::convert::convert_buffer_into(&src.data, &mut out.data);
        }

        out.attributes.clear();
        out.attributes.copy_from(&src.attributes);
        Ok(out)
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
        crate::convert::convert_buffer_into(&src.data, &mut out.data);
        out.unique_id = src.unique_id;
        out.copy_time_stamps_from(src);
        out.attributes.copy_from(&src.attributes);
        Ok(out)
    }

    /// Full convert with dimension changes: extract sub-region, bin, reverse.
    ///
    /// The conversion kernel itself lives in [`crate::convert::convert_dims_into`]
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
        let out_dims = crate::convert::output_dims(src, dims_out)?;
        let mut arr = self.alloc(out_dims, target_type)?;
        crate::convert::convert_dims_into(src, dims_out, &mut arr.data)?;
        arr.unique_id = src.unique_id;
        arr.copy_time_stamps_from(src);
        arr.attributes.copy_from(&src.attributes);

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
            out.push_str("  freeList: (index, dataSize, capacity, dataType, numElements)\n");
            for (i, buf) in free.iter().enumerate() {
                out.push_str(&format!(
                    "    {} {} {} {:?} {}\n",
                    i,
                    buf.data_size,
                    buf.data.capacity_bytes(),
                    buf.data.data_type(),
                    buf.data.len()
                ));
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

/// A pool a driver owns: C's `NDArrayPool` with a `pDriver_`, the only kind
/// whose arrays C ever hands a plugin.
///
/// Its allocations are [`DriverArray`]s, and those are what the publish API
/// takes, so an array no driver counts as queued cannot reach a plugin. An
/// [`NDArrayPool`] built with [`NDArrayPool::new`] stays for scratch arrays.
#[derive(Clone)]
pub struct DriverPool {
    pool: Arc<NDArrayPool>,
    owner: Arc<ArrayOwner>,
}

impl DriverPool {
    /// C++ `NDArrayPool(pDriver, maxMemory)`.
    pub fn for_driver(max_memory: usize, owner: &Arc<ArrayOwner>) -> Self {
        Self {
            pool: NDArrayPool::for_driver(max_memory, owner),
            owner: Arc::clone(owner),
        }
    }

    /// A pool with a driver of its own that no port reads, for a tool or a
    /// test that publishes without a driver behind it.
    pub fn standalone(max_memory: usize) -> Self {
        Self::for_driver(max_memory, &ArrayOwner::new())
    }

    /// The driver behind `pool`, if it has one that is still there.
    pub(crate) fn of(pool: Arc<NDArrayPool>) -> Option<Self> {
        let owner = pool.owner()?;
        Some(Self { pool, owner })
    }

    pub fn pool(&self) -> &Arc<NDArrayPool> {
        &self.pool
    }

    pub fn owner(&self) -> &Arc<ArrayOwner> {
        &self.owner
    }

    fn own(&self, array: ADResult<NDArray>) -> ADResult<DriverArray> {
        Ok(DriverArray {
            array: array?,
            pool: self.clone(),
        })
    }

    /// [`NDArrayPool::alloc`].
    pub fn alloc(&self, dims: Vec<NDDimension>, data_type: NDDataType) -> ADResult<DriverArray> {
        self.own(self.pool.alloc(dims, data_type))
    }

    /// [`NDArrayPool::alloc_sized`].
    pub fn alloc_sized(
        &self,
        dims: Vec<NDDimension>,
        data_type: NDDataType,
        data_size: usize,
    ) -> ADResult<DriverArray> {
        self.own(self.pool.alloc_sized(dims, data_type, data_size))
    }

    /// [`NDArrayPool::alloc_copy`].
    pub fn alloc_copy(&self, source: &NDArray) -> ADResult<DriverArray> {
        self.own(self.pool.alloc_copy(source))
    }

    /// [`NDArrayPool::convert`].
    pub fn convert(
        &self,
        src: &NDArray,
        dims_out: &[NDDimension],
        target_type: NDDataType,
    ) -> ADResult<DriverArray> {
        self.own(self.pool.convert(src, dims_out, target_type))
    }

    /// Take an array this pool allocated through a helper that works on the
    /// plain [`NDArrayPool`], such as [`crate::roi::crop_roi`]; `None` for
    /// any other array.
    pub fn adopt(&self, array: NDArray) -> Option<DriverArray> {
        let charged = matches!(array.pool_tie(), Some(PoolTie::Charged(_)));
        (charged && array.pool_id() == self.pool.id()).then(|| DriverArray {
            array,
            pool: self.clone(),
        })
    }

    /// [`NDArrayPool::release`].
    pub fn release(&self, array: DriverArray) {
        drop(array);
    }

    /// [`NDArrayPool::convert_type`].
    pub fn convert_type(&self, src: &NDArray, target_type: NDDataType) -> ADResult<DriverArray> {
        self.own(self.pool.convert_type(src, target_type))
    }

    /// What C's `endProcessCallbacks` makes of a plugin's output
    /// (NDPluginDriver.cpp:285-291). `copied` is C's `copyArray`: the output
    /// is copied into this pool and counted on this pool's driver, and the
    /// copy keeps only the codec's name (NDArrayPool.cpp:293). Otherwise an
    /// array from a driver's pool goes out as it is, counted on that driver,
    /// and any other is copied into this pool first. `attributes` are what
    /// `getAttributes` merges onto it; an output someone else still holds is
    /// copied before they are set on it.
    pub(crate) fn publishable(
        &self,
        mut array: Arc<NDArray>,
        attributes: Option<&NDAttributeList>,
        copied: bool,
    ) -> ADResult<PublishedArray> {
        if !copied && let Some(pool) = array.pool().and_then(Self::of) {
            let Some(attributes) = attributes else {
                return Ok(PublishedArray { array, pool });
            };
            if let Some(own) = Arc::get_mut(&mut array) {
                own.attributes.copy_from(attributes);
                return Ok(PublishedArray { array, pool });
            }
        }
        let mut copy = self.alloc_copy(&array)?;
        if let Some(attributes) = attributes {
            copy.attributes.copy_from(attributes);
        }
        Ok(copy.into())
    }
}

impl std::ops::Deref for DriverPool {
    type Target = NDArrayPool;

    fn deref(&self) -> &NDArrayPool {
        &self.pool
    }
}

/// An array from a [`DriverPool`], still the driver's to fill in.
pub struct DriverArray {
    array: NDArray,
    pool: DriverPool,
}

impl DriverArray {
    /// Hand the array over for publishing; nothing mutates it after this.
    pub fn share(self) -> PublishedArray {
        PublishedArray {
            array: Arc::new(self.array),
            pool: self.pool,
        }
    }
}

impl std::ops::Deref for DriverArray {
    type Target = NDArray;

    fn deref(&self) -> &NDArray {
        &self.array
    }
}

impl std::ops::DerefMut for DriverArray {
    fn deref_mut(&mut self) -> &mut NDArray {
        &mut self.array
    }
}

/// What a plugin receives: a shared array with the driver pool it is counted
/// on, C's `pArray` with its `pNDArrayPool` and `pDriver`.
///
/// Only a [`DriverPool`] makes one, so every array a plugin queues is
/// counted on a driver's `NDNumQueuedArrays` (NDPluginDriver.cpp:462, :538).
///
/// ```compile_fail
/// use ad_core_rs::ndarray::{NDArray, NDDataType};
/// use ad_core_rs::plugin::channel::NDArraySender;
///
/// async fn uncounted(sender: &NDArraySender) {
///     let array = NDArray::new(vec![], NDDataType::UInt8);
///     sender.publish(std::sync::Arc::new(array)).await;
/// }
/// ```
#[derive(Clone)]
pub struct PublishedArray {
    array: Arc<NDArray>,
    pool: DriverPool,
}

impl PublishedArray {
    pub fn array(&self) -> &Arc<NDArray> {
        &self.array
    }

    /// The pool it is counted on.
    pub fn pool(&self) -> &DriverPool {
        &self.pool
    }

    /// Back to the driver to change, when this is the array's last holder.
    pub fn try_into_driver(self) -> Result<DriverArray, PublishedArray> {
        match Arc::try_unwrap(self.array) {
            Ok(array) => Ok(DriverArray {
                array,
                pool: self.pool,
            }),
            Err(array) => Err(PublishedArray {
                array,
                pool: self.pool,
            }),
        }
    }
}

impl From<DriverArray> for PublishedArray {
    fn from(array: DriverArray) -> Self {
        array.share()
    }
}

impl std::ops::Deref for PublishedArray {
    type Target = NDArray;

    fn deref(&self) -> &NDArray {
        &self.array
    }
}

/// The pool's one charge for a [`PersistentArray`]'s buffer, held by the
/// array and by every publish of it: C's one `NDArray` and its reference
/// count. The buffer goes back to the pool when the last holder goes.
#[derive(Debug)]
pub(crate) struct SharedCharge {
    pool: Weak<NDArrayPool>,
    data_size: usize,
    /// The persistent array's buffer, parked here when it is dropped first.
    parked: Mutex<NDDataBuffer>,
}

impl SharedCharge {
    pub(crate) fn pool(&self) -> &Weak<NDArrayPool> {
        &self.pool
    }

    pub(crate) fn data_size(&self) -> usize {
        self.data_size
    }
}

impl Drop for SharedCharge {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            pool.recycle(std::mem::take(self.parked.get_mut()), self.data_size);
        }
    }
}

/// An array a driver allocates once and refills, C's persistent `NDArray *`
/// such as pilatus `pFlatField` or NDDriverStdArrays `pArrays[0]`: refilled
/// in place whoever holds its last publish.
///
/// A publish is a copy, so a refill never races a plugin, but it is not a
/// second charge: the array and its publishes hold one charge, so
/// the pool counts one buffer until the driver and every plugin have let go,
/// as C's reserve/release of the one array (pilatusDetector.cpp:1728, :290;
/// NDDriverStdArrays.cpp:153-158, :328).
pub struct PersistentArray {
    array: DriverArray,
    shown: Option<PublishedArray>,
}

impl PersistentArray {
    pub fn new(mut array: DriverArray) -> Self {
        if let Some(PoolTie::Charged(pool)) = array.array.pool_tie() {
            let charge = SharedCharge {
                pool: pool.clone(),
                data_size: array.array.data_size,
                parked: Mutex::default(),
            };
            array
                .array
                .tie(PoolOnly(()), PoolTie::Shared(Arc::new(charge)));
        }
        Self { array, shown: None }
    }

    /// The buffer to refill.
    pub fn refill(&mut self) -> &mut NDArray {
        self.shown = None;
        &mut self.array
    }

    /// The array to publish: the same one until the next refill.
    pub fn published(&mut self) -> PublishedArray {
        self.shown
            .get_or_insert_with(|| {
                let mut copy = NDArray::clone(&self.array);
                if let Some(PoolTie::Shared(charge)) = self.array.pool_tie() {
                    copy.tie(PoolOnly(()), PoolTie::Shared(charge.clone()));
                }
                PublishedArray {
                    array: Arc::new(copy),
                    pool: self.array.pool.clone(),
                }
            })
            .clone()
    }
}

impl Drop for PersistentArray {
    fn drop(&mut self) {
        if let Some(PoolTie::Shared(charge)) = self.array.array.pool_tie() {
            let charge = Arc::clone(charge);
            *charge.parked.lock() = std::mem::take(&mut self.array.array.data);
        }
    }
}

impl std::ops::Deref for PersistentArray {
    type Target = NDArray;

    fn deref(&self) -> &NDArray {
        &self.array
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ndarray::{NDDataView, NDDataViewMut};

    /// A refill is never refused, held publish or not, and the pool counts
    /// the persistent array's one buffer throughout, as C's `pFlatField`.
    #[test]
    fn a_persistent_array_is_refilled_whoever_holds_its_publish() {
        let pool = DriverPool::standalone(8);
        let mut flat = PersistentArray::new(
            pool.alloc(vec![NDDimension::new(2)], NDDataType::Int32)
                .unwrap(),
        );
        let counts = |pool: &DriverPool| {
            (
                pool.num_alloc_buffers(),
                pool.num_free_buffers(),
                pool.allocated_bytes(),
            )
        };
        flat.refill().unique_id = 1;
        let first = flat.published();
        assert!(Arc::ptr_eq(first.array(), flat.published().array()));
        assert_eq!(first.pool_id(), pool.id());

        flat.refill().unique_id = 2;
        assert_eq!(first.unique_id, 1);
        assert_eq!(flat.published().unique_id, 2);
        assert_eq!(counts(&pool), (1, 0, 8));

        drop(first);
        assert_eq!(counts(&pool), (1, 0, 8));
        assert!(
            pool.adopt(Arc::unwrap_or_clone(flat.published().array().clone()))
                .is_none()
        );
        drop(flat);
        assert_eq!(counts(&pool), (1, 1, 8));
    }

    /// The persistent array and every publish of it hold one charge, as C's
    /// one `NDArray` and its reserves: with the pool exactly full, a publish
    /// still goes out, and a held publish keeps the buffer charged past the
    /// persistent array, so the next alloc is refused until it is let go.
    #[test]
    fn a_held_publish_keeps_the_persistent_array_charged() {
        let pool = DriverPool::standalone(8);
        let alloc = || pool.alloc(vec![NDDimension::new(2)], NDDataType::Int32);
        let mut current = PersistentArray::new(alloc().unwrap());
        current.refill().unique_id = 1;
        let held = current.published();
        assert_eq!(held.unique_id, 1);
        assert_eq!((pool.num_alloc_buffers(), pool.allocated_bytes()), (1, 8));

        drop(current);
        assert_eq!((pool.num_alloc_buffers(), pool.num_free_buffers()), (1, 0));
        assert!(alloc().is_err(), "the held publish still holds the pool");

        drop(held);
        assert_eq!((pool.num_alloc_buffers(), pool.num_free_buffers()), (1, 1));
        assert!(alloc().is_ok());
    }

    /// A publish is held to the persistent array's one charge: back with the
    /// driver as its last holder, it grows no further than the array could.
    #[test]
    fn a_persistent_array_publish_never_grows_past_its_charge() {
        let pool = DriverPool::standalone(8);
        let mut flat = PersistentArray::new(
            pool.alloc(vec![NDDimension::new(2)], NDDataType::Int32)
                .unwrap(),
        );
        let held = flat.published();
        flat.refill().unique_id = 1;
        let Ok(mut copy) = held.try_into_driver() else {
            panic!("the refill let go of the publish");
        };
        copy.data.resize(3);
        assert_eq!(copy.data.len(), 2);
    }

    /// A reused free buffer is tied to its pool and owner as a fresh one is.
    #[test]
    fn a_reused_free_buffer_is_tied_to_the_pool_and_owner() {
        let owner = ArrayOwner::new();
        let pool = NDArrayPool::for_driver(64, &owner);
        drop(
            pool.alloc(vec![NDDimension::new(8)], NDDataType::UInt8)
                .unwrap(),
        );
        assert_eq!((pool.num_alloc_buffers(), pool.num_free_buffers()), (1, 1));

        let frame = pool
            .alloc(vec![NDDimension::new(8)], NDDataType::UInt8)
            .unwrap();
        assert_eq!((pool.num_alloc_buffers(), pool.num_free_buffers()), (1, 0));
        assert_eq!(pool.allocated_bytes(), 8);
        assert_eq!(frame.pool_id(), pool.id());
        assert!(Arc::ptr_eq(&frame.owner().unwrap(), &owner));
        drop(frame);
    }

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

        // Reuse path: C recycles the whole NDArray and leaves its stamps
        // alone; the port recycles only the buffer, so a reused array starts
        // from the same defaults as a fresh one — and the pair still agrees.
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
        assert_eq!(
            reused.timestamp,
            crate::timestamp::EpicsTimestamp::default()
        );
        assert_eq!(reused.time_stamp, 0.0);
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
        if let NDDataViewMut::U8(v) = source.data.view_mut() {
            v[0] = 1;
            v[1] = 2;
            v[2] = 3;
            v[3] = 4;
        }

        let copy = pool.alloc_copy(&source).unwrap();
        assert_eq!(copy.unique_id, source.unique_id);
        assert_eq!(copy.dims.len(), source.dims.len());
        if let NDDataView::U8(v) = copy.data.view() {
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

    /// C++ finds a free buffer by its tracked `dataSize` (NDArrayPool.cpp:161-165),
    /// which a reuse under another data type keeps.
    #[test]
    fn a_reused_buffer_is_found_by_its_data_size() {
        let pool = NDArrayPool::new(1_000_000);
        let a = pool
            .alloc(vec![NDDimension::new(1000)], NDDataType::UInt8)
            .unwrap();
        pool.release(a);
        let b = pool
            .alloc(vec![NDDimension::new(400)], NDDataType::UInt16)
            .unwrap();
        assert_eq!(b.data_size, 1000);
        pool.release(b);
        let c = pool
            .alloc(vec![NDDimension::new(900)], NDDataType::UInt8)
            .unwrap();
        assert_eq!(c.data_size, 1000);
        assert_eq!(pool.num_alloc_buffers(), 1);
        assert_eq!(pool.allocated_bytes(), 1000);
    }

    /// A free buffer of another element type that holds the request is
    /// reused in its own allocation, as C sets `dataType` on the array it
    /// takes from the free list (NDArrayPool.cpp:191).
    #[test]
    fn a_free_buffer_of_another_type_is_reused_in_its_allocation() {
        let pool = NDArrayPool::new(1_000_000);
        let a = pool
            .alloc(vec![NDDimension::new(1000)], NDDataType::UInt16)
            .unwrap();
        let ptr = a.data.as_u8_slice().as_ptr();
        pool.release(a);

        let b = pool
            .alloc(vec![NDDimension::new(250)], NDDataType::Float64)
            .unwrap();
        assert_eq!(b.data.as_u8_slice().as_ptr(), ptr);
        assert_eq!(
            (b.data.data_type(), b.data.len()),
            (NDDataType::Float64, 250)
        );
        pool.release(b);

        // Narrower and smaller, within THRESHOLD_SIZE_RATIO.
        let c = pool
            .alloc(vec![NDDimension::new(1500)], NDDataType::UInt8)
            .unwrap();
        assert_eq!(c.data.as_u8_slice().as_ptr(), ptr);
        assert_eq!(
            (c.data.data_type(), c.data.len()),
            (NDDataType::UInt8, 1500)
        );
        assert_eq!(
            (pool.num_alloc_buffers(), pool.allocated_bytes()),
            (1, 2000)
        );
    }

    /// C's alloc hands a reused `pData` back as it was, only `dataType`
    /// changes (NDArrayPool.cpp:177, :191): retyping clears nothing.
    #[test]
    fn a_retyped_buffer_keeps_its_previous_bytes() {
        let pool = NDArrayPool::new(1_000_000);
        let mut a = pool
            .alloc(vec![NDDimension::new(8)], NDDataType::UInt16)
            .unwrap();
        a.data
            .as_mut_slice::<u16>()
            .unwrap()
            .copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let bytes = a.data.as_u8_slice().to_vec();
        pool.release(a);

        let b = pool
            .alloc(vec![NDDimension::new(2)], NDDataType::Float64)
            .unwrap();
        assert_eq!(b.data.as_u8_slice(), &bytes[..]);
    }

    /// `copy` with `copy_data_type` retypes the output in its own buffer
    /// (NDArrayPool.cpp:290-299).
    #[test]
    fn copy_data_type_retypes_the_output_in_its_buffer() {
        let pool = NDArrayPool::new(1_000_000);
        let src = NDArray::with_data(
            vec![NDDimension::new(2)],
            NDDataBuffer::from_vec::<f64>(vec![1.5, -2.0]),
        );
        let out = pool
            .alloc(vec![NDDimension::new(16)], NDDataType::UInt8)
            .unwrap();
        let ptr = out.data.as_u8_slice().as_ptr();
        let out = pool.copy(&src, Some(out), true, true, true).unwrap();
        assert_eq!(out.data.as_u8_slice().as_ptr(), ptr);
        assert_eq!(out.data.as_slice::<f64>(), Some(&[1.5, -2.0][..]));
    }

    /// No buffer method takes a pooled buffer past the bytes its pool
    /// charged, the reused buffer's `dataSize` and not the request when
    /// reused; a buffer off the pool, a clone included, grows as asked.
    #[test]
    fn a_pooled_buffer_never_grows_past_its_charge() {
        let pool = NDArrayPool::new(1_000_000);
        let mut a = pool
            .alloc(vec![NDDimension::new(4)], NDDataType::UInt8)
            .unwrap();
        let ptr = a.data.as_u8_slice().as_ptr();
        a.data.resize(16);
        assert_eq!(a.data.len(), 4);
        a.data.fill_from_f64(&[9.0; 16]);
        assert_eq!(a.data.as_slice::<u8>(), Some(&[9u8; 4][..]));
        assert_eq!(a.data.as_u8_slice().as_ptr(), ptr);

        let mut copy = a.data.clone();
        copy.resize(16);
        assert_eq!(copy.len(), 16);
        let mut loose = NDDataBuffer::zeros(NDDataType::UInt8, 4);
        loose.fill_from_f64(&[1.0; 16]);
        assert_eq!(loose.len(), 16);

        let b = pool
            .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
            .unwrap();
        pool.release(b);
        let mut c = pool
            .alloc(vec![NDDimension::new(80)], NDDataType::UInt8)
            .unwrap();
        assert_eq!(c.data_size, 100);
        c.data.resize(200);
        assert_eq!(c.data.len(), 100);
        assert_eq!(pool.allocated_bytes(), 4 + 100);
    }

    /// `copy` into an existing output writes at most its `dataSize` bytes
    /// (NDArrayPool.cpp:296), in its own buffer, on each path that sets the
    /// output's length: same type, retyped, converted, retyped without data.
    #[test]
    fn copy_into_an_output_stops_at_its_data_size() {
        let pool = NDArrayPool::new(1_000_000);
        let bytes: Vec<u8> = (1..=16).collect();
        let src_u8 =
            NDArray::with_data(vec![NDDimension::new(16)], NDDataBuffer::from_slice(&bytes));
        let src_f64 = NDArray::with_data(
            vec![NDDimension::new(4)],
            NDDataBuffer::from_vec::<f64>(vec![1.5, 2.5, 3.5, 4.5]),
        );
        let output = |n: usize, data_type: NDDataType| {
            let out = pool.alloc(vec![NDDimension::new(n)], data_type).unwrap();
            let ptr = out.data.as_u8_slice().as_ptr();
            (out, ptr)
        };

        let (out, ptr) = output(4, NDDataType::UInt8);
        let out = pool.copy(&src_u8, Some(out), true, false, false).unwrap();
        assert_eq!(out.data.as_slice::<u8>(), Some(&bytes[..4]));
        assert_eq!(out.data.as_u8_slice().as_ptr(), ptr);

        let (out, ptr) = output(16, NDDataType::UInt8);
        let out = pool.copy(&src_f64, Some(out), true, false, true).unwrap();
        assert_eq!(out.data.as_slice::<f64>(), Some(&[1.5, 2.5][..]));
        assert_eq!(out.data.as_u8_slice().as_ptr(), ptr);

        let (out, ptr) = output(4, NDDataType::UInt16);
        let out = pool.copy(&src_u8, Some(out), true, false, false).unwrap();
        assert_eq!(out.data.as_slice::<u16>(), Some(&[1, 2, 3, 4][..]));
        assert_eq!(out.data.as_u8_slice().as_ptr(), ptr);

        let (out, _) = output(12, NDDataType::UInt8);
        let out = pool.copy(&src_f64, Some(out), false, false, true).unwrap();
        assert_eq!(out.data.data_type(), NDDataType::Float64);
        assert!(out.data.total_bytes() <= out.data_size);

        assert_eq!(pool.allocated_bytes(), 4 + 16 + 8 + 12);
    }

    /// A copy the pool allocates is C's `alloc` of the source's dims and type
    /// (NDArrayPool.cpp:280): an uncompressed buffer longer than its dims is
    /// cut at them on every path, and a codec frame is charged its
    /// uncompressed size, or its payload when the codec overhead makes that
    /// larger.
    #[test]
    fn a_pool_made_copy_is_charged_as_c_allocates_it() {
        let pool = NDArrayPool::new(1_000_000);
        let payload: Vec<u8> = (1..=10).collect();
        let src = NDArray::with_data(
            vec![NDDimension::new(4)],
            NDDataBuffer::from_slice(&payload),
        );

        let out = pool.copy(&src, None, true, true, true).unwrap();
        assert_eq!(out.data.as_slice::<u8>(), Some(&payload[..4]));
        assert_eq!((out.data_size, pool.allocated_bytes()), (4, 4));

        let copy = pool.alloc_copy(&src).unwrap();
        assert_eq!(copy.data.as_slice::<u8>(), Some(&payload[..4]));
        assert_eq!((copy.data_size, pool.allocated_bytes()), (4, 8));

        let same = pool.convert_type(&src, NDDataType::UInt8).unwrap();
        assert_eq!(same.data.as_slice::<u8>(), Some(&payload[..4]));
        let wide = pool.convert_type(&src, NDDataType::UInt16).unwrap();
        assert_eq!(wide.data.as_slice::<u16>(), Some(&[1, 2, 3, 4][..]));
        assert_eq!((wide.data_size, pool.allocated_bytes()), (8, 20));

        let frame = |bytes: usize| {
            let mut a = NDArray::with_data(
                vec![NDDimension::new(10), NDDimension::new(10)],
                NDDataBuffer::from_vec((0..bytes).map(|i| i as u8).collect()),
            );
            a.codec = Some(Codec {
                name: crate::codec::CodecName::Blosc,
                compressed_size: bytes,
                level: 5,
                shuffle: 1,
                compressor: 0,
                original_data_type: NDDataType::UInt16,
            });
            a
        };
        let small = frame(50);
        let copy = pool.alloc_copy(&small).unwrap();
        assert_eq!(copy.data.as_u8_slice(), small.data.as_u8_slice());
        assert_eq!((copy.data_size, pool.allocated_bytes()), (200, 220));
        let big = frame(216);
        let copy = pool.copy(&big, None, true, true, true).unwrap();
        assert_eq!(copy.data.as_u8_slice(), big.data.as_u8_slice());
        assert_eq!((copy.data_size, pool.allocated_bytes()), (216, 436));
    }

    /// A buffer is not stretched past its `dataSize` whatever its capacity:
    /// C++ has no reuse that grows (NDArrayPool.cpp:161-183).
    #[test]
    fn a_free_buffer_smaller_than_the_request_is_not_reused() {
        let pool = NDArrayPool::new(1_000_000);
        let mut a = pool
            .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
            .unwrap();
        a.data = NDDataBuffer::zeros(NDDataType::UInt8, 200);
        a.data.resize(100);
        pool.release(a);
        let b = pool
            .alloc(vec![NDDimension::new(150)], NDDataType::UInt8)
            .unwrap();
        assert_eq!(b.data_size, 150);
        assert_eq!(pool.num_alloc_buffers(), 2);
        assert_eq!(pool.allocated_bytes(), 250);
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

        let live = pool
            .alloc(vec![NDDimension::new(10)], NDDataType::UInt8)
            .unwrap();
        assert_eq!(pool.num_free_buffers(), 0);
        drop(live);
        assert_eq!(pool.num_free_buffers(), 1);
    }

    #[test]
    fn test_concurrent_alloc_release() {
        use std::thread;

        let pool = NDArrayPool::new(10_000_000);
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
        if let NDDataViewMut::U8(v) = src.data.view_mut() {
            v[0] = 10;
            v[1] = 20;
            v[2] = 30;
            v[3] = 40;
        }

        let out = pool.convert_type(&src, NDDataType::UInt8).unwrap();
        assert_eq!(out.data.data_type(), NDDataType::UInt8);
        if let NDDataView::U8(v) = out.data.view() {
            assert_eq!(v, &[10, 20, 30, 40]);
        } else {
            panic!("wrong type");
        }
    }

    #[test]
    fn test_convert_type_u8_to_f32() {
        let pool = NDArrayPool::new(1_000_000);
        let mut src = NDArray::new(vec![NDDimension::new(3)], NDDataType::UInt8);
        if let NDDataViewMut::U8(v) = src.data.view_mut() {
            v[0] = 0;
            v[1] = 128;
            v[2] = 255;
        }

        let out = pool.convert_type(&src, NDDataType::Float32).unwrap();
        assert_eq!(out.data.data_type(), NDDataType::Float32);
        if let NDDataView::F32(v) = out.data.view() {
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
        if let NDDataViewMut::U16(v) = src.data.view_mut() {
            v[0] = 100;
            v[1] = 300;
        }

        let out = pool.convert_type(&src, NDDataType::UInt8).unwrap();
        if let NDDataView::U8(v) = out.data.view() {
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
        if let NDDataViewMut::U8(v) = arr.data.view_mut() {
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
        if let NDDataView::U8(v) = out.data.view() {
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
        if let NDDataView::U8(v) = out.data.view() {
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
        if let NDDataView::U8(v) = out.data.view() {
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
                if let NDDataViewMut::$variant(v) = src.data.view_mut() {
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
        match out.data.view() {
            NDDataView::U16(v) => assert_eq!(v[0], 800, "C sums in u16: 200*4"),
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
        match out.data.view() {
            NDDataView::U8(v) => assert_eq!(v[0], 144, "400 wraps mod 256 in u8"),
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
        if let NDDataViewMut::I64(v) = src.data.view_mut() {
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
        match out.data.view() {
            NDDataView::I64(v) => assert_eq!(v[0], value, "2^53+1 kept exactly"),
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
        if let NDDataViewMut::U8(v) = src.data.view_mut() {
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
        if let NDDataView::U8(v) = out.data.view() {
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
        if let NDDataViewMut::U16(v) = src.data.view_mut() {
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
        if let NDDataView::U16(v) = out.data.view() {
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
        if let NDDataView::F32(v) = out.data.view() {
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
        if let NDDataViewMut::U8(v) = src.data.view_mut() {
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
        if let NDDataViewMut::U16(v) = src.data.view_mut() {
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
        if let NDDataView::U16(v) = out.data.view() {
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
        if let NDDataViewMut::U8(v) = src.data.view_mut() {
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
        if let NDDataView::U8(v) = out.data.view() {
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
        if let NDDataViewMut::U16(v) = src.data.view_mut() {
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
        if let NDDataView::U16(v) = out.data.view() {
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
        assert_eq!(out.pool_id(), pool.id());
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

    /// C `NDArrayPool::copy` assigns only `codec.name` (NDArrayPool.cpp:293),
    /// so a blosc frame's level, shuffle and compressor read back -1 on the
    /// copy, through `alloc_copy` and `copy` alike.
    #[test]
    fn a_copy_keeps_only_the_codec_name() {
        let pool = NDArrayPool::new(1_000_000);
        let mut src = make_4x4_u8();
        src.codec = Some(Codec {
            name: crate::codec::CodecName::Blosc,
            compressed_size: 4,
            level: 5,
            shuffle: 1,
            compressor: 2,
            original_data_type: NDDataType::UInt16,
        });
        let fields = |a: NDArray| {
            let c = a.codec.clone().unwrap();
            let kept = (c.name, c.compressed_size, c.original_data_type);
            (kept, c.level, c.shuffle, c.compressor)
        };
        let unset = Codec::UNSET;
        let want = (
            (crate::codec::CodecName::Blosc, 4, NDDataType::UInt16),
            unset,
            unset,
            unset,
        );
        assert_eq!(fields(pool.alloc_copy(&src).unwrap()), want);
        assert_eq!(
            fields(pool.copy(&src, None, true, true, true).unwrap()),
            want
        );
        assert_eq!(
            fields(pool.copy(&src, None, false, true, true).unwrap()),
            want
        );
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
        // Release pool A's array into pool B: B is untouched, A gets it back.
        pool_b.release(arr);
        assert_eq!(pool_b.allocated_bytes(), bytes_b_before);
        assert_eq!(pool_b.num_free_buffers(), free_b_before);
        assert_eq!(pool_a.num_free_buffers(), 1);
    }

    /// G1: a non-pool array (pool_id() == 0) is also rejected by `release`.
    #[test]
    fn test_release_non_pool_array_rejected() {
        let pool = NDArrayPool::new(1_000_000);
        let arr = NDArray::new(vec![NDDimension::new(10)], NDDataType::UInt8);
        assert_eq!(arr.pool_id(), 0);
        pool.release(arr);
        assert_eq!(pool.num_free_buffers(), 0);
        assert_eq!(pool.allocated_bytes(), 0);
    }

    /// G2: `copy` into a fresh array copies data, dims, type.
    #[test]
    fn test_copy_allocates_and_copies() {
        let pool = NDArrayPool::new(1_000_000);
        let mut src = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        if let NDDataViewMut::U8(v) = src.data.view_mut() {
            v.copy_from_slice(&[9, 8, 7, 6]);
        }
        let out = pool.copy(&src, None, true, true, true).unwrap();
        assert_eq!(out.pool_id(), pool.id());
        assert_eq!(out.dims.len(), 1);
        if let NDDataView::U8(v) = out.data.view() {
            assert_eq!(v, &[9, 8, 7, 6]);
        } else {
            panic!("wrong type");
        }
    }

    /// Concurrent allocations that have to evict free buffers never push
    /// `allocated_bytes` past `max_memory`: the limit check and the charge
    /// are one CAS.
    #[test]
    fn test_concurrent_evicting_allocs_do_not_overshoot_max_memory() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::thread;

        const N: usize = 16;
        // Repeat to exercise the race window.
        for _ in 0..50 {
            // N free 100-byte buffers hold 1600 of the 2000 bytes; none holds
            // a 200-byte request, so every alloc is fresh and the later ones
            // must evict.
            let pool = NDArrayPool::new(2000);
            let warm: Vec<_> = (0..N)
                .map(|_| {
                    pool.alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
                        .unwrap()
                })
                .collect();
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
                "allocated_bytes overshot max_memory during concurrent allocs"
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

    /// The owner path: an array published as `Arc<NDArray>` hands its buffer
    /// back when the last reference goes, and the next alloc reuses it.
    #[test]
    fn dropping_the_last_arc_returns_the_buffer_to_the_pool() {
        let pool = NDArrayPool::new(1_000_000);
        let arr = pool
            .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
            .unwrap();
        let ptr = arr.data.as_u8_slice().as_ptr();
        let shared = Arc::new(arr);
        let other = Arc::clone(&shared);
        drop(shared);
        assert_eq!(pool.num_free_buffers(), 0);
        drop(other);
        assert_eq!(pool.num_free_buffers(), 1);
        assert_eq!(pool.num_alloc_buffers(), 1);
        assert_eq!(pool.allocated_bytes(), 100);

        let again = pool
            .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
            .unwrap();
        assert_eq!(again.data.as_u8_slice().as_ptr(), ptr);
        assert_eq!(pool.num_free_buffers(), 0);
        assert_eq!(pool.num_alloc_buffers(), 1);
    }

    /// A clone is a plain heap array: its buffer was never charged to the
    /// pool, so it must not land in the free list either.
    #[test]
    fn a_clone_owes_the_pool_nothing() {
        let pool = NDArrayPool::new(1_000_000);
        let arr = pool
            .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
            .unwrap();
        let copy = arr.clone();
        assert_eq!(copy.pool_id(), 0);
        assert_eq!(arr.pool_id(), pool.id());
        drop(copy);
        assert_eq!(pool.num_free_buffers(), 0);
        drop(arr);
        assert_eq!(pool.num_free_buffers(), 1);
    }

    /// An array may outlive its pool; it then frees like any other.
    #[test]
    fn an_array_outliving_its_pool_is_simply_freed() {
        let pool = NDArrayPool::new(1_000_000);
        let arr = pool
            .alloc(vec![NDDimension::new(100)], NDDataType::UInt8)
            .unwrap();
        drop(pool);
        assert_eq!(arr.pool_id(), 0);
        drop(arr);
    }

    /// A driver that allocates a frame per exposure and publishes it as an
    /// `Arc` must never exhaust a pool sized for a few frames: the dropped
    /// frames are the free list.
    #[test]
    fn a_frame_pipeline_never_exhausts_the_pool() {
        let pool = NDArrayPool::new(3 * 1024);
        for _ in 0..100 {
            let frame = Arc::new(
                pool.alloc(vec![NDDimension::new(1024)], NDDataType::UInt8)
                    .unwrap(),
            );
            let consumer = Arc::clone(&frame);
            drop(frame);
            drop(consumer);
        }
        assert!(pool.allocated_bytes() <= 3 * 1024);
        assert_eq!(pool.num_alloc_buffers(), 1);
    }

    /// `alloc_copy`, `convert_type` and `convert` fill the buffer the pool
    /// handed out rather than swapping in a fresh one, so a recycled buffer
    /// stays recycled.
    #[test]
    fn pool_copies_and_conversions_fill_the_pooled_buffer_in_place() {
        let pool = NDArrayPool::new(1_000_000);
        let src = make_4x4_u8();

        let first = pool.alloc_copy(&src).unwrap();
        let ptr = first.data.as_u8_slice().as_ptr();
        drop(first);
        let copy = pool.alloc_copy(&src).unwrap();
        assert_eq!(copy.data.as_u8_slice().as_ptr(), ptr);
        assert_eq!(copy.data.as_u8_slice(), src.data.as_u8_slice());
        drop(copy);

        let converted = pool.convert_type(&src, NDDataType::UInt16).unwrap();
        let ptr16 = converted.data.as_u8_slice().as_ptr();
        assert_eq!(pool.num_alloc_buffers(), 2);
        drop(converted);
        let converted = pool.convert_type(&src, NDDataType::UInt16).unwrap();
        assert_eq!(converted.data.as_u8_slice().as_ptr(), ptr16);
        assert_eq!(pool.num_alloc_buffers(), 2);
        drop(converted);

        let dims_out: Vec<NDDimension> = src.dims.clone();
        let binned = pool.convert(&src, &dims_out, NDDataType::UInt8).unwrap();
        assert_eq!(binned.data.as_u8_slice().as_ptr(), ptr);
        assert_eq!(binned.data.as_u8_slice(), src.data.as_u8_slice());
        assert_eq!(pool.num_alloc_buffers(), 2);
    }

    #[test]
    fn alloc_sized_holds_the_requested_bytes_under_the_dims() {
        let pool = NDArrayPool::new(0);
        let dims = vec![NDDimension::new(4), NDDimension::new(4)];
        // A codec output: the input's dims, a payload of its own size.
        let arr = pool
            .alloc_sized(dims.clone(), NDDataType::UInt8, 7)
            .unwrap();
        assert_eq!(arr.data.as_u8_slice().len(), 7);
        assert_eq!(arr.dims.len(), 2);
        assert_eq!(pool.allocated_bytes(), 7);

        // A payload that is not whole elements is refused, not truncated.
        assert!(pool.alloc_sized(dims, NDDataType::UInt16, 7).is_err());
    }

    /// C++ `NDArrayPool::convert` copies `uniqueId` along with both stamps
    /// (NDArrayPool.cpp:658-660), on every one of its paths. One case per
    /// branch that decides where the output buffer comes from.
    #[test]
    fn convert_carries_the_source_unique_id() {
        let pool = NDArrayPool::new(1_000_000);
        let mut src = pool
            .alloc(vec![NDDimension::new(4)], NDDataType::UInt8)
            .unwrap();
        src.unique_id = 77;

        // Same type: the alloc_copy branch.
        let same = pool.convert_type(&src, NDDataType::UInt8).unwrap();
        assert_eq!(same.unique_id, 77);

        // Differing type: the convert_buffer_into branch.
        let widened = pool.convert_type(&src, NDDataType::Float64).unwrap();
        assert_eq!(widened.unique_id, 77);

        // Dimension-changing convert.
        let mut d = NDDimension::new(2);
        d.offset = 1;
        let cropped = pool.convert(&src, &[d], NDDataType::UInt8).unwrap();
        assert_eq!(cropped.unique_id, 77);
    }
}
