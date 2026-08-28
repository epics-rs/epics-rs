//! MQ4: at IOC shutdown every port actor is stopped, so every driver's own
//! `Drop` teardown actually runs.
//!
//! A driver that owes its device a goodbye — an MQTT DISCONNECT, a serial
//! `disconnect`, a TCP close — writes it in `Drop`. Nothing ran those: the port
//! actor owns the driver and the actor only stopped when the port became
//! unreachable, which a registered port never does. The teardown was correct
//! and never reached.
//!
//! The owner is `epics_libcom_rs::runtime::exit` (C `epicsExit.c`), which
//! `IocApplication::run` calls on every way out. Ports enrol themselves in it at
//! creation, exactly where C's `registerPort` does
//! (`epicsAtExit(destroyPortDriver, …)`, asynManager.c:2097), so a driver still
//! knows nothing about shutdown.
//!
//! Three ports here, torn down by one call: one driver that records its own
//! `Drop`, and two shipped drivers that are not the one that reported this —
//! `DrvAsynIPPort`, whose peer must see the connection close, and (on Unix)
//! `DrvAsynSerialPort`, whose `Drop` runs `disconnect` and must release the tty.

use std::io::Read;
use std::sync::Arc;
use std::sync::mpsc::{Sender, TryRecvError};
use std::time::Duration;

use asyn_rs::drivers::ip_port::DrvAsynIPPort;
use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::registry::PortRegistry;
use asyn_rs::runtime::port::PortRuntimeHandle;
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime};
use asyn_rs::trace::TraceManager;

/// A driver whose entire teardown is observable: it reports its own `Drop`.
/// Stands in for every driver with a protocol goodbye to send.
struct TeardownDriver {
    base: PortDriverBase,
    torn_down: Sender<&'static str>,
}

impl TeardownDriver {
    fn new(name: &str, torn_down: Sender<&'static str>) -> Self {
        let mut base = PortDriverBase::new(name, 1, PortFlags::default());
        base.create_param("VAL", ParamType::Int32).unwrap();
        Self { base, torn_down }
    }
}

impl PortDriver for TeardownDriver {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

impl Drop for TeardownDriver {
    fn drop(&mut self) {
        let _ = self.torn_down.send("torn down");
    }
}

/// Publish a port the way every real port creator does — `drvAsyn*PortConfigure`,
/// mqtt-rs's and modbus-rs's `register_port` — and drop the
/// `PortRuntimeHandle`. The registry's `PortHandle` is then the only thing
/// reaching the port, and by design that keeps it alive for the life of the
/// process: nothing short of the shutdown owner can stop it.
///
/// Every port below goes through this. A port left unreachable would stop on
/// its own and prove nothing about who stopped it.
fn publish(registry: &PortRegistry, name: &str, port: &PortRuntimeHandle) {
    registry
        .register(
            name,
            port.port_handle().clone(),
            Arc::new(TraceManager::new()),
        )
        .expect("port name is free");
}

/// Long enough that a scheduling delay cannot fail the test, short enough that
/// a regression reports rather than hangs. Only the two observations that cross
/// a thread or the kernel need it; the driver-`Drop` one is exact.
const SETTLE: Duration = Duration::from_secs(5);

#[test]
fn ioc_shutdown_stops_every_port_actor_and_runs_each_drivers_teardown() {
    // ---- port 1: a driver that reports its own Drop --------------------
    let (torn_down_tx, torn_down) = std::sync::mpsc::channel();
    let (recorder, _recorder_thread) = create_port_runtime(
        TeardownDriver::new("MQ4:RECORDER", torn_down_tx),
        RuntimeConfig::default(),
    )
    .expect("the port runtime thread must start");
    let registry = PortRegistry::new();
    publish(&registry, "MQ4:RECORDER", &recorder);

    // ---- port 2: DrvAsynIPPort, and a peer watching for the close ------
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let device_port = listener.local_addr().unwrap().port();
    let (peer_saw, peer_result) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut conn, _)) = listener.accept() else {
            return;
        };
        // Returns 0 exactly when the port's socket is closed at the other end.
        // Until then it blocks, which is the unfixed behaviour: an IOC that
        // exits without stopping its ports leaves this read hanging.
        let mut sink = [0u8; 1];
        let _ = peer_saw.send(conn.read(&mut sink));
    });
    let (ip_port, _ip_thread) = create_port_runtime(
        DrvAsynIPPort::new("MQ4:IP", &format!("127.0.0.1:{device_port}")).expect("configure"),
        RuntimeConfig::default(),
    )
    .expect("the port runtime thread must start");
    publish(&registry, "MQ4:IP", &ip_port);

    // ---- port 3: DrvAsynSerialPort on a pty (Unix) ---------------------
    #[cfg(unix)]
    let serial = serial::open_port_on_a_pty(&registry);

    // ---- nothing is torn down yet --------------------------------------
    assert_eq!(
        torn_down.try_recv(),
        Err(TryRecvError::Empty),
        "a running IOC must not have torn its ports down"
    );
    #[cfg(unix)]
    if let Some(pty) = &serial {
        assert!(
            pty.driver_still_holds_the_tty(),
            "the serial port must be connected before shutdown, or this test \
             proves nothing about its teardown"
        );
    }

    // ---- the IOC shuts down --------------------------------------------
    //
    // The one call `IocApplication::run` makes on its way out. It waits for
    // each actor thread, so when it returns every driver has been dropped and
    // every `Drop` has finished — the assertions below need no settling time
    // except where they cross a thread or the kernel.
    epics_libcom_rs::runtime::exit::call_at_exits();

    // ---- every driver's teardown has run --------------------------------
    assert_eq!(
        torn_down.try_recv(),
        Ok("torn down"),
        "IOC shutdown must drop the port actor — and with it the driver, whose \
         Drop is where a protocol goodbye is written"
    );
    match peer_result.recv_timeout(SETTLE) {
        Ok(Ok(0)) => {}
        other => panic!(
            "the device must see the IP port's connection close at IOC \
             shutdown; the peer read reported {other:?}"
        ),
    }
    #[cfg(unix)]
    if let Some(pty) = &serial {
        assert!(
            !pty.driver_still_holds_the_tty(),
            "DrvAsynSerialPort's Drop runs `disconnect`; IOC shutdown must \
             reach it, releasing the tty"
        );
    }
}

