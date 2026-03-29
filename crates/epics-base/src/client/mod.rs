mod search;
mod state;
mod subscription;
mod transport;
mod types;

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};


use std::time::Duration;

use crate::runtime::sync::{broadcast, mpsc, oneshot};

use crate::channel::{alloc_cid, alloc_ioid, alloc_subid, AccessRights, ChannelInfo};
use crate::error::{CaError, CaResult};
use crate::protocol::*;
use crate::repeater;
use crate::server::snapshot::{DbrClass, Snapshot};
use crate::types::{DbFieldType, EpicsValue, decode_dbr};

pub use state::{ChannelState, ConnectionEvent};

use state::ChannelInner;
use subscription::SubscriptionRegistry;
use types::*;

/// CA client with persistent channels and auto-reconnection.
pub struct CaClient {
    search_tx: mpsc::UnboundedSender<SearchRequest>,
    transport_tx: mpsc::UnboundedSender<TransportCommand>,
    coord_tx: mpsc::UnboundedSender<CoordRequest>,
    _coordinator: tokio::task::JoinHandle<()>,
    _search_task: tokio::task::JoinHandle<()>,
    _transport_task: tokio::task::JoinHandle<()>,
}

/// Internal coordinator requests from CaChannel / public API
#[allow(dead_code)]
enum CoordRequest {
    RegisterChannel {
        cid: u32,
        pv_name: String,
        conn_tx: broadcast::Sender<ConnectionEvent>,
    },
    WaitConnected {
        cid: u32,
        reply: oneshot::Sender<()>,
    },
    GetChannelInfo {
        cid: u32,
        reply: oneshot::Sender<Option<ChannelSnapshot>>,
    },
    Subscribe {
        cid: u32,
        subid: u32,
        mask: u16,
        callback_tx: mpsc::UnboundedSender<CaResult<EpicsValue>>,
        reply: oneshot::Sender<CaResult<()>>,
    },
    Unsubscribe {
        subid: u32,
    },
    DropChannel {
        cid: u32,
    },
    ReadNotify {
        cid: u32,
        ioid: u32,
        reply: oneshot::Sender<CaResult<(u16, u32, Vec<u8>)>>,
    },
    WriteNotify {
        cid: u32,
        ioid: u32,
        value: EpicsValue,
        reply: oneshot::Sender<CaResult<()>>,
    },
}

#[derive(Clone)]
struct ChannelSnapshot {
    sid: u32,
    native_type: DbFieldType,
    element_count: u32,
    server_addr: SocketAddr,
    access_rights: AccessRights,
    state: ChannelState,
    pv_name: String,
}

impl CaClient {
    pub async fn new() -> CaResult<Self> {
        // Run repeater registration in background — don't block client startup.
        crate::runtime::task::spawn(async { repeater::ensure_repeater().await });

        let addr_list = parse_addr_list()?;

        let (search_tx, search_rx) = mpsc::unbounded_channel();
        let (search_resp_tx, search_resp_rx) = mpsc::unbounded_channel();

        let (transport_tx, transport_rx) = mpsc::unbounded_channel();
        let (transport_evt_tx, transport_evt_rx) = mpsc::unbounded_channel();

        let (coord_tx, coord_rx) = mpsc::unbounded_channel();

        let search_task = crate::runtime::task::spawn(search::run_search_engine(
            addr_list,
            search_rx,
            search_resp_tx,
        ));

        let transport_task = crate::runtime::task::spawn(transport::run_transport_manager(
            transport_rx,
            transport_evt_tx,
        ));

        let coordinator = crate::runtime::task::spawn(run_coordinator(
            coord_rx,
            search_resp_rx,
            transport_evt_rx,
            search_tx.clone(),
            transport_tx.clone(),
        ));

        Ok(Self {
            search_tx,
            transport_tx,
            coord_tx,
            _coordinator: coordinator,
            _search_task: search_task,
            _transport_task: transport_task,
        })
    }

