use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicU32;
use std::time::SystemTime;

use epics_base_rs::runtime::sync::mpsc;
use tokio::sync::Notify;
use tokio::sync::mpsc::error::TrySendError;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{DbFieldType, EpicsValue, decode_dbr};

use super::types::TransportCommand;

/// Producer routing decision for a value snapshot, computed
/// atomically against the pause flag (see [`CoalesceSlot::route_value`]).
pub(crate) enum ValueRoute {
    /// Coalesced into a slot cell (active overflow into `ready`, or a
    /// during-pause arrival into `gated`). No channel write — invisible
    /// to flow control.
    Slotted,
    /// `ready` empty and not paused — the caller should try the bounded
    /// channel (and fall back to [`CoalesceSlot::put_value`] on full).
    /// Boxed so this large variant doesn't bloat the others; the
    /// snapshot is already heap-backed.
    TryChannel(Box<Snapshot>),
}

/// Per-subscription overflow/pause buffer shared between the
/// coordinator (producer) and the [`super::MonitorHandle`] (consumer).
///
/// # State machine
///
/// Three value-holding cells, written by exactly one method
/// (`coalesce_value_locked` for values, `put_error` for errors) and
/// read by exactly one (`take_deliverable`):
///
/// - `error` — pending error; highest priority; bypasses pause.
/// - `ready` — the single latest value that is deliverable NOW.
/// - `gated` — the single latest value that arrived during the current
///   pause; withheld until resume.
///
/// # Invariants (each maintained by construction)
///
/// - **I1 — the consumer is invisible to the wire.** No cell here, and no
///   channel depth, feeds CA flow control. `EVENTS_OFF` / `EVENTS_ON` are
///   decided in `transport::read_loop` from OS socket-buffer occupancy
///   alone (C `tcpiiu.cpp:548-567`), so neither a client-side pause nor a
///   consumer that stops polling can freeze sibling subscriptions.
/// - **I2 — error priority.** `error` is its own cell; a value can never
///   overwrite or hide it, and `take_deliverable` yields it first.
/// - **I3 — pause gating is structural, not conditional.** The sole
///   invariant `gated.is_some() ⟹ paused` holds by construction:
///   `gated` is written only while paused (`coalesce_value_locked`),
///   and `resume` empties it by coalescing into `ready`. Consequently
///   `take_deliverable` needs NO pause check — it just yields `error`
///   then `ready`; `gated` is unreachable to the consumer until resume
///   promotes it. A value buffered before a pause sits in `ready` and
///   stays deliverable throughout the pause; a value that becomes
///   deliverable after a resume is already in `ready`, so a later pause
///   can never re-gate it. There is no "held survives resume" dual
///   meaning, hence no multi-pause edge cases.
///
/// Coalescing is uniform: when the consumer falls behind, only the
/// latest value survives — including across a pause boundary. (During a
/// pause the pre-pause `ready` value is still deliverable, so a consumer
/// that recv()s during the pause sees it; a consumer that does not is,
/// by definition, behind and gets the latest on resume — standard
/// monitor latest-value semantics.)
///
/// All cells + the pause flag live under one mutex, so producer and
/// consumer operations are each atomic against `resume`.
pub(crate) struct CoalesceSlot {
    inner: StdMutex<CoalesceInner>,
    notify: Notify,
}

struct CoalesceInner {
    error: Option<CaError>,
    ready: Option<Snapshot>,
    /// INVARIANT: `Some` ⟹ `paused`.
    gated: Option<Snapshot>,
    paused: bool,
}

impl CoalesceSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: StdMutex::new(CoalesceInner {
                error: None,
                ready: None,
                gated: None,
                paused: false,
            }),
            notify: Notify::new(),
        })
    }

    /// The SOLE writer of the value cells. While paused the latest value
    /// coalesces into `gated` (maintaining `gated.is_some() ⟹ paused`);
    /// otherwise into `ready`. Caller holds the lock.
    fn coalesce_value_locked(inner: &mut CoalesceInner, snapshot: Snapshot) {
        if inner.paused {
            inner.gated = Some(snapshot);
        } else {
            inner.ready = Some(snapshot);
        }
    }

    /// Route a value snapshot. Goes to the bounded channel ONLY when the
    /// slot is entirely empty AND not paused; any pending `error` or
    /// `ready` (or being paused) means the value must coalesce into the
    /// slot rather than jump ahead via the channel — a value must
    /// never overtake a buffered error, and order is preserved.
    /// (`gated` is empty when not paused by the invariant.) Out of flow
    /// control (I1).
    pub fn route_value(&self, snapshot: Snapshot) -> ValueRoute {
        let mut g = self.inner.lock().expect("CoalesceSlot mutex poisoned");
        if !g.paused && g.error.is_none() && g.ready.is_none() {
            return ValueRoute::TryChannel(Box::new(snapshot));
        }
        let paused = g.paused;
        Self::coalesce_value_locked(&mut g, snapshot);
        drop(g);
        // A value written while paused goes to `gated` (not deliverable
        // until resume), so no wake. Otherwise it's deliverable now.
        if !paused {
            self.notify.notify_one();
        }
        ValueRoute::Slotted
    }

    /// Overflow fallback after a full channel: coalesce the value into
    /// the slot. Out of flow control (I1).
    fn put_value(&self, snapshot: Snapshot) {
        let mut g = self.inner.lock().expect("CoalesceSlot mutex poisoned");
        let paused = g.paused;
        Self::coalesce_value_locked(&mut g, snapshot);
        drop(g);
        if !paused {
            self.notify.notify_one();
        }
    }

    /// Store an error in the dedicated error cell (I2). Latest error
    /// wins; never touches the value cells. Bypasses pause.
    fn put_error(&self, err: CaError) {
        let mut g = self.inner.lock().expect("CoalesceSlot mutex poisoned");
        g.error = Some(err);
        drop(g);
        self.notify.notify_one();
    }

    /// The SOLE reader of delivery priority: `error` then `ready`.
    /// There is NO pause check and NO `gated` branch — the gate is
    /// structural (I3): `gated` is unreachable here, and `resume`
    /// promotes it into `ready` when the pause ends.
    pub fn take_deliverable(&self) -> Option<CaResult<Snapshot>> {
        let mut g = self.inner.lock().expect("CoalesceSlot mutex poisoned");
        if let Some(err) = g.error.take() {
            return Some(Err(err));
        }
        g.ready.take().map(Ok)
    }

    /// Future that resolves on the next `notify` (deliverable write or
    /// `resume`).
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }

    /// Set/clear the pause flag. Returns the previous value.
    ///
    /// RESUME promotes any `gated` value into `ready` (coalesce: the
    /// during-pause value is the latest and supersedes an undrained
    /// pre-pause `ready`) and wakes `recv`. This is the SOLE place
    /// `gated` is emptied, so the invariant `gated.is_some() ⟹ paused`
    /// holds. Pause ENTRY needs no special handling — `gated` is already
    /// empty by the invariant.
    pub fn set_paused(&self, paused: bool) -> bool {
        let mut g = self.inner.lock().expect("CoalesceSlot mutex poisoned");
        let prev = g.paused;
        g.paused = paused;
        let resuming = prev && !paused;
        if resuming && g.gated.is_some() {
            g.ready = g.gated.take();
        }
        drop(g);
        if resuming {
            self.notify.notify_one();
        }
        prev
    }

    pub fn is_paused(&self) -> bool {
        self.inner
            .lock()
            .expect("CoalesceSlot mutex poisoned")
            .paused
    }

    /// Drop all three cells regardless of pause state. Used on disconnect
    /// so a stale snapshot/error can't outlive the circuit.
    fn clear(&self) {
        let mut g = self.inner.lock().expect("CoalesceSlot mutex poisoned");
        g.error = None;
        g.ready = None;
        g.gated = None;
    }

    /// Test-only: unconditional drain — error, then `ready`, then
    /// `gated`, ignoring pause.
    #[cfg(test)]
    fn take_raw(&self) -> Option<CaResult<Snapshot>> {
        let mut g = self.inner.lock().expect("CoalesceSlot mutex poisoned");
        if let Some(err) = g.error.take() {
            return Some(Err(err));
        }
        if let Some(v) = g.ready.take() {
            return Some(Ok(v));
        }
        g.gated.take().map(Ok)
    }
}

