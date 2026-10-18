//! `AsynDriverInterface.cc`: the bus to an asyn port.
//!
//! C's one `asynUser` per client is a [`DeviceUser`] bound to the port, and
//! the port's announcements reach `Inner::exception_handler` through the
//! guard [`DeviceUser::exception_callback_add`] returns.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Once, OnceLock, Weak};
use std::time::Duration;

use asyn_rs::error::{AsynError, AsynStatus};
use asyn_rs::exception::{AsynException, ExceptionCallbackGuard};
use asyn_rs::interfaces::InterfaceType;
use asyn_rs::interpose::EomReason;
use asyn_rs::interrupt::{InterruptFilter, InterruptValue, SyncCallbackSubscription};
use asyn_rs::manager::DeviceUser;
use asyn_rs::param::ParamValue;
use asyn_rs::port::QueuePriority;
use asyn_rs::port_handle::PortHandle;
use asyn_rs::request::{CellBody, PortCalls, RequestCell, RequestOp, Unrun};
use asyn_rs::user::AsynUser;
use epics_base_rs::runtime::task::{CallbackPriority, sleep_background, spawn_background};
use epics_base_rs::runtime::time::thread_sleep_quantum;
use parking_lot::Mutex;

use crate::buffer::StreamBuffer;
use crate::bus::{self, Client, ReadInput, StreamBusInterface, StreamIoStatus};
use crate::error::stream_error;
use crate::{cmsg, debug, debug2};

/// `ENUM (IoAction, ...)`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IoAction {
    None,
    Lock,
    Write,
    Read,
    AsyncRead,
    AsyncReadMore,
    ReceiveEvent,
    Connect,
    Disconnect,
}

impl IoAction {
    fn to_str(self) -> &'static str {
        match self {
            IoAction::None => "None",
            IoAction::Lock => "Lock",
            IoAction::Write => "Write",
            IoAction::Read => "Read",
            IoAction::AsyncRead => "AsyncRead",
            IoAction::AsyncReadMore => "AsyncReadMore",
            IoAction::ReceiveEvent => "ReceiveEvent",
            IoAction::Connect => "Connect",
            IoAction::Disconnect => "Disconnect",
        }
    }
}

/// `toStr(asynStatus)`
fn status_str(status: AsynStatus) -> &'static str {
    status.str_status()
}

/// C's `%g` of a timeout in seconds.
fn fmt_g(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    // Six significant digits, rounded once, decide the form as printf does.
    let e = format!("{v:.5e}");
    let (mantissa, exp) = e.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let trim = |s: String| {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    };
    if !(-4..6).contains(&exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", trim(mantissa.to_string()), exp.abs())
    } else {
        trim(format!("{v:.prec$}", prec = (5 - exp) as usize))
    }
}

/// `ASYN_REASON_SIGNAL` (asynDriver.h:113), C's -1 as asyn-rs's reason.
const ASYN_REASON_SIGNAL: usize = -1i32 as usize;

/// `eomReasonToStr(eomReason)`
fn eom_reason_str(eom: EomReason) -> &'static str {
    [
        "NONE",
        "CNT",
        "EOS",
        "CNT+EOS",
        "END",
        "CNT+END",
        "EOS+END",
        "CNT+EOS+END",
    ][(eom.bits() & 7) as usize]
}

/// `ms*0.001`, as C keeps a timeout.
fn secs(ms: u64) -> f64 {
    ms as f64 * 0.001
}

/// `toStr(asynException)`
fn exception_str(exception: AsynException) -> &'static str {
    match exception {
        AsynException::Connect => "Connect",
        AsynException::Enable => "Enable",
        AsynException::AutoConnect => "AutoConnect",
        AsynException::TraceMask => "TraceMask",
        AsynException::TraceIoMask => "TraceIOMask",
        AsynException::TraceInfoMask => "TraceInfoMask",
        AsynException::TraceFile => "TraceFile",
        AsynException::TraceIoTruncateSize => "TraceIOTruncateSize",
        AsynException::Shutdown => "unknown",
    }
}

/// What the C class keeps in its members and changes from the port thread
/// and the client's: never held across a call into the port or the client.
struct State {
    connected: bool,
    io_action: IoAction,
    /// `pasynGpib != NULL`
    gpib: bool,
    peeksize: usize,
    previous_asyn_status: AsynStatus,
    /// `lockTimeout`, in msec: 0 is C's -1.0, no queue timeout.
    lock_timeout_ms: u64,
    /// `writeTimeout`, in msec.
    write_timeout_ms: u64,
    /// `outputBuffer`/`outputSize`: what is still to be written.
    output: Vec<u8>,
    /// `replyTimeout`, `readTimeout`, in msec.
    reply_timeout_ms: u64,
    read_timeout_ms: u64,
    expected_length: isize,
    /// `inputBuffer`: only its capacity is used, as the size of a read.
    input_buffer: StreamBuffer,
    event_mask: u64,
    received_event: u64,
}

/// The port [`Inner::connect_to_bus`] bound, and C's `pasynUser` on it as a
/// request: one cell, queued for every `queueRequest` C makes.
struct Bound {
    port: PortHandle,
    cell: Arc<RequestCell>,
    addr: i32,
}

/// C's `handleRequest`/`handleTimeout` wrappers, the `pasynUser`'s
/// `processUser`/`timeoutUser`. They reach the interface through its
/// `userPvt`, held weakly: the cell belongs to the interface.
struct Handler(Weak<Inner>);

impl CellBody for Handler {
    fn process(&self, port: &mut dyn PortCalls) {
        if let Some(me) = self.0.upgrade() {
            me.handle_request(port);
        }
    }

    /// C's `cancelRequest` calls neither callback; a queue timeout and a
    /// request the port refuses at dequeue (a disconnected device,
    /// asynManager.c:886) call `timeoutUser`.
    fn unrun(&self, why: Unrun) {
        if matches!(why, Unrun::Canceled) {
            return;
        }
        if let Some(me) = self.0.upgrade() {
            me.handle_timeout();
        }
    }

    fn done(self: Arc<Self>) {}
}

struct Inner {
    client: Weak<dyn Client>,
    /// `clientName()`, read once: the client's own `name()` takes its lock,
    /// which a port-thread callback must not wait for.
    client_name: String,
    /// `name()`: the name [`bus::find`] gives this bus.
    name: String,
    user: Mutex<DeviceUser>,
    /// Set by [`Inner::connect_to_bus`].
    bound: OnceLock<Bound>,
    exception: Mutex<Option<ExceptionCallbackGuard>>,
    state: Mutex<State>,
    /// `intrPvtInt32` or `intrPvtUInt32`: the event interrupt registered by
    /// [`Inner::supports_event`], canceled when dropped.
    events: Mutex<Option<SyncCallbackSubscription>>,
    /// `intrPvtOctet`: the input interrupt registered by
    /// [`Inner::supports_async_read`], canceled when dropped.
    octet: Mutex<Option<SyncCallbackSubscription>>,
    /// The `timer`: a start or a cancel moves it on, and a sleeping timer
    /// task expires only if it still finds the value it was started with.
    timer: AtomicU64,
}

/// The [`StreamBusInterface`] [`get_bus_interface`] hands out.
pub struct AsynDriverInterface(Arc<Inner>);

