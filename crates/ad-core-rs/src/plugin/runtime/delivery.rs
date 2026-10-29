//! What C `endProcessCallbacks` works on (NDPluginDriver.cpp:265-328): the
//! output pool, the `pArrays[0]` cache, the MaxByteRate throttle, the sort
//! buffer and the downstream outputs.
//!
//! Every method locks, works and returns what it made, so no guard leaves
//! this module: the lock is a leaf that no other lock or wait is taken under,
//! and a frame can be handed on from any thread holding any lock.

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll};

use asyn_rs::port_handle::PortHandle;
use asyn_rs::request::SectionScope;
use asyn_rs::trace::TraceMask;
use parking_lot::Mutex;

use epics_libcom_rs::runtime::task::ThreadDrive;

use crate::attributes::NDAttributeList;
use crate::ndarray::NDArray;
use crate::ndarray_pool::{DriverPool, NDArrayPool, PublishedArray};

use super::super::channel::{NDArrayOutput, NDArraySender, publish_each};
use super::super::throttler::Throttler;
use super::{ArrayCallbacks, CopyArray, Frame, PluginPool, SortBuffer};
use smallvec::smallvec;

pub(super) struct Delivery {
    /// One output per port address: what C's asynGenericPointer interrupt
    /// list is per address. Index 0 is the `endProcessCallbacks` output.
    outputs: Vec<Arc<Mutex<NDArrayOutput>>>,
    array_callbacks: ArrayCallbacks,
    /// NDPluginStdArrays, which routes its waveform with NDArrayCallbacks 0.
    std_arrays: bool,
    port_handle: PortHandle,
    /// The processor allocates from this same pool ([`super::NDPluginProcess::bind_pool`]).
    pool: PluginPool,
    state: Mutex<State>,
}

struct State {
    /// Output byte-rate limit (C++ `MaxByteRate`); 0 disables throttling.
    max_byte_rate: f64,
    /// Token-bucket throttler enforcing `max_byte_rate` on the output path (G7).
    throttler: Throttler,
    /// Sort mode: 0 = disabled, 1 = sorted output.
    sort_mode: i32,
    /// C `sortingThreadId_ != 0`: SortMode has been Sorted at least once.
    /// Only the sort path writes SortFree (NDPluginDriver.cpp:301, :667), so
    /// until then it stays undefined.
    sorting_started: bool,
    /// Sort time: seconds — per-element staleness deadline for the sort buffer.
    sort_time: f64,
    /// Sort size: maximum number of uniqueId entries in the sort buffer.
    sort_size: i32,
    /// Sort buffer for reordering output arrays by uniqueId.
    sort_buffer: SortBuffer,
    /// Last array the plugin handed to `endProcessCallbacks` (C++
    /// `pArrays[0]`), cached whatever NDArrayCallbacks, the MaxByteRate
    /// throttle or the sort buffer then decide about it
    /// (NDPluginDriver.cpp:259-264, :275-278). It is the dimensions template
    /// `NDPoolPreAllocBuffers` allocates from.
    last_output_array: Option<Arc<NDArray>>,
}

/// The counters C's sort path sets.
#[derive(Clone, Copy)]
pub(super) struct SortCounters {
    pub disordered_arrays: i32,
    pub dropped_output_arrays: i32,
    /// SortFree, once SortMode has been Sorted.
    pub sort_free: Option<i32>,
}

impl Delivery {
    pub(super) fn new(
        outputs: Vec<Arc<Mutex<NDArrayOutput>>>,
        pool: PluginPool,
        array_callbacks: ArrayCallbacks,
        std_arrays: bool,
        port_handle: PortHandle,
    ) -> Self {
        Self {
            outputs,
            array_callbacks,
            std_arrays,
            port_handle,
            pool,
            state: Mutex::new(State {
                max_byte_rate: 0.0,
                throttler: Throttler::new(0.0),
                sort_mode: 0,
                sorting_started: false,
                sort_time: 0.0,
                sort_size: 10,
                sort_buffer: SortBuffer::new(),
                last_output_array: None,
            }),
        }
    }