#[cfg(unix)]
mod serial {
    use std::os::unix::io::RawFd;

    use asyn_rs::drivers::serial_port::DrvAsynSerialPort;
    use asyn_rs::registry::PortRegistry;
    use asyn_rs::runtime::{RuntimeConfig, create_port_runtime};

    /// The pty master, kept open so the slave's fate is observable.
    pub struct Pty {
        master: RawFd,
    }

    impl Drop for Pty {
        fn drop(&mut self) {
            unsafe { libc::close(self.master) };
        }
    }

    impl Pty {
        /// Whether anything still has the slave side open — which, since this
        /// test closed its own, means the driver.
        ///
        /// A non-blocking read on the master answers it: `EAGAIN` while a slave
        /// is open and idle, `EIO` (or EOF) once the last one closes.
        pub fn driver_still_holds_the_tty(&self) -> bool {
            let mut byte = 0u8;
            let n = unsafe {
                libc::read(
                    self.master,
                    &mut byte as *mut u8 as *mut libc::c_void,
                    1usize,
                )
            };
            n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock
        }
    }

    /// A serial port on a fresh pty, or `None` where the host has no pty to
    /// give — the same skip the driver's own pty tests take.
    pub fn open_port_on_a_pty(registry: &PortRegistry) -> Option<Pty> {
        let mut master: RawFd = 0;
        let mut slave: RawFd = 0;
        let mut name = [0u8; 256];
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                name.as_mut_ptr() as *mut libc::c_char,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if opened < 0 {
            eprintln!("openpty unavailable; skipping the serial half of this test");
            return None;
        }
        let device = unsafe {
            std::ffi::CStr::from_ptr(name.as_ptr() as *const libc::c_char)
                .to_string_lossy()
                .into_owned()
        };
        // The driver opens the slave by name; this test must not hold one too,
        // or "the driver released the tty" would be unobservable.
        unsafe { libc::close(slave) };
        unsafe { libc::fcntl(master, libc::F_SETFL, libc::O_NONBLOCK) };

        let (port, _thread) = create_port_runtime(
            DrvAsynSerialPort::new("MQ4:SERIAL", &device).expect("configure"),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        super::publish(registry, "MQ4:SERIAL", &port);
        Some(Pty { master })
    }
}