impl Inner {
    /// `AsynDriverInterface(Client*)`
    fn new(client: Weak<dyn Client>, client_name: String, name: String) -> Self {
        debug!(format!("AsynDriverInterface({client_name})\n"));
        debug!(format!(
            "AsynDriverInterface({client_name}) createAsynUser\n"
        ));
        debug!(format!(
            "AsynDriverInterface({client_name}) epicsTimerQueueActive::allocate(true)\n"
        ));
        debug!(format!(
            "AsynDriverInterface({client_name}) timerQueue->createTimer()\n"
        ));
        debug!(format!("AsynDriverInterface({client_name}) done\n"));
        Self {
            client,
            client_name,
            name,
            user: Mutex::new(DeviceUser::default()),
            bound: OnceLock::new(),
            exception: Mutex::new(None),
            state: Mutex::new(State {
                connected: false,
                io_action: IoAction::None,
                gpib: false,
                peeksize: 1,
                previous_asyn_status: AsynStatus::Success,
                lock_timeout_ms: 0,
                write_timeout_ms: 0,
                output: Vec::new(),
                reply_timeout_ms: 0,
                read_timeout_ms: 0,
                expected_length: 0,
                input_buffer: StreamBuffer::new(),
                event_mask: 0,
                received_event: 0,
            }),
            events: Mutex::new(None),
            octet: Mutex::new(None),
            timer: AtomicU64::new(0),
        }
    }

    fn client(&self) -> Option<Arc<dyn Client>> {
        self.client.upgrade()
    }

    /// `connectToBus(portname, addr)`
    fn connect_to_bus(self: &Arc<Self>, portname: &str, addr: i32) -> bool {
        let mut user = self.user.lock();
        let status = user.connect_device(Some(portname), addr);
        debug!(format!(
            "{}: AsynDriverInterface::connectToBus({portname}, {addr}): \
             pasynManager->connectDevice({:p}, {portname}, {addr}) = {}\n",
            self.client_name,
            &*user as *const DeviceUser,
            if status.is_ok() {
                "asynSuccess"
            } else {
                "asynError"
            }
        ));
        let Ok(entry) = status else {
            // asynDriver does not know this portname/address
            return false;
        };
        let handle = entry.handle;

        // find the asynCommon interface
        if user.find_common().is_err() {
            stream_error(&cmsg!(
                self.client_name,
                ": asyn port ",
                portname,
                " does not support asynCommon interface\n"
            ));
            return false;
        }
        // The port thread's exception callback takes this lock.
        drop(user);

        // find the asynOctet interface
        if !handle.has_interface(InterfaceType::Octet) {
            stream_error(&cmsg!(
                self.client_name,
                ": asyn port ",
                portname,
                " does not support asynOctet interface\n"
            ));
            return false;
        }

        // Check if device knows EOS. C warns "No input EOS support." when
        // getInputEos fails; asyn-rs's getInputEos cannot fail.
        if self
            .client()
            .is_some_and(|c| c.get_in_terminator().is_some())
        {
            let _ =
                handle.submit_blocking(RequestOp::GetInputEos, AsynUser::new(0).with_addr(addr));
        }

        // is it a GPIB interface ?
        if handle.has_interface(InterfaceType::Gpib) {
            let mut state = self.state.lock();
            state.gpib = true;
            // asynGpib returns overflow error if we try to peek
            // (read only one byte first).
            state.peeksize = StreamBuffer::new().capacity();
        }
        let cell = handle.new_cell(Arc::new(Handler(Arc::downgrade(self))));
        let _ = self.bound.set(Bound {
            port: handle,
            cell,
            addr,
        });

        // Install callback for connect/disconnect events. The callback holds
        // the interface weakly: the guard it is kept in belongs to it.
        let weak = Arc::downgrade(self);
        let mut user = self.user.lock();
        *self.exception.lock() = user.exception_callback_add(move |event| {
            if let Some(me) = weak.upgrade() {
                me.exception_handler(event.exception);
            }
        });
        let connected = user.is_connected().unwrap_or(false);
        drop(user);
        self.state.lock().connected = connected;
        debug!(format!(
            "{}: AsynDriverInterface::connectToBus({portname}, {addr}): device is now {}\n",
            self.client_name,
            if connected {
                "connected"
            } else {
                "disconnected"
            }
        ));
        true
    }

    /// `StreamBusInterface::priority()`
    fn client_priority(&self) -> i64 {
        self.client().map_or(0, |c| c.priority())
    }

    /// `priority()`: the client's, as C casts it to `asynQueuePriority`.
    fn priority(&self) -> QueuePriority {
        match self.client_priority() {
            1 => QueuePriority::Medium,
            2 => QueuePriority::High,
            3 => QueuePriority::Connect,
            _ => QueuePriority::Low,
        }
    }

    /// `pasynManager->queueRequest(pasynUser, priority, timeout)`; a zero
    /// `timeout` arms no queue timer, as C's does not.
    fn queue_request(&self, priority: QueuePriority, timeout: Duration) -> Result<(), AsynError> {
        let bound = self.bound.get().expect("a live interface is bound");
        let mut user = AsynUser::new(0)
            .with_addr(bound.addr)
            .with_priority(priority);
        if !timeout.is_zero() {
            user = user.with_queue_timeout(timeout);
        }
        bound.port.queue_request(&bound.cell, &user).map(|_| ())
    }

    /// `reportAsynStatus(status, name)`: each change of status once.
    fn report_asyn_status(&self, status: &Result<(), AsynError>, name: &str) {
        let now = status
            .as_ref()
            .map_or_else(AsynError::status, |_| AsynStatus::Success);
        {
            let mut state = self.state.lock();
            if state.previous_asyn_status == now {
                return;
            }
            state.previous_asyn_status = now;
        }
        match status {
            Ok(()) => stream_error(&cmsg!(
                self.client_name,
                " ",
                name,
                ": status returned to normal\n"
            )),
            Err(e) => stream_error(&cmsg!(self.client_name, " ", name, ": ", e.message(), "\n")),
        }
    }

    /// `lockRequest(lockTimeout_ms)`: 0 waits for ever (`@init`).
    fn lock_request(&self, lock_timeout_ms: u64) -> bool {
        debug!(format!(
            "AsynDriverInterface::lockRequest({}, {lock_timeout_ms} msec)\n",
            self.client_name
        ));
        {
            let mut state = self.state.lock();
            state.lock_timeout_ms = lock_timeout_ms;
            state.io_action = IoAction::Lock;
        }
        let status = self.queue_request(self.priority(), Duration::from_millis(lock_timeout_ms));
        self.report_asyn_status(&status, "lockRequest");
        if status.is_err() {
            self.state.lock().io_action = IoAction::None;
            return false;
        }
        true
        // continues with:
        //    handleRequest() -> lockHandler() -> lockCallback()
        // or handleTimeout() -> lockCallback(StreamIoTimeout)
    }

    /// `lockHandler()`: now, we can have exclusive access (called by
    /// asynManager).
    fn lock_handler(&self) {
        debug!(format!(
            "AsynDriverInterface::lockHandler({})\n",
            self.client_name
        ));
        let bound = self.bound.get().expect("a live interface is bound");
        let status = bound.cell.block_process_callback(false);
        let Some(client) = self.client() else {
            return;
        };
        if let Err(e) = status {
            stream_error(&cmsg!(
                self.client_name,
                " lockHandler: pasynManager->blockProcessCallback() failed: ",
                e.message(),
                "\n"
            ));
            client.lock_callback(StreamIoStatus::StreamIoFault);
            return;
        }
        client.lock_callback(StreamIoStatus::StreamIoSuccess);
    }

