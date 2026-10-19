//! The asyn bus against C StreamDevice: each `tests/oracle/asyn/<case>.log`
//! is what C's AsynDriverInterface.cc printed (its debug lines, pointers
//! masked as `0x*`) for records on port P (see `tests/oracle/asyn/run.sh`).
//! The port thread's lines are taken with the process-wide capture, which
//! is clean only with one test per process: run these under nextest.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interfaces::{Capability, InterfaceMask, octet_transport_capabilities};
use asyn_rs::manager::{PortManager, claim_port_driver};
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::request::{OctetReadOp, RequestOp};
use asyn_rs::user::AsynUser;
use stream_rs::bus::{self, Client, OwnedReadInput, ReadInput, StreamIoStatus};
use stream_rs::error::{
    capture_process_stderr, capture_stderr, set_stream_debug, set_stream_msg_time_stamped,
    take_captured,
};

/// The debug switch and the captured stderr are process-wide.
static SWITCHES: Mutex<()> = Mutex::new(());

/// A record: what its bus calls back with goes to its [`Calls`].
struct Rec {
    name: &'static str,
    in_terminator: Option<Vec<u8>>,
    out_terminator: Option<Vec<u8>>,
    locks: Mutex<Sender<StreamIoStatus>>,
    connects: Mutex<Sender<StreamIoStatus>>,
    writes: Mutex<Sender<StreamIoStatus>>,
    reads: Mutex<Sender<(StreamIoStatus, OwnedReadInput, isize)>>,
    disconnects: Mutex<Sender<StreamIoStatus>>,
    events: Mutex<Sender<StreamIoStatus>>,
    read_more: Arc<Mutex<VecDeque<isize>>>,
    /// The input of the read in progress.
    input: Mutex<Vec<u8>>,
    accept: Arc<AtomicBool>,
}

/// The callbacks a [`Rec`] got, one receiver per callback.
struct Calls {
    locks: Receiver<StreamIoStatus>,
    connects: Receiver<StreamIoStatus>,
    writes: Receiver<StreamIoStatus>,
    /// Each readCallback's status, input and answer.
    reads: Receiver<(StreamIoStatus, OwnedReadInput, isize)>,
    disconnects: Receiver<StreamIoStatus>,
    events: Receiver<StreamIoStatus>,
    /// What the next readCallbacks return.
    read_more: Arc<Mutex<VecDeque<isize>>>,
    /// The IOC's interruptAccept, set from the start.
    accept: Arc<AtomicBool>,
}

impl Client for Rec {
    fn lock_callback(&self, status: StreamIoStatus) {
        let _ = self.locks.lock().unwrap().send(status);
    }
    fn connect_callback(&self, status: StreamIoStatus) {
        let _ = self.connects.lock().unwrap().send(status);
    }
    fn write_callback(&self, status: StreamIoStatus) {
        let _ = self.writes.lock().unwrap().send(status);
    }
    fn disconnect_callback(&self, status: StreamIoStatus) {
        let _ = self.disconnects.lock().unwrap().send(status);
    }
    fn event_callback(&self, status: StreamIoStatus) {
        let _ = self.events.lock().unwrap().send(status);
    }
    /// Past its `read_more`, what StreamCore::readCallback returns for a
    /// protocol without MaxInput (StreamCore.cc:1092-1157): -1, read on,
    /// while a successful read has not brought the in terminator; 0 once
    /// it has, and for every other status.
    fn read_callback(&self, status: StreamIoStatus, input: ReadInput<'_>) -> isize {
        let mut so_far = self.input.lock().unwrap();
        match input {
            ReadInput::Bytes(b) => so_far.extend_from_slice(b),
            ReadInput::TakeBack(n) => {
                let keep = so_far.len().saturating_sub(n);
                so_far.truncate(keep);
            }
        }
        let more = self
            .read_more
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                let ended = self
                    .in_terminator
                    .as_ref()
                    .is_some_and(|t| so_far.windows(t.len()).any(|w| w == t.as_slice()));
                if status == StreamIoStatus::StreamIoSuccess && !ended {
                    -1
                } else {
                    0
                }
            });
        if more == 0 {
            so_far.clear();
        }
        let _ = self
            .reads
            .lock()
            .unwrap()
            .send((status, input.to_owned(), more));
        more
    }
    fn interrupt_accept(&self) -> bool {
        self.accept.load(Ordering::Relaxed)
    }
    fn get_in_terminator(&self) -> Option<Vec<u8>> {
        self.in_terminator.clone()
    }
    fn get_out_terminator(&self) -> Option<Vec<u8>> {
        self.out_terminator.clone()
    }
    fn name(&self) -> String {
        self.name.into()
    }
}

fn rec(name: &'static str) -> (Arc<dyn Client>, Receiver<StreamIoStatus>) {
    let (client, calls) = rec_calls(name, None);
    (client, calls.locks)
}

/// [`rec`], with the receiver of its connect callbacks.
fn rec_connects(
    name: &'static str,
) -> (
    Arc<dyn Client>,
    Receiver<StreamIoStatus>,
    Receiver<StreamIoStatus>,
) {
    let (client, calls) = rec_calls(name, None);
    (client, calls.locks, calls.connects)
}

/// A record whose protocol has `out_terminator`, and its callbacks.
fn rec_calls(name: &'static str, out_terminator: Option<&[u8]>) -> (Arc<dyn Client>, Calls) {
    rec_terminated(name, None, out_terminator)
}

/// A record whose protocol has these terminators, and its callbacks.
fn rec_terminated(
    name: &'static str,
    in_terminator: Option<&[u8]>,
    out_terminator: Option<&[u8]>,
) -> (Arc<dyn Client>, Calls) {
    let (locks, locked) = channel();
    let (connects, connected) = channel();
    let (writes, written) = channel();
    let (reads, read) = channel();
    let (disconnects, disconnected) = channel();
    let (events, evented) = channel();
    let read_more = Arc::new(Mutex::new(VecDeque::new()));
    let accept = Arc::new(AtomicBool::new(true));
    let client: Arc<dyn Client> = Arc::new(Rec {
        name,
        in_terminator: in_terminator.map(<[u8]>::to_vec),
        out_terminator: out_terminator.map(<[u8]>::to_vec),
        locks: Mutex::new(locks),
        connects: Mutex::new(connects),
        writes: Mutex::new(writes),
        reads: Mutex::new(reads),
        disconnects: Mutex::new(disconnects),
        events: Mutex::new(events),
        read_more: read_more.clone(),
        input: Mutex::new(Vec::new()),
        accept: accept.clone(),
    });
    let calls = Calls {
        locks: locked,
        connects: connected,
        writes: written,
        reads: read,
        disconnects: disconnected,
        events: evented,
        read_more,
        accept,
    };
    (client, calls)
}

const WAIT: Duration = Duration::from_secs(5);

struct Dev {
    base: PortDriverBase,
    caps: Vec<Capability>,
}

impl PortDriver for Dev {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn capabilities(&self) -> Vec<Capability> {
        self.caps.clone()
    }
}

fn port(name: &str, caps: Vec<Capability>) -> asyn_rs::port_handle::PortHandle {
    port_with(name, caps, true)
}

fn port_with(
    name: &str,
    caps: Vec<Capability>,
    can_block: bool,
) -> asyn_rs::port_handle::PortHandle {
    port_interrupting(name, caps, can_block, InterfaceMask::OCTET)
}

/// [`port_with`], interrupting on `interrupts`.
fn port_interrupting(
    name: &str,
    caps: Vec<Capability>,
    can_block: bool,
    interrupts: InterfaceMask,
) -> asyn_rs::port_handle::PortHandle {
    let flags = PortFlags {
        can_block,
        ..PortFlags::default()
    };
    let base = PortDriverBase::new(name, 1, flags, InterfaceMask::OCTET, interrupts);
    let manager = PortManager::new();
    manager
        .register_port(
            claim_port_driver(name).unwrap(),
            Dev { base, caps },
            &asyn_rs::registry::open_owner(),
        )
        .unwrap();
    manager.find_port_handle(name).unwrap()
}

/// The bus's own lines of `out`: its debug lines without their
/// `asyn.rs:N: ` prefix, and the error lines, which have none.
fn bus_lines(out: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(out);
    let mut lines = Vec::new();
    for l in text.lines() {
        let line = match l.split_once(": ") {
            Some((at, rest)) if at.contains(".rs:") => {
                if !at.starts_with("asyn.rs:") {
                    continue;
                }
                rest
            }
            _ => l,
        };
        lines.push(mask_pointers(line));
    }
    lines
}

fn mask_pointers(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(i) = rest.find("0x") {
        out.push_str(&rest[..i + 2]);
        rest = rest[i + 2..].trim_start_matches(|c: char| c.is_ascii_hexdigit());
        out.push('*');
    }
    out.push_str(rest);
    out
}

fn oracle(case: &str) -> Vec<String> {
    let path = format!(
        "{}/tests/oracle/asyn/{case}.log",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .lines()
        .map(str::to_string)
        .collect()
}

/// Run `f` with this thread's debug and error lines captured; the port
/// thread's are not (the capture is per thread).
fn captured<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    set_stream_msg_time_stamped(0);
    capture_stderr();
    set_stream_debug(1);
    let out = f();
    set_stream_debug(0);
    (out, bus_lines(&take_captured()))
}

/// Run `f` with every thread's debug and error lines captured, the port
/// thread's included.
fn all_threads<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    all_threads_at(1, f)
}

/// [`all_threads`] at debug `level`.
fn all_threads_at<T>(level: i32, f: impl FnOnce() -> T) -> (T, Vec<String>) {
    set_stream_msg_time_stamped(0);
    let capture = capture_process_stderr();
    set_stream_debug(level);
    let out = f();
    set_stream_debug(0);
    (out, bus_lines(&capture.take()))
}

