//! InProcessClient: direct enum pass-through to PortHandle (no serialization).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::port_handle::PortHandle;
use crate::protocol::convert::result_to_reply;
use crate::protocol::event::{EventPayload, PortEvent};
use crate::protocol::value::Timestamp;
use crate::protocol::{EventFilter, PortReply, PortRequest};
use crate::request::RequestOp;
use crate::user::AsynUser;

use super::client::{ConnectionState, RuntimeClient};
use super::error::TransportError;
use super::tracker::RequestTracker;

/// In-process transport client backed by a PortHandle.
///
/// No serialization — direct enum pass-through. This is the primary fast path.
#[derive(Clone)]
pub struct InProcessClient {
    handle: PortHandle,
    _tracker: Arc<RequestTracker>,
}

impl InProcessClient {
    pub fn new(handle: PortHandle) -> Self {
        Self {
            handle,
            _tracker: Arc::new(RequestTracker::new()),
        }
    }

    /// Access the underlying PortHandle.
    pub fn handle(&self) -> &PortHandle {
        &self.handle
    }

    fn build_user(req: &PortRequest) -> AsynUser {
        let mut user = AsynUser::new(req.meta.reason)
            .with_addr(req.meta.addr)
            .with_timeout(req.meta.timeout());
        user.priority = req.meta.priority.into();
        if let Some(token) = req.meta.block_token {
            user.block_token = Some(token);
        }
        user
    }
}

impl RuntimeClient for InProcessClient {
    fn request(
        &self,
        req: PortRequest,
    ) -> Pin<Box<dyn Future<Output = Result<PortReply, TransportError>> + Send + '_>> {
        let request_id = req.meta.request_id;
        let op = RequestOp::from(&req.command);
        let user = Self::build_user(&req);
        let handle = self.handle.clone();