    /// `unlock()`: we don't need exclusive access any more.
    fn unlock(&self) -> bool {
        debug!(format!(
            "AsynDriverInterface::unlock({})\n",
            self.client_name
        ));
        let bound = self.bound.get().expect("a live interface is bound");
        if let Err(e) = bound.cell.unblock_process_callback(false) {
            stream_error(&cmsg!(
                self.client_name,
                " unlock: pasynManager->unblockProcessCallback() failed: ",
                e.message(),
                "\n"
            ));
            return false;
        }
        true
    }

    /// `connectToAsynPort()`, on the port thread.
    fn connect_to_asyn_port(&self, port: &mut dyn PortCalls) -> bool {
        debug!(format!(
            "AsynDriverInterface::connectToAsynPort({})\n",
            self.client_name
        ));
        if !self.state.lock().connected {
            let status = port
                .call(&RequestOp::Connect, self.user_on_port())
                .map(|_| ());
            self.report_asyn_status(&status, "connectToAsynPort");
            if status.is_err() {
                return false;
            }
            self.state.lock().connected = true;
        }
        true
    }

    /// `connectRequest(connecttimeout_ms)`
    fn connect_request(&self, connecttimeout_ms: u64) -> bool {
        self.state.lock().io_action = IoAction::Connect;
        debug!(format!(
            "AsynDriverInterface::connectRequest {}\n",
            self.client_name
        ));
        let status = self.queue_request(
            QueuePriority::Connect,
            Duration::from_millis(connecttimeout_ms),
        );
        self.report_asyn_status(&status, "connectRequest");
        status.is_ok()
        // continues with:
        //    handleRequest() -> connectHandler() -> connectCallback()
        // or handleTimeout() -> connectCallback(StreamIoTimeout)
    }

    /// `connectHandler()`
    fn connect_handler(&self, port: &mut dyn PortCalls) {
        let status = if self.connect_to_asyn_port(port) {
            StreamIoStatus::StreamIoSuccess
        } else {
            StreamIoStatus::StreamIoFault
        };
        if let Some(client) = self.client() {
            client.connect_callback(status);
        }
    }

    /// `disconnectRequest()`
    fn disconnect_request(&self) -> bool {
        self.state.lock().io_action = IoAction::Disconnect;
        debug!(format!(
            "AsynDriverInterface::disconnectRequest {}\n",
            self.client_name
        ));
        let status = self.queue_request(QueuePriority::Connect, Duration::ZERO);
        self.report_asyn_status(&status, "disconnectRequest");
        status.is_ok()
        // continues with:
        //    handleRequest() -> disconnectHandler()
    }

    /// `disconnectHandler()`
    fn disconnect_handler(&self, port: &mut dyn PortCalls) {
        let connected = self.state.lock().connected;
        debug!(format!(
            "AsynDriverInterface::disconnectHandler {} is {} disconnected\n",
            self.client_name,
            if connected { "not yet" } else { "already" }
        ));
        let mut status = StreamIoStatus::StreamIoSuccess;
        if connected {
            match port.call(&RequestOp::Disconnect, self.user_on_port()) {
                // C's message names connectRequest.
                Err(e) => {
                    stream_error(&cmsg!(
                        self.client_name,
                        " connectRequest: pasynCommon->disconnect() failed: ",
                        e.message(),
                        "\n"
                    ));
                    status = StreamIoStatus::StreamIoFault;
                }
                Ok(_) => self.state.lock().connected = false,
            }
        }
        if let Some(client) = self.client() {
            client.disconnect_callback(status);
        }
    }

    /// `finish()`
    fn finish(&self) {
        debug!(format!(
            "AsynDriverInterface::finish({}) start\n",
            self.client_name
        ));
        self.cancel_timer();
        self.state.lock().io_action = IoAction::None;
        debug!(format!(
            "AsynDriverInterface::finish({}) done\n",
            self.client_name
        ));
    }

    /// The `pasynUser` of a call the port thread makes from a callback.
    fn user_on_port(&self) -> AsynUser {
        let bound = self.bound.get().expect("a live interface is bound");
        AsynUser::new(0).with_addr(bound.addr)
    }

    /// [`Self::user_on_port`] with `pasynUser->timeout` set.
    fn user_timed(&self, timeout: Duration) -> AsynUser {
        self.user_on_port().with_timeout(timeout)
    }

    /// `writeRequest(output, size, writeTimeout_ms)`: we want to write
    /// something.
    fn write_request(&self, output: &[u8], write_timeout_ms: u64) -> bool {
        debug!(
            "AsynDriverInterface::writeRequest(",
            self.client_name,
            ", \"",
            StreamBuffer::from_bytes(output).expand(),
            format!("\", {write_timeout_ms} msec)\n")
        );
        {
            let mut state = self.state.lock();
            state.output.clear();
            state.output.extend_from_slice(output);
            state.write_timeout_ms = write_timeout_ms;
            state.io_action = IoAction::Write;
        }
        let status = self.queue_request(self.priority(), Duration::from_millis(write_timeout_ms));
        self.report_asyn_status(&status, "writeRequest");
        status.is_ok()
        // continues with:
        //    handleRequest() -> writeHandler() -> writeCallback()
        // or handleTimeout() -> writeCallback(StreamIoTimeout)
    }

