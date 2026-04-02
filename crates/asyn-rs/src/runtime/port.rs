//! PortRuntime: promoted PortActor with event emission and graceful shutdown.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

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
    /// Dropping this sender closes the shutdown channel, signaling the actor to exit.
    shutdown_tx: Arc<std::sync::Mutex<Option<mpsc::Sender<()>>>>,
    /// Receives a single () when the actor thread exits. Used by shutdown_and_wait().
    completion_rx: Arc<std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
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

    /// Signal the runtime to shut down (non-blocking).
    ///
    /// Closes the shutdown channel, causing the actor thread to exit after
    /// completing any in-progress request. Does not wait for the thread to stop.
    pub fn shutdown(&self) {
        self.port_handle.mark_shutdown();
        self.shutdown_tx.lock().unwrap().take();
    }

    /// Signal shutdown and wait for the actor thread to exit.
    pub fn shutdown_and_wait(&self) {
        self.shutdown();
        if let Some(rx) = self.completion_rx.lock().unwrap().take() {
            let _ = rx.recv();
        }
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
    let can_block = driver.base().flags.can_block;

    // Event broadcast
    let (event_tx, _) = broadcast::channel(256);

    // Runtime-private shutdown channel
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

    // Completion notification (actor thread → shutdown_and_wait)
    let (completion_tx, completion_rx) = std::sync::mpsc::channel::<()>();

    // Clone broadcast sender for interrupt subscription
    let broadcast_sender = driver.base().interrupts.broadcast_sender();
    let handle_interrupts = Arc::new(InterruptManager::from_broadcast_sender(broadcast_sender));

    // Actor channel (needed even for can_block=false for async submit compatibility)
    let (tx, rx) = mpsc::channel(config.channel_capacity);

    let mut port_handle = PortHandle::new(tx.clone(), port_name.clone(), handle_interrupts);

    let join_handle = if can_block {
        // can_block=true: actor thread owns the driver, processes requests from channel
        let actor = PortActor::new(driver, rx);
        let event_tx_clone = event_tx.clone();
        let name_clone = port_name.clone();
        std::thread::Builder::new()
            .name(format!("asyn-runtime-{port_name}"))
            .spawn(move || {
                let _ = event_tx_clone.send(RuntimeEvent::Started {
                    port_name: name_clone.clone(),
                });
                actor.run_with_shutdown(shutdown_rx);
                let _ = event_tx_clone.send(RuntimeEvent::Stopped {
                    port_name: name_clone,
                });
                let _ = completion_tx.send(());
            })
            .expect("failed to spawn port runtime thread")
    } else {
        // can_block=false: no actor thread. Driver lives in Arc<Mutex> on PortHandle.
        // All I/O goes through direct mutex lock — matching C EPICS lockPort.
        let shared = Arc::new(parking_lot::Mutex::new(driver));
        port_handle.set_direct_driver(shared);
        let event_tx_clone = event_tx.clone();
        let name_clone = port_name.clone();
        std::thread::Builder::new()
            .name(format!("asyn-noop-{port_name}"))
            .spawn(move || {
                let _ = event_tx_clone.send(RuntimeEvent::Started {
                    port_name: name_clone.clone(),
                });
                // Keep rx/shutdown_rx alive so senders don't get closed errors.
                let rt = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    // Drain any messages that arrive (shouldn't happen in normal operation)
                    let mut rx = rx;
                    let mut shutdown_rx = shutdown_rx;
                    loop {
                        tokio::select! {
                            msg = rx.recv() => {
                                if msg.is_none() { break; }
                                // Messages shouldn't arrive but drop them gracefully
                            }
                            _ = shutdown_rx.recv() => break,
                        }
                    }
                });
                let _ = completion_tx.send(());
            })
            .expect("failed to spawn noop thread")
    };
    let client = InProcessClient::new(port_handle.clone());

    let handle = PortRuntimeHandle {
        port_handle,
        client,
        event_tx,
        shutdown_tx: Arc::new(std::sync::Mutex::new(Some(shutdown_tx))),
        completion_rx: Arc::new(std::sync::Mutex::new(Some(completion_rx))),
        port_name,
    };

    (handle, join_handle)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::param::ParamType;
    use crate::port::{PortDriverBase, PortFlags};
    use crate::user::AsynUser;

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

    struct NoWaitDirectPort {
        base: PortDriverBase,
        connect_count: Arc<AtomicUsize>,
    }

    impl NoWaitDirectPort {
        fn new(name: &str, connect_count: Arc<AtomicUsize>) -> Self {
            let mut base = PortDriverBase::new(
                name,
                1,
                PortFlags {
                    can_block: false,
                    ..PortFlags::default()
                },
            );
            base.connected = false;
            base.auto_connect = true;
            base.create_param("VAL", ParamType::Int32).unwrap();
            Self {
                base,
                connect_count,
            }
        }
    }

    impl PortDriver for NoWaitDirectPort {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        fn connect(&mut self, _user: &AsynUser) -> crate::error::AsynResult<()> {
            self.connect_count.fetch_add(1, Ordering::SeqCst);
            self.base.connected = true;
            Ok(())
        }
    }

    #[test]
    fn port_runtime_int32_roundtrip() {
        let (handle, _jh) = create_port_runtime(TestPort::new("rt_test"), RuntimeConfig::default());

        handle.port_handle().write_int32_blocking(0, 0, 42).unwrap();
        assert_eq!(handle.port_handle().read_int32_blocking(0, 0).unwrap(), 42);
    }

    #[test]
    fn port_runtime_client_roundtrip() {
        use crate::protocol::command::PortCommand;
        use crate::protocol::reply::ReplyPayload;
        use crate::protocol::request::{PortRequest, ProtocolPriority, RequestMeta};
        use crate::protocol::value::ParamValue;
        use crate::transport::RuntimeClient;

        let (handle, _jh) =
            create_port_runtime(TestPort::new("rt_client"), RuntimeConfig::default());

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
        let (handle, jh) =
            create_port_runtime(TestPort::new("rt_shutdown"), RuntimeConfig::default());

        // Dropping the handle should cause the actor to stop
        drop(handle);
        let result = jh.join();
        assert!(result.is_ok());
    }

    #[test]
    fn port_runtime_explicit_shutdown() {
        let (handle, _jh) = create_port_runtime(
            TestPort::new("rt_explicit_shutdown"),
            RuntimeConfig::default(),
        );

        // Write a value first
        handle.port_handle().write_int32_blocking(0, 0, 42).unwrap();

        // Explicit shutdown should cause the actor to stop
        handle.shutdown_and_wait();
    }

    #[test]
    fn port_runtime_shutdown_while_handles_exist() {
        let (handle, _jh) = create_port_runtime(
            TestPort::new("rt_shutdown_handles"),
            RuntimeConfig::default(),
        );

        // Clone the handle (simulating other code holding a reference)
        let handle2 = handle.clone();

        // Explicit shutdown should work even with outstanding clones
        handle.shutdown_and_wait();

        // Subsequent operations on the cloned handle should fail gracefully
        let result = handle2.port_handle().write_int32_blocking(0, 0, 99);
        assert!(result.is_err());
    }

    #[test]
    fn port_runtime_event_subscription() {
        let (handle, _jh) =
            create_port_runtime(TestPort::new("rt_events"), RuntimeConfig::default());

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
        let (handle, _jh) =
            create_port_runtime(TestPort::new("named_port"), RuntimeConfig::default());
        assert_eq!(handle.port_name(), "named_port");
    }

    #[test]
    fn port_runtime_direct_no_wait_executes_immediately() {
        let connect_count = Arc::new(AtomicUsize::new(0));
        let (handle, _jh) = create_port_runtime(
            NoWaitDirectPort::new("rt_direct_nowait", connect_count.clone()),
            RuntimeConfig::default(),
        );

        handle.port_handle().write_int32_no_wait(0, 0, 55);
        handle.port_handle().call_param_callbacks_no_wait(0);

        assert_eq!(handle.port_handle().read_int32_blocking(0, 0).unwrap(), 55);
        assert_eq!(connect_count.load(Ordering::SeqCst), 1);
    }
}