/// [`all_threads`] through the first line that starts with `last`: a
/// handler's closing debug line comes after the callback `f` waits for.
fn all_threads_through<T>(last: &str, f: impl FnOnce() -> T) -> (T, Vec<String>) {
    set_stream_msg_time_stamped(0);
    let capture = capture_process_stderr();
    set_stream_debug(1);
    let out = f();
    let deadline = std::time::Instant::now() + WAIT;
    let mut bytes = capture.take();
    while !bus_lines(&bytes).iter().any(|l| l.starts_with(last))
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
        bytes.extend(capture.take());
    }
    set_stream_debug(0);
    (out, bus_lines(&bytes))
}

/// Attach "rec" to `portname` with the debug lines captured.
fn attach(client: &Arc<dyn Client>, portname: &str) -> (Option<bus::BusInterface>, Vec<String>) {
    set_stream_msg_time_stamped(0);
    capture_stderr();
    set_stream_debug(1);
    let found = bus::find(client, portname, 0, "");
    set_stream_debug(0);
    (found, bus_lines(&take_captured()))
}

#[test]
fn attaching_prints_what_c_prints() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let handle = port("P", octet_transport_capabilities());
    let (client, _locks) = rec("rec");

    handle
        .submit_blocking(RequestOp::Connect, AsynUser::new(0))
        .unwrap();
    let (found, lines) = attach(&client, "P");
    assert!(found.is_some());
    assert_eq!(lines, oracle("attach-connected"));
    drop(found);

    disconnect_for_good(&handle);
    let (found, lines) = attach(&client, "P");
    assert!(found.is_some());
    assert_eq!(lines, oracle("attach-disconnected"));
    drop(found);

    let (found, lines) = attach(&client, "Q");
    assert!(found.is_none());
    assert_eq!(lines, oracle("attach-noport"));
}

/// No C port in streamApp lacks asynOctet; the line is
/// AsynDriverInterface.cc:445-446.
#[test]
fn a_port_without_octet_is_refused() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let _handle = port("NOOCTET", vec![Capability::Int32Read, Capability::Connect]);
    let (client, _locks) = rec("rec");
    let (found, lines) = attach(&client, "NOOCTET");
    assert!(found.is_none());
    assert_eq!(
        lines.last().map(String::as_str),
        Some("rec: asyn port NOOCTET does not support asynOctet interface")
    );
}

/// One protocol's bus ownership, as C's lockRequest/lockHandler/unlock print
/// it, with another user's request on the device held from the lock until
/// the unlock.
#[test]
fn a_lock_holds_the_device_until_unlock() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let handle = port("LK", octet_transport_capabilities());
    handle
        .submit_blocking(RequestOp::Connect, AsynUser::new(0))
        .unwrap();
    let (client, locks) = rec("rec");
    let (bus, _) = attach(&client, "LK");
    let bus = bus.unwrap();

    let ((), lines) = all_threads(|| {
        assert!(bus.bus().lock_request(5000));
        assert_eq!(
            locks.recv_timeout(WAIT),
            Ok(StreamIoStatus::StreamIoSuccess)
        );

        let other = handle.clone();
        let (ran_tx, ran) = channel();
        std::thread::spawn(move || {
            let _ = other.submit_blocking(RequestOp::Flush, AsynUser::new(0));
            ran_tx.send(()).unwrap();
        });
        assert!(
            ran.recv_timeout(Duration::from_millis(200)).is_err(),
            "another user's request ran while rec owned the device"
        );
        assert!(bus.bus().unlock());
        ran.recv_timeout(WAIT).unwrap();
    });
    assert_eq!(lines, oracle("lock"));
}

/// AsynDriverInterface.cc:570-573 with asynManager.c:1739-1743's text.
#[test]
fn an_unlock_without_a_lock_is_refused() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let handle = port("NL", octet_transport_capabilities());
    handle
        .submit_blocking(RequestOp::Connect, AsynUser::new(0))
        .unwrap();
    let (client, _locks) = rec("rec");
    let (bus, _) = attach(&client, "NL");
    let (unlocked, lines) = captured(|| bus.unwrap().bus().unlock());
    assert!(!unlocked);
    assert_eq!(
        lines.last().map(String::as_str),
        Some(
            "rec unlock: pasynManager->unblockProcessCallback() failed: \
             asynManager::unblockProcessCallback but not locked"
        )
    );
}

/// reportAsynStatus (AsynDriverInterface.cc:486-498): queueRequest's refusal
/// (asynManager.c:1549-1552) is reported once, its end once.
#[test]
fn a_refused_lock_is_reported_once_and_its_end_once() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let handle = port("DC", octet_transport_capabilities());
    disconnect_for_good(&handle);
    let (client, locks) = rec("rec");
    let (bus, _) = attach(&client, "DC");
    let bus = bus.unwrap();

    let refused = "rec lockRequest: port DC not connected".to_string();
    let (locked, lines) = captured(|| bus.bus().lock_request(100));
    assert!(!locked);
    assert_eq!(lines.last(), Some(&refused));
    let (locked, lines) = captured(|| bus.bus().lock_request(100));
    assert!(!locked);
    assert!(!lines.contains(&refused), "reported twice: {lines:?}");

    handle
        .submit_blocking(RequestOp::Connect, AsynUser::new(0))
        .unwrap();
    let (locked, lines) = captured(|| bus.bus().lock_request(100));
    assert!(locked);
    assert!(
        lines.contains(&"rec lockRequest: status returned to normal".to_string()),
        "{lines:?}"
    );
    assert_eq!(
        locks.recv_timeout(WAIT),
        Ok(StreamIoStatus::StreamIoSuccess)
    );
    assert!(bus.bus().unlock());
}

/// handleTimeout's Lock case (AsynDriverInterface.cc:1477-1479): a lock that
/// waits past its timeout behind another record's ends in
/// lockCallback(StreamIoTimeout); the holder's unlock then frees the device.
#[test]
fn a_lock_waiting_past_its_timeout_times_out() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let handle = port("TO", octet_transport_capabilities());
    handle
        .submit_blocking(RequestOp::Connect, AsynUser::new(0))
        .unwrap();
    let (first, first_locks) = rec("first");
    let (second, second_locks) = rec("second");
    let (holder, _) = attach(&first, "TO");
    let (waiter, _) = attach(&second, "TO");
    let (holder, waiter) = (holder.unwrap(), waiter.unwrap());

    let ((), lines) = all_threads(|| {
        assert!(holder.bus().lock_request(5000));
        assert_eq!(
            first_locks.recv_timeout(WAIT),
            Ok(StreamIoStatus::StreamIoSuccess)
        );
        assert!(waiter.bus().lock_request(50));
        assert_eq!(
            second_locks.recv_timeout(WAIT),
            Ok(StreamIoStatus::StreamIoTimeout)
        );
        assert!(holder.bus().unlock());
    });
    assert_eq!(lines, oracle("locktimeout"));

    assert!(waiter.bus().lock_request(5000));
    assert_eq!(
        second_locks.recv_timeout(WAIT),
        Ok(StreamIoStatus::StreamIoSuccess)
    );
    assert!(waiter.bus().unlock());
}

/// exceptionHandler (AsynDriverInterface.cc:1271-1286) and lockRequest's
/// reset (:512-516): a lock queueRequest refuses leaves the interface idle,
/// so the port's connect calls connectCallback.
#[test]
fn a_refused_lock_leaves_the_interface_idle_for_a_connect() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let handle = port("P", octet_transport_capabilities());
    disconnect_for_good(&handle);
    let (client, _locks, connects) = rec_connects("rec");
    let (bus, _) = attach(&client, "P");
    let bus = bus.unwrap();

    let ((), lines) = all_threads(|| {
        assert!(!bus.bus().lock_request(5000));
        handle
            .submit_blocking(RequestOp::Connect, AsynUser::new(0))
            .unwrap();
        assert_eq!(
            connects.recv_timeout(WAIT),
            Ok(StreamIoStatus::StreamIoSuccess)
        );
    });
    assert_eq!(lines, oracle("reconnect"));
}

/// exceptionHandler calls connectCallback only with no I/O in progress
/// (AsynDriverInterface.cc:1283): an interface whose lock ran keeps ioAction
/// Lock until finish() and is not called back.
#[test]
fn a_connect_does_not_call_back_an_interface_with_io_in_progress() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let handle = port("BY", octet_transport_capabilities());
    handle
        .submit_blocking(RequestOp::Connect, AsynUser::new(0))
        .unwrap();
    let (busy, busy_locks, busy_connects) = rec_connects("busy");
    let (idle, _, idle_connects) = rec_connects("idle");
    let (busy_bus, _) = attach(&busy, "BY");
    let (_idle_bus, _) = attach(&idle, "BY");
    let busy_bus = busy_bus.unwrap();
    assert!(busy_bus.bus().lock_request(5000));
    assert_eq!(
        busy_locks.recv_timeout(WAIT),
        Ok(StreamIoStatus::StreamIoSuccess)
    );
    assert!(busy_bus.bus().unlock());

    let ((), lines) = all_threads(|| {
        handle
            .submit_blocking(RequestOp::Disconnect, AsynUser::new(0))
            .unwrap();
        handle
            .submit_blocking(RequestOp::Connect, AsynUser::new(0))
            .unwrap();
        assert_eq!(
            idle_connects.recv_timeout(WAIT),
            Ok(StreamIoStatus::StreamIoSuccess)
        );
    });
    assert!(
        lines.contains(
            &"AsynDriverInterface::exceptionHandler(busy) BY 0 connected. ioAction: Lock"
                .to_string()
        ),
        "{lines:?}"
    );
    assert_eq!(busy_connects.try_recv().ok(), None, "{lines:?}");
}