    /// `writeHandler()`: now, we can write (called by asynManager).
    fn write_handler(&self, port: &mut dyn PortCalls) {
        debug!(format!(
            "AsynDriverInterface::writeHandler({})\n",
            self.client_name
        ));
        let (gpib, write_timeout_ms, lock_timeout_ms, output) = {
            let mut state = self.state.lock();
            (
                state.gpib,
                state.write_timeout_ms,
                state.lock_timeout_ms,
                std::mem::take(&mut state.output),
            )
        };
        if !gpib {
            // discard any early input, but forward it to potential async
            // records thus do not use pasynOctet->flush()
            // unfortunately we cannot do this with GPIB because addressing a
            // device as talker when it has nothing to say is an error.
            loop {
                debug!(format!(
                    "AsynDriverInterface::writeHandler({}): reading old input\n",
                    self.client_name
                ));
                let read = port.call(
                    &RequestOp::OctetRead { buf_size: 256 },
                    self.user_timed(Duration::ZERO),
                );
                let (status, received) = match &read {
                    Ok(r) => (
                        AsynStatus::Success,
                        r.data
                            .as_deref()
                            .map_or(&[][..], |d| &d[..r.nbytes.min(d.len())]),
                    ),
                    Err(e) => (
                        e.status(),
                        e.partial_read().map_or(&[][..], |p| &p.data[..]),
                    ),
                };
                if status == AsynStatus::Error || received.is_empty() {
                    break;
                }
                debug!(
                    format!(
                        "AsynDriverInterface::writeHandler({}): flushing {} bytes: \"",
                        self.client_name,
                        received.len()
                    ),
                    StreamBuffer::from_bytes(received).expand(),
                    "\"\n"
                );
                if status != AsynStatus::Success {
                    break;
                }
            }
        } else {
            debug!(format!(
                "AsynDriverInterface::writeHandler({}): flushing old input\n",
                self.client_name
            ));
            let _ = port.call(&RequestOp::Flush, self.user_timed(Duration::ZERO));
        }

        // has stream already added a terminator or should asyn do so?
        let write_timeout = Duration::from_millis(write_timeout_ms);
        let mut old_eos = None;
        if self
            .client()
            .is_some_and(|c| c.get_out_terminator().is_some())
        {
            // stream has already added eos, don't do it again in asyn. C keeps
            // no old terminator when getOutputEos fails; asyn-rs's cannot.
            old_eos = port
                .call(&RequestOp::GetOutputEos, self.user_timed(write_timeout))
                .ok()
                .map(|r| r.data.unwrap_or_default());
            let _ = port.call(
                &RequestOp::SetOutputEos { eos: Vec::new() },
                self.user_timed(write_timeout),
            );
        }
        let mut write_try = 0;
        let (status, written, message) = loop {
            let (status, written, message) = match port.call(
                &RequestOp::OctetWrite {
                    data: output.clone(),
                },
                self.user_timed(write_timeout),
            ) {
                Ok(r) => (AsynStatus::Success, r.nbytes, String::new()),
                Err(e) => (e.status(), e.partial_write().unwrap_or(0), e.message()),
            };
            debug!(
                "AsynDriverInterface::writeHandler(",
                self.client_name,
                "): write(..., \"",
                StreamBuffer::from_bytes(&output).expand(),
                format!(
                    "\", outputSize={}, written={written}) [timeout={} sec] = {} {message}{}\n",
                    output.len(),
                    fmt_g(secs(write_timeout_ms)),
                    status_str(status),
                    match (status, write_try) {
                        (AsynStatus::Success, _) => "",
                        (_, 0) => " try to connect",
                        _ => " failed twice",
                    }
                )
            );
            // When devices goes offline, we get an error only at next write.
            // Maybe the device has re-connected meanwhile? Let's try once more.
            let again = status == AsynStatus::Error && write_try == 0;
            write_try += 1;
            if !(again && self.connect_to_asyn_port(port)) {
                break (status, written, message);
            }
        };
        if let Some(eos) = old_eos {
            // restore asyn terminator
            let _ = port.call(
                &RequestOp::SetOutputEos { eos },
                self.user_timed(write_timeout),
            );
        }

        let Some(client) = self.client() else {
            return;
        };
        let timeout = fmt_g(secs(write_timeout_ms));
        match status {
            AsynStatus::Success => {
                let rest = &output[written.min(output.len())..];
                if !rest.is_empty() {
                    self.state.lock().output = rest.to_vec();
                    let status =
                        self.queue_request(self.priority(), Duration::from_millis(lock_timeout_ms));
                    self.report_asyn_status(&status, "writeHandler");
                    if status.is_err() {
                        client.write_callback(StreamIoStatus::StreamIoFault);
                    }
                    // continues with:
                    //    handleRequest() -> writeHandler() -> writeCallback()
                    // or handleTimeout() -> writeCallback(StreamIoTimeout)
                    return;
                }
                client.write_callback(StreamIoStatus::StreamIoSuccess);
            }
            AsynStatus::Timeout => {
                stream_error(&cmsg!(
                    self.client_name,
                    ": asynTimeout (",
                    timeout,
                    " sec) in write: ",
                    message,
                    "\n"
                ));
                client.write_callback(StreamIoStatus::StreamIoTimeout);
            }
            AsynStatus::Overflow => {
                stream_error(&cmsg!(
                    self.client_name,
                    ": asynOverflow in write: ",
                    message,
                    "\n"
                ));
                client.write_callback(StreamIoStatus::StreamIoFault);
            }
            AsynStatus::Error => {
                if !self.state.lock().connected {
                    stream_error(&cmsg!(
                        self.client_name,
                        ": device ",
                        self.name,
                        " disconnected\n"
                    ));
                    client.disconnect_callback(StreamIoStatus::StreamIoSuccess);
                } else {
                    stream_error(&cmsg!(
                        self.client_name,
                        ": asynError in write: ",
                        message,
                        "\n"
                    ));
                    client.write_callback(StreamIoStatus::StreamIoFault);
                }
            }
            AsynStatus::Disconnected => {
                stream_error(&cmsg!(
                    self.client_name,
                    ": asynDisconnected in write: ",
                    message,
                    "\n"
                ));
                client.disconnect_callback(StreamIoStatus::StreamIoSuccess);
            }
            AsynStatus::Disabled => {
                stream_error(&cmsg!(
                    self.client_name,
                    ": asynDisabled in write: ",
                    message,
                    "\n"
                ));
                client.write_callback(StreamIoStatus::StreamIoFault);
            }
        }
    }

    /// `startTimer(timeout)`: [`Self::timer_expired`] in `timeout` seconds
    /// and half a sleep quantum, unless started again or canceled first.
    /// C's timer queue thread runs at `epicsThreadPriorityMin + 10`
    /// (epicsTimer.h:84), below every callback band but the lowest.
    fn start_timer(self: &Arc<Self>, timeout: f64) {
        let started = self.timer.fetch_add(1, Ordering::AcqRel) + 1;
        let me = Arc::downgrade(self);
        let delay = Duration::from_secs_f64(timeout + thread_sleep_quantum() * 0.5);
        drop(spawn_background(CallbackPriority::Low, async move {
            sleep_background(delay).await;
            if let Some(me) = me.upgrade()
                && me
                    .timer
                    .compare_exchange(started, started + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                me.timer_expired();
            }
        }));
    }

    /// `cancelTimer()`
    fn cancel_timer(&self) {
        self.timer.fetch_add(1, Ordering::AcqRel);
    }

    /// `readRequest(replyTimeout_ms, readTimeout_ms, expectedLength, async)`:
    /// we want to read something.
    fn read_request(
        self: &Arc<Self>,
        reply_timeout_ms: u64,
        read_timeout_ms: u64,
        expected_length: isize,
        async_: bool,
    ) -> bool {
        debug!(format!(
            "AsynDriverInterface::readRequest({}, {reply_timeout_ms} msec reply, \
             {read_timeout_ms} msec read, expect {} bytes, async={})\n",
            self.client_name,
            expected_length as usize,
            if async_ { "yes" } else { "no" }
        ));
        {
            let mut state = self.state.lock();
            state.read_timeout_ms = read_timeout_ms;
            state.reply_timeout_ms = reply_timeout_ms;
            state.expected_length = expected_length;
            // First poll for input (no timeout), later poll periodically if
            // no other input arrives from intrCallbackOctet()
            state.io_action = if async_ {
                IoAction::AsyncRead
            } else {
                IoAction::Read
            };
        }
        let (queue_timeout, queue_timeout_s) = if async_ {
            (Duration::ZERO, -1.0)
        } else {
            (
                Duration::from_millis(reply_timeout_ms),
                secs(reply_timeout_ms),
            )
        };
        let priority = self.client_priority();
        let status = self.queue_request(self.priority(), queue_timeout);
        debug!(format!(
            "AsynDriverInterface::readRequest {}: queueRequest(..., priority={priority}, \
             queueTimeout={} sec) = {} [async={async_}] {}\n",
            self.client_name,
            fmt_g(queue_timeout_s),
            status_str(
                status
                    .as_ref()
                    .map_or_else(AsynError::status, |_| AsynStatus::Success)
            ),
            status
                .as_ref()
                .err()
                .map(AsynError::message)
                .unwrap_or_default()
        ));
        if !async_ {
            self.report_asyn_status(&status, "readRequest");
        }
        if status.is_err() {
            // Not queued for some reason (e.g. disconnected / already queued)
            if async_ {
                // silently try again later
                self.start_timer(secs(reply_timeout_ms));
                return true;
            }
            return false;
        }
        true
        // continues with:
        //    handleRequest() -> readHandler() -> readCallback()
        // or handleTimeout() -> readCallback(StreamIoTimeout)
    }