    /// C `endProcessCallbacks` up to its delivery (NDPluginDriver.cpp:
    /// 265-328), for a processed frame and for the frames NDPluginFile
    /// writes (NDPluginFile.cpp:252, :301, :349). `pArrays[0]` is cached
    /// before the throttle and the sort buffer see the array: the array
    /// itself with NDArrayCallbacks 0 (:273-280), otherwise its copy once
    /// the copy succeeded (:285-299). Returns what is ready to deliver.
    pub(super) fn end_process(
        &self,
        arrays: Frame<Arc<NDArray>>,
        attributes: Option<&NDAttributeList>,
        copy: CopyArray<'_>,
    ) -> Frame<PublishedArray> {
        self.end(arrays, attributes, copy, true)
    }

    /// `pArrays[0] = pArray` of the `endProcessCallbacks(pArray, false, ..)`
    /// a ReadFile write runs inside `writeInt32` (NDPluginFile.cpp:190-193),
    /// so the write's next one, a PoolPreAllocBuffers say, finds it. Set on
    /// the writing thread: the rest waits for the data thread, which a write
    /// must not wait for.
    pub(super) fn cache_written(&self, array: &Arc<NDArray>) {
        self.state.lock().last_output_array = Some(Arc::clone(array));
    }

    /// [`Self::end_process`] for what a write published, whose `pArrays[0]`
    /// [`Self::cache_written`] set in the write.
    pub(super) fn end_written(
        &self,
        arrays: Frame<Arc<NDArray>>,
        attributes: Option<&NDAttributeList>,
    ) -> Frame<PublishedArray> {
        self.end(arrays, attributes, CopyArray::Never, false)
    }

    fn end(
        &self,
        arrays: Frame<Arc<NDArray>>,
        attributes: Option<&NDAttributeList>,
        copy: CopyArray<'_>,
        cache: bool,
    ) -> Frame<PublishedArray> {
        let callbacks = self.array_callbacks.get();
        let mut state = self.state.lock();
        if cache
            && !callbacks
            && let Some(last) = arrays.last()
        {
            state.last_output_array = Some(Arc::clone(last));
        }
        if !callbacks && !self.std_arrays {
            return Frame::new();
        }
        // C copies only past the NDArrayCallbacks return (:274-286); the
        // StdArrays waveform route takes the frame as it is.
        let copy = if callbacks { copy } else { CopyArray::Never };
        let arrays = self.publishable(arrays, attributes, copy);
        if cache
            && callbacks
            && let Some(last) = arrays.last()
        {
            state.last_output_array = Some(Arc::clone(last.array()));
        }
        state.route_output_arrays(arrays)
    }

    /// NDArrayCallbacks.
    pub(super) fn delivers(&self) -> bool {
        self.array_callbacks.get()
    }

    /// [`Self::end_process`] for one frame NDPluginFile wrote, copied with
    /// `attributes`, for [`Self::deliver`] (NDPluginFile.cpp:252, :301,
    /// :349).
    pub(super) fn hand_on(
        &self,
        array: &Arc<NDArray>,
        attributes: Option<&NDAttributeList>,
    ) -> Frame<PublishedArray> {
        self.end_process(smallvec![Arc::clone(array)], attributes, CopyArray::Always)
    }

    /// Deliver what [`Self::hand_on`] made ready before this returns. A
    /// blocking downstream runs it on this thread's behalf meanwhile, as C's
    /// `doCallbacksGenericPointer` runs it inline (NDPluginDriver.cpp:419).
    pub(super) fn deliver(&self, ready: Frame<PublishedArray>) {
        if ready.is_empty() || !self.delivers() {
            return;
        }
        let Some(senders) = self.senders(0) else {
            return;
        };
        let scope = SectionScope::current();
        park_on(async {
            for array in &ready {
                publish_each(&senders, array.clone(), scope).await;
            }
        });
    }