        Box::pin(async move {
            let result = handle
                .submit(op, user)
                .await
                .map_err(TransportError::from)?;
            Ok(result_to_reply(&result, request_id))
        })
    }

    fn request_blocking(&self, req: PortRequest) -> Result<PortReply, TransportError> {
        let request_id = req.meta.request_id;
        let op = RequestOp::from(&req.command);
        let user = Self::build_user(&req);
        let result = self
            .handle
            .submit_blocking(op, user)
            .map_err(TransportError::from)?;
        Ok(result_to_reply(&result, request_id))
    }

    fn subscribe(
        &self,
        filter: EventFilter,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<tokio::sync::mpsc::Receiver<PortEvent>, TransportError>>
                + Send
                + '_,
        >,
    > {
        let port_name = self.handle.port_name().to_string();
        let mut broadcast_rx = self.handle.interrupts().subscribe_async();

        Box::pin(async move {
            let (tx, rx) = tokio::sync::mpsc::channel(256);

            tokio::spawn(async move {
                loop {
                    match broadcast_rx.recv().await {
                        Ok(iv) => {
                            if let Some(r) = filter.reason {
                                if iv.reason != r {
                                    continue;
                                }
                            }
                            if let Some(a) = filter.addr {
                                if iv.addr != a {
                                    continue;
                                }
                            }
                            let event = PortEvent {
                                port_name: port_name.clone(),
                                payload: EventPayload::from(&iv),
                                timestamp: Timestamp::from(iv.timestamp),
                            };
                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });

            Ok(rx)
        })
    }

    fn connection_state(&self) -> ConnectionState {
        ConnectionState::Connected
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::interrupt::InterruptValue;
    use crate::manager::PortManager;
    use crate::param::ParamType;
    use crate::port::{PortDriver, PortDriverBase, PortFlags};
    use crate::protocol::command::PortCommand;
    use crate::protocol::reply::ReplyPayload;
    use crate::protocol::request::{ProtocolPriority, RequestMeta};
    use crate::protocol::value::ParamValue;

    struct TestPort {
        base: PortDriverBase,
    }

    impl TestPort {
        fn new() -> Self {
            let mut base = PortDriverBase::new("ipc_test", 1, PortFlags::default());
            base.create_param("VAL", ParamType::Int32).unwrap();
            base.create_param("F64", ParamType::Float64).unwrap();
            base.create_param("MSG", ParamType::Octet).unwrap();
            Self { base }
        }
    }

    impl PortDriver for TestPort {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
    }

    fn make_client() -> (PortManager, InProcessClient) {
        let mgr = PortManager::new();
        let rt_handle = mgr.register_port(TestPort::new());
        let client = InProcessClient::new(rt_handle.port_handle().clone());
        (mgr, client)
    }

    fn make_request(cmd: PortCommand, reason: usize) -> PortRequest {
        PortRequest {
            meta: RequestMeta {
                request_id: 1,
                port_name: "ipc_test".into(),
                addr: 0,
                reason,
                timeout_ms: 5000,
                priority: ProtocolPriority::Medium,
                block_token: None,
            },
            command: cmd,
        }
    }

    #[tokio::test]
    async fn int32_write_read_cycle() {
        let (_mgr, client) = make_client();

        // Write
        let req = make_request(PortCommand::Int32Write { value: 42 }, 0);
        let reply = client.request(req).await.unwrap();
        assert_eq!(reply.payload, ReplyPayload::Ack);

        // Read
        let req = make_request(PortCommand::Int32Read, 0);
        let reply = client.request(req).await.unwrap();
        match reply.payload {
            ReplyPayload::Value(ParamValue::Int32(v)) => assert_eq!(v, 42),
            _ => panic!("expected Int32 value, got {:?}", reply.payload),
        }
    }

    #[tokio::test]
    async fn float64_write_read_cycle() {
        let (_mgr, client) = make_client();

        let req = make_request(PortCommand::Float64Write { value: 3.14 }, 1);
        let reply = client.request(req).await.unwrap();
        assert_eq!(reply.payload, ReplyPayload::Ack);

        let req = make_request(PortCommand::Float64Read, 1);
        let reply = client.request(req).await.unwrap();
        match reply.payload {
            ReplyPayload::Value(ParamValue::Float64(v)) => assert!((v - 3.14).abs() < 1e-10),
            _ => panic!("expected Float64 value"),
        }
    }

    #[test]
    fn blocking_int32_cycle() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (_mgr, client) = rt.block_on(async { make_client() });

        let req = make_request(PortCommand::Int32Write { value: 99 }, 0);
        let reply = client.request_blocking(req).unwrap();
        assert_eq!(reply.payload, ReplyPayload::Ack);

        let req = make_request(PortCommand::Int32Read, 0);
        let reply = client.request_blocking(req).unwrap();
        match reply.payload {
            ReplyPayload::Value(ParamValue::Int32(v)) => assert_eq!(v, 99),
            _ => panic!("expected Int32 value"),
        }
    }

    #[test]
    fn connection_state_connected() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (_mgr, client) = rt.block_on(async { make_client() });
        assert_eq!(client.connection_state(), ConnectionState::Connected);
    }

    #[tokio::test]
    async fn event_subscription() {
        let (_mgr, client) = make_client();

        let mut rx = client.subscribe(EventFilter::default()).await.unwrap();

        // Trigger an interrupt via the port handle
        let handle = client.handle();
        handle.interrupts().notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: crate::param::ParamValue::Int32(77),
            timestamp: SystemTime::now(),
        });

        let event = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(event.port_name, "ipc_test");
        match event.payload {
            EventPayload::ValueChanged {
                reason,
                addr,
                value,
            } => {
                assert_eq!(reason, 0);
                assert_eq!(addr, 0);
                assert_eq!(value, ParamValue::Int32(77));
            }
            _ => panic!("expected ValueChanged"),
        }
    }

    #[tokio::test]
    async fn event_subscription_filtered() {
        let (_mgr, client) = make_client();

        let mut rx = client
            .subscribe(EventFilter {
                reason: Some(1),
                addr: None,
            })
            .await
            .unwrap();

        let handle = client.handle();

        // Send reason=0, should be filtered out
        handle.interrupts().notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: crate::param::ParamValue::Int32(10),
            timestamp: SystemTime::now(),
        });

        // Send reason=1, should pass
        handle.interrupts().notify(InterruptValue {
            reason: 1,
            addr: 0,
            value: crate::param::ParamValue::Int32(20),
            timestamp: SystemTime::now(),
        });

        let event = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();

        match event.payload {
            EventPayload::ValueChanged { reason, value, .. } => {
                assert_eq!(reason, 1);
                assert_eq!(value, ParamValue::Int32(20));
            }
            _ => panic!("expected ValueChanged"),
        }
    }
}