/// streamReinit (AsynDriverInterface.cc:1502-1525): exceptionDisconnect and
/// then exceptionConnect on the port, each heard by the attached interface,
/// which calls back connected. Auto-connect is off, so only the
/// exceptionConnect can bring the port up.
#[test]
fn stream_reinit_takes_the_port_down_and_up_as_c_does() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let handle = port("P", octet_transport_capabilities());
    handle
        .submit_blocking(RequestOp::Connect, AsynUser::new(0))
        .unwrap();
    handle
        .submit_blocking(RequestOp::SetAutoConnect { yes: false }, AsynUser::new(0))
        .unwrap();
    let (client, _locks, connects) = rec_connects("rec");
    let (bus, _) = attach(&client, "P");
    let _bus = bus.unwrap();

    let mut err = Vec::new();
    let (status, lines) = all_threads(|| {
        let status = stream_rs::asyn::stream_reinit(Some("P"), 0, &mut err);
        assert_eq!(
            connects.recv_timeout(WAIT),
            Ok(StreamIoStatus::StreamIoSuccess)
        );
        status
    });
    assert_eq!(status, 0);
    assert_eq!(String::from_utf8_lossy(&err), "");
    let handler: Vec<String> = lines
        .into_iter()
        .filter(|l| l.contains("::exceptionHandler("))
        .collect();
    assert_eq!(handler, oracle("reinit"));
}

/// streamReinit's failures print the usage or asynManager's errorMessage to
/// stderr (AsynDriverInterface.cc:1504-1520).
#[test]
fn stream_reinit_failures_print_what_c_prints() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    let handle = port("P", octet_transport_capabilities());
    disconnect_for_good(&handle);
    let mut err = Vec::new();
    let statuses = [None, Some("Z"), Some("P")]
        .map(|portname| stream_rs::asyn::stream_reinit(portname, 0, &mut err));
    assert_eq!(statuses, [-1, 3, 3]);
    let lines: Vec<String> = String::from_utf8_lossy(&err)
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(lines, oracle("reinit-fail"));

    let commands = stream_rs::asyn::iocsh_commands();
    let names: Vec<_> = commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["streamReinit"]);
    let args: Vec<_> = commands[0].args.iter().map(|a| a.name).collect();
    assert_eq!(args, ["portname", "[addr]"]);
}

/// lockHandler's refusal (AsynDriverInterface.cc:550-556): a synchronous port
/// refuses blockProcessCallback, and the lock ends in StreamIoFault.
#[test]
fn a_synchronous_port_faults_the_lock() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let handle = port_with("E", octet_transport_capabilities(), false);
    handle
        .submit_blocking(RequestOp::Connect, AsynUser::new(0))
        .unwrap();
    let (client, locks) = rec("rec");
    let (bus, _) = attach(&client, "E");
    let bus = bus.unwrap();

    let ((), lines) = all_threads(|| {
        assert!(bus.bus().lock_request(5000));
        assert_eq!(locks.recv_timeout(WAIT), Ok(StreamIoStatus::StreamIoFault));
    });
    assert_eq!(lines, oracle("syncrefused"));
}

/// The destructor's cancelRequest (AsynDriverInterface.cc:308) ends a lock
/// still waiting for the device with neither handleRequest nor handleTimeout.
#[test]
fn dropping_an_interface_cancels_its_waiting_lock() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let handle = port("CN", octet_transport_capabilities());
    handle
        .submit_blocking(RequestOp::Connect, AsynUser::new(0))
        .unwrap();
    let (first, first_locks) = rec("first");
    let (second, second_locks) = rec("second");
    let (holder, _) = attach(&first, "CN");
    let (waiter, _) = attach(&second, "CN");
    let holder = holder.unwrap();

    let ((), lines) = all_threads(|| {
        assert!(holder.bus().lock_request(5000));
        assert_eq!(
            first_locks.recv_timeout(WAIT),
            Ok(StreamIoStatus::StreamIoSuccess)
        );
        assert!(waiter.as_ref().unwrap().bus().lock_request(100));
        drop(waiter);
        std::thread::sleep(Duration::from_millis(200));
        assert!(holder.bus().unlock());
        std::thread::sleep(Duration::from_millis(100));
    });
    assert_eq!(second_locks.try_recv().ok(), None, "{lines:?}");
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("(second)") && !l.contains("lockRequest")),
        "{lines:?}"
    );
}

/// Dropping an interface that holds the lock releases its block, so another
/// user's request runs. C leaves the port blocked for good here (see
/// `RequestCell`'s drop); that defect is not copied.
#[test]
fn dropping_an_interface_that_holds_the_lock_frees_the_port() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let handle = port("DH", octet_transport_capabilities());
    handle
        .submit_blocking(RequestOp::Connect, AsynUser::new(0))
        .unwrap();
    let (client, locks) = rec("rec");
    let (holder, _) = attach(&client, "DH");
    let holder = holder.unwrap();
    assert!(holder.bus().lock_request(5000));
    assert_eq!(
        locks.recv_timeout(WAIT),
        Ok(StreamIoStatus::StreamIoSuccess)
    );
    drop(holder);

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tx.send(
            handle
                .submit_blocking(RequestOp::Flush, AsynUser::new(0))
                .is_ok(),
        )
    });
    assert_eq!(rx.recv_timeout(WAIT), Ok(true));
}

/// `rt/dev.py` on a port of its own: it sends `greeting`
/// on connecting and, whenever what it received since its last answer ends
/// with a request of `table`, that request's reply. Returns its address.
fn device(greeting: &'static [u8], table: &'static [(&'static [u8], &'static [u8])]) -> String {
    device_heard(greeting, table).0
}

/// [`device`], with what it receives, read by read.
fn device_heard(
    greeting: &'static [u8],
    table: &'static [(&'static [u8], &'static [u8])],
) -> (String, Receiver<Vec<u8>>) {
    let (heard, hears) = channel();
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        // Each connection in turn, as dev.py serves them.
        for mut c in listener.incoming().map_while(Result::ok) {
            if c.write_all(greeting).is_err() {
                continue;
            }
            let mut got = Vec::new();
            let mut buf = [0u8; 1000];
            while let Ok(n @ 1..) = c.read(&mut buf) {
                got.extend_from_slice(&buf[..n]);
                let _ = heard.send(buf[..n].to_vec());
                if let Some((_, reply)) = table.iter().find(|(k, _)| got.ends_with(k)) {
                    got.clear();
                    if c.write_all(reply).is_err() {
                        break;
                    }
                }
            }
        }
    });
    (addr, hears)
}

/// `drvAsynIPPortConfigure name host 0 0 0`; it connects on its own.
fn ip_port(name: &str, host: &str) -> asyn_rs::port_handle::PortHandle {
    let driver =
        asyn_rs::drivers::ip_port::DrvAsynIPPort::new_configured(name, host, false, false).unwrap();
    let manager = PortManager::new();
    manager
        .register_port(
            claim_port_driver(name).unwrap(),
            driver,
            &asyn_rs::registry::open_owner(),
        )
        .unwrap();
    manager.find_port_handle(name).unwrap()
}

/// "rec" holding `name`, as a protocol's `out` finds the bus; the client
/// lives as long as the bus is used.
fn locked(
    name: &str,
    out_terminator: Option<&[u8]>,
) -> (bus::BusInterface, Calls, Arc<dyn Client>) {
    locked_terminated(name, None, out_terminator)
}

/// [`locked`], the protocol with an in terminator too.
fn locked_terminated(
    name: &str,
    in_terminator: Option<&[u8]>,
    out_terminator: Option<&[u8]>,
) -> (bus::BusInterface, Calls, Arc<dyn Client>) {
    let (client, calls) = rec_terminated("rec", in_terminator, out_terminator);
    let (bus, _) = attach(&client, name);
    let bus = bus.unwrap();
    assert!(bus.bus().lock_request(5000));
    assert_eq!(
        calls.locks.recv_timeout(WAIT),
        Ok(StreamIoStatus::StreamIoSuccess)
    );
    (bus, calls, client)
}

/// The lines of `lines` that C's `keep` selects by these function names.
fn of(lines: &[String], functions: &[&str]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| {
            functions.iter().any(|f| {
                l.starts_with(&format!("AsynDriverInterface::{f}("))
                    || l.starts_with(&format!("AsynDriverInterface::{f} "))
            })
        })
        .cloned()
        .collect()
}

/// writeRequest and writeHandler (AsynDriverInterface.cc:581-758): the old
/// input is polled for, then the output written with the write timeout.
#[test]
fn a_write_prints_what_c_prints() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let _handle = ip_port("WR", &device(b"", &[(b"X", b"1\r")]));
    let (bus, calls, _client) = locked("WR", None);
    let ((), lines) = all_threads(|| {
        assert!(bus.bus().write_request(b"X", 100));
        assert_eq!(
            calls.writes.recv_timeout(WAIT),
            Ok(StreamIoStatus::StreamIoSuccess)
        );
    });
    assert_eq!(
        of(&lines, &["writeRequest", "writeHandler"]),
        oracle("write")
    );
}

/// Input that came before the write is read and dropped, not flushed
/// (AsynDriverInterface.cc:612-631).
#[test]
fn a_write_reads_away_early_input() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let _handle = ip_port("WF", &device(b"early\r", &[(b"X", b"1\r")]));
    let (bus, calls, _client) = locked("WF", None);
    std::thread::sleep(Duration::from_millis(100));
    let ((), lines) = all_threads(|| {
        assert!(bus.bus().write_request(b"X", 100));
        assert_eq!(
            calls.writes.recv_timeout(WAIT),
            Ok(StreamIoStatus::StreamIoSuccess)
        );
    });
    assert_eq!(of(&lines, &["writeHandler"]), oracle("flush"));
}

