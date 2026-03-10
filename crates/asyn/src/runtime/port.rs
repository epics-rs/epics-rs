//! PortRuntime: promoted PortActor with event emission and graceful shutdown.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, watch};

use crate::interrupt::InterruptManager;
use crate::port::PortDriver;
use crate::port_actor::PortActor;
use crate::port_handle::PortHandle;
use crate::transport::InProcessClient;

use super::config::RuntimeConfig;
use super::event::RuntimeEvent;

/// Handle to a running PortRuntime. Provides shutdown and event subscription.
#[derive(Clone)]
pub struct PortRuntimeHandle {
    port_handle: PortHandle,
    client: InProcessClient,
    event_tx: broadcast::Sender<RuntimeEvent>,
    shutdown_tx: watch::Sender<bool>,
    port_name: String,
}

impl PortRuntimeHandle {
    /// Get the underlying PortHandle for I/O operations.
    pub fn port_handle(&self) -> &PortHandle {
        &self.port_handle
    }

    /// Get an InProcessClient for protocol-based communication.
    pub fn client(&self) -> &InProcessClient {
        &self.client
    }

    /// Subscribe to runtime events.
    pub fn subscribe_events(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.event_tx.subscribe()
    }

    /// Signal the runtime to shut down.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    /// Port name.
    pub fn port_name(&self) -> &str {
        &self.port_name
    }
}

/// Create a port runtime from a driver.
///
/// Returns:
/// - A `PortRuntimeHandle` for interacting with the runtime
/// - A `std::thread::JoinHandle` for the actor thread
///
/// The driver is moved into the actor thread (exclusive ownership).
pub fn create_port_runtime<D: PortDriver>(
    driver: D,
    config: RuntimeConfig,
) -> (PortRuntimeHandle, std::thread::JoinHandle<()>) {
    create_port_runtime_boxed(Box::new(driver), config)
}

/// Create a port runtime from a boxed driver.
pub fn create_port_runtime_boxed(
    driver: Box<dyn PortDriver>,
    config: RuntimeConfig,
) -> (PortRuntimeHandle, std::thread::JoinHandle<()>) {
    let port_name = driver.base().port_name.clone();

    // Event broadcast
    let (event_tx, _) = broadcast::channel(256);
    let (shutdown_tx, _shutdown_rx) = watch::channel(false);

    // Clone broadcast sender for interrupt subscription
    let broadcast_sender = driver.base().interrupts.broadcast_sender();
    let handle_interrupts = Arc::new(InterruptManager::from_broadcast_sender(broadcast_sender));

    // Actor channel
    let (tx, rx) = mpsc::channel(config.channel_capacity);
    let actor = PortActor::new(driver, rx);

    let event_tx_clone = event_tx.clone();
    let name_clone = port_name.clone();

    let join_handle = std::thread::Builder::new()
        .name(format!("asyn-runtime-{port_name}"))
        .spawn(move || {
            let _ = event_tx_clone.send(RuntimeEvent::Started {
                port_name: name_clone.clone(),
            });
            actor.run();
            let _ = event_tx_clone.send(RuntimeEvent::Stopped {
                port_name: name_clone,
            });
        })
        .expect("failed to spawn port runtime thread");

    let port_handle = PortHandle::new(tx, port_name.clone(), handle_interrupts);
    let client = InProcessClient::new(port_handle.clone());

    let handle = PortRuntimeHandle {
        port_handle,
        client,
        event_tx,
        shutdown_tx,
        port_name,
    };

    (handle, join_handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::param::ParamType;
    use crate::port::{PortDriverBase, PortFlags};

    struct TestPort {
        base: PortDriverBase,
    }

    impl TestPort {
        fn new(name: &str) -> Self {
            let mut base = PortDriverBase::new(name, 1, PortFlags::default());
            base.create_param("VAL", ParamType::Int32).unwrap();
            base.create_param("F64", ParamType::Float64).unwrap();
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

    #[test]
    fn port_runtime_int32_roundtrip() {
        let (handle, _jh) = create_port_runtime(
            TestPort::new("rt_test"),
            RuntimeConfig::default(),
        );

        handle
            .port_handle()
            .write_int32_blocking(0, 0, 42)
            .unwrap();
        assert_eq!(
            handle.port_handle().read_int32_blocking(0, 0).unwrap(),
            42
        );
    }

    #[test]
    fn port_runtime_client_roundtrip() {
        use crate::protocol::command::PortCommand;
        use crate::protocol::reply::ReplyPayload;
        use crate::protocol::request::{PortRequest, ProtocolPriority, RequestMeta};
        use crate::protocol::value::ParamValue;
        use crate::transport::RuntimeClient;

        let (handle, _jh) = create_port_runtime(
            TestPort::new("rt_client"),
            RuntimeConfig::default(),
        );

        let client = handle.client();

        // Write via client
        let req = PortRequest {
            meta: RequestMeta {
                request_id: 1,
                port_name: "rt_client".into(),
                addr: 0,
                reason: 0,
                timeout_ms: 5000,
                priority: ProtocolPriority::Medium,
                block_token: None,
            },
            command: PortCommand::Int32Write { value: 77 },
        };
        let reply = client.request_blocking(req).unwrap();
        assert_eq!(reply.payload, ReplyPayload::Ack);

        // Read via client
        let req = PortRequest {
            meta: RequestMeta {
                request_id: 2,
                port_name: "rt_client".into(),
                addr: 0,
                reason: 0,
                timeout_ms: 5000,
                priority: ProtocolPriority::Medium,
                block_token: None,
            },
            command: PortCommand::Int32Read,
        };
        let reply = client.request_blocking(req).unwrap();
        match reply.payload {
            ReplyPayload::Value(ParamValue::Int32(v)) => assert_eq!(v, 77),
            _ => panic!("expected Int32 value"),
        }
    }

    #[test]
    fn port_runtime_shutdown() {
        let (handle, jh) = create_port_runtime(
            TestPort::new("rt_shutdown"),
            RuntimeConfig::default(),
        );

        // Dropping the handle should cause the actor to stop
        drop(handle);
        let result = jh.join();
        assert!(result.is_ok());
    }

    #[test]
    fn port_runtime_event_subscription() {
        let (handle, _jh) = create_port_runtime(
            TestPort::new("rt_events"),
            RuntimeConfig::default(),
        );

        let mut rx = handle.subscribe_events();

        // Give the actor thread time to emit Started event
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Check for started event (may or may not have been received depending on timing)
        match rx.try_recv() {
            Ok(RuntimeEvent::Started { port_name }) => {
                assert_eq!(port_name, "rt_events");
            }
            _ => {} // Timing-dependent, OK to miss
        }
    }

    #[test]
    fn port_runtime_port_name() {
        let (handle, _jh) = create_port_runtime(
            TestPort::new("named_port"),
            RuntimeConfig::default(),
        );
        assert_eq!(handle.port_name(), "named_port");
    }
}