/// Outcome of `on_monitor_data` — the single signal that drives the
/// coordinator's delivery metrics.
///
/// None of these feed CA flow control (invariant I1) — that is decided
/// from socket occupancy in `transport::read_loop`, never from how far
/// behind the application is.
pub(crate) enum MonitorDeliveryOutcome {
    /// Written to the bounded channel.
    Queued,
    /// Buffered in the coalesce slot (overflow `ready`, during-pause
    /// `gated`, or overflow error). Diagnostic only.
    Slotted,
    /// Dropped because the consumer channel is closed (the application
    /// dropped its `MonitorHandle`). The only remaining drop case.
    Dropped,
    /// Filtered by client-side deadband (no action).
    Filtered,
    /// Subscription not found.
    NotFound,
}

pub(crate) struct SubscriptionRecord {
    pub subid: u32,
    pub cid: u32,
    pub data_type: Option<u16>,
    /// The resolved wire element count for the current connection: a
    /// positive clamped `-#` cap, or `0` for autosize (no cap). See
    /// [`resolve_subscription_count`].
    pub count: Option<u32>,
    /// The caller's requested element-count cap (`camonitor -#`), or
    /// `None` for autosize (wire count `0`, the standard CA-tool default).
    /// Unlike the resolved [`Self::count`] (which caches the wire count for
    /// the current connection), this is a persistent *preference* — carried
    /// across reconnects and re-resolved against the fresh native element
    /// count on each connect via [`resolve_subscription_count`] (a positive
    /// cap re-clamps per C `camonitor.c:169`; `None` stays autosize).
    /// Orthogonal to [`Self::type_user_supplied`]: the cap survives a
    /// `NativeTypeChanged` reset because it is the user's intent, not an
    /// auto-derived value.
    pub req_count: Option<u32>,
    /// `true` when `data_type`/`count` were chosen explicitly by the
    /// caller, `false` when they were auto-derived from the channel's
    /// native type at subscribe time. Auto-derived values must be
    /// re-derived when the IOC redefines the record (the channel
    /// reports `NativeTypeChanged` on reconnect); user-supplied values
    /// are preserved across reconnects. See `restore_for_channel`.
    pub type_user_supplied: bool,
    /// ENUM readback mode for this subscription: `Native` (keep
    /// `DBR_TIME_ENUM`), `Label` (the `camonitor` default `DBR_TIME_STRING`,
    /// C `camonitor.c:156-160`), or `Numeric` (`camonitor -n`
    /// `DBR_TIME_INT`, C `camonitor.c:158`). See
    /// [`super::EnumReadback`]. Carried here so the reconnect re-derivation
    /// in `restore_for_channel` re-applies the same ENUM substitution against
    /// the (possibly new) native type.
    pub enum_readback: super::EnumReadback,
    /// `camonitor -s` float-as-string preference. When `true`, a
    /// FLOAT/DOUBLE field's auto-derived monitor type is the
    /// `DBR_TIME_STRING` form so the server renders the value to a string
    /// at record precision (C `camonitor.c:162-166`). Carried here for the
    /// same reconnect re-derivation reason as `enum_as_string`; the ENUM
    /// substitution takes precedence (C `if (ENUM) … else if (float …)`).
    pub float_as_string: bool,
    pub mask: u16,
    pub server_addr: SocketAddr,
    /// the CA priority of the channel this subscription rides.
    /// Fixed at channel creation and preserved across reconnects (only
    /// `server_addr` can change). `(server_addr, priority)` is the
    /// circuit this subscription's flow control accounts against.
    pub priority: u8,
    pub callback_tx: mpsc::Sender<CaResult<Snapshot>>,
    /// "Latest pending" slot — see [`CoalesceSlot`]. Shared with the
    /// [`super::MonitorHandle`] so the consumer drains it after the bounded
    /// channel empties.
    pub coalesce_slot: Arc<CoalesceSlot>,
    pub needs_restore: bool,
    /// Client-side deadband: suppress callback if |new - old| < deadband.
    pub deadband: f64,
    /// Last delivered scalar value (for deadband filtering).
    pub last_value: Option<f64>,
    /// Diagnostic counter — number of overflow-coalesce events for
    /// this subscription. Mirrors `dbEvent.c::pevent->nreplace`.
    pub nreplace: u64,
}

pub(crate) struct SubscriptionRegistry {
    subscriptions: HashMap<u32, SubscriptionRecord>,
    /// monotonic `subid` source owned by the registry that
    /// holds the live subscriptions. [`Self::alloc_subid`] probes
    /// `subscriptions` so a counter wrapping through 2^32 cannot
    /// reissue a subid that an active subscription still uses — a
    /// stale `EVENT_ADD`/`EVENT_CANCEL` for the old subscription would
    /// otherwise be routed to the wrong record.
    next_subid: AtomicU32,
}

/// Resolve a subscription's wire element count from the caller's requested
/// cap and the channel's native element count. Single owner of the
/// `reqElems → wire count` rule for monitors.
///
/// "No cap" (`None`, or an explicit `0` — C treats `reqElems == 0` the
/// same) resolves to wire `count = 0`, the CA *autosize* request, NOT the
/// native capacity. With a zero count the server reports the record's
/// CURRENT element count in every event's response header and sends only
/// that many elements: `rsrv/camessage.c:504-509` reads `m_count == 0` as
/// "all available", then `:537-538` fetches the actual `item_count` and
/// `:563-568` rewrites the response header count to it (non-autosize pads
/// the tail instead). This is exactly what the standard CA tools request
/// with no `-#`: `camonitor.c:39,168-169` and `caget.c:200` default
/// `reqElems` to `0` and pass it straight to `ca_create_subscription` /
/// `ca_array_get_callback`, and ca-gateway subscribes with count `0`
/// (`gatePv.cc:765-774`). Requesting the native capacity instead makes a
/// dynamic waveform (`NORD < NELM`) deliver a max-capacity array with a
/// padded/stale tail.
///
/// A positive cap (`camonitor -#N`) is clamped to the native element count
/// (C `camonitor.c:169` `reqElems = reqElems > nElems ? nElems : reqElems`)
/// and sent as a concrete non-zero count, so an over-large `-#` requests the
/// native count rather than triggering autosize. Re-run on every connect
/// against the fresh native count, exactly as C runs the clamp inside its
/// connect handler.
pub(crate) fn resolve_subscription_count(req_count: Option<u32>, element_count: u32) -> u32 {
    match req_count {
        Some(n) if n > 0 => n.min(element_count),
        _ => 0,
    }
}

impl SubscriptionRegistry {
    pub fn new() -> Self {
        Self {
            subscriptions: HashMap::new(),
            next_subid: AtomicU32::new(1),
        }
    }

    /// Allocate a `subid` not currently held by any live subscription.
    /// Owner-side allocation: the coordinator (sole mutator of
    /// this registry) calls this when registering a new subscription,
    /// so the live-table probe and the subsequent [`Self::add`] happen
    /// in the same single-threaded context with no other allocator.
    pub fn alloc_subid(&self) -> u32 {
        crate::channel::alloc_nonzero_probe(&self.next_subid, |v| {
            self.subscriptions.contains_key(&v)
        })
    }

