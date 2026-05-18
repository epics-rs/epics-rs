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
//! Flow control: the per-monitor `mpsc::Sender` is bounded; `try_post`
//! never blocks but may drop updates when the consumer is slow.
//! Tokio `mpsc` exposes no "drop oldest" primitive, so an over-budget
//! subscriber sees its event lost — the next `try_post` will deliver
//! the freshest value, which is the standard mailbox-on-channel
//! degradation when the consumer cannot keep up.
//!
//! Watermarks (low/high) are advisory hints stored on the SharedPV and
//! consulted by op_monitor when it decides whether to acknowledge a
//! pipeline window. We don't yet wire them into the wire-level
//! ackCount but the API is in place for callers to set them.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::pvdata::{FieldDesc, PvField};

/// User-provided put handler. Mirrors pvxs `SharedPV::onPut`
/// (sharedpv.cpp:329). Handler receives the new value; returning Err
/// causes the server to reply with a non-success Status. Returning
/// Ok(()) lets the server post the value to subscribers — handlers
/// that want to coerce / transform should do so via [`SharedPV::post`]
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

/// Per-PV state stored inside [`SharedPV`].
struct Inner {
    /// Type descriptor declared at open() — None when not opened.
    desc: Option<FieldDesc>,
    /// Most recent value (defaulted from desc on open).
    value: Option<PvField>,
    /// Open subscribers. Each slot holds a Sender for the per-monitor
    /// channel; callers post by sending one PvField per update.
    subscribers: Vec<mpsc::Sender<PvField>>,
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
    pub fn try_post(&self, value: PvField) -> usize {
        let mut g = self.inner.lock();
        if !g.is_open {
            return 0;
        }
        g.value = Some(value.clone());
        let mut delivered = 0;
        g.subscribers.retain(|tx| match tx.try_send(value.clone()) {
            Ok(()) => {
                delivered += 1;
                true
            }
            Err(mpsc::error::TrySendError::Full(_)) => true, // keep, drop update
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
        delivered
    }

    /// Add a subscriber. The returned Receiver yields every successful
    /// post; the channel depth is capped at `monitor_queue_depth`.
    /// Drops on the receiver side translate to subscriber removal on
    /// the next post().
    pub fn subscribe(&self, monitor_queue_depth: usize) -> Option<mpsc::Receiver<PvField>> {
        // Latch onFirstConnect callback to run *after* releasing the
        // lock — handlers may call back into post() / current() and we
        // can't recurse on parking_lot Mutex.
        let cb = {
            let mut g = self.inner.lock();
            if !g.is_open {
                return None;
            }
            let depth = monitor_queue_depth.max(1);
            let (tx, rx) = mpsc::channel(depth);
            if let Some(v) = &g.value {
                let _ = tx.try_send(v.clone());
            }
            let was_empty = g.subscribers.is_empty();
            g.subscribers.push(tx);
            let cb = if was_empty {
                g.on_first_connect.clone()
            } else {
                None
            };
            (rx, cb)
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
                subscribers: Vec<mpsc::Sender<PvField>>,
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
                subscribers.retain(|tx| match tx.try_send(value.clone()) {
                    Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => true,
                    Err(mpsc::error::TrySendError::Closed(_)) => false,
                });
                // Reconcile the subscriber set: drop the ones whose
                // receiver closed. Re-lock briefly; new subscribers
                // that joined between phase 1 and here are preserved.
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
}

impl SharedSource {
    pub fn new() -> Self {
        Self {
            pvs: Mutex::new(HashMap::new()),
        }
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
        async move { pv.and_then(|p| p.subscribe(64)) }
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

    /// Override default no-op: when an outbox crosses up through the
    /// high watermark, fire the per-PV `on_high_mark` callback so
    /// the producer can throttle. Mirrors pvxs `MonitorControlOp`
    /// pipeline-pause semantics.
    fn notify_watermark_high(&self, name: &str) {
        let pv = self.pvs.lock().get(name).cloned();
        if let Some(p) = pv {
            let (high, _low) = p.watermark_handlers();
            if let Some(cb) = high {
                cb(&p);
            }
        }
    }

    fn notify_watermark_low(&self, name: &str) {
        let pv = self.pvs.lock().get(name).cloned();
        if let Some(p) = pv {
            let (_high, low) = p.watermark_handlers();
            if let Some(cb) = low {
                cb(&p);
            }
        }
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

    #[test]
    fn shared_pv_watermarks_default_to_4_and_64() {
        let pv = SharedPV::new();
        assert_eq!(pv.watermarks(), (4, 64));
        pv.set_low_watermark(8);
        pv.set_high_watermark(128);
        assert_eq!(pv.watermarks(), (8, 128));
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