    /// `readHandler()`: now, we can read (called by asynManager).
    fn read_handler(self: &Arc<Self>, port: &mut dyn PortCalls) {
        let Some(client) = self.client() else {
            return;
        };
        // Setup eos if required.
        let streameos = client.get_in_terminator();
        let streameoslen = streameos.as_ref().map_or(0, Vec::len);
        let mut deveos: &[u8] = streameos.as_deref().unwrap_or_default();
        let mut oldeos = None;
        // streameos == None means: don't change eos. C keeps no old
        // terminator when getInputEos fails; asyn-rs's cannot.
        if streameos.is_some()
            && let Ok(old) = port.call(&RequestOp::GetInputEos, self.user_on_port())
        {
            let old = old.data.unwrap_or_default();
            // device (e.g. GPIB) might not accept full eos length
            while deveos != old.as_slice() {
                match port.call(
                    &RequestOp::SetInputEos {
                        eos: deveos.to_vec(),
                    },
                    self.user_on_port(),
                ) {
                    Ok(_) => {
                        debug2!(
                            "AsynDriverInterface::readHandler(",
                            self.client_name,
                            ") input EOS changed from \"",
                            StreamBuffer::from_bytes(&old).expand(),
                            "\" to \"",
                            StreamBuffer::from_bytes(deveos).expand(),
                            "\"\n"
                        );
                        break;
                    }
                    Err(e) => {
                        deveos = deveos.get(1..).unwrap_or_default();
                        if deveos.is_empty() {
                            stream_error(&cmsg!(
                                self.client_name,
                                ": warning: pasynOctet->setInputEos() failed: ",
                                e.message(),
                                "\n"
                            ));
                            break;
                        }
                    }
                }
            }
            oldeos = Some(old);
        }

        let (mut bytes_to_read, buffersize, reply_timeout_ms, read_timeout_ms) = {
            let mut state = self.state.lock();
            let mut bytes_to_read = state.peeksize;
            let buffersize = if state.expected_length > 0 {
                if state.peeksize > 1 {
                    // we can't peek, try to read whole message
                    bytes_to_read = state.expected_length as usize;
                }
                state.expected_length as usize
            } else {
                state.input_buffer.capacity()
            };
            state.input_buffer.clear().reserve(buffersize);
            (
                bytes_to_read,
                buffersize,
                state.reply_timeout_ms,
                state.read_timeout_ms,
            )
        };
        // A readMore larger than the buffer is read whole here; C reads it
        // into inputBuffer's `buffersize` bytes and writes past them.
        let mut timeout = if self.state.lock().io_action == IoAction::AsyncRead {
            // In AsyncRead mode just poll and read as much as possible
            bytes_to_read = buffersize;
            0.0
        } else {
            secs(reply_timeout_ms)
        };
        let mut wait_for_reply = true;
        loop {
            let mut read_more: isize = 0;
            // C's `received` is `buffer.len() - taken_back`: -1 when the EOS
            // layer takes back a terminator byte the last read delivered
            // (asynInterposeEos.c:201-204).
            let (mut status, mut buffer, mut taken_back, mut eom_reason, message) = match port.call(
                &RequestOp::OctetRead {
                    buf_size: bytes_to_read,
                },
                self.user_timed(Duration::from_secs_f64(timeout)),
            ) {
                Ok(r) => {
                    let mut data = r.data.unwrap_or_default();
                    data.truncate(r.nbytes);
                    (
                        AsynStatus::Success,
                        data,
                        r.taken_back,
                        EomReason::from_bits_truncate(r.eom_reason),
                        String::new(),
                    )
                }
                Err(e) => {
                    let (data, eom) = e
                        .partial_read()
                        .map_or((Vec::new(), EomReason::empty()), |p| {
                            (p.data.clone(), p.eom_reason)
                        });
                    (e.status(), data, 0, eom, e.message())
                }
            };
            let (io_action, connected) = {
                let state = self.state.lock();
                (state.io_action, state.connected)
            };
            let received = buffer.len() as isize - taken_back as isize;
            if !(status == AsynStatus::Timeout && timeout == 0.0 && received == 0) {
                debug!(
                    format!(
                        "AsynDriverInterface::readHandler({}): ioAction={} \
                         read({bytes_to_read} bytes, timeout={} sec) returned status {}: \
                         received={} bytes, eomReason={}, buffer=\"",
                        self.client_name,
                        io_action.to_str(),
                        fmt_g(timeout),
                        status_str(status),
                        received,
                        eom_reason_str(eom_reason)
                    ),
                    StreamBuffer::from_bytes(&buffer).expand(),
                    "\"\n"
                );
            }
            // asyn 4.16 sets reason to ASYN_EOM_END when device disconnects.
            if !connected {
                eom_reason |= EomReason::END;
            }
            if status == AsynStatus::Timeout && timeout == 0.0 && received > 0 {
                // Jens Eden (PTB) pointed out that polling asynInterposeEos
                // with timeout = 0.0 returns asynTimeout even when bytes
                // have been received, but not yet the terminator.
                status = AsynStatus::Success;
            }
            match status {
                AsynStatus::Success => {
                    if io_action != IoAction::Read {
                        debug!(
                            format!(
                                "AsynDriverInterface::readHandler({}): AsyncRead poll: \
                                 received {} of {bytes_to_read} bytes \"",
                                self.client_name, received
                            ),
                            StreamBuffer::from_bytes(&buffer).expand(),
                            format!(
                                "\" eomReason={} [data ignored]\n",
                                eom_reason_str(eom_reason)
                            )
                        );
                        // ignore what we got from here: input was already
                        // handled by asynReadHandler(); read until no more
                        // input is available
                        read_more = -1;
                    } else {
                        debug!(
                            format!(
                                "AsynDriverInterface::readHandler({}): \
                                 received {} of {bytes_to_read} bytes \"",
                                self.client_name, received
                            ),
                            StreamBuffer::from_bytes(&buffer).expand(),
                            format!("\" eomReason={}\n", eom_reason_str(eom_reason))
                        );
                        // asynOctet->read() cuts off terminator, but if
                        // stream has set a terminator which is longer than
                        // what the device (e.g. GPIB) can handle, only the
                        // last part is cut. Restore complete terminator and
                        // leave it to StreamCore to find out if this was
                        // really the end of the input.
                        // C skips the terminator bytes that would land before
                        // the buffer while `received` < 0.
                        if deveos.len() < streameoslen && eom_reason.contains(EomReason::EOS) {
                            let skip = taken_back.min(deveos.len());
                            buffer.extend_from_slice(&deveos[skip..]);
                            taken_back -= skip;
                            eom_reason.remove(EomReason::EOS);
                        }
                        read_more = client.read_callback(
                            if eom_reason.intersects(EomReason::END | EomReason::EOS) {
                                StreamIoStatus::StreamIoEnd
                            } else {
                                StreamIoStatus::StreamIoSuccess
                            },
                            if taken_back > 0 {
                                ReadInput::TakeBack(taken_back)
                            } else {
                                ReadInput::Bytes(&buffer)
                            },
                        );
                    }
                }
                AsynStatus::Timeout if buffer.is_empty() && wait_for_reply => {
                    // reply timeout
                    if io_action == IoAction::AsyncRead {
                        debug2!(format!(
                            "AsynDriverInterface::readHandler({}): \
                             no async input, retry in in {} seconds\n",
                            self.client_name,
                            fmt_g(secs(reply_timeout_ms))
                        ));
                        // start next poll after timer expires
                        if reply_timeout_ms != 0 {
                            self.start_timer(secs(reply_timeout_ms));
                        }
                        // continues with:
                        //    timerExpired() -> queueRequest() ->
                        //                 handleRequest() -> readHandler()
                        // or intrCallbackOctet() -> asynReadHandler()
                    } else {
                        debug!(format!(
                            "AsynDriverInterface::readHandler({}): no reply\n",
                            self.client_name
                        ));
                        read_more = client
                            .read_callback(StreamIoStatus::StreamIoNoReply, ReadInput::Bytes(&[]));
                    }
                }
                AsynStatus::Timeout => {
                    // read timeout
                    debug!(
                        format!(
                            "AsynDriverInterface::readHandler({}): ioAction={}, \
                             timeout [{} sec] after {} of {bytes_to_read} bytes \"",
                            self.client_name,
                            io_action.to_str(),
                            fmt_g(timeout),
                            buffer.len()
                        ),
                        StreamBuffer::from_bytes(&buffer).expand(),
                        "\"\n"
                    );
                    if matches!(io_action, IoAction::AsyncRead | IoAction::AsyncReadMore) {
                        // we already got the data from asynReadHandler()
                        client
                            .read_callback(StreamIoStatus::StreamIoTimeout, ReadInput::Bytes(&[]));
                    } else {
                        read_more = client.read_callback(
                            StreamIoStatus::StreamIoTimeout,
                            ReadInput::Bytes(&buffer),
                        );
                    }
                }
                AsynStatus::Overflow => {
                    {
                        let mut state = self.state.lock();
                        if bytes_to_read == 1 {
                            // device does not support peeking
                            // try to read whole message next time
                            state.input_buffer.clear().reserve(100);
                        } else {
                            // buffer was still too small
                            // try larger buffer next time
                            let capacity = state.input_buffer.capacity();
                            state.input_buffer.clear().reserve(capacity * 2);
                        }
                        state.peeksize = state.input_buffer.capacity();
                    }
                    // deliver whatever we could save
                    stream_error(&cmsg!(
                        self.client_name,
                        ": asynOverflow in read: ",
                        message,
                        "\n"
                    ));
                    client.read_callback(StreamIoStatus::StreamIoFault, ReadInput::Bytes(&buffer));
                }
                AsynStatus::Error => {
                    if !connected {
                        stream_error(&cmsg!(
                            self.client_name,
                            ": device ",
                            self.name,
                            " disconnected\n"
                        ));
                        client.disconnect_callback(StreamIoStatus::StreamIoSuccess);
                    } else {
                        stream_error(&cmsg!(
                            self.client_name,
                            ": asynError in read: ",
                            message,
                            "\n"
                        ));
                        client.read_callback(
                            StreamIoStatus::StreamIoFault,
                            ReadInput::Bytes(&buffer),
                        );
                    }
                }
                AsynStatus::Disconnected => {
                    stream_error(&cmsg!(
                        self.client_name,
                        ": asynDisconnected in read: ",
                        message,
                        "\n"
                    ));
                    self.state.lock().connected = false;
                    client.disconnect_callback(StreamIoStatus::StreamIoSuccess);
                    return;
                }
                AsynStatus::Disabled => {
                    stream_error(&cmsg!(
                        self.client_name,
                        ": asynDisabled in read: ",
                        message,
                        "\n"
                    ));
                    client.read_callback(StreamIoStatus::StreamIoFault, ReadInput::Bytes(&[]));
                    return;
                }
            }
            if read_more == 0 {
                break;
            }
            bytes_to_read = if read_more > 0 {
                read_more as usize
            } else {
                self.state.lock().input_buffer.capacity()
            };
            debug!(format!(
                "AsynDriverInterface::readHandler({}) readMore={read_more} \
                 bytesToRead={bytes_to_read}\n",
                self.client_name
            ));
            timeout = secs(read_timeout_ms);
            wait_for_reply = false;
        }

        // restore original EOS. C restores only when the old terminator
        // differs from the one set in length and in bytes
        // (AsynDriverInterface.cc:1088-1089), so an old terminator of the
        // same length stays replaced; here any different one is restored.
        if let Some(oldeos) = oldeos
            && deveos != oldeos.as_slice()
        {
            let _ = port.call(
                &RequestOp::SetInputEos {
                    eos: oldeos.clone(),
                },
                self.user_on_port(),
            );
            debug2!(
                "AsynDriverInterface::readHandler(",
                self.client_name,
                ") input EOS restored from \"",
                StreamBuffer::from_bytes(deveos).expand(),
                "\" to \"",
                StreamBuffer::from_bytes(&oldeos).expand(),
                "\"\n"
            );
        }
    }