/// A scripted read: the bytes and eomReason, or a failure with the bytes
/// read before it.
type ScriptRead =
    Result<(&'static [u8], EomReason), (asyn_rs::error::AsynStatus, &'static str, &'static [u8])>;

/// What a [`Script`] port's writes, reads and connects answer, in order; a
/// write past the script writes everything, a read past it times out with
/// nothing.
#[derive(Default)]
struct Book {
    writes: VecDeque<Result<usize, (asyn_rs::error::AsynStatus, &'static str)>>,
    /// Each write's bytes.
    written: Vec<Vec<u8>>,
    reads: VecDeque<ScriptRead>,
    /// Each read's buffer size.
    read_sizes: Vec<usize>,
    connects: u32,
    /// The error a connect or a disconnect fails with, if it does.
    connect_fails: Option<&'static str>,
    disconnect_fails: Option<&'static str>,
    /// Whether a write or read that fails also takes the port down, as
    /// drvAsynIPPort's closeConnection does.
    drops: bool,
}

/// A port whose octet transfers follow a [`Book`].
struct Script {
    base: PortDriverBase,
    book: Arc<Mutex<Book>>,
}

impl PortDriver for Script {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn capabilities(&self) -> Vec<Capability> {
        octet_transport_capabilities()
    }
    fn connect(&mut self, user: &AsynUser) -> asyn_rs::error::AsynResult<()> {
        let mut book = self.book.lock().unwrap();
        book.connects += 1;
        if let Some(message) = book.connect_fails {
            return Err(asyn_rs::error::AsynError::Status {
                status: AsynErr,
                message: message.into(),
            });
        }
        drop(book);
        self.base_mut().set_addr_connected(user.addr, true);
        Ok(())
    }
    fn disconnect(&mut self, user: &AsynUser) -> asyn_rs::error::AsynResult<()> {
        if let Some(message) = self.book.lock().unwrap().disconnect_fails {
            return Err(asyn_rs::error::AsynError::Status {
                status: AsynErr,
                message: message.into(),
            });
        }
        self.base_mut().set_addr_connected(user.addr, false);
        Ok(())
    }
    fn io_read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> OctetReadResult {
        let mut book = self.book.lock().unwrap();
        book.read_sizes.push(buf.len());
        let read = book
            .reads
            .pop_front()
            .unwrap_or(Err((Timeout, "nothing", b"")));
        match read {
            Ok((data, eom)) => {
                buf[..data.len()].copy_from_slice(data);
                OctetReadResult::ok(data.len(), eom)
            }
            Err((status, message, data)) => {
                if book.drops {
                    drop(book);
                    self.base_mut().set_addr_connected(user.addr, false);
                }
                buf[..data.len()].copy_from_slice(data);
                OctetReadResult {
                    status: Err(asyn_rs::error::AsynError::Status {
                        status,
                        message: message.into(),
                    }),
                    ..OctetReadResult::ok(data.len(), EomReason::empty())
                }
            }
        }
    }
    fn io_write_octet(
        &mut self,
        user: &mut AsynUser,
        data: &[u8],
    ) -> asyn_rs::error::AsynResult<usize> {
        let mut book = self.book.lock().unwrap();
        book.written.push(data.to_vec());
        match book.writes.pop_front().unwrap_or(Ok(data.len())) {
            Ok(n) => Ok(n.min(data.len())),
            Err((status, message)) => {
                if book.drops {
                    drop(book);
                    self.base_mut().set_addr_connected(user.addr, false);
                }
                Err(asyn_rs::error::AsynError::Status {
                    status,
                    message: message.into(),
                })
            }
        }
    }
}

/// Disconnect `handle`'s port and keep it down: an auto-connect port's
/// connect timer connects it again 10 ms on (asynManager.c:2184).
fn disconnect_for_good(handle: &asyn_rs::port_handle::PortHandle) {
    handle.set_auto_connect(0, false).unwrap();
    handle
        .submit_blocking(RequestOp::Disconnect, AsynUser::new(0))
        .unwrap();
}

/// A connected [`Script`] port called `name`, and its book.
fn script(name: &str) -> (asyn_rs::port_handle::PortHandle, Arc<Mutex<Book>>) {
    script_with(name, false)
}

/// [`script`], its reads handed to the octet interrupt users if
/// `octet_interrupts`, as asynOctetBase's `interruptProcess`, under the
/// EOS layer an IP port has.
fn script_with(
    name: &str,
    octet_interrupts: bool,
) -> (asyn_rs::port_handle::PortHandle, Arc<Mutex<Book>>) {
    let flags = PortFlags {
        can_block: true,
        ..PortFlags::default()
    };
    let book = Arc::new(Mutex::new(Book::default()));
    let mut base = PortDriverBase::new(name, 1, flags, InterfaceMask::ALL, InterfaceMask::ALL);
    if octet_interrupts {
        base.octet_interrupt_process = true;
        base.install_octet_interpose(Box::new(EosInterpose::default()));
    }
    let manager = PortManager::new();
    manager
        .register_port(
            claim_port_driver(name).unwrap(),
            Script {
                base,
                book: book.clone(),
            },
            &asyn_rs::registry::open_owner(),
        )
        .unwrap();
    let handle = manager.find_port_handle(name).unwrap();
    handle
        .submit_blocking(RequestOp::Connect, AsynUser::new(0))
        .unwrap();
    book.lock().unwrap().connects = 0;
    (handle, book)
}

/// The callback a write ended with.
#[derive(Debug, PartialEq)]
enum Ended {
    Write(StreamIoStatus),
    Disconnect(StreamIoStatus),
}

/// Writes `output` with "rec" holding `name`, a [`script`] port following
/// `writes`; returns the bus's lines, how the write ended and the book.
fn scripted_write(
    name: &str,
    output: &[u8],
    writes: &[Result<usize, (asyn_rs::error::AsynStatus, &'static str)>],
    drops: bool,
) -> (Vec<String>, Ended, Arc<Mutex<Book>>) {
    let (handle, book) = script(name);
    if drops {
        // The port stays down after a drop: asynManager's connect timer
        // would bring it back 10 ms on and print past the line the run
        // ends with. connectToAsynPort connects without it (:530-537).
        handle.set_auto_connect(0, false).unwrap();
    }
    {
        let mut book = book.lock().unwrap();
        book.writes = writes.iter().cloned().collect();
        book.drops = drops;
    }
    let (bus, calls, _client) = locked(name, None);
    let (ended, lines) = all_threads(|| {
        assert!(bus.bus().write_request(output, 100));
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            if let Ok(status) = calls.writes.try_recv() {
                break Ended::Write(status);
            }
            if let Ok(status) = calls.disconnects.try_recv() {
                break Ended::Disconnect(status);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the write never ended"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    (lines, ended, book)
}

use asyn_rs::error::AsynStatus::{Disabled, Disconnected, Error as AsynErr, Overflow, Timeout};
use asyn_rs::interpose::eos::EosInterpose;
use asyn_rs::interpose::{EomReason, OctetReadResult};

/// A write that fails is tried once more after connectToAsynPort, which
/// connects nothing on a port still up (AsynDriverInterface.cc:662-686,
/// :525-539, :731-736).
#[test]
fn a_failed_write_is_tried_once_more() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (lines, ended, book) = scripted_write(
        "WE",
        b"X",
        &[Err((AsynErr, "boom")), Err((AsynErr, "boom"))],
        false,
    );
    assert_eq!(ended, Ended::Write(StreamIoStatus::StreamIoFault));
    assert_eq!(book.lock().unwrap().connects, 0);
    assert_eq!(
        of(&lines, &["writeHandler", "connectToAsynPort"])[2..],
        [
            "AsynDriverInterface::writeHandler(rec): write(..., \"X\", outputSize=1, written=0) \
             [timeout=0.1 sec] = asynError boom try to connect",
            "AsynDriverInterface::connectToAsynPort(rec)",
            "AsynDriverInterface::writeHandler(rec): write(..., \"X\", outputSize=1, written=0) \
             [timeout=0.1 sec] = asynError boom failed twice",
        ]
    );
    assert_eq!(lines.last().unwrap(), "rec: asynError in write: boom");
}

/// The second try's success is the write's.
#[test]
fn a_write_that_fails_once_succeeds() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (_, ended, book) = scripted_write("WO", b"X", &[Err((AsynErr, "gone"))], false);
    assert_eq!(ended, Ended::Write(StreamIoStatus::StreamIoSuccess));
    assert_eq!(book.lock().unwrap().written, [b"X".to_vec(), b"X".to_vec()]);
}

/// A failed write that took the port down: connectToAsynPort connects it
/// (:530-537), and the retry runs.
#[test]
fn a_write_that_drops_the_port_reconnects_it() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (_, ended, book) = scripted_write("WD", b"X", &[Err((AsynErr, "gone"))], true);
    assert_eq!(ended, Ended::Write(StreamIoStatus::StreamIoSuccess));
    assert_eq!(book.lock().unwrap().connects, 1);
}

/// A failed write on a device that went away: "device ... disconnected"
/// and disconnectCallback (:723-729).
#[test]
fn a_write_to_a_lost_device_disconnects() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (lines, ended, _) = scripted_write(
        "WL",
        b"X",
        &[Err((AsynErr, "gone")), Err((AsynErr, "gone"))],
        true,
    );
    assert_eq!(ended, Ended::Disconnect(StreamIoStatus::StreamIoSuccess));
    assert_eq!(lines.last().unwrap(), "rec: device WL 0 disconnected");
}

/// :713-717 and :738-742: a timeout is not tried again.
#[test]
fn a_write_timeout_and_asyn_disconnected_end_the_write() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (lines, ended, book) = scripted_write("WT", b"X", &[Err((Timeout, "slow"))], false);
    assert_eq!(ended, Ended::Write(StreamIoStatus::StreamIoTimeout));
    assert_eq!(book.lock().unwrap().written.len(), 1);
    assert_eq!(
        lines.last().unwrap(),
        "rec: asynTimeout (0.1 sec) in write: slow"
    );

    let (lines, ended, _) = scripted_write("WN", b"X", &[Err((Disconnected, "down"))], false);
    assert_eq!(ended, Ended::Disconnect(StreamIoStatus::StreamIoSuccess));
    assert_eq!(
        lines.last().unwrap(),
        "rec: asynDisconnected in write: down"
    );
}

/// A short write queues the rest (:694-709), and one writeCallback ends
/// both.
#[test]
fn a_short_write_writes_the_rest() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (lines, ended, book) = scripted_write("WS", b"XY", &[Ok(1)], false);
    assert_eq!(ended, Ended::Write(StreamIoStatus::StreamIoSuccess));
    assert_eq!(
        book.lock().unwrap().written,
        [b"XY".to_vec(), b"Y".to_vec()]
    );
    let writes: Vec<_> = lines.iter().filter(|l| l.contains("): write(")).collect();
    assert_eq!(writes.len(), 2, "{lines:?}");
    assert!(
        writes[1].contains("\"Y\", outputSize=1, written=1"),
        "{lines:?}"
    );
}

/// With the protocol's terminator already in the output, asyn's is cleared
/// for the write and put back after (:647-689).
#[test]
fn a_write_with_a_protocol_terminator_suspends_asyns() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (host, heard) = device_heard(b"", &[]);
    let handle = ip_port("WEOS", &host);
    let eos = || {
        handle
            .submit_blocking(RequestOp::GetOutputEos, AsynUser::new(0))
            .unwrap()
            .data
            .unwrap_or_default()
    };
    handle
        .submit_blocking(
            RequestOp::SetOutputEos {
                eos: b"\n".to_vec(),
            },
            AsynUser::new(0),
        )
        .unwrap();
    let (bus, calls, _client) = locked("WEOS", Some(b"\r"));
    assert!(bus.bus().write_request(b"X\r", 100));
    assert_eq!(
        calls.writes.recv_timeout(WAIT),
        Ok(StreamIoStatus::StreamIoSuccess)
    );
    assert_eq!(heard.recv_timeout(WAIT).unwrap(), b"X\r");
    assert!(bus.bus().unlock());
    assert_eq!(eos(), b"\n");
}