    /// C `endProcessCallbacks` copies an output into the pool when it must;
    /// a refused copy prints its line and ends the frame's output there with
    /// `return asynError` (NDPluginDriver.cpp:285-299).
    pub(super) fn publishable(
        &self,
        arrays: Frame<Arc<NDArray>>,
        attributes: Option<&NDAttributeList>,
        copy: CopyArray<'_>,
    ) -> Frame<PublishedArray> {
        let pool = self.pool.current();
        let mut published = Frame::new();
        for array in arrays {
            let copied = match copy {
                CopyArray::IfInput(input) => std::ptr::eq(&*array, input),
                CopyArray::Always => true,
                CopyArray::Never => false,
            };
            let Ok(array) = pool.publishable(array, attributes, copied) else {
                self.port_handle.trace().output(
                    self.port_handle.port_name(),
                    TraceMask::ERROR,
                    "NDPluginDriver::endProcessCallbacks: Couldn't allocate output array. Further processing terminated.\n",
                );
                break;
            };
            published.push(array);
        }
        published
    }

    /// An addressed post is C's own `pNDArrayPool->alloc` (e.g.
    /// NDPluginTimeSeries.cpp:374), not `endProcessCallbacks`: a refusal
    /// prints only the pool's line and skips that post.
    pub(super) fn publishable_addressed(
        &self,
        arrays: Vec<(usize, Arc<NDArray>)>,
    ) -> Frame<(usize, PublishedArray)> {
        let pool = self.pool.current();
        arrays
            .into_iter()
            .filter_map(|(addr, arr)| Some((addr, pool.publishable(arr, None, false).ok()?)))
            .collect()
    }

    /// C `this->pNDArrayPool = pArray->pNDArrayPool` (NDPluginDriver.cpp:
    /// 419): the plugin allocates from the pool its frame came from.
    pub(super) fn adopt_pool(&self, pool: &DriverPool) -> Arc<NDArrayPool> {
        self.pool.adopt(pool);
        Arc::clone(pool.pool())
    }

    /// C `pArrays[0]`, the array `preAllocateBuffers` copies
    /// (asynNDArrayDriver.cpp:715-725).
    pub(super) fn last_output_array(&self) -> Option<Arc<NDArray>> {
        self.state.lock().last_output_array.clone()
    }

    /// The senders registered at `addr` as they stand, used with no lock
    /// held, as C's `doCallbacksGenericPointer` walks its list
    /// (asynPortDriver.cpp:3353).
    pub(super) fn senders(&self, addr: usize) -> Option<Arc<Vec<NDArraySender>>> {
        Some(self.outputs.get(addr)?.lock().senders())
    }

    /// C `throttled(pArray)` for an output that is not an NDArray
    /// (NDPluginPva.cpp:99-108): takes the array's bytes from the same
    /// bucket `endProcessCallbacks` uses, and counts a refusal in
    /// DroppedOutputArrays.
    pub(super) fn throttled(&self, array: &NDArray) -> bool {
        !self.state.lock().throttle_ok(array)
    }

    /// G7: a MaxByteRate change resets the output throttler (C++
    /// writeFloat64 NDPluginDriver.cpp:788-790).
    pub(super) fn set_max_byte_rate(&self, rate: f64) {
        let mut state = self.state.lock();
        state.max_byte_rate = rate;
        state.throttler.reset(rate);
    }

    /// Set SortMode; returns SortTime.
    pub(super) fn set_sort_mode(&self, mode: i32) -> f64 {
        let mut state = self.state.lock();
        state.sort_mode = mode;
        state.sorting_started |= mode == 1;
        state.sort_time
    }

    /// Set SortTime; returns SortMode.
    pub(super) fn set_sort_time(&self, time: f64) -> i32 {
        let mut state = self.state.lock();
        state.sort_time = time;
        state.sort_mode
    }

    pub(super) fn set_sort_size(&self, size: i32) {
        self.state.lock().sort_size = size;
    }

    /// The sort buffer head-first while contiguous or stale (C++
    /// sortingTask periodic tick).
    pub(super) fn drain_ready(&self) -> Vec<(i32, Vec<PublishedArray>)> {
        let mut state = self.state.lock();
        let sort_time = state.sort_time;
        state
            .sort_buffer
            .drain_ready(sort_time, std::time::Instant::now())
    }