    /// Test-only: seed the next-subid counter to drive the wrap path
    /// deterministically.
    #[cfg(test)]
    pub fn seed_next_subid(&self, v: u32) {
        self.next_subid
            .store(v, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn add(&mut self, rec: SubscriptionRecord) {
        self.subscriptions.insert(rec.subid, rec);
    }

    pub fn remove(&mut self, subid: u32) -> Option<SubscriptionRecord> {
        self.subscriptions.remove(&subid)
    }

    /// Deliver a non-NORMAL monitor status (libca `pmiu->exception`
    /// path, `cac.cpp:973-977`) to the per-subscription callback as
    /// an `Err(CaError::ServerError(eca_status))`. Best-effort: the
    /// existing `try_deliver_err` helper silently drops the error
    /// if the receiver queue is full or closed.
    pub fn on_monitor_error(&mut self, subid: u32, eca_status: u32) -> MonitorDeliveryOutcome {
        let Some(rec) = self.subscriptions.get_mut(&subid) else {
            return MonitorDeliveryOutcome::NotFound;
        };
        try_deliver_err(rec, epics_base_rs::error::CaError::ServerError(eca_status))
    }

    pub fn on_monitor_data(
        &mut self,
        subid: u32,
        data_type: u16,
        count: u32,
        data: &[u8],
    ) -> MonitorDeliveryOutcome {
        let Some(rec) = self.subscriptions.get_mut(&subid) else {
            return MonitorDeliveryOutcome::NotFound;
        };
        let snapshot = if data_type <= 6 {
            let dbr_type = match DbFieldType::from_u16(data_type) {
                Ok(t) => t,
                Err(e) => {
                    return try_deliver_err(rec, e);
                }
            };
            match EpicsValue::from_bytes_array(dbr_type, data, count as usize) {
                Ok(value) => Snapshot::new(value, 0, 0, SystemTime::now()),
                Err(e) => {
                    return try_deliver_err(rec, e);
                }
            }
        } else {
            match decode_dbr(data_type, data, count as usize) {
                Ok(s) => s,
                Err(e) => {
                    return try_deliver_err(rec, e);
                }
            }
        };

        // Client-side deadband filtering (scalar values only)
        if rec.deadband > 0.0 {
            if let Some(new_val) = snapshot.value.to_f64() {
                if let Some(old_val) = rec.last_value {
                    if (new_val - old_val).abs() < rec.deadband {
                        return MonitorDeliveryOutcome::Filtered;
                    }
                }
                rec.last_value = Some(new_val);
            }
        }

        // Value routing, computed atomically against the pause flag.
        // Only a successful channel write counts toward flow control
        // (I1); every slot write is `Slotted` and out of band.
        match rec.coalesce_slot.route_value(snapshot) {
            // Held during pause, or overflow-coalesced while active —
            // either way the value is in the slot, not the channel.
            ValueRoute::Slotted => {
                rec.nreplace = rec.nreplace.saturating_add(1);
                MonitorDeliveryOutcome::Slotted
            }
            ValueRoute::TryChannel(snapshot) => match rec.callback_tx.try_send(Ok(*snapshot)) {
                Ok(()) => MonitorDeliveryOutcome::Queued,
                Err(TrySendError::Full(rejected)) => {
                    // Bounded channel full — coalesce into the slot
                    // instead of the pre-fix silent drop (which lost
                    // terminal transitions like DMOV 1→0 under load).
                    // Mirrors C `dbEvent.c::db_post_events` replace-last.
                    // The slot is out of flow control (I1); EVENTS_OFF
                    // already fired long ago (channel is full ≫ the
                    // threshold), so no flow-control bump here.
                    rec.nreplace = rec.nreplace.saturating_add(1);
                    let snap = rejected.expect("route_value only boxes Ok values");
                    rec.coalesce_slot.put_value(snap);
                    MonitorDeliveryOutcome::Slotted
                }
                Err(TrySendError::Closed(_)) => MonitorDeliveryOutcome::Dropped,
            },
        }
    }

    /// Mark all subscriptions for a given server's channels as needing restore.
    /// Returns the cids that were affected.
    ///
    /// also deliver one `Err(CaError::ServerError(ECA_DISCONN))`
    /// per affected subscription's callback channel — libca
    /// `cac::disconnectAllIO()` (`modules/ca/src/client/cac.cpp:678-698`)
    /// iterates every in-flight IO on the channel (including
    /// subscriptions) and fires `pNetIO->exception(... ECA_DISCONN ...)`.
    /// Pre-fix Rust silently flipped `needs_restore = true` and waited
    /// for reconnect, so a libca-style `MonitorHandle::recv()` saw
    /// nothing when the circuit died.
    /// The disconnect error goes ALWAYS into the error cell, never the
    /// bounded channel: it is delivered with priority and bypasses pause,
    /// and any items already in the channel stay there for the consumer to
    /// drain.
    pub fn mark_disconnected(&mut self, cids: &[u32]) {
        const ECA_DISCONN: u32 = 192; // protocol::ECA_DISCONN
        for rec in self.subscriptions.values_mut() {
            if cids.contains(&rec.cid) {
                rec.needs_restore = true;
                // Drop stale value cells, then park ECA_DISCONN in the
                // error cell.
                rec.coalesce_slot.clear();
                rec.coalesce_slot
                    .put_error(CaError::ServerError(ECA_DISCONN));
            }
        }
    }

    /// Generate restore commands for subscriptions tied to the given cid,
    /// using the new sid.
    /// Restore subscriptions after reconnect. Returns (restored, failed) counts.
    ///
    /// `native_changed` is `true` when this (re)connection reports a
    /// native DBR type different from the one observed before (the IOC
    /// redefined the record, or the channel reconnected to a different
    /// IOC). When set, auto-derived `data_type`/`count` are reset to
    /// `None` so they re-derive from the fresh `native_type`; subscriptions
    /// created with an explicit user-chosen type keep their type.
    #[allow(clippy::too_many_arguments)]
    pub fn restore_for_channel(
        &mut self,
        cid: u32,
        new_sid: u32,
        native_type: u16,
        element_count: u32,
        native_changed: bool,
        server_addr: std::net::SocketAddr,
        // Minor protocol version of the circuit this restore rides. C
        // resolves the wire count per request from it
        // (`subscr.getCount(guard, CA_V413(minorProtocolVersion))`,
        // `tcpiiu.cpp:1573`), so a reconnect to a peer with a different
        // version re-resolves rather than replaying the old wire count.
        peer_minor: u16,
        transport_tx: &mpsc::UnboundedSender<TransportCommand>,
    ) -> (u32, u32) {
        let mut restored = 0u32;
        let mut failed = 0u32;
        // Collect stale subids first (callback receiver dropped)
        let stale: Vec<u32> = self
            .subscriptions
            .values()
            .filter(|rec| rec.cid == cid && rec.needs_restore && rec.callback_tx.is_closed())
            .map(|rec| rec.subid)
            .collect();
        for subid in &stale {
            self.subscriptions.remove(subid);
            failed += 1;
        }
        for rec in self.subscriptions.values_mut() {
            if rec.cid == cid && rec.needs_restore {
                rec.needs_restore = false;
                rec.server_addr = server_addr;
                // The IOC redefined the record: a previously auto-derived
                // type/count is now stale and would decode monitor frames
                // against the wrong DBR type. Drop both so they re-derive
                // from the fresh native type/count below — the count
                // re-derivation re-clamps the `-#` cap to the new native
                // element count. User-supplied type/count are kept. Without
                // a native-type change, auto-derived type AND count stay
                // locked to their first-connect values (established
                // reconnect behaviour).
                if native_changed && !rec.type_user_supplied {
                    rec.data_type = None;
                    rec.count = None;
                }
                // Re-derive the DBR type from the fresh native type via the
                // shared subscribe-time owner, so the reconnect re-derivation
                // applies the identical substitution chain as the initial
                // subscribe: ENUM honours -n, else FLOAT/DOUBLE under -s
                // becomes DBR_TIME_STRING, else native TIME type
                // (C camonitor.c:155-166). native_type is the server-reported
                // CA wire type (0..6), so the `+ 14` fallback (only reached if
                // from_u16 fails) lands in the DBR_TIME range; Int64 (7)
                // cannot reach here.
                let derived = DbFieldType::from_u16(native_type)
                    .ok()
                    .map(|t| {
                        super::subscription_readback_dbr(t, rec.enum_readback, rec.float_as_string)
                    })
                    .unwrap_or(native_type + 14);
                let data_type = *rec.data_type.get_or_insert(derived);
                // Single owner of the auto-derived count seed: a `-#` cap is
                // clamped to the native element count, no cap resolves to
                // wire 0 (autosize) (`resolve_subscription_count`, C
                // `camonitor.c:168-169` / `caget.c:200`). Cached like
                // `data_type` via `get_or_insert`, so it is computed at first
                // connect and re-derived only when a native-type change
                // cleared it above — keeping count and type re-derivation
                // uniform. User-supplied counts (already `Some`) are kept.
                let count = *rec
                    .count
                    .get_or_insert(resolve_subscription_count(rec.req_count, element_count));
                // The cached `rec.count` is C's `netSubscription::count` (the
                // user's cap); the wire count is derived from it per request
                // via `getCount(allow_zero = CA_V413(minor))`
                // (`netIO.h:241-251`) and then bounded against what the
                // circuit can carry (`tcpiiu.cpp:1574-1585`).
                let wire_count = match crate::protocol::subscription_wire_count(
                    count,
                    element_count,
                    data_type,
                    peer_minor,
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        // C `nciu::resubscribe` (`nciu.cpp:535-541`) catches
                        // the throw, logs "CAC: failed to send subscription
                        // request during channel connect", and leaves the
                        // subscription uninstalled — the channel still
                        // connects.
                        tracing::error!(
                            subid = rec.subid,
                            error = %e,
                            "failed to send subscription request during channel connect"
                        );
                        rec.needs_restore = true;
                        continue;
                    }
                };
                let _ = transport_tx.send(TransportCommand::Subscribe {
                    sid: new_sid,
                    data_type,
                    count: wire_count,
                    subid: rec.subid,
                    mask: rec.mask,
                    server_addr,
                    priority: rec.priority,
                });
                restored += 1;
            }
        }
        (restored, failed)
    }

    /// Number of active subscriptions.
    #[allow(dead_code)]
    pub fn count(&self) -> usize {
        self.subscriptions.len()
    }

    /// Remove subscriptions whose callback receiver has been dropped.
    /// Returns the subids that were removed.
    ///
    /// Not currently called — channel drop sends ClearChannel to the IOC
    /// which cleans up server-side subscriptions automatically.
    #[allow(dead_code)]
    pub fn cleanup_closed(&mut self) -> Vec<u32> {
        let closed: Vec<u32> = self
            .subscriptions
            .iter()
            .filter(|(_, rec)| rec.callback_tx.is_closed())
            .map(|(&subid, _)| subid)
            .collect();
        for subid in &closed {
            self.subscriptions.remove(subid);
        }
        closed
    }

    /// Get subscription info for generating CANCEL commands
    pub fn get(&self, subid: u32) -> Option<&SubscriptionRecord> {
        self.subscriptions.get(&subid)
    }

    /// Get all subscriptions for a given cid
    pub fn for_cid(&self, cid: u32) -> Vec<u32> {
        self.subscriptions
            .iter()
            .filter(|(_, rec)| rec.cid == cid)
            .map(|(&subid, _)| subid)
            .collect()
    }
}

/// Deliver an error to the consumer. Errors bypass pause and use the
/// dedicated error slot (I2): they go to the bounded channel when there
/// is room, otherwise the sticky error slot, which a value can never
/// overwrite. The error slot does not displace a pending value — both can
/// be queued and the consumer's `take_deliverable` delivers the error first.
fn try_deliver_err(rec: &mut SubscriptionRecord, err: CaError) -> MonitorDeliveryOutcome {
    match rec.callback_tx.try_send(Err(err)) {
        Ok(()) => MonitorDeliveryOutcome::Queued,
        Err(TrySendError::Full(rejected)) => {
            // Channel full — park the error in its own slot. Recover the
            // rejected error rather than cloning.
            let e = match rejected {
                Err(e) => e,
                Ok(_) => unreachable!("we just sent an Err"),
            };
            rec.coalesce_slot.put_error(e);
            MonitorDeliveryOutcome::Slotted
        }
        Err(TrySendError::Closed(_)) => MonitorDeliveryOutcome::Dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> SocketAddr {
        "127.0.0.1:5064".parse().unwrap()
    }

    /// Builds a record plus its callback receiver. The caller must keep
    /// the returned receiver alive — `restore_for_channel` drops any
    /// subscription whose callback receiver has been closed.
    fn record(
        subid: u32,
        cid: u32,
        type_user_supplied: bool,
    ) -> (SubscriptionRecord, mpsc::Receiver<CaResult<Snapshot>>) {
        let (callback_tx, rx) = mpsc::channel(8);
        let rec = SubscriptionRecord {
            subid,
            cid,
            data_type: None,
            count: None,
            req_count: None,
            type_user_supplied,
            enum_readback: crate::client::EnumReadback::Native,
            float_as_string: false,
            mask: 1,
            server_addr: addr(),
            priority: 0,
            callback_tx,
            coalesce_slot: CoalesceSlot::new(),
            needs_restore: true,
            deadband: 0.0,
            last_value: None,
            nreplace: 0,
        };
        (rec, rx)
    }

    /// Drains a `TransportCommand::Subscribe` and returns its
    /// `(data_type, count)`.
    fn drained_type(rx: &mut mpsc::UnboundedReceiver<TransportCommand>) -> (u16, u32) {
        match rx.try_recv() {
            Ok(TransportCommand::Subscribe {
                data_type, count, ..
            }) => (data_type, count),
            Ok(_) => panic!("expected Subscribe command, got a different TransportCommand"),
            Err(e) => panic!("expected Subscribe command, channel error: {e:?}"),
        }
    }

    /// a subid counter that wraps onto a still-live
    /// subscription must skip it, or a stale `EVENT_ADD`/`EVENT_CANCEL`
    /// for the wrapped id would be routed to the wrong record.
    #[test]
    fn alloc_subid_skips_live_subscription_on_wrap() {
        let mut reg = SubscriptionRegistry::new();
        let (rec, _cb_rx) = record(1, 100, /* type_user_supplied */ false);
        reg.add(rec);
        // Force the counter back onto the live subid.
        reg.seed_next_subid(1);
        let next = reg.alloc_subid();
        assert_ne!(
            next, 1,
            "must not reissue a subid held by a live subscription"
        );
        assert!(reg.get(next).is_none(), "allocated subid must be free");
    }

    /// Fresh registry hands out distinct, non-zero subids.
    #[test]
    fn alloc_subid_is_distinct_and_nonzero() {
        let reg = SubscriptionRegistry::new();
        let a = reg.alloc_subid();
        let b = reg.alloc_subid();
        assert_ne!(a, b);
        assert_ne!(a, 0);
    }

    /// An auto-derived subscription must re-derive `data_type`/`count`
    /// when the channel reports its native type changed on reconnect.
    /// Without the reset, monitor frames would decode against the
    /// stale DBR type locked in by the first connect.
    #[test]
    fn auto_derived_type_resets_on_native_change() {
        let mut reg = SubscriptionRegistry::new();
        let (rec, _cb_rx) = record(1, 100, /* type_user_supplied */ false);
        reg.add(rec);
        let (tx, mut rx) = mpsc::unbounded_channel();

        // First connect: native type DBR_SHORT(1), count 1.
        // data_type derives to STS-class 1 + 14 = 15. This record has no
        // `-#` cap, so the wire count is 0 (autosize) regardless of native.
        let (restored, failed) = reg.restore_for_channel(
            100,
            7,
            1,
            1,
            false,
            addr(),
            crate::protocol::CA_MINOR_VERSION,
            &tx,
        );
        assert_eq!((restored, failed), (1, 0));
        assert_eq!(drained_type(&mut rx), (15, 0));

        // IOC redefines the record: reconnect with native type
        // DBR_DOUBLE(6), count 3, native_changed = true.
        reg.subscriptions.get_mut(&1).unwrap().needs_restore = true;
        let (restored, failed) = reg.restore_for_channel(
            100,
            8,
            6,
            3,
            true,
            addr(),
            crate::protocol::CA_MINOR_VERSION,
            &tx,
        );
        assert_eq!((restored, failed), (1, 0));
        // data_type must re-derive to 6 + 14 = 20; count stays 0 (autosize).
        assert_eq!(drained_type(&mut rx), (20, 0));
    }

    /// `camonitor` parity for a native type change across reconnect
    /// (`camonitor.c:143-180`): C compares `ppv->dbfType` against
    /// `ca_field_type(chid)` in its CONN_UP handler, clears the old
    /// subscription, and re-subscribes with the type re-derived through the
    /// SAME substitution chain — so a PV that comes back as an ENUM is
    /// monitored as `DBR_TIME_STRING` (state labels) and not as the numeric
    /// type frozen at first connect.
    ///
    /// The port does that re-derivation here, in the ONE subscribe-time
    /// owner: `ChannelCreated` computes `native_changed`
    /// (`client/mod.rs:3721-3725`), passes it to `restore_for_channel`
    /// (`:3832-3844`), which clears the auto-derived `data_type` and
    /// re-derives via `subscription_readback_dbr` with the record's stored
    /// `enum_readback`. The camonitor front-end therefore needs no
    /// clear/re-subscribe of its own.
    #[test]
    fn label_readback_re_derives_to_dbr_time_string_when_the_pv_returns_as_enum() {
        let mut reg = SubscriptionRegistry::new();
        let (mut rec, _cb_rx) = record(1, 100, /* type_user_supplied */ false);
        // `camonitor` without `-n`: ENUM fields are monitored as labels.
        rec.enum_readback = crate::client::EnumReadback::Label;
        reg.add(rec);
        let (tx, mut rx) = mpsc::unbounded_channel();

        // First connect: native DBR_DOUBLE(6) → DBR_TIME_DOUBLE (6 + 14 = 20).
        let (restored, failed) = reg.restore_for_channel(
            100,
            7,
            6,
            1,
            false,
            addr(),
            crate::protocol::CA_MINOR_VERSION,
            &tx,
        );
        assert_eq!((restored, failed), (1, 0));
        assert_eq!(drained_type(&mut rx), (20, 0));

        // The IOC comes back with the record redefined as an ENUM(3).
        reg.subscriptions.get_mut(&1).unwrap().needs_restore = true;
        let (restored, failed) = reg.restore_for_channel(
            100,
            8,
            3,
            1,
            true,
            addr(),
            crate::protocol::CA_MINOR_VERSION,
            &tx,
        );
        assert_eq!((restored, failed), (1, 0));
        // NOT the frozen 20, and NOT the native DBR_TIME_ENUM (17): the ENUM
        // substitution re-applies, so the re-subscribe asks for
        // DBR_TIME_STRING (14) and camonitor keeps printing state labels.
        assert_eq!(
            drained_type(&mut rx),
            (epics_base_rs::types::DBR_TIME_STRING, 0),
            "reconnect must re-derive the ENUM label substitution, not reuse the pre-change type"
        );
    }

    /// The cached `rec.count` is C's `netSubscription::count` (the user's
    /// cap); the WIRE count is re-resolved per request from the circuit's
    /// minor version (`subscr.getCount(guard, CA_V413(minor))`,
    /// `tcpiiu.cpp:1573`). So the same registry, restored onto a pre-V413
    /// circuit, must put the native count on the wire where a V413 circuit
    /// gets the bare 0 — and the cached cap must not be rewritten by either.
    #[test]
    fn restore_resolves_the_wire_count_from_the_peer_version() {
        let mut reg = SubscriptionRegistry::new();
        let (rec, _cb_rx) = record(1, 100, /* type_user_supplied */ false);
        reg.add(rec); // no `-#` cap ⇒ cached count resolves to 0 (autosize)
        let (tx, mut rx) = mpsc::unbounded_channel();

        // Pre-V413 peer: the autosize 0 has no meaning there, so the
        // channel's native count (4) goes on the wire instead.
        let (restored, _) = reg.restore_for_channel(100, 7, 1, 4, false, addr(), 12, &tx);
        assert_eq!(restored, 1);
        assert_eq!(drained_type(&mut rx).1, 4);
        // The cached cap itself is untouched — only the wire count changed.
        assert_eq!(reg.get(1).unwrap().count, Some(0));

        // Same registry, reconnect onto a V413 peer: the 0 travels verbatim.
        reg.subscriptions.get_mut(&1).unwrap().needs_restore = true;
        let (restored, _) = reg.restore_for_channel(100, 8, 1, 4, false, addr(), 13, &tx);
        assert_eq!(restored, 1);
        assert_eq!(drained_type(&mut rx).1, 0);
    }

    /// `getCount`'s second collapse (`netIO.h:249`): a cap above the native
    /// count resolves to the native count on the wire, on every peer version
    /// — the cached cap is what the user asked for, not what is sent.
    #[test]
    fn restore_clamps_an_over_large_cap_to_native_on_the_wire() {
        let mut reg = SubscriptionRegistry::new();
        let (mut rec, _cb_rx) = record(1, 100, /* type_user_supplied */ true);
        rec.data_type = Some(19);
        rec.count = Some(99); // user asked for more elements than the record has
        reg.add(rec);
        let (tx, mut rx) = mpsc::unbounded_channel();

        let (restored, _) = reg.restore_for_channel(100, 7, 1, 4, false, addr(), 13, &tx);
        assert_eq!(restored, 1);
        assert_eq!(drained_type(&mut rx), (19, 4));
        assert_eq!(reg.get(1).unwrap().count, Some(99));
    }

    /// A subscription created with an explicit user-chosen type keeps
    /// that type across a native-type change — only auto-derived ones
    /// are reset.
    #[test]
    fn user_supplied_type_preserved_on_native_change() {
        let mut reg = SubscriptionRegistry::new();
        let (mut rec, _cb_rx) = record(1, 100, /* type_user_supplied */ true);
        rec.data_type = Some(19); // explicit DBR_TIME_SHORT
        rec.count = Some(2);
        reg.add(rec);
        let (tx, mut rx) = mpsc::unbounded_channel();

        let (restored, _) = reg.restore_for_channel(
            100,
            8,
            6,
            5,
            true,
            addr(),
            crate::protocol::CA_MINOR_VERSION,
            &tx,
        );
        assert_eq!(restored, 1);
        // User-supplied type/count survive the native-type change.
        assert_eq!(drained_type(&mut rx), (19, 2));
    }

    /// `camonitor -#` requested cap is clamped to the native element count
    /// at the first connect — mirrors C `camonitor.c:169`
    /// `reqElems = reqElems > nElems ? nElems : reqElems`. Boundary: a cap
    /// below the native count requests the cap; a cap above requests the
    /// native count.
    #[test]
    fn auto_derived_count_clamps_req_count_to_native() {
        // cap (2) < native (5) ⇒ request the cap.
        let mut reg = SubscriptionRegistry::new();
        let (mut rec, _cb_rx) = record(1, 100, /* type_user_supplied */ false);
        rec.req_count = Some(2);
        reg.add(rec);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (restored, _) = reg.restore_for_channel(
            100,
            7,
            6,
            5,
            false,
            addr(),
            crate::protocol::CA_MINOR_VERSION,
            &tx,
        );
        assert_eq!(restored, 1);
        assert_eq!(drained_type(&mut rx).1, 2, "cap below native ⇒ cap");

        // Separate record: cap (4) > native (1) ⇒ clamp DOWN to native.
        let mut reg2 = SubscriptionRegistry::new();
        let (mut rec2, _cb_rx2) = record(2, 200, /* type_user_supplied */ false);
        rec2.req_count = Some(4);
        reg2.add(rec2);
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        let (restored, _) = reg2.restore_for_channel(
            200,
            9,
            6,
            1,
            false,
            addr(),
            crate::protocol::CA_MINOR_VERSION,
            &tx2,
        );
        assert_eq!(restored, 1);
        assert_eq!(drained_type(&mut rx2).1, 1, "cap above native ⇒ native");
    }

    /// The `-#` cap re-clamps to the FRESH native element count when a
    /// native-type change forces re-derivation (C `camonitor.c:169` re-runs
    /// the clamp on each connect; here the type-change reset is what triggers
    /// the recompute, consistent with the auto-derived `data_type` rule).
    #[test]
    fn auto_derived_count_reclamps_on_native_change() {
        let mut reg = SubscriptionRegistry::new();
        let (mut rec, _cb_rx) = record(1, 100, /* type_user_supplied */ false);
        rec.req_count = Some(4);
        reg.add(rec);
        let (tx, mut rx) = mpsc::unbounded_channel();
        // First connect: native count 5, cap 4 ⇒ request 4.
        let (restored, _) = reg.restore_for_channel(
            100,
            7,
            1,
            5,
            false,
            addr(),
            crate::protocol::CA_MINOR_VERSION,
            &tx,
        );
        assert_eq!(restored, 1);
        assert_eq!(drained_type(&mut rx).1, 4, "cap below native ⇒ cap");
        // Native-type change, array now 2 elements: cap 4 > native 2 ⇒ 2.
        reg.subscriptions.get_mut(&1).unwrap().needs_restore = true;
        let (restored, _) = reg.restore_for_channel(
            100,
            8,
            6,
            2,
            true,
            addr(),
            crate::protocol::CA_MINOR_VERSION,
            &tx,
        );
        assert_eq!(restored, 1);
        assert_eq!(drained_type(&mut rx).1, 2, "native change re-clamps cap");
    }

    /// Direct branch coverage for the wire-count owner: no cap and an
    /// explicit `0` both autosize (wire 0); a positive cap clamps to the
    /// native count (C `camonitor.c:168-169`). The `element_count` argument
    /// is consulted only on the positive-cap branch.
    #[test]
    fn resolve_subscription_count_autosizes_no_cap_and_clamps_positive() {
        assert_eq!(resolve_subscription_count(None, 7), 0, "no cap ⇒ autosize");
        assert_eq!(
            resolve_subscription_count(Some(0), 7),
            0,
            "explicit 0 ⇒ autosize (C reqElems==0)"
        );
        assert_eq!(
            resolve_subscription_count(Some(3), 7),
            3,
            "cap below native ⇒ cap"
        );
        assert_eq!(
            resolve_subscription_count(Some(9), 7),
            7,
            "cap above native ⇒ native (concrete, not autosize)"
        );
    }

    /// No `-#` cap (`req_count == None`) requests wire count 0 (CA
    /// autosize), NOT the native capacity — even when the native count is
    /// non-trivial (4 here). Matches C `camonitor.c:168-169` / `caget.c:200`
    /// (default `reqElems == 0`) so a dynamic waveform reports `NORD`, not a
    /// padded `NELM` array.
    #[test]
    fn auto_derived_count_without_cap_requests_autosize_zero() {
        let mut reg = SubscriptionRegistry::new();
        let (rec, _cb_rx) = record(1, 100, /* type_user_supplied */ false);
        assert_eq!(rec.req_count, None);
        reg.add(rec);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (restored, _) = reg.restore_for_channel(
            100,
            7,
            6,
            4,
            false,
            addr(),
            crate::protocol::CA_MINOR_VERSION,
            &tx,
        );
        assert_eq!(restored, 1);
        assert_eq!(drained_type(&mut rx).1, 0, "no cap ⇒ autosize (wire 0)");
    }

    /// Regression: pre-fix `on_monitor_data` did `try_send → drop` on a
    /// full callback channel, losing terminal transitions like DMOV
    /// 1→0 under burst load (ophyd MoveStatus stuck forever). The fix
    /// coalesces the latest pending snapshot into [`CoalesceSlot`] and
    /// [`MonitorHandle::recv`] drains it after the bounded channel
    /// empties.
    // A small fixture record builder for the slot/flow-control tests.
    fn slotted_record(
        coalesce_slot: Arc<CoalesceSlot>,
        callback_tx: mpsc::Sender<CaResult<Snapshot>>,
    ) -> SubscriptionRecord {
        const DBR_TIME_LONG: u16 = 19;
        SubscriptionRecord {
            subid: 1,
            cid: 100,
            data_type: Some(DBR_TIME_LONG),
            count: Some(1),
            req_count: None,
            type_user_supplied: true,
            enum_readback: crate::client::EnumReadback::Native,
            float_as_string: false,
            mask: 1,
            server_addr: addr(),
            priority: 0,
            callback_tx,
            coalesce_slot,
            needs_restore: false,
            deadband: 0.0,
            last_value: None,
            nreplace: 0,
        }
    }

    fn long_snap(v: i32) -> Snapshot {
        Snapshot::new(EpicsValue::Long(v), 0, 0, SystemTime::now())
    }

    fn post_long(reg: &mut SubscriptionRegistry, v: i32) -> MonitorDeliveryOutcome {
        const DBR_TIME_LONG: u16 = 19;
        let bytes = epics_base_rs::types::encode_dbr(DBR_TIME_LONG, &long_snap(v)).expect("encode");
        reg.on_monitor_data(1, DBR_TIME_LONG, 1, &bytes)
    }

    #[test]
    fn coalesce_on_overflow_preserves_latest_dmov_transition() {
        let mut reg = SubscriptionRegistry::new();
        // Channel of size 2 so the third update is forced into the
        // coalesce slot — exercises the overflow path deterministically.
        let (callback_tx, mut rx) = mpsc::channel::<CaResult<Snapshot>>(2);
        let coalesce_slot = CoalesceSlot::new();
        reg.add(slotted_record(coalesce_slot.clone(), callback_tx));

        // Burst [1, 2, 3, 4, 0] — the trailing 0 stands in for a DMOV
        // 1→0 transition that the pre-fix encoder dropped.
        for (i, v) in [1_i32, 2, 3, 4, 0].iter().enumerate() {
            let outcome = post_long(&mut reg, *v);
            match (i, &outcome) {
                // 1, 2 fill the channel.
                (0..=1, MonitorDeliveryOutcome::Queued) => {}
                // 3, 4, 0 overflow into the slot (out of flow control).
                (2..=4, MonitorDeliveryOutcome::Slotted) => {}
                _ => panic!("unexpected outcome at i={i}"),
            }
        }

        // C `dbEvent.c::nreplace` parity — 3 overflow events.
        assert_eq!(reg.get(1).expect("rec").nreplace, 3);

        // Bounded channel has the first two values (FIFO preserved).
        assert_eq!(
            rx.try_recv().expect("first").expect("Ok").value,
            EpicsValue::Long(1)
        );
        assert_eq!(
            rx.try_recv().expect("second").expect("Ok").value,
            EpicsValue::Long(2)
        );
        assert!(rx.try_recv().is_err(), "bounded channel drained");

        // Slot holds the LATEST value (the DMOV transition); 3 and 4
        // were intermediate-coalesced-away as designed.
        let last = coalesce_slot
            .take_raw()
            .expect("slot non-empty")
            .expect("Ok");
        assert_eq!(
            last.value,
            EpicsValue::Long(0),
            "the terminal DMOV 1→0 transition must survive overflow",
        );
        assert!(coalesce_slot.take_raw().is_none(), "slot is single-entry");
    }

    /// Regression: a new value MUST NOT enter the bounded channel
    /// while the value slot is occupied — otherwise the consumer
    /// (channel first, then slot) sees the newer value before the older
    /// slotted one. Order inversion.
    #[test]
    fn coalesce_preserves_order_under_partial_drain() {
        let mut reg = SubscriptionRegistry::new();
        let (callback_tx, mut rx) = mpsc::channel::<CaResult<Snapshot>>(2);
        let coalesce_slot = CoalesceSlot::new();
        reg.add(slotted_record(coalesce_slot.clone(), callback_tx));

        assert!(matches!(
            post_long(&mut reg, 1),
            MonitorDeliveryOutcome::Queued
        ));
        assert!(matches!(
            post_long(&mut reg, 2),
            MonitorDeliveryOutcome::Queued
        ));
        assert!(matches!(
            post_long(&mut reg, 3),
            MonitorDeliveryOutcome::Slotted
        ));

        // Drain ONE channel item — channel now has a free cell, but the
        // value slot is still occupied (3).
        assert_eq!(
            rx.try_recv().expect("v1").expect("Ok").value,
            EpicsValue::Long(1)
        );

        // A fresh value MUST go to the slot (replace 3→4), NOT the
        // now-free channel — else the consumer reads 4 before 3.
        assert!(
            matches!(post_long(&mut reg, 4), MonitorDeliveryOutcome::Slotted),
            "slot-occupied invariant violated — value leaked into channel"
        );

        // Order: 2 (channel), 4 (slot; 3 coalesced away by latest-wins).
        assert_eq!(
            rx.try_recv().expect("v2").expect("Ok").value,
            EpicsValue::Long(2)
        );
        assert!(rx.try_recv().is_err(), "channel drained");
        let v_slot = coalesce_slot.take_raw().expect("slot").expect("Ok");
        assert_eq!(v_slot.value, EpicsValue::Long(4), "slot holds latest (4)");
        assert_eq!(reg.get(1).expect("rec").nreplace, 2);
    }

    /// Regression: when the bounded channel is full at disconnect
    /// time, `ECA_DISCONN` must land in the (separate) error slot — not
    /// silently drop. The consumer must learn the circuit died.
    #[test]
    fn disconnect_error_coalesces_when_channel_full() {
        let mut reg = SubscriptionRegistry::new();
        let (callback_tx, mut rx) = mpsc::channel::<CaResult<Snapshot>>(2);
        let coalesce_slot = CoalesceSlot::new();
        reg.add(slotted_record(coalesce_slot.clone(), callback_tx));

        // channel=[1,2], value slot=Some(3).
        for v in [1, 2, 3] {
            post_long(&mut reg, v);
        }

        reg.mark_disconnected(&[100]);

        // Channel still has pre-disconnect data. Drain it.
        let _v1 = rx.try_recv().expect("v1");
        let _v2 = rx.try_recv().expect("v2");
        assert!(rx.try_recv().is_err(), "channel drained");

        // The disconnect signal MUST be visible from the error slot.
        match coalesce_slot.take_raw().expect("error slot has DISCONN") {
            Err(epics_base_rs::error::CaError::ServerError(code)) => {
                assert_eq!(code, 192, "ECA_DISCONN");
            }
            other => panic!("expected ECA_DISCONN, got {other:?}"),
        }
    }

    /// A′ (2a): while paused, `route_value` holds the value in the slot
    /// (Slotted) and the recv-side gate (`take_deliverable`) withholds
    /// it until resume.
    #[test]
    fn paused_value_held_and_gated() {
        let slot = CoalesceSlot::new();
        slot.set_paused(true);

        assert!(
            matches!(slot.route_value(long_snap(7)), ValueRoute::Slotted),
            "paused value must be held in slot, not routed to channel"
        );
        assert!(
            slot.take_deliverable().is_none(),
            "recv-side gate must withhold a value held during pause"
        );
        assert!(slot.set_paused(false), "was paused");
        let released = slot.take_deliverable().expect("released after resume");
        assert_eq!(released.expect("Ok").value, EpicsValue::Long(7));
    }

    /// A value arriving during pause must NOT overwrite or hide
    /// a pending error. The error sits in its own slot and bypasses the
    /// pause gate.
    #[test]
    fn paused_value_does_not_clobber_pending_error() {
        let slot = CoalesceSlot::new();
        // An error parks in the error slot (channel-full path).
        slot.put_error(CaError::ServerError(192)); // ECA_DISCONN
        slot.set_paused(true);
        // A value arrives during pause → `gated` cell (separate from error).
        assert!(matches!(
            slot.route_value(long_snap(5)),
            ValueRoute::Slotted
        ));

        // Error is delivered first, even while paused.
        match slot.take_deliverable().expect("error bypasses pause") {
            Err(CaError::ServerError(192)) => {}
            other => panic!("expected ECA_DISCONN first, got {other:?}"),
        }
        // The held value is still withheld (paused) — not lost, not
        // delivered yet.
        assert!(
            slot.take_deliverable().is_none(),
            "held value remains gated after the error drains"
        );
        slot.set_paused(false);
        assert_eq!(
            slot.take_deliverable()
                .expect("value after resume")
                .expect("Ok")
                .value,
            EpicsValue::Long(5),
            "the held value survived the error and resumes intact"
        );
    }

    /// A value buffered BEFORE pause (overflow) stays
    /// deliverable while paused; only during-pause values are gated.
    #[test]
    fn prepause_overflow_value_deliverable_while_paused() {
        let slot = CoalesceSlot::new();
        // Overflow value buffered while active (not paused).
        slot.put_value(long_snap(11));
        // Now pause.
        slot.set_paused(true);
        // The pre-pause value is still deliverable (3a backlog).
        let v = slot
            .take_deliverable()
            .expect("pre-pause overflow value deliverable while paused");
        assert_eq!(v.expect("Ok").value, EpicsValue::Long(11));
        // A value arriving DURING pause is gated.
        assert!(matches!(
            slot.route_value(long_snap(22)),
            ValueRoute::Slotted
        ));
        assert!(
            slot.take_deliverable().is_none(),
            "during-pause value is withheld until resume"
        );
    }

    /// I3 (the precise repro): a pre-pause `ready` value and a
    /// during-pause value coexist. The during-pause value must land in
    /// the SEPARATE `gated` cell, NOT overwrite `ready`. take_deliverable
    /// yields the pre-pause value; the during-pause one surfaces only
    /// after resume.
    #[test]
    fn prepause_ready_not_clobbered_by_concurrent_during_pause_value() {
        let slot = CoalesceSlot::new();
        slot.put_value(long_snap(11)); // pre-pause ready
        slot.set_paused(true);
        // During pause a new value arrives — must NOT overwrite ready.
        assert!(matches!(
            slot.route_value(long_snap(22)),
            ValueRoute::Slotted
        ));
        // While still paused, the deliverable item is the PRE-PAUSE 11.
        let v = slot
            .take_deliverable()
            .expect("pre-pause value still deliverable");
        assert_eq!(
            v.expect("Ok").value,
            EpicsValue::Long(11),
            "during-pause 22 must not clobber pre-pause 11"
        );
        // 22 stays gated until resume, then surfaces.
        assert!(slot.take_deliverable().is_none(), "22 gated while paused");
        slot.set_paused(false);
        assert_eq!(
            slot.take_deliverable()
                .expect("22 after resume")
                .expect("Ok")
                .value,
            EpicsValue::Long(22),
        );
    }

    /// Boundary: `mark_disconnected` on an idle subscription (nothing in the
    /// bounded channel) parks the DISCONN in the error cell, never in the
    /// channel.
    #[test]
    fn mark_disconnected_on_empty_channel_parks_error_in_cell() {
        let mut reg = SubscriptionRegistry::new();
        let (callback_tx, mut rx) = mpsc::channel::<CaResult<Snapshot>>(4);
        let coalesce_slot = CoalesceSlot::new();
        reg.add(slotted_record(coalesce_slot.clone(), callback_tx));

        reg.mark_disconnected(&[100]);
        assert!(rx.try_recv().is_err(), "DISCONN did NOT go to the channel");
        match coalesce_slot.take_raw().expect("DISCONN in error cell") {
            Err(CaError::ServerError(192)) => {}
            other => panic!("expected ECA_DISCONN, got {other:?}"),
        }
    }

    /// New-finding a pending error in the error cell must keep a
    /// later value from jumping ahead via a partially-drained channel.
    /// route_value treats `error.is_some()` as slot-occupied, so the
    /// value coalesces into the slot and is delivered AFTER the error.
    #[test]
    fn pending_error_stays_ahead_of_later_value() {
        let mut reg = SubscriptionRegistry::new();
        let (callback_tx, mut rx) = mpsc::channel::<CaResult<Snapshot>>(2);
        let coalesce_slot = CoalesceSlot::new();
        reg.add(slotted_record(coalesce_slot.clone(), callback_tx));

        // Fill the channel, then an error arrives while it is full →
        // parks in the error cell.
        post_long(&mut reg, 1);
        post_long(&mut reg, 2);
        assert!(matches!(
            reg.on_monitor_error(1, 192),
            MonitorDeliveryOutcome::Slotted
        ));

        // Consumer drains ONE channel item — a cell frees up.
        assert_eq!(
            rx.try_recv().expect("ch1").expect("Ok").value,
            EpicsValue::Long(1)
        );

        // A new value must NOT take the free channel cell ahead of the
        // pending error — it coalesces into the slot.
        assert!(matches!(
            post_long(&mut reg, 3),
            MonitorDeliveryOutcome::Slotted
        ));

        // Order: remaining channel item (2), then the error, then 3.
        assert_eq!(
            rx.try_recv().expect("ch2").expect("Ok").value,
            EpicsValue::Long(2)
        );
        assert!(rx.try_recv().is_err(), "channel drained");
        match coalesce_slot.take_deliverable().expect("error first") {
            Err(CaError::ServerError(192)) => {}
            other => panic!("expected error ahead of value, got {other:?}"),
        }
        assert_eq!(
            coalesce_slot
                .take_deliverable()
                .expect("value after error")
                .expect("Ok")
                .value,
            EpicsValue::Long(3),
        );
    }

    /// Resume coalesces an UNDRAINED pre-pause `ready` with the
    /// during-pause `gated` value (uniform latest-wins). The older value
    /// is superseded — standard monitor latest-value semantics for a
    /// consumer that didn't drain during the pause. (A consumer that
    /// DOES recv during the pause gets 11 — see
    /// `prepause_ready_not_clobbered_by_concurrent_during_pause_value`.)
    #[test]
    fn resume_coalesces_undrained_ready_to_latest() {
        let slot = CoalesceSlot::new();
        slot.put_value(long_snap(11)); // ready = 11 (active)
        slot.set_paused(true);
        assert!(matches!(
            slot.route_value(long_snap(22)),
            ValueRoute::Slotted
        )); // gated = 22
        // Do NOT drain during pause; resume directly.
        slot.set_paused(false); // resume → gated(22) promoted into ready
        assert_eq!(
            slot.take_deliverable().expect("22").expect("Ok").value,
            EpicsValue::Long(22),
            "latest (22) survives; undrained 11 coalesced away",
        );
        assert!(
            slot.take_deliverable().is_none(),
            "single deliverable value"
        );
    }

    /// After resume the deliverable value lives in `ready` (the coalesced
    /// latest); a fresh active value coalesces into it (uniform).
    #[test]
    fn post_resume_value_coalesces_into_ready() {
        let slot = CoalesceSlot::new();
        slot.put_value(long_snap(11)); // ready = 11
        slot.set_paused(true);
        slot.route_value(long_snap(22)); // gated = 22
        slot.set_paused(false); // resume → ready = 22
        assert!(matches!(
            slot.route_value(long_snap(33)),
            ValueRoute::Slotted
        )); // ready = 33
        assert_eq!(
            slot.take_deliverable().expect("33").expect("Ok").value,
            EpicsValue::Long(33),
            "uniform coalesce to latest",
        );
        assert!(slot.take_deliverable().is_none());
    }

    /// A value made deliverable by a PRIOR resume is pre-(this-)pause
    /// backlog and must stay deliverable across a second pause. Because
    /// resume promotes `gated` into `ready`, the value sits in `ready`
    /// (never gated) and a later pause leaves it alone — the invariant
    /// `gated.is_some() ⟹ paused` makes re-gating structurally
    /// impossible.
    #[test]
    fn held_backlog_stays_deliverable_across_second_pause() {
        let slot = CoalesceSlot::new();
        slot.set_paused(true); // pause 1
        slot.route_value(long_snap(1)); // gated = 1 (during pause 1)
        slot.set_paused(false); // resume 1 → gated promoted into ready = 1
        // No recv. Pause again; ready=1 is untouched (gated is empty).
        slot.set_paused(true); // pause 2 (gated empty)
        let v = slot
            .take_deliverable()
            .expect("post-resume backlog must survive a new pause (I3)");
        assert_eq!(v.expect("Ok").value, EpicsValue::Long(1));
    }

    /// Multi-cycle bound: across pause/resume cycles the deliverable
    /// backlog coalesces to the latest at each resume (it never grows
    /// past one `ready` + one `gated`). Older intermediate values are
    /// dropped as designed (uniform latest-wins); the newest survives.
    #[test]
    fn repeated_pause_cycles_coalesce_to_latest() {
        let slot = CoalesceSlot::new();
        slot.put_value(long_snap(11)); // ready = 11 (active)
        slot.set_paused(true); // pause1 (gated empty → ready stays 11)
        slot.route_value(long_snap(22)); // gated = 22 (during pause1)
        slot.set_paused(false); // resume1 → ready = 22 (11 coalesced away)
        slot.set_paused(true); // pause2 (gated empty, ready stays 22)
        // 11 was the older backlog — coalesced away; 22 is deliverable.
        let v = slot.take_deliverable().expect("latest backlog deliverable");
        assert_eq!(v.expect("Ok").value, EpicsValue::Long(22));
        assert!(
            slot.take_deliverable().is_none(),
            "only the latest survived the cycle"
        );
    }

    /// A′ error policy: errors bypass pause — `take_deliverable` yields
    /// an `Err` from the error slot even while paused.
    #[test]
    fn paused_error_bypasses_gate() {
        let slot = CoalesceSlot::new();
        slot.set_paused(true);
        slot.put_error(CaError::ServerError(192)); // ECA_DISCONN
        let got = slot.take_deliverable().expect("error must bypass pause");
        assert!(
            matches!(got, Err(CaError::ServerError(192))),
            "ECA_DISCONN delivered while paused"
        );
    }

    /// `route_value` when not paused: empty → TryChannel, occupied →
    /// Slotted (the order-preservation short-circuit).
    #[test]
    fn route_value_not_paused_channel_then_replace() {
        let slot = CoalesceSlot::new();
        assert!(
            matches!(slot.route_value(long_snap(1)), ValueRoute::TryChannel(_)),
            "empty slot, not paused → caller tries the channel"
        );
        // Occupy the slot.
        slot.put_value(long_snap(2));
        assert!(
            matches!(slot.route_value(long_snap(3)), ValueRoute::Slotted),
            "occupied slot, not paused → replace in place (no channel jump-ahead)"
        );
        assert_eq!(
            slot.take_raw().expect("slot").expect("Ok").value,
            EpicsValue::Long(3),
            "latest wins"
        );
    }

    /// Without a native-type change, an auto-derived type stays locked
    /// to its first-connect value (the established reconnect behaviour).
    #[test]
    fn auto_derived_type_stable_without_native_change() {
        let mut reg = SubscriptionRegistry::new();
        let (rec, _cb_rx) = record(1, 100, false);
        reg.add(rec);
        let (tx, mut rx) = mpsc::unbounded_channel();

        let (restored, _) = reg.restore_for_channel(
            100,
            7,
            1,
            1,
            false,
            addr(),
            crate::protocol::CA_MINOR_VERSION,
            &tx,
        );
        assert_eq!(restored, 1);
        // No `-#` cap → count is autosize (0); type derives to 15.
        assert_eq!(drained_type(&mut rx), (15, 0));

        reg.subscriptions.get_mut(&1).unwrap().needs_restore = true;
        let (restored, _) = reg.restore_for_channel(
            100,
            8,
            6,
            3,
            false,
            addr(),
            crate::protocol::CA_MINOR_VERSION,
            &tx,
        );
        assert_eq!(restored, 1);
        // native_changed = false → type stays at the first-connect value;
        // count stays autosize (0).
        assert_eq!(drained_type(&mut rx), (15, 0));
    }
}