/// handleTimeout on a write still queued (:1477-1479): writeCallback
/// with StreamIoTimeout.
#[test]
fn a_write_left_queued_times_out() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let handle = port("WQ", octet_transport_capabilities());
    handle
        .submit_blocking(RequestOp::Connect, AsynUser::new(0))
        .unwrap();
    let (_holder, _, _holder_client) = locked("WQ", None);
    let (client, calls) = rec_calls("second", None);
    let (bus, _) = attach(&client, "WQ");
    let bus = bus.unwrap();
    assert!(bus.bus().write_request(b"X", 50));
    assert_eq!(
        calls.writes.recv_timeout(WAIT),
        Ok(StreamIoStatus::StreamIoTimeout)
    );
}

/// The readCallbacks a record got, in order.
type Reads = Vec<(StreamIoStatus, OwnedReadInput)>;

/// The readCallbacks "rec" got until one ended its read, answering 0, or
/// `WAIT` passed with none.
fn reads_until_end(calls: &Calls) -> Reads {
    let mut got = Vec::new();
    while let Ok((status, input, more)) = calls.reads.recv_timeout(WAIT) {
        got.push((status, input));
        if more == 0 {
            break;
        }
    }
    got
}

/// "rec" on an IP port to a device answering `table`, its protocol's
/// in terminator `in_terminator`: writes "X", reads the reply and unlocks;
/// returns the lines, the reads, and the port's input EOS after, asked for
/// once readHandler is done.
fn read_reply(
    name: &str,
    table: &'static [(&'static [u8], &'static [u8])],
    in_terminator: &[u8],
    level: i32,
    before: impl FnOnce(&asyn_rs::port_handle::PortHandle),
) -> (Vec<String>, Reads, Vec<u8>) {
    let handle = ip_port(name, &device(b"", table));
    before(&handle);
    let (bus, calls, _client) = locked_terminated(name, Some(in_terminator), None);
    let ((reads, eos), lines) = all_threads_at(level, || {
        assert!(bus.bus().write_request(b"X", 100));
        assert_eq!(
            calls.writes.recv_timeout(WAIT),
            Ok(StreamIoStatus::StreamIoSuccess)
        );
        assert!(bus.bus().read_request(1000, 100, 0, false));
        let reads = reads_until_end(&calls);
        assert!(bus.bus().unlock());
        let eos = handle
            .submit_blocking(RequestOp::GetInputEos, AsynUser::new(0))
            .unwrap()
            .data
            .unwrap_or_default();
        (reads, eos)
    });
    (lines, reads, eos)
}

/// `lines` and the oracle `case` agree on the requesting thread's
/// readRequest lines and, apart, on the port thread's readHandler lines:
/// which thread prints first after queueRequest is a race.
fn assert_read_lines(lines: &[String], case: &str) {
    let expected = oracle(case);
    for f in ["readRequest", "readHandler"] {
        assert_eq!(of(lines, &[f]), of(&expected, &[f]), "{f}");
    }
}

/// The error lines of `lines`: those not naming a function.
fn errors(lines: &[String]) -> Vec<&str> {
    lines
        .iter()
        .map(String::as_str)
        .filter(|l| !l.starts_with("AsynDriverInterface::"))
        .collect()
}

fn read(status: StreamIoStatus, input: &[u8]) -> (StreamIoStatus, OwnedReadInput) {
    (status, OwnedReadInput::Bytes(input.to_vec()))
}

use StreamIoStatus::{
    StreamIoEnd as End, StreamIoFault as Fault, StreamIoNoReply as NoReply,
    StreamIoSuccess as Success, StreamIoTimeout as IoTimeout,
};

/// readRequest and readHandler (AsynDriverInterface.cc:758-1099): a peek
/// of one byte, then the rest up to the terminator asyn cuts off.
#[test]
fn a_read_prints_what_c_prints() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (lines, reads, _) = read_reply("RD", &[(b"X", b"1.5\r")], b"\r", 1, |_| {});
    assert_eq!(reads, [read(Success, b"1"), read(End, b".5")]);
    assert_read_lines(&lines, "read");
}

/// No reply within the reply timeout (:978-1001).
#[test]
fn a_read_without_a_reply_reports_none() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (lines, reads, _) = read_reply("RN", &[(b"Q", b"")], b"\r", 1, |_| {});
    assert_eq!(reads, [read(NoReply, b"")]);
    assert_read_lines(&lines, "noreply");
}

/// A reply that stops before its terminator: the read timeout ends it with
/// what came (:1002-1017).
#[test]
fn a_read_cut_short_times_out_with_its_input() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (lines, reads, _) = read_reply("RT", &[(b"X", b"12")], b"\r", 1, |_| {});
    assert_eq!(reads, [read(Success, b"1"), read(IoTimeout, b"2")]);
    assert_read_lines(&lines, "readtimeout");
}

/// A terminator longer than asyn's EOS takes is set from its tail
/// (:833-855), and what asyn cut is put back (:944-956).
#[test]
fn a_long_terminator_is_set_as_far_as_asyn_takes_it() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (lines, reads, eos) = read_reply("RE", &[(b"X", b"1\r\n\r")], b"\r\n\r", 2, |_| {});
    assert_eq!(reads, [read(Success, b"1"), read(Success, b"\r\n\r")]);
    assert_eq!(of(&lines, &["readHandler"]), oracle("eos"));
    assert_eq!(eos, b"");
}

/// The port's own input EOS is put back after the read. C does not when it
/// has the protocol terminator's length (:1088-1089): its log ends without
/// the restore.
#[test]
fn a_replaced_input_eos_of_the_same_length_is_restored() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (lines, reads, eos) = read_reply("RR", &[(b"X", b"1\r")], b"\r", 2, |handle| {
        handle
            .submit_blocking(
                RequestOp::SetInputEos {
                    eos: b"\n".to_vec(),
                },
                AsynUser::new(0),
            )
            .unwrap();
    });
    assert_eq!(reads, [read(Success, b"1"), read(End, b"")]);
    let mut expected = oracle("eosrestore");
    expected.push(
        "AsynDriverInterface::readHandler(rec) input EOS restored from \"<0d>\" to \"<0a>\"".into(),
    );
    assert_eq!(of(&lines, &["readHandler"]), expected);
    assert_eq!(eos, b"\n");
}

/// An empty reply: the one-byte peek delivers the CR, and asyn's EOS layer
/// takes it back with the LF, so C calls readCallback with size -1
/// (asynInterposeEos.c:201-204, AsynDriverInterface.cc:957-959).
#[test]
fn a_terminator_split_by_the_peek_is_taken_back() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (lines, reads, _) = read_reply("RS", &[(b"X", b"\r\n")], b"\r\n", 2, |_| {});
    assert_eq!(
        reads,
        [read(Success, b"\r"), (End, OwnedReadInput::TakeBack(1))]
    );
    assert_eq!(of(&lines, &["readHandler"]), oracle("split"));
}

/// The same split under a terminator asyn takes only the tail of: C puts
/// back the terminator bytes from the buffer start on, the one before it
/// (received -1) is the byte already delivered (:944-956).
#[test]
fn a_split_terminator_is_restored_from_the_buffer_start() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (lines, reads, _) = read_reply("RP", &[(b"X", b"\n\r")], b"\r\n\r", 2, |_| {});
    assert_eq!(
        reads,
        [
            read(Success, b"\n"),
            read(Success, b"\r"),
            read(IoTimeout, b"")
        ]
    );
    assert_eq!(of(&lines, &["readHandler"]), oracle("splitrestore"));
}

/// Reads "rec" makes on a [`script`] port `name` following `reads`, with
/// `read_more` its readCallbacks' first answers; returns the lines, the
/// callbacks until one ended the read, and the book.
fn scripted_read(
    name: &str,
    reads: &[ScriptRead],
    read_more: &[isize],
    drops: bool,
) -> (Vec<String>, Reads, Arc<Mutex<Book>>) {
    let (handle, book) = script(name);
    if drops {
        // The port stays down after a drop: asynManager's connect timer
        // would bring it back 10 ms on and print past the line the run
        // ends with. connectToAsynPort connects without it (:530-537).
        handle.set_auto_connect(0, false).unwrap();
    }
    {
        let mut book = book.lock().unwrap();
        book.reads = reads.iter().cloned().collect();
        book.drops = drops;
    }
    let (bus, calls, _client) = locked(name, None);
    calls.read_more.lock().unwrap().extend(read_more);
    let (got, lines) = all_threads(|| {
        assert!(bus.bus().read_request(100, 100, 0, false));
        reads_until_end(&calls)
    });
    (lines, got, book)
}

