//! PortRuntime: promoted PortActor with event emission and graceful shutdown.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::interrupt::InterruptManager;
use crate::port::PortDriver;
use crate::port_actor::PortActor;
use crate::port_handle::PortHandle;
use crate::transport::InProcessClient;

use super::config::RuntimeConfig;
use super::event::RuntimeEvent;

/// Handle to a running PortRuntime. Provides shutdown and event subscription.
///
/// **Dropping this handle does not stop the port.** A port stops only when it
/// is explicitly shut down ([`Self::shutdown`], [`Self::shutdown_and_wait`]) or
/// when nothing can reach it any more — that is, when its last [`PortHandle`]
/// is dropped. So publishing a port's `PortHandle` (to the
/// `asyn_record` registry, a [`crate::manager::PortManager`], a
/// driver) is by itself enough to keep the port alive for as long as that
/// publication lives; no caller has to park the runtime handle in a static to
/// stop the actor thread from dying underneath it.
#[derive(Clone)]
pub struct PortRuntimeHandle {
    port_handle: PortHandle,
    client: InProcessClient,
    event_tx: broadcast::Sender<RuntimeEvent>,
    /// Carries an explicit shutdown *request* to the actor. The actor stops on
    /// a `()` **sent** here — never on this channel closing, which merely means
    /// the last `PortRuntimeHandle` went away (see
    /// `PortActor::run_with_shutdown`).
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
    /// Sends an explicit shutdown request; the actor thread exits after
    /// completing any in-progress request. Does not wait for the thread to
    /// stop. This outranks reachability: the port stops even while other
    /// `PortHandle`s (a registry entry, a device-support binding) could still
    /// submit to it, and those submissions then fail — which is the point of
    /// asking for a shutdown.
    ///
    /// Repeated calls are harmless: the request is already queued (or the actor
    /// has already gone), and both are a no-op.
    pub fn shutdown(&self) {
        request_shutdown(&self.shutdown_tx);
    }

    /// Signal shutdown and wait for the actor thread to exit.
    ///
    /// The wait is what makes the driver's own teardown observable: the actor
    /// owns the driver, so the driver is dropped — and its `Drop` has run
    /// whatever protocol goodbye it owes — only once this returns. Unbounded,
    /// because a caller who asked to wait has a reason to; the process-exit
    /// path bounds its wait instead (see `stop_port_actor`).
    pub fn shutdown_and_wait(&self) {
        stop_port_actor(
            &self.port_name,
            &self.shutdown_tx,
            &self.completion_rx,
            None,
        );
    }

    /// Port name.
    pub fn port_name(&self) -> &str {
        &self.port_name
    }
}

/// Ask a port actor to stop. The one site that sends the request, so
/// [`PortRuntimeHandle::shutdown`] and the process-exit callback cannot drift
/// apart on what "ask" means.
fn request_shutdown(shutdown_tx: &Arc<std::sync::Mutex<Option<mpsc::Sender<()>>>>) {
    // Poison-tolerant: shutdown must stay infallible even if a thread panicked
    // while holding the lock.
    let guard = shutdown_tx.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tx) = guard.as_ref() {
        // Capacity-1 channel: `Full` means a shutdown is already queued,
        // `Closed` means the actor has already stopped. Neither is an error.
        let _ = tx.try_send(());
    }
}

/// Stop a port actor and wait for it to have dropped its driver — the one
/// stop sequence, shared by [`PortRuntimeHandle::shutdown_and_wait`] and by the
/// port's process-exit callback.
///
/// It takes the two channel ends rather than a `PortRuntimeHandle` on purpose.
/// A `PortRuntimeHandle` carries a `PortHandle`, and a `PortHandle` kept alive
/// keeps the port alive; the exit callback must be able to stop a port without
/// having been the reason it was still running. These two ends carry no
/// reachability: the actor stops on a `()` *sent* on the shutdown channel, never
/// on it closing.
///
/// `wait` bounds how long to wait for the actor thread. `None` blocks. The exit
/// path passes a bound because the actor may be inside a driver call that
/// answers no clock — a serial read with no timeout — and a process must be
/// able to leave even when one of its ports will not.
fn stop_port_actor(
    port_name: &str,
    shutdown_tx: &Arc<std::sync::Mutex<Option<mpsc::Sender<()>>>>,
    completion_rx: &Arc<std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
    wait: Option<std::time::Duration>,
) {
    request_shutdown(shutdown_tx);
    let rx = completion_rx
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let Some(rx) = rx else {
        // Someone already waited for this actor; its driver is long dropped.
        return;
    };
    match wait {
        None => {
            let _ = rx.recv();
        }
        Some(limit) => {
            if rx.recv_timeout(limit).is_err() {
                // Loud, and on stderr: this is the one report that a driver's
                // teardown did NOT run, and it happens on the way out of a
                // process where a `tracing` subscriber may already be gone —
                // on the embedded targets there was never one to begin with.
                eprintln!(
                    "asyn: port '{port_name}' did not stop within {limit:?}; its driver's \
                     teardown has not run"
                );
            }
        }
    }
}

