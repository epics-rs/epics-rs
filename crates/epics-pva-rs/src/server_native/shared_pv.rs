//! [`SharedPV`] — open/post/close mailbox PV for server-side use.
//!
//! Mirrors pvxs `sharedpv.cpp::SharedPV`. A SharedPV holds the current
//! value of a single PV and exposes:
//!
//! - `open(initial)` to declare the type/value
//! - `post(value)` to push a new value to all current subscribers
//! - `close()` to drop subscriptions and reject further GETs
//!
//! Many SharedPVs can be plugged into a single server via
//! [`SharedSource`] (collection mapping name → SharedPV).
//!
//! Flow control: each subscriber owns a bounded [`MonitorInbox`] backed
//! by `Mutex<VecDeque>+Notify`. When the queue is full, a normal post
//! replaces the tail entry with the newest value (squash-to-tail;
//! pvxs `servermon.cpp:283-286`). A `maybe` post is silently dropped.
//! This matches pvxs semantics: slow subscribers converge to the latest
//! posted state after congestion clears.
//!
//! Watermarks (low/high) are advisory hints stored on the SharedPV and
//! consulted by op_monitor when it decides whether to acknowledge a
//! pipeline window. We don't yet wire them into the wire-level
//! ackCount but the API is in place for callers to set them.

use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::pvdata::{FieldDesc, PvField};

/// User-provided put handler. Mirrors pvxs `SharedPV::onPut`
/// (sharedpv.cpp:329). Handler receives the new value; returning Err
/// causes the server to reply with a non-success Status. Returning
/// Ok(()) lets the server post the value to subscribers — handlers
/// that want to coerce / transform should do so via [`SharedPV::try_post`]
/// inside the closure and return Ok.
pub type OnPutFn = Arc<dyn Fn(&SharedPV, PvField) -> Result<(), String> + Send + Sync>;

/// User-provided process handler. Fired by the PVA `PROCESS` wire
/// command (cmd 16) — processing is triggered with no value payload,
/// the wire equivalent of an EPICS `dbProcess` / `caput .PROC`.
/// Returning `Err` makes the server reply with a non-success PROCESS
/// status. When no handler is installed `process()` is a no-op
/// success, mirroring a passive record's response to `.PROC`.
pub type OnProcessFn = Arc<dyn Fn(&SharedPV) -> Result<(), String> + Send + Sync>;

/// User-provided RPC handler. Mirrors pvxs `SharedPV::onRPC`. Handler
/// receives `(request_desc, request_value)` and returns the response
/// pair or an error message.
pub type OnRpcFn = Arc<
    dyn Fn(&SharedPV, FieldDesc, PvField) -> Result<(FieldDesc, PvField), String> + Send + Sync,
>;