/// A positive readMore is the size of the next read (:1082-1085).
#[test]
fn read_more_sizes_the_next_read() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (_, got, book) = scripted_read(
        "RM",
        &[
            Ok((b"a", EomReason::CNT)),
            Ok((b"bcd", EomReason::CNT)),
            Ok((b"e", EomReason::END)),
        ],
        &[3],
        false,
    );
    assert_eq!(
        got,
        [read(Success, b"a"), read(Success, b"bcd"), read(End, b"e")]
    );
    assert_eq!(book.lock().unwrap().read_sizes, [1, 3, 63]);
}

/// An overflow delivers what was read as a fault and makes the next read
/// as large as the grown buffer (:1023-1039).
#[test]
fn an_overflow_faults_the_read_and_grows_the_next() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (_handle, book) = script("RO");
    book.lock()
        .unwrap()
        .reads
        .push_back(Err((Overflow, "full", b"a")));
    let (bus, calls, _client) = locked("RO", None);
    let ((), lines) = all_threads(|| {
        assert!(bus.bus().read_request(100, 100, 0, false));
        assert_eq!(reads_until_end(&calls), [read(Fault, b"a")]);
        assert!(bus.bus().read_request(100, 100, 0, false));
        assert_eq!(reads_until_end(&calls), [read(NoReply, b"")]);
    });
    assert!(lines.contains(&"rec: asynOverflow in read: full".to_string()));
    let sizes = book.lock().unwrap().read_sizes.clone();
    assert_eq!(sizes[0], 1);
    assert!(sizes[1] >= 100, "{sizes:?}");
}

/// The other ends of a read (:1040-1072).
#[test]
fn read_errors_end_the_read() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (lines, got, _) = scripted_read("RX", &[Err((AsynErr, "boom", b"a"))], &[], false);
    assert_eq!(got, [read(Fault, b"a")]);
    assert_eq!(errors(&lines), ["rec: asynError in read: boom"]);

    let (lines, got, _) = scripted_read("RB", &[Err((Disabled, "off", b""))], &[], false);
    assert_eq!(got, [read(Fault, b"")]);
    assert_eq!(errors(&lines), ["rec: asynDisabled in read: off"]);

    for (name, status, line) in [
        ("RL", AsynErr, "rec: device RL 0 disconnected"),
        ("RC", Disconnected, "rec: asynDisconnected in read: gone"),
    ] {
        let (_handle, book) = script(name);
        {
            let mut book = book.lock().unwrap();
            book.reads.push_back(Err((status, "gone", b"")));
            book.drops = true;
        }
        let (bus, calls, _client) = locked(name, None);
        let ((), lines) = all_threads(|| {
            assert!(bus.bus().read_request(100, 100, 0, false));
            assert_eq!(
                calls.disconnects.recv_timeout(WAIT),
                Ok(StreamIoStatus::StreamIoSuccess)
            );
        });
        assert!(calls.reads.try_recv().is_err());
        assert_eq!(errors(&lines), [line]);
    }
}

/// handleTimeout on a read still queued (:1480-1482): readCallback with
/// StreamIoFault.
#[test]
fn a_read_left_queued_faults() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let handle = port("RQ", octet_transport_capabilities());
    handle
        .submit_blocking(RequestOp::Connect, AsynUser::new(0))
        .unwrap();
    let (_holder, _, _holder_client) = locked("RQ", None);
    let (client, calls) = rec_calls("second", None);
    let (bus, _) = attach(&client, "RQ");
    let bus = bus.unwrap();
    assert!(bus.bus().read_request(50, 100, 0, false));
    assert_eq!(
        calls.reads.recv_timeout(WAIT).map(|(s, i, _)| (s, i)),
        Ok(read(Fault, b""))
    );
}

/// An async read polls with no timeout and, finding nothing, polls again
/// after the reply timeout from the timer (:991-997, :1306-1332), until the
/// interface goes (:285).
#[test]
fn an_async_read_polls_until_the_interface_goes() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (_handle, book) = script("RA");
    let (bus, calls, _client) = locked("RA", None);
    assert!(bus.bus().unlock());
    assert!(bus.bus().read_request(20, 100, 0, true));
    std::thread::sleep(Duration::from_millis(500));
    let polls = book.lock().unwrap().read_sizes.len();
    assert!(polls >= 3, "{polls} polls");
    assert!(calls.reads.try_recv().is_err());
    drop(bus);
    let polls = book.lock().unwrap().read_sizes.len();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(book.lock().unwrap().read_sizes.len(), polls);
}

/// What an async poll reads is not handed on, asynReadHandler has had it;
/// it reads on until the read timeout, which alone is reported
/// (:925-938, :1010-1014). A poll's timeout with bytes is a success
/// (:908-917).
#[test]
fn an_async_poll_ignores_its_data() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (_handle, book) = script("RP");
    let script_reads: [ScriptRead; 2] = [
        Err((Timeout, "part", b"abc")),
        Ok((b"de", EomReason::empty())),
    ];
    book.lock().unwrap().reads.extend(script_reads);
    let (bus, calls, _client) = locked("RP", None);
    let ((), lines) = all_threads(|| {
        assert!(bus.bus().read_request(1000, 100, 0, true));
        assert_eq!(
            calls.reads.recv_timeout(WAIT).map(|(s, i, _)| (s, i)),
            Ok(read(IoTimeout, b""))
        );
    });
    assert!(calls.reads.try_recv().is_err());
    assert_eq!(
        of(&lines, &["readHandler"]),
        [
            "AsynDriverInterface::readHandler(rec): ioAction=AsyncRead read(63 bytes, \
             timeout=0 sec) returned status asynTimeout: received=3 bytes, eomReason=NONE, \
             buffer=\"abc\"",
            "AsynDriverInterface::readHandler(rec): AsyncRead poll: received 3 of 63 bytes \
             \"abc\" eomReason=NONE [data ignored]",
            "AsynDriverInterface::readHandler(rec) readMore=-1 bytesToRead=63",
            "AsynDriverInterface::readHandler(rec): ioAction=AsyncRead read(63 bytes, \
             timeout=0.1 sec) returned status asynSuccess: received=2 bytes, eomReason=NONE, \
             buffer=\"de\"",
            "AsynDriverInterface::readHandler(rec): AsyncRead poll: received 2 of 63 bytes \
             \"de\" eomReason=NONE [data ignored]",
            "AsynDriverInterface::readHandler(rec) readMore=-1 bytesToRead=63",
            "AsynDriverInterface::readHandler(rec): ioAction=AsyncRead read(63 bytes, \
             timeout=0.1 sec) returned status asynTimeout: received=0 bytes, eomReason=NONE, \
             buffer=\"\"",
            "AsynDriverInterface::readHandler(rec): ioAction=AsyncRead, timeout [0.1 sec] \
             after 0 of 63 bytes \"\"",
        ]
    );
}

/// A request run cancels the poll timer (:1434): a write after an async
/// poll is not followed by the timer's.
#[test]
fn a_request_cancels_the_poll_timer() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (_handle, book) = script("RW");
    let (bus, calls, _client) = locked("RW", None);
    assert!(bus.bus().unlock());
    let ((), lines) = all_threads(|| {
        assert!(bus.bus().read_request(200, 100, 0, true));
        std::thread::sleep(Duration::from_millis(50));
        assert!(bus.bus().write_request(b"X", 100));
        assert_eq!(
            calls.writes.recv_timeout(WAIT),
            Ok(StreamIoStatus::StreamIoSuccess)
        );
        std::thread::sleep(Duration::from_millis(400));
    });
    assert_eq!(errors(&lines), Vec::<&str>::new());
    let polls = book
        .lock()
        .unwrap()
        .read_sizes
        .iter()
        .filter(|&&n| n == 63)
        .count();
    assert_eq!(polls, 1);
}

/// asynDisconnected marks the device disconnected (:1062), and a read on a
/// disconnected device ends with what it got (:903).
#[test]
fn asyn_disconnected_in_a_read_ends_the_next_one() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (_handle, book) = script("RG");
    let script_reads: [ScriptRead; 2] = [
        Err((Disconnected, "gone", b"")),
        Ok((b"a", EomReason::empty())),
    ];
    book.lock().unwrap().reads.extend(script_reads);
    let (bus, calls, _client) = locked("RG", None);
    assert!(bus.bus().read_request(100, 100, 0, false));
    assert_eq!(
        calls.disconnects.recv_timeout(WAIT),
        Ok(StreamIoStatus::StreamIoSuccess)
    );
    assert!(bus.bus().read_request(100, 100, 0, false));
    assert_eq!(reads_until_end(&calls), [read(End, b"a")]);
}

/// "rec" asked supportsEvent: its answer and lines, and what it is on.
struct Evented {
    supported: bool,
    lines: Vec<String>,
    bus: bus::BusInterface,
    calls: Calls,
    handle: asyn_rs::port_handle::PortHandle,
    /// Bind it: `..` drops it with the struct, and with it the record.
    client: Arc<dyn Client>,
}

/// "rec" attached to a connected port `name` with `caps`, interrupting on
/// `interrupts`, asked supportsEvent.
fn evented(name: &str, caps: Vec<Capability>, interrupts: InterfaceMask) -> Evented {
    let handle = port_interrupting(name, caps, true, interrupts);
    handle
        .submit_blocking(RequestOp::Connect, AsynUser::new(0))
        .unwrap();
    let (client, calls) = rec_calls("rec", None);
    let (bus, _) = attach(&client, name);
    let bus = bus.unwrap();
    let (supported, lines) = captured(|| bus.bus().supports_event());
    Evented {
        supported,
        lines,
        bus,
        calls,
        handle,
        client,
    }
}

