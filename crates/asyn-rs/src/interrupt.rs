// RTEMS-EXEC-MODEL-ALLOW(14): checked, not waived — all 14 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p asyn-rs
// --all-features`, 1081/1081). asyn-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

use tokio::sync::broadcast;

use crate::error::AsynStatus;
use crate::interfaces::InterfaceType;
use crate::param::ParamValue;

/// Filter for selecting which interrupts to receive.
#[derive(Debug, Clone, Default)]
pub struct InterruptFilter {
    /// If set, only receive interrupts with this reason (parameter index).
    pub reason: Option<usize>,
    /// If set, only receive interrupts with this addr.
    pub addr: Option<i32>,
    /// For UInt32Digital: bitmask of bits this subscriber is interested in.
    /// If set, only interrupts where changed bits overlap this mask are forwarded.
    /// C parity: pInterrupt->mask in asynUInt32DigitalInterrupt.
    pub uint32_mask: Option<u32>,
    /// If set, only receive interrupts whose value was produced for this asyn
    /// interface. `None` = accept any interface (the legacy/untyped path).
    ///
    /// C asyn keeps a separate interrupt list per interface type
    /// (`int32InterruptList` / `float64InterruptList` /
    /// `uInt32DigitalInterruptList` …, asynManager interruptBase is allocated
    /// per interface), so one reason delivers an interface-correct value to
    /// each record by the interface its DTYP bound. A record subscribes with
    /// its own interface here; a driver that fires per-interface values
    /// (`PortDriverBase::notify_interface_value`) tags each, and only the
    /// matching records receive it. A driver that fires a single untyped value
    /// (`call_param_callbacks`) leaves the value's `iface` `None`, which every
    /// subscriber still accepts — preserving the pre-per-interface behaviour.
    pub iface: Option<InterfaceType>,
}

impl InterruptFilter {
    /// Whether an interrupt value passes this filter (reason + addr +
    /// UInt32Digital changed-bit mask). Shared by the mailbox and the
    /// synchronous-callback delivery paths.
    fn matches(&self, iv: &InterruptValue) -> bool {
        if let Some(r) = self.reason {
            if iv.reason != r {
                return false;
            }
        }
        if let Some(a) = self.addr {
            if iv.addr != a {
                return false;
            }
        }
        if let Some(m) = self.uint32_mask {
            if iv.uint32_changed_mask & m == 0 {
                return false;
            }
        }
        // Per-interface routing: a typed value (driver fired it for a specific
        // interface) reaches only subscribers on that interface. An untyped
        // value (`iv.iface == None`, the `call_param_callbacks` path) reaches
        // every subscriber, and a subscriber with no interface filter
        // (`self.iface == None`) accepts every value — so the gate fires only
        // when both sides name an interface and they differ. This mirrors C's
        // per-interface interrupt lists without changing single-value drivers.
        if let (Some(want), Some(got)) = (self.iface, iv.iface) {
            if want != got {
                return false;
            }
        }
        true
    }

    /// Whether an OCTET interrupt fired under `rule` reaches this subscriber.
    ///
    /// C's octet interrupt list is not keyed by `reason` **at all**:
    /// `asynOctetBase::callInterruptUsers` tests `if (addr == pinterrupt->addr)`
    /// and consults `reason` nowhere (asynOctetBase.c:202-210). The only thing
    /// that varies between C's two octet fan-outs is whether the addr test runs
    /// — see [`OctetFanOut`]. So this gate is the addr rule plus the
    /// per-interface routing every path shares; the `reason` and
    /// `uint32_mask` gates of [`matches`](Self::matches) do not apply to octet.
    ///
    /// [`matches`]: Self::matches
    fn accepts_octet(&self, rule: OctetFanOut) -> bool {
        if let OctetFanOut::ByAddr(addr) = rule {
            if let Some(a) = self.addr {
                if addr != a {
                    return false;
                }
            }
        }
        // A subscriber bound to a different interface never receives octet.
        !matches!(self.iface, Some(want) if want != InterfaceType::Octet)
    }

    /// Whether a value fired for `(reason, addr, iface)` could reach this
    /// subscriber, ignoring the UInt32Digital changed-bit gate. This is the
    /// *presence* predicate (would a fire ever land here), not the per-value
    /// [`matches`](Self::matches) gate: a polling driver uses it to skip the
    /// cost of decoding+firing an interface with no subscriber, mirroring C
    /// `readPoller` iterating an empty interrupt list. The `uint32_mask` gate is
    /// deliberately skipped — it depends on which bits *changed* this poll, a
    /// per-value property, whereas presence is independent of the value.
    fn accepts(&self, reason: usize, addr: i32, iface: InterfaceType) -> bool {
        if let Some(r) = self.reason {
            if reason != r {
                return false;
            }
        }
        if let Some(a) = self.addr {
            if addr != a {
                return false;
            }
        }
        if let Some(want) = self.iface {
            if want != iface {
                return false;
            }
        }
        true
    }
}

/// Which subscribers an **octet** interrupt reaches.
///
/// C has exactly two octet fan-out rules, and they are different on purpose:
///
/// - [`ByAddr`](Self::ByAddr) — `asynOctetBase::callInterruptUsers`
///   (asynOctetBase.c:202-210): deliver to every registered octet user whose
///   `addr` equals the read's. This is the rule for a device read.
/// - [`EveryUser`](Self::EveryUser) — `drvAsynIPServerPort`'s listener thread
///   (drvAsynIPServerPort.c:388-398 for a new connection, :326-334 for a UDP
///   datagram): walk the interrupt list and call EVERY node, with no test at
///   all.
///
/// Neither consults `reason`. The rule therefore belongs to the **emitter**, not
/// to the subscriber's filter — a port that applies one filter to both fan-outs
/// announces a new connection only to the record registered on that slot's addr
/// (R19-109) and drops octet values for any record whose REASON is non-zero
/// (R19-113). Naming the rule at the fire site makes the third, invented rule
/// unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OctetFanOut {
    /// The device-read rule: `addr` must match.
    ByAddr(i32),
    /// The IP-server listener rule: no filter at all.
    EveryUser,
}

