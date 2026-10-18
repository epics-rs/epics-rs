//! InProcessClient: direct enum pass-through to PortHandle (no serialization).

// RTEMS-EXEC-MODEL-ALLOW(9): checked, not waived — all 9 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p asyn-rs
// --all-features`, 1081/1081). asyn-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::interrupt::InterruptFilter;
use crate::port_handle::PortHandle;
use crate::protocol::convert::result_to_reply;
use crate::protocol::{EventFilter, EventRing, PortReply, PortRequest};
use crate::request::RequestOp;
use crate::user::AsynUser;

use super::client::{ConnectionState, EventReceiver, RuntimeClient};
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
                .submit_async(op, user)
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
        ring: EventRing,
    ) -> Pin<Box<dyn Future<Output = Result<EventReceiver, TransportError>> + Send + '_>> {
        let rx = self
            .handle
            .interrupts()
            .subscribe_async_filtered(
                InterruptFilter {
                    reason: filter.reason,
                    addr: filter.addr,
                    ..Default::default()
                },
                ring.ring_len,
                ring.payload_bytes,
            )
            .map(|ring| EventReceiver {
                ring,
                port_name: self.handle.port_name().to_string(),
            })
            .map_err(TransportError::from);
        Box::pin(async move { rx })
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
    use crate::interrupt::{AsyncRecvError, InterruptValue};
    use crate::manager::PortManager;
    use crate::param::ParamType;
    use crate::port::InterfaceMask;
    use crate::port::{PortDriver, PortDriverBase, PortFlags};
    use crate::protocol::EventPayload;
    use crate::protocol::command::PortCommand;
    use crate::protocol::reply::ReplyPayload;
    use crate::protocol::request::{ProtocolPriority, RequestMeta};
    use crate::protocol::value::ParamValue;

    struct TestPort {
        base: PortDriverBase,
    }

    impl TestPort {
        // Per-test unique port names: registration goes through the
        // process-global registry, which now errors on duplicates
        // (C registerPort parity), so tests sharing one name would
        // collide when run in a single process (`cargo test`).
        fn new(name: &str) -> Self {
            let mut base = PortDriverBase::new(
                name,
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
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

    const TEST_RING: EventRing = EventRing {
        ring_len: 256,
        payload_bytes: 4096,
    };

    fn make_client(name: &str) -> (PortManager, InProcessClient) {
        let mgr = PortManager::new();
        let rt_handle = mgr
            .register_port_unclaimed(TestPort::new(name), &crate::registry::open_owner())
            .unwrap();
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
        let (_mgr, client) = make_client("ipc_int32");

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
        let (_mgr, client) = make_client("ipc_f64");

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
        let (_mgr, client) = rt.block_on(async { make_client("ipc_blocking") });

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
        let (_mgr, client) = rt.block_on(async { make_client("ipc_conn") });
        assert_eq!(client.connection_state(), ConnectionState::Connected);
    }

    /// The ring holds only what its filter passes: values of other reasons
    /// do not push out the subscriber's own (C registers the ring's
    /// callback with the record's reason and addr).
    #[tokio::test]
    async fn other_reasons_do_not_evict_a_filtered_subscribers_values() {
        let (_mgr, client) = make_client("ipc_filter_ring");
        let mut rx = client
            .subscribe(
                EventFilter {
                    reason: Some(1),
                    addr: None,
                },
                EventRing {
                    ring_len: 4,
                    payload_bytes: 0,
                },
            )
            .await
            .unwrap();
        let handle = client.handle();
        let fire = |reason| {
            handle.interrupts().notify(InterruptValue {
                reason,
                addr: 0,
                value: crate::param::ParamValue::Int32(reason as i32),
                timestamp: SystemTime::now(),
                uint32_changed_mask: 0,
                aux_status: crate::error::AsynStatus::Success,
                alarm_status: 0,
                alarm_severity: 0,
                iface: None,
            })
        };
        fire(1);
        for _ in 0..4 {
            fire(0);
        }
        let ev = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("the reason-1 value arrives")
            .expect("not lagged");
        assert_eq!(
            ev.payload,
            EventPayload::ValueChanged {
                reason: 1,
                addr: 0,
                value: ParamValue::Int32(1)
            }
        );
    }

    #[tokio::test]
    async fn event_subscription() {
        let (_mgr, client) = make_client("ipc_event");

        let mut rx = client
            .subscribe(EventFilter::default(), TEST_RING)
            .await
            .unwrap();

        // Trigger an interrupt via the port handle
        let handle = client.handle();
        handle.interrupts().notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: crate::param::ParamValue::Int32(77),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });

        let event = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(event.port_name, "ipc_event");
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

    /// The subscription's payload clamp is the ring the caller passed, not a
    /// default: an octet longer than `payload_bytes` arrives cut to the ring.
    #[tokio::test]
    async fn event_subscription_clamps_to_the_requested_ring() {
        let (_mgr, client) = make_client("ipc_event_ring");
        let mut rx = client
            .subscribe(
                EventFilter::default(),
                EventRing {
                    ring_len: 4,
                    payload_bytes: 8,
                },
            )
            .await
            .unwrap();
        client.handle().interrupts().notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: crate::param::ParamValue::Octet(vec![b'x'; 20].into()),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });
        let event = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match event.payload {
            EventPayload::ValueChanged { value, .. } => match value {
                // valSize - 1: C keeps a byte for the NUL (devAsynOctet.c:489).
                ParamValue::Octet(bytes) => assert_eq!(bytes.len(), 7),
                other => panic!("expected Octet, got {other:?}"),
            },
            _ => panic!("expected ValueChanged"),
        }
    }

    #[tokio::test]
    async fn event_subscription_filtered() {
        let (_mgr, client) = make_client("ipc_event_filt");

        let mut rx = client
            .subscribe(
                EventFilter {
                    reason: Some(1),
                    addr: None,
                },
                TEST_RING,
            )
            .await
            .unwrap();

        let handle = client.handle();

        // Send reason=0, should be filtered out
        handle.interrupts().notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: crate::param::ParamValue::Int32(10),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        });

        // Send reason=1, should pass
        handle.interrupts().notify(InterruptValue {
            reason: 1,
            addr: 0,
            value: crate::param::ParamValue::Int32(20),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
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

    /// The caller's `ring_len` is the whole depth: nothing between the ring and
    /// the consumer queues more, and the survivors are the newest.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_callers_ring_is_the_only_queue() {
        let (_mgr, client) = make_client("ipc_event_depth");
        let mut rx = client
            .subscribe(
                EventFilter::default(),
                EventRing {
                    ring_len: 4,
                    payload_bytes: 0,
                },
            )
            .await
            .unwrap();
        for i in 0..300 {
            client.handle().interrupts().notify(InterruptValue {
                reason: 0,
                addr: 0,
                value: crate::param::ParamValue::Int32(i),
                timestamp: SystemTime::now(),
                uint32_changed_mask: 0,
                ..Default::default()
            });
            tokio::time::sleep(std::time::Duration::from_micros(200)).await;
        }
        assert_eq!(rx.recv().await.unwrap_err(), AsyncRecvError::Lagged(296));
        for want in 296..300 {
            match rx.recv().await.unwrap().payload {
                EventPayload::ValueChanged { value, .. } => {
                    assert_eq!(value, ParamValue::Int32(want))
                }
                _ => panic!("expected ValueChanged"),
            }
        }
    }
}