    /// Create a persistent channel. Returns immediately (starts searching in background).
    pub fn create_channel(&self, name: &str) -> CaChannel {
        let cid = alloc_cid();
        let (conn_tx, _) = broadcast::channel(16);

        let _ = self.coord_tx.send(CoordRequest::RegisterChannel {
            cid,
            pv_name: name.to_string(),
            conn_tx: conn_tx.clone(),
        });

        let _ = self.search_tx.send(SearchRequest::Search {
            cid,
            pv_name: name.to_string(),
        });

        CaChannel {
            cid,
            pv_name: name.to_string(),
            coord_tx: self.coord_tx.clone(),
            transport_tx: self.transport_tx.clone(),
            conn_tx,
        }
    }

    // --- Legacy one-shot API (backwards-compatible) ---

    pub async fn caget(&self, pv_name: &str) -> CaResult<(DbFieldType, EpicsValue)> {
        let ch = self.create_channel(pv_name);
        ch.wait_connected(Duration::from_secs(3)).await?;
        let result = ch.get().await;
        let _ = self.coord_tx.send(CoordRequest::DropChannel { cid: ch.cid });
        result
    }

    /// Fire-and-forget write (CA_PROTO_WRITE). Matches C `caput` behavior.
    pub async fn caput(&self, pv_name: &str, value_str: &str) -> CaResult<()> {
        let ch = self.create_channel(pv_name);
        ch.wait_connected(Duration::from_secs(3)).await?;

        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::GetChannelInfo {
            cid: ch.cid,
            reply: reply_tx,
        });
        let snap = reply_rx
            .await
            .map_err(|_| CaError::Shutdown)?
            .ok_or(CaError::Disconnected)?;

        let value = EpicsValue::parse(snap.native_type, value_str)?;
        ch.put_nowait(&value).await?;
        let _ = self.coord_tx.send(CoordRequest::DropChannel { cid: ch.cid });
        Ok(())
    }

    /// Write with completion callback (CA_PROTO_WRITE_NOTIFY). Matches C `caput -c`.
    pub async fn caput_callback(&self, pv_name: &str, value_str: &str, timeout_secs: f64) -> CaResult<()> {
        let ch = self.create_channel(pv_name);
        ch.wait_connected(Duration::from_secs(3)).await?;

        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::GetChannelInfo {
            cid: ch.cid,
            reply: reply_tx,
        });
        let snap = reply_rx
            .await
            .map_err(|_| CaError::Shutdown)?
            .ok_or(CaError::Disconnected)?;

        let value = EpicsValue::parse(snap.native_type, value_str)?;
        ch.put_with_timeout(&value, Duration::from_secs_f64(timeout_secs)).await?;
        let _ = self.coord_tx.send(CoordRequest::DropChannel { cid: ch.cid });
        Ok(())
    }

    pub async fn cainfo(&self, pv_name: &str) -> CaResult<ChannelInfo> {
        let ch = self.create_channel(pv_name);
        ch.wait_connected(Duration::from_secs(3)).await?;

        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::GetChannelInfo {
            cid: ch.cid,
            reply: reply_tx,
        });
        let snap = reply_rx
            .await
            .map_err(|_| CaError::Shutdown)?
            .ok_or(CaError::Disconnected)?;

        let _ = self.coord_tx.send(CoordRequest::DropChannel { cid: ch.cid });

        Ok(ChannelInfo {
            pv_name: snap.pv_name,
            server_addr: snap.server_addr,
            native_type: snap.native_type,
            element_count: snap.element_count,
            access_rights: snap.access_rights,
        })
    }

    /// Monitor a PV with callback (legacy API).
    pub async fn camonitor<F>(&self, pv_name: &str, mut callback: F) -> CaResult<()>
    where
        F: FnMut(EpicsValue),
    {
        let ch = self.create_channel(pv_name);
        let mut monitor = ch.subscribe().await?;

        while let Some(result) = monitor.recv().await {
            match result {
                Ok(value) => callback(value),
                Err(e) => return Err(e),
            }
        }

        Ok(())
    }
}

/// A persistent CA channel with auto-reconnection.
#[derive(Clone)]
pub struct CaChannel {
    cid: u32,
    pv_name: String,
    coord_tx: mpsc::UnboundedSender<CoordRequest>,
    transport_tx: mpsc::UnboundedSender<TransportCommand>,
    conn_tx: broadcast::Sender<ConnectionEvent>,
}