    /// `supportsEvent()`: can we handle the StreamDevice command 'event'?
    fn supports_event(self: &Arc<Self>) -> bool {
        let mut events = self.events.lock();
        if events.is_some() {
            return true;
        }
        let bound = self.bound.get().expect("a live interface is bound");
        let mut message = String::new();
        // look for interfaces for events; no asynInt32 available, thus try
        // asynUInt32
        for (iface, mask) in [
            (InterfaceType::Int32, None),
            (InterfaceType::UInt32Digital, Some(0xFFFF_FFFF)),
        ] {
            if !bound.port.has_interface(iface) {
                continue;
            }
            let me = Arc::downgrade(self);
            let filter = InterruptFilter {
                // required for GPIB
                reason: Some(ASYN_REASON_SIGNAL),
                addr: Some(bound.addr),
                uint32_mask: mask,
                iface: Some(iface),
                ..InterruptFilter::default()
            };
            match bound.port.interrupts().register_sync_callback(
                filter,
                move |value: &InterruptValue| {
                    if let Some(me) = me.upgrade() {
                        me.intr_callback(&value.value);
                    }
                },
            ) {
                Ok(subscription) => {
                    *events = Some(subscription);
                    return true;
                }
                Err(e) => message = e.message(),
            }
        }
        stream_error(&cmsg!(
            self.client_name,
            ": port ",
            self.name,
            " does not allow to register for Int32 or UInt32 interrupts: ",
            message,
            "\n"
        ));
        // no event interface available
        false
    }

    /// `supportsAsyncRead()`: hook "I/O Intr" support.
    fn supports_async_read(self: &Arc<Self>) -> bool {
        let mut octet = self.octet.lock();
        if octet.is_some() {
            return true;
        }
        let bound = self.bound.get().expect("a live interface is bound");
        let me = Arc::downgrade(self);
        let filter = InterruptFilter {
            addr: Some(bound.port.param_list(bound.addr)),
            iface: Some(InterfaceType::Octet),
            ..InterruptFilter::default()
        };
        match bound.port.interrupts().register_sync_callback(
            filter,
            move |value: &InterruptValue| {
                if let Some(me) = me.upgrade() {
                    me.intr_callback_octet(value);
                }
            },
        ) {
            Ok(subscription) => {
                *octet = Some(subscription);
                true
            }
            Err(e) => {
                stream_error(&cmsg!(
                    self.client_name,
                    ": asyn port ",
                    self.name,
                    " does not support asynchronous input: ",
                    e.message(),
                    "\n"
                ));
                false
            }
        }
    }

    /// `intrCallbackOctet(data, numchars, eomReason)`
    fn intr_callback_octet(self: &Arc<Self>, value: &InterruptValue) {
        let ParamValue::Octet(data) = &value.value else {
            return;
        };
        let Some(client) = self.client() else {
            return;
        };
        if !client.interrupt_accept() {
            return; // too early to process records
        }
        self.asyn_read_handler(&*client, data, value.eom_reason);
    }