/// What asynGpib's SRQ poll fires (asynGpib.c:293-295): `value` on reason
/// ASYN_REASON_SIGNAL for address 0.
fn signal(handle: &asyn_rs::port_handle::PortHandle, value: ParamValue) {
    let iface = match value {
        ParamValue::UInt32Digital(_) => InterfaceType::UInt32Digital,
        _ => InterfaceType::Int32,
    };
    handle.interrupts().notify(InterruptValue {
        reason: -1i32 as usize,
        addr: 0,
        value,
        uint32_changed_mask: u32::MAX,
        iface: Some(iface),
        ..Default::default()
    });
}

use asyn_rs::interfaces::InterfaceType;
use asyn_rs::interrupt::InterruptValue;
use asyn_rs::param::ParamValue;

fn int32_caps() -> Vec<Capability> {
    let mut caps = octet_transport_capabilities();
    caps.push(Capability::Int32Read);
    caps
}

/// supportsEvent registers for the signal on asynInt32 (:343-363), and an
/// event in the mask ends acceptEvent (:1217-1248).
#[test]
fn an_event_in_the_mask_is_accepted() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let Evented {
        supported,
        bus,
        calls,
        handle,
        client: _client,
        ..
    } = evented(
        "EVA",
        int32_caps(),
        InterfaceMask::OCTET | InterfaceMask::INT32,
    );
    assert!(supported);
    assert!(bus.bus().supports_event());
    assert!(bus.bus().accept_event(4, 0));
    let ((), lines) = captured(|| {
        signal(&handle, ParamValue::Int32(2));
        assert!(calls.events.try_recv().is_err());
        signal(&handle, ParamValue::Int32(-1));
        assert_eq!(calls.events.try_recv(), Ok(StreamIoStatus::StreamIoSuccess));
        signal(&handle, ParamValue::Int32(4));
    });
    assert_eq!(
        lines,
        [
            "AsynDriverInterface::intrCallbackInt32 (rec, 2)",
            "AsynDriverInterface::intrCallbackInt32 (rec, -1)",
            "AsynDriverInterface::intrCallbackInt32 (rec, 4)",
        ]
    );
    assert!(calls.events.try_recv().is_err());
}

/// An event before acceptEvent is kept and ends it at once (:1220-1226,
/// :1245-1246); another reason or address is no event.
#[test]
fn an_early_event_is_kept_for_accept_event() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let Evented {
        bus,
        calls,
        handle,
        client: _client,
        ..
    } = evented(
        "EVE",
        int32_caps(),
        InterfaceMask::OCTET | InterfaceMask::INT32,
    );
    signal(&handle, ParamValue::Int32(4));
    handle.interrupts().notify(InterruptValue {
        reason: 0,
        value: ParamValue::Int32(8),
        iface: Some(InterfaceType::Int32),
        ..Default::default()
    });
    handle.interrupts().notify(InterruptValue {
        reason: -1i32 as usize,
        addr: 1,
        value: ParamValue::Int32(8),
        iface: Some(InterfaceType::Int32),
        ..Default::default()
    });
    assert!(bus.bus().accept_event(8, 0));
    assert!(calls.events.try_recv().is_err());
    assert!(bus.bus().accept_event(4, 0));
    assert_eq!(calls.events.try_recv(), Ok(StreamIoStatus::StreamIoSuccess));
    assert!(bus.bus().accept_event(4, 0));
    assert!(calls.events.try_recv().is_err());
}

/// No event within the reply timeout (:1228, :1297-1301).
#[test]
fn an_event_not_coming_times_out() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let Evented {
        bus,
        calls,
        client: _client,
        ..
    } = evented(
        "EVT",
        int32_caps(),
        InterfaceMask::OCTET | InterfaceMask::INT32,
    );
    assert!(bus.bus().accept_event(4, 50));
    assert_eq!(
        calls.events.recv_timeout(WAIT),
        Ok(StreamIoStatus::StreamIoTimeout)
    );
}

/// Without asynInt32, asynUInt32Digital (:365-379).
#[test]
fn events_come_on_uint32_digital_without_int32() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let mut caps = octet_transport_capabilities();
    caps.push(Capability::UInt32DigitalRead);
    let Evented {
        supported,
        bus,
        calls,
        handle,
        client: _client,
        ..
    } = evented(
        "EVU",
        caps,
        InterfaceMask::OCTET | InterfaceMask::UINT32_DIGITAL,
    );
    assert!(supported);
    assert!(bus.bus().accept_event(1, 0));
    let ((), lines) = captured(|| signal(&handle, ParamValue::UInt32Digital(0x8000_0001)));
    assert_eq!(
        lines,
        ["AsynDriverInterface::intrCallbackUInt32 (rec, 2147483649)"]
    );
    assert_eq!(calls.events.try_recv(), Ok(StreamIoStatus::StreamIoSuccess));
}

/// A port with no event interrupts refuses (:380-385), naming asyn's
/// reason when it has the interface but not its interrupts.
#[test]
fn a_port_without_event_interrupts_refuses_events() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let Evented {
        supported,
        lines,
        client: _client,
        ..
    } = evented("EVN", octet_transport_capabilities(), InterfaceMask::OCTET);
    assert!(!supported);
    assert_eq!(
        lines,
        ["rec: port EVN 0 does not allow to register for Int32 or UInt32 interrupts: "]
    );
    let Evented {
        supported,
        lines,
        client: _client,
        ..
    } = evented("EVM", int32_caps(), InterfaceMask::OCTET);
    assert!(!supported);
    assert_eq!(
        lines,
        [
            "rec: port EVM 0 does not allow to register for Int32 or UInt32 interrupts: \
             asynManager:getInterruptPvt Driver does not support interrupts on interface \
             asynInt32"
        ]
    );
}

/// connectRequest, connectHandler, disconnectRequest, disconnectHandler
/// and finish (AsynDriverInterface.cc:1351-1426): a protocol's
/// `disconnect; connect 1000;`.
#[test]
fn connect_and_disconnect_print_what_c_prints() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let _handle = ip_port("CD", &device(b"", &[(b"X", b"1\r")]));
    let (bus, calls, _client) = locked("CD", None);
    let ((), lines) = all_threads(|| {
        assert!(bus.bus().disconnect_request());
        assert_eq!(calls.disconnects.recv_timeout(WAIT), Ok(Success));
        assert!(bus.bus().connect_request(1000));
        assert_eq!(calls.connects.recv_timeout(WAIT), Ok(Success));
        bus.bus().finish();
    });
    // C's port is P.
    let expected: Vec<String> = oracle("connect")
        .iter()
        .map(|l| l.replace(" P 0 ", " CD 0 "))
        .collect();
    for f in [
        "disconnectRequest",
        "disconnectHandler",
        "connectRequest",
        "connectToAsynPort",
        "exceptionHandler",
        "finish",
    ] {
        assert_eq!(of(&lines, &[f]), of(&expected, &[f]), "{f}");
    }
    assert_eq!(errors(&lines), [""; 0]);
}

/// A device already disconnected is not disconnected again (:1396).
#[test]
fn disconnecting_a_disconnected_device_only_calls_back() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (handle, book) = script("DA");
    let (client, calls) = rec_calls("rec", None);
    let (bus, _) = attach(&client, "DA");
    let bus = bus.unwrap();
    disconnect_for_good(&handle);
    // A disconnect reaching the port would now fault.
    book.lock().unwrap().disconnect_fails = Some("disconnected twice");
    let ((), lines) = all_threads(|| {
        assert!(bus.bus().disconnect_request());
        assert_eq!(calls.disconnects.recv_timeout(WAIT), Ok(Success));
    });
    assert_eq!(
        of(&lines, &["disconnectHandler"]),
        ["AsynDriverInterface::disconnectHandler rec is already disconnected"]
    );
}

/// A disconnect the port refuses faults, the device still connected
/// (:1398-1404).
#[test]
fn a_refused_disconnect_faults() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (_handle, book) = script("DF");
    book.lock().unwrap().disconnect_fails = Some("no");
    let (client, calls) = rec_calls("rec", None);
    let (bus, _) = attach(&client, "DF");
    let bus = bus.unwrap();
    let ((), lines) = all_threads(|| {
        assert!(bus.bus().disconnect_request());
        assert_eq!(calls.disconnects.recv_timeout(WAIT), Ok(Fault));
        book.lock().unwrap().disconnect_fails = None;
        assert!(bus.bus().disconnect_request());
        assert_eq!(calls.disconnects.recv_timeout(WAIT), Ok(Success));
    });
    assert_eq!(
        errors(&lines),
        ["rec connectRequest: pasynCommon->disconnect() failed: no"]
    );
    assert_eq!(
        of(&lines, &["disconnectHandler"]),
        ["AsynDriverInterface::disconnectHandler rec is not yet disconnected"; 2]
    );
}

/// A connect the port cannot make faults (:1369-1372).
#[test]
fn a_failed_connect_faults() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (handle, book) = script("CF");
    disconnect_for_good(&handle);
    book.lock().unwrap().connect_fails = Some("refused");
    let (client, calls) = rec_calls("rec", None);
    let (bus, _) = attach(&client, "CF");
    let bus = bus.unwrap();
    assert!(bus.bus().connect_request(1000));
    assert_eq!(calls.connects.recv_timeout(WAIT), Ok(Fault));
}

/// A connect still queued when its timeout passes times out (:1480). The
/// connect queue goes ahead of a block (asynManager.c:812), so what keeps
/// it queued is the port thread busy with another user's read.
#[test]
fn a_connect_left_queued_times_out() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let _handle = ip_port("CQ", &device(b"", &[(b"Q", b"")]));
    let (holder, holder_calls, _holder_client) = locked("CQ", None);
    assert!(holder.bus().read_request(1000, 100, 0, false));
    std::thread::sleep(Duration::from_millis(100));
    let (client, calls) = rec_calls("second", None);
    let (bus, _) = attach(&client, "CQ");
    let bus = bus.unwrap();
    assert!(bus.bus().connect_request(50));
    assert_eq!(calls.connects.recv_timeout(WAIT), Ok(IoTimeout));
    assert_eq!(reads_until_end(&holder_calls), [read(NoReply, b"")]);
}