impl CaChannel {
    pub async fn wait_connected(&self, timeout: Duration) -> CaResult<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::WaitConnected {
            cid: self.cid,
            reply: reply_tx,
        });
        tokio::time::timeout(timeout, reply_rx)
            .await
            .map_err(|_| CaError::ChannelNotFound(self.pv_name.clone()))?
            .map_err(|_| CaError::Shutdown)
    }

    pub async fn get(&self) -> CaResult<(DbFieldType, EpicsValue)> {
        let (info_tx, info_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::GetChannelInfo {
            cid: self.cid,
            reply: info_tx,
        });
        let snap = info_rx
            .await
            .map_err(|_| CaError::Shutdown)?
            .ok_or(CaError::Disconnected)?;

        if snap.state != ChannelState::Connected {
            return Err(CaError::Disconnected);
        }

        let ioid = alloc_ioid();
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::ReadNotify {
            cid: self.cid,
            ioid,
            reply: reply_tx,
        });

        let _ = self.transport_tx.send(TransportCommand::ReadNotify {
            sid: snap.sid,
            data_type: snap.native_type as u16,
            count: snap.element_count,
            ioid,
            server_addr: snap.server_addr,
        });

        let (data_type, count, data) = tokio::time::timeout(Duration::from_secs(5), reply_rx)
            .await
            .map_err(|_| CaError::Timeout)?
            .map_err(|_| CaError::Shutdown)??;

        let dbr_type = DbFieldType::from_u16(data_type)?;
        let value = EpicsValue::from_bytes_array(dbr_type, &data, count as usize)?;
        Ok((dbr_type, value))
    }

    /// Get a PV value with metadata. Use `DbrClass::Time` for timestamp + alarm,
    /// or `DbrClass::Ctrl` for full control metadata (units, limits, precision).
    pub async fn get_with_metadata(&self, class: DbrClass) -> CaResult<Snapshot> {
        let (info_tx, info_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::GetChannelInfo {
            cid: self.cid,
            reply: info_tx,
        });
        let snap = info_rx
            .await
            .map_err(|_| CaError::Shutdown)?
            .ok_or(CaError::Disconnected)?;

        if snap.state != ChannelState::Connected {
            return Err(CaError::Disconnected);
        }

        let native = DbFieldType::from_u16(snap.native_type as u16)?;
        let request_type = match class {
            DbrClass::Time => native.time_dbr_type(),
            DbrClass::Ctrl => native.ctrl_dbr_type(),
            DbrClass::Sts => native as u16 + 7,
            DbrClass::Gr => native as u16 + 21,
            DbrClass::Plain => native as u16,
        };

        let ioid = alloc_ioid();
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::ReadNotify {
            cid: self.cid,
            ioid,
            reply: reply_tx,
        });

        let _ = self.transport_tx.send(TransportCommand::ReadNotify {
            sid: snap.sid,
            data_type: request_type,
            count: snap.element_count,
            ioid,
            server_addr: snap.server_addr,
        });

        let (data_type, count, data) = tokio::time::timeout(Duration::from_secs(5), reply_rx)
            .await
            .map_err(|_| CaError::Timeout)?
            .map_err(|_| CaError::Shutdown)??;

        decode_dbr(data_type, &data, count as usize)
    }

    pub async fn put(&self, value: &EpicsValue) -> CaResult<()> {
        let (info_tx, info_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::GetChannelInfo {
            cid: self.cid,
            reply: info_tx,
        });
        let snap = info_rx
            .await
            .map_err(|_| CaError::Shutdown)?
            .ok_or(CaError::Disconnected)?;

        if snap.state != ChannelState::Connected {
            return Err(CaError::Disconnected);
        }

        let ioid = alloc_ioid();
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::WriteNotify {
            cid: self.cid,
            ioid,
            value: value.clone(),
            reply: reply_tx,
        });

        let payload = value.to_bytes();
        let count = value.count() as u32;
        let _ = self.transport_tx.send(TransportCommand::WriteNotify {
            sid: snap.sid,
            data_type: snap.native_type as u16,
            count,
            ioid,
            payload,
            server_addr: snap.server_addr,
        });

        tokio::time::timeout(Duration::from_secs(5), reply_rx)
            .await
            .map_err(|_| CaError::Timeout)?
            .map_err(|_| CaError::Shutdown)?
    }

    /// Write with completion callback and configurable timeout.
    pub async fn put_with_timeout(&self, value: &EpicsValue, timeout: Duration) -> CaResult<()> {
        let (info_tx, info_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::GetChannelInfo {
            cid: self.cid,
            reply: info_tx,
        });
        let snap = info_rx
            .await
            .map_err(|_| CaError::Shutdown)?
            .ok_or(CaError::Disconnected)?;

        if snap.state != ChannelState::Connected {
            return Err(CaError::Disconnected);
        }

        let ioid = alloc_ioid();
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::WriteNotify {
            cid: self.cid,
            ioid,
            value: value.clone(),
            reply: reply_tx,
        });

        let payload = value.to_bytes();
        let count = value.count() as u32;
        let _ = self.transport_tx.send(TransportCommand::WriteNotify {
            sid: snap.sid,
            data_type: snap.native_type as u16,
            count,
            ioid,
            payload,
            server_addr: snap.server_addr,
        });

        tokio::time::timeout(timeout, reply_rx)
            .await
            .map_err(|_| CaError::Timeout)?
            .map_err(|_| CaError::Shutdown)?
    }

    /// Fire-and-forget put (CA_PROTO_WRITE). Returns immediately without
    /// waiting for server acknowledgement. Used by ophyd's EpicsMotor.set()
    /// which monitors DMOV for completion instead.
    pub async fn put_nowait(&self, value: &EpicsValue) -> CaResult<()> {
        let (info_tx, info_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::GetChannelInfo {
            cid: self.cid,
            reply: info_tx,
        });
        let snap = info_rx
            .await
            .map_err(|_| CaError::Shutdown)?
            .ok_or(CaError::Disconnected)?;

        if snap.state != ChannelState::Connected {
            return Err(CaError::Disconnected);
        }

        let payload = value.to_bytes();
        let count = value.count() as u32;
        let _ = self.transport_tx.send(TransportCommand::Write {
            sid: snap.sid,
            data_type: snap.native_type as u16,
            count,
            payload,
            server_addr: snap.server_addr,
        });

        Ok(())
    }

    pub async fn subscribe(&self) -> CaResult<MonitorHandle> {
        // Wait for connection first
        self.wait_connected(Duration::from_secs(5)).await?;

        let subid = alloc_subid();
        let (callback_tx, callback_rx) = mpsc::unbounded_channel();

        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.coord_tx.send(CoordRequest::Subscribe {
            cid: self.cid,
            subid,
            mask: DBE_VALUE | DBE_ALARM,
            callback_tx,
            reply: reply_tx,
        });

        reply_rx.await.map_err(|_| CaError::Shutdown)??;

        Ok(MonitorHandle {
            subid,
            callback_rx,
            coord_tx: self.coord_tx.clone(),
        })
    }

    pub fn connection_events(&self) -> broadcast::Receiver<ConnectionEvent> {
        self.conn_tx.subscribe()
    }
}

