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
/// - **I3 — pause scope.** A value buffered *before* `pause()` lives in
///   `ready` and stays deliverable while paused. A value arriving
///   *during* the pause lives in `held` (a SEPARATE cell) and is
///   withheld until `resume`; it can never overwrite the pre-pause
///   `ready` value.
///
/// All cells + the pause flag live under one mutex, so the producer's
/// "decide vs pause + write" and the consumer's "check pause + take"
/// are each atomic against `resume`'s "collapse + flip + notify" — no
/// window where a held value written just-before-resume is stranded.
pub(crate) struct CoalesceSlot {
    inner: StdMutex<CoalesceInner>,
    notify: Notify,
}

struct CoalesceInner {
    /// Pending error — sticky, bypasses pause, delivered first (I2).
    /// Latest error wins among errors. Never touched by value writes.
    error: Option<CaError>,
    /// Latest value deliverable now: active overflow, or the value
    /// frozen at the pause boundary (I3 pre-pause backlog).
    ready: Option<Snapshot>,
    /// Latest value that arrived DURING a pause — withheld from the
    /// consumer until `resume` collapses it into `ready` (I3 hold).
    /// A separate cell so it can never clobber a pre-pause `ready`.
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

    /// Route a value snapshot atomically against the pause flag.
    /// Out of flow control (I1).
    pub fn route_value(&self, snapshot: Snapshot) -> ValueRoute {
        let mut g = self.inner.lock().expect("CoalesceSlot mutex poisoned");
        if g.paused {
            // I3: during-pause value → `held` (separate cell). The
            // pre-pause `ready` value is untouched and stays
            // deliverable. No notify — recv is gated for `held`;
            // resume() collapses + wakes.
            g.held = Some(snapshot);
            ValueRoute::Slotted
        } else if g.ready.is_some() {
            // Active overflow — coalesce into `ready` (order-preserving:
            // a fresh value must not jump ahead of an older slotted one
            // via the channel).
            g.ready = Some(snapshot);
            drop(g);
            self.notify.notify_one();
            ValueRoute::Slotted
        } else {
            ValueRoute::TryChannel(Box::new(snapshot))
        }
    }

    /// Overflow fallback after a full channel: coalesce into `ready`
    /// (active) or `held` (if a pause raced in). Out of flow control.
    fn put_value(&self, snapshot: Snapshot) {
        let mut g = self.inner.lock().expect("CoalesceSlot mutex poisoned");
        if g.paused {
            g.held = Some(snapshot);
            // gated — no notify
        } else {
            g.ready = Some(snapshot);
            drop(g);
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

    /// Take the next deliverable item. Errors first (bypass pause, I2),
    /// then the `ready` value. The `held` value is never delivered
    /// directly — it only becomes deliverable when `resume` collapses
    /// it into `ready` (I3), so the pause gate is enforced purely by
    /// routing, not by a runtime check here.
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

    /// Set/clear the pause flag. Returns the previous value. On a
    /// true→false transition (resume) it collapses any `held` value
    /// into `ready` (latest-wins) so it becomes deliverable, and wakes
    /// `recv`. Atomic with the producer/consumer slot operations.
    pub fn set_paused(&self, paused: bool) -> bool {
        let mut g = self.inner.lock().expect("CoalesceSlot mutex poisoned");
        let prev = g.paused;
        g.paused = paused;
        let resumed = prev && !paused;
        if resumed && g.held.is_some() {
            // The during-pause value supersedes the pre-pause `ready`
            // (latest-wins coalesce), now deliverable.
            g.ready = g.held.take();
        }
        drop(g);
        if resumed {
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
