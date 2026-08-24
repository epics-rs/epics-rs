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
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use parking_lot::Mutex;
use tokio::sync::mpsc::error::TryRecvError;

use super::source::MonitorStream;
use crate::pvdata::{FieldDesc, PvField, RpcReply};
use crate::server_native::source::{ChannelInvalidator, OpError};

/// User-provided put handler. Mirrors pvxs `SharedPV::onPut`
/// (sharedpv.cpp:329). Handler receives the new value; returning Err
/// causes the server to reply with a non-success Status. Returning
/// Ok(()) lets the server post the value to subscribers — handlers
/// that want to coerce / transform should do so via [`SharedPV::try_post`]
/// inside the closure and return Ok.
pub type OnPutFn = Arc<dyn Fn(&SharedPV, PvField) -> Result<(), String> + Send + Sync>;

/// Per-PV PUT policy. Replaces the previous `Option<OnPutFn>` so the
/// three pvxs behaviours each have one unambiguous meaning instead of
/// being inferred from "handler present?" plus a side flag:
///
/// - [`PutPolicy::Reject`] — a plain `SharedPV` with no handler. A
///   client PUT fails (pvxs `sharedpv.cpp:209-227` — no `onPut`).
/// - [`PutPolicy::Mailbox`] — built-in writable store-and-post. The
///   delta merge AND the store happen under one lock, preserving the
///   atomic read-merge-write invariant for concurrent disjoint PUTs.
/// - [`PutPolicy::Readonly`] — explicit refusal with the pvxs
///   `"Read-only PV"` message (`sharedpv.cpp:135-144`).
/// - [`PutPolicy::Custom`] — a user `onPut`. The merged value is handed
///   to the handler outside the lock (handlers may re-enter the PV).
#[derive(Clone)]
enum PutPolicy {
    Reject,
    Mailbox,
    Readonly,
    Custom(OnPutFn),
}

impl PutPolicy {
    /// Whether this policy *accepts* client writes.
    fn is_writable(&self) -> bool {
        matches!(self, PutPolicy::Mailbox | PutPolicy::Custom(_))
    }
}

/// User-provided process handler. Fired by the PVA `PROCESS` wire
/// command (cmd 16) — processing is triggered with no value payload,
/// the wire equivalent of an EPICS `dbProcess` / `caput .PROC`.
/// Returning `Err` makes the server reply with a non-success PROCESS
/// status. When no handler is installed `process()` is a no-op
/// success, mirroring a passive record's response to `.PROC`.
pub type OnProcessFn = Arc<dyn Fn(&SharedPV) -> Result<(), String> + Send + Sync>;

/// User-provided RPC handler. Mirrors pvxs `SharedPV::onRPC`. Handler
/// receives `(request_desc, request_value)` and returns the reply or an
/// error message. The reply is an [`RpcReply`], covering both pvxs
/// `ExecOp::reply()` overloads: a `(FieldDesc, PvField)` pair converts
/// into `RpcReply::Value`, and `RpcReply::Empty` is the no-value reply.
pub type OnRpcFn =
    Arc<dyn Fn(&SharedPV, FieldDesc, PvField) -> Result<RpcReply, String> + Send + Sync>;