impl Drop for CaChannel {
    fn drop(&mut self) {
        let _ = self.coord_tx.send(CoordRequest::DropChannel { cid: self.cid });
    }
}

/// Handle for a monitor subscription. Dropping it cancels the subscription.
pub struct MonitorHandle {
    subid: u32,
    callback_rx: mpsc::UnboundedReceiver<CaResult<EpicsValue>>,
    coord_tx: mpsc::UnboundedSender<CoordRequest>,
}

impl MonitorHandle {
    pub async fn recv(&mut self) -> Option<CaResult<EpicsValue>> {
        self.callback_rx.recv().await
    }
}

impl Drop for MonitorHandle {
    fn drop(&mut self) {
        let _ = self.coord_tx.send(CoordRequest::Unsubscribe {
            subid: self.subid,
        });
    }
}

// --- Coordinator ---

async fn run_coordinator(
    mut coord_rx: mpsc::UnboundedReceiver<CoordRequest>,
    mut search_rx: mpsc::UnboundedReceiver<SearchResponse>,
    mut transport_rx: mpsc::UnboundedReceiver<TransportEvent>,
    search_tx: mpsc::UnboundedSender<SearchRequest>,
    transport_tx: mpsc::UnboundedSender<TransportCommand>,
) {
    let mut channels: HashMap<u32, ChannelInner> = HashMap::new();
    let mut subscriptions = SubscriptionRegistry::new();
    let mut read_waiters: HashMap<u32, oneshot::Sender<CaResult<(u16, u32, Vec<u8>)>>> = HashMap::new();
    let mut write_waiters: HashMap<u32, oneshot::Sender<CaResult<()>>> = HashMap::new();

    loop {
        tokio::select! {
            req = coord_rx.recv() => {
                let Some(req) = req else { return };
                match req {
                    CoordRequest::RegisterChannel { cid, pv_name, conn_tx } => {
                        channels.insert(cid, ChannelInner {
                            cid,
                            pv_name,
                            state: ChannelState::Searching,
                            sid: 0,
                            native_type: None,
                            element_count: 0,
                            server_addr: None,
                            access_rights: AccessRights::from_u32(0),
                            connect_waiters: Vec::new(),
                            conn_tx,
                        });
                    }
                    CoordRequest::WaitConnected { cid, reply } => {
                        if let Some(ch) = channels.get_mut(&cid) {
                            if ch.state == ChannelState::Connected {
                                let _ = reply.send(());
                            } else {
                                ch.connect_waiters.push(reply);
                            }
                        } else {
                            let _ = reply.send(());
                        }
                    }
                    CoordRequest::GetChannelInfo { cid, reply } => {
                        let snap = channels.get(&cid).and_then(|ch| {
                            Some(ChannelSnapshot {
                                sid: ch.sid,
                                native_type: ch.native_type?,
                                element_count: ch.element_count,
                                server_addr: ch.server_addr?,
                                access_rights: ch.access_rights,
                                state: ch.state,
                                pv_name: ch.pv_name.clone(),
                            })
                        });
                        let _ = reply.send(snap);
                    }
                    CoordRequest::Subscribe { cid, subid, mask, callback_tx, reply } => {
                        if let Some(ch) = channels.get(&cid) {
                            if ch.state == ChannelState::Connected {
                                let native_type = ch.native_type.unwrap() as u16;
                                let count = ch.element_count;
                                let sid = ch.sid;
                                let server_addr = ch.server_addr.unwrap();

                                subscriptions.add(subid, cid, native_type, count, mask, callback_tx);

                                let _ = transport_tx.send(TransportCommand::Subscribe {
                                    sid,
                                    data_type: native_type,
                                    count,
                                    subid,
                                    mask,
                                    server_addr,
                                });
                                let _ = reply.send(Ok(()));
                            } else {
                                let _ = reply.send(Err(CaError::Disconnected));
                            }
                        } else {
                            let _ = reply.send(Err(CaError::Disconnected));
                        }
                    }
                    CoordRequest::Unsubscribe { subid } => {
                        if let Some(rec) = subscriptions.get(subid) {
                            let cid = rec.cid;
                            if let Some(ch) = channels.get(&cid) {
                                if ch.state == ChannelState::Connected {
                                    let _ = transport_tx.send(TransportCommand::Unsubscribe {
                                        sid: ch.sid,
                                        subid,
                                        data_type: rec.data_type,
                                        server_addr: ch.server_addr.unwrap(),
                                    });
                                }
                            }
                        }
                        subscriptions.remove(subid);
                    }
                    CoordRequest::DropChannel { cid } => {
                        // Cancel all subscriptions for this channel
                        let sub_ids = subscriptions.for_cid(cid);
                        for subid in sub_ids {
                            if let Some(rec) = subscriptions.get(subid) {
                                if let Some(ch) = channels.get(&cid) {
                                    if ch.state == ChannelState::Connected {
                                        let _ = transport_tx.send(TransportCommand::Unsubscribe {
                                            sid: ch.sid,
                                            subid,
                                            data_type: rec.data_type,
                                            server_addr: ch.server_addr.unwrap(),
                                        });
                                    }
                                }
                            }
                            subscriptions.remove(subid);
                        }

                        // Clear channel on server
                        if let Some(ch) = channels.get(&cid) {
                            if ch.state == ChannelState::Connected {
                                let _ = transport_tx.send(TransportCommand::ClearChannel {
                                    cid,
                                    sid: ch.sid,
                                    server_addr: ch.server_addr.unwrap(),
                                });
                            }
                            // Cancel search if still searching
                            if ch.state == ChannelState::Searching {
                                let _ = search_tx.send(SearchRequest::Cancel { cid });
                            }
                        }
                        channels.remove(&cid);
                    }
                    CoordRequest::ReadNotify { cid: _, ioid, reply } => {
                        read_waiters.insert(ioid, reply);
                    }
                    CoordRequest::WriteNotify { cid: _, ioid, value: _, reply } => {
                        write_waiters.insert(ioid, reply);
                    }
                }
            }
            resp = search_rx.recv() => {
                let Some(resp) = resp else { return };
                match resp {
                    SearchResponse::Found { cid, server_addr } => {
                        if let Some(ch) = channels.get_mut(&cid) {
                            if ch.state == ChannelState::Searching || ch.state == ChannelState::Disconnected {
                                ch.state = ChannelState::Connecting;
                                ch.server_addr = Some(server_addr);
                                let _ = transport_tx.send(TransportCommand::CreateChannel {
                                    cid,
                                    pv_name: ch.pv_name.clone(),
                                    server_addr,
                                });
                            }
                        }
                    }
                }
            }
            evt = transport_rx.recv() => {
                let Some(evt) = evt else { return };
                match evt {
                    TransportEvent::ChannelCreated { cid, sid, data_type, element_count, access, server_addr } => {
                        if let Some(ch) = channels.get_mut(&cid) {
                            let dbr_type = DbFieldType::from_u16(data_type).ok();
                            ch.state = ChannelState::Connected;
                            ch.sid = sid;
                            ch.native_type = dbr_type;
                            ch.element_count = element_count;
                            ch.server_addr = Some(server_addr);
                            ch.access_rights = access;

                            // Wake connect waiters
                            for waiter in ch.connect_waiters.drain(..) {
                                let _ = waiter.send(());
                            }

                            // Broadcast connected event
                            let _ = ch.conn_tx.send(ConnectionEvent::Connected);

                            // Restore subscriptions
                            subscriptions.restore_for_channel(cid, sid, server_addr, &transport_tx);
                        }
                    }
                    TransportEvent::ReadResponse { ioid, data_type, count, data } => {
                        if let Some(waiter) = read_waiters.remove(&ioid) {
                            let _ = waiter.send(Ok((data_type, count, data)));
                        }
                    }
                    TransportEvent::ReadError { ioid, eca_status } => {
                        if let Some(waiter) = read_waiters.remove(&ioid) {
                            let _ = waiter.send(Err(CaError::Protocol(
                                format!("server returned ECA error {eca_status:#06x}")
                            )));
                        }
                    }
                    TransportEvent::WriteResponse { ioid, status } => {
                        if let Some(waiter) = write_waiters.remove(&ioid) {
                            if status == 1 || status == ECA_NORMAL {
                                let _ = waiter.send(Ok(()));
                            } else {
                                let _ = waiter.send(Err(CaError::WriteFailed(status)));
                            }
                        }
                    }
                    TransportEvent::MonitorData { subid, data_type, count, data } => {
                        subscriptions.on_monitor_data(subid, data_type, count, &data);
                    }
                    TransportEvent::AccessRightsChanged { cid, access } => {
                        if let Some(ch) = channels.get_mut(&cid) {
                            ch.access_rights = access;
                            let _ = ch.conn_tx.send(ConnectionEvent::AccessRightsChanged {
                                read: access.read,
                                write: access.write,
                            });
                        }
                    }
                    TransportEvent::ChannelCreateFailed { cid } => {
                        if let Some(ch) = channels.get_mut(&cid) {
                            // Fail all connect waiters
                            for waiter in ch.connect_waiters.drain(..) {
                                let _ = waiter.send(());
                            }
                            // Transition to Disconnected and re-search
                            ch.state = ChannelState::Disconnected;
                            let _ = ch.conn_tx.send(ConnectionEvent::Disconnected);
                            let _ = search_tx.send(SearchRequest::Search {
                                cid,
                                pv_name: ch.pv_name.clone(),
                            });
                        }
                    }
                    TransportEvent::ServerError { .. } => {
                        // Logged in transport layer; no further action needed
                    }
                    TransportEvent::TcpClosed { server_addr } => {
                        handle_disconnect(&mut channels, &mut subscriptions, &search_tx, server_addr);
                    }
                    TransportEvent::ServerDisconnect { cid, server_addr } => {
                        // Single channel disconnect (CA_PROTO_SERVER_DISCONN)
                        if let Some(ch) = channels.get_mut(&cid) {
                            if ch.server_addr == Some(server_addr) {
                                ch.state = ChannelState::Disconnected;
                                let _ = ch.conn_tx.send(ConnectionEvent::Disconnected);

                                let cids = vec![cid];
                                subscriptions.mark_disconnected(&cids);

                                // Re-search
                                let _ = search_tx.send(SearchRequest::Search {
                                    cid,
                                    pv_name: ch.pv_name.clone(),
                                });
                            }
                        }
                    }
                }
            }
        }
    }
}

