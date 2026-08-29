// RTEMS-EXEC-MODEL-ALLOW(17): checked, not waived — all 17 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p ad-core-rs
// --all-features`, 345/345). ad-core-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use crate::ndarray::NDArray;

/// Tracks the number of queued (in-flight) arrays across plugins.
/// Used by drivers to perform a bounded wait at end of acquisition.
pub struct QueuedArrayCounter {
    count: AtomicUsize,
    mutex: parking_lot::Mutex<()>,
    condvar: parking_lot::Condvar,
}

impl QueuedArrayCounter {
    /// Create a new counter starting at zero.
    pub fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            mutex: parking_lot::Mutex::new(()),
            condvar: parking_lot::Condvar::new(),
        }
    }

    /// Increment the queued count (called before send).
    pub fn increment(&self) {
        self.count.fetch_add(1, Ordering::AcqRel);
    }

    /// Decrement the queued count. Notifies waiters when reaching zero.
    pub fn decrement(&self) {
        let prev = self.count.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            let _guard = self.mutex.lock();
            self.condvar.notify_all();
        }
    }

    /// Current queued count.
    pub fn get(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Wait until count reaches zero, or timeout expires.
    /// Returns `true` if count is zero, `false` on timeout.
    pub fn wait_until_zero(&self, timeout: Duration) -> bool {
        let mut guard = self.mutex.lock();
        if self.count.load(Ordering::Acquire) == 0 {
            return true;
        }
        !self
            .condvar
            .wait_while_for(
                &mut guard,
                |_| self.count.load(Ordering::Acquire) != 0,
                timeout,
            )
            .timed_out()
    }
}

impl Default for QueuedArrayCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// Array message with optional queued-array counter and completion signal.
/// When dropped, decrements the counter (if present) — this signals that
/// the downstream plugin has finished processing the array.
pub struct ArrayMessage {
    pub array: Arc<NDArray>,
    pub(crate) counter: Option<Arc<QueuedArrayCounter>>,
    /// When Some, the sender awaits this to confirm downstream processing completed.
    /// Fired when ArrayMessage is dropped (i.e., after plugin process_array finishes).
    pub(crate) done_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for ArrayMessage {
    fn drop(&mut self) {
        if let Some(tx) = self.done_tx.take() {
            let _ = tx.send(());
        }
        if let Some(c) = self.counter.take() {
            c.decrement();
        }
    }
}

/// Outcome of a `publish` call, mirroring C++ `driverCallback` accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    /// The array was enqueued (and, in blocking mode, processed).
    Delivered,
    /// `enable_callbacks` was 0 — array not sent (not a drop, not counted).
    Disabled,
    /// The downstream queue was full and the array was dropped. The caller
    /// must increment `DroppedArrays`, matching C++ `trySend` semantics.
    DroppedQueueFull,
    /// The array carried a codec and the downstream plugin is not compression
    /// aware, so it was dropped and counted before ever reaching the queue
    /// (C++ `driverCallback` NDPluginDriver.cpp:383-394).
    DroppedCompressed,
    /// The array arrived inside the downstream plugin's `MinCallbackTime`
    /// window and was discarded without being counted (C++ `driverCallback`
    /// falls through the `deltaTime > minCallbackTime` gate at :407 and
    /// touches nothing).
    Throttled,
    /// The downstream channel was closed (receiver gone).
    ChannelClosed,
}

/// What C++ `driverCallback` decides about an arriving array BEFORE it ever
/// reaches `pToThreadMsgQ_` (NDPluginDriver.cpp:383-418): the compression gate
/// at `:385`, then the `deltaTime > minCallbackTime` gate at `:407`, then the
/// `lastProcessTime_` stamp at `:417` that a passing array leaves behind.
///
/// It lives on the producer's side of the queue because that is where C runs
/// it, and the side is the whole observable. A compressed array on a
/// non-aware plugin and an array inside the MinCallbackTime window occupy no
/// queue slot in C, so they can never push a LATER array out of one. Deciding
/// the same thing after `recv` instead turns MinCallbackTime — whose purpose
/// is to relieve queue pressure — into a cause of it, and makes a compressed
/// array's drop compete with the queue-full episode counter it should never
/// have reached.
///
/// One instance per receiving plugin, shared by every producer that feeds it,
/// exactly as `lastProcessTime_` is one member of one plugin however many
/// drivers call back into it.
pub struct ArrayAdmission {
    /// C `compressionAware_`, fixed at construction.
    compression_aware: AtomicBool,
    /// C `NDPluginDriverMinCallbackTime`, seconds, as `f64` bits.
    min_callback_time: AtomicU64,
    /// C `lastProcessTime_`. Behind a mutex so the read of the gate and the
    /// stamp that follows it are one step, which is what C's `this->lock()`
    /// across the whole of `driverCallback` buys: two drivers calling back at
    /// once cannot both pass one window.
    last_process: parking_lot::Mutex<Option<std::time::Instant>>,
    /// Raised whenever a producer counts a drop against `DroppedArrays`, so
    /// the plugin's data loop can publish the readback even on a run where no
    /// array is ever processed. C gets this for free: `driverCallback` ends in
    /// `callParamCallbacks()` on every call, dropped or not (`:449`).
    counted_drop: tokio::sync::Notify,
}