/// Async RPC handler. Returns a boxed future the dispatch path
/// awaits, so the user's async work runs on the calling task's
/// runtime without `block_in_place`/`block_on`. Used by the
/// `#[pva_service]` framework.
pub type OnRpcAsyncFn = Arc<
    dyn Fn(
            SharedPV,
            FieldDesc,
            PvField,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send>,
        > + Send
        + Sync,
>;

/// Lifecycle callback fired when the first subscriber connects or the
/// last one disappears. pvxs `SharedPV::onFirstConnect` /
/// `SharedPV::onLastDisconnect` (sharedpv.cpp:303-323).
pub type LifecycleFn = Arc<dyn Fn(&SharedPV) + Send + Sync>;

/// Watermark-crossing callback fired when ANY of this SharedPV's
/// monitor outboxes fills past `high_watermark` (transition into
/// over-high) or drains back to empty (transition out of over-high
/// — "low mark").
///
/// Mirrors pvxs `MonitorControlOp::onHighMark` / `onLowMark`
/// (sharedpv.cpp:354-371). Producers use these to throttle their
/// post-rate when the consumer falls behind, instead of relying on
/// the lossy default (drop or squash). Typical use:
///
/// ```ignore
/// shared.set_on_high_mark(Arc::new(|pv| {
///     // Slow viewer detected — back off polling
///     PRODUCER_RATE.store(LOW_RATE, Ordering::Relaxed);
/// }));
/// shared.set_on_low_mark(Arc::new(|pv| {
///     PRODUCER_RATE.store(NORMAL_RATE, Ordering::Relaxed);
/// }));
/// ```
pub type WatermarkFn = Arc<dyn Fn(&SharedPV) + Send + Sync>;

/// PVA-FR-11: monitor start/stop callback. Fired with `true` when a
/// downstream client begins or resumes MONITOR updates and `false` when
/// it pauses, cancels, disconnects, or destroys the subscription.
/// Mirrors pvxs `MonitorControlOp::onStart(std::function<void(bool)>)`
/// (`source.h:130`). Producers gate expensive sampling / upstream
/// subscriptions on it — start work on `true`, suspend on `false`:
///
/// ```ignore
/// shared.set_on_start(Arc::new(|_pv, start| {
///     if start { SAMPLER.resume() } else { SAMPLER.suspend() }
/// }));
/// ```
///
/// The wire layer fires it exactly once per Executing<->Idle edge per
/// op (one `MonitorStartControl` per op), so a handler never sees a
/// duplicate `true`/`false` or a `false` without a preceding `true`.
pub type OnStartFn = Arc<dyn Fn(&SharedPV, bool) + Send + Sync>;

// ── Per-subscriber bounded queue with squash-to-tail semantics ──────────────

struct MonitorQueueInner {
    items: VecDeque<PvField>,
    limit: usize,
    /// True once the producer side (SharedPV) signals no more data.
    producer_done: bool,
}

struct MonitorQueueShared {
    inner: Mutex<MonitorQueueInner>,
    notify: tokio::sync::Notify,
    /// Set in MonitorInbox::drop; post() checks this to decide whether to remove
    /// the outbox from the subscriber list.
    receiver_dropped: AtomicBool,
    /// Live `MonitorOutbox` endpoints for this queue. MR-R2: closure
    /// (`producer_done`) must be tied to the *last* producer endpoint
    /// disappearing, not to any single cloned endpoint dropping. A
    /// temporary clone made for a lock-free post (e.g. `put_delta`'s
    /// `g.subscribers.clone()`) keeps this count above 1 while the
    /// canonical outbox lives, so its drop never closes the inbox.
    producer_count: AtomicUsize,
}

/// Sender half of a per-subscriber queue. Held by `SharedPV::subscribers`.
///
/// MR-R2: `Clone` is implemented by hand (not derived) so each clone
/// increments `producer_count`. The invariant — "a monitor queue
/// becomes `producer_done` only when its *last* `MonitorOutbox`
/// endpoint drops" — is enforced structurally here and in `Drop`,
/// so a transient clone used for lock-free delivery cannot close the
/// subscriber's inbox.
pub struct MonitorOutbox {
    shared: Arc<MonitorQueueShared>,
}

/// Receiver half of a per-subscriber queue. Returned by `SharedPV::subscribe`.
pub struct MonitorInbox {
    shared: Arc<MonitorQueueShared>,
}

fn make_monitor_queue(limit: usize) -> (MonitorOutbox, MonitorInbox) {
    let limit = limit.max(1);
    let shared = Arc::new(MonitorQueueShared {
        inner: Mutex::new(MonitorQueueInner {
            items: VecDeque::with_capacity(limit),
            limit,
            producer_done: false,
        }),
        notify: tokio::sync::Notify::new(),
        receiver_dropped: AtomicBool::new(false),
        // Exactly one producer endpoint exists at creation.
        producer_count: AtomicUsize::new(1),
    });
    (
        MonitorOutbox {
            shared: Arc::clone(&shared),
        },
        MonitorInbox { shared },
    )
}

impl MonitorOutbox {
    /// Post a value. `maybe=false`: full queue → squash tail (pvxs servermon.cpp:283-286).
    /// `maybe=true`: full queue → drop silently.
    /// Returns `false` when the receiver has been dropped (caller should remove this outbox).
    fn post(&self, value: PvField, maybe: bool) -> bool {
        if self.shared.receiver_dropped.load(Ordering::Relaxed) {
            return false;
        }
        let mut inner = self.shared.inner.lock();
        if inner.producer_done {
            // Producer already closed; keep outbox alive so Drop delivers the signal.
            return true;
        }
        if inner.items.len() < inner.limit {
            inner.items.push_back(value);
        } else if !maybe {
            // pvxs servermon.cpp:283-286: queue.back().assign(val) — squash tail
            if let Some(tail) = inner.items.back_mut() {
                *tail = value;
            }
        }
        // maybe+full: drop silently — same as pvxs "nope" branch (servermon.cpp:287)
        drop(inner);
        self.shared.notify.notify_one();
        !self.shared.receiver_dropped.load(Ordering::Relaxed)
    }

    fn is_closed(&self) -> bool {
        self.shared.receiver_dropped.load(Ordering::Relaxed)
    }
}

impl Clone for MonitorOutbox {
    fn clone(&self) -> Self {
        // MR-R2: every live endpoint counts. A clone made for a
        // lock-free post is a producer endpoint until it drops.
        self.shared.producer_count.fetch_add(1, Ordering::AcqRel);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl Drop for MonitorOutbox {
    fn drop(&mut self) {
        // MR-R2: signal `producer_done` only when the *last* producer
        // endpoint for this queue drops. A transient clone (e.g. the
        // `put_delta` snapshot) drops first while the canonical outbox
        // held in `SharedPV::subscribers` is still alive, so the
        // subscriber's inbox is not closed by an internal clone.
        if self.shared.producer_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.inner.lock().producer_done = true;
            self.shared.notify.notify_waiters();
        }
    }
}

impl Drop for MonitorInbox {
    fn drop(&mut self) {
        self.shared.receiver_dropped.store(true, Ordering::Relaxed);
    }
}

impl MonitorInbox {
    /// Async receive. Returns `None` when the producer closed and the queue is drained.
    pub async fn recv(&mut self) -> Option<PvField> {
        loop {
            let notified = self.shared.notify.notified();
            tokio::pin!(notified);
            // Register the waiter before checking so a concurrent notify_one()
            // fired between check and await is captured (same Notify::enable()
            // pattern as channel.rs wait_until_inactive).
            notified.as_mut().enable();
            {
                let mut inner = self.shared.inner.lock();
                if let Some(v) = inner.items.pop_front() {
                    return Some(v);
                }
                if inner.producer_done {
                    return None;
                }
            }
            notified.await;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Per-PV state stored inside [`SharedPV`].
struct Inner {
    /// Type descriptor declared at open() — None when not opened.
    desc: Option<FieldDesc>,
    /// Most recent value (defaulted from desc on open).
    value: Option<PvField>,
    /// Open subscribers. Each slot holds a MonitorOutbox for squash-to-tail delivery.
    subscribers: Vec<MonitorOutbox>,
    /// Optional flow-control watermark: monitor stream sends MORE
    /// only when its outbox depth crosses below `low_watermark`.
    /// Currently advisory; preserved here for op_monitor to consult.
    pub low_watermark: usize,
    /// Pause sending updates when the monitor outbox depth is at or
    /// above `high_watermark`. Currently advisory.
    pub high_watermark: usize,
    /// `is_open` is required to reject GETs after close().
    is_open: bool,
    /// Optional user put handler; when None the default "store and
    /// post" behavior runs. pvxs `onPut` parity.
    on_put: Option<OnPutFn>,
    /// Optional user RPC handler; when None RPC returns "not
    /// supported". pvxs `onRPC` parity.
    on_rpc: Option<OnRpcFn>,
    /// Optional user PROCESS handler; when None `process()` is a
    /// no-op success (passive-record semantics).
    on_process: Option<OnProcessFn>,
    /// Optional async RPC handler. Takes precedence over `on_rpc`
    /// when both are set. Used by the `service` framework
    /// (`#[pva_service]`) so dispatch can run on the calling task's
    /// own runtime without `block_in_place`/`block_on`.
    on_rpc_async: Option<OnRpcAsyncFn>,
    /// First-subscriber-arrived hook.
    on_first_connect: Option<LifecycleFn>,
    /// Last-subscriber-left hook.
    on_last_disconnect: Option<LifecycleFn>,
    /// Outbox crossed `high_watermark` going up. Producer throttle
    /// hint. See [`WatermarkFn`].
    on_high_mark: Option<WatermarkFn>,
    /// Outbox drained back to zero (or below `low_watermark`).
    /// Producer un-throttle hint.
    on_low_mark: Option<WatermarkFn>,
    /// PVA-FR-11: monitor start/stop hook (pvxs `onStart`). See
    /// [`OnStartFn`].
    on_start: Option<OnStartFn>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            desc: None,
            value: None,
            subscribers: Vec::new(),
            low_watermark: 4,
            high_watermark: 64,
            is_open: false,
            on_put: None,
            on_rpc: None,
            on_process: None,
            on_rpc_async: None,
            on_first_connect: None,
            on_last_disconnect: None,
            on_high_mark: None,
            on_low_mark: None,
            on_start: None,
        }
    }
}

/// Server-side handle for a single PV's value + subscriber set.
///
/// Cheap to clone: it's just an `Arc<Mutex<...>>`.
#[derive(Clone)]
pub struct SharedPV {
    inner: Arc<Mutex<Inner>>,
}

impl SharedPV {
    /// New, unopened SharedPV. open() must be called before serving GETs.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }

    /// Declare the type and seed the initial value. Repeated calls
    /// replace the type and value; subscribers are kept and will see
    /// the new value on next post().
    pub fn open(&self, desc: FieldDesc, initial: PvField) {
        let mut g = self.inner.lock();
        g.desc = Some(desc);
        g.value = Some(initial);
        g.is_open = true;
    }

    /// Returns true iff the PV has been opened.
    pub fn is_open(&self) -> bool {
        self.inner.lock().is_open
    }

    /// Drop all subscribers; subsequent GETs return `None` until
    /// open() is called again.
    pub fn close(&self) {
        let mut g = self.inner.lock();
        g.is_open = false;
        g.subscribers.clear();
    }

    /// Type descriptor (None until opened).
    pub fn introspection(&self) -> Option<FieldDesc> {
        self.inner.lock().desc.clone()
    }

    /// Current value (None until opened).
    pub fn current(&self) -> Option<PvField> {
        self.inner.lock().value.clone()
    }

    /// Push a new value to all subscribers; lossy semantics — drops
    /// updates when a subscriber's outbox is full. Returns the number
    /// of subscribers we successfully sent to.
    ///
    /// PVA-R5: a descriptor/value mismatch is logged at warn level
    /// and the post is dropped (returns 0). The shape is "best
    /// effort" because `-> usize` predates Result-typed posts;
    /// internal callers that can handle a Result should use
    /// [`Self::try_post_checked`] instead, which mirrors pvxs
    /// `sharedpv.cpp:417-431` and refuses post on:
    /// (a) PV not yet opened, (b) descriptor mismatch.
    pub fn try_post(&self, value: PvField) -> usize {
        if let Err(e) = self.try_post_checked(value) {
            tracing::warn!(
                error = %e,
                "SharedPV::try_post: dropped — value does not fit opened descriptor"
            );
            return 0;
        }
        // `try_post_checked` already updated `value` + delivered to
        // every live subscriber. It does not return a count; we
        // recompute by inspecting subscriber map size after the
        // post (a transient under-count under heavy churn is
        // acceptable for the legacy `-> usize` API).
        let g = self.inner.lock();
        g.subscribers.len()
    }

    /// Result-typed post with descriptor enforcement. PVA-R5:
    /// mirrors pvxs `sharedpv.cpp:417-431`. Returns `Err` when the
    /// PV is not yet opened, or when the value's runtime shape
    /// does not fit the opened descriptor. Subscribers see the new
    /// value only on `Ok`.
    pub fn try_post_checked(&self, value: PvField) -> crate::error::PvaResult<usize> {
        let mut g = self.inner.lock();
        if !g.is_open {
            return Err(crate::error::PvaError::Protocol(
                "SharedPV not open".to_string(),
            ));
        }
        if let Some(desc) = g.desc.as_ref() {
            if let Err(e) = crate::pvdata::value_matches_descriptor(&value, desc) {
                return Err(crate::error::PvaError::InvalidValue(format!(
                    "SharedPV::try_post: value does not fit opened descriptor ({e})"
                )));
            }
        }
        g.value = Some(value.clone());
        // pvxs servermon.cpp:283-286: normal post squashes tail when queue full.
        g.subscribers.retain(|tx| tx.post(value.clone(), false));
        Ok(g.subscribers.len())
    }

    /// Add a subscriber. Returns a [`MonitorInbox`] that yields posted values
    /// with squash-to-tail semantics (pvxs `servermon.cpp:283-286`) when the
    /// `limit`-deep queue is full. Drops on the receiver side translate to
    /// outbox removal on the next post.
    ///
    /// `limit` is the maximum number of unread events; pvxs default is 4
    /// (`servermon.cpp:66`). Values ≥ 1 are accepted; 0 is clamped to 1.
    pub fn subscribe(&self, limit: usize) -> Option<MonitorInbox> {
        // Latch onFirstConnect callback to run *after* releasing the
        // lock — handlers may call back into post() / current() and we
        // can't recurse on parking_lot Mutex.
        let cb = {
            let mut g = self.inner.lock();
            if !g.is_open {
                return None;
            }
            let (outbox, inbox) = make_monitor_queue(limit);
            if let Some(v) = &g.value {
                // Initial value: queue is empty so limit not yet reached.
                outbox.post(v.clone(), false);
            }
            let was_empty = g.subscribers.is_empty();
            g.subscribers.push(outbox);
            let cb = if was_empty {
                g.on_first_connect.clone()
            } else {
                None
            };
            (inbox, cb)
        };
        if let Some(f) = cb.1 {
            f(self);
        }
        Some(cb.0)
    }

    /// Apply a PUT. By default, the new value is posted to all
    /// subscribers and stored as `current()`. When [`Self::on_put`]
    /// has been set, the user handler runs instead and is responsible
    /// for any side-effects / re-posting. Mirrors pvxs `onPut`
    /// dispatch.
    pub fn put(&self, value: PvField) -> Result<(), String> {
        if !self.is_open() {
            return Err("SharedPV not open".into());
        }
        let on_put = self.inner.lock().on_put.clone();
        if let Some(f) = on_put {
            return f(self, value);
        }
        let _ = self.try_post(value);
        Ok(())
    }

    /// Apply a **BitSet-delta PUT** atomically.
    ///
    /// PVA PUT/PUT_GET data frames carry only the changed fields plus
    /// a changed-BitSet (pvData spec §5.4). Applying such a delta
    /// requires read-merge-write: read the PV's current "complete"
    /// value, overlay the marked fields from `delta`, store the
    /// result. Doing that as a separate `current()` then `put()` on
    /// the wire layer opens a TOCTOU lost-update window — two
    /// concurrent partial PUTs with disjoint changed-fields can both
    /// read the same prior, and the second write silently drops the
    /// first's fields.
    ///
    /// This method performs the read + merge + store under a SINGLE
    /// acquisition of the inner mutex, so concurrent delta PUTs
    /// serialize: the second writer's merge sees the first writer's
    /// stored value as its prior. `desc` is the PV introspection used
    /// for per-field bit numbering; `changed` is the wire changed-
    /// BitSet; `delta` is the decoded sparse value (unmarked leaves
    /// hold type defaults).
    ///
    /// For PVs with an [`Self::on_put`] handler, the merge against
    /// `current()` is still atomic, but the handler then owns any
    /// further side-effects exactly as it does for [`Self::put`].
    pub fn put_delta(
        &self,
        desc: &FieldDesc,
        changed: &crate::proto::BitSet,
        delta: PvField,
    ) -> Result<(), String> {
        // Phase 1: under the lock, read prior, merge, and (for the
        // default no-handler path) store + snapshot subscribers.
        // `crate::pvdata::encode::fill_unmarked_from_prior` is a pure
        // function — safe to call while holding the parking_lot mutex.
        enum Applied {
            // No on_put handler: value already stored under the lock;
            // post the merged value to these subscribers.
            Posted {
                value: PvField,
                subscribers: Vec<MonitorOutbox>,
            },
            // on_put handler installed: run it with the merged value.
            Handler {
                handler: OnPutFn,
                value: PvField,
            },
        }
        let applied = {
            let mut g = self.inner.lock();
            if !g.is_open {
                return Err("SharedPV not open".into());
            }
            let merged = match &g.value {
                Some(prior) => {
                    crate::pvdata::encode::fill_unmarked_from_prior(desc, changed, 0, delta, prior)
                }
                // No prior value yet: the delta is all we have.
                None => delta,
            };
            match g.on_put.clone() {
                Some(handler) => Applied::Handler {
                    handler,
                    value: merged,
                },
                None => {
                    // PVA-R5: descriptor enforcement for the
                    // no-handler store path. Without a check the
                    // merged value could carry a shape unrelated
                    // to the opened descriptor (pvxs `sharedpv.cpp:
                    // 417-431` rejects this). Compare against
                    // `g.desc` (the opened descriptor, not the
                    // per-put `desc` parameter — the wire request
                    // may carry a stale descriptor cached by the
                    // peer).
                    if let Some(opened) = g.desc.as_ref() {
                        if let Err(e) = crate::pvdata::value_matches_descriptor(&merged, opened) {
                            return Err(format!(
                                "SharedPV::put_delta: merged value does not fit opened descriptor ({e})"
                            ));
                        }
                    }
                    // Store atomically with the read above so a
                    // concurrent put_delta sees this as its prior.
                    g.value = Some(merged.clone());
                    Applied::Posted {
                        value: merged,
                        subscribers: g.subscribers.clone(),
                    }
                }
            }
        };
        // Phase 2: outside the lock — post to subscribers or run the
        // user handler (handlers may call back into SharedPV).
        match applied {
            Applied::Posted {
                value,
                mut subscribers,
            } => {
                // pvxs servermon.cpp:283-286: squash-to-tail for normal post.
                subscribers.retain(|tx| tx.post(value.clone(), false));
                // Reconcile the canonical subscriber set: drop receivers that
                // closed between the phase-1 snapshot and now.
                let mut g = self.inner.lock();
                g.subscribers.retain(|tx| !tx.is_closed());
                Ok(())
            }
            Applied::Handler { handler, value } => handler(self, value),
        }
    }

    /// Dispatch an RPC request. Falls back to "RPC not supported" when
    /// no [`Self::on_rpc`] handler has been installed.
    pub fn rpc(
        &self,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> Result<(FieldDesc, PvField), String> {
        let on_rpc = self.inner.lock().on_rpc.clone();
        match on_rpc {
            Some(f) => f(self, request_desc, request_value),
            None => Err("RPC not supported by this SharedPV".into()),
        }
    }

    /// Trigger PROCESS on this PV. Runs the installed [`Self::on_process`]
    /// handler; with no handler this is a no-op success (a passive
    /// record returns OK to a `.PROC` write). Mirrors how pvxs would
    /// route a `PROCESS` wire command into the underlying record.
    pub fn process(&self) -> Result<(), String> {
        if !self.is_open() {
            return Err("SharedPV not open".into());
        }
        let on_process = self.inner.lock().on_process.clone();
        match on_process {
            Some(f) => f(self),
            None => Ok(()),
        }
    }

    /// Async RPC dispatch. Tries the async handler first
    /// (registered via [`Self::on_rpc_async`]); falls back to the
    /// sync `on_rpc` handler when only that one is set; finally
    /// returns "not supported" when neither is installed. The
    /// `#[pva_service]` framework uses this so user async methods
    /// run on the calling task's runtime, no `block_in_place`.
    pub async fn rpc_async(
        &self,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> Result<(FieldDesc, PvField), String> {
        let (sync, async_h) = {
            let g = self.inner.lock();
            (g.on_rpc.clone(), g.on_rpc_async.clone())
        };
        if let Some(f) = async_h {
            return f(self.clone(), request_desc, request_value).await;
        }
        match sync {
            Some(f) => f(self, request_desc, request_value),
            None => Err("RPC not supported by this SharedPV".into()),
        }
    }

    /// Install a put handler. Pass `None` to clear. Mirrors pvxs
    /// `SharedPV::onPut`.
    pub fn on_put<F>(&self, handler: F)
    where
        F: Fn(&SharedPV, PvField) -> Result<(), String> + Send + Sync + 'static,
    {
        self.inner.lock().on_put = Some(Arc::new(handler));
    }

    /// Install an RPC handler. Mirrors pvxs `SharedPV::onRPC`.
    pub fn on_rpc<F>(&self, handler: F)
    where
        F: Fn(&SharedPV, FieldDesc, PvField) -> Result<(FieldDesc, PvField), String>
            + Send
            + Sync
            + 'static,
    {
        self.inner.lock().on_rpc = Some(Arc::new(handler));
    }

    /// Install a process handler. Pass `None` to clear (by re-installing).
    /// Fired by the PVA `PROCESS` wire command — see [`OnProcessFn`].
    pub fn on_process<F>(&self, handler: F)
    where
        F: Fn(&SharedPV) -> Result<(), String> + Send + Sync + 'static,
    {
        self.inner.lock().on_process = Some(Arc::new(handler));
    }

    /// Install an async RPC handler. Used by `#[pva_service]` so the
    /// generated dispatch can `await` user code without bouncing
    /// through `block_on`. Takes a closure that returns a future;
    /// the dispatch path awaits the future on the same tokio task
    /// that delivered the RPC frame.
    pub fn on_rpc_async<F, Fut>(&self, handler: F)
    where
        F: Fn(SharedPV, FieldDesc, PvField) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send + 'static,
    {
        let arc: OnRpcAsyncFn = Arc::new(move |pv, d, v| Box::pin(handler(pv, d, v)));
        self.inner.lock().on_rpc_async = Some(arc);
    }

    /// Hook fired when the *first* subscriber connects (subscribers
    /// 0 → 1). Mirrors pvxs `SharedPV::onFirstConnect` —
    /// applications hook here to start a producer task on demand.
    pub fn on_first_connect<F>(&self, handler: F)
    where
        F: Fn(&SharedPV) + Send + Sync + 'static,
    {
        self.inner.lock().on_first_connect = Some(Arc::new(handler));
    }

    /// Hook fired when the *last* subscriber leaves (subscribers
    /// N → 0). Mirrors pvxs `SharedPV::onLastDisconnect` — pair with
    /// `on_first_connect` to gate cost-of-production on actual
    /// listener interest.
    pub fn on_last_disconnect<F>(&self, handler: F)
    where
        F: Fn(&SharedPV) + Send + Sync + 'static,
    {
        self.inner.lock().on_last_disconnect = Some(Arc::new(handler));
    }

    /// Non-allocating snapshot — copies the current value into `out`
    /// without cloning if the descriptors match. Returns false when
    /// the PV isn't opened or has no value yet. Mirrors pvxs
    /// `SharedPV::fetch`.
    pub fn fetch(&self, out: &mut Option<PvField>) -> bool {
        let g = self.inner.lock();
        match (&g.value, g.is_open) {
            (Some(v), true) => {
                *out = Some(v.clone());
                true
            }
            _ => false,
        }
    }

    /// Drop dead (closed-receiver) subscribers and fire
    /// `on_last_disconnect` if the set just became empty. Called by
    /// the per-channel TCP task on monitor close so SharedPV can
    /// notice subscribers leaving without waiting for the next post().
    pub fn prune_subscribers(&self) {
        let cb = {
            let mut g = self.inner.lock();
            let was_nonempty = !g.subscribers.is_empty();
            g.subscribers.retain(|tx| !tx.is_closed());
            if was_nonempty && g.subscribers.is_empty() {
                g.on_last_disconnect.clone()
            } else {
                None
            }
        };
        if let Some(f) = cb {
            f(self);
        }
    }

    /// Set the low watermark hint (advisory).
    pub fn set_low_watermark(&self, low: usize) {
        self.inner.lock().low_watermark = low;
    }

    /// Set the high watermark hint (advisory).
    pub fn set_high_watermark(&self, high: usize) {
        self.inner.lock().high_watermark = high;
    }

    /// Read the current watermark pair.
    pub fn watermarks(&self) -> (usize, usize) {
        let g = self.inner.lock();
        (g.low_watermark, g.high_watermark)
    }

    /// Install a high-mark callback. The callback fires when ANY
    /// monitor outbox of this SharedPV transitions from below to
    /// above `high_watermark`. Producers can use this to throttle
    /// their `post()` rate when the slow consumer is falling
    /// behind. See [`WatermarkFn`] for the typical pattern.
    pub fn set_on_high_mark(&self, cb: WatermarkFn) {
        self.inner.lock().on_high_mark = Some(cb);
    }

    /// Install a low-mark callback (paired with `on_high_mark`).
    /// Fires when an outbox drains back to empty after having
    /// crossed `high_watermark`. Use to un-throttle the producer.
    pub fn set_on_low_mark(&self, cb: WatermarkFn) {
        self.inner.lock().on_low_mark = Some(cb);
    }

    /// Internal: snapshot the current high-mark / low-mark callbacks
    /// so the per-connection monitor task can fire them without
    /// holding the inner mutex across `.await`. Returns `(high, low)`.
    pub(crate) fn watermark_handlers(&self) -> (Option<WatermarkFn>, Option<WatermarkFn>) {
        let g = self.inner.lock();
        (g.on_high_mark.clone(), g.on_low_mark.clone())
    }

    /// PVA-FR-11: install a monitor start/stop callback. Fired with
    /// `true` when a downstream client begins or resumes MONITOR updates
    /// and `false` when it pauses, cancels, disconnects, or destroys the
    /// subscription. Mirrors pvxs `MonitorControlOp::onStart`. Use it to
    /// start/stop work that only matters while a client is consuming
    /// (upstream subscriptions, hardware sampling). See [`OnStartFn`].
    pub fn set_on_start(&self, cb: OnStartFn) {
        self.inner.lock().on_start = Some(cb);
    }

    /// Internal: snapshot the current monitor start/stop callback so the
    /// per-connection control path can fire it without holding the inner
    /// mutex across the call.
    pub(crate) fn on_start_handler(&self) -> Option<OnStartFn> {
        self.inner.lock().on_start.clone()
    }
}

impl Default for SharedPV {
    fn default() -> Self {
        Self::new()
    }
}

/// Trivial map-of-named-SharedPV adapter that implements
/// [`super::source::ChannelSource`]. Construct via `SharedSource::new()`,
/// `add(name, shared_pv)`, then pass to [`super::runtime::run_pva_server`].
pub struct SharedSource {
    pvs: Mutex<HashMap<String, SharedPV>>,
    /// Optional per-source access gate. When `None`, the trait
    /// default open gate is returned (back-compat). When `Some`,
    /// every wire op routes its allow/deny check through this
    /// gate — use with `AccessGate::required(acf, resolver)` to
    /// enforce a real .acf policy against PVA clients. Set via
    /// [`SharedSource::set_access_gate`].
    access_gate: std::sync::OnceLock<epics_base_rs::server::access_security::AccessGate>,
}

impl SharedSource {
    pub fn new() -> Self {
        Self {
            pvs: Mutex::new(HashMap::new()),
            access_gate: std::sync::OnceLock::new(),
        }
    }

    /// Install an [`AccessGate`](epics_base_rs::server::access_security::AccessGate) on this source. Subsequent wire
    /// ops use it for every allow/deny check. Idempotent — only
    /// the first call wins (the gate is stored in a `OnceLock`
    /// so subscribers can hold borrowed references without
    /// invalidation races). For dynamic ACF reload, use the
    /// existing `AccessGate::required(...)` shape whose internal
    /// `Arc<RwLock<...>>` already supports hot-swap.
    pub fn set_access_gate(
        &self,
        gate: epics_base_rs::server::access_security::AccessGate,
    ) -> Result<(), &'static str> {
        self.access_gate
            .set(gate)
            .map_err(|_| "SharedSource access gate already installed")
    }

    pub fn add(&self, name: impl Into<String>, pv: SharedPV) {
        let key = name.into();
        // R-6: warn on silent overwrite. A second registration with
        // the same name swaps in the new SharedPV — but in-flight
        // clones held by ongoing RPCs still reference the previous
        // SharedPV, so live operations don't migrate. Surfacing
        // this nudges callers toward `remove` then `add` for
        // intentional swaps.
        if self.pvs.lock().insert(key.clone(), pv).is_some() {
            tracing::warn!(
                pv = %key,
                "SharedSource::add overwriting an existing PV — \
                 in-flight operations on the old SharedPV continue \
                 with stale state. Call SharedSource::remove first \
                 for intentional swaps."
            );
        }
    }

    /// Remove a previously-added PV by name. Returns the removed
    /// SharedPV when the name was present. Used by
    /// [`crate::service::remove_rpc_service`] to tear down an RPC
    /// service registered via `add_rpc_service`; also useful for
    /// dynamic IOC topologies where PVs come and go at runtime.
    pub fn remove(&self, name: &str) -> Option<SharedPV> {
        self.pvs.lock().remove(name)
    }

    pub fn get(&self, name: &str) -> Option<SharedPV> {
        self.pvs.lock().get(name).cloned()
    }
}

impl Default for SharedSource {
    fn default() -> Self {
        Self::new()
    }
}

impl super::source::ChannelSource for SharedSource {
    fn access(&self) -> &epics_base_rs::server::access_security::AccessGate {
        match self.access_gate.get() {
            Some(g) => g,
            None => {
                static OPEN_GATE: std::sync::OnceLock<
                    epics_base_rs::server::access_security::AccessGate,
                > = std::sync::OnceLock::new();
                OPEN_GATE.get_or_init(epics_base_rs::server::access_security::AccessGate::open)
            }
        }
    }

    fn list_pvs(&self) -> impl std::future::Future<Output = Vec<String>> + Send {
        let names: Vec<String> = self.pvs.lock().keys().cloned().collect();
        async move { names }
    }

    fn has_pv(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let exists = self.pvs.lock().contains_key(name);
        async move { exists }
    }

    fn get_introspection(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        let pv = self.pvs.lock().get(name).cloned();
        async move { pv.and_then(|p| p.introspection()) }
    }

    fn get_value(&self, name: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let pv = self.pvs.lock().get(name).cloned();
        async move { pv.and_then(|p| p.current()) }
    }

    fn put_value(
        &self,
        name: &str,
        value: PvField,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let pv = self.pvs.lock().get(name).cloned();
        async move {
            match pv {
                Some(p) => p.put(value),
                None => Err(format!("no such PV: {name}")),
            }
        }
    }

    /// Atomic BitSet-delta PUT: routes to [`SharedPV::put_delta`],
    /// which reads + merges + stores under a single mutex
    /// acquisition. Closes the TOCTOU lost-update window the default
    /// trait impl (`get_value` + merge + `put_value`) has under
    /// concurrent partial PUTs.
    fn put_delta_checked(
        &self,
        checked: super::source::AccessChecked,
        desc: FieldDesc,
        changed: crate::proto::BitSet,
        delta: PvField,
        ctx: super::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let pv = self.pvs.lock().get(checked.pv_name()).cloned();
        async move {
            if !checked.allows_write() {
                return Err(format!(
                    "PUT denied by access security: '{}' from {}/{}/{}",
                    checked.pv_name(),
                    ctx.host,
                    ctx.account,
                    ctx.method,
                ));
            }
            match pv {
                Some(p) => p.put_delta(&desc, &changed, delta),
                None => Err(format!("no such PV: {}", checked.pv_name())),
            }
        }
    }

    fn is_writable(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        let exists = self.pvs.lock().contains_key(name);
        async move { exists }
    }

    fn subscribe(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<mpsc::Receiver<PvField>>> + Send {
        let pv = self.pvs.lock().get(name).cloned();
        async move {
            // pvxs servermon.cpp:66: default queue limit = 4.
            let inbox = pv.and_then(|p| p.subscribe(4))?;
            // Bridge MonitorInbox → mpsc::Receiver so the ChannelSource trait
            // signature stays stable; squash-to-tail semantics live in inbox.
            let (tx, rx) = mpsc::channel::<PvField>(1);
            tokio::spawn(async move {
                let mut inbox = inbox;
                while let Some(v) = inbox.recv().await {
                    if tx.send(v).await.is_err() {
                        break;
                    }
                }
            });
            Some(rx)
        }
    }

    fn rpc(
        &self,
        name: &str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> impl std::future::Future<Output = Result<(FieldDesc, PvField), String>> + Send {
        let pv = self.pvs.lock().get(name).cloned();
        let name = name.to_string();
        async move {
            match pv {
                // Routes through rpc_async so a SharedPV with an
                // `on_rpc_async` handler (typical of services
                // registered by `#[pva_service]`) runs the user's
                // future on this task's runtime — no block_on or
                // block_in_place needed.
                Some(p) => p.rpc_async(request_desc, request_value).await,
                None => Err(format!("no such PV: {name}")),
            }
        }
    }

    fn process(&self, name: &str) -> impl std::future::Future<Output = Result<(), String>> + Send {
        let pv = self.pvs.lock().get(name).cloned();
        let name = name.to_string();
        async move {
            match pv {
                Some(p) => p.process(),
                None => Err(format!("no such PV: {name}")),
            }
        }
    }

    /// Override default no-op: fire the per-PV `on_high_mark` /
    /// `on_low_mark` callback so the producer can throttle. Mirrors pvxs
    /// `MonitorControlOp` pipeline-pause semantics.
    fn notify_watermark(
        &self,
        name: &str,
        _ctx: &super::source::ChannelContext,
        ev: super::source::WatermarkEvent,
    ) {
        use super::source::WatermarkKind;
        // A single-owner SharedPV serves all subscribers from one outbox,
        // so credential scoping (`_ctx`), the per-op identity (`ev.op_id`)
        // and ordering token (`ev.seq`) carry no extra meaning here — they
        // matter only to the gateway, which fans per-credential upstreams
        // into separate caches and reference-counts pause votes. There is
        // likewise no shared-upstream to strand, so `Withdraw` is a no-op.
        let cb = match ev.kind {
            WatermarkKind::Resume => self.pvs.lock().get(name).and_then(|p| {
                let (high, _low) = p.watermark_handlers();
                high.map(|cb| (cb, p.clone()))
            }),
            WatermarkKind::Pause => self.pvs.lock().get(name).and_then(|p| {
                let (_high, low) = p.watermark_handlers();
                low.map(|cb| (cb, p.clone()))
            }),
            WatermarkKind::Withdraw => None,
        };
        if let Some((cb, p)) = cb {
            cb(&p);
        }
    }

    /// PVA-FR-11: override default no-op to fire the per-PV `on_start`
    /// callback so a producer can start/stop work as clients begin and
    /// stop consuming. Mirrors pvxs `MonitorControlOp::onStart`. As with
    /// the watermark callbacks, credential scoping (`_ctx`) carries no
    /// extra meaning for a single-owner SharedPV — it matters only to the
    /// gateway, which fans per-credential upstreams into separate caches.
    /// The wire layer guarantees one call per Executing<->Idle edge.
    fn notify_monitor_start(&self, name: &str, _ctx: &super::source::ChannelContext, start: bool) {
        let cb = self
            .pvs
            .lock()
            .get(name)
            .and_then(|p| p.on_start_handler().map(|cb| (cb, p.clone())));
        if let Some((cb, p)) = cb {
            cb(&p, start);
        }
    }

    /// PVA-FR-4: expose the per-PV `(low, high)` watermark levels so the
    /// monitor loop fires the callbacks off the pipeline window rather
    /// than server-queue occupancy.
    fn monitor_watermarks(&self, name: &str) -> Option<(usize, usize)> {
        self.pvs.lock().get(name).map(|p| p.watermarks())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pvdata::{ScalarType, ScalarValue};

    fn nt_scalar_int_desc() -> FieldDesc {
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Int))],
        }
    }

    fn nt_scalar_int_value(v: i32) -> PvField {
        let mut s = crate::pvdata::PvStructure::new("epics:nt/NTScalar:1.0");
        s.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Int(v))));
        PvField::Structure(s)
    }

    #[test]
    fn shared_pv_open_then_current() {
        let pv = SharedPV::new();
        assert!(!pv.is_open());
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(42));
        assert!(pv.is_open());
        assert!(pv.current().is_some());
    }

    #[tokio::test]
    async fn shared_pv_subscribe_sees_initial_then_updates() {
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0));
        let mut rx = pv.subscribe(8).expect("subscribe");
        // Initial value delivered immediately.
        let first = rx.recv().await.expect("first");
        assert!(matches!(first, PvField::Structure(_)));
        // Post an update.
        pv.try_post(nt_scalar_int_value(7));
        let second = rx.recv().await.expect("second");
        if let PvField::Structure(s) = second {
            assert_eq!(
                s.get_field("value"),
                Some(&PvField::Scalar(ScalarValue::Int(7)))
            );
        }
    }

    #[test]
    fn shared_pv_close_drops_subscribers() {
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0));
        let _rx = pv.subscribe(8);
        pv.close();
        assert!(!pv.is_open());
        assert_eq!(pv.try_post(nt_scalar_int_value(1)), 0);
    }

    fn extract_int(v: &PvField) -> i32 {
        match v {
            PvField::Structure(s) => match s.get_field("value") {
                Some(PvField::Scalar(ScalarValue::Int(n))) => *n,
                other => panic!("unexpected field: {other:?}"),
            },
            other => panic!("not a structure: {other:?}"),
        }
    }

    /// PVA-R6 regression: when a subscriber's queue is full, a normal post
    /// must replace the queue TAIL (squash-to-tail, pvxs servermon.cpp:283-286),
    /// NOT drop the new value.
    ///
    /// Setup: subscribe with limit=2, drain initial. Post 4 updates without
    /// consuming. Expected queue contents (squash-to-tail):
    ///   post(1) → [1]
    ///   post(2) → [1, 2]  (full)
    ///   post(3) → [1, 3]  (tail 2 → 3)
    ///   post(4) → [1, 4]  (tail 3 → 4)
    /// Consumer sees 1, 4 — NOT 1, 2 (the drop-newest behaviour before the fix).
    ///
    /// Before fix: try_send returned TrySendError::Full for posts 3 and 4, so
    /// consumer saw [1, 2] and the assertion v2 == 4 failed.
    #[tokio::test]
    async fn pva_r6_squash_to_tail() {
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0));
        let mut rx = pv.subscribe(2).expect("subscribe"); // queue limit = 2

        // Drain the initial value so the queue is empty.
        let _ = rx.recv().await.expect("initial value");

        // Post 4 updates while not consuming (simulates slow subscriber).
        pv.try_post(nt_scalar_int_value(1)); // queue: [1]
        pv.try_post(nt_scalar_int_value(2)); // queue: [1, 2] — full
        pv.try_post(nt_scalar_int_value(3)); // squash: [1, 3]
        pv.try_post(nt_scalar_int_value(4)); // squash: [1, 4]

        let v1 = rx.recv().await.expect("update 1");
        let v2 = rx.recv().await.expect("update 2 (squashed tail)");

        assert_eq!(extract_int(&v1), 1, "first in-order item must be 1");
        assert_eq!(
            extract_int(&v2),
            4,
            "squash-to-tail: newest value (4) must survive"
        );

        // Queue is now empty — no more posts were made.
        let empty = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(empty.is_err(), "queue must be empty after squash drain");
    }

    #[test]
    fn shared_pv_watermarks_default_to_4_and_64() {
        let pv = SharedPV::new();
        assert_eq!(pv.watermarks(), (4, 64));
        pv.set_low_watermark(8);
        pv.set_high_watermark(128);
        assert_eq!(pv.watermarks(), (8, 128));
    }

    /// PVA-FR-4: the source exposes per-PV `(low, high)` watermark
    /// levels to the monitor loop (which fires the callbacks off the
    /// pipeline window), and `set_*_watermark` retunes them. An unknown
    /// PV / level-less source returns `None`.
    #[test]
    fn shared_source_exposes_per_pv_watermark_levels() {
        use super::super::source::ChannelSource;
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0));
        pv.set_low_watermark(2);
        pv.set_high_watermark(5);
        let src = SharedSource::new();
        src.add("wm:pv", pv);
        assert_eq!(src.monitor_watermarks("wm:pv"), Some((2, 5)));
        assert_eq!(src.monitor_watermarks("nope"), None);
    }

    /// MR-R2 regression: a no-handler `put_delta` to a `SharedPV` with
    /// a live subscriber clones `g.subscribers` for a lock-free post.
    /// Before the fix, the cloned `MonitorOutbox` vector dropped at
    /// function exit and each clone's `Drop` set `producer_done = true`,
    /// terminating the subscriber inbox even though the receiver was
    /// never dropped. After the fix, only the *last* producer endpoint
    /// drop closes the queue, so the subscriber survives `put_delta`
    /// and keeps receiving posts.
    #[tokio::test]
    async fn mr_r2_put_delta_clone_drop_keeps_subscriber_alive() {
        use crate::proto::BitSet;

        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0));
        let mut rx = pv.subscribe(8).expect("subscribe");
        // Drain the initial value.
        let _ = rx.recv().await.expect("initial value");

        // Delta PUT marking the `value` leaf (field index 1: 0 is the
        // structure root, 1 is the first leaf).
        let desc = nt_scalar_int_desc();
        let mut changed = BitSet::new();
        changed.set(1);
        pv.put_delta(&desc, &changed, nt_scalar_int_value(11))
            .expect("first put_delta");

        // After put_delta, the temporary subscriber clone has dropped.
        // The subscriber inbox MUST still be open and deliver the value.
        let v1 = rx.recv().await;
        assert!(
            v1.is_some(),
            "subscriber inbox must survive put_delta's clone drop"
        );
        assert_eq!(extract_int(&v1.unwrap()), 11);

        // A second put_delta must also be delivered — the queue is not
        // producer-done.
        let mut changed2 = BitSet::new();
        changed2.set(1);
        pv.put_delta(&desc, &changed2, nt_scalar_int_value(22))
            .expect("second put_delta");
        let v2 = rx.recv().await;
        assert!(
            v2.is_some(),
            "subscriber inbox must keep receiving after repeated put_delta"
        );
        assert_eq!(extract_int(&v2.unwrap()), 22);
    }

    /// MR-R2: explicit `close()` must still terminate the subscriber
    /// inbox — closure is an owner action, and the invariant only
    /// forbids *internal clone drops* from closing the queue.
    #[tokio::test]
    async fn mr_r2_close_still_terminates_subscriber() {
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0));
        let mut rx = pv.subscribe(8).expect("subscribe");
        let _ = rx.recv().await.expect("initial value");
        pv.close();
        // close() drops the canonical outbox (last producer endpoint),
        // so the inbox drains to `None`.
        let after = rx.recv().await;
        assert!(
            after.is_none(),
            "close() must terminate the subscriber inbox"
        );
    }

    #[tokio::test]
    async fn shared_source_serves_named_pv() {
        use super::super::source::ChannelSource;
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(123));
        let src = SharedSource::new();
        src.add("test:pv", pv);

        assert!(src.has_pv("test:pv").await);
        let val = src.get_value("test:pv").await.expect("value");
        if let PvField::Structure(s) = val {
            assert_eq!(
                s.get_field("value"),
                Some(&PvField::Scalar(ScalarValue::Int(123)))
            );
        }
    }
}