fn handle_disconnect(
    channels: &mut HashMap<u32, ChannelInner>,
    subscriptions: &mut SubscriptionRegistry,
    search_tx: &mpsc::UnboundedSender<SearchRequest>,
    server_addr: SocketAddr,
) {
    let mut affected_cids = Vec::new();

    for ch in channels.values_mut() {
        if ch.server_addr == Some(server_addr)
            && (ch.state == ChannelState::Connected || ch.state == ChannelState::Connecting)
        {
            ch.state = ChannelState::Disconnected;
            affected_cids.push(ch.cid);
            let _ = ch.conn_tx.send(ConnectionEvent::Disconnected);

            // Re-search
            let _ = search_tx.send(SearchRequest::Search {
                cid: ch.cid,
                pv_name: ch.pv_name.clone(),
            });
        }
    }

    subscriptions.mark_disconnected(&affected_cids);
}

fn resolve_host(host: &str, port: u16) -> CaResult<SocketAddr> {
    // Try direct IP parse first (fast path)
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)));
    }
    // DNS resolution — prefer IPv4 (CA protocol is IPv4-only)
    use std::net::ToSocketAddrs;
    let addr_str = format!("{host}:{port}");
    let addrs: Vec<SocketAddr> = addr_str
        .to_socket_addrs()
        .map_err(|e| CaError::Protocol(format!("cannot resolve '{host}': {e}")))?
        .collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or(addrs.first())
        .copied()
        .ok_or_else(|| CaError::Protocol(format!("no addresses for '{host}'")))
}

fn parse_addr_list() -> CaResult<Vec<SocketAddr>> {
    let mut addrs = Vec::new();

    if let Some(list) = crate::runtime::env::get("EPICS_CA_ADDR_LIST") {
        for entry in list.split_whitespace() {
            let addr = if entry.contains(':') {
                // Try direct parse first, fall back to DNS resolution
                entry.parse::<SocketAddr>().or_else(|_| {
                    let (host, port_str) = entry.rsplit_once(':').unwrap();
                    let port: u16 = port_str
                        .parse()
                        .map_err(|e| CaError::Protocol(format!("bad port in '{entry}': {e}")))?;
                    resolve_host(host, port)
                })?
            } else {
                resolve_host(entry, CA_SERVER_PORT)?
            };
            addrs.push(addr);
        }
    }

    let auto_addr = crate::runtime::env::get_or("EPICS_CA_AUTO_ADDR_LIST", "YES");

    if auto_addr.eq_ignore_ascii_case("YES") || addrs.is_empty() {
        addrs.push(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::BROADCAST,
            CA_SERVER_PORT,
        )));
    }

    Ok(addrs)
}