/// Async RPC handler. Returns a boxed future the dispatch path
/// awaits, so the user's async work runs on the calling task's
/// runtime without `block_in_place`/`block_on`. Used by the
/// `#[pva_service]` framework.
pub type OnRpcAsyncFn = Arc<
    dyn Fn(
            SharedPV,
            FieldDesc,
            PvField,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<RpcReply, String>> + Send>>
        + Send
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

/// monitor start/stop callback. Fired with `true` when a
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

struct MonitorQueueInner<T> {
    items: VecDeque<T>,
    limit: usize,
    /// True once the producer side (SharedPV) signals no more data.
    producer_done: bool,
}

struct MonitorQueueShared<T> {
    inner: Mutex<MonitorQueueInner<T>>,
    notify: tokio::sync::Notify,
    /// Set in MonitorInbox::drop; post() checks this to decide whether to remove
    /// the outbox from the subscriber list.
    receiver_dropped: AtomicBool,
    /// Live `MonitorOutbox` endpoints for this queue. closure
    /// (`producer_done`) must be tied to the *last* producer endpoint
    /// disappearing, not to any single cloned endpoint dropping. A
    /// temporary clone made for a lock-free post (e.g. `put_delta`'s
    /// `g.subscribers.clone()`) keeps this count above 1 while the
    /// canonical outbox lives, so its drop never closes the inbox.
    producer_count: AtomicUsize,
}

/// Sender half of a per-subscriber queue. Held by `SharedPV::subscribers`.
///
/// `Clone` is implemented by hand (not derived) so each clone
/// increments `producer_count`. The invariant — "a monitor queue
/// becomes `producer_done` only when its *last* `MonitorOutbox`
/// endpoint drops" — is enforced structurally here and in `Drop`,
/// so a transient clone used for lock-free delivery cannot close the
/// subscriber's inbox.
pub struct MonitorOutbox<T = PvField> {
    shared: Arc<MonitorQueueShared<T>>,
}

/// Receiver half of a per-subscriber queue — the **monitor ring**.
///
/// Generic in the element type so the one primitive serves every monitor
/// stream shape the `ChannelSource` trait carries (`PvField`,
/// `MonitorUpdate`, `RawMonitorEvent`), instead of each source bridging its
/// ring into an `mpsc` just to satisfy the trait's return type. Nothing in
/// the queue was ever `PvField`-specific — append/squash-to-tail and the
/// producer-closed flag are element-agnostic.
///
/// Returned by `SharedPV::subscribe` (as the [`MonitorInbox`] alias) and
/// carried by [`MonitorStream::Ring`](super::source::MonitorStream).
pub struct MonitorRing<T = PvField> {
    shared: Arc<MonitorQueueShared<T>>,
}

/// The `PvField` ring — the shape `SharedPV` hands out. Kept as a named
/// alias because that is the only element type a `SharedPV` ever queues.
pub type MonitorInbox = MonitorRing<PvField>;

fn make_monitor_queue<T>(limit: usize) -> (MonitorOutbox<T>, MonitorRing<T>) {
    let limit = limit.max(1);
    let shared = Arc::new(MonitorQueueShared {
        inner: Mutex::new(MonitorQueueInner {
            // Grow lazily. `limit` is the client-supplied queueSize (unbounded
            // u32) and must never be pre-allocated — a single MONITOR INIT with
            // an enormous queueSize would otherwise force a multi-GB reservation
            // and abort the process. `post` bounds live length to `limit` via
            // tail-squash, so lazy growth preserves the same semantics; pvxs
            // likewise never pre-sizes its monitor deque.
            items: VecDeque::new(),
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
        MonitorRing { shared },
    )
}

impl<T> MonitorOutbox<T> {
    /// Post a value. `maybe=false`: full queue → squash tail (pvxs servermon.cpp:283-286).
    /// `maybe=true`: full queue → drop silently.
    /// Returns `false` when the receiver has been dropped (caller should remove this outbox).
    fn post(&self, value: T, maybe: bool) -> bool {
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

impl<T> Clone for MonitorOutbox<T> {
    fn clone(&self) -> Self {
        // every live endpoint counts. A clone made for a
        // lock-free post is a producer endpoint until it drops.
        self.shared.producer_count.fetch_add(1, Ordering::AcqRel);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Drop for MonitorOutbox<T> {
    fn drop(&mut self) {
        // signal `producer_done` only when the *last* producer
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

impl<T> Drop for MonitorRing<T> {
    fn drop(&mut self) {
        self.shared.receiver_dropped.store(true, Ordering::Relaxed);
    }
}

impl<T> MonitorRing<T> {
    /// Async receive. Returns `None` when the producer closed and the queue is drained.
    pub async fn recv(&mut self) -> Option<T> {
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

    /// Non-blocking [`Self::recv`] — the port of
    /// [`EvQue::try_next`](epics_base_rs::server::event_queue) (`event_queue.rs:371`),
    /// which is what `EventReader::try_recv` exposes on the CA side.
    ///
    /// Exists so a **blocking** per-connection drain loop can poll this ring
    /// without entering async at all: the RTEMS backend has no reactor, so the
    /// operation thread takes what is queued with this and only parks (via
    /// `block_on_sync` → `park_on`) when everything is drained. The ring itself
    /// is already runtime-agnostic — `Mutex<VecDeque>` plus a `tokio::sync::Notify`
    /// that stores no reactor state — so this method is the last piece a
    /// non-async consumer needed.
    ///
    /// **Drain-before-close, same as [`Self::recv`].** Items are inspected before
    /// `producer_done`, so a closed producer whose queue still holds values keeps
    /// yielding `Ok` and only reports `Disconnected` once empty. Reversing the two
    /// would silently swallow the tail of a monitor whose PV just closed.
    ///
    /// Unlike [`Self::recv`] this does **not** arm the `Notify` first. Arming
    /// exists to close the check/await race — register the waiter before
    /// inspecting so a `notify_one()` landing in between is not lost. There is no
    /// await here, so there is no race to close and no waiter to register;
    /// `EvQue::try_next` likewise takes the lock and nothing else. The ordering
    /// discipline that *does* carry over is the one above: one lock acquisition,
    /// items examined before the closed flag.
    ///
    /// The error type is `tokio::sync::mpsc::error::TryRecvError` rather than a
    /// private mirror so that a consumer polling this ring and a consumer polling
    /// an mpsc-backed monitor stream read identically — the server's existing
    /// drain idiom (`tcp.rs:2245`, `while let Ok(e) = rx.try_recv()`) works over
    /// either without a conversion at the seam.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let mut inner = self.shared.inner.lock();
        if let Some(v) = inner.items.pop_front() {
            return Ok(v);
        }
        if inner.producer_done {
            return Err(TryRecvError::Disconnected);
        }
        Err(TryRecvError::Empty)
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Open/closed lifecycle of a SharedPV's published type + value.
///
/// pvxs keys the entire open-state on a single member, `impl->current`
/// (`sharedpv.cpp:391` `isOpen() = !!impl->current`): `open()` sets it,
/// `close()` clears it (`:404-405`), and `fetch()` throws `"open() first"`
/// whenever it is empty (`:443-469`). Modelling the descriptor and value
/// only inside the `Open` variant reproduces that single source of truth:
/// "closed ⟹ nothing readable" holds by construction, because there is no
/// desc/value field to read while `Closed`. This is what stops a closed PV
/// from surfacing a stale descriptor/value through `current()` /
/// `introspection()` (and hence GET / GET_FIELD), the way three independent
/// `desc`/`value`/`is_open` fields allowed when `close()` only flipped the
/// flag.
enum PvState {
    /// Not opened (or closed): no descriptor or value exists.
    Closed,
    /// Opened: both the declared type and the most recent value are present.
    Open { desc: FieldDesc, value: PvField },
}

/// Per-PV state stored inside [`SharedPV`].
struct Inner {
    /// Published type + value, or `Closed`. Single source of truth for
    /// open-state (pvxs `impl->current`).
    state: PvState,
    /// Open subscribers. Each slot holds a MonitorOutbox for squash-to-tail delivery.
    subscribers: Vec<MonitorOutbox>,
    /// Optional flow-control watermark: monitor stream sends MORE
    /// only when its outbox depth crosses below `low_watermark`.
    /// Currently advisory; preserved here for op_monitor to consult.
    pub low_watermark: usize,
    /// Pause sending updates when the monitor outbox depth is at or
    /// above `high_watermark`. Currently advisory.
    pub high_watermark: usize,
    /// PUT policy: reject / mailbox / read-only / custom handler. The
    /// single source of truth for both write dispatch and `is_writable`
    /// (pvxs `onPut` parity — see [`PutPolicy`]).
    put_policy: PutPolicy,
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
    /// Count of currently-attached client channels (pvxs
    /// `impl->channels.size()`). Driven by [`SharedPV::attach_channel`] /
    /// [`SharedPV::detach_channel`] from the server's CREATE_CHANNEL /
    /// channel-teardown owner — NOT by monitor subscriptions. The
    /// first/last-channel hooks key off its 0↔1 transitions.
    channel_count: usize,
    /// First-channel-attached hook (pvxs `onFirstConnect`). Fires on the
    /// channel-count 0→1 transition, independent of monitor subscribers.
    on_first_connect: Option<LifecycleFn>,
    /// Last-channel-detached hook (pvxs `onLastDisconnect`). Fires on the
    /// channel-count 1→0 transition, independent of monitor subscribers.
    on_last_disconnect: Option<LifecycleFn>,
    /// Outbox crossed `high_watermark` going up. Producer throttle
    /// hint. See [`WatermarkFn`].
    on_high_mark: Option<WatermarkFn>,
    /// Outbox drained back to zero (or below `low_watermark`).
    /// Producer un-throttle hint.
    on_low_mark: Option<WatermarkFn>,
    /// monitor start/stop hook (pvxs `onStart`). See
    /// [`OnStartFn`].
    on_start: Option<OnStartFn>,
    /// Server channel-invalidators bound by the [`SharedSource`] this PV
    /// is registered with, paired with the name it is registered under.
    /// [`SharedPV::close`] publishes each name through its invalidator so the
    /// per-connection read loop force-disconnects every attached server
    /// channel for this PV — the second half of pvxs `SharedPV::close()`
    /// (`sharedpv.cpp:411-414` closes every attached channel, not just the
    /// value/type). Empty for a standalone (never-registered) PV.
    invalidators: Vec<(ChannelInvalidator, String)>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            state: PvState::Closed,
            subscribers: Vec::new(),
            low_watermark: 4,
            high_watermark: 64,
            put_policy: PutPolicy::Reject,
            on_rpc: None,
            on_process: None,
            on_rpc_async: None,
            channel_count: 0,
            on_first_connect: None,
            on_last_disconnect: None,
            on_high_mark: None,
            on_low_mark: None,
            on_start: None,
            invalidators: Vec::new(),
        }
    }
}

/// Server-side handle for a single PV's value + subscriber set.
///
/// Cheap to clone: it's just an `Arc<Mutex<...>>`.
#[derive(Clone)]
pub struct SharedPV {
    inner: Arc<Mutex<Inner>>,
    /// Woken by [`Self::open`], awaited by [`Self::wait_open`]. This is
    /// the port's `pending`/`mpending`: pvxs holds the parked ops in the
    /// PV and walks them in `open()` (`sharedpv.cpp:239-249`, `:259-275`,
    /// `:348-384`); here the parked op is a task suspended on this
    /// notify, so `open()` releases them all with one `notify_waiters`
    /// and the server never has to own a second copy of the op set.
    opened: Arc<tokio::sync::Notify>,
}

impl SharedPV {
    /// New, unopened SharedPV. open() must be called before serving GETs.
    ///
    /// A plain `SharedPV` has NO PUT handler, so it is NOT writable —
    /// a client PUT is rejected (pvxs `sharedpv.cpp:209-227`). Build a
    /// writable PV with [`Self::build_mailbox`], a read-only one that
    /// explicitly refuses writes with [`Self::build_readonly`], or
    /// install a custom [`Self::on_put`] handler.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            opened: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// A writable "mailbox" PV: a client PUT stores the value and posts
    /// it to all subscribers. Mirrors pvxs `SharedPV::buildMailbox`
    /// (`sharedpv.cpp:106-132`) — it installs an `onPut` handler that
    /// `post`s the value, additionally stamping `timeStamp` with the
    /// server's current time when the client's PUT marked neither the
    /// timeStamp field nor any of its children (`sharedpv.cpp:113-121`).
    /// A client-supplied (marked) timeStamp is preserved. See
    /// `Self::fill_mailbox_timestamp`.
    pub fn build_mailbox() -> Self {
        let pv = Self::new();
        pv.inner.lock().put_policy = PutPolicy::Mailbox;
        pv
    }

    /// A read-only PV that explicitly refuses writes. Mirrors pvxs
    /// `SharedPV::buildReadonly` (`sharedpv.cpp:135-144`), whose `onPut`
    /// handler replies `"Read-only PV"`. Reports `is_writable() == false`.
    pub fn build_readonly() -> Self {
        let pv = Self::new();
        pv.inner.lock().put_policy = PutPolicy::Readonly;
        pv
    }

    /// Whether this PV *accepts* client writes. `true` only for a
    /// mailbox ([`Self::build_mailbox`]) or a custom handler
    /// ([`Self::on_put`]); `false` for a plain [`Self::new`] (PUT
    /// rejected) and for [`Self::build_readonly`] (writes refused).
    /// pvxs has no implicit-writable `SharedPV`.
    pub fn is_writable(&self) -> bool {
        self.inner.lock().put_policy.is_writable()
    }

    /// Declare the type and seed the initial value, transitioning the
    /// PV from `PvState::Closed` to `PvState::Open`.
    ///
    /// Returns `Err` if the PV is already open. pvxs `sharedpv.cpp:357-358`
    /// throws `"close() first"` when `impl->current` is already set, so a
    /// republish of the type/value requires an explicit [`Self::close`]
    /// first. The rejection keys off the `PvState::Open` discriminant that
    /// stores the descriptor and value — not a side `is_open` flag — so
    /// "open ⟺ a descriptor+value exist" holds by construction: a second
    /// `open()` cannot swap the descriptor out from under monitor
    /// operations that captured the old introspection at subscribe time.
    pub fn open(&self, desc: FieldDesc, initial: PvField) -> crate::error::PvaResult<()> {
        let mut g = self.inner.lock();
        if matches!(g.state, PvState::Open { .. }) {
            return Err(crate::error::PvaError::Protocol(
                "SharedPV already open; close() first".to_string(),
            ));
        }
        // The seeded value must fit its descriptor — the SAME guard
        // `try_post_checked`/`put_delta` enforce on every later write.
        // Without it the first value had weaker invariants than all
        // subsequent ones, so a startup-only descriptor/value mismatch
        // could be encoded under the advertised descriptor on the very
        // first GET / monitor snapshot.
        if let Err(e) = crate::pvdata::value_matches_descriptor(&initial, &desc) {
            return Err(crate::error::PvaError::InvalidValue(format!(
                "SharedPV::open: initial value does not fit descriptor ({e})"
            )));
        }
        g.state = PvState::Open {
            desc,
            value: initial,
        };
        drop(g);
        // Release every op parked on the closed PV. pvxs moves `pending`
        // and `mpending` out here and runs `connectOp`/`connectSub` on
        // each (`sharedpv.cpp:355-384`); the parked op is a suspended
        // task in this port, so waking them is the same act.
        self.opened.notify_waiters();
        Ok(())
    }

    /// Resolve to this PV's descriptor, waiting for [`Self::open`] if it
    /// has not been called yet.
    ///
    /// The awaiting side of pvxs's parked-op sets: `SharedPV::onOp`
    /// inserts a `ConnectOp` into `pending` when `!self->current`
    /// (`sharedpv.cpp:239-249`) and `onSubscribe` puts a `MonitorSetupOp`
    /// into `mpending` (`:259-275`) rather than answering an error, and
    /// `SharedPV::open` completes them all (`:348-384`). Cancelling this
    /// future is the whole of pvxs's `conn->onClose` erasing the op from
    /// the set — nothing else has to be undone.
    pub async fn wait_open(&self) -> FieldDesc {
        loop {
            // Register for the wake BEFORE re-reading the state, so an
            // `open()` landing between the two is not missed.
            let waiting = self.opened.notified();
            if let Some(desc) = self.introspection() {
                return desc;
            }
            waiting.await;
        }
    }

    /// Returns true iff the PV has been opened.
    pub fn is_open(&self) -> bool {
        matches!(self.inner.lock().state, PvState::Open { .. })
    }

    /// Bind a [`SharedSource`]'s server channel-invalidator and the name
    /// this PV is registered under, so a later [`Self::close`] can
    /// force-disconnect every attached server channel. Called by
    /// [`SharedSource`] at registration time (and when the server installs
    /// the invalidator). Multiple bindings accumulate: one PV registered
    /// under several names / sources closes them all.
    pub(crate) fn bind_invalidator(&self, invalidator: ChannelInvalidator, name: String) {
        self.inner.lock().invalidators.push((invalidator, name));
    }

    /// Close the PV: clear its descriptor and value, drop all subscribers,
    /// AND force-disconnect every attached server channel. After close,
    /// `current()` / `introspection()` (and thus GET / GET_FIELD) return
    /// `None` until `open()` is called again — pvxs `sharedpv.cpp:404-405`
    /// clears `impl->current`, which is what makes `fetch()` throw
    /// `"open() first"` afterwards (`:443-469`).
    ///
    /// pvxs `sharedpv.cpp:407-414` ALSO moves out the attached-channel set
    /// and calls `chan->close()` on each, sending a server-initiated
    /// `DESTROY_CHANNEL`. We mirror that by publishing the registered name(s)
    /// through the bound [`ChannelInvalidator`]: every per-connection read
    /// loop tears down the channels it serves under that name through the
    /// single teardown owner (`finalize_channel_destroy` →
    /// `notify_channel_close` once + report close + `DESTROY_CHANNEL`). This
    /// closes the close/reopen-with-new-descriptor hazard where a live
    /// pre-close channel kept negotiating ops against the stale descriptor it
    /// captured at CREATE_CHANNEL while the source served the reopened value.
    pub fn close(&self) {
        // Snapshot the bindings under the lock, then publish after releasing
        // it — `publish` only enqueues to per-connection mpsc queues, but
        // keeping it off the PV lock avoids re-entrancy hazards.
        let invalidations: Vec<(ChannelInvalidator, String)> = {
            let mut g = self.inner.lock();
            g.state = PvState::Closed;
            g.subscribers.clear();
            g.invalidators.clone()
        };
        for (invalidator, name) in invalidations {
            invalidator.publish(std::sync::Arc::from(vec![name]));
        }
    }

    /// Type descriptor (None while closed).
    pub fn introspection(&self) -> Option<FieldDesc> {
        match &self.inner.lock().state {
            PvState::Open { desc, .. } => Some(desc.clone()),
            PvState::Closed => None,
        }
    }

    /// Current value (None while closed).
    pub fn current(&self) -> Option<PvField> {
        match &self.inner.lock().state {
            PvState::Open { value, .. } => Some(value.clone()),
            PvState::Closed => None,
        }
    }

    /// Push a new value to all subscribers; lossy semantics — drops
    /// updates when a subscriber's outbox is full. Returns the number
    /// of subscribers we successfully sent to.
    ///
    /// a descriptor/value mismatch is logged at warn level
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

    /// Result-typed post with descriptor enforcement.
    /// mirrors pvxs `sharedpv.cpp:417-431`. Returns `Err` when the
    /// PV is not yet opened, or when the value's runtime shape
    /// does not fit the opened descriptor. Subscribers see the new
    /// value only on `Ok`.
    pub fn try_post_checked(&self, value: PvField) -> crate::error::PvaResult<usize> {
        let mut g = self.inner.lock();
        let inner = &mut *g;
        let PvState::Open { desc, value: cur } = &mut inner.state else {
            return Err(crate::error::PvaError::Protocol(
                "SharedPV not open".to_string(),
            ));
        };
        if let Err(e) = crate::pvdata::value_matches_descriptor(&value, desc) {
            return Err(crate::error::PvaError::InvalidValue(format!(
                "SharedPV::try_post: value does not fit opened descriptor ({e})"
            )));
        }
        *cur = value.clone();
        // pvxs servermon.cpp:283-286: normal post squashes tail when queue full.
        inner.subscribers.retain(|tx| tx.post(value.clone(), false));
        Ok(inner.subscribers.len())
    }

    /// Add a subscriber. Returns a [`MonitorInbox`] that yields posted values
    /// with squash-to-tail semantics (pvxs `servermon.cpp:283-286`) when the
    /// `limit`-deep queue is full. Drops on the receiver side translate to
    /// outbox removal on the next post.
    ///
    /// `limit` is the maximum number of unread events; pvxs default is 4
    /// (`servermon.cpp:66`). Values ≥ 1 are accepted; 0 is clamped to 1.
    pub fn subscribe(&self, limit: usize) -> Option<MonitorInbox> {
        // A monitor subscription is NOT a channel-lifecycle edge: pvxs
        // tracks monitor `subscribers` separately from `impl->channels`
        // and does NOT run onFirstConnect/onLastDisconnect off subscriber
        // counts (sharedpv.cpp:252-275). The first/last hooks are driven
        // by `attach_channel`/`detach_channel` instead.
        let mut g = self.inner.lock();
        let PvState::Open { value, .. } = &g.state else {
            return None;
        };
        // Initial value: queue is empty so limit not yet reached.
        let initial = value.clone();
        let (outbox, inbox) = make_monitor_queue(limit);
        outbox.post(initial, false);
        g.subscribers.push(outbox);
        Some(inbox)
    }

    /// Add an **updates-only** subscriber and atomically capture the
    /// current value as the connect-time monitor seed — both under one
    /// `inner` lock, so no `post`/`put` can slip between the snapshot and
    /// registration. This is the single-seed counterpart of
    /// [`Self::subscribe`]: the returned `inbox` carries only
    /// post-registration events, and the caller emits `initial` itself
    /// (the server does this via [`crate::server_native::source::SubscriptionSeed`]).
    /// Returns `None` only when the PV is not open. `initial` is the
    /// current value (always `Some` here, since the PV is open).
    pub fn subscribe_seeded(&self, limit: usize) -> Option<(Option<PvField>, MonitorInbox)> {
        let mut g = self.inner.lock();
        let PvState::Open { value, .. } = &g.state else {
            return None;
        };
        let initial = value.clone();
        let (outbox, inbox) = make_monitor_queue(limit);
        g.subscribers.push(outbox);
        Some((Some(initial), inbox))
    }

    /// Apply a PUT. When [`Self::on_put`] has been set, the user
    /// handler runs and is responsible for any side-effects /
    /// re-posting. When NO handler is installed the PUT is REJECTED —
    /// pvxs `sharedpv.cpp:209-227` replies `op->error(...)` for a
    /// plain `SharedPV` with no `onPut` rather than silently storing
    /// the value. A writable PV is built with [`Self::build_mailbox`]
    /// (posting handler) or [`Self::build_readonly`] (rejecting
    /// handler), or by installing a custom [`Self::on_put`].
    pub fn put(&self, value: PvField) -> Result<(), String> {
        if !self.is_open() {
            return Err("SharedPV not open".into());
        }
        let policy = self.inner.lock().put_policy.clone();
        match policy {
            PutPolicy::Reject => Err("PUT not supported by this PV".into()),
            PutPolicy::Readonly => Err("Read-only PV".into()),
            // Mailbox: store + post (descriptor-enforced).
            PutPolicy::Mailbox => self
                .try_post_checked(value)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            PutPolicy::Custom(f) => f(self, value),
        }
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
    ///
    /// A PV with NO handler REJECTS the PUT (pvxs `sharedpv.cpp:209-227`
    /// — a plain `SharedPV` is not implicitly writable); the merged
    /// value is neither stored nor posted. Use [`Self::build_mailbox`]
    /// for a writable PV.
    pub fn put_delta(
        &self,
        desc: &FieldDesc,
        changed: &crate::proto::BitSet,
        delta: PvField,
    ) -> Result<(), String> {
        // Under the lock, dispatch on PUT policy. The merge
        // (`fill_unmarked_from_prior`, a pure function) is safe to call
        // while holding the parking_lot mutex.
        //
        // Reject/Readonly bail BEFORE merging so a non-writable PV's
        // stored value is never touched (pvxs `sharedpv.cpp:209-227`).
        //
        // MAILBOX merge + store + deliver all happen under THIS lock.
        // Storing under the lock gives the read-merge-write invariant
        // (two concurrent disjoint delta PUTs cannot both read the same
        // prior and clobber each other). Delivering under the SAME lock
        // closes the post-and-close race: `close()`
        // also takes `inner`, so a post and a close serialize exactly as
        // pvxs serializes `post()`/`close()` on `impl->lock`
        // (`sharedpv.cpp:394-407`) — there is no window in which a value
        // is delivered to a subscriber that `close()` already cleared.
        // (The prior design cloned `inner.subscribers` under the lock and
        // posted to the clone after releasing it; a `close()` landing in
        // that gap cleared the canonical set while the live clone still
        // delivered.) `MonitorOutbox::post` only locks its own per-queue
        // mutex (never `inner`), so holding `inner` across delivery cannot
        // deadlock. This mirrors `try_post_checked`'s in-lock delivery.
        // Only a CUSTOM handler is deferred OUTSIDE the lock (it may
        // re-enter the PV — post/close — so holding `inner` across it
        // would deadlock), exactly as `put` defers it.
        let deferred = {
            let mut g = self.inner.lock();
            let inner = &mut *g;
            let policy = inner.put_policy.clone();
            match policy {
                PutPolicy::Reject => return Err("PUT not supported by this PV".into()),
                PutPolicy::Readonly => return Err("Read-only PV".into()),
                PutPolicy::Mailbox => {
                    let PvState::Open {
                        desc: opened,
                        value: prior,
                    } = &mut inner.state
                    else {
                        return Err("SharedPV not open".into());
                    };
                    let mut merged = crate::pvdata::encode::fill_unmarked_from_prior(
                        desc, changed, 0, delta, prior,
                    );
                    // pvxs `buildMailbox` installs an `onPut` that stamps an
                    // unmarked `timeStamp` with the server's current time
                    // before posting (`sharedpv.cpp:113-121`). Do that here,
                    // in the mailbox owner, so the stored+posted value
                    // carries a fresh timestamp when the client PUT left it
                    // unset. A client-marked timeStamp is preserved.
                    Self::fill_mailbox_timestamp(desc, changed, &mut merged);
                    // descriptor enforcement for the store path: the
                    // merged value must fit the opened descriptor, not a
                    // stale peer-cached `desc` (pvxs `sharedpv.cpp:417-431`).
                    if let Err(e) = crate::pvdata::value_matches_descriptor(&merged, opened) {
                        return Err(format!(
                            "SharedPV::put_delta: merged value does not fit opened descriptor ({e})"
                        ));
                    }
                    // Store + deliver atomically under this lock — the
                    // close-race fix. pvxs servermon.cpp:283-286:
                    // squash-to-tail for a normal post; retain drops
                    // receivers that have closed.
                    *prior = merged.clone();
                    inner
                        .subscribers
                        .retain(|tx| tx.post(merged.clone(), false));
                    return Ok(());
                }
                PutPolicy::Custom(handler) => {
                    let PvState::Open { value: prior, .. } = &mut inner.state else {
                        return Err("SharedPV not open".into());
                    };
                    let merged = crate::pvdata::encode::fill_unmarked_from_prior(
                        desc, changed, 0, delta, prior,
                    );
                    (handler, merged)
                }
            }
        };
        // A CUSTOM handler runs OUTSIDE the lock (it may re-enter the PV);
        // it owns any re-post / close side-effects, exactly as for `put`.
        let (handler, value) = deferred;
        handler(self, value)
    }

    /// pvxs `SharedPV::buildMailbox` `onPut` timestamp-fill
    /// (`sharedpv.cpp:113-121`): when the merged value carries a
    /// top-level `timeStamp` structure that the client marked neither at
    /// its own bit nor at any descendant, stamp `secondsPastEpoch` /
    /// `nanoseconds` from the server's current POSIX time. A
    /// client-supplied (marked) timeStamp is left untouched.
    ///
    /// `desc` / `changed` use the wire bit numbering (pvData §5.4); the
    /// "marked" test covers `[ts_bit, ts_bit + total_bits)` — the field's
    /// own bit plus every descendant — matching pvxs `isMarked(true,
    /// true)`. Child leaves are rewritten in place, preserving their
    /// declared integer type so the merged value still fits the opened
    /// descriptor. Mailbox-only: a custom `on_put` handler owns its own
    /// timestamp policy, exactly as pvxs installs this fill only in
    /// `buildMailbox` (a full-value `post`/[`Self::put`] is not stamped).
    fn fill_mailbox_timestamp(
        desc: &FieldDesc,
        changed: &crate::proto::BitSet,
        value: &mut PvField,
    ) {
        use crate::pvdata::ScalarValue;
        // Locate the top-level `timeStamp` field and its bit offset
        // (root is bit 0; the first child is bit 1, then depth-first).
        let FieldDesc::Structure { fields, .. } = desc else {
            return;
        };
        let mut ts_bit = 1usize;
        let mut ts_desc = None;
        for (name, child) in fields {
            if name == "timeStamp" {
                ts_desc = Some(child);
                break;
            }
            ts_bit += child.total_bits();
        }
        // Only a `timeStamp` *structure* is stampable (pvxs writes the two
        // child leaves of `val["timeStamp"]`).
        let Some(ts_desc @ FieldDesc::Structure { .. }) = ts_desc else {
            return;
        };
        // Client marked the field or any descendant → keep its timestamp.
        let total = ts_desc.total_bits();
        if (0..total).any(|i| changed.get(ts_bit + i)) {
            return;
        }
        // Stamp the two child leaves from the server's current POSIX time.
        let PvField::Structure(root) = value else {
            return;
        };
        let Some(PvField::Structure(ts)) = root.get_field_mut("timeStamp") else {
            return;
        };
        let dur = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        // Overwrite preserving the declared integer width/signedness so
        // the value still matches the opened descriptor.
        fn stamp_int(slot: &mut ScalarValue, v: i64) {
            match slot {
                ScalarValue::Byte(x) => *x = v as i8,
                ScalarValue::Short(x) => *x = v as i16,
                ScalarValue::Int(x) => *x = v as i32,
                ScalarValue::Long(x) => *x = v,
                ScalarValue::UByte(x) => *x = v as u8,
                ScalarValue::UShort(x) => *x = v as u16,
                ScalarValue::UInt(x) => *x = v as u32,
                ScalarValue::ULong(x) => *x = v as u64,
                // Non-integer leaf: not a standard `time_t` field — leave.
                _ => {}
            }
        }
        if let Some(PvField::Scalar(s)) = ts.get_field_mut("secondsPastEpoch") {
            stamp_int(s, dur.as_secs() as i64);
        }
        if let Some(PvField::Scalar(s)) = ts.get_field_mut("nanoseconds") {
            stamp_int(s, dur.subsec_nanos() as i64);
        }
    }

    /// Dispatch an RPC request. With no [`Self::on_rpc`] handler installed,
    /// pvxs never sets `chan->onRPC`, and an RPC EXEC on that channel is
    /// answered with the fixed text [`super::source::RPC_NOT_IMPLEMENTED`]
    /// (serverget.cpp:482-486).
    pub fn rpc(&self, request_desc: FieldDesc, request_value: PvField) -> Result<RpcReply, String> {
        let on_rpc = self.inner.lock().on_rpc.clone();
        match on_rpc {
            Some(f) => f(self, request_desc, request_value),
            None => Err(super::source::RPC_NOT_IMPLEMENTED.into()),
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
    /// returns pvxs's "RPC Not Implemented" when neither is installed. The
    /// `#[pva_service]` framework uses this so user async methods
    /// run on the calling task's runtime, no `block_in_place`.
    pub async fn rpc_async(
        &self,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> Result<RpcReply, String> {
        let (sync, async_h) = {
            let g = self.inner.lock();
            (g.on_rpc.clone(), g.on_rpc_async.clone())
        };
        if let Some(f) = async_h {
            return f(self.clone(), request_desc, request_value).await;
        }
        match sync {
            Some(f) => f(self, request_desc, request_value),
            None => Err(super::source::RPC_NOT_IMPLEMENTED.into()),
        }
    }

    /// Install a put handler, making the PV writable. Mirrors pvxs
    /// `SharedPV::onPut`. Installing a handler marks the PV writable
    /// (`is_writable() == true`); for an explicitly read-only PV use
    /// [`Self::build_readonly`] instead, which installs a refusing
    /// handler while keeping `is_writable() == false`.
    pub fn on_put<F>(&self, handler: F)
    where
        F: Fn(&SharedPV, PvField) -> Result<(), String> + Send + Sync + 'static,
    {
        self.inner.lock().put_policy = PutPolicy::Custom(Arc::new(handler));
    }

    /// Install an RPC handler. Mirrors pvxs `SharedPV::onRPC`.
    ///
    /// The handler may return a `(FieldDesc, PvField)` pair (pvxs
    /// `ExecOp::reply(Value)`) or an [`RpcReply`] — returning
    /// [`RpcReply::Empty`] emits pvxs's no-value reply
    /// (`ExecOp::reply()`), a bare NULL type code with no body
    /// (serverget.cpp:104-112).
    pub fn on_rpc<F, R>(&self, handler: F)
    where
        F: Fn(&SharedPV, FieldDesc, PvField) -> Result<R, String> + Send + Sync + 'static,
        R: Into<RpcReply>,
    {
        self.inner.lock().on_rpc =
            Some(Arc::new(move |pv, d, v| handler(pv, d, v).map(Into::into)));
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
    pub fn on_rpc_async<F, Fut, R>(&self, handler: F)
    where
        F: Fn(SharedPV, FieldDesc, PvField) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<R, String>> + Send + 'static,
        R: Into<RpcReply>,
    {
        let arc: OnRpcAsyncFn = Arc::new(move |pv, d, v| {
            let fut = handler(pv, d, v);
            Box::pin(async move { fut.await.map(Into::into) })
        });
        self.inner.lock().on_rpc_async = Some(arc);
    }

    /// Hook fired when the *first client channel* attaches (channel
    /// count 0 → 1). Mirrors pvxs `SharedPV::onFirstConnect`
    /// (`sharedpv.cpp:299-313`): applications hook here to open the PV /
    /// start a producer on demand. Driven by [`Self::attach_channel`],
    /// NOT by monitor subscriptions — a GET/PUT/RPC/GET_FIELD-only client
    /// triggers it just like a monitoring one.
    pub fn on_first_connect<F>(&self, handler: F)
    where
        F: Fn(&SharedPV) + Send + Sync + 'static,
    {
        self.inner.lock().on_first_connect = Some(Arc::new(handler));
    }

    /// Hook fired when the *last client channel* detaches (channel count
    /// N → 0). Mirrors pvxs `SharedPV::onLastDisconnect`
    /// (`sharedpv.cpp:278-296`) — pair with `on_first_connect` to gate
    /// cost-of-production on actual channel interest. Driven by
    /// [`Self::detach_channel`], NOT by monitor subscriptions.
    pub fn on_last_disconnect<F>(&self, handler: F)
    where
        F: Fn(&SharedPV) + Send + Sync + 'static,
    {
        self.inner.lock().on_last_disconnect = Some(Arc::new(handler));
    }

    /// Record that a client channel attached, firing `on_first_connect`
    /// on the channel-count 0 → 1 transition. Called by the server's
    /// CREATE_CHANNEL owner via `SharedSource::notify_channel_open`,
    /// mirroring pvxs `SharedPV::attach` inserting into `impl->channels`
    /// and running `onFirstConnect` on empty→non-empty
    /// (`sharedpv.cpp:299-313`). The callback runs after the lock is
    /// released so it may re-enter `open`/`post`/`current`.
    pub fn attach_channel(&self) {
        let cb = {
            let mut g = self.inner.lock();
            g.channel_count += 1;
            if g.channel_count == 1 {
                g.on_first_connect.clone()
            } else {
                None
            }
        };
        if let Some(f) = cb {
            f(self);
        }
    }

    /// Record that a client channel detached, firing `on_last_disconnect`
    /// on the channel-count 1 → 0 transition. Called by the server's
    /// channel-teardown owner via `SharedSource::notify_channel_close`,
    /// mirroring pvxs erasing from `impl->channels` and running
    /// `onLastDisconnect` on the non-empty→empty edge
    /// (`sharedpv.cpp:278-296`). Saturating so a stray close never
    /// underflows the count.
    pub fn detach_channel(&self) {
        let cb = {
            let mut g = self.inner.lock();
            if g.channel_count == 0 {
                None
            } else {
                g.channel_count -= 1;
                if g.channel_count == 0 {
                    g.on_last_disconnect.clone()
                } else {
                    None
                }
            }
        };
        if let Some(f) = cb {
            f(self);
        }
    }

    /// Non-allocating snapshot — copies the current value into `out`
    /// without cloning if the descriptors match. Returns false when
    /// the PV isn't opened or has no value yet. Mirrors pvxs
    /// `SharedPV::fetch`.
    pub fn fetch(&self, out: &mut Option<PvField>) -> bool {
        let g = self.inner.lock();
        match &g.state {
            PvState::Open { value, .. } => {
                *out = Some(value.clone());
                true
            }
            PvState::Closed => false,
        }
    }

    /// Drop dead (closed-receiver) monitor subscribers so a long-idle PV
    /// doesn't retain closed outboxes until the next `post()`. This is a
    /// monitor-subscriber cleanup only: it does NOT fire
    /// `on_last_disconnect`, which is a *channel*-lifecycle edge driven by
    /// [`Self::detach_channel`] (pvxs keeps `subscribers` and
    /// `impl->channels` separate, sharedpv.cpp:252-296).
    pub fn prune_subscribers(&self) {
        self.inner.lock().subscribers.retain(|tx| !tx.is_closed());
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

    /// install a monitor start/stop callback. Fired with
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

/// Error from [`SharedSource::try_add`] / [`crate::service::add_rpc_service`]
/// when a PV name is already registered. The existing PV is left in place
/// — the served namespace is never swapped. Mirrors pvxs
/// `StaticSource::add()` throwing `"add() will not create duplicate PV"`
/// (`sharedpv.cpp:568-581`); for an intentional swap, [`SharedSource::remove`]
/// first.
#[derive(Debug, Clone, thiserror::Error)]
#[error("SharedSource: PV '{0}' is already registered (remove() it first to replace)")]
pub struct AddPvError(pub String);

/// Trivial map-of-named-SharedPV adapter that implements
/// [`super::source::ChannelSource`]. Construct via `SharedSource::new()`,
/// `add(name, shared_pv)`, then pass to `super::runtime::run_pva_server`.
pub struct SharedSource {
    pvs: Mutex<HashMap<String, SharedPV>>,
    /// Optional per-source access gate. When `None`, the trait
    /// default open gate is returned (back-compat). When `Some`,
    /// every wire op routes its allow/deny check through this
    /// gate — use with `AccessGate::required(acf, resolver)` to
    /// enforce a real .acf policy against PVA clients. Set via
    /// [`SharedSource::set_access_gate`].
    access_gate: std::sync::OnceLock<epics_base_rs::server::access_security::AccessGate>,
    /// Registry beacon-change counter, surfaced through
    /// [`ChannelSource::beacon_change`](super::source::ChannelSource::beacon_change).
    /// pvxs treats the built-in `StaticSource` PV registry as part of the
    /// single server `beaconChange`: `Server::addPV` / `removePV` bump it
    /// (`server.cpp:180,189`) just like `addSource` / `removeSource`. This
    /// is the Rust equivalent of those `addPV` / `removePV` bumps —
    /// incremented by [`Self::try_add`] on a real insert and by
    /// [`Self::remove`] on a real erase — so the UDP beacon task advances
    /// its `change_count` when a hosted PV is added or replaced even
    /// within one beacon interval (when `list_pvs()` is unchanged
    /// before/after, e.g. remove+re-add of the same name).
    beacon_change: AtomicU64,
    /// Server channel-invalidator, installed by the runtime via
    /// [`ChannelSource::set_channel_invalidator`](super::source::ChannelSource::set_channel_invalidator)
    /// before connections are accepted. `None` until then (e.g. before the
    /// server runs, or in tests that never start one). Bound into every
    /// registered [`SharedPV`] so [`SharedPV::close`] can force-disconnect
    /// the PV's attached server channels (pvxs `SharedPV::close()` channel
    /// teardown, `sharedpv.cpp:411-414`).
    invalidator: Mutex<Option<super::source::ChannelInvalidator>>,
}

impl SharedSource {
    pub fn new() -> Self {
        Self {
            pvs: Mutex::new(HashMap::new()),
            access_gate: std::sync::OnceLock::new(),
            beacon_change: AtomicU64::new(0),
            invalidator: Mutex::new(None),
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

    /// Register `pv` under `name`, rejecting a duplicate name without
    /// mutating the map (pvxs `StaticSource::add()` parity,
    /// `sharedpv.cpp:568-581`). The check-and-insert is atomic under the
    /// single map lock, so two concurrent registrations of the same name
    /// cannot both succeed. This is the single owner of insertion — every
    /// add path routes through it, so "a name is registered at most once"
    /// holds by construction and the served namespace can never silently
    /// swap underneath in-flight operations holding the old `SharedPV`.
    pub fn try_add(&self, name: impl Into<String>, pv: SharedPV) -> Result<(), AddPvError> {
        use std::collections::hash_map::Entry;
        // Single lock-acquisition order across this and
        // `set_channel_invalidator`: `pvs` first, then `invalidator`. The PV
        // inner mutex (`bind_invalidator`) is a leaf, so no cycle exists.
        let mut pvs = self.pvs.lock();
        match pvs.entry(name.into()) {
            Entry::Occupied(e) => Err(AddPvError(e.key().clone())),
            Entry::Vacant(slot) => {
                // If the server is already running, bind its invalidator now
                // so `SharedPV::close()` force-disconnects this PV's channels.
                // A PV added before the server starts is bound later, when the
                // runtime calls `set_channel_invalidator`.
                if let Some(invalidator) = self.invalidator.lock().clone() {
                    pv.bind_invalidator(invalidator, slot.key().clone());
                }
                slot.insert(pv);
                // pvxs `Server::addPV` bumps `beaconChange` only after the
                // built-in `StaticSource::add` succeeds (it throws on a
                // duplicate, skipping the bump — server.cpp:177-180). This
                // is the single insertion owner, so bumping here on the
                // vacant arm gives the lenient `add()` wrapper the same
                // "bump on success, not on duplicate" semantics for free.
                self.beacon_change.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }
    }

    /// Lenient register: [`Self::try_add`] but a duplicate is logged and
    /// the existing PV is kept (never replaced) instead of surfacing an
    /// error — for the many infallible call sites. Callers that must
    /// observe the duplicate (the service framework) use `try_add`.
    pub fn add(&self, name: impl Into<String>, pv: SharedPV) {
        if let Err(e) = self.try_add(name, pv) {
            tracing::warn!(
                pv = %e.0,
                "SharedSource::add: PV already registered — keeping the \
                 existing SharedPV, ignoring the duplicate. Call \
                 SharedSource::remove first for an intentional swap."
            );
        }
    }

    /// Remove a previously-added PV by name. Returns the removed
    /// SharedPV when the name was present. Used by
    /// [`crate::service::remove_rpc_service`] to tear down an RPC
    /// service registered via `add_rpc_service`; also useful for
    /// dynamic IOC topologies where PVs come and go at runtime.
    pub fn remove(&self, name: &str) -> Option<SharedPV> {
        let removed = self.pvs.lock().remove(name);
        if removed.is_some() {
            // pvxs `Server::removePV` bumps `beaconChange` so the next
            // BEACON signals the registry change (server.cpp:184-189).
            // Bump only on a real erase — consistent with
            // [`CompositeSource::remove_source`], which treats a no-op
            // remove (absent name) as not a topology change. A real erase
            // combined with a re-add of the same name within one beacon
            // interval still advances `change_count` (net `+2`) even
            // though `list_pvs()` is unchanged.
            self.beacon_change.fetch_add(1, Ordering::Relaxed);
        }
        removed
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

    /// Surface this source's PV-registry counter so the UDP beacon task
    /// advances `change_count` on an `add` / `remove` of a hosted PV —
    /// the Rust equivalent of pvxs `Server::addPV` / `removePV` bumping
    /// the single server `beaconChange` (`server.cpp:180,189`). Without
    /// this override the trait default returns `0`, and the beacon task
    /// would fall back to the PV-name-set hash, which cannot see a
    /// remove+re-add of the same name within one beacon interval.
    fn beacon_change(&self) -> u64 {
        self.beacon_change.load(Ordering::Relaxed)
    }

    /// Store the server's channel-invalidator and bind it into every
    /// already-registered [`SharedPV`], so `SharedPV::close()` can
    /// force-disconnect that PV's attached server channels (the channel
    /// half of pvxs `SharedPV::close()`, `sharedpv.cpp:411-414`). PVs added
    /// after this call are bound by [`Self::try_add`]. Same lock order as
    /// `try_add` — `pvs` first, then `invalidator` — so the two cannot
    /// deadlock, and holding `pvs` across the install makes the
    /// store-and-bind atomic against a concurrent `add`.
    fn set_channel_invalidator(&self, invalidator: super::source::ChannelInvalidator) {
        let pvs = self.pvs.lock();
        *self.invalidator.lock() = Some(invalidator.clone());
        for (name, pv) in pvs.iter() {
            pv.bind_invalidator(invalidator.clone(), name.clone());
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

    /// Park until the PV is `open()`ed, instead of answering "not yet"
    /// as a refusal — pvxs `sharedpv.cpp:239-249` / `:259-275` /
    /// `:348-384`. A name this source does not host resolves `None`
    /// immediately: that IS a permanent answer.
    fn await_introspection(
        &self,
        name: &str,
        _ctx: super::source::ChannelContext,
    ) -> impl std::future::Future<Output = Option<FieldDesc>> + Send {
        let pv = self.pvs.lock().get(name).cloned();
        async move {
            match pv {
                Some(pv) => Some(pv.wait_open().await),
                None => None,
            }
        }
    }

    fn get_value(&self, name: &str) -> impl std::future::Future<Output = Option<PvField>> + Send {
        let pv = self.pvs.lock().get(name).cloned();
        async move { pv.and_then(|p| p.current()) }
    }

    fn put_value(
        &self,
        name: &str,
        value: PvField,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let pv = self.pvs.lock().get(name).cloned();
        async move {
            match pv {
                Some(p) => p.put(value).map_err(OpError::failed),
                None => Err(OpError::failed(format!("no such PV: {name}"))),
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
        desc: std::sync::Arc<FieldDesc>,
        changed: crate::proto::BitSet,
        delta: &PvField,
        ctx: super::source::ChannelContext,
    ) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let pv = self.pvs.lock().get(checked.pv_name()).cloned();
        async move {
            if !checked.allows_write() {
                return Err(OpError::denied(format!(
                    "PUT denied by access security: '{}' from {}/{}/{}",
                    checked.pv_name(),
                    ctx.creds.host,
                    ctx.creds.account,
                    ctx.creds.method,
                )));
            }
            match pv {
                Some(p) => p
                    .put_delta(&desc, &changed, delta.clone())
                    .map_err(OpError::failed),
                None => Err(OpError::failed(format!(
                    "no such PV: {}",
                    checked.pv_name()
                ))),
            }
        }
    }

    fn is_writable(&self, name: &str) -> impl std::future::Future<Output = bool> + Send {
        // Per-PV write policy, not mere existence: a PV is writable only
        // when it has an `on_put` handler (built via
        // [`SharedPV::build_mailbox`] or a custom `on_put`). A plain or
        // read-only PV reports `false`, matching pvxs which has no
        // implicit-writable `SharedPV`.
        let writable = self.pvs.lock().get(name).map(|p| p.is_writable());
        async move { writable.unwrap_or(false) }
    }

    fn subscribe(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Option<MonitorStream<PvField>>> + Send {
        let pv = self.pvs.lock().get(name).cloned();
        async move {
            // pvxs servermon.cpp:66: default queue limit = 4.
            let inbox = pv.and_then(|p| p.subscribe(4))?;
            // The ring IS the stream: no bridge task, no second queue.
            // Squash-to-tail semantics live in the ring itself.
            Some(MonitorStream::Ring(inbox))
        }
    }

    /// Single-seed MONITOR: atomically capture the current value as the
    /// connect-time seed and register an **updates-only** subscriber via
    /// [`SharedPV::subscribe_seeded`], returning both as one
    /// [`super::source::SubscriptionSeed`]. The default impl would subscribe (the
    /// prepend path) and ALSO seed via `get_value` — the double-seed
    /// PVA-RS closes; overriding here keeps the seed atomic with
    /// registration (no gap-duplicate) and updates-only.
    fn subscribe_seeded(
        &self,
        checked: super::source::AccessChecked,
        ctx: super::source::ChannelContext,
        opts: super::source::MonitorOptions,
    ) -> impl std::future::Future<
        Output = Option<super::source::SubscriptionSeed<super::source::MonitorUpdate>>,
    > + Send {
        let _ = ctx;
        // pvxs `op->limit` (servermon.cpp:66,533-543): the negotiated per-op
        // limit — the server's default unless the client's
        // `record._options.queueSize` replaced it — sizes the source-side
        // accrual buffer, so a STOP->START or INIT->START window holds up to
        // `limit` distinct posts, not just the latest. The limit arrives
        // already resolved (never 0), so there is no second default here.
        let queue_limit = (opts.queue_size as usize).max(1);
        let pv = if checked.allows_read() {
            self.pvs.lock().get(checked.pv_name()).cloned()
        } else {
            None
        };
        async move {
            let (initial, inbox) = pv.and_then(|p| p.subscribe_seeded(queue_limit))?;
            Some(super::source::SubscriptionSeed {
                // A SharedPV's stored Value is wholly assigned by `open()` /
                // `post()`, so it declares no leaf subset — the server frames
                // every leaf the request selected, as pvxs does for a
                // fully-marked `Value`.
                initial: initial.map(super::source::SourceRead::from),
                updates: super::source::plain_monitor_updates(MonitorStream::Ring(inbox)),
                on_start: None,
            })
        }
    }

    fn rpc(
        &self,
        name: &str,
        request_desc: FieldDesc,
        request_value: PvField,
    ) -> impl std::future::Future<Output = Result<RpcReply, OpError>> + Send {
        let pv = self.pvs.lock().get(name).cloned();
        let name = name.to_string();
        async move {
            match pv {
                // Routes through rpc_async so a SharedPV with an
                // `on_rpc_async` handler (typical of services
                // registered by `#[pva_service]`) runs the user's
                // future on this task's runtime — no block_on or
                // block_in_place needed.
                Some(p) => p
                    .rpc_async(request_desc, request_value)
                    .await
                    .map_err(OpError::failed),
                None => Err(OpError::failed(format!("no such PV: {name}"))),
            }
        }
    }

    fn process(&self, name: &str) -> impl std::future::Future<Output = Result<(), OpError>> + Send {
        let pv = self.pvs.lock().get(name).cloned();
        let name = name.to_string();
        async move {
            match pv {
                Some(p) => p.process().map_err(OpError::failed),
                None => Err(OpError::failed(format!("no such PV: {name}"))),
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

    /// override default no-op to fire the per-PV `on_start`
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

    /// Override default no-op: route the channel-attach edge to the named
    /// `SharedPV` so it can fire `on_first_connect` on the first channel.
    /// This is the channel-lifecycle path pvxs drives through
    /// `SharedPV::attach` (`sharedpv.cpp:299-313`), kept separate from
    /// monitor subscription. A GET/PUT/RPC/GET_FIELD-only channel opens
    /// the producer here, not only a monitoring one.
    fn notify_channel_open(&self, name: &str, _ctx: &super::source::ChannelContext) {
        let pv = self.pvs.lock().get(name).cloned();
        if let Some(p) = pv {
            p.attach_channel();
        }
    }

    /// Override default no-op: route the channel-detach edge to the named
    /// `SharedPV` so it can fire `on_last_disconnect` on the last channel
    /// leaving (pvxs `sharedpv.cpp:278-296`). Mirror of
    /// [`super::source::ChannelSource::notify_channel_open`].
    fn notify_channel_close(&self, name: &str, _ctx: &super::source::ChannelContext) {
        let pv = self.pvs.lock().get(name).cloned();
        if let Some(p) = pv {
            p.detach_channel();
        }
    }

    /// expose the per-PV `(low, high)` watermark levels so the
    /// monitor loop fires the callbacks off the pipeline window rather
    /// than server-queue occupancy.
    async fn monitor_watermarks(&self, name: &str) -> Option<(usize, usize)> {
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

    /// A client-supplied `queueSize` must never be eagerly pre-allocated: an INIT
    /// with a hostile queueSize would otherwise force a multi-GB reservation and
    /// abort the process. `make_monitor_queue` grows lazily and `post` bounds live
    /// length to `limit`, so the huge limit is honored logically without the
    /// allocation.
    #[test]
    fn make_monitor_queue_does_not_preallocate_client_limit() {
        let huge = 1_000_000_000usize;
        let (outbox, inbox) = make_monitor_queue(huge);
        // Construction reserves nothing.
        assert_eq!(inbox.shared.inner.lock().items.capacity(), 0);
        // Posting eight values grows the deque only to serve those live items.
        for i in 0..8 {
            outbox.post(nt_scalar_int_value(i), false);
        }
        let inner = inbox.shared.inner.lock();
        assert!(inner.items.len() <= inner.limit);
        assert!(
            inner.items.capacity() < huge,
            "capacity {} must stay lazy, not track the client limit {huge}",
            inner.items.capacity(),
        );
    }

    /// NTScalar<Int> with a `time_t` `timeStamp` member. Bit numbering:
    /// root=0, value=1, timeStamp=2, secondsPastEpoch=3, nanoseconds=4,
    /// userTag=5 (so the timeStamp subtree covers bits 2..=5).
    fn nt_scalar_ts_desc() -> FieldDesc {
        let time_t = FieldDesc::Structure {
            struct_id: "time_t".into(),
            fields: vec![
                (
                    "secondsPastEpoch".into(),
                    FieldDesc::Scalar(ScalarType::Long),
                ),
                ("nanoseconds".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("userTag".into(), FieldDesc::Scalar(ScalarType::Int)),
            ],
        };
        FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![
                ("value".into(), FieldDesc::Scalar(ScalarType::Int)),
                ("timeStamp".into(), time_t),
            ],
        }
    }

    fn nt_scalar_ts_value(v: i32, secs: i64, nanos: i32) -> PvField {
        let mut ts = crate::pvdata::PvStructure::new("time_t");
        ts.fields.push((
            "secondsPastEpoch".into(),
            PvField::Scalar(ScalarValue::Long(secs)),
        ));
        ts.fields.push((
            "nanoseconds".into(),
            PvField::Scalar(ScalarValue::Int(nanos)),
        ));
        ts.fields
            .push(("userTag".into(), PvField::Scalar(ScalarValue::Int(0))));
        let mut s = crate::pvdata::PvStructure::new("epics:nt/NTScalar:1.0");
        s.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::Int(v))));
        s.fields.push(("timeStamp".into(), PvField::Structure(ts)));
        PvField::Structure(s)
    }

    /// Read back `(secondsPastEpoch, nanoseconds)` from an NTScalar value.
    fn extract_timestamp(v: &PvField) -> (i64, i32) {
        let PvField::Structure(root) = v else {
            panic!("not a structure");
        };
        let Some(PvField::Structure(ts)) = root.get_field("timeStamp") else {
            panic!("no timeStamp");
        };
        let secs = match ts.get_field("secondsPastEpoch") {
            Some(PvField::Scalar(ScalarValue::Long(s))) => *s,
            other => panic!("unexpected secondsPastEpoch: {other:?}"),
        };
        let nanos = match ts.get_field("nanoseconds") {
            Some(PvField::Scalar(ScalarValue::Int(n))) => *n,
            other => panic!("unexpected nanoseconds: {other:?}"),
        };
        (secs, nanos)
    }

    #[test]
    fn shared_pv_open_then_current() {
        let pv = SharedPV::new();
        assert!(!pv.is_open());
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(42))
            .unwrap();
        assert!(pv.is_open());
        assert!(pv.current().is_some());
    }

    /// Regression: `open()` must enforce the same descriptor/value guard
    /// every later post does — a scalar-type mismatch is rejected and the
    /// PV stays Closed, so a startup-only mismatch cannot be encoded under
    /// the advertised descriptor on the first GET. The same value also
    /// fails identically through `try_post_checked`.
    #[test]
    fn shared_pv_open_rejects_descriptor_value_type_mismatch() {
        // Descriptor says NTScalar<Double>, value is NTScalar<Int>.
        let double_desc = FieldDesc::Structure {
            struct_id: "epics:nt/NTScalar:1.0".into(),
            fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::Double))],
        };
        let double_value = || {
            let mut s = crate::pvdata::PvStructure::new("epics:nt/NTScalar:1.0");
            s.fields
                .push(("value".into(), PvField::Scalar(ScalarValue::Double(1.0))));
            PvField::Structure(s)
        };
        let pv = SharedPV::new();
        let err = pv
            .open(double_desc.clone(), nt_scalar_int_value(42))
            .expect_err("type mismatch must be rejected");
        assert!(matches!(err, crate::error::PvaError::InvalidValue(_)));
        assert!(!pv.is_open(), "PV must stay Closed after a rejected open");

        // The same mismatch fails identically on the post path once the
        // PV is correctly opened with a matching Double value.
        let pv2 = SharedPV::build_mailbox();
        pv2.open(double_desc, double_value()).unwrap();
        assert!(pv2.try_post_checked(nt_scalar_int_value(42)).is_err());
    }

    /// A `Variant` ("any") root is a deliberate Rust generalization for
    /// generic RPC-method slots (the service framework registers each
    /// method as a SharedPV opened with `FieldDesc::Variant`). Unlike
    /// pvxs's Struct-only root, it is accepted — but the seeded value
    /// must still fit the descriptor, so `Null` (an unset `any`) opens
    /// while a concrete value that the variant cannot faithfully carry
    /// does not.
    #[test]
    fn shared_pv_open_accepts_variant_root_with_null_value() {
        let pv = SharedPV::new();
        pv.open(FieldDesc::Variant, PvField::Null)
            .expect("variant root with null value must open");
        assert!(pv.is_open());
    }

    /// `try_add` is the single owner of insertion: a duplicate name is
    /// rejected and the originally-registered `SharedPV` is kept — the
    /// served namespace never silently swaps (pvxs `StaticSource::add()`
    /// rejects duplicates, `sharedpv.cpp:568-581`). The lenient `add`
    /// wrapper has the same keep-original effect. An intentional swap goes
    /// through `remove` first.
    #[test]
    fn try_add_rejects_duplicate_and_keeps_original() {
        let source = SharedSource::new();

        let first = SharedPV::new();
        first
            .open(nt_scalar_int_desc(), nt_scalar_int_value(1))
            .unwrap();
        source.try_add("dup", first).expect("first add succeeds");

        // A second PV under the same name is rejected; the map is untouched.
        let second = SharedPV::new();
        second
            .open(nt_scalar_int_desc(), nt_scalar_int_value(2))
            .unwrap();
        let err = source
            .try_add("dup", second)
            .expect_err("duplicate name must be rejected");
        assert_eq!(err.0, "dup");

        // The original (value 1) is still the registered PV.
        let kept = source.get("dup").expect("original PV remains");
        match kept.current() {
            Some(PvField::Structure(s)) => assert_eq!(
                s.get_field("value"),
                Some(&PvField::Scalar(ScalarValue::Int(1)))
            ),
            other => panic!("unexpected current value: {other:?}"),
        }

        // The lenient wrapper also keeps the original on a collision.
        let third = SharedPV::new();
        third
            .open(nt_scalar_int_desc(), nt_scalar_int_value(3))
            .unwrap();
        source.add("dup", third);
        let still = source.get("dup").expect("original PV still present");
        match still.current() {
            Some(PvField::Structure(s)) => assert_eq!(
                s.get_field("value"),
                Some(&PvField::Scalar(ScalarValue::Int(1)))
            ),
            other => panic!("unexpected current value: {other:?}"),
        }

        // After an explicit remove, the name is free to be re-registered.
        assert!(source.remove("dup").is_some());
        let fresh = SharedPV::new();
        fresh
            .open(nt_scalar_int_desc(), nt_scalar_int_value(9))
            .unwrap();
        source
            .try_add("dup", fresh)
            .expect("re-add after remove succeeds");
    }

    #[epics_macros_rs::epics_test]
    async fn shared_pv_subscribe_sees_initial_then_updates() {
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
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
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
        let _rx = pv.subscribe(8);
        pv.close();
        assert!(!pv.is_open());
        assert_eq!(pv.try_post(nt_scalar_int_value(1)), 0);
    }

    /// `SharedPV::close()` must publish the PV's registered name(s) through
    /// the bound server invalidator so each per-connection read loop
    /// force-disconnects the channels it serves under that name — the
    /// channel half of pvxs `SharedPV::close()` (`sharedpv.cpp:411-414`).
    /// Covers BOTH binding paths: a PV registered before the server
    /// installs the invalidator (`set_channel_invalidator` injection) and a
    /// PV registered after (`try_add` injection).
    #[test]
    fn close_publishes_registered_name_through_invalidator() {
        use crate::server_native::source::{ChannelInvalidator, ChannelSource};

        // Path 1: PV registered BEFORE the invalidator is installed.
        let src = SharedSource::new();
        let pv_before = SharedPV::build_mailbox();
        pv_before
            .open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
        src.add("before", pv_before.clone());

        let invalidator = ChannelInvalidator::new();
        let mut rx = invalidator.subscribe();
        src.set_channel_invalidator(invalidator);

        // Path 2: PV registered AFTER the invalidator is installed.
        let pv_after = SharedPV::build_mailbox();
        pv_after
            .open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
        src.add("after", pv_after.clone());

        pv_before.close();
        let batch = rx.try_recv().expect("close() must publish a batch");
        assert_eq!(
            batch.as_ref(),
            ["before".to_string()],
            "close() publishes the pre-install-registered name"
        );

        pv_after.close();
        let batch = rx.try_recv().expect("close() must publish a batch");
        assert_eq!(
            batch.as_ref(),
            ["after".to_string()],
            "close() publishes the post-install-registered name"
        );
    }

    /// A standalone `SharedPV` never registered with a source has no bound
    /// invalidator: `close()` must still clear value/subscribers and must
    /// not panic — there is simply no channel teardown to publish.
    #[test]
    fn close_without_invalidator_is_silent() {
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
        pv.close();
        assert!(!pv.is_open());
    }

    /// Regression: pvxs runs `onFirstConnect`/`onLastDisconnect` off the
    /// *channel* set (`impl->channels`, sharedpv.cpp:278-313), which is
    /// tracked separately from monitor `subscribers` (`:252-275`). The
    /// Rust hooks previously fired off the monitor-subscriber count, so a
    /// GET/PUT/RPC/GET_FIELD-only client never opened a lazy PV and
    /// `on_last_disconnect` (only reachable via the never-called
    /// `prune_subscribers`) never fired at all. They must now key off
    /// `attach_channel`/`detach_channel` 0↔1 transitions, and a monitor
    /// `subscribe()` must NOT fire them.
    #[test]
    fn channel_lifecycle_hooks_track_channels_not_subscribers() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let first = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(AtomicUsize::new(0));
        let pv = SharedPV::new();
        {
            let f = first.clone();
            pv.on_first_connect(move |p| {
                f.fetch_add(1, Ordering::SeqCst);
                // pvxs lazy-open pattern: open the PV when the first
                // channel attaches (testget.cpp:204-234).
                p.open(nt_scalar_int_desc(), nt_scalar_int_value(1))
                    .unwrap();
            });
        }
        {
            let l = last.clone();
            pv.on_last_disconnect(move |p| {
                l.fetch_add(1, Ordering::SeqCst);
                p.close();
            });
        }

        // A monitor subscribe on a still-closed PV returns None and must
        // NOT fire on_first_connect — it is a subscriber edge.
        assert!(pv.subscribe(4).is_none(), "closed PV: no subscription");
        assert_eq!(
            first.load(Ordering::SeqCst),
            0,
            "subscribe() must not fire on_first_connect"
        );
        assert!(!pv.is_open());

        // First channel attach fires on_first_connect → lazy open.
        pv.attach_channel();
        assert_eq!(first.load(Ordering::SeqCst), 1);
        assert!(pv.is_open(), "on_first_connect opened the PV");
        // A GET-only client (no monitor) now sees the value.
        assert!(pv.current().is_some());

        // Second channel attach: no re-fire (only the 0→1 edge counts).
        pv.attach_channel();
        assert_eq!(
            first.load(Ordering::SeqCst),
            1,
            "on_first_connect fires once on 0→1"
        );

        // One detach with a channel still left: on_last must NOT fire.
        pv.detach_channel();
        assert_eq!(last.load(Ordering::SeqCst), 0);
        assert!(pv.is_open());

        // Last detach: on_last_disconnect fires and closes the PV.
        pv.detach_channel();
        assert_eq!(
            last.load(Ordering::SeqCst),
            1,
            "on_last_disconnect fires on 1→0"
        );
        assert!(!pv.is_open());

        // A stray extra detach must not underflow the count or re-fire.
        pv.detach_channel();
        assert_eq!(
            last.load(Ordering::SeqCst),
            1,
            "saturating detach: no re-fire below zero"
        );
    }

    /// Regression: the server's CREATE_CHANNEL / teardown owner drives
    /// the channel lifecycle through `ChannelSource::notify_channel_open`
    /// / `notify_channel_close`. `SharedSource` must route those to the
    /// named `SharedPV`'s attach/detach so a lazy PV opens for a
    /// GET-only client (no monitor) on first channel and closes on last,
    /// matching pvxs lazy GET/PUT/RPC/GET_FIELD coverage.
    #[epics_macros_rs::epics_test]
    async fn shared_source_channel_open_close_drives_lazy_lifecycle() {
        use crate::server_native::source::{ChannelContext, ChannelSource};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let src = SharedSource::new();
        let pv = SharedPV::new();
        let opened = Arc::new(AtomicUsize::new(0));
        {
            let o = opened.clone();
            pv.on_first_connect(move |p| {
                o.fetch_add(1, Ordering::SeqCst);
                p.open(nt_scalar_int_desc(), nt_scalar_int_value(5))
                    .unwrap();
            });
        }
        pv.on_last_disconnect(|p| p.close());
        src.add("dut", pv);

        let ctx = ChannelContext {
            peer: "127.0.0.1:5075".parse().unwrap(),
            creds: std::sync::Arc::new(crate::server_native::config::ClientCredentials {
                account: String::new(),
                method: "anonymous".into(),
                host: String::new(),
                authority: String::new(),
                roles: Vec::new(),
            }),
            pv_request: None,
            log: Default::default(),
        };

        // No channel yet: a GET-only path sees a closed PV.
        assert!(src.get_value("dut").await.is_none());

        // Channel attach (the CREATE_CHANNEL owner's hook) drives lazy
        // open — no monitor subscription involved.
        src.notify_channel_open("dut", &ctx);
        assert_eq!(opened.load(Ordering::SeqCst), 1);
        assert!(
            src.get_value("dut").await.is_some(),
            "lazy GET sees value after first channel attaches"
        );

        // Channel close drives onLastDisconnect → PV closes again.
        src.notify_channel_close("dut", &ctx);
        assert!(
            src.get_value("dut").await.is_none(),
            "PV closed after last channel detaches"
        );
    }

    /// Regression: pvxs `close()` clears `impl->current`
    /// (`sharedpv.cpp:404-405`), so afterwards the descriptor and value
    /// are gone and `fetch()` throws `"open() first"` (`:443-469`). The
    /// Rust `close()` previously only flipped `is_open` and left
    /// `desc`/`value` populated, so `current()` / `introspection()` (and
    /// the GET / GET_FIELD wire paths that read them) surfaced the stale
    /// pre-close state. After the close all of `current()`,
    /// `introspection()`, `fetch()`, and a new `subscribe()` must report
    /// withdrawn; a fresh `open()` must restore them.
    #[test]
    fn shared_pv_close_withdraws_descriptor_value_and_fetch() {
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(42))
            .unwrap();
        assert!(pv.current().is_some());
        assert!(pv.introspection().is_some());

        pv.close();

        assert!(!pv.is_open());
        assert!(
            pv.current().is_none(),
            "closed SharedPV must not surface a stale value"
        );
        assert!(
            pv.introspection().is_none(),
            "closed SharedPV must not surface a stale descriptor"
        );
        let mut snap = None;
        assert!(
            !pv.fetch(&mut snap),
            "fetch() on a closed SharedPV must report no value"
        );
        assert!(snap.is_none());
        assert!(
            pv.subscribe(8).is_none(),
            "subscribe() on a closed SharedPV must be rejected"
        );

        // Reopen restores the lifecycle.
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(7))
            .unwrap();
        assert!(pv.is_open());
        assert!(pv.current().is_some());
        assert!(pv.introspection().is_some());
    }

    /// Regression: pvxs `SharedPV::open()` throws `"close() first"` when
    /// `impl->current` is already set (`sharedpv.cpp:357-358`), so a second
    /// `open()` without an intervening `close()` is rejected rather than
    /// silently swapping the descriptor/value out from under attached
    /// monitors. The Rust `open()` returns `Err` off the `PvState::Open`
    /// discriminant; only after `close()` does a fresh `open()` succeed.
    #[test]
    fn shared_pv_open_while_open_is_rejected() {
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(42))
            .unwrap();

        // Second open without close() is refused; the original type/value
        // are left intact (no partial swap).
        let err = pv
            .open(nt_scalar_int_desc(), nt_scalar_int_value(99))
            .expect_err("open() on an already-open SharedPV must be rejected");
        assert!(
            matches!(err, crate::error::PvaError::Protocol(_)),
            "reopen rejection is a protocol/logic error, got {err:?}"
        );
        assert_eq!(
            extract_int(&pv.current().expect("still open after rejected reopen")),
            42,
            "rejected reopen must not overwrite the published value"
        );

        // close() first, then open() succeeds — the pvxs close-before-open
        // contract.
        pv.close();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(99))
            .unwrap();
        assert_eq!(extract_int(&pv.current().expect("open after close")), 99,);
    }

    /// Regression: the GET / GET_FIELD wire paths read through
    /// `SharedSource::get_value` / `get_introspection`, which forward to
    /// `SharedPV::current()` / `introspection()`. A closed PV must report
    /// `None` on both, matching pvxs withdrawing `impl->current` so a
    /// post-close fetch throws `"open() first"` (`sharedpv.cpp:443-469`).
    #[epics_macros_rs::epics_test]
    async fn shared_source_get_paths_withdraw_after_close() {
        use crate::server_native::source::ChannelSource;

        let src = SharedSource::new();
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(42))
            .unwrap();
        src.add("X", pv.clone());

        assert!(src.get_value("X").await.is_some());
        assert!(src.get_introspection("X").await.is_some());

        pv.close();

        assert!(
            src.get_value("X").await.is_none(),
            "GET on a closed SharedPV must not return a stale value"
        );
        assert!(
            src.get_introspection("X").await.is_none(),
            "GET_FIELD on a closed SharedPV must not return a stale descriptor"
        );
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

    /// Regression: when a subscriber's queue is full, a normal post
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
    #[epics_macros_rs::epics_test]
    async fn pva_r6_squash_to_tail() {
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
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
        let empty =
            epics_base_rs::runtime::task::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await;
        assert!(empty.is_err(), "queue must be empty after squash drain");
    }

    // ── MonitorInbox::try_recv — one case per invariant boundary ────────────
    //
    // `try_recv` has exactly two boundaries, and every case below pins one
    // side of one of them:
    //
    //   items.is_empty()   false → Ok(front)      true → consult producer_done
    //   producer_done      false → Err(Empty)     true → Err(Disconnected)
    //
    // The (items non-empty, producer_done true) corner is the one that a
    // "check closed first" implementation would get wrong, so it gets its own
    // case rather than riding along inside a longer scenario.

    /// Boundary `items.is_empty() == true`, `producer_done == false`:
    /// nothing queued yet but the producer is alive → `Empty`, never
    /// `Disconnected`. A drain loop must read this as "park", not "tear down".
    #[test]
    fn try_recv_empty_ring_is_empty_not_disconnected() {
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
        let mut rx = pv.subscribe(4).expect("subscribe");
        // Drop the seed `open()` queued so the ring is genuinely empty.
        assert!(rx.try_recv().is_ok(), "seed value is queued by open()");
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    /// Boundary `items.is_empty() == false`: one queued value comes back
    /// without awaiting, and the ring returns to `Empty` behind it.
    #[test]
    fn try_recv_one_item_yields_it_then_empty() {
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
        let mut rx = pv.subscribe(4).expect("subscribe");
        let _seed = rx.try_recv().expect("seed");

        pv.try_post(nt_scalar_int_value(11));

        let got = rx.try_recv().expect("posted value");
        assert_eq!(extract_int(&got), 11);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    /// Same squash-to-tail rule `recv` observes (pvxs `servermon.cpp:283-286`)
    /// must hold on the non-blocking path: with `limit = 2`, posts 3 and 4
    /// overwrite the tail, so a `try_recv` drain sees `[1, 4]` — not `[1, 2]`
    /// (drop-newest) and not `[3, 4]` (drop-oldest).
    #[test]
    fn try_recv_sees_the_squashed_tail_not_the_dropped_value() {
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
        let mut rx = pv.subscribe(2).expect("subscribe"); // queue limit = 2
        let _seed = rx.try_recv().expect("seed");

        pv.try_post(nt_scalar_int_value(1)); // [1]
        pv.try_post(nt_scalar_int_value(2)); // [1, 2] — full
        pv.try_post(nt_scalar_int_value(3)); // squash: [1, 3]
        pv.try_post(nt_scalar_int_value(4)); // squash: [1, 4]

        let drained: Vec<i32> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|v| extract_int(&v))
            .collect();
        assert_eq!(
            drained,
            vec![1, 4],
            "squash-to-tail: oldest distinct entry plus the newest value"
        );
    }

    /// The corner that ordering gets wrong: `producer_done == true` while
    /// items remain. Drain-before-close means those items are still delivered;
    /// only once the ring runs dry does the closure surface. Checking
    /// `producer_done` first would swallow the tail of a monitor whose PV just
    /// closed.
    #[test]
    fn try_recv_drains_queued_items_before_reporting_producer_done() {
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
        let mut rx = pv.subscribe(4).expect("subscribe");
        let _seed = rx.try_recv().expect("seed");

        pv.try_post(nt_scalar_int_value(1));
        pv.try_post(nt_scalar_int_value(2));
        // `close()` clears `subscribers`, dropping the last `MonitorOutbox`
        // endpoint → `producer_done = true` with two values still queued.
        pv.close();

        assert_eq!(extract_int(&rx.try_recv().expect("queued 1")), 1);
        assert_eq!(extract_int(&rx.try_recv().expect("queued 2")), 2);
        assert_eq!(
            rx.try_recv(),
            Err(TryRecvError::Disconnected),
            "closure surfaces only after the queue drained"
        );
    }

    /// Boundary `items.is_empty() == true`, `producer_done == true`: the
    /// terminal state. Distinct from `Empty` — a drain loop ends here instead
    /// of parking forever on a ring nothing will ever post to. This is the
    /// non-blocking twin of `recv()` returning `None`.
    #[test]
    fn try_recv_empty_ring_after_close_is_disconnected() {
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
        let mut rx = pv.subscribe(4).expect("subscribe");
        let _seed = rx.try_recv().expect("seed");
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty), "alive and empty");

        pv.close();

        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
        // Terminal: repeated polls stay Disconnected, they do not flip back.
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    }

    #[test]
    fn shared_pv_watermarks_default_to_4_and_64() {
        let pv = SharedPV::new();
        assert_eq!(pv.watermarks(), (4, 64));
        pv.set_low_watermark(8);
        pv.set_high_watermark(128);
        assert_eq!(pv.watermarks(), (8, 128));
    }

    /// the source exposes per-PV `(low, high)` watermark
    /// levels to the monitor loop (which fires the callbacks off the
    /// pipeline window), and `set_*_watermark` retunes them. An unknown
    /// PV / level-less source returns `None`.
    #[epics_macros_rs::epics_test]
    async fn shared_source_exposes_per_pv_watermark_levels() {
        use super::super::source::ChannelSource;
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
        pv.set_low_watermark(2);
        pv.set_high_watermark(5);
        let src = SharedSource::new();
        src.add("wm:pv", pv);
        assert_eq!(src.monitor_watermarks("wm:pv").await, Some((2, 5)));
        assert_eq!(src.monitor_watermarks("nope").await, None);
    }

    /// Regression: consecutive `put_delta` writes to a writable
    /// (mailbox) `SharedPV` keep delivering to a live subscriber. The
    /// mailbox handler posts through `try_post_checked`, which retains
    /// the canonical subscriber set under the lock, so a subscriber
    /// survives back-to-back delta PUTs and receives every value.
    #[epics_macros_rs::epics_test]
    async fn mr_r2_put_delta_clone_drop_keeps_subscriber_alive() {
        use crate::proto::BitSet;

        let pv = SharedPV::build_mailbox();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
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

    /// A mailbox PUT that marks only `value` (timeStamp left unset) must
    /// get a fresh server timestamp, mirroring pvxs `buildMailbox`'s
    /// `onPut` (`sharedpv.cpp:113-121`). The PV opened with a zero
    /// timestamp; after the value-only PUT the stored timeStamp must hold
    /// the current wall-clock time, not the stale zero.
    #[epics_macros_rs::epics_test]
    async fn r0604_mailbox_put_value_only_stamps_timestamp() {
        use crate::proto::BitSet;
        let pv = SharedPV::build_mailbox();
        pv.open(nt_scalar_ts_desc(), nt_scalar_ts_value(0, 0, 0))
            .unwrap();
        let desc = nt_scalar_ts_desc();
        // Mark only `value` (bit 1); timeStamp subtree (bits 2..=5) unset.
        let mut changed = BitSet::new();
        changed.set(1);
        pv.put_delta(&desc, &changed, nt_scalar_ts_value(11, 0, 0))
            .expect("mailbox put ok");
        let (secs, _nanos) = extract_timestamp(&pv.current().expect("has value"));
        // A real POSIX wall-clock stamp is far past this 2020 epoch; the
        // pre-fix path would leave the opened zero in place.
        assert!(
            secs > 1_600_000_000,
            "unmarked timeStamp must be stamped from current time, got {secs}"
        );
        assert_eq!(extract_int(&pv.current().unwrap()), 11, "value stored");
    }

    /// A client that marks `timeStamp.secondsPastEpoch` keeps its own
    /// timestamp — pvxs only stamps when `!isMarked(true, true)`, so a
    /// marked child suppresses the server fill.
    #[epics_macros_rs::epics_test]
    async fn r0604_mailbox_put_marked_timestamp_is_preserved() {
        use crate::proto::BitSet;
        let pv = SharedPV::build_mailbox();
        pv.open(nt_scalar_ts_desc(), nt_scalar_ts_value(0, 0, 0))
            .unwrap();
        let desc = nt_scalar_ts_desc();
        // Mark `value` (1) and `secondsPastEpoch` (3): client supplied a
        // timestamp, so the server must not overwrite it.
        let mut changed = BitSet::new();
        changed.set(1);
        changed.set(3);
        pv.put_delta(&desc, &changed, nt_scalar_ts_value(11, 12_345, 0))
            .expect("mailbox put ok");
        let (secs, _nanos) = extract_timestamp(&pv.current().expect("has value"));
        assert_eq!(
            secs, 12_345,
            "client-marked timeStamp must be preserved, not re-stamped"
        );
    }

    /// A value type with no `timeStamp` field is unaffected: the PUT
    /// stores the value and the server fabricates no timeStamp.
    #[epics_macros_rs::epics_test]
    async fn r0604_mailbox_put_no_timestamp_field_unchanged() {
        use crate::proto::BitSet;
        let pv = SharedPV::build_mailbox();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
        let desc = nt_scalar_int_desc();
        let mut changed = BitSet::new();
        changed.set(1);
        pv.put_delta(&desc, &changed, nt_scalar_int_value(11))
            .expect("mailbox put ok");
        let cur = pv.current().expect("has value");
        assert_eq!(extract_int(&cur), 11, "value stored");
        let PvField::Structure(s) = cur else {
            panic!("not a structure");
        };
        assert!(
            s.get_field("timeStamp").is_none(),
            "no timeStamp must be fabricated for a type without one"
        );
    }

    /// A plain (no-handler) `SharedPV` is NOT writable: both `put` and
    /// `put_delta` are rejected, and the stored value is unchanged
    /// (pvxs `sharedpv.cpp:209-227`). `build_mailbox` makes it writable;
    /// `build_readonly` rejects with the pvxs `"Read-only PV"` message.
    #[epics_macros_rs::epics_test]
    async fn plain_shared_pv_rejects_put_mailbox_accepts_readonly_refuses() {
        use crate::proto::BitSet;

        // Plain: reject, value unchanged.
        let plain = SharedPV::new();
        plain
            .open(nt_scalar_int_desc(), nt_scalar_int_value(7))
            .unwrap();
        assert!(!plain.is_writable(), "plain PV must not be writable");
        assert!(
            plain.put(nt_scalar_int_value(9)).is_err(),
            "plain put rejected"
        );
        let mut changed = BitSet::new();
        changed.set(1);
        assert!(
            plain
                .put_delta(&nt_scalar_int_desc(), &changed, nt_scalar_int_value(9))
                .is_err(),
            "plain put_delta rejected"
        );
        assert_eq!(
            extract_int(&plain.current().unwrap()),
            7,
            "rejected PUT must not mutate the stored value"
        );

        // Mailbox: accept and store.
        let mbox = SharedPV::build_mailbox();
        mbox.open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
        assert!(mbox.is_writable(), "mailbox PV must be writable");
        mbox.put(nt_scalar_int_value(42)).expect("mailbox put ok");
        assert_eq!(extract_int(&mbox.current().unwrap()), 42);

        // Read-only: explicit refusal with the pvxs message.
        let ro = SharedPV::build_readonly();
        ro.open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
        assert!(
            !ro.is_writable(),
            "read-only PV reports not writable (still has a refusing handler)"
        );
        let err = ro.put(nt_scalar_int_value(1)).unwrap_err();
        assert_eq!(err, "Read-only PV");
    }

    /// explicit `close()` must still terminate the subscriber
    /// inbox — closure is an owner action, and the invariant only
    /// forbids *internal clone drops* from closing the queue.
    #[epics_macros_rs::epics_test]
    async fn mr_r2_close_still_terminates_subscriber() {
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
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

    /// Mailbox `put_delta` now stores the merged value AND delivers it to
    /// subscribers under one `inner` lock — the same lock `close()` takes
    /// (pvxs serializes `post()`/`close()` on `impl->lock`,
    /// `sharedpv.cpp:394-407`). This pins the in-lock contract: a PUT
    /// while open commits the value and the subscriber observes exactly
    /// that committed value; once `close()` runs, the inbox terminates
    /// and a later PUT is rejected with no surviving clone to deliver a
    /// post-close value.
    #[epics_macros_rs::epics_test]
    async fn r0604_put_delta_store_and_delivery_serialize_with_close() {
        use crate::proto::BitSet;
        let pv = SharedPV::build_mailbox();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0))
            .unwrap();
        let mut rx = pv.subscribe(8).expect("subscribe");
        let _ = rx.recv().await.expect("initial value");

        let desc = nt_scalar_int_desc();
        let mut changed = BitSet::new();
        changed.set(1);
        // PUT while open: store and delivery happen under one lock, so
        // `current()` and the delivered value are the same commit.
        pv.put_delta(&desc, &changed, nt_scalar_int_value(7))
            .expect("put while open");
        assert_eq!(extract_int(&pv.current().unwrap()), 7, "store committed");
        assert_eq!(
            extract_int(&rx.recv().await.unwrap()),
            7,
            "subscriber observes the committed value"
        );

        // close() terminates the subscriber. Because delivery is under
        // the same lock, no post can be ordered after this boundary.
        pv.close();
        assert!(rx.recv().await.is_none(), "close terminates the inbox");

        // A PUT after close is rejected; there is no surviving subscriber
        // clone (the pre-fix cross-lock clone) to receive a post-close
        // value.
        let err = pv
            .put_delta(&desc, &changed, nt_scalar_int_value(9))
            .unwrap_err();
        assert!(err.contains("not open"), "put after close rejected: {err}");
        assert!(pv.current().is_none(), "closed PV has no current value");
    }

    /// Liveness.
    /// In-lock delivery must not deadlock against `close()`. Both `put_delta`
    /// (mailbox) and `close()` take the `inner` mutex, and delivery now runs
    /// while `inner` is held; this is safe only because `MonitorOutbox::post`
    /// locks solely its own per-queue mutex (never `inner`). Race real
    /// threads: every PUT and every close must make progress (both joins
    /// return) — a deadlock would hang this test.
    #[test]
    fn r0604_put_delta_close_race_makes_progress_without_deadlock() {
        use crate::proto::BitSet;
        use std::sync::{Arc, Barrier};
        let desc = nt_scalar_int_desc();
        let mut changed = BitSet::new();
        changed.set(1);

        for iter in 0..100 {
            let pv = SharedPV::build_mailbox();
            pv.open(nt_scalar_int_desc(), nt_scalar_int_value(0))
                .unwrap();
            // Hold a live subscriber so delivery runs under the lock during
            // the race (the path that would deadlock if post() took inner).
            let _rx = pv.subscribe(8).expect("subscribe");
            let barrier = Arc::new(Barrier::new(2));

            let w_pv = pv.clone();
            let w_desc = desc.clone();
            let w_changed = changed.clone();
            let w_barrier = Arc::clone(&barrier);
            let writer = std::thread::spawn(move || {
                w_barrier.wait();
                // A PUT racing a close may be accepted or rejected; both are
                // valid serializations. We only require it to return.
                let _ = w_pv.put_delta(&w_desc, &w_changed, nt_scalar_int_value(iter));
            });
            let c_pv = pv.clone();
            let c_barrier = Arc::clone(&barrier);
            let closer = std::thread::spawn(move || {
                c_barrier.wait();
                c_pv.close();
            });
            writer.join().unwrap();
            closer.join().unwrap();
            // close() always wins eventually: the PV ends closed.
            assert!(!pv.is_open(), "PV must be closed after the race");
        }
    }

    #[epics_macros_rs::epics_test]
    async fn shared_source_serves_named_pv() {
        use super::super::source::ChannelSource;
        let pv = SharedPV::new();
        pv.open(nt_scalar_int_desc(), nt_scalar_int_value(123))
            .unwrap();
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

    /// A built-in `SharedSource`
    /// PV registry mutation must advance the beacon-change counter, the
    /// Rust analog of pvxs `Server::addPV` / `removePV` bumping
    /// `beaconChange` (server.cpp:180,189). The boundary case the PV-name
    /// hash fallback cannot detect — remove + re-add of the SAME name
    /// within one beacon interval, so `list_pvs()` is identical
    /// before/after — must still advance the counter.
    #[epics_macros_rs::epics_test]
    async fn shared_source_beacon_change_advances_on_pv_registry_mutations() {
        use super::super::source::ChannelSource;

        let src = SharedSource::new();
        let v0 = src.beacon_change();

        // add → bump.
        src.add("a", SharedPV::new());
        let v1 = src.beacon_change();
        assert!(v1 > v0, "add must bump beacon_change: {v0} -> {v1}");

        // try_add of a DUPLICATE name → Err, must NOT bump (pvxs addPV
        // throws before the bump, server.cpp:177-180).
        assert!(src.try_add("a", SharedPV::new()).is_err());
        assert_eq!(
            src.beacon_change(),
            v1,
            "duplicate try_add must not advance beacon_change"
        );

        // remove of a present PV → bump.
        assert!(src.remove("a").is_some());
        let v2 = src.beacon_change();
        assert!(v2 > v1, "remove must bump beacon_change: {v1} -> {v2}");

        // no-op remove of an absent name → must NOT bump (consistent with
        // CompositeSource::remove_source's no-op semantics).
        assert!(src.remove("a").is_none());
        assert_eq!(
            src.beacon_change(),
            v2,
            "no-op remove must not advance beacon_change"
        );

        // The boundary case: re-add "b", capture the PV set, then
        // remove+re-add "b" within one interval. list_pvs() is identical
        // before and after, but the counter advances (net +2).
        src.add("b", SharedPV::new());
        let before: Vec<String> = src.list_pvs().await;
        let v3 = src.beacon_change();
        assert!(src.remove("b").is_some());
        src.add("b", SharedPV::new());
        let after: Vec<String> = src.list_pvs().await;
        let v4 = src.beacon_change();
        assert_eq!(
            before, after,
            "list_pvs must be identical across remove+re-add"
        );
        assert!(
            v4 > v3,
            "remove+re-add of the same name within one interval must still \
             advance beacon_change even though list_pvs is unchanged: {v3} -> {v4}"
        );
    }
}