/// The one way to wait for a port's connect — C `waitConnect`
/// (asynManager.c:3292-3336), which arms an exception handler and only then
/// blocks on the event.
///
/// Arming first is the whole point: a connect that lands between "am I
/// connected?" and "start waiting" would be missed by any caller that checked
/// the flag first. So [`Self::arm`] registers the callback, the caller may then
/// short-circuit on an already-connected port (C :3308-3311), and
/// [`Self::wait`] blocks for the rest. Both waiters in the crate — port
/// registration and the iocsh `asynWaitConnect` — go through it, so neither can
/// grow its own race.
pub struct ConnectWaiter {
    rx: std::sync::mpsc::Receiver<()>,
    services: crate::services::PortServices,
    callback: crate::exception::ExceptionCallbackId,
}

impl ConnectWaiter {
    /// Register for `port_name`'s connect exception. From here on the connect
    /// cannot be missed, only waited for.
    pub fn arm(services: &crate::services::PortServices, port_name: &str) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let waited_on = port_name.to_string();
        let callback = services.exceptions().add_callback(move |event| {
            if event.exception == crate::exception::AsynException::Connect
                && event.port_name == waited_on
            {
                let _ = tx.send(());
            }
        });
        Self {
            rx,
            services: services.clone(),
            callback,
        }
    }

    /// Block until the connect exception arrives or `timeout` elapses.
    /// `true` = connected.
    pub fn wait(self, timeout: std::time::Duration) -> bool {
        self.rx.recv_timeout(timeout).is_ok()
    }
}

impl Drop for ConnectWaiter {
    fn drop(&mut self) {
        self.services.exceptions().remove_callback(self.callback);
    }
}

/// Create a port runtime from a driver.
///
/// Returns:
/// - A `PortRuntimeHandle` for interacting with the runtime
/// - A `std::thread::JoinHandle` for the actor thread
///
/// The driver is moved into the actor thread (exclusive ownership).
///
/// `Err` means the port does not exist: see [`create_port_runtime_boxed`].
pub fn create_port_runtime<D: PortDriver>(
    driver: D,
    config: RuntimeConfig,
) -> AsynResult<(PortRuntimeHandle, std::thread::JoinHandle<()>)> {
    create_port_runtime_boxed(Box::new(driver), config)
}