/// Value delivered through the interrupt system.
///
/// C parity: every `asynPortDriver::int32Callback` /
/// `float64Callback` / `octetCallback` walks the interrupt list and
/// before invoking each subscriber callback writes
/// `pInterrupt->pasynUser->auxStatus` / `alarmStatus` /
/// `alarmSeverity` / `timestamp` from the param's stored status
/// (`asynPortDriver.cpp:633-637,677-681,722-726,766-770,811-815`).
/// Drivers / records read these on the consumer side to escalate
/// alarms and report I/O status. The Rust port carries the same
/// fields on `InterruptValue` so subscribers see what C would set on
/// the `pasynUser`.
///
/// New fields are `Default::default()`-friendly so callers that do
/// not need them can rely on the struct-update syntax
/// (`..Default::default()`).
#[derive(Debug, Clone)]
pub struct InterruptValue {
    pub reason: usize,
    pub addr: i32,
    pub value: ParamValue,
    pub timestamp: SystemTime,
    /// For UInt32Digital: bitmask of which bits changed (for per-callback filtering).
    pub uint32_changed_mask: u32,
    /// C parity: `pasynUser->auxStatus` set on every callback emission.
    /// Carries the originating param status (asynPortDriver.cpp:633).
    pub aux_status: AsynStatus,
    /// C parity: `pasynUser->alarmStatus` (asynPortDriver.cpp:634).
    pub alarm_status: u16,
    /// C parity: `pasynUser->alarmSeverity` (asynPortDriver.cpp:635).
    pub alarm_severity: u16,
    /// The asyn interface this value was decoded for, or `None` when the
    /// driver fired a single untyped value (the `call_param_callbacks` path).
    /// See [`InterruptFilter::iface`] for the per-interface routing contract.
    pub iface: Option<InterfaceType>,
}