/// The three ways C++ `driverCallback` can dispose of an array before the
/// queue: process it, drop and count it, or silently skip it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Past both gates; `lastProcessTime_` has been stamped.
    Admit,
    /// Compressed input to a non-compression-aware plugin (`:385-394`).
    DropCompressed,
    /// Inside the MinCallbackTime window (`:407`).
    Throttled,
}

impl Default for ArrayAdmission {
    fn default() -> Self {
        Self {
            compression_aware: AtomicBool::new(false),
            min_callback_time: AtomicU64::new(0.0f64.to_bits()),
            last_process: parking_lot::Mutex::new(None),
            counted_drop: tokio::sync::Notify::new(),
        }
    }
}

impl ArrayAdmission {
    /// C `compressionAware_`; set once from the plugin's processor.
    pub fn set_compression_aware(&self, aware: bool) {
        self.compression_aware.store(aware, Ordering::Release);
    }

    /// C `setDoubleParam(NDPluginDriverMinCallbackTime, ...)`.
    pub fn set_min_callback_time(&self, seconds: f64) {
        self.min_callback_time
            .store(seconds.to_bits(), Ordering::Release);
    }

    /// Classify one arriving array, stamping `lastProcessTime_` when it
    /// passes. C stamps before the blocking/non-blocking branch and before
    /// `trySend`, so an array that passes the gate and is then refused by a
    /// full queue still resets the clock (`:417` vs `:433`).
    pub(crate) fn classify(&self, array: &NDArray) -> Admission {
        // The compression gate is FIRST in C, so a compressed array is dropped
        // and counted even when it arrives inside a throttle window.
        if array.codec.is_some() && !self.compression_aware.load(Ordering::Acquire) {
            return Admission::DropCompressed;
        }
        let min = f64::from_bits(self.min_callback_time.load(Ordering::Acquire));
        let mut last = self.last_process.lock();
        if min > 0.0
            && let Some(previous) = *last
            && previous.elapsed().as_secs_f64() < min
        {
            return Admission::Throttled;
        }
        *last = Some(std::time::Instant::now());
        Admission::Admit
    }

    /// Wake the data loop so it republishes `DroppedArrays`.
    pub(crate) fn note_counted_drop(&self) {
        self.counted_drop.notify_one();
    }

    /// Await the next counted drop. A drop raised while nobody was waiting
    /// leaves a permit, so no readback is lost.
    pub(crate) async fn counted_drop(&self) {
        self.counted_drop.notified().await;
    }
}

/// C++ `asynUser::auxStatus` on a producer's `pasynUser`, which is the only
/// state `driverCallback` carries from one call to the next
/// (NDPluginDriver.cpp:405-406, :433-434).
///
/// It exists so `DroppedArrays` counts one per overflow EPISODE, not one per
/// dropped array: the first refusal arms the cell, every consecutive refusal
/// reads it as `ignoreQueueFull` and stays silent, and the first successful
/// enqueue leaves it disarmed so the next refusal opens a new episode. A
/// detector running 1 kHz into a plugin that stalls for a second therefore
/// adds 1 to the counter, not 1000.
///
/// One cell per PRODUCER, because that is what a `pasynUser` is. The array
/// port edge and the plugin's own `ProcessPlugin` re-injection are two
/// producers in C — `driverCallback` is reached with
/// `pasynUserGenericPointer_` for the first and `pasynUserSelf` for the second
/// (`:539-541` vs `:741`) — so they get two cells here, and one path's
/// overflow never silences the other's count.
#[derive(Debug, Default)]
pub(crate) struct OverflowEpisode(AtomicBool);

impl OverflowEpisode {
    /// C `if (pasynUser->auxStatus == asynOverflow) ignoreQueueFull = true;`
    /// immediately followed by `pasynUser->auxStatus = asynSuccess;` (:405-406)
    /// — the read and the consume are one step, so no caller can read the flag
    /// without clearing it.
    fn take(&self) -> bool {
        self.0.swap(false, Ordering::AcqRel)
    }

