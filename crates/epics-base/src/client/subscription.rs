use std::collections::HashMap;

use crate::runtime::sync::mpsc;

use crate::error::CaResult;
use crate::types::{DbFieldType, EpicsValue};

use super::types::TransportCommand;

pub(crate) struct SubscriptionRecord {
    pub subid: u32,
    pub cid: u32,
    pub data_type: u16,
    pub count: u32,
    pub mask: u16,
    pub callback_tx: mpsc::UnboundedSender<CaResult<EpicsValue>>,
    pub needs_restore: bool,
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

    pub fn add(
        &mut self,
        subid: u32,
        cid: u32,
        data_type: u16,
        count: u32,
        mask: u16,
        callback_tx: mpsc::UnboundedSender<CaResult<EpicsValue>>,
    ) {
        self.subscriptions.insert(
            subid,
            SubscriptionRecord {
                subid,
                cid,
                data_type,
                count,
                mask,
                callback_tx,
                needs_restore: false,
            },
        );
    }

    pub fn remove(&mut self, subid: u32) -> Option<SubscriptionRecord> {
        self.subscriptions.remove(&subid)
    }

    pub fn on_monitor_data(&self, subid: u32, data_type: u16, count: u32, data: &[u8]) {
        if let Some(rec) = self.subscriptions.get(&subid) {
            let dbr_type = match DbFieldType::from_u16(data_type) {
                Ok(t) => t,
                Err(e) => {
                    let _ = rec.callback_tx.send(Err(e));
                    return;
                }
            };
            let value = EpicsValue::from_bytes_array(dbr_type, data, count as usize);
            let _ = rec.callback_tx.send(value);
        }
    }

    /// Mark all subscriptions for a given server's channels as needing restore.
    /// Returns the cids that were affected.
    pub fn mark_disconnected(&mut self, cids: &[u32]) {
        for rec in self.subscriptions.values_mut() {
            if cids.contains(&rec.cid) {
                rec.needs_restore = true;
            }
        }
    }

    /// Generate restore commands for subscriptions tied to the given cid,
    /// using the new sid.
    pub fn restore_for_channel(
        &mut self,
        cid: u32,
        new_sid: u32,
        server_addr: std::net::SocketAddr,
        transport_tx: &mpsc::UnboundedSender<TransportCommand>,
    ) {
        for rec in self.subscriptions.values_mut() {
            if rec.cid == cid && rec.needs_restore {
                rec.needs_restore = false;
                let _ = transport_tx.send(TransportCommand::Subscribe {
                    sid: new_sid,
                    data_type: rec.data_type,
                    count: rec.count,
                    subid: rec.subid,
                    mask: rec.mask,
                    server_addr,
                });
            }
        }
    }

    /// Check if any subscription for a cid has a closed callback_tx (receiver dropped).
    /// Remove them and return the subids.
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
