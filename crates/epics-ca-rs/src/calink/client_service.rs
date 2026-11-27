//! The network context of the IOC's CA client
//! ([`epics_base_rs::server::database::CaClientContext`]): the channels a
//! record opens itself on a name its own database does not serve, run on
//! the CA links' shared client.

use std::sync::Arc;

use futures_util::StreamExt;

use epics_base_rs::runtime::sync::broadcast;
use epics_base_rs::server::database::{
    CaClientService, ClientChannel, ClientSubscription, Connection, InProcessEvents, PutCompletion,
};
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{DbFieldType, EpicsValue};

use super::CaLinkResolver;
use crate::client::{CaChannel, ConnectionEvent, DbrType, MonitorHandle};

#[async_trait::async_trait]
impl CaClientService for CaLinkResolver {
    async fn create_channel(
        &self,
        name: &str,
        events: &InProcessEvents,
    ) -> Option<Arc<dyn ClientChannel>> {
        let client = self.client().await.ok()?;
        let channel = client.create_channel(name);
        let connection = channel.connection_events();
        Some(Arc::new(NetChannel {
            events: events.clone(),
            channel,
            watch: tokio::sync::Mutex::new(Watch {
                events: connection,
                reported: None,
            }),
        }))
    }
}

struct NetChannel {
    /// Its context's: its monitors deliver there.
    events: InProcessEvents,
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
        let dbr_type = DbrType::new(dbr_type).map_err(|e| e.to_string())?;
        let handle = self
            .channel
            .subscribe_with_mask_dbr_type(mask, dbr_type, count)
            .await
            .map_err(|e| e.to_string())?;
        let mut values = Box::pin(values(handle));
        self.events
            .subscribe_network(Box::new(move |cx| values.poll_next_unpin(cx)), callback)
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

/// Each value of `handle`.
fn values(handle: MonitorHandle) -> impl futures_util::Stream<Item = Snapshot> + Send {
    futures_util::stream::unfold(handle, |mut handle| async move {
        loop {
            // An error event (the disconnect) delivers nothing; the
            // subscription resumes on the reconnect.
            if let Ok(snapshot) = handle.recv().await? {
                return Some((snapshot, handle));
            }
        }
    })
}