    /// The entire sort buffer in uniqueId order (sort mode turned off).
    pub(super) fn drain_all(&self) -> Vec<(i32, Vec<PublishedArray>)> {
        self.state.lock().sort_buffer.drain_all()
    }

    pub(super) fn sort_counters(&self) -> SortCounters {
        let state = self.state.lock();
        SortCounters {
            disordered_arrays: state.sort_buffer.disordered_arrays,
            dropped_output_arrays: state.sort_buffer.dropped_output_arrays,
            sort_free: state
                .sorting_started
                .then(|| state.sort_size - state.sort_buffer.len()),
        }
    }

    /// A control-plane write of DroppedOutputArrays (B15).
    pub(super) fn resync_dropped_output_arrays(&self, value: i32) {
        self.state.lock().sort_buffer.dropped_output_arrays = value;
    }

    /// A control-plane write of DisorderedArrays (B15).
    pub(super) fn resync_disordered_arrays(&self, value: i32) {
        self.state.lock().sort_buffer.disordered_arrays = value;
    }
}

impl State {
    /// Byte cost of an array for throttling (C++ `NDPluginDriver::throttled`):
    /// compressed size when a codec is present, else total raw bytes.
    fn array_byte_cost(array: &NDArray) -> f64 {
        match &array.codec {
            Some(c) => c.compressed_size as f64,
            None => array.info().total_bytes as f64,
        }
    }

    /// Apply the output throttle to one array. Returns `true` if the array
    /// should be emitted, `false` if it was dropped (and counts the drop).
    fn throttle_ok(&mut self, array: &NDArray) -> bool {
        if self.max_byte_rate == 0.0 {
            return true;
        }
        let cost = Self::array_byte_cost(array);
        if self.throttler.try_take(cost) {
            true
        } else {
            self.sort_buffer.dropped_output_arrays += 1;
            false
        }
    }

    /// Route output arrays through the throttle, the in-order fast path, and
    /// the sort buffer. Returns arrays ready to emit *now*, in order.
    ///
    /// Port of C++ `endProcessCallbacks` (NDPluginDriver.cpp:295-328): an
    /// array whose uniqueId is contiguous with `prevUniqueId_` is emitted
    /// immediately (B2); only out-of-order arrays enter the sort buffer.
    /// Disordered arrays are counted at emission time in both modes (B4).
    fn route_output_arrays(&mut self, arrays: Frame<PublishedArray>) -> Frame<PublishedArray> {
        let mut ready = Frame::new();
        for arr in arrays {
            if !self.throttle_ok(&arr) {
                continue; // G7: dropped by MaxByteRate throttle
            }
            let uid = arr.unique_id;
            if self.sort_mode != 0 && !self.sort_buffer.order_ok(uid) {
                // Out of order with sort mode on: buffer it (B2/B3).
                self.sort_buffer.insert(uid, vec![arr], self.sort_size);
            } else {
                // In order (or sort mode off): emit immediately, count disorder.
                self.sort_buffer.note_emitted(uid);
                ready.push(arr);
            }
        }
        // After emitting in-order arrays, the sort buffer head may now be
        // contiguous — release any newly-ready run (C++ sortingTask).
        if self.sort_mode != 0 {
            for (_id, bucket) in self
                .sort_buffer
                .drain_ready(self.sort_time, std::time::Instant::now())
            {
                ready.extend(bucket);
            }
        }
        ready
    }
}

/// Drive `fut` on this thread, parking between polls: a hand-on runs on a
/// callback thread, the data loop or the port actor, none of which may
/// enter a runtime. The futures are `tokio::sync` sends, woken by the
/// downstream's own thread.
fn park_on<F: Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(tokio::task::unconstrained(fut));
    let drive = ThreadDrive::enter();
    let mut cx = Context::from_waker(drive.waker());
    loop {
        if let Poll::Ready(value) = fut.as_mut().poll(&mut cx) {
            return value;
        }
        drive.park(None);
    }
}
