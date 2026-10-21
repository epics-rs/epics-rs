//! A libca client running inside the IOC, as a record that holds its own
//! CA channels (sscan's `recDynLink`) uses libca: `ca_create_channel` on a
//! context `dbServiceIOInit` gave `dbContext` (`dbContext.cpp:59-66`), so a
//! name this database serves opens an in-process channel and any other one
//! goes to the network context.
//!
//! [`CaClientContext::create_channel`] is that first try. The network half
//! is the [`CaClientService`] the lset registered as `ca` offers
//! ([`LinkSet::ca_client_service`](super::LinkSet::ca_client_service)).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};

use super::ca_link::{LocalChannel, LocalMonitor};
use super::{PvDatabase, PvDatabaseInner};
use crate::runtime::task::TaskHandle;
use crate::server::snapshot::Snapshot;
use crate::types::{DbFieldType, EpicsValue};

/// A boxed future the client hands back, run wherever the caller spawns it.
pub type ClientFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// The completion of a `ca_array_put_callback`: resolves when the server
/// reports the put done, with libca's error text when it did not
/// complete. No timeout: a positioner move completes when it completes.
pub type PutCompletion = ClientFuture<Result<(), String>>;

/// A channel's connection state, as libca's connection handler reports it
/// (`cs_conn` / `cs_prev_conn`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connection {
    Connected,
    Disconnected,
}

/// One `chid`.
#[async_trait::async_trait]
pub trait ClientChannel: Send + Sync {
    /// `ca_field_type`; `None` while the channel has never connected.
    fn native_type(&self) -> Option<DbFieldType>;

    /// `ca_element_count`; `None` while the channel has never connected.
    fn element_count(&self) -> Option<u32>;

    /// `ca_read_access` / `ca_write_access`.
    fn access(&self) -> (bool, bool);

    /// The next connection-handler call. The first is the state the channel
    /// is in when it is first asked; `None` once the channel is gone.
    async fn next_connection(&self) -> Option<Connection>;

    /// `ca_array_get_callback` of `count` elements of `dbr_type`.
    async fn get(&self, dbr_type: u16, count: u32) -> Result<Snapshot, String>;

    /// `ca_add_masked_array_event` of `count` elements of `dbr_type`.
    async fn subscribe(
        &self,
        dbr_type: u16,
        count: u32,
        mask: u16,
    ) -> Result<Box<dyn ClientMonitor>, String>;

    /// `ca_array_put` (`callback == false`, completion `None`) or
    /// `ca_array_put_callback`: the put is issued when this returns, and its
    /// completion is the returned future.
    async fn put(&self, value: EpicsValue, callback: bool)
    -> Result<Option<PutCompletion>, String>;
}

/// One `evid`: each value the subscription delivers, `None` once it ended.
#[async_trait::async_trait]
pub trait ClientMonitor: Send {
    async fn next(&mut self) -> Option<Snapshot>;
}

/// The network context's half of [`CaClientContext::create_channel`].
#[async_trait::async_trait]
pub trait CaClientService: Send + Sync {
    /// `ca_create_channel` of a name this database does not serve; `None`
    /// when the client cannot start.
    async fn create_channel(&self, name: &str) -> Option<Arc<dyn ClientChannel>>;
}

/// libca's context with `dbContext` installed as its service.
#[derive(Clone)]
pub struct CaClientContext {
    db: Weak<PvDatabaseInner>,
}

impl PvDatabase {
    /// The CA client context records of this database open channels on.
    pub fn ca_client(&self) -> CaClientContext {
        CaClientContext::from_weak(Arc::downgrade(&self.inner))
    }
}

impl CaClientContext {
    pub(super) fn from_weak(db: Weak<PvDatabaseInner>) -> Self {
        Self { db }
    }

    fn db(&self) -> Option<PvDatabase> {
        self.db.upgrade().map(|inner| PvDatabase { inner })
    }

    /// `ca_create_channel`: `dbChannel_create` first, then the network
    /// context. `None` when neither exists — the database is gone, or the
    /// name is not served here and no network half is registered, a channel
    /// whose search nobody answers.
    pub async fn create_channel(&self, name: &str) -> Option<Arc<dyn ClientChannel>> {
        let db = self.db()?;
        if let Some(channel) = LocalChannel::create(&db, name.as_bytes()).await {
            return Some(Arc::new(LocalClientChannel {
                channel,
                announced: std::sync::atomic::AtomicBool::new(false),
            }));
        }
        let service = db.inner.ca_links.client_service()?;
        service.create_channel(name).await
    }

    /// Run `future` on the reactor the database's CA links run on. `None`
    /// when the database is gone or captured no reactor.
    pub fn spawn<F>(&self, future: F) -> Option<TaskHandle<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let db = self.db()?;
        let bridge = db.inner.link_puts.network()?;
        Some(bridge.spawn(future))
    }
}

/// An in-process channel: connected from creation, never disconnected,
/// full access (`dbChannelIO`).
struct LocalClientChannel {
    channel: LocalChannel,
    announced: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl ClientChannel for LocalClientChannel {
    fn native_type(&self) -> Option<DbFieldType> {
        Some(self.channel.native_type())
    }

    fn element_count(&self) -> Option<u32> {
        Some(self.channel.element_count())
    }

    fn access(&self) -> (bool, bool) {
        (true, true)
    }

    async fn next_connection(&self) -> Option<Connection> {
        if !self
            .announced
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Some(Connection::Connected);
        }
        std::future::pending().await
    }

    async fn get(&self, dbr_type: u16, count: u32) -> Result<Snapshot, String> {
        self.channel
            .read(dbr_type, count)
            .ok_or_else(|| "dbChannel_get failed".to_string())
    }

    async fn subscribe(
        &self,
        dbr_type: u16,
        count: u32,
        mask: u16,
    ) -> Result<Box<dyn ClientMonitor>, String> {
        let db = self.channel.db().ok_or("database gone")?;
        let monitor = self
            .channel
            .subscribe(&db, dbr_type, mask)
            .map_err(|e| format!("{e:?}"))?;
        Ok(Box::new(LocalClientMonitor { monitor, count }))
    }

    async fn put(
        &self,
        value: EpicsValue,
        callback: bool,
    ) -> Result<Option<PutCompletion>, String> {
        let mut value = value.into_type(self.channel.native_type());
        value.truncate(self.channel.element_count() as usize);
        self.channel.issue_put(value, callback).await
    }
}

struct LocalClientMonitor {
    monitor: LocalMonitor,
    count: u32,
}

#[async_trait::async_trait]
impl ClientMonitor for LocalClientMonitor {
    async fn next(&mut self) -> Option<Snapshot> {
        loop {
            let event = self.monitor.reader.recv().await?;
            if let Some(snapshot) = self.monitor.state_notify(event, self.count) {
                return Some(snapshot);
            }
        }
    }
}
