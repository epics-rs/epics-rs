//! The network context of the IOC's CA client
//! ([`epics_base_rs::server::database::CaClientContext`]): the channels a
//! record opens itself on a name its own database does not serve, run on
//! the CA links' shared client.

use std::sync::Arc;

use epics_base_rs::runtime::sync::broadcast;
use epics_base_rs::runtime::task::{Reactor, TaskHandle};
use epics_base_rs::server::database::{
    CaClientService, ClientChannel, ClientSubscription, Connection, PutCompletion,
};
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{DbFieldType, EpicsValue};

use super::CaLinkResolver;
use crate::client::{CaChannel, ConnectionEvent, EnumReadback, MonitorHandle};

#[async_trait::async_trait]
impl CaClientService for CaLinkResolver {
    async fn create_channel(&self, name: &str) -> Option<Arc<dyn ClientChannel>> {
        let client = self.client().await.ok()?;
        let channel = client.create_channel(name);
        let events = channel.connection_events();
        Some(Arc::new(NetChannel {
            reactor: client.reactor().clone(),
            channel,
            watch: tokio::sync::Mutex::new(Watch {
                events,
                reported: None,
            }),
        }))
    }
}

struct NetChannel {
    reactor: Reactor,
    channel: CaChannel,
    watch: tokio::sync::Mutex<Watch>,
}

struct Watch {
    events: broadcast::Receiver<ConnectionEvent>,
    /// The state the connection handler last reported: libca calls it per
    /// edge.
    reported: Option<Connection>,
}

/// The base DBF of a CA `DBR_*` request type.
fn dbr_base(dbr_type: u16) -> Option<DbFieldType> {
    DbFieldType::from_u16(dbr_type % 7).ok()
}

#[async_trait::async_trait]
impl ClientChannel for NetChannel {
    fn native_type(&self) -> Option<DbFieldType> {
        self.channel.native_field_type().ok()
    }

    fn element_count(&self) -> Option<u32> {
        self.channel.element_count().ok()
    }

    fn access(&self) -> (bool, bool) {
        let rights = self.channel.access_rights();
        (rights.read, rights.write)
    }

    async fn next_connection(&self) -> Option<Connection> {
        use broadcast::error::RecvError;
        let mut watch = self.watch.lock().await;
        loop {
            // The fold, not the event, is the state: an event is only the
            // wake, and a lag wakes the same way.
            let now = if self.channel.conn_fold().connected {
                Connection::Connected
            } else {
                Connection::Disconnected
            };
            let first_disconnected = watch.reported.is_none() && now == Connection::Disconnected;
            if watch.reported != Some(now) && !first_disconnected {
                watch.reported = Some(now);
                return Some(now);
            }
            match watch.events.recv().await {
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return None,
            }
        }
    }

    async fn get(&self, dbr_type: u16, count: u32) -> Result<Snapshot, String> {
        let got = self
            .channel
            .get_callback(dbr_type, count)
            .map_err(|e| e.to_string())?;
        let mut snapshot = got.await.map_err(|e| e.to_string())?;
        if let Some(base) = dbr_base(dbr_type) {
            snapshot.value = snapshot.value.convert_to(base);
        }
        Ok(snapshot)
    }

    async fn subscribe(
        &self,
        dbr_type: u16,
        count: u32,
        mask: u16,
        callback: Box<dyn Fn(Snapshot) + Send + Sync>,
    ) -> Result<ClientSubscription, String> {
        let handle = self
            .channel
            .subscribe_with_mask_readback_count(0.0, mask, EnumReadback::Native, false, Some(count))
            .await
            .map_err(|e| e.to_string())?;
        let task = self
            .reactor
            .spawn(deliver(handle, dbr_base(dbr_type), callback));
        Ok(ClientSubscription::new(Abort(task)))
    }

    async fn put(
        &self,
        value: EpicsValue,
        callback: bool,
    ) -> Result<Option<PutCompletion>, String> {
        if !callback {
            return self
                .channel
                .put_nowait(&value)
                .await
                .map(|()| None)
                .map_err(|e| e.to_string());
        }
        let done = self
            .channel
            .put_callback(&value)
            .map_err(|e| e.to_string())?;
        Ok(Some(Box::pin(async move {
            done.await.map_err(|e| e.to_string())
        })))
    }
}

/// Each value of `handle` to `callback`, as `base` when it has one.
async fn deliver(
    mut handle: MonitorHandle,
    base: Option<DbFieldType>,
    callback: Box<dyn Fn(Snapshot) + Send + Sync>,
) {
    while let Some(event) = handle.recv().await {
        // An error event (the disconnect) delivers nothing; the
        // subscription resumes on the reconnect.
        if let Ok(mut snapshot) = event {
            if let Some(base) = base {
                snapshot.value = snapshot.value.convert_to(base);
            }
            callback(snapshot);
        }
    }
}

/// A subscription's delivery task, aborted on drop.
struct Abort(TaskHandle<()>);

impl Drop for Abort {
    fn drop(&mut self) {
        self.0.abort();
    }
}