    /// `asynReadHandler(buffer, received, eomReason)`: get asynchronous
    /// input.
    fn asyn_read_handler(self: &Arc<Self>, client: &dyn Client, buffer: &[u8], eom: EomReason) {
        let mut eom_reason = eom;
        debug!(
            format!(
                "AsynDriverInterface::asynReadHandler({}, buffer=\"",
                self.client_name
            ),
            StreamBuffer::from_bytes(buffer).expand(),
            format!(
                "\", received={} eomReason={}) ioAction={}\n",
                buffer.len(),
                eom_reason_str(eom_reason),
                self.state.lock().io_action.to_str()
            )
        );
        // The poll a timer may have queued is now useless: mark it obsolete
        // (see handleRequest), as cancelRequest must not block here.
        self.state.lock().io_action = IoAction::None;
        let mut read_more: isize = 1;
        if !buffer.is_empty() {
            let bound = self.bound.get().expect("a live interface is bound");
            let streameos = client.get_in_terminator();
            let deveos;
            let mut buffer = buffer;
            if eom_reason.contains(EomReason::EOS) {
                // Terminator was cut off.
                if streameos.is_none() {
                    // Just handle eos as normal end condition.
                    eom_reason |= EomReason::END;
                } else {
                    // We can't just append the terminator: first process
                    // the received data, then add the terminator.
                    deveos = bound.port.input_eos(bound.addr);
                    client.read_callback(StreamIoStatus::StreamIoSuccess, ReadInput::Bytes(buffer));
                    buffer = &deveos;
                }
            } else if streameos.is_none() {
                // Terminator neither cut off nor set by stream: cut it off
                // now.
                deveos = bound.port.input_eos(bound.addr);
                if let Some(rest) = buffer.strip_suffix(deveos.as_slice()) {
                    buffer = rest;
                    eom_reason |= EomReason::END;
                }
            }
            read_more = client.read_callback(
                if eom_reason.contains(EomReason::END) {
                    StreamIoStatus::StreamIoEnd
                } else {
                    StreamIoStatus::StreamIoSuccess
                },
                ReadInput::Bytes(buffer),
            );
        }
        if read_more != 0 {
            // wait for more input
            let read_timeout_ms = {
                let mut state = self.state.lock();
                state.io_action = IoAction::AsyncReadMore;
                state.read_timeout_ms
            };
            self.start_timer(secs(read_timeout_ms));
        }
        debug!(format!(
            "AsynDriverInterface::asynReadHandler({}) readMore={read_more}, ioAction={} \n",
            self.client_name,
            self.state.lock().io_action.to_str()
        ));
    }

    /// `acceptEvent(mask, replytimeout_ms)`: we want to receive an event.
    fn accept_event(self: &Arc<Self>, mask: u64, replytimeout_ms: u64) -> bool {
        {
            let mut state = self.state.lock();
            if state.received_event & mask == 0 {
                state.event_mask = mask;
                state.io_action = IoAction::ReceiveEvent;
                drop(state);
                if replytimeout_ms != 0 {
                    self.start_timer(secs(replytimeout_ms));
                }
                return true;
            }
            // handle early events
            state.received_event = 0;
        }
        if let Some(client) = self.client() {
            client.event_callback(StreamIoStatus::StreamIoSuccess);
        }
        true
    }

    /// `intrCallbackInt32(data)` and `intrCallbackUInt32(data)`, as C
    /// widens each to the `unsigned long` of the event mask.
    fn intr_callback(&self, value: &ParamValue) {
        let (name, data, shown) = match *value {
            ParamValue::Int32(v) => ("intrCallbackInt32", v as i64 as u64, v as i64),
            ParamValue::UInt32Digital(v) => ("intrCallbackUInt32", v as u64, v as i64),
            _ => return,
        };
        debug!(format!(
            "AsynDriverInterface::{name} ({}, {shown})\n",
            self.client_name
        ));
        {
            let mut state = self.state.lock();
            if state.event_mask == 0 {
                // store early events
                state.received_event = data;
                return;
            }
            if data & state.event_mask == 0 {
                return;
            }
            state.event_mask = 0;
        }
        if let Some(client) = self.client() {
            client.event_callback(StreamIoStatus::StreamIoSuccess);
        }
    }

    /// `timerExpired()`
    fn timer_expired(self: &Arc<Self>) {
        let (io_action, connected, reply_timeout_ms) = {
            let state = self.state.lock();
            (state.io_action, state.connected, state.reply_timeout_ms)
        };
        match io_action {
            // Timeout of async poll crossed with parasitic input
            IoAction::None => {}
            IoAction::ReceiveEvent => {
                // timeout while waiting for event
                self.state.lock().io_action = IoAction::None;
                if let Some(client) = self.client() {
                    client.event_callback(StreamIoStatus::StreamIoTimeout);
                }
            }
            IoAction::AsyncReadMore => {
                // timeout after reading some async data: deliver what we
                // have
                if let Some(client) = self.client() {
                    client.read_callback(StreamIoStatus::StreamIoTimeout, ReadInput::Bytes(&[]));
                }
            }
            IoAction::AsyncRead => {
                // No async input for some time, thus let's poll.
                let autoconnect = self.user.lock().is_auto_connect().unwrap_or(false);
                if autoconnect && !connected {
                    // has explicitely been disconnected: a poll would
                    // autoConnect which is not what we want; just retry later
                    self.start_timer(secs(reply_timeout_ms));
                } else {
                    // queue for read poll (no timeout)
                    let status = self.queue_request(QueuePriority::Low, Duration::ZERO);
                    // if this fails, we are already queued by another thread
                    debug2!(format!(
                        "AsynDriverInterface::timerExpired {}: \
                         queueRequest(..., priority=Low, queueTimeout=-1) = {} {}\n",
                        self.client_name,
                        status_str(
                            status
                                .as_ref()
                                .map_or_else(AsynError::status, |_| AsynStatus::Success)
                        ),
                        status
                            .as_ref()
                            .err()
                            .map(AsynError::message)
                            .unwrap_or_default()
                    ));
                    if status.is_err() {
                        self.start_timer(secs(reply_timeout_ms));
                    }
                    // continues with:
                    //    handleRequest() -> readHandler() -> readCallback()
                }
            }
            IoAction::Lock
            | IoAction::Write
            | IoAction::Read
            | IoAction::Connect
            | IoAction::Disconnect => stream_error(&cmsg!(
                "INTERNAL ERROR (",
                self.client_name,
                "): timerExpired() unexpected ioAction ",
                io_action.to_str(),
                "\n"
            )),
        }
    }

    /// `handleRequest()`: the `pasynUser`'s `processUser`.
    fn handle_request(self: &Arc<Self>, port: &mut dyn PortCalls) {
        self.cancel_timer();
        let io_action = self.state.lock().io_action;
        debug2!(format!(
            "AsynDriverInterface::handleRequest({}) {}\n",
            self.client_name,
            io_action.to_str()
        ));
        match io_action {
            // ignore obsolete poll request
            IoAction::None => {}
            IoAction::Lock => self.lock_handler(),
            IoAction::Write => self.write_handler(port),
            // polled async input, sync input
            IoAction::AsyncRead | IoAction::AsyncReadMore | IoAction::Read => {
                self.read_handler(port)
            }
            IoAction::Connect => self.connect_handler(port),
            IoAction::Disconnect => self.disconnect_handler(port),
            IoAction::ReceiveEvent => stream_error(&cmsg!(
                "INTERNAL ERROR (",
                self.client_name,
                "): handleRequest() unexpected ioAction ",
                io_action.to_str(),
                "\n"
            )),
        }
    }

