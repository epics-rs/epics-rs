use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
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
    /// during-pause hold into `held`). No channel write — invisible to
    /// flow control.
    Slotted,
    /// `ready` empty and not paused — the caller should try the bounded
    /// channel (and fall back to [`CoalesceSlot::put_value`] on full).
    /// Boxed so this large variant doesn't bloat the others; the
    /// snapshot is already heap-backed.
    TryChannel(Box<Snapshot>),
}

/// Per-subscription overflow/pause buffer shared between the
/// coordinator (producer) and the [`MonitorHandle`] (consumer).
///
/// # Invariants (structurally enforced by the three distinct cells)
///
/// - **I1 — flow control counts the channel only.** `pending_deliveries`
///   and the per-circuit `EVENTS_OFF` accounting count ONLY items in the
///   bounded channel. Every cell here is out of band: writing or reading
///   it never touches flow control. (A client-side pause must not trip
///   the wire-level `EVENTS_OFF` and freeze sibling subscriptions.)
/// - **I2 — error preservation.** The `error` cell is separate from the
///   value cells. A value can never overwrite or hide a pending error;
///   errors bypass pause and are delivered first.
/// - **I3 — pause scope.** Everything deliverable at the instant
///   `pause()` is called stays deliverable: pause ENTRY collapses the
///   backlog into a single latest `ready` value, and `ready` is never
///   gated. Only values arriving *during* the pause land in `held`
///   (gated by the `take_deliverable` order) — and because pause entry
///   emptied `held`, it means exactly "arrived during THIS pause", so a
///   value made deliverable by a prior resume can't be re-gated by a
///   later pause. `resume` moves nothing (no destructive collapse);
///   `take_deliverable` simply yields `ready` then `held`.
///
/// All cells + the pause flag live under one mutex, so the producer's
/// "decide vs pause + write" and the consumer's "check pause + take"
/// are each atomic against `resume`'s "flip + notify" — no window where
/// a value written just-before-resume is stranded.
pub(crate) struct CoalesceSlot {
    inner: StdMutex<CoalesceInner>,
    notify: Notify,
}

struct CoalesceInner {
    /// Pending error — sticky, bypasses pause, delivered first (I2).
    /// Latest error wins among errors. Never touched by value writes.
    error: Option<CaError>,
    /// The OLDER of up to two pending values — delivered after `error`
    /// and always deliverable, even while paused (I3 pre-pause backlog).
    ready: Option<Snapshot>,
    /// The NEWER (latest) pending value, coalesced. Delivered after
    /// `ready`, but only while NOT paused (2a: a value that arrived
    /// during the pause stays here, gated, until resume). A separate
    /// cell so it can never clobber the older `ready` value.
    held: Option<Snapshot>,
    paused: bool,
}