    /// C `pasynUser->auxStatus = asynOverflow;` on a refused `trySend` (:433),
    /// and `NDPluginScatter`'s pre-arm of a node it means to reroute past
    /// (NDPluginScatter.cpp:83).
    fn arm(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// C `NDPluginScatter.cpp:84` — the last node is given `asynSuccess`, so
    /// it counts its drop even if the previous round ended in overflow.
    fn disarm(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The `trySend` arm of C++ `driverCallback` (NDPluginDriver.cpp:423-442):
/// enqueue, or drop the array and count it against the *receiving* plugin's
/// `DroppedArrays` — unless this producer's [`OverflowEpisode`] says the
/// previous call already opened the episode.
///
/// The one owner of that accounting: an upstream publish and the plugin's own
/// `ProcessPlugin` re-injection both come through here, so a queue-full drop
/// is counted the same however the array got to the queue.
fn try_send_arm(
    tx: &parking_lot::RwLock<tokio::sync::mpsc::Sender<ArrayMessage>>,
    queued_counter: &Option<Arc<QueuedArrayCounter>>,
    dropped_arrays: &AtomicI32,
    array: Arc<NDArray>,
    episode: &OverflowEpisode,
    admission: &ArrayAdmission,
) -> PublishOutcome {
    // C reads and clears `auxStatus` before deciding anything else (:405-406),
    // so a successful enqueue below ends the episode by simply not re-arming.
    let ignore_queue_full = episode.take();
    // Build the message only on the way into try_send so a full queue does
    // not touch the counter.
    if let Some(c) = queued_counter {
        c.increment();
    }
    let msg = ArrayMessage {
        array,
        counter: queued_counter.clone(),
        done_tx: None,
    };
    match tx.read().try_send(msg) {
        Ok(()) => PublishOutcome::Delivered,
        // `msg` is dropped here → counter decremented by ArrayMessage::drop.
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            episode.arm();
            if !ignore_queue_full {
                dropped_arrays.fetch_add(1, Ordering::AcqRel);
                admission.note_counted_drop();
            }
            PublishOutcome::DroppedQueueFull
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => PublishOutcome::ChannelClosed,
    }
}

/// Sender held by upstream.
///
/// # Default: drop-on-full (C++ parity)
///
/// By default `publish` uses a bounded `try_send`: when the downstream queue
/// is full the array is **dropped** and `PublishOutcome::DroppedQueueFull` is
/// returned, matching C++ `NDPluginDriver::driverCallback` `trySend` — a slow
/// plugin drops frames rather than back-pressuring the detector driver.
///
/// # `blocking_callbacks=1`: reliable opt-in
///
/// When `blocking_callbacks` is set, `publish` instead uses a reliable
/// `send().await` and waits for the downstream plugin to finish processing.
/// This is the explicit opt-in for "never drop, apply back-pressure"
/// behavior. It is NOT the default.
#[derive(Clone)]
pub struct NDArraySender {
    /// The queue itself, behind a shared cell so it can be REPLACED.
    ///
    /// C keeps the input queue as `pToThreadMsgQ_`, a pointer inside the
    /// plugin that every producer reaches through `driverCallback`; a
    /// QueueSize write deletes it and news it at the new depth
    /// (`NDPluginDriver.cpp:730-733` -> `:985`), and every producer follows
    /// because they never held the queue, only the plugin. Upstream ports
    /// here hold cloned `NDArraySender`s instead, so the shared cell is what
    /// gives one replacement the same reach. A plain `Sender` field would
    /// leave every existing clone addressing the old queue, which is a
    /// QueueSize write that moves the readback and nothing else.
    tx: Arc<parking_lot::RwLock<tokio::sync::mpsc::Sender<ArrayMessage>>>,
    port_name: String,
    enabled: Arc<AtomicBool>,
    blocking_mode: Arc<AtomicBool>,
    queued_counter: Option<Arc<QueuedArrayCounter>>,
    /// Cumulative count of arrays dropped because this sender's downstream
    /// input queue was full. Owned by the downstream plugin (which publishes
    /// it to its `DROPPED_ARRAYS` param), shared back to every upstream
    /// sender that feeds this plugin — matching C++ `driverCallback` which
    /// increments the *receiving* plugin's `NDPluginDriverDroppedArrays`.
    dropped_arrays: Arc<AtomicI32>,
    /// This edge's `pasynUser->auxStatus`. Shared by every clone of the
    /// sender, because in C the cell belongs to the receiving plugin's one
    /// registered `pasynUserGenericPointer_` and not to whichever upstream
    /// happens to be pushing.
    overflow: Arc<OverflowEpisode>,
    /// The receiving plugin's pre-queue gates. Consulted here rather than
    /// after `recv` because that is where C consults them — see
    /// [`ArrayAdmission`].
    admission: Arc<ArrayAdmission>,
}

impl NDArraySender {
    /// Publish an array downstream.
    ///
    /// - `enable_callbacks=0`: returns `Disabled`, array not sent.
    /// - `blocking_callbacks=0` (default): bounded `try_send` — on a full queue
    ///   the array is dropped and `DroppedQueueFull` is returned (C++ parity).
    /// - `blocking_callbacks=1`: reliable `send().await` + awaits downstream
    ///   processing completion (explicit opt-in, never drops).
    pub async fn publish(&self, array: Arc<NDArray>) -> PublishOutcome {
        self.publish_inner(array).await
    }

    /// Publish for the scatter reroute path. Mirrors C++ `NDPluginScatter`'s
    /// `auxStatus` protocol: `doNDArrayCallbacks` writes the consumer's
    /// `auxStatus` before EVERY call — `asynOverflow` for each node it means
    /// to reroute past, `asynSuccess` for the last (NDPluginScatter.cpp:83-84)
    /// — so a full-queue consumer that is not the last is skipped without
    /// counting a dropped array. Scatter writes the same cell
    /// `driverCallback` arms on its own refusals, which is why this is an
    /// arm/disarm of the episode rather than a bypass flag: were they two
    /// mechanisms, a scatter round following a natural overflow would silence
    /// the last node too.
    pub async fn publish_scatter(&self, array: Arc<NDArray>, is_last: bool) -> PublishOutcome {
        if is_last {
            self.overflow.disarm();
        } else {
            self.overflow.arm();
        }
        self.publish_inner(array).await
    }

    /// Shared publish body.
    async fn publish_inner(&self, array: Arc<NDArray>) -> PublishOutcome {
        if !self.enabled.load(Ordering::Acquire) {
            return PublishOutcome::Disabled;
        }

        // C runs both gates before it even reads `blockingCallbacks`
        // (NDPluginDriver.cpp:385-407 vs the branch at :419), so they apply to
        // the inline and the queued mode alike.
        match self.admission.classify(&array) {
            Admission::Admit => {}
            Admission::DropCompressed => {
                self.dropped_arrays.fetch_add(1, Ordering::AcqRel);
                self.admission.note_counted_drop();
                return PublishOutcome::DroppedCompressed;
            }
            Admission::Throttled => return PublishOutcome::Throttled,
        }

        let blocking = self.blocking_mode.load(Ordering::Acquire);

        if !blocking {
            return try_send_arm(
                &self.tx,
                &self.queued_counter,
                &self.dropped_arrays,
                array,
                &self.overflow,
                &self.admission,
            );
        }

        // Reliable blocking path: never drops, awaits completion. C clears
        // `auxStatus` at :406 before branching on `blockingCallbacks` and the
        // inline arm never touches the queue, so an episode ends here too.
        self.overflow.disarm();
        if let Some(ref c) = self.queued_counter {
            c.increment();
        }
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let msg = ArrayMessage {
            array,
            counter: self.queued_counter.clone(),
            done_tx: Some(done_tx),
        };
        let tx = self.tx.read().clone();
        if tx.send(msg).await.is_err() {
            // Channel closed — counter was decremented by ArrayMessage::drop
            return PublishOutcome::ChannelClosed;
        }
        let _ = done_rx.await;
        PublishOutcome::Delivered
    }

    /// Whether this sender's plugin has callbacks enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Whether this sender's plugin is in blocking mode.
    pub fn is_blocking(&self) -> bool {
        self.blocking_mode.load(Ordering::Acquire)
    }

    pub fn port_name(&self) -> &str {
        &self.port_name
    }

    /// Set the queued-array counter for tracking in-flight arrays.
    pub fn set_queued_counter(&mut self, counter: Arc<QueuedArrayCounter>) {
        self.queued_counter = Some(counter);
    }

    /// Attach the downstream plugin's shared `DroppedArrays` counter so that
    /// a full-queue drop on this sender is accounted to that plugin (C++ parity).
    pub fn set_dropped_arrays_counter(&mut self, counter: Arc<AtomicI32>) {
        self.dropped_arrays = counter;
    }

    /// The shared `DroppedArrays` counter for this sender's downstream queue.
    pub fn dropped_arrays_counter(&self) -> &Arc<AtomicI32> {
        &self.dropped_arrays
    }

    /// The receiving plugin's pre-queue gates, so the plugin runtime can keep
    /// them current from its param loop.
    pub fn admission(&self) -> &Arc<ArrayAdmission> {
        &self.admission
    }

    /// Current capacity (free slots) of the downstream input queue.
    pub fn capacity(&self) -> usize {
        self.tx.read().capacity()
    }

    /// Maximum capacity of the downstream input queue.
    pub fn max_capacity(&self) -> usize {
        self.tx.read().max_capacity()
    }

    /// A non-owning handle for the one owner allowed to replace this queue.
    ///
    /// Weak on purpose. The data loop is that owner, and it also learns that
    /// every upstream is gone by its receiver closing — which only happens
    /// once the last `NDArraySender` drops. A strong handle held inside the
    /// loop would keep a sender alive forever and the loop would never see
    /// its own shutdown.
    pub(crate) fn self_queue_handle(&self) -> SelfQueueHandle {
        SelfQueueHandle {
            tx: Arc::downgrade(&self.tx),
            queued_counter: self.queued_counter.clone(),
            dropped_arrays: self.dropped_arrays.clone(),
            // NOT `self.overflow`: C reaches `driverCallback` with
            // `pasynUserSelf` for a `ProcessPlugin` re-injection (:741) and
            // with `pasynUserGenericPointer_` for an array-port callback
            // (:539-541), so the two producers carry separate episodes.
            overflow: OverflowEpisode::default(),
            // The gates, unlike the episode, ARE shared: `lastProcessTime_`
            // and `compressionAware_` are plugin members, so a re-injection
            // competes for the same MinCallbackTime window a detector frame
            // does.
            admission: self.admission.clone(),
        }
    }

    /// Set the enabled/blocking mode flags (used by plugin runtime wiring).
    pub(crate) fn set_mode_flags(
        &mut self,
        enabled: Arc<AtomicBool>,
        blocking_mode: Arc<AtomicBool>,
    ) {
        self.enabled = enabled;
        self.blocking_mode = blocking_mode;
    }
}

/// Receiver held by downstream plugin.
pub struct NDArrayReceiver {
    rx: tokio::sync::mpsc::Receiver<ArrayMessage>,
    /// The same gate the producers consult, so the consumer end can publish
    /// the parameters that feed it (MinCallbackTime) and observe the drops it
    /// counts. One gate per plugin, shared by every producer — C keeps
    /// `lastProcessTime_` and `compressionAware_` on the plugin instance and
    /// every caller of `driverCallback` reads them under the plugin's lock.
    admission: Arc<ArrayAdmission>,
}

impl NDArrayReceiver {
    /// The admission gate this queue is fronted by.
    pub fn admission(&self) -> &Arc<ArrayAdmission> {
        &self.admission
    }

    /// Number of currently buffered (pending) messages in the input queue.
    pub fn pending(&self) -> usize {
        self.rx.len()
    }

    /// Maximum capacity of the input queue.
    pub fn max_capacity(&self) -> usize {
        self.rx.max_capacity()
    }

    /// Number of free slots in the input queue (`max_capacity - pending`).
    pub fn capacity(&self) -> usize {
        self.rx.capacity()
    }

    /// Blocking receive (for use in std::thread data processing loops).
    pub fn blocking_recv(&mut self) -> Option<Arc<NDArray>> {
        self.rx.blocking_recv().map(|msg| msg.array.clone())
    }

    /// Async receive.
    pub async fn recv(&mut self) -> Option<Arc<NDArray>> {
        self.rx.recv().await.map(|msg| msg.array.clone())
    }

    /// Receive the full ArrayMessage (crate-internal). The message's Drop
    /// will signal completion when the caller is done with it.
    pub(crate) async fn recv_msg(&mut self) -> Option<ArrayMessage> {
        self.rx.recv().await
    }

    /// Take a buffered message without waiting. Used to drain a queue that has
    /// just been replaced: the sender no longer points here, so `None` means
    /// empty rather than "not yet".
    pub(crate) fn try_recv_msg(&mut self) -> Option<ArrayMessage> {
        self.rx.try_recv().ok()
    }
}

/// Lets the plugin's data loop swap its own input queue for a deeper or
/// shallower one without owning a sender.
///
/// This is C's `pToThreadMsgQ_`: a queue every producer reaches through the
/// plugin rather than holding directly, so deleting it and re-creating it at
/// the new depth on a QueueSize write (`NDPluginDriver.cpp:730-733` -> `:985`)
/// moves every producer at once.
pub(crate) struct SelfQueueHandle {
    tx: std::sync::Weak<parking_lot::RwLock<tokio::sync::mpsc::Sender<ArrayMessage>>>,
    queued_counter: Option<Arc<QueuedArrayCounter>>,
    dropped_arrays: Arc<AtomicI32>,
    /// This producer's own `pasynUserSelf->auxStatus`.
    overflow: OverflowEpisode,
    /// The same gates the array-port edge consults: C reaches `driverCallback`
    /// for a `ProcessPlugin` re-injection too (`:741`), so the cached array is
    /// re-classified rather than waved past.
    admission: Arc<ArrayAdmission>,
}

impl SelfQueueHandle {
    /// Point every live sender at a fresh queue of `capacity` and return its
    /// receiver. `None` once every sender is gone — there is then no producer
    /// left to redirect, and the caller is already shutting down.
    ///
    /// Publishes racing this land in one queue or the other and none is
    /// refused; the caller owns draining whatever the old receiver still
    /// holds. C instead switches its array interrupt off and waits for the old
    /// queue to empty, losing whatever arrives in that window.
    pub(crate) fn replace_queue(&self, capacity: usize) -> Option<NDArrayReceiver> {
        let cell = self.tx.upgrade()?;
        let (tx, rx) = tokio::sync::mpsc::channel(capacity.max(1));
        *cell.write() = tx;
        // The gate is a property of the plugin, not of the queue in front of
        // it: a QueueSize write must not reset MinCallbackTime's clock.
        Some(NDArrayReceiver {
            rx,
            admission: Arc::clone(&self.admission),
        })
    }

    /// Put an array at the tail of the plugin's own input queue, dropping and
    /// counting it against `DroppedArrays` when there is no room.
    ///
    /// This is how `ProcessPlugin` re-injects the cached input array: C hands
    /// it to `driverCallback` (NDPluginDriver.cpp:741), the same entry point a
    /// detector array arrives through, so it queues behind whatever is already
    /// waiting, is refused when the queue is full, and is processed by a
    /// callback thread rather than by the writer.
    ///
    /// Always the `trySend` arm, whatever `blockingCallbacks` says: the only
    /// consumer of this queue is the caller, so awaiting delivery here would
    /// be waiting on itself. C's blocking arm (`:419-422`) is not a queue
    /// operation at all — it runs `processCallbacks` inline on the calling
    /// thread — so the caller keeps its own inline path for that mode.
    ///
    /// `None` once every sender is gone, which is the caller shutting down.
    pub(crate) fn try_enqueue(&self, array: Arc<NDArray>) -> Option<PublishOutcome> {
        let cell = self.tx.upgrade()?;
        match self.admission.classify(&array) {
            Admission::Admit => {}
            Admission::DropCompressed => {
                self.dropped_arrays.fetch_add(1, Ordering::AcqRel);
                self.admission.note_counted_drop();
                return Some(PublishOutcome::DroppedCompressed);
            }
            Admission::Throttled => return Some(PublishOutcome::Throttled),
        }
        Some(try_send_arm(
            &cell,
            &self.queued_counter,
            &self.dropped_arrays,
            array,
            &self.overflow,
            &self.admission,
        ))
    }
}

/// Create a matched sender/receiver pair.
pub fn ndarray_channel(port_name: &str, queue_size: usize) -> (NDArraySender, NDArrayReceiver) {
    let (tx, rx) = tokio::sync::mpsc::channel(queue_size.max(1));
    let admission = Arc::new(ArrayAdmission::default());
    (
        NDArraySender {
            tx: Arc::new(parking_lot::RwLock::new(tx)),
            port_name: port_name.to_string(),
            enabled: Arc::new(AtomicBool::new(true)),
            blocking_mode: Arc::new(AtomicBool::new(false)),
            queued_counter: None,
            dropped_arrays: Arc::new(AtomicI32::new(0)),
            overflow: Arc::new(OverflowEpisode::default()),
            admission: Arc::clone(&admission),
        },
        NDArrayReceiver { rx, admission },
    )
}

/// Fan-out: publishes arrays to multiple downstream receivers.
pub struct NDArrayOutput {
    senders: Vec<NDArraySender>,
}

impl NDArrayOutput {
    pub fn new() -> Self {
        Self {
            senders: Vec::new(),
        }
    }

    pub fn add(&mut self, sender: NDArraySender) {
        self.senders.push(sender);
    }

    pub fn remove(&mut self, port_name: &str) {
        self.senders.retain(|s| s.port_name != port_name);
    }

    /// Remove a sender by port name and return it (if found).
    pub fn take(&mut self, port_name: &str) -> Option<NDArraySender> {
        let idx = self.senders.iter().position(|s| s.port_name == port_name)?;
        Some(self.senders.swap_remove(idx))
    }

    /// Publish an array to all downstream receivers (async, concurrent).
    ///
    /// Each sender publishes independently. Returns the per-sender outcomes
    /// so the caller can count `DroppedArrays` for any downstream whose queue
    /// was full (C++ `driverCallback` semantics).
    pub async fn publish(&self, array: Arc<NDArray>) -> Vec<PublishOutcome> {
        let futs = self.senders.iter().map(|s| s.publish(array.clone()));
        futures_util::future::join_all(futs).await
    }

    /// Publish an array to a single downstream receiver by index (for scatter/round-robin).
    pub async fn publish_to(&self, index: usize, array: Arc<NDArray>) -> Option<PublishOutcome> {
        if let Some(sender) = self.senders.get(index % self.senders.len().max(1)) {
            Some(sender.publish(array).await)
        } else {
            None
        }
    }

    pub fn num_senders(&self) -> usize {
        self.senders.len()
    }

    /// Clone the senders list (for publishing outside a lock in async context).
    pub(crate) fn senders_clone(&self) -> Vec<NDArraySender> {
        self.senders.clone()
    }
}

/// Cloneable async handle for publishing arrays to downstream plugins.
///
/// This is the public API for driver acquisition tasks.
/// Internally it snapshots the sender list, releases the lock, then
/// publishes to all senders concurrently.
///
/// # Example
/// ```ignore
/// if config.array_callbacks {
///     publisher.publish(Arc::new(frame)).await;
/// }
/// ```
#[derive(Clone)]
pub struct ArrayPublisher {
    output: Arc<parking_lot::Mutex<NDArrayOutput>>,
}

impl ArrayPublisher {
    /// Create a publisher backed by the given output.
    pub fn new(output: Arc<parking_lot::Mutex<NDArrayOutput>>) -> Self {
        Self { output }
    }

    /// Publish an array to all downstream plugins (async, concurrent fan-out).
    ///
    /// Returns the per-downstream outcomes — a `DroppedQueueFull` entry means
    /// that downstream plugin's input queue was full and the array was dropped
    /// (C++ `driverCallback` `trySend`). The driver should count those as
    /// `DroppedArrays`.
    pub async fn publish(&self, array: Arc<NDArray>) -> Vec<PublishOutcome> {
        let senders = self.output.lock().senders_clone();
        let futs = senders.iter().map(|s| s.publish(array.clone()));
        futures_util::future::join_all(futs).await
    }
}

impl Default for NDArrayOutput {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ndarray::{NDArray, NDDataType, NDDimension};

    fn make_test_array(id: i32) -> Arc<NDArray> {
        let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
        arr.unique_id = id;
        Arc::new(arr)
    }

    #[tokio::test]
    async fn test_publish_receive_basic() {
        let (sender, mut receiver) = ndarray_channel("TEST", 10);
        sender.publish(make_test_array(1)).await;
        sender.publish(make_test_array(2)).await;

        let a1 = receiver.recv().await.unwrap();
        assert_eq!(a1.unique_id, 1);
        let a2 = receiver.recv().await.unwrap();
        assert_eq!(a2.unique_id, 2);
    }

    #[tokio::test]
    async fn test_publish_blocking_no_drop() {
        // In blocking_callbacks mode, reliable send().await is used: even a
        // queue of 1 must not drop — the producer back-pressures instead.
        let (sender, mut receiver) = ndarray_channel("TEST", 1);
        sender.blocking_mode.store(true, Ordering::Release);

        let s = sender.clone();
        let pub_handle = tokio::spawn(async move {
            s.publish(make_test_array(1)).await;
            s.publish(make_test_array(2)).await;
            s.publish(make_test_array(3)).await;
        });

        // Receive all 3 — no drops in blocking mode.
        let a1 = receiver.recv().await.unwrap();
        assert_eq!(a1.unique_id, 1);
        let a2 = receiver.recv().await.unwrap();
        assert_eq!(a2.unique_id, 2);
        let a3 = receiver.recv().await.unwrap();
        assert_eq!(a3.unique_id, 3);

        pub_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_publish_drops_on_full_queue() {
        // B1: default (non-blocking) mode drops on a full queue and reports
        // DroppedQueueFull, matching C++ trySend.
        let (sender, _receiver) = ndarray_channel("TEST", 1);

        // First publish fills the queue.
        assert_eq!(
            sender.publish(make_test_array(1)).await,
            PublishOutcome::Delivered
        );
        // Second publish finds the queue full → dropped + counted.
        assert_eq!(
            sender.publish(make_test_array(2)).await,
            PublishOutcome::DroppedQueueFull
        );
    }

    #[tokio::test]
    async fn test_drop_on_full_does_not_leak_counter() {
        // A dropped array must not leave the queued-array counter incremented.
        let counter = Arc::new(QueuedArrayCounter::new());
        let (mut sender, _receiver) = ndarray_channel("TEST", 1);
        sender.set_queued_counter(counter.clone());

        sender.publish(make_test_array(1)).await; // delivered, counter=1
        assert_eq!(counter.get(), 1);
        let outcome = sender.publish(make_test_array(2)).await; // dropped
        assert_eq!(outcome, PublishOutcome::DroppedQueueFull);
        // Counter must still be 1 — the dropped message decremented on drop.
        assert_eq!(counter.get(), 1);
    }

    #[tokio::test]
    async fn test_blocking_callbacks_completion_wait() {
        let (sender, mut receiver) = ndarray_channel("TEST", 10);
        sender.blocking_mode.store(true, Ordering::Release);

        let completed = Arc::new(AtomicBool::new(false));
        let completed_clone = completed.clone();

        // Spawn receiver that takes some time to process
        let recv_handle = tokio::spawn(async move {
            let msg = receiver.recv_msg().await.unwrap();
            assert_eq!(msg.array.unique_id, 42);
            // Simulate processing time
            tokio::time::sleep(Duration::from_millis(50)).await;
            completed_clone.store(true, Ordering::Release);
            // msg dropped here → done_tx fires
        });

        // publish() should wait for completion
        sender.publish(make_test_array(42)).await;

        // By the time publish returns, downstream should have completed
        assert!(completed.load(Ordering::Acquire));

        recv_handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_fanout_three_receivers() {
        let (s1, mut r1) = ndarray_channel("P1", 10);
        let (s2, mut r2) = ndarray_channel("P2", 10);
        let (s3, mut r3) = ndarray_channel("P3", 10);

        let mut output = NDArrayOutput::new();
        output.add(s1);
        output.add(s2);
        output.add(s3);

        output.publish(make_test_array(42)).await;

        assert_eq!(r1.recv().await.unwrap().unique_id, 42);
        assert_eq!(r2.recv().await.unwrap().unique_id, 42);
        assert_eq!(r3.recv().await.unwrap().unique_id, 42);
    }

    #[test]
    fn test_blocking_recv() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (sender, mut receiver) = ndarray_channel("TEST", 10);

        let handle = std::thread::spawn(move || {
            let arr = receiver.blocking_recv().unwrap();
            arr.unique_id
        });

        rt.block_on(sender.publish(make_test_array(99)));
        let id = handle.join().unwrap();
        assert_eq!(id, 99);
    }

    #[tokio::test]
    async fn test_channel_closed_on_receiver_drop() {
        let (sender, receiver) = ndarray_channel("TEST", 10);
        drop(receiver);
        // Sending to closed channel should not panic
        sender.publish(make_test_array(1)).await;
    }

    #[test]
    fn test_queued_counter_basic() {
        let counter = QueuedArrayCounter::new();
        assert_eq!(counter.get(), 0);
        counter.increment();
        assert_eq!(counter.get(), 1);
        counter.increment();
        assert_eq!(counter.get(), 2);
        counter.decrement();
        assert_eq!(counter.get(), 1);
        counter.decrement();
        assert_eq!(counter.get(), 0);
    }

    #[test]
    fn test_queued_counter_wait_until_zero() {
        let counter = Arc::new(QueuedArrayCounter::new());
        counter.increment();
        counter.increment();

        let c = counter.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            c.decrement();
            std::thread::sleep(Duration::from_millis(10));
            c.decrement();
        });

        assert!(counter.wait_until_zero(Duration::from_secs(5)));
        h.join().unwrap();
    }

    #[test]
    fn test_queued_counter_wait_timeout() {
        let counter = Arc::new(QueuedArrayCounter::new());
        counter.increment();
        assert!(!counter.wait_until_zero(Duration::from_millis(10)));
    }

    #[tokio::test]
    async fn test_publish_increments_counter() {
        let counter = Arc::new(QueuedArrayCounter::new());
        let (mut sender, mut _receiver) = ndarray_channel("TEST", 10);
        sender.set_queued_counter(counter.clone());

        sender.publish(make_test_array(1)).await;
        assert_eq!(counter.get(), 1);
        sender.publish(make_test_array(2)).await;
        assert_eq!(counter.get(), 2);
    }

    #[tokio::test]
    async fn test_message_drop_decrements() {
        let counter = Arc::new(QueuedArrayCounter::new());
        counter.increment();
        let msg = ArrayMessage {
            array: make_test_array(1),
            counter: Some(counter.clone()),
            done_tx: None,
        };
        assert_eq!(counter.get(), 1);
        drop(msg);
        assert_eq!(counter.get(), 0);
    }

    /// One drop per overflow EPISODE per producer, C's `ignoreQueueFull`
    /// (NDPluginDriver.cpp:405, :433-441). Each case below is one boundary of
    /// the episode's lifetime, not one scenario.
    mod overflow_episode {
        use super::*;

        fn dropped(sender: &NDArraySender) -> i32 {
            sender.dropped_arrays.load(Ordering::Acquire)
        }

        #[tokio::test]
        async fn the_first_refusal_of_an_episode_counts() {
            let (sender, _receiver) = ndarray_channel("TEST", 1);
            sender.publish(make_test_array(1)).await; // fills the queue
            assert_eq!(
                sender.publish(make_test_array(2)).await,
                PublishOutcome::DroppedQueueFull
            );
            assert_eq!(dropped(&sender), 1);
        }

        #[tokio::test]
        async fn consecutive_refusals_do_not_count_again() {
            // C: the refusal at :433 leaves `auxStatus = asynOverflow`, so the
            // next call reads `ignoreQueueFull` and skips `droppedArrays++`.
            // Without this a detector pushing into a stalled plugin inflates
            // the counter by one per FRAME instead of one per stall.
            let (sender, _receiver) = ndarray_channel("TEST", 1);
            sender.publish(make_test_array(1)).await;
            for id in 2..=20 {
                assert_eq!(
                    sender.publish(make_test_array(id)).await,
                    PublishOutcome::DroppedQueueFull
                );
            }
            assert_eq!(dropped(&sender), 1, "19 dropped arrays, one episode");
        }

        #[tokio::test]
        async fn a_successful_enqueue_ends_the_episode() {
            // The other edge of the same cell: C never re-arms `auxStatus` on
            // the success path, and the unconditional `= asynSuccess` at :406
            // has already cleared it, so the next stall is a new episode.
            let (sender, mut receiver) = ndarray_channel("TEST", 1);
            sender.publish(make_test_array(1)).await;
            sender.publish(make_test_array(2)).await; // refused, counts
            sender.publish(make_test_array(3)).await; // refused, silent
            assert_eq!(dropped(&sender), 1);

            receiver.recv().await.unwrap(); // drain: room again
            sender.publish(make_test_array(4)).await; // accepted → episode over
            sender.publish(make_test_array(5)).await; // refused: new episode
            assert_eq!(dropped(&sender), 2);
        }

        #[tokio::test]
        async fn every_clone_of_a_sender_shares_one_episode() {
            // The cell is the receiving plugin's single registered
            // `pasynUserGenericPointer_` (:539-541), not per upstream: two
            // producers stalling on the same queue are one episode in C.
            let (sender, _receiver) = ndarray_channel("TEST", 1);
            let other = sender.clone();
            sender.publish(make_test_array(1)).await;
            sender.publish(make_test_array(2)).await;
            other.publish(make_test_array(3)).await;
            assert_eq!(dropped(&sender), 1);
        }

        #[tokio::test]
        async fn the_reinjection_producer_carries_its_own_episode() {
            // C reaches `driverCallback` with `pasynUserSelf` for a
            // ProcessPlugin re-injection (:741) and with
            // `pasynUserGenericPointer_` for an array-port callback, so an
            // array-port overflow must not silence the re-injection's first
            // drop.
            let (sender, _receiver) = ndarray_channel("TEST", 1);
            let handle = sender.self_queue_handle();
            sender.publish(make_test_array(1)).await; // fills
            sender.publish(make_test_array(2)).await; // array-port episode opens
            assert_eq!(dropped(&sender), 1);

            assert_eq!(
                handle.try_enqueue(make_test_array(3)),
                Some(PublishOutcome::DroppedQueueFull)
            );
            assert_eq!(
                dropped(&sender),
                2,
                "a separate producer, a separate episode"
            );
            handle.try_enqueue(make_test_array(4));
            assert_eq!(dropped(&sender), 2, "…which then runs its own episode");
        }

        #[tokio::test]
        async fn a_scatter_reroute_arms_the_episode_and_the_last_node_still_counts() {
            // C `NDPluginScatter.cpp:83-84` writes the SAME cell: overflow for
            // a node it means to reroute past, success for the last. Because
            // it is one cell and not a second mechanism, the last node counts
            // its drop even though the round before it armed the flag.
            let (sender, _receiver) = ndarray_channel("TEST", 1);
            sender.publish(make_test_array(1)).await; // fills
            assert_eq!(
                sender.publish_scatter(make_test_array(2), false).await,
                PublishOutcome::DroppedQueueFull
            );
            assert_eq!(dropped(&sender), 0, "rerouted past, not dropped");
            assert_eq!(
                sender.publish_scatter(make_test_array(3), true).await,
                PublishOutcome::DroppedQueueFull
            );
            assert_eq!(dropped(&sender), 1, "the last node owns the drop");
        }

        #[tokio::test]
        async fn the_blocking_arm_ends_an_open_episode() {
            // C clears `auxStatus` at :406, before it branches on
            // `blockingCallbacks`, and the blocking arm never touches the
            // queue — so switching to blocking mode and back leaves no stale
            // episode to swallow the next real drop.
            let (sender, mut receiver) = ndarray_channel("TEST", 1);
            sender.publish(make_test_array(1)).await;
            sender.publish(make_test_array(2)).await; // episode opens
            assert_eq!(dropped(&sender), 1);

            receiver.recv().await.unwrap();
            sender.blocking_mode.store(true, Ordering::Release);
            let s = sender.clone();
            let pending = tokio::spawn(async move { s.publish(make_test_array(3)).await });
            let msg = receiver.recv_msg().await.unwrap();
            drop(msg);
            pending.await.unwrap();

            sender.blocking_mode.store(false, Ordering::Release);
            sender.publish(make_test_array(4)).await; // fills
            sender.publish(make_test_array(5)).await; // refused: counts
            assert_eq!(dropped(&sender), 2);
        }
    }
}