/// finish leaves no request to act on: one still queued is ignored when
/// it runs (:1413-1426, :1436-1439).
#[test]
fn finish_drops_a_request_left_queued() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (_handle, _book) = script("FQ");
    let (holder, _, _holder_client) = locked("FQ", None);
    let (client, calls) = rec_calls("second", None);
    let (bus, _) = attach(&client, "FQ");
    let bus = bus.unwrap();
    assert!(bus.bus().read_request(1000, 100, 0, false));
    bus.bus().finish();
    assert!(holder.bus().unlock());
    assert!(
        calls
            .reads
            .recv_timeout(Duration::from_millis(300))
            .is_err()
    );
}

/// "rec", its protocol's in terminator `in_terminator`, on a [`script`] port
/// `name` that hands its reads to octet interrupt users, asked
/// supportsAsyncRead.
fn intr_rec(
    name: &str,
    in_terminator: Option<&[u8]>,
) -> (
    asyn_rs::port_handle::PortHandle,
    Arc<Mutex<Book>>,
    bus::BusInterface,
    Calls,
    Arc<dyn Client>,
) {
    let (handle, book) = script_with(name, true);
    let (client, calls) = rec_terminated("rec", in_terminator, None);
    let (bus, _) = attach(&client, name);
    let bus = bus.unwrap();
    assert!(bus.bus().supports_async_read());
    (handle, book, bus, calls, client)
}

/// Another client of the port reads `data`, ending with `eom`: what reaches
/// the interrupt users of asynOctetBase (asynOctetBase.c:224-238).
fn input(
    handle: &asyn_rs::port_handle::PortHandle,
    book: &Mutex<Book>,
    data: &'static [u8],
    eom: EomReason,
) {
    book.lock().unwrap().reads.push_back(Ok((data, eom)));
    // What the reader itself gets is the EOS layer's business.
    let _ = handle.submit_octet_read_blocking(
        OctetReadOp::Read { buf_size: 64 },
        AsynUser::new(0).with_timeout(Duration::from_millis(100)),
    );
}

/// The readCallbacks the last input brought: the interrupt user runs inside
/// the read, so they are all in by the time it returns.
fn reads_now(calls: &Calls) -> Vec<(StreamIoStatus, OwnedReadInput, isize)> {
    calls.reads.try_iter().collect()
}

/// A readCallback's status, input and answer.
type ReadCall = (StreamIoStatus, OwnedReadInput, isize);

/// [`reads_now`] for input whose last readCallback asks for more, and the
/// timeout of the read timer that starts (:1204-1209, :1302-1306): with
/// no read request the read timeout is 0, so it may be in already.
fn reads_then_timeout(calls: &Calls) -> (Vec<ReadCall>, ReadCall) {
    let mut reads = reads_now(calls);
    let timeout = match reads.last() {
        Some((IoTimeout, ..)) => reads.pop().unwrap(),
        _ => calls.reads.recv_timeout(WAIT).unwrap(),
    };
    (reads, timeout)
}

/// supportsAsyncRead, intrCallbackOctet and asynReadHandler
/// (AsynDriverInterface.cc:388-402, :1101-1212): an "I/O Intr" record's
/// poll reads the device's greeting, which comes back to it as an interrupt.
#[test]
fn async_input_prints_what_c_prints() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let _handle = ip_port("IR", &device(b"1.5\r", &[]));
    let (client, calls) = rec_terminated("rec", Some(b"\r"), None);
    let (bus, _) = attach(&client, "IR");
    let bus = bus.unwrap();
    let ((), lines) = all_threads_through(
        "AsynDriverInterface::asynReadHandler(rec) readMore=",
        || {
            assert!(bus.bus().supports_async_read());
            assert!(bus.bus().read_request(1000, 100, 0, true));
            assert_eq!(reads_until_end(&calls), [read(Success, b"1.5\r")]);
        },
    );
    assert_eq!(of(&lines, &["asynReadHandler"]), oracle("intr"));
    assert_eq!(errors(&lines), Vec::<&str>::new());
}

/// Input before iocInit has set interruptAccept is dropped
/// (AsynDriverInterface.cc:1110).
#[test]
fn input_before_interrupt_accept_is_dropped() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (handle, book, _bus, calls, _client) = intr_rec("IA", Some(b"\r"));
    calls.accept.store(false, Ordering::Relaxed);
    input(&handle, &book, b"1\r", EomReason::empty());
    assert_eq!(reads_now(&calls), []);
    calls.accept.store(true, Ordering::Relaxed);
    input(&handle, &book, b"2\r", EomReason::empty());
    assert_eq!(
        reads_now(&calls),
        [(Success, OwnedReadInput::Bytes(b"2\r".to_vec()), 0)]
    );
}

/// A read that ends with END ends the input (:1199-1202).
#[test]
fn an_end_read_ends_the_input() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (handle, book, _bus, calls, _client) = intr_rec("IE", Some(b"\r"));
    input(&handle, &book, b"12", EomReason::END);
    assert_eq!(
        reads_now(&calls),
        [(End, OwnedReadInput::Bytes(b"12".to_vec()), 0)]
    );
}

/// A terminator asyn has cut off is given after the input when the protocol
/// has one (:1155-1170), and ends it when it has none (:1150-1154).
#[test]
fn a_cut_terminator_is_given_back_or_ends_the_input() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (handle, book, _bus, calls, _client) = intr_rec("IC", Some(b"\r\n"));
    handle
        .set_input_eos_blocking(AsynUser::new(0), b"\r\n")
        .unwrap();
    input(&handle, &book, b"12", EomReason::EOS);
    assert_eq!(
        reads_now(&calls),
        [
            (Success, OwnedReadInput::Bytes(b"12".to_vec()), -1),
            (Success, OwnedReadInput::Bytes(b"\r\n".to_vec()), 0)
        ]
    );
    let (handle, book, _bus, calls, _client) = intr_rec("ID", None);
    input(&handle, &book, b"12", EomReason::EOS);
    assert_eq!(
        reads_now(&calls),
        [(End, OwnedReadInput::Bytes(b"12".to_vec()), 0)]
    );
}

/// Without a protocol terminator, the device's is cut off the input and
/// ends it (:1172-1194).
#[test]
fn a_device_terminator_is_cut_off_without_a_protocol_one() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (handle, book, _bus, calls, _client) = intr_rec("IT", None);
    handle
        .set_input_eos_blocking(AsynUser::new(0), b"\r\n")
        .unwrap();
    input(&handle, &book, b"12\r\n", EomReason::empty());
    assert_eq!(
        reads_now(&calls),
        [(End, OwnedReadInput::Bytes(b"12".to_vec()), 0)]
    );
    input(&handle, &book, b"3\n", EomReason::empty());
    assert_eq!(
        reads_then_timeout(&calls),
        (
            vec![(Success, OwnedReadInput::Bytes(b"3\n".to_vec()), -1)],
            (IoTimeout, OwnedReadInput::Bytes(Vec::new()), 0)
        )
    );
}

/// Input that wants more waits the read timeout for it, then times out
/// (:1204-1209, :1302-1306).
#[test]
fn input_wanting_more_times_out() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (handle, book, _bus, calls, _client) = intr_rec("IM", Some(b"\r"));
    let ((), lines) = all_threads(|| {
        input(&handle, &book, b"12", EomReason::empty());
        assert_eq!(
            reads_then_timeout(&calls),
            (
                vec![(Success, OwnedReadInput::Bytes(b"12".to_vec()), -1)],
                (IoTimeout, OwnedReadInput::Bytes(Vec::new()), 0)
            )
        );
        // the timeout ends the read: nothing follows it
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(reads_now(&calls), []);
    });
    assert_eq!(
        of(&lines, &["asynReadHandler"]),
        [
            "AsynDriverInterface::asynReadHandler(rec, buffer=\"12\", received=2 \
             eomReason=NONE) ioAction=None",
            "AsynDriverInterface::asynReadHandler(rec) readMore=-1, ioAction=AsyncReadMore ",
        ]
    );
}

/// Input makes the poll it crossed obsolete (:1135, :1296-1298): once it
/// has ended the read, the poll timer queues no further polls.
#[test]
fn input_ends_the_polling() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (handle, book, bus, calls, _client) = intr_rec("IP", Some(b"\r"));
    assert!(bus.bus().read_request(50, 100, 0, true));
    std::thread::sleep(Duration::from_millis(120));
    input(&handle, &book, b"1\r", EomReason::empty());
    assert_eq!(
        reads_now(&calls),
        [(Success, OwnedReadInput::Bytes(b"1\r".to_vec()), 0)]
    );
    let polls = book.lock().unwrap().read_sizes.len();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(book.lock().unwrap().read_sizes.len(), polls);
}

/// Dropping the interface cancels its interrupt user (:299-307).
#[test]
fn a_dropped_interface_takes_no_input() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let (handle, book, bus, calls, _client) = intr_rec("IX", Some(b"\r"));
    drop(bus);
    input(&handle, &book, b"1\r", EomReason::empty());
    assert_eq!(reads_now(&calls), []);
}

/// A port without octet interrupts refuses asynchronous input (:393-399).
#[test]
fn a_port_without_octet_interrupts_refuses_async_input() {
    let _switches = SWITCHES.lock().unwrap_or_else(|e| e.into_inner());
    stream_rs::asyn::register();
    let handle = port_interrupting(
        "IN",
        octet_transport_capabilities(),
        true,
        InterfaceMask::NONE,
    );
    handle
        .submit_blocking(RequestOp::Connect, AsynUser::new(0))
        .unwrap();
    let (client, _calls) = rec_calls("rec", None);
    let (bus, _) = attach(&client, "IN");
    let bus = bus.unwrap();
    let (supported, lines) = captured(|| bus.bus().supports_async_read());
    assert!(!supported);
    assert_eq!(
        lines,
        ["rec: asyn port IN 0 does not support asynchronous input: \
             asynManager:getInterruptPvt Driver does not support interrupts on interface \
             asynOctet"]
    );
}