impl Default for InterruptValue {
    fn default() -> Self {
        Self {
            reason: 0,
            addr: 0,
            value: ParamValue::Undefined,
            timestamp: SystemTime::UNIX_EPOCH,
            uint32_changed_mask: 0,
            aux_status: AsynStatus::Success,
            alarm_status: 0,
            alarm_severity: 0,
            iface: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Mailbox-based subscription (replaces broadcast filter+forward tasks)
// ---------------------------------------------------------------------------

/// Per-subscriber mailbox: stores the latest matching interrupt value.
/// Intermediate updates are coalesced — consumer always sees the most recent state.
struct SubscriptionMailbox {
    filter: InterruptFilter,
    /// Latest matching value (overwritten on each notify, taken on recv).
    latest: parking_lot::Mutex<Slot>,
    /// Wakeup signal for the consumer.
    wakeup: tokio::sync::Notify,
    /// Set to false when the subscription is dropped.
    active: AtomicBool,
    /// Read at fire time: a value is written only while this says true.
    accept: Option<Box<dyn Fn() -> bool + Send + Sync>>,
}

/// Element kind of an octet or array payload the mailbox keeps by copy.
#[derive(Clone, Copy)]
enum PayloadKind {
    Octet,
    I8,
    I16,
    I32,
    I64,
    U64,
    F32,
    F64,
}

/// A mailbox's one value. The octet / array payload of a value is copied into
/// `buf`, which is sized at registration, and is not held by `Arc`: an
/// overwrite on the notify thread then drops nothing the notifier owns the
/// last reference to, so it frees nothing under the record's hold. This is C's
/// `interruptCallback` memcpy into a buffer allocated at init, with the length
/// clamped to it (devAsynOctet.c:489/:500/:535; arrays devAsynXXXArray.cpp:415).
struct Slot {
    /// Everything but the payload; `value` is `Undefined` while `payload`
    /// is `Some`.
    value: Option<InterruptValue>,
    payload: Option<(PayloadKind, usize)>,
    buf: Vec<u8>,
}

/// Copy as many whole elements of `src` as fit in `buf`; returns the count.
fn store_elems<T: Copy, const N: usize>(
    buf: &mut [u8],
    src: &[T],
    to_bytes: impl Fn(T) -> [u8; N],
) -> usize {
    let n = src.len().min(buf.len() / N);
    for (chunk, &x) in buf.as_chunks_mut::<N>().0.iter_mut().zip(&src[..n]) {
        chunk.copy_from_slice(&to_bytes(x));
    }
    n
}

fn load_elems<T, const N: usize>(
    buf: &[u8],
    n: usize,
    from_bytes: impl Fn([u8; N]) -> T,
) -> Arc<[T]> {
    buf.as_chunks::<N>()
        .0
        .iter()
        .take(n)
        .map(|c| from_bytes(*c))
        .collect()
}

impl Slot {
    fn new(payload_bytes: usize) -> Self {
        Self {
            value: None,
            payload: None,
            buf: vec![0; payload_bytes],
        }
    }

    fn is_some(&self) -> bool {
        self.value.is_some()
    }

    /// Allocates nothing for a scalar or a payload that fits `buf`.
    fn put(&mut self, v: &InterruptValue) {
        let buf = &mut self.buf[..];
        let (payload, value) = match &v.value {
            ParamValue::Octet(b) => {
                // C: `if (len >= valSize) len = valSize-1` (devAsynOctet.c:489).
                let n = b.len().min(buf.len().saturating_sub(1));
                buf[..n].copy_from_slice(&b[..n]);
                (Some((PayloadKind::Octet, n)), ParamValue::Undefined)
            }
            ParamValue::Int8Array(a) => (
                Some((PayloadKind::I8, store_elems(buf, a, i8::to_ne_bytes))),
                ParamValue::Undefined,
            ),
            ParamValue::Int16Array(a) => (
                Some((PayloadKind::I16, store_elems(buf, a, i16::to_ne_bytes))),
                ParamValue::Undefined,
            ),
            ParamValue::Int32Array(a) => (
                Some((PayloadKind::I32, store_elems(buf, a, i32::to_ne_bytes))),
                ParamValue::Undefined,
            ),
            ParamValue::Int64Array(a) => (
                Some((PayloadKind::I64, store_elems(buf, a, i64::to_ne_bytes))),
                ParamValue::Undefined,
            ),
            ParamValue::UInt64Array(a) => (
                Some((PayloadKind::U64, store_elems(buf, a, u64::to_ne_bytes))),
                ParamValue::Undefined,
            ),
            ParamValue::Float32Array(a) => (
                Some((PayloadKind::F32, store_elems(buf, a, f32::to_ne_bytes))),
                ParamValue::Undefined,
            ),
            ParamValue::Float64Array(a) => (
                Some((PayloadKind::F64, store_elems(buf, a, f64::to_ne_bytes))),
                ParamValue::Undefined,
            ),
            other => (None, other.clone()),
        };
        self.payload = payload;
        self.value = Some(InterruptValue {
            value,
            ..v.clone_meta()
        });
    }

    /// The consumer's side: the payload gets its `Arc` here, off the notify
    /// thread.
    fn take(&mut self) -> Option<InterruptValue> {
        let mut v = self.value.take()?;
        if let Some((kind, n)) = self.payload.take() {
            let b = &self.buf[..];
            v.value = match kind {
                PayloadKind::Octet => ParamValue::Octet(Arc::from(&b[..n])),
                PayloadKind::I8 => ParamValue::Int8Array(load_elems(b, n, i8::from_ne_bytes)),
                PayloadKind::I16 => ParamValue::Int16Array(load_elems(b, n, i16::from_ne_bytes)),
                PayloadKind::I32 => ParamValue::Int32Array(load_elems(b, n, i32::from_ne_bytes)),
                PayloadKind::I64 => ParamValue::Int64Array(load_elems(b, n, i64::from_ne_bytes)),
                PayloadKind::U64 => ParamValue::UInt64Array(load_elems(b, n, u64::from_ne_bytes)),
                PayloadKind::F32 => ParamValue::Float32Array(load_elems(b, n, f32::from_ne_bytes)),
                PayloadKind::F64 => ParamValue::Float64Array(load_elems(b, n, f64::from_ne_bytes)),
            };
        }
        Some(v)
    }
}

impl InterruptValue {
    /// Every field but `value`, which is `Undefined`.
    fn clone_meta(&self) -> InterruptValue {
        InterruptValue {
            reason: self.reason,
            addr: self.addr,
            value: ParamValue::Undefined,
            timestamp: self.timestamp,
            uint32_changed_mask: self.uint32_changed_mask,
            aux_status: self.aux_status,
            alarm_status: self.alarm_status,
            alarm_severity: self.alarm_severity,
            iface: self.iface,
        }
    }
}

// ---------------------------------------------------------------------------
// Synchronous callback subscription (averaging device support)
// ---------------------------------------------------------------------------

/// A synchronous interrupt callback, invoked INLINE inside `notify()` for every
/// matching value — no coalescing, no channel, no sample loss. This is the
/// faithful analogue of C `registerInterruptUser`: the driver calls the
/// callback directly per sample (asynPortDriver.cpp). Required by averaging
/// device support (`asynInt32Average` / `asynFloat64Average`), which must
/// accumulate EVERY sample (C `interruptCallbackAverage` does `sum += value`
/// per callback, devAsynInt32.c:665-666). The mailbox path coalesces rapid
/// updates to the latest, and the broadcast path drops on lag — either would
/// silently corrupt the mean, so averaging cannot use them.
struct SyncCallback {
    filter: InterruptFilter,
    callback: Box<dyn Fn(&InterruptValue) + Send + Sync>,
    active: AtomicBool,
}

/// RAII handle for a synchronous interrupt callback. Dropping it deactivates
/// the callback and removes it from the shared list (mirrors
/// [`InterruptSubscription`]).
pub struct SyncCallbackSubscription {
    callback: Arc<SyncCallback>,
    state: Arc<InterruptSharedState>,
}

impl Drop for SyncCallbackSubscription {
    fn drop(&mut self) {
        self.callback.active.store(false, Ordering::Release);
        let mut list = self.state.sync_callbacks.lock();
        let kept: Arc<[Arc<SyncCallback>]> = list
            .iter()
            .filter(|c| c.active.load(Ordering::Relaxed))
            .cloned()
            .collect();
        *list = kept;
    }
}

/// Receiver for a filtered interrupt subscription.
///
/// Uses a per-subscriber mailbox instead of broadcast+filter task.
/// Intermediate updates are coalesced: if the consumer is slow, only the latest
/// value is preserved. This eliminates broadcast Lagged errors entirely.
pub struct InterruptReceiver {
    mailbox: Arc<SubscriptionMailbox>,
}

impl InterruptReceiver {
    /// Wait for the next interrupt value matching this subscription's filter.
    /// Returns `None` when the subscription is cancelled (dropped).
    pub async fn recv(&mut self) -> Option<InterruptValue> {
        loop {
            // Register wakeup interest BEFORE checking the slot.
            // This avoids the race where notify_one fires between our check and await.
            let notified = self.mailbox.wakeup.notified();

            // Check if a value is already waiting.
            if let Some(value) = self.mailbox.latest.lock().take() {
                return Some(value);
            }
            // Check if subscription was cancelled.
            if !self.mailbox.active.load(Ordering::Acquire) {
                return None;
            }

            // No value yet — wait for wakeup.
            notified.await;
        }
    }
}

/// RAII subscription handle. Dropping this cancels the subscription.
pub struct InterruptSubscription {
    mailbox: Arc<SubscriptionMailbox>,
    state: Arc<InterruptSharedState>,
}

impl Drop for InterruptSubscription {
    fn drop(&mut self) {
        self.mailbox.active.store(false, Ordering::Release);
        // Wake consumer so it sees active=false and returns None.
        self.mailbox.wakeup.notify_one();
        // Remove from subscription list.
        self.state
            .mailboxes
            .lock()
            .retain(|s| s.active.load(Ordering::Relaxed));
    }
}

// ---------------------------------------------------------------------------
// Shared state between InterruptManager instances (driver ↔ PortHandle)
// ---------------------------------------------------------------------------

/// Shared interrupt infrastructure. Both the driver's InterruptManager and the
/// PortHandle's InterruptManager reference the same `InterruptSharedState` so
/// that subscribers registered on either side receive notifications.
pub struct InterruptSharedState {
    /// Broadcast channel for unfiltered subscribers (subscribe_async).
    /// Kept for backward compatibility with transport layer and tests.
    async_tx: broadcast::Sender<InterruptValue>,
    /// Mailbox-based subscriptions for filtered subscribers (I/O Intr records).
    mailboxes: parking_lot::Mutex<Vec<Arc<SubscriptionMailbox>>>,
    /// Synchronous callbacks invoked inline in `notify()` (averaging device
    /// support — every sample, no coalescing). See [`SyncCallback`].
    ///
    /// An immutable list replaced whole at register/unregister, so `dispatch`
    /// clones one `Arc` under the lock and allocates nothing.
    sync_callbacks: parking_lot::Mutex<Arc<[Arc<SyncCallback>]>>,
    /// Total number of notify() calls.
    notify_count: AtomicU64,
    /// Number of times a mailbox value was overwritten before the consumer read it.
    coalesce_count: AtomicU64,
    /// The `interruptAccept` of the database that owns this port, set once
    /// at registration — see [`InterruptManager::accepting`]. A single cell:
    /// the flush path reads it with one atomic load, takes no lock and
    /// frees nothing.
    owner: std::sync::OnceLock<Arc<epics_libcom_rs::runtime::interrupt_accept::InterruptGate>>,
}

// ---------------------------------------------------------------------------
// InterruptManager
// ---------------------------------------------------------------------------

/// Manages interrupt/callback delivery.
///
/// Two delivery paths:
/// - **Filtered subscriptions** (I/O Intr records): mailbox-based, no data loss.
///   `register_interrupt_user(, 4096)` creates a per-subscriber mailbox that stores
///   the latest matching value. Intermediate updates are coalesced.
/// - **Unfiltered subscriptions** (transport, tests): broadcast-based.
///   `subscribe_async()` returns a broadcast receiver for backward compatibility.
pub struct InterruptManager {
    state: Arc<InterruptSharedState>,
}

impl InterruptManager {
    pub fn new(async_capacity: usize) -> Self {
        let (async_tx, _) = broadcast::channel(async_capacity);
        Self {
            state: Arc::new(InterruptSharedState {
                async_tx,
                mailboxes: parking_lot::Mutex::new(Vec::new()),
                sync_callbacks: parking_lot::Mutex::new(Arc::from([])),
                notify_count: AtomicU64::new(0),
                coalesce_count: AtomicU64::new(0),
                owner: std::sync::OnceLock::new(),
            }),
        }
    }

    /// Create an InterruptManager sharing the same state as another.
    /// Used by `create_port_runtime` so the PortHandle and the driver share
    /// the same subscription list and broadcast channel.
    pub fn from_shared_state(state: Arc<InterruptSharedState>) -> Self {
        Self { state }
    }

    /// A manager that is owned from construction, for a driver a test drives
    /// by hand with no registry: it replaces the base's own manager before
    /// anything subscribes. Such a manager is already owned, so
    /// `register_port` refuses its handle ([`crate::error::AsynError::PortAlreadyOwned`]).
    pub fn owned_by(
        async_capacity: usize,
        gate: Arc<epics_libcom_rs::runtime::interrupt_accept::InterruptGate>,
    ) -> Self {
        let this = Self::new(async_capacity);
        let _ = this.state.owner.set(gate);
        this
    }

    /// Name the database that owns this port: the one `interruptAccept` it
    /// reads. First owner wins; returns whether `gate` became the owner.
    /// Only `registry::register_port` sets it, once, and refuses a port that
    /// already has one.
    pub(crate) fn set_owner(
        &self,
        gate: Arc<epics_libcom_rs::runtime::interrupt_accept::InterruptGate>,
    ) -> bool {
        self.state.owner.set(gate).is_ok()
    }

    /// C `interruptAccept` as this port reads it: its owner's gate alone. A
    /// record of another database on this port does not add a second
    /// reading — C has one `interruptAccept` per port's IOC, and two gates
    /// let one database's open consume the changed flags before the owner's
    /// own records could take them. A port with no owner (between
    /// construction and registration) reads closed, which keeps a seed
    /// written before registration.
    pub fn accepting(&self) -> bool {
        self.state.owner.get().is_some_and(|g| g.is_open())
    }

    /// Get the shared state for cross-manager sharing.
    pub fn shared_state(&self) -> Arc<InterruptSharedState> {
        self.state.clone()
    }

    /// Create an InterruptManager sharing an existing broadcast sender.
    /// **Deprecated**: prefer `from_shared_state` which also shares mailbox subscriptions.
    /// Kept for backward compatibility.
    pub fn from_broadcast_sender(sender: broadcast::Sender<InterruptValue>) -> Self {
        Self {
            state: Arc::new(InterruptSharedState {
                async_tx: sender,
                mailboxes: parking_lot::Mutex::new(Vec::new()),
                sync_callbacks: parking_lot::Mutex::new(Arc::from([])),
                notify_count: AtomicU64::new(0),
                coalesce_count: AtomicU64::new(0),
                owner: std::sync::OnceLock::new(),
            }),
        }
    }

    /// Subscribe for async interrupt delivery (unfiltered, broadcast-based).
    /// Multiple subscribers OK. Used by transport layer and tests.
    pub fn subscribe_async(&self) -> broadcast::Receiver<InterruptValue> {
        self.state.async_tx.subscribe()
    }

    /// Clone the broadcast sender for sharing.
    pub fn broadcast_sender(&self) -> broadcast::Sender<InterruptValue> {
        self.state.async_tx.clone()
    }

    /// Register a synchronous interrupt callback (averaging device support).
    ///
    /// The callback is invoked INLINE inside every matching [`notify`](Self::notify)
    /// — synchronously, on the notifying thread, with no coalescing and no
    /// channel. This is the faithful analogue of C `registerInterruptUser`
    /// (asynPortDriver.cpp), required where the consumer must observe every
    /// sample (e.g. `interruptCallbackAverage`'s `sum += value`). Returns an
    /// RAII [`SyncCallbackSubscription`]; dropping it unregisters the callback.
    /// The callback must be cheap (it runs on the port-actor thread) and must
    /// not call back into the interrupt system.
    pub fn register_sync_callback<F>(
        &self,
        filter: InterruptFilter,
        callback: F,
    ) -> SyncCallbackSubscription
    where
        F: Fn(&InterruptValue) + Send + Sync + 'static,
    {
        let cb = Arc::new(SyncCallback {
            filter,
            callback: Box::new(callback),
            active: AtomicBool::new(true),
        });
        {
            let mut list = self.state.sync_callbacks.lock();
            let next: Arc<[Arc<SyncCallback>]> = list.iter().cloned().chain([cb.clone()]).collect();
            *list = next;
        }
        SyncCallbackSubscription {
            callback: cb,
            state: self.state.clone(),
        }
    }

    /// Send an interrupt to all subscribers (both broadcast and mailbox),
    /// filtered by reason + addr + interface + UInt32Digital changed-bit mask.
    ///
    /// This is the rule for the **typed scalar / array** interfaces, whose C
    /// interrupt lists are keyed by reason. The octet interfaces are not: they
    /// go through [`notify_octet`](Self::notify_octet), which names its fan-out
    /// rule explicitly.
    pub fn notify(&self, value: InterruptValue) {
        self.dispatch(value, |f, v| f.matches(v));
    }

    /// Send an **octet** interrupt under one of C's two octet fan-out rules
    /// (see [`OctetFanOut`]). Neither rule consults `reason`.
    pub fn notify_octet(&self, rule: OctetFanOut, value: InterruptValue) {
        self.dispatch(value, |f, _| f.accepts_octet(rule));
    }

    /// The single delivery path — sync callbacks, then mailboxes, then the
    /// legacy broadcast — with the caller's fan-out rule as the only variable.
    /// Every emitter routes through here, so a new rule cannot come with its own
    /// half-implemented delivery.
    fn dispatch(
        &self,
        value: InterruptValue,
        pass: impl Fn(&InterruptFilter, &InterruptValue) -> bool,
    ) {
        self.state.notify_count.fetch_add(1, Ordering::Relaxed);

        // Synchronous callbacks (averaging device support): invoke inline for
        // every matching value, no coalescing — C registerInterruptUser. Clone
        // the list's `Arc` under the lock, then invoke after releasing it so a
        // callback cannot block concurrent register/drop. `notify` runs under
        // a record's hold: nothing here may allocate.
        let sync_cbs = self.state.sync_callbacks.lock().clone();
        for cb in sync_cbs.iter() {
            if !cb.active.load(Ordering::Relaxed) {
                continue;
            }
            if pass(&cb.filter, &value) {
                (cb.callback)(&value);
            }
        }

        // Deliver to mailbox subscribers (filtered, coalescing).
        let subs = self.state.mailboxes.lock();
        for sub in subs.iter() {
            if !sub.active.load(Ordering::Relaxed) {
                continue;
            }
            if !pass(&sub.filter, &value) {
                continue;
            }
            if sub.accept.as_ref().is_some_and(|accept| !accept()) {
                continue;
            }
            let mut slot = sub.latest.lock();
            if slot.is_some() {
                self.state.coalesce_count.fetch_add(1, Ordering::Relaxed);
            }
            slot.put(&value);
            drop(slot);
            sub.wakeup.notify_one();
        }
        drop(subs);

        // Deliver to broadcast subscribers (unfiltered, legacy).
        let _ = self.state.async_tx.send(value);
    }

    /// Register a filtered interrupt subscription using the mailbox model.
    ///
    /// Returns an RAII `InterruptSubscription` (dropping it unsubscribes) and an
    /// `InterruptReceiver` for receiving matching interrupts.
    ///
    /// Unlike the broadcast-based approach, this **never drops values** due to
    /// channel pressure. If the consumer is slow, intermediate updates are
    /// coalesced (latest value preserved, coalesce_count incremented).
    ///
    /// `payload_bytes` is the most an octet or array value this client can
    /// carry — the record's `valSize` (octet) or `NELM` × element size
    /// (arrays), which C allocates at init and clamps to before its memcpy
    /// (devAsynOctet.c:489, devAsynXXXArray.cpp:415/:435). A longer value is
    /// delivered clamped: an octet keeps `payload_bytes - 1` bytes (C
    /// reserves the NUL), an array its first `payload_bytes / size_of::<T>()`
    /// elements. A client that only receives scalars passes 0.
    pub fn register_interrupt_user(
        &self,
        filter: InterruptFilter,
        payload_bytes: usize,
    ) -> (InterruptSubscription, InterruptReceiver) {
        self.register_mailbox(filter, payload_bytes, None)
    }

    /// [`register_interrupt_user`](Self::register_interrupt_user) whose
    /// mailbox is written only while `accept` returns true, read when the
    /// callback fires. A value fired while it says false is dropped, not
    /// held for a later drain — C's `interruptAccept` return at callback
    /// time. `accept` runs inline in `notify`: it must not block or allocate.
    /// `payload_bytes` is as for [`register_interrupt_user`](Self::register_interrupt_user).
    pub fn register_interrupt_user_when(
        &self,
        filter: InterruptFilter,
        payload_bytes: usize,
        accept: impl Fn() -> bool + Send + Sync + 'static,
    ) -> (InterruptSubscription, InterruptReceiver) {
        self.register_mailbox(filter, payload_bytes, Some(Box::new(accept)))
    }

    fn register_mailbox(
        &self,
        filter: InterruptFilter,
        payload_bytes: usize,
        accept: Option<Box<dyn Fn() -> bool + Send + Sync>>,
    ) -> (InterruptSubscription, InterruptReceiver) {
        let mailbox = Arc::new(SubscriptionMailbox {
            filter,
            latest: parking_lot::Mutex::new(Slot::new(payload_bytes)),
            wakeup: tokio::sync::Notify::new(),
            active: AtomicBool::new(true),
            accept,
        });
        self.state.mailboxes.lock().push(mailbox.clone());
        (
            InterruptSubscription {
                mailbox: mailbox.clone(),
                state: self.state.clone(),
            },
            InterruptReceiver { mailbox },
        )
    }

    /// The filter of every live interrupt client, for `asynReport` — C's
    /// `reportInterrupt` walks each interface's `interruptList` and prints one
    /// line per registered client (asynPortDriver.cpp:1870-1894), which is the
    /// `details >= 3` block of `asynPortDriver::report` (:3695-3708).
    ///
    /// Rust keeps one list, with the interface as a field of the filter rather
    /// than as the identity of the list, so one walk answers what C's twelve
    /// `reportInterrupt` calls answer.
    pub fn clients(&self) -> Vec<InterruptFilter> {
        self.state
            .mailboxes
            .lock()
            .iter()
            .filter(|m| m.active.load(Ordering::Acquire))
            .map(|m| m.filter.clone())
            .collect()
    }

    /// True if any active mailbox subscription would receive a value fired for
    /// `(reason, addr, iface)`.
    ///
    /// A polling driver (e.g. Modbus `readPoller`) uses this to skip the cost of
    /// decoding+firing an interface that no record is bound to — mirroring C,
    /// where an empty interrupt list means the per-element `readPlcInt32` /
    /// `readPlcFloat` loop never runs (drvModbusAsyn.cpp:1825-1854). This gates
    /// only the expensive whole-block ARRAY decode, and array interfaces are
    /// bound exclusively through coalescing mailbox subscriptions — so only
    /// `mailboxes` are consulted. Sync-callback bindings (averaging /
    /// time-series) are scalar-only and never bind an array interface, and
    /// broadcast subscribers (`subscribe_async`, transports/tests) are
    /// observers, not bindings; neither needs to gate an array decode. (For
    /// *which offsets to poll at all*, both mailbox and sync-callback bindings
    /// matter — see [`subscribed_bindings`].)
    ///
    /// [`subscribed_bindings`]: Self::subscribed_bindings
    pub fn has_subscriber(&self, reason: usize, addr: i32, iface: InterfaceType) -> bool {
        self.state
            .mailboxes
            .lock()
            .iter()
            .any(|s| s.active.load(Ordering::Relaxed) && s.filter.accepts(reason, addr, iface))
    }

    /// The distinct `(reason, addr)` pairs of every active record binding —
    /// both coalescing mailbox subscriptions (I/O-Intr records) AND synchronous
    /// callbacks (averaging / time-series device support) — that pins a
    /// concrete reason AND address.
    ///
    /// A polling driver (Modbus `readPoller`) drives its fire set directly from
    /// this — exactly C's model, where `readPoller` fires every record on the
    /// interrupt list (populated at `registerInterruptUser`), with no
    /// dependence on whether the record was ever read. The interrupt registry
    /// is the single owner of "which records want a fire"; the driver holds no
    /// parallel set to keep in sync.
    ///
    /// BOTH subscription kinds are record bindings and must be enumerated: an
    /// averaging ai (`asynInt32Average`) or a time-series waveform registers
    /// only a [`register_sync_callback`], no mailbox, so a mailbox-only set
    /// would never poll it and the record would silently get zero samples.
    /// Only the broadcast path (`subscribe_async`, used by transports/tests) is
    /// excluded — it is an observer, not a record binding. A wildcard
    /// subscription (reason or addr `None`) names no single offset to poll and
    /// is skipped; every record binding sets both (the asyn device support
    /// fills `reason`/`addr` from its link), so none are lost.
    ///
    /// [`register_sync_callback`]: Self::register_sync_callback
    pub fn subscribed_bindings(&self) -> Vec<(usize, i32)> {
        let concrete = |f: &InterruptFilter| match (f.reason, f.addr) {
            (Some(r), Some(a)) => Some((r, a)),
            _ => None,
        };
        // Snapshot each list's concrete bindings under its own lock, then dedup
        // outside the locks (a callback never re-enters these).
        let mailbox_pairs: Vec<(usize, i32)> = {
            let g = self.state.mailboxes.lock();
            g.iter()
                .filter(|s| s.active.load(Ordering::Relaxed))
                .filter_map(|s| concrete(&s.filter))
                .collect()
        };
        let sync_pairs: Vec<(usize, i32)> = {
            let g = self.state.sync_callbacks.lock();
            g.iter()
                .filter(|c| c.active.load(Ordering::Relaxed))
                .filter_map(|c| concrete(&c.filter))
                .collect()
        };
        let mut out: Vec<(usize, i32)> = Vec::new();
        for p in mailbox_pairs.into_iter().chain(sync_pairs) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
        out
    }

    // --- Metrics ---

    /// Total number of notify() calls since creation.
    pub fn notify_count(&self) -> u64 {
        self.state.notify_count.load(Ordering::Relaxed)
    }

    /// Number of times a mailbox value was overwritten before the consumer read it.
    /// High coalesce count at moderate frame rates indicates consumer backpressure.
    pub fn coalesce_count(&self) -> u64 {
        self.state.coalesce_count.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    /// Boundary — a port follows its owner's gate only; one with none is
    /// closed whatever other databases do, and a second owner is refused.
    #[test]
    fn a_port_follows_its_owner_alone_and_an_ownerless_one_is_closed() {
        use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;

        let ownerless = InterruptManager::new(4);
        let owned = InterruptManager::new(4);
        let mine = InterruptGate::new();
        assert!(owned.set_owner(mine.clone()));
        let elsewhere = InterruptGate::opened();

        assert!(!ownerless.accepting(), "ownerless: closed");
        assert!(!owned.accepting(), "owned: its owner is closed");
        assert!(!owned.set_owner(elsewhere.clone()), "first owner wins");
        assert!(!owned.accepting(), "another open gate adds nothing");
        mine.set(true);
        assert!(owned.accepting());
        mine.set(false);
        assert!(!owned.accepting());
    }

    use super::*;

    #[tokio::test]
    async fn test_async_subscribe_receive() {
        let im = InterruptManager::new(16);
        let mut rx = im.subscribe_async();
        im.notify(InterruptValue {
            reason: 1,
            addr: 0,
            value: ParamValue::Float64(3.14),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });
        let v = rx.recv().await.unwrap();
        assert_eq!(v.reason, 1);
    }

    #[tokio::test]
    async fn test_async_multiple_subscribers() {
        let im = InterruptManager::new(16);
        let mut rx1 = im.subscribe_async();
        let mut rx2 = im.subscribe_async();
        im.notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: ParamValue::Int32(99),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });
        let v1 = rx1.recv().await.unwrap();
        let v2 = rx2.recv().await.unwrap();
        assert_eq!(v1.reason, 0);
        assert_eq!(v2.reason, 0);
    }

    #[tokio::test]
    async fn test_register_interrupt_user_filter_by_reason() {
        let im = InterruptManager::new(16);
        let (_sub, mut rx) = im.register_interrupt_user(
            InterruptFilter {
                reason: Some(1),
                addr: None,
                ..Default::default()
            },
            4096,
        );

        // Send reason 0 — should NOT be received
        im.notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: ParamValue::Int32(10),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });

        // Send reason 1 — should be received
        im.notify(InterruptValue {
            reason: 1,
            addr: 0,
            value: ParamValue::Int32(20),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });

        let v = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v.reason, 1);
        if let ParamValue::Int32(n) = v.value {
            assert_eq!(n, 20);
        } else {
            panic!("expected Int32");
        }
    }

    #[tokio::test]
    async fn test_register_interrupt_user_filter_by_addr() {
        let im = InterruptManager::new(16);
        let (_sub, mut rx) = im.register_interrupt_user(
            InterruptFilter {
                reason: None,
                addr: Some(3),
                ..Default::default()
            },
            4096,
        );

        im.notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: ParamValue::Int32(1),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });
        im.notify(InterruptValue {
            reason: 0,
            addr: 3,
            value: ParamValue::Int32(2),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });

        let v = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v.addr, 3);
    }

    #[tokio::test]
    async fn test_register_interrupt_user_no_filter() {
        let im = InterruptManager::new(16);
        let (_sub, mut rx) = im.register_interrupt_user(InterruptFilter::default(), 4096);

        im.notify(InterruptValue {
            reason: 5,
            addr: 2,
            value: ParamValue::Float64(1.5),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });

        let v = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v.reason, 5);
        assert_eq!(v.addr, 2);
    }

    #[tokio::test]
    async fn test_register_interrupt_user_drop_unsubscribes() {
        let im = InterruptManager::new(16);
        let (sub, mut rx) = im.register_interrupt_user(InterruptFilter::default(), 4096);

        // Drop subscription
        drop(sub);

        // Consumer should see None (subscription cancelled)
        let result = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        match result {
            Ok(None) => {} // cancelled — correct
            Err(_) => {}   // timed out — also acceptable
            Ok(Some(_)) => panic!("should not receive after unsubscribe"),
        }
    }

    #[tokio::test]
    async fn test_register_interrupt_user_multiple_subscribers() {
        let im = InterruptManager::new(16);
        let (_sub1, mut rx1) = im.register_interrupt_user(
            InterruptFilter {
                reason: Some(0),
                addr: None,
                ..Default::default()
            },
            4096,
        );
        let (_sub2, mut rx2) = im.register_interrupt_user(
            InterruptFilter {
                reason: Some(1),
                addr: None,
                ..Default::default()
            },
            4096,
        );

        im.notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: ParamValue::Int32(10),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });
        im.notify(InterruptValue {
            reason: 1,
            addr: 0,
            value: ParamValue::Int32(20),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });

        let v1 = tokio::time::timeout(std::time::Duration::from_millis(100), rx1.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v1.reason, 0);

        let v2 = tokio::time::timeout(std::time::Duration::from_millis(100), rx2.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v2.reason, 1);
    }

    #[test]
    fn test_notify_no_subscribers_no_panic() {
        let im = InterruptManager::new(16);
        im.notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: ParamValue::Int32(1),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });
    }

    #[tokio::test]
    async fn test_coalescing() {
        let im = InterruptManager::new(16);
        let (_sub, mut rx) = im.register_interrupt_user(
            InterruptFilter {
                reason: Some(0),
                ..Default::default()
            },
            4096,
        );

        // Send 3 values without consumer reading — should coalesce to latest.
        im.notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: ParamValue::Int32(1),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });
        im.notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: ParamValue::Int32(2),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });
        im.notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: ParamValue::Int32(3),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });

        // Consumer should see only the latest value (3).
        let v = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if let ParamValue::Int32(n) = v.value {
            assert_eq!(n, 3);
        } else {
            panic!("expected Int32");
        }

        // Coalesce count should be 2 (first write creates, next two overwrite).
        assert_eq!(im.coalesce_count(), 2);
    }

    #[tokio::test]
    async fn test_shared_state_between_managers() {
        let im1 = InterruptManager::new(16);
        let shared = im1.shared_state();
        let im2 = InterruptManager::from_shared_state(shared);

        // Subscribe via im2
        let (_sub, mut rx) = im2.register_interrupt_user(
            InterruptFilter {
                reason: Some(0),
                ..Default::default()
            },
            4096,
        );

        // Notify via im1 — subscriber should receive because state is shared
        im1.notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: ParamValue::Int32(42),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });

        let v = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v.reason, 0);
        if let ParamValue::Int32(n) = v.value {
            assert_eq!(n, 42);
        } else {
            panic!("expected Int32");
        }
    }

    /// Reproducer for asyn upstream issue #170: parallel callback
    /// processing overflows a fixed callback queue, dropping events.
    ///
    /// Our mailbox subscription model never drops: a burst of N events
    /// with a slow consumer collapses to a single observable value
    /// (the latest), and `coalesce_count` reflects the overwrites.
    /// This pins that contract — a regression toward bounded mpsc
    /// without coalesce slot would lose events under burst.
    #[tokio::test]
    async fn mailbox_burst_coalesces_no_drop() {
        let im = InterruptManager::new(16);
        let (_sub, mut rx) = im.register_interrupt_user(InterruptFilter::default(), 4096);

        let n: usize = 1000;
        for i in 0..n {
            im.notify(InterruptValue {
                reason: 0,
                addr: 0,
                value: ParamValue::Int32(i as i32),
                timestamp: SystemTime::now(),
                uint32_changed_mask: 0,
                ..Default::default()
            });
        }

        // The first (and only observable) recv must yield the LAST
        // event in the burst. All intermediates were coalesced.
        let v = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("recv must complete within 100ms")
            .expect("active subscription must yield a value");
        match v.value {
            ParamValue::Int32(x) => {
                assert_eq!(x, (n - 1) as i32, "must coalesce to the latest event")
            }
            other => panic!("expected Int32, got {other:?}"),
        }

        assert_eq!(im.notify_count(), n as u64);
        // n-1 overwrites of the slot before the consumer drained it.
        assert_eq!(im.coalesce_count(), (n - 1) as u64);
    }

    /// Per-interface routing (the modbus R54 fix): one reason+addr firing
    /// several interface-typed values delivers each only to the subscriber on
    /// that interface, and an untyped value reaches every subscriber.
    #[tokio::test]
    async fn notify_routes_typed_values_per_interface() {
        use crate::interfaces::InterfaceType;
        let im = InterruptManager::new(16);
        let (_si, mut rx_int) = im.register_interrupt_user(
            InterruptFilter {
                reason: Some(0),
                addr: Some(0),
                iface: Some(InterfaceType::Int32),
                ..Default::default()
            },
            4096,
        );
        let (_su, mut rx_uint) = im.register_interrupt_user(
            InterruptFilter {
                reason: Some(0),
                addr: Some(0),
                iface: Some(InterfaceType::UInt32Digital),
                uint32_mask: Some(0xFF),
            },
            4096,
        );

        // One reason fires two interface-typed values (the per-interface shape
        // `PortDriverBase::notify_interface_value` emits).
        im.notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: ParamValue::Int32(7),
            iface: Some(InterfaceType::Int32),
            ..Default::default()
        });
        im.notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: ParamValue::UInt32Digital(0xAB),
            iface: Some(InterfaceType::UInt32Digital),
            uint32_changed_mask: !0,
            ..Default::default()
        });

        let dur = std::time::Duration::from_millis(100);
        // The Int32 subscriber sees the Int32 value...
        let vi = tokio::time::timeout(dur, rx_int.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(vi.value, ParamValue::Int32(7)),
            "got {:?}",
            vi.value
        );
        // ...and the UInt32Digital subscriber sees the UInt32 value.
        let vu = tokio::time::timeout(dur, rx_uint.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(vu.value, ParamValue::UInt32Digital(0xAB)),
            "got {:?}",
            vu.value
        );
        // Each received ONLY its own interface's value (the other was filtered
        // out, not merely coalesced): a second recv has nothing to deliver.
        let short = std::time::Duration::from_millis(30);
        assert!(
            tokio::time::timeout(short, rx_int.recv()).await.is_err(),
            "Int32 subscriber must not receive the UInt32 fire"
        );
        assert!(
            tokio::time::timeout(short, rx_uint.recv()).await.is_err(),
            "UInt32 subscriber must not receive the Int32 fire"
        );

        // An untyped value (the `call_param_callbacks` path, iface None) still
        // reaches an interface-filtered subscriber — backward compatibility.
        im.notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: ParamValue::Int32(99),
            iface: None,
            ..Default::default()
        });
        let vi = tokio::time::timeout(dur, rx_int.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(vi.value, ParamValue::Int32(99)));
    }

    /// The two octet fan-out rules, at their boundaries. C's octet interrupt
    /// list is keyed by `addr` alone (asynOctetBase.c:202-210) — `reason` is
    /// never consulted — and the IP-server listener applies no key at all
    /// (drvAsynIPServerPort.c:388-398).
    ///
    /// Boundaries: reason-matches vs reason-differs (must not decide anything);
    /// addr-matches vs addr-differs under each rule; and a subscriber bound to a
    /// non-octet interface (never receives either).
    #[tokio::test]
    async fn octet_fan_out_ignores_reason_and_honours_the_emitter_rule() {
        use crate::interfaces::InterfaceType;
        let im = InterruptManager::new(16);

        let octet_sub = |reason: usize, addr: i32| {
            im.register_interrupt_user(
                InterruptFilter {
                    reason: Some(reason),
                    addr: Some(addr),
                    iface: Some(InterfaceType::Octet),
                    ..Default::default()
                },
                4096,
            )
        };
        // Same addr as the read, but a REASON the read does not carry — C
        // delivers to it; the port used to drop it.
        let (_s_other_reason, mut rx_other_reason) = octet_sub(7, 0);
        // Same addr, same reason.
        let (_s_same, mut rx_same) = octet_sub(0, 0);
        // A different addr.
        let (_s_other_addr, mut rx_other_addr) = octet_sub(0, 3);
        // A non-octet interface never sees an octet value.
        let (_s_int32, mut rx_int32) = im.register_interrupt_user(
            InterruptFilter {
                reason: Some(0),
                addr: Some(0),
                iface: Some(InterfaceType::Int32),
                ..Default::default()
            },
            4096,
        );

        let octet = |addr: i32, s: &str| InterruptValue {
            reason: 0,
            addr,
            value: ParamValue::Octet(s.as_bytes().into()),
            iface: Some(InterfaceType::Octet),
            ..Default::default()
        };
        let dur = std::time::Duration::from_millis(100);
        let short = std::time::Duration::from_millis(30);

        // --- ByAddr(0): the device-read rule.
        im.notify_octet(OctetFanOut::ByAddr(0), octet(0, "read"));
        for rx in [&mut rx_same, &mut rx_other_reason] {
            let v = tokio::time::timeout(dur, rx.recv()).await.unwrap().unwrap();
            assert!(matches!(v.value, ParamValue::Octet(ref s) if &**s == b"read"));
        }
        assert!(
            tokio::time::timeout(short, rx_other_addr.recv())
                .await
                .is_err(),
            "ByAddr must not reach a subscriber on another addr"
        );
        assert!(
            tokio::time::timeout(short, rx_int32.recv()).await.is_err(),
            "an octet value must not reach an Int32 subscriber"
        );

        // --- EveryUser: the IP-server listener rule. Every octet user, whatever
        // its addr — this is how a maxClients>1 server announces slot 3 to a
        // record registered on slot 0.
        im.notify_octet(OctetFanOut::EveryUser, octet(3, "srv:3"));
        for rx in [&mut rx_same, &mut rx_other_reason, &mut rx_other_addr] {
            let v = tokio::time::timeout(dur, rx.recv()).await.unwrap().unwrap();
            assert!(matches!(v.value, ParamValue::Octet(ref s) if &**s == b"srv:3"));
        }
        assert!(
            tokio::time::timeout(short, rx_int32.recv()).await.is_err(),
            "EveryUser is every OCTET user, not every subscriber"
        );
    }

    /// The subscriber-presence gate a polling driver uses to skip an interface
    /// with no record bound (R56 array fan-out): `has_subscriber` is true iff an
    /// active mailbox filter would accept a fire for that `(reason, addr, iface)`,
    /// an untyped (`iface == None`) subscriber accepts any interface, and a drop
    /// unregisters.
    #[tokio::test]
    async fn has_subscriber_reports_presence_per_iface() {
        use crate::interfaces::InterfaceType;
        let im = InterruptManager::new(16);
        // No subscribers yet.
        assert!(!im.has_subscriber(0, 0, InterfaceType::Int32Array));

        let (sub, _rx) = im.register_interrupt_user(
            InterruptFilter {
                reason: Some(0),
                addr: Some(0),
                iface: Some(InterfaceType::Int32Array),
                ..Default::default()
            },
            4096,
        );
        // Present for the matching tuple; absent for a different iface/addr/reason.
        assert!(im.has_subscriber(0, 0, InterfaceType::Int32Array));
        assert!(!im.has_subscriber(0, 0, InterfaceType::Float64Array));
        assert!(!im.has_subscriber(0, 1, InterfaceType::Int32Array));
        assert!(!im.has_subscriber(1, 0, InterfaceType::Int32Array));

        // An untyped subscriber (no iface filter) is present for every iface.
        let (sub2, _rx2) = im.register_interrupt_user(
            InterruptFilter {
                reason: Some(5),
                ..Default::default()
            },
            4096,
        );
        assert!(im.has_subscriber(5, 99, InterfaceType::Float64Array));

        // Dropping the subscription unregisters it.
        drop(sub);
        assert!(!im.has_subscriber(0, 0, InterfaceType::Int32Array));
        drop(sub2);
        assert!(!im.has_subscriber(5, 99, InterfaceType::Float64Array));
    }

    #[tokio::test]
    async fn subscribed_bindings_enumerates_distinct_concrete_pairs() {
        use crate::interfaces::InterfaceType;
        let im = InterruptManager::new(16);
        assert!(im.subscribed_bindings().is_empty());

        // Two ifaces on the SAME (reason, addr) collapse to one binding.
        let (s_a, _r0) = im.register_interrupt_user(
            InterruptFilter {
                reason: Some(2),
                addr: Some(7),
                iface: Some(InterfaceType::Int32Array),
                ..Default::default()
            },
            4096,
        );
        let (s_b, _r1) = im.register_interrupt_user(
            InterruptFilter {
                reason: Some(2),
                addr: Some(7),
                iface: Some(InterfaceType::Float64Array),
                ..Default::default()
            },
            4096,
        );
        // A distinct addr is its own binding.
        let (s_other, _r2) = im.register_interrupt_user(
            InterruptFilter {
                reason: Some(2),
                addr: Some(8),
                iface: Some(InterfaceType::Int32Array),
                ..Default::default()
            },
            4096,
        );
        let mut got = im.subscribed_bindings();
        got.sort();
        assert_eq!(got, vec![(2, 7), (2, 8)]);

        // A wildcard subscription (addr None) pins no offset and is skipped.
        let (s_wild, _r3) = im.register_interrupt_user(
            InterruptFilter {
                reason: Some(9),
                ..Default::default()
            },
            4096,
        );
        let mut got = im.subscribed_bindings();
        got.sort();
        assert_eq!(got, vec![(2, 7), (2, 8)]);

        // Sync-callback bindings (averaging / time-series) MUST be enumerated
        // too — they are record bindings, not observers. A sync callback at a
        // fresh (reason, addr) adds a binding; one co-located with a mailbox
        // binding dedups.
        let sc_new = im.register_sync_callback(
            InterruptFilter {
                reason: Some(3),
                addr: Some(1),
                ..Default::default()
            },
            |_| {},
        );
        let sc_dup = im.register_sync_callback(
            InterruptFilter {
                reason: Some(2),
                addr: Some(8),
                ..Default::default()
            },
            |_| {},
        );
        let mut got = im.subscribed_bindings();
        got.sort();
        assert_eq!(got, vec![(2, 7), (2, 8), (3, 1)]);

        // Dropping the sync callback removes its binding; the co-located one
        // leaves (2, 8) alive via the surviving mailbox subscription.
        drop(sc_new);
        let mut got = im.subscribed_bindings();
        got.sort();
        assert_eq!(got, vec![(2, 7), (2, 8)]);
        drop(sc_dup);
        let mut got = im.subscribed_bindings();
        got.sort();
        assert_eq!(got, vec![(2, 7), (2, 8)]);

        // The pair stays alive while either iface subscription survives.
        drop(s_a);
        let mut got = im.subscribed_bindings();
        got.sort();
        assert_eq!(got, vec![(2, 7), (2, 8)]);
        drop(s_b);
        let mut got = im.subscribed_bindings();
        got.sort();
        assert_eq!(got, vec![(2, 8)]);

        drop(s_other);
        drop(s_wild);
        assert!(im.subscribed_bindings().is_empty());
    }
}
