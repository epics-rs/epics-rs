use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::SystemTime;

use epics_base_rs::runtime::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{DbFieldType, EpicsValue, decode_dbr};

use super::types::TransportCommand;

/// Outcome of `on_monitor_data` — lets the coordinator decide whether to
/// bump flow control or the dropped counter.
pub(crate) enum MonitorDeliveryOutcome {
    /// Snapshot was queued to the application; caller should increment
    /// per-server flow control outstanding count.
    Queued(SocketAddr),
    /// Snapshot was dropped (queue full or callback closed). Caller should
    /// bump the dropped counter and ideally request EVENTS_OFF.
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
    pub needs_restore: bool,
    /// Client-side deadband: suppress callback if |new - old| < deadband.
    pub deadband: f64,
    /// Last delivered scalar value (for deadband filtering).
    pub last_value: Option<f64>,
    /// Number of monitor updates queued to the application but not yet consumed.
    pub pending_deliveries: usize,
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

        match rec.callback_tx.try_send(Ok(snapshot)) {
            Ok(()) => {
                rec.pending_deliveries += 1;
                MonitorDeliveryOutcome::Queued(server_addr)
            }
            Err(TrySendError::Full(_)) => MonitorDeliveryOutcome::Dropped(server_addr),
            Err(TrySendError::Closed(_)) => MonitorDeliveryOutcome::Dropped(server_addr),
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
    pub fn mark_disconnected(&mut self, cids: &[u32]) -> HashMap<SocketAddr, usize> {
        use epics_base_rs::error::CaError;
        const ECA_DISCONN: u32 = 192; // protocol::ECA_DISCONN
        let mut cleared = HashMap::new();
        for rec in self.subscriptions.values_mut() {
            if cids.contains(&rec.cid) {
                rec.needs_restore = true;
                if rec.pending_deliveries > 0 {
                    *cleared.entry(rec.server_addr).or_insert(0) += rec.pending_deliveries;
                }
                rec.pending_deliveries = 0;
                // Best-effort: a closed receiver silently drops, same
                // as `try_deliver_err`. We don't return this through
                // MonitorDeliveryOutcome because the caller is the
                // disconnect bookkeeping path, not a per-frame router.
                let _ = rec
                    .callback_tx
                    .try_send(Err(CaError::ServerError(ECA_DISCONN)));
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

/// Helper to deliver an error result. Best-effort: if the queue is full or
/// the consumer dropped, the error is silently lost (the next successful
/// delivery will be a fresh snapshot, which is more useful than a stale
/// decode error).
fn try_deliver_err(
    rec: &mut SubscriptionRecord,
    err: epics_base_rs::error::CaError,
    server_addr: SocketAddr,
) -> MonitorDeliveryOutcome {
    match rec.callback_tx.try_send(Err(err)) {
        Ok(()) => {
            rec.pending_deliveries += 1;
            MonitorDeliveryOutcome::Queued(server_addr)
        }
        Err(_) => MonitorDeliveryOutcome::Dropped(server_addr),
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
            needs_restore: true,
            deadband: 0.0,
            last_value: None,
            pending_deliveries: 0,
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