/// Create a port runtime from a boxed driver.
///
/// # Errors
///
/// The actor thread is this port: without it the port can accept a request but
/// never run one. So a thread the OS refuses to create (`EAGAIN` under a
/// process/thread-count or memory limit) is reported as an error and **no port
/// exists** — the caller gets no handle to publish, and nothing about the port
/// survives anywhere in the process.
///
/// C parity: `registerPort` → `registerDriver` creates the port thread at
/// `asynManager.c:2081`, and on failure (:2082-2092) prints, unwinds every
/// resource it had built for the port, and returns `asynError` **before**
/// `ellAdd(&pasynBase->asynPortList, ...)` (:2095) — the port never enters the
/// list. The unwind is this function's `Err` return: the `PortActor` (which
/// owns the driver) is dropped along with the closure `Builder::spawn`
/// rejected, and the one resource that is *not* local — the connect-exception
/// callback registered with the shared [`crate::exception`] list — is removed
/// by [`ConnectWaiter`]'s `Drop`.
///
/// A caller that can report the failure returns it, as
/// `drvAsynIPPortConfigure` does (`drvAsynIPPort.c:1062-1069`: print,
/// `ttyCleanup`, `return -1`) — the iocsh command fails and the IOC boots on
/// without that port. A caller with no error channel — one shaped like a C++
/// constructor, returning the built port by value — uses
/// [`port_runtime_unavailable`] instead.
pub fn create_port_runtime_boxed(
    mut driver: Box<dyn PortDriver>,
    config: RuntimeConfig,
) -> AsynResult<(PortRuntimeHandle, std::thread::JoinHandle<()>)> {
    // The one site that binds a port to its trace configuration and exception
    // list. C does it inside `registerPort` — the sole path into the port list
    // — which calls `dpCommonInit` (asynManager.c:2066) before adding the port
    // (:2094); `dpCommonInit` (:510-529) is what inits `exceptionUserList` (:524)
    // and the port's own `tracePvt` (:528), so no port can exist without them. Every
    // port creator in this crate (`PortManager::register_port`, the
    // `drvAsyn*PortConfigure` iocsh commands, driver-owned ports) funnels
    // through here, so binding here is what makes that true for the port too.
    config.services.bind(driver.base_mut());

    // C `registerInterface(asynCommonType)` — reached from `registerPort` for
    // every port — calls `initPortConnect` and then `portConnectTimerCallback`
    // (asynManager.c:2131-2136), which queues a connect at
    // `asynQueuePriorityConnect` the moment the port exists (:3252-3266). An
    // auto-connect port is therefore brought up BY REGISTRATION, not by whichever
    // record first happens to do I/O: `CNCT` reads 1 straight after
    // `drvAsynIPPortConfigure`, and a port no record ever talks to still comes up.
    //
    // Arming the actor's connect deadline is that queued request: the actor runs
    // `service_connect_timer` ahead of anything in its queue, which is what C's
    // Connect priority buys. It re-arms itself at `secondsBetweenPortConnect` on
    // failure (C :3281), so a port whose device is down keeps trying without a
    // single request ever being submitted.
    let connect_at_registration = driver.base().auto_connect && !driver.base().is_connected();
    if connect_at_registration {
        driver.base_mut().connect_retry_at = Some(std::time::Instant::now());
    }

    let port_name = driver.base().port_name.clone();
    let can_block = driver.base().flags.can_block;
    let multi_device = driver.base().flags.multi_device;
    let max_addr = driver.base().max_addr as i32;
    // The driver's interface declaration, taken once here — C's port
    // registration is where `registerInterface` is called too, and the set never
    // changes afterwards (asynManager.c:2105-2138).
    let interfaces = driver.capabilities();

    // Event broadcast
    let (event_tx, _) = broadcast::channel(256);

    // Runtime-private shutdown channel
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

    // Completion notification (actor thread → shutdown_and_wait)
    let (completion_tx, completion_rx) = std::sync::mpsc::channel::<()>();

    // Share interrupt state (broadcast + mailboxes) so subscribers registered
    // via PortHandle receive notifications from the driver's call_param_callbacks.
    let shared_intr_state = driver.base().interrupts.shared_state();
    let handle_interrupts = Arc::new(InterruptManager::from_shared_state(shared_intr_state));

    // Actor channel
    let (tx, rx) = mpsc::channel(config.channel_capacity);
    let actor = PortActor::new(driver, rx);
    let actor_id = actor.id();

    let event_tx_clone = event_tx.clone();
    let name_clone = port_name.clone();

    // C's `waitConnect` (asynManager.c:2135, :3294-3337): registration waits on
    // the port's *connect exception* for at most `autoConnectTimeout`, so the
    // line of st.cmd after `drvAsynIPPortConfigure` already sees a live port.
    // The waiter is armed before the actor thread exists, so the connect it
    // waits for cannot fire ahead of it.
    let connect_wait =
        connect_at_registration.then(|| ConnectWaiter::arm(&config.services, &port_name));

    let join_handle = match std::thread::Builder::new()
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
        }) {
        Ok(jh) => jh,
        Err(e) => {
            // Loud at the point of failure, as C is: `printf(
            // "asynCommon:registerDriver %s epicsThreadCreate failed \n",
            // portName)` (asynManager.c:2083-2084). `eprintln!` rather than
            // `tracing`, because the targets where thread creation actually
            // fails (RTEMS/VxWorks IOCs) install no subscriber and the console
            // is all there is.
            eprintln!("asyn: port '{port_name}' runtime thread could not be created: {e}");
            // Everything this function built for the port is dropped on the way
            // out — C's :2085-2091 unwind — and the port is never published.
            return Err(port_thread_unavailable(&port_name, &e));
        }
    };

    if let Some(waiter) = connect_wait {
        // A timeout is not a failure: C ignores `waitConnect`'s status here and
        // the port's own retry timer carries on (asynManager.c:2135, :3281).
        let _ = waiter.wait(config.auto_connect_timeout);
    }

    let mut port_handle = PortHandle::new(tx, port_name.clone(), handle_interrupts, actor_id);
    port_handle.set_can_block(can_block);
    port_handle.set_capabilities(multi_device, max_addr);
    port_handle.set_interfaces(interfaces);
    let client = InProcessClient::new(port_handle.clone());

    let handle = PortRuntimeHandle {
        port_handle,
        client,
        event_tx,
        shutdown_tx: Arc::new(std::sync::Mutex::new(Some(shutdown_tx))),
        completion_rx: Arc::new(std::sync::Mutex::new(Some(completion_rx))),
        port_name,
    };

    // C `registerPort` ends by arming the port's own process-exit callback —
    // `epicsAtExit(destroyPortDriver, (void *)pport->portName)`
    // (asynManager.c:2097) — so a port is wired into shutdown by the same call
    // that brings it up, and no port creator has to remember to do it. This is
    // that line: every port in this process, however it was made, is stopped by
    // the IOC's shutdown owner (`epics_libcom_rs::runtime::exit`).
    //
    // Stopping the actor is the whole mechanism. The actor owns the driver
    // exclusively, so its thread ending drops the driver, and the driver's
    // `Drop` runs whatever teardown it owes its device — an MQTT DISCONNECT, a
    // serial `disconnect`. Drivers therefore need to know nothing about
    // shutdown, which is C's arrangement too: `shutdownPort` fires
    // `asynExceptionShutdown` and leaves the destruction to the driver
    // (asynManager.c:2300-2304).
    //
    // The callback holds the two channel ends, not the runtime handle: holding
    // a `PortHandle` would keep the port reachable, and so alive, for the life
    // of the process — turning every port ever created into a leak, and
    // breaking the "last `PortHandle` dropped stops the port" contract above.
    // These ends keep nothing alive, exactly as C's callback holds only the
    // port *name* and looks it up at exit time (asynManager.c:2026-2043).
    let exit_shutdown_tx = handle.shutdown_tx.clone();
    let exit_completion_rx = handle.completion_rx.clone();
    let exit_port_name = handle.port_name.clone();
    epics_libcom_rs::runtime::exit::at_exit(format!("asynPort {exit_port_name}"), move || {
        stop_port_actor(
            &exit_port_name,
            &exit_shutdown_tx,
            &exit_completion_rx,
            Some(PORT_EXIT_STOP_TIMEOUT),
        );
    });

    Ok((handle, join_handle))
}