impl CoalesceSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: StdMutex::new(CoalesceInner {
                error: None,
                ready: None,
                held: None,
                paused: false,
            }),
            notify: Notify::new(),
        })
    }

    /// Coalesce a value into the slot's value tail (caller holds the
    /// lock). The tail is `held` when a second pending value already
    /// exists; otherwise `ready` (the first/only pending value). During
    /// pause the latest value always lands in `held` so the pause gate
    /// in `take_deliverable` withholds it. This is the SOLE writer of
    /// the value cells, so the "older stays in ready, newer in held"
    /// shape is structural.
    fn coalesce_value_locked(inner: &mut CoalesceInner, snapshot: Snapshot) {
        if inner.paused || inner.held.is_some() {
            inner.held = Some(snapshot);
        } else {
            inner.ready = Some(snapshot);
        }
    }

    /// Route a value snapshot. Goes to the bounded channel ONLY when the
    /// slot is entirely empty AND not paused; any pending cell — error,
    /// ready, or held — means the value must coalesce into the slot
    /// rather than jump ahead via the channel. Including a pending
    /// `error` (F1: a value must never overtake a buffered error).
    /// Out of flow control (I1).
    pub fn route_value(&self, snapshot: Snapshot) -> ValueRoute {
        let mut g = self.inner.lock().expect("CoalesceSlot mutex poisoned");
        if !g.paused && g.error.is_none() && g.ready.is_none() && g.held.is_none() {
            return ValueRoute::TryChannel(Box::new(snapshot));
        }
        let paused = g.paused;
        Self::coalesce_value_locked(&mut g, snapshot);
        drop(g);
        // A value written while paused goes to `held` (gated); recv
        // can't take it, so no wake. Otherwise it's deliverable now.
        if !paused {
            self.notify.notify_one();
        }
        ValueRoute::Slotted
    }

    /// Overflow fallback after a full channel: coalesce the value into
    /// the slot tail. Out of flow control (I1).
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

    /// The SOLE owner of delivery priority. Takes, in order:
    ///   1. `error` — bypasses pause, always first (I2).
    ///   2. `ready` — the older value, deliverable even while paused
    ///      (I3 pre-pause backlog).
    ///   3. `held` — the newer value, ONLY when not paused (2a gate).
    ///
    /// `resume` does not move values between cells, so an undrained
    /// `ready` is never overwritten; the gate lives here alone.
    pub fn take_deliverable(&self) -> Option<CaResult<Snapshot>> {
        let mut g = self.inner.lock().expect("CoalesceSlot mutex poisoned");
        if let Some(err) = g.error.take() {
            return Some(Err(err));
        }
        if let Some(v) = g.ready.take() {
            return Some(Ok(v));
        }
        if !g.paused {
            if let Some(v) = g.held.take() {
                return Some(Ok(v));
            }
        }
        None
    }

    /// Future that resolves on the next `notify` (deliverable write or
    /// `resume`).
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }

    /// Set/clear the pause flag. Returns the previous value.
    ///
    /// On pause ENTRY it freezes the currently-deliverable backlog into
    /// a single latest value in `ready` (collapsing `held`, which is the
    /// newer of the two, into it) and leaves `held` empty. This is what
    /// keeps `held` meaning exactly "arrived during THIS pause": a value
    /// that became deliverable after a PRIOR resume is pre-(this-)pause
    /// backlog and must stay deliverable (I3), so it is promoted to
    /// `ready` rather than re-gated by the new pause.
    ///
    /// On RESUME it does NOT move any value (the old resume-time collapse
    /// could overwrite an undrained pre-pause `ready`); it only flips the
    /// flag and wakes `recv`, which then yields `ready` then `held`.
    pub fn set_paused(&self, paused: bool) -> bool {
        let mut g = self.inner.lock().expect("CoalesceSlot mutex poisoned");
        let prev = g.paused;
        g.paused = paused;
        if !prev && paused && g.held.is_some() {
            // Entering pause: collapse the deliverable backlog to its
            // latest (`held` wins over the older `ready`) so `held`
            // becomes free for genuinely-during-this-pause arrivals.
            g.ready = g.held.take();
        }
        drop(g);
        if prev && !paused {
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

    /// Drop both value cells (ready + held) AND the error regardless of
    /// pause state. Used on disconnect so a stale snapshot/error can't
    /// outlive the circuit.
    fn clear(&self) {
        let mut g = self.inner.lock().expect("CoalesceSlot mutex poisoned");
        g.error = None;
        g.ready = None;
        g.held = None;
    }

    /// Test-only: unconditional drain — error, then `ready`, then
    /// `held`, ignoring pause.
    #[cfg(test)]
    fn take_raw(&self) -> Option<CaResult<Snapshot>> {
        let mut g = self.inner.lock().expect("CoalesceSlot mutex poisoned");
        if let Some(err) = g.error.take() {
            return Some(Err(err));
        }
        if let Some(v) = g.ready.take() {
            return Some(Ok(v));
        }
        g.held.take().map(Ok)
    }
}

/// Outcome of `on_monitor_data` — the single signal that drives the
/// coordinator's per-circuit flow control.
///
/// Only `Queued` (a bounded-channel write) feeds flow control; every
/// slot write is `Slotted` and is invisible to it (invariant I1). This
/// is the single gate: the coordinator bumps outstanding on `Queued`
/// and decrements on the matching channel-drain `MonitorConsumed`.
pub(crate) enum MonitorDeliveryOutcome {
    /// Written to the bounded channel — counts toward flow control.
    Queued(SocketAddr),
    /// Buffered in the coalesce slot (overflow value, pause-held value,
    /// or overflow error). Out of flow control — diagnostic only.
    Slotted(SocketAddr),
    /// Dropped because the consumer channel is closed (the application
    /// dropped its `MonitorHandle`). The only remaining drop case.
    Dropped(SocketAddr),
    /// Filtered by client-side deadband (no action).
    Filtered,
    /// Subscription not found.
    NotFound,
}

pub(crate) struct SubscriptionRecord {
    pub subid: u32,
    pub cid: u32,
    pub data_type: Option<u16>,
    pub count: Option<u32>,
    /// `true` when `data_type`/`count` were chosen explicitly by the
    /// caller, `false` when they were auto-derived from the channel's
    /// native type at subscribe time. Auto-derived values must be
    /// re-derived when the IOC redefines the record (the channel
    /// reports `NativeTypeChanged` on reconnect); user-supplied values
    /// are preserved across reconnects. See `restore_for_channel`.
    pub type_user_supplied: bool,
    pub mask: u16,
    pub server_addr: SocketAddr,
    pub callback_tx: mpsc::Sender<CaResult<Snapshot>>,
    /// "Latest pending" slot — see [`CoalesceSlot`]. Shared with the
    /// [`MonitorHandle`] so the consumer drains it after the bounded
    /// channel empties.
    pub coalesce_slot: Arc<CoalesceSlot>,
    pub needs_restore: bool,
    /// Client-side deadband: suppress callback if |new - old| < deadband.
    pub deadband: f64,
    /// Last delivered scalar value (for deadband filtering).
    pub last_value: Option<f64>,
    /// Number of monitor updates in the bounded channel awaiting
    /// consumption. Invariant I1: this counts ONLY channel items —
    /// coalesce-slot entries (overflow/held/error) are out of band and
    /// never bump it, so a client-side pause can't trip the per-circuit
    /// `EVENTS_OFF`.
    pub pending_deliveries: usize,
    /// Diagnostic counter — number of overflow-coalesce events for
    /// this subscription. Mirrors `dbEvent.c::pevent->nreplace`.
    pub nreplace: u64,
}

pub(crate) struct SubscriptionRegistry {
    subscriptions: HashMap<u32, SubscriptionRecord>,
}

impl SubscriptionRegistry {
    pub fn new() -> Self {
        Self {
            subscriptions: HashMap::new(),
        }
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
        let server_addr = rec.server_addr;
        try_deliver_err(
            rec,
            epics_base_rs::error::CaError::ServerError(eca_status),
            server_addr,
        )
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
        let server_addr = rec.server_addr;

        let snapshot = if data_type <= 6 {
            let dbr_type = match DbFieldType::from_u16(data_type) {
                Ok(t) => t,
                Err(e) => {
                    return try_deliver_err(rec, e, server_addr);
                }
            };
            match EpicsValue::from_bytes_array(dbr_type, data, count as usize) {
                Ok(value) => Snapshot::new(value, 0, 0, SystemTime::now()),
                Err(e) => {
                    return try_deliver_err(rec, e, server_addr);
                }
            }
        } else {
            match decode_dbr(data_type, data, count as usize) {
                Ok(s) => s,
                Err(e) => {
                    return try_deliver_err(rec, e, server_addr);
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
                MonitorDeliveryOutcome::Slotted(server_addr)
            }
            ValueRoute::TryChannel(snapshot) => match rec.callback_tx.try_send(Ok(*snapshot)) {
                Ok(()) => {
                    rec.pending_deliveries += 1;
                    MonitorDeliveryOutcome::Queued(server_addr)
                }
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
                    MonitorDeliveryOutcome::Slotted(server_addr)
                }
                Err(TrySendError::Closed(_)) => MonitorDeliveryOutcome::Dropped(server_addr),
            },
        }
    }

    /// Mark all subscriptions for a given server's channels as needing restore.
    /// Returns the cids that were affected.
    ///
    /// R2-37: also deliver one `Err(CaError::ServerError(ECA_DISCONN))`
    /// per affected subscription's callback channel — libca
    /// `cac::disconnectAllIO()` (`modules/ca/src/client/cac.cpp:678-698`)
    /// iterates every in-flight IO on the channel (including
    /// subscriptions) and fires `pNetIO->exception(... ECA_DISCONN ...)`.
    /// Pre-fix Rust silently flipped `needs_restore = true` and waited
    /// for reconnect, so a libca-style `MonitorHandle::recv()` saw
    /// nothing when the circuit died.
    /// Returns a structured flow-control delta: per-circuit map of how
    /// many bounded-channel items were "forgotten" (abandoned for
    /// flow-control purposes) so the coordinator can decrement the
    /// circuit's outstanding count. Every disconnect path MUST apply
    /// this delta (the F3 fix).
    ///
    /// The disconnect error goes ALWAYS into the error cell (never the
    /// bounded channel), so it never bumps `pending` — this keeps the
    /// flow-control owner single (channel send/recv only, I1) and the
    /// returned delta unambiguous (no "did the error land in the
    /// channel?" case to reconcile).
    pub fn mark_disconnected(&mut self, cids: &[u32]) -> HashMap<SocketAddr, usize> {
        const ECA_DISCONN: u32 = 192; // protocol::ECA_DISCONN
        let mut cleared = HashMap::new();
        for rec in self.subscriptions.values_mut() {
            if cids.contains(&rec.cid) {
                rec.needs_restore = true;
                // Forget the bounded-channel items for flow control:
                // they stay in the channel for the consumer to drain
                // (R2-37), but draining them won't re-decrement
                // outstanding because `pending` is now 0
                // (`mark_consumed` returns `None`).
                let old_pending = rec.pending_deliveries;
                rec.pending_deliveries = 0;
                // Drop stale value cells, then park ECA_DISCONN in the
                // error cell. It is delivered with priority and bypasses
                // pause; being out of band it leaves `pending` at 0.
                rec.coalesce_slot.clear();
                rec.coalesce_slot
                    .put_error(CaError::ServerError(ECA_DISCONN));
                if old_pending > 0 {
                    *cleared.entry(rec.server_addr).or_insert(0) += old_pending;
                }
            }
        }
        cleared
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
                // against the wrong DBR type. Drop it so it re-derives from
                // the fresh native type below. User-supplied types are kept.
                if native_changed && !rec.type_user_supplied {
                    rec.data_type = None;
                    rec.count = None;
                }
                // native_type is the server-reported CA wire type (0..6),
                // so `+ 14` always lands in the DBR_TIME range; Int64 (7)
                // cannot reach here.
                let data_type = *rec.data_type.get_or_insert(native_type + 14);
                let count = *rec.count.get_or_insert(element_count);
                let _ = transport_tx.send(TransportCommand::Subscribe {
                    sid: new_sid,
                    data_type,
                    count,
                    subid: rec.subid,
                    mask: rec.mask,
                    server_addr,
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

    pub fn mark_consumed(&mut self, subid: u32) -> Option<SocketAddr> {
        let rec = self.subscriptions.get_mut(&subid)?;
        if rec.pending_deliveries == 0 {
            return None;
        }
        rec.pending_deliveries -= 1;
        Some(rec.server_addr)
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
/// is room (counts toward flow control), otherwise the sticky error
/// slot (out of flow control, never overwritten by a value). The error
/// slot does not displace a pending value — both can be queued and the
/// consumer's `take_deliverable` delivers the error first.
fn try_deliver_err(
    rec: &mut SubscriptionRecord,
    err: CaError,
    server_addr: SocketAddr,
) -> MonitorDeliveryOutcome {
    match rec.callback_tx.try_send(Err(err)) {
        Ok(()) => {
            rec.pending_deliveries += 1;
            MonitorDeliveryOutcome::Queued(server_addr)
        }
        Err(TrySendError::Full(rejected)) => {
            // Channel full — park the error in its own slot. Out of
            // flow control (I1); EVENTS_OFF already fired. Recover the
            // rejected error rather than cloning.
            let e = match rejected {
                Err(e) => e,
                Ok(_) => unreachable!("we just sent an Err"),
            };
            rec.coalesce_slot.put_error(e);
            MonitorDeliveryOutcome::Slotted(server_addr)
        }
        Err(TrySendError::Closed(_)) => MonitorDeliveryOutcome::Dropped(server_addr),
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
            type_user_supplied,
            mask: 1,
            server_addr: addr(),
            callback_tx,
            coalesce_slot: CoalesceSlot::new(),
            needs_restore: true,
            deadband: 0.0,
            last_value: None,
            pending_deliveries: 0,
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
        // data_type derives to STS-class 1 + 14 = 15.
        let (restored, failed) = reg.restore_for_channel(100, 7, 1, 1, false, addr(), &tx);
        assert_eq!((restored, failed), (1, 0));
        assert_eq!(drained_type(&mut rx), (15, 1));

        // IOC redefines the record: reconnect with native type
        // DBR_DOUBLE(6), count 3, native_changed = true.
        reg.subscriptions.get_mut(&1).unwrap().needs_restore = true;
        let (restored, failed) = reg.restore_for_channel(100, 8, 6, 3, true, addr(), &tx);
        assert_eq!((restored, failed), (1, 0));
        // data_type must re-derive to 6 + 14 = 20, count to 3.
        assert_eq!(drained_type(&mut rx), (20, 3));
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

        let (restored, _) = reg.restore_for_channel(100, 8, 6, 5, true, addr(), &tx);
        assert_eq!(restored, 1);
        // User-supplied type/count survive the native-type change.
        assert_eq!(drained_type(&mut rx), (19, 2));
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
            type_user_supplied: true,
            mask: 1,
            server_addr: addr(),
            callback_tx,
            coalesce_slot,
            needs_restore: false,
            deadband: 0.0,
            last_value: None,
            pending_deliveries: 0,
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
                (0..=1, MonitorDeliveryOutcome::Queued(_)) => {}
                // 3, 4, 0 overflow into the slot (out of flow control).
                (2..=4, MonitorDeliveryOutcome::Slotted(_)) => {}
                _ => panic!("unexpected outcome at i={i}"),
            }
        }

        // Slot is out of flow control (I1): pending counts channel only.
        assert_eq!(reg.get(1).expect("rec").pending_deliveries, 2);
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

    /// F1 regression: a new value MUST NOT enter the bounded channel
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
            MonitorDeliveryOutcome::Queued(_)
        ));
        assert!(matches!(
            post_long(&mut reg, 2),
            MonitorDeliveryOutcome::Queued(_)
        ));
        assert!(matches!(
            post_long(&mut reg, 3),
            MonitorDeliveryOutcome::Slotted(_)
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
            matches!(post_long(&mut reg, 4), MonitorDeliveryOutcome::Slotted(_)),
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

    /// F2 regression: when the bounded channel is full at disconnect
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
        assert_eq!(
            reg.get(1).expect("rec").pending_deliveries,
            2,
            "channel only (I1)"
        );

        let cleared = reg.mark_disconnected(&[100]);
        // Net cleared = old channel pending (2) - new pending (0; DISCONN
        // went to the error slot because the channel was full) = 2.
        assert_eq!(*cleared.get(&addr()).expect("server addr"), 2);
        assert_eq!(
            reg.get(1).expect("rec").pending_deliveries,
            0,
            "DISCONN parked in the error slot (out of flow control)",
        );

        // Channel still has pre-disconnect data (R2-37). Drain it.
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

    /// F2 / I2: a value arriving during pause must NOT overwrite or hide
    /// a pending error. The error sits in its own slot and bypasses the
    /// pause gate.
    #[test]
    fn paused_value_does_not_clobber_pending_error() {
        let slot = CoalesceSlot::new();
        // An error parks in the error slot (channel-full path).
        slot.put_error(CaError::ServerError(192)); // ECA_DISCONN
        slot.set_paused(true);
        // A value arrives during pause → value slot (held). Separate slot.
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

    /// F3 / I3: a value buffered BEFORE pause (overflow) stays
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

    /// This-round F1 / I3 (the precise repro): a pre-pause `ready` value
    /// and a during-pause value coexist. The during-pause value must
    /// land in the SEPARATE `held` cell, NOT overwrite `ready`.
    /// take_deliverable yields the pre-pause value; the during-pause one
    /// surfaces only after resume.
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

    /// F2 boundary: `mark_disconnected` with `old_pending == 0` (no
    /// channel items) yields no flow-control delta and never bumps
    /// `pending` — the DISCONN goes to the error cell only, never the
    /// channel.
    #[test]
    fn mark_disconnected_old_pending_zero_yields_no_delta() {
        let mut reg = SubscriptionRegistry::new();
        let (callback_tx, mut rx) = mpsc::channel::<CaResult<Snapshot>>(4);
        let coalesce_slot = CoalesceSlot::new();
        reg.add(slotted_record(coalesce_slot.clone(), callback_tx));
        assert_eq!(reg.get(1).expect("rec").pending_deliveries, 0);

        let cleared = reg.mark_disconnected(&[100]);
        assert!(
            cleared.is_empty(),
            "no channel items → empty flow-control delta"
        );
        assert_eq!(
            reg.get(1).expect("rec").pending_deliveries,
            0,
            "DISCONN parks in the error cell; pending never bumped",
        );
        assert!(rx.try_recv().is_err(), "DISCONN did NOT go to the channel");
        match coalesce_slot.take_raw().expect("DISCONN in error cell") {
            Err(CaError::ServerError(192)) => {}
            other => panic!("expected ECA_DISCONN, got {other:?}"),
        }
    }

    /// New-finding F1: a pending error in the error cell must keep a
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
            MonitorDeliveryOutcome::Slotted(_)
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
            MonitorDeliveryOutcome::Slotted(_)
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

    /// New-finding F2: resume must NOT overwrite an undrained pre-pause
    /// `ready` with the during-pause `held` value. After resume,
    /// take_deliverable yields `ready` (11) then `held` (22), in order.
    #[test]
    fn resume_does_not_overwrite_undrained_ready() {
        let slot = CoalesceSlot::new();
        slot.put_value(long_snap(11)); // ready = 11 (active)
        slot.set_paused(true);
        assert!(matches!(
            slot.route_value(long_snap(22)),
            ValueRoute::Slotted
        )); // held = 22
        // Do NOT drain ready during pause; resume directly.
        slot.set_paused(false);
        assert_eq!(
            slot.take_deliverable().expect("11").expect("Ok").value,
            EpicsValue::Long(11),
            "undrained pre-pause ready survives resume",
        );
        assert_eq!(
            slot.take_deliverable().expect("22").expect("Ok").value,
            EpicsValue::Long(22),
        );
        assert!(slot.take_deliverable().is_none());
    }

    /// New-finding F2 (cont.): after resume with ready=11 and held=22, a
    /// fresh active value 33 coalesces into the tail (held), preserving
    /// the older `ready`. Delivery: 11 then latest 33 (22 dropped).
    #[test]
    fn post_resume_value_coalesces_tail_preserving_ready() {
        let slot = CoalesceSlot::new();
        slot.put_value(long_snap(11)); // ready = 11
        slot.set_paused(true);
        slot.route_value(long_snap(22)); // held = 22
        slot.set_paused(false); // resume — no collapse: ready=11, held=22
        assert!(matches!(
            slot.route_value(long_snap(33)),
            ValueRoute::Slotted
        ));
        assert_eq!(
            slot.take_deliverable().expect("11").expect("Ok").value,
            EpicsValue::Long(11),
            "older ready preserved",
        );
        assert_eq!(
            slot.take_deliverable().expect("33").expect("Ok").value,
            EpicsValue::Long(33),
            "tail coalesced to latest (22 superseded by 33)",
        );
        assert!(slot.take_deliverable().is_none());
    }

    /// This-round finding: a value made deliverable by a PRIOR resume
    /// is pre-(this-)pause backlog and must stay deliverable across a
    /// second pause. Pause ENTRY collapses it into `ready`, so it is no
    /// longer gated.
    #[test]
    fn held_backlog_stays_deliverable_across_second_pause() {
        let slot = CoalesceSlot::new();
        slot.set_paused(true); // pause 1
        slot.route_value(long_snap(1)); // held = 1 (during pause 1)
        slot.set_paused(false); // resume 1 → held=1 now deliverable
        // No recv. Pause again BEFORE draining held=1.
        slot.set_paused(true); // pause 2 entry → collapse held(1) → ready
        let v = slot
            .take_deliverable()
            .expect("post-resume backlog must survive a new pause (I3)");
        assert_eq!(v.expect("Ok").value, EpicsValue::Long(1));
    }

    /// Multi-cycle bound: across pause/resume cycles the deliverable
    /// backlog coalesces to the latest at each pause entry (it never
    /// grows past ready+held). Older intermediate values are dropped as
    /// designed (latest-wins), the newest survives.
    #[test]
    fn repeated_pause_cycles_coalesce_to_latest() {
        let slot = CoalesceSlot::new();
        slot.put_value(long_snap(11)); // ready = 11 (active)
        slot.set_paused(true); // pause1 entry (held empty → ready stays 11)
        slot.route_value(long_snap(22)); // held = 22 (during pause1)
        slot.set_paused(false); // resume1 → ready=11, held=22
        slot.set_paused(true); // pause2 entry → collapse: ready = 22 (latest)
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

        let (restored, _) = reg.restore_for_channel(100, 7, 1, 1, false, addr(), &tx);
        assert_eq!(restored, 1);
        assert_eq!(drained_type(&mut rx), (15, 1));

        reg.subscriptions.get_mut(&1).unwrap().needs_restore = true;
        let (restored, _) = reg.restore_for_channel(100, 8, 6, 3, false, addr(), &tx);
        assert_eq!(restored, 1);
        // native_changed = false → type/count stay at first-connect values.
        assert_eq!(drained_type(&mut rx), (15, 1));
    }
}