    /// `handleTimeout()`: the `pasynUser`'s `timeoutUser`.
    fn handle_timeout(&self) {
        let io_action = self.state.lock().io_action;
        debug!(format!(
            "AsynDriverInterface::handleTimeout({})\n",
            self.client_name
        ));
        match io_action {
            IoAction::Lock => {
                if let Some(client) = self.client() {
                    client.lock_callback(StreamIoStatus::StreamIoTimeout);
                }
            }
            IoAction::Write => {
                if let Some(client) = self.client() {
                    client.write_callback(StreamIoStatus::StreamIoTimeout);
                }
            }
            IoAction::Read => {
                if let Some(client) = self.client() {
                    client.read_callback(StreamIoStatus::StreamIoFault, ReadInput::Bytes(&[]));
                }
            }
            IoAction::AsyncReadMore => {
                if let Some(client) = self.client() {
                    client.read_callback(StreamIoStatus::StreamIoTimeout, ReadInput::Bytes(&[]));
                }
            }
            IoAction::Connect => {
                if let Some(client) = self.client() {
                    client.connect_callback(StreamIoStatus::StreamIoTimeout);
                }
            }
            // should not happen because of infinite timeout
            IoAction::Disconnect => stream_error(&cmsg!(
                "AsynDriverInterface ",
                self.client_name,
                ": disconnect timeout\n"
            )),
            // No AsyncRead here because we don't use timeout when polling
            IoAction::None | IoAction::AsyncRead | IoAction::ReceiveEvent => stream_error(&cmsg!(
                "INTERNAL ERROR (",
                self.client_name,
                "): handleTimeout() unexpected ioAction ",
                io_action.to_str(),
                "\n"
            )),
        }
    }

    /// `exceptionHandler(exception)`
    fn exception_handler(&self, exception: AsynException) {
        debug!(format!(
            "AsynDriverInterface::exceptionHandler({}, {})\n",
            self.client_name,
            exception_str(exception)
        ));
        if exception != AsynException::Connect {
            return;
        }
        let connected = self.user.lock().is_connected().unwrap_or(false);
        let io_action = {
            let mut state = self.state.lock();
            state.connected = connected;
            state.io_action
        };
        debug!(format!(
            "AsynDriverInterface::exceptionHandler({}) {} {}. ioAction: {}\n",
            self.client_name,
            self.name,
            if connected {
                "connected"
            } else {
                "disconnected"
            },
            io_action.to_str()
        ));
        if connected
            && io_action == IoAction::None
            && let Some(client) = self.client()
        {
            client.connect_callback(StreamIoStatus::StreamIoSuccess);
        }
    }
}

impl Drop for Inner {
    /// `~AsynDriverInterface()`: off the exception list, then unbound.
    fn drop(&mut self) {
        self.exception.get_mut().take();
        self.user.get_mut().disconnect();
    }
}

/// `getBusInterface(client, portname, addr, param)`: do we have this
/// port/addr ?
pub fn get_bus_interface(
    client: Weak<dyn Client>,
    portname: &str,
    addr: i32,
    _param: &str,
) -> Option<Box<dyn StreamBusInterface>> {
    let client_name = client.upgrade().map(|c| c.name()).unwrap_or_default();
    debug!(format!(
        "AsynDriverInterface::getBusInterface({client_name}, {portname}, {addr})\n"
    ));
    let name = if addr >= 0 {
        format!("{portname} {addr}")
    } else {
        portname.to_string()
    };
    let interface = Arc::new(Inner::new(client, client_name, name));
    if interface.connect_to_bus(portname, addr) {
        debug!(format!(
            "AsynDriverInterface::getBusInterface({portname}, {addr}): new interface allocated\n"
        ));
        return Some(Box::new(AsynDriverInterface(interface)));
    }
    None
}

impl Drop for AsynDriverInterface {
    /// `~AsynDriverInterface()`'s cancelTimer, cancelInterruptUser and
    /// cancelRequest (AsynDriverInterface.cc:285-308), made while the
    /// handlers can still reach the interface: a queued request ends with
    /// neither callback.
    fn drop(&mut self) {
        self.0.cancel_timer();
        self.0.events.lock().take();
        self.0.octet.lock().take();
        if let Some(bound) = self.0.bound.get() {
            bound.cell.cancel();
        }
    }
}

impl StreamBusInterface for AsynDriverInterface {
    fn lock_request(&self, lock_timeout_ms: u64) -> bool {
        self.0.lock_request(lock_timeout_ms)
    }
    fn unlock(&self) -> bool {
        self.0.unlock()
    }
    fn write_request(&self, output: &[u8], timeout_ms: u64) -> bool {
        self.0.write_request(output, timeout_ms)
    }
    fn read_request(
        &self,
        replytimeout_ms: u64,
        readtimeout_ms: u64,
        expected_length: isize,
        async_: bool,
    ) -> bool {
        self.0
            .read_request(replytimeout_ms, readtimeout_ms, expected_length, async_)
    }
    fn supports_event(&self) -> bool {
        self.0.supports_event()
    }
    fn supports_async_read(&self) -> bool {
        self.0.supports_async_read()
    }
    fn accept_event(&self, mask: u64, replytimeout_ms: u64) -> bool {
        self.0.accept_event(mask, replytimeout_ms)
    }
    fn connect_request(&self, connecttimeout_ms: u64) -> bool {
        self.0.connect_request(connecttimeout_ms)
    }
    fn disconnect_request(&self) -> bool {
        self.0.disconnect_request()
    }
    fn finish(&self) {
        self.0.finish()
    }
}

/// `RegisterStreamBusInterface(AsynDriverInterface)`: once per process, as
/// C's static registrar runs once.
pub fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| bus::register_stream_bus_interface("AsynDriverInterface", get_bus_interface));
}

/// `streamReinit(portname, addr)`: the port's device announced down and up
/// again, without the driver, so its records reconnect. The errorMessage
/// lines go to `err`, C's stderr.
pub fn stream_reinit(portname: Option<&str>, addr: i32, err: &mut dyn std::io::Write) -> i64 {
    let Some(portname) = portname else {
        let _ = err.write_all(b"Usage: streamReinit \"portname\", [addr]\n");
        return -1;
    };
    let mut user = DeviceUser::default();
    let status = match user.connect_device(Some(portname), addr) {
        Err(_) => Err((AsynStatus::Error, user.error_message().to_owned())),
        Ok(entry) => entry
            .handle
            .exception_disconnect_blocking(addr)
            .and_then(|()| entry.handle.exception_connect_blocking(addr))
            .map_err(|e| (e.status(), e.message())),
    };
    user.disconnect();
    match status {
        Ok(()) => AsynStatus::Success as i64,
        Err((status, message)) => {
            let _ = writeln!(err, "{message}");
            status as i64
        }
    }
}

/// `AsynDriverInterfaceRegistrar`'s iocsh command `streamReinit`.
pub fn iocsh_commands() -> Vec<epics_base_rs::server::iocsh::registry::CommandDef> {
    use epics_base_rs::server::iocsh::registry::{
        ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome,
    };
    vec![CommandDef::new(
        "streamReinit",
        vec![
            ArgDesc {
                name: "portname",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "[addr]",
                arg_type: ArgType::Int,
            },
        ],
        // C's iocshFuncDef has no usage
        "",
        |args: &[ArgValue], ctx: &CommandContext| {
            let portname = match args.first() {
                Some(ArgValue::String(s)) => Some(s.to_str_lossy().into_owned()),
                _ => None,
            };
            // iocsh hands a missing int as 0
            let addr = match args.get(1) {
                Some(ArgValue::Int(v)) => *v as i32,
                _ => 0,
            };
            let mut err = Vec::new();
            stream_reinit(portname.as_deref(), addr, &mut err);
            for line in String::from_utf8_lossy(&err).lines() {
                ctx.eprintln(line);
            }
            Ok(CommandOutcome::Continue)
        },
    )]
}