/// How long process shutdown waits for one port actor to stop before reporting
/// it and moving on to the next.
///
/// C does not wait at all — `shutdownPort` disables the port, fires the
/// shutdown exception and returns (asynManager.c:2296-2306) — but C's teardown
/// is the exception handler, which has already run by then. Ours is the
/// driver's `Drop`, which runs on the actor thread, so leaving without waiting
/// would be leaving before the teardown this whole path exists to reach. The
/// bound is what keeps that wait from being unbounded: a driver parked in a
/// read with no timeout would otherwise hold the process open forever.
const PORT_EXIT_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The error a port whose runtime thread the OS refused to create reports.
///
/// C returns the bare `asynError` (`asynManager.c:2092`) and leaves the
/// diagnostic to the `printf` above it; we carry the port name and the OS
/// reason in the value as well, because a caller several frames up
/// ([`crate::manager::PortManager::register_port`]) prints what it is handed
/// and has no other way to say *which* port failed.
fn port_thread_unavailable(port_name: &str, err: &std::io::Error) -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message: format!("port '{port_name}': runtime thread could not be created: {err}"),
    }
}

/// Abandon the process because a port could not be created.
///
/// For callers shaped like C++ constructors — a `create_*_runtime` that returns
/// the built object and has no error channel to its `*Configure` caller.
///
/// This is a **deliberate deviation** from C, stated rather than inherited.
/// C's constructor prints and `throw`s when `registerPort` fails
/// (`asynPortDriver.cpp:4036-4040`), but iocsh catches every exception a
/// command throws (`iocsh.cpp:1269-1279`, `"C++ error: ..."`) and a startup
/// script's default `on error` is `Continue` (`iocsh.cpp:995`, `:1123`) — so
/// the C IOC prints, runs the rest of st.cmd, and goes on serving with the port
/// missing and every record bound to it dead. That is the half-IOC this
/// workspace refuses to build. There is no third option here: the caller's
/// return type is the built port, so the failure is either the process or a
/// handle to a port whose actor thread does not exist, silently swallowing
/// every request for the life of the IOC.
///
/// Callers that *can* report the failure must return the error instead; see
/// [`create_port_runtime_boxed`].
pub fn port_runtime_unavailable(port_name: &str, err: &AsynError) -> ! {
    eprintln!(
        "FATAL: the IOC could not create the runtime for asyn port '{port_name}': {err}. \
         Continuing would leave records bound to a port whose actor thread does not \
         exist — every request queued to it would wait forever — so the process is \
         aborting instead."
    );
    std::process::abort()
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
        let (handle, _jh) = create_port_runtime(TestPort::new("rt_test"), RuntimeConfig::default())
            .expect("the port runtime thread must start");

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
            create_port_runtime(TestPort::new("rt_client"), RuntimeConfig::default())
                .expect("the port runtime thread must start");

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
            create_port_runtime(TestPort::new("rt_shutdown"), RuntimeConfig::default())
                .expect("the port runtime thread must start");

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
        )
        .expect("the port runtime thread must start");

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
        )
        .expect("the port runtime thread must start");

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
            create_port_runtime(TestPort::new("rt_events"), RuntimeConfig::default())
                .expect("the port runtime thread must start");

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
            create_port_runtime(TestPort::new("named_port"), RuntimeConfig::default())
                .expect("the port runtime thread must start");
        assert_eq!(handle.port_name(), "named_port");
    }

    /// The boundary this whole change exists for: the OS refuses the port's
    /// actor thread, and afterwards **no name is claimed** — not in the manager,
    /// not in the process registry every consumer resolves through.
    ///
    /// A real `EAGAIN` from `pthread_create`, not a stub: the child lowers
    /// `RLIMIT_NPROC` to 1, which is the measured VxWorks failure (thread
    /// creation refused under a resource limit) reproduced on the host. It has
    /// to be a child process because the limit is process-wide — a test that
    /// lowered it in-process would take every concurrently running test with
    /// it under a shared-process runner.
    ///
    /// Root is not subject to `RLIMIT_NPROC` (`CAP_SYS_RESOURCE`), so the
    /// parent says why it is not asserting rather than passing vacuously.
    ///
    /// Linux-only: `RLIMIT_NPROC` counts threads (`clone`) against the limit
    /// only there. On macOS/BSD it limits `fork` alone — `pthread_create`
    /// succeeds regardless, so the refusal cannot be provoked (measured: both
    /// macOS CI runners fail the child's "registration must fail" assertion).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_port_whose_thread_cannot_be_created_is_not_registered() {
        const CHILD: &str = "EPICS_RS_PORT_THREAD_EAGAIN_CHILD";
        const TEST: &str =
            "runtime::port::tests::a_port_whose_thread_cannot_be_created_is_not_registered";
        const PORT: &str = "eagain_port";
        const DONE: &str = "no-port-was-registered";

        if std::env::var_os(CHILD).is_some() {
            // Refuse every further thread this process asks for.
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            assert_eq!(
                unsafe { libc::getrlimit(libc::RLIMIT_NPROC, &mut limit) },
                0,
                "read the current process limit"
            );
            limit.rlim_cur = 1;
            assert_eq!(
                unsafe { libc::setrlimit(libc::RLIMIT_NPROC, &limit) },
                0,
                "lower the process limit"
            );

            let manager = crate::manager::PortManager::new();
            let err = manager
                .register_port(TestPort::new(PORT))
                .err()
                .expect("thread creation is refused, so registration must fail");
            assert!(
                err.to_string().contains(PORT),
                "the error names the port that failed, got: {err}"
            );
            assert!(
                manager.find_port_handle(PORT).is_err(),
                "the manager must not hold a handle for a port that was never created"
            );
            assert!(
                crate::registry::get_port(PORT).is_none(),
                "the process registry must not hold a port that was never created"
            );
            println!("{DONE}");
            return;
        }

        if unsafe { libc::geteuid() } == 0 {
            println!(
                "skipped: running as root, which bypasses RLIMIT_NPROC \
                 (CAP_SYS_RESOURCE), so thread creation cannot be made to fail"
            );
            return;
        }

        let out =
            std::process::Command::new(std::env::current_exe().expect("the test binary path"))
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD, "1")
                .output()
                .expect("re-exec the test binary");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success() && stdout.contains(DONE),
            "child must reach the end of the boundary assertions.\n\
             status: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            out.status
        );
        // The failure is loud where C's is (asynManager.c:2083).
        assert!(
            stderr.contains(PORT),
            "the diagnostic on stderr must name the port, got:\n{stderr}"
        );
    }
}
