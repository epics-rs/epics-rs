//! Port driver base and trait.
//!
//! # I/O Model
//!
//! Ports are driven by a `PortActor` running on a dedicated thread.
//! The actor exclusively owns the driver and processes requests from a channel.
//!
//! **Cache path** (default `read_*`/`write_*` methods):
//! - Default implementations operate on the parameter cache (non-blocking).
//! - Background tasks update cache via `set_*_param()` + `call_param_callbacks()`.
//!
//! **Actor path** (requests submitted via [`crate::port_handle::PortHandle`]):
//! - Each port gets a dedicated actor thread that dispatches requests to driver methods.
//! - `can_block` indicates the port may perform blocking I/O.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use std::any::Any;

/// C `autoConnectDevice` reconnect throttle window (asynManager.c:713).
/// A disconnected `auto_connect` device is refused a fresh connect attempt
/// until this much time has elapsed since its last connect/disconnect
/// transition or attempt, bounding reconnect storms to one attempt per
/// window.
const AUTO_CONNECT_THROTTLE: Duration = Duration::from_secs(2);

/// First autonomous connect-retry delay after a port drops. C
/// `exceptionDisconnect` arms the port's connect timer at `.01` seconds
/// (asynManager.c:2181-2182), so the reconnect is attempted essentially
/// immediately and then backs off to [`DEFAULT_SECONDS_BETWEEN_PORT_CONNECT`].
const CONNECT_RETRY_INITIAL: Duration = Duration::from_millis(10);

/// C `DEFAULT_SECONDS_BETWEEN_PORT_CONNECT` (asynManager.c:48) — the interval
/// `portConnectProcessCallback` re-arms the connect timer at after a failed
/// attempt (asynManager.c:3281).
const DEFAULT_SECONDS_BETWEEN_PORT_CONNECT: Duration = Duration::from_secs(20);

/// Per-address device state for multi-device ports.
#[derive(Debug, Clone)]
pub struct DeviceState {
    pub connected: bool,
    pub enabled: bool,
    pub auto_connect: bool,
    /// Monotonic instant of the last connect/disconnect transition or
    /// auto-connect attempt for this device — the anchor for the 2s
    /// reconnect throttle (C `dpCommon.lastConnectDisconnect`). `None`
    /// mirrors C's zero-initialised timestamp: the first attempt is
    /// always permitted.
    pub last_connect_disconnect: Option<Instant>,
}

impl Default for DeviceState {
    fn default() -> Self {
        Self {
            connected: true,
            enabled: true,
            auto_connect: true,
            last_connect_disconnect: None,
        }
    }
}

/// One device's end-of-string terminators — C's `eosPvt.eosIn` / `eosPvt.eosOut`
/// (asynInterposeEos.c:44-52), which exist once per (port, addr).
#[derive(Debug, Clone, Default)]
pub struct DeviceEos {
    /// Input EOS sequence (max 2 bytes). Empty = no input EOS detection.
    pub input: Vec<u8>,
    /// Output EOS sequence (max 2 bytes). Empty = no output EOS append.
    pub output: Vec<u8>,
}

/// The device an EOS hook's `asynUser` selects — the single owner of the rule,
/// shared by [`PortDriverBase`] and the EOS interpose so the terminator a
/// `setInputEos` writes is the one the next `read` on that user applies.
///
/// C creates the EOS interpose per (port, addr) and every hook takes the
/// `asynUser` (asynInterposeEos.c:288-296), so on a multi-device port the addr
/// picks the instance. On a port that never declared `ASYN_MULTIDEVICE` there
/// are no devices to pick from: `findDpCommon` (asynManager.c:536-544) and
/// `findInterface` resolve *every* addr to the port itself, so `asynSetEos`
/// with addr 0 and with addr -1 must reach the same terminator. That collapse
/// is what the `-1` key below is.
pub fn eos_device_key(multi_device: bool, addr: i32) -> i32 {
    dp_common_key(multi_device, addr).unwrap_or(-1)
}

/// C `findDpCommon` (asynManager.c:538-545) as a key: which `dpCommon` a
/// `(port, addr)` pair names. `Some(a)` is that device's own slot; `None` is
/// the port's — C's `!(pport->attributes&ASYN_MULTIDEVICE) || !pdevice` arm,
/// which a single-device port and a port-level user both take (`connectDevice`
/// creates a device node only for `addr >= 0`, asynManager.c:1349-1352).
///
/// One function for every consumer of the resolution — connection state, the
/// trace configuration ([`crate::trace`]) and the EOS terminators
/// ([`eos_device_key`]) — because C asks it once, in `findDpCommon`, and then
/// reads *every* field off the one struct it returned: `enabled`, `connected`,
/// `autoConnect`, `numberConnects`, `lastConnectDisconnect` and `trace`
/// (through `findTracePvt`, :546-551). Separate copies of the rule are how the
/// trace mask came to be per-device while `enabled` beside it stayed port-wide.
pub fn dp_common_key(multi_device: bool, addr: i32) -> Option<i32> {
    (multi_device && addr >= 0).then_some(addr)
}

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::exception::{AsynException, ExceptionEvent, ExceptionManager};
use crate::interfaces::InterfaceType;
use crate::interpose::{
    EomReason, EosSet, OctetInterpose, OctetInterposeStack, OctetNext, OctetReadResult,
};
use crate::interrupt::{InterruptManager, InterruptValue, OctetFanOut};
use crate::param::{EnumEntry, InterruptReason, ParamList, ParamType, ParamValue};
use crate::trace::TraceManager;
use crate::user::{AsynUser, ConnectCheck};

/// C asyn `queueRequest` priority. In asyn-rs this exists as compatibility
/// metadata only — there is no actual request queue or priority-based scheduling.
/// Drivers manage their own async tasks directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum QueuePriority {
    Low = 0,
    #[default]
    Medium = 1,
    High = 2,
    /// Connect/disconnect operations — processed even when disabled/disconnected.
    Connect = 3,
}

/// Port configuration flags.
#[derive(Debug, Clone, Copy)]
pub struct PortFlags {
    /// True if port supports multiple sub-addresses (ASYN_MULTIDEVICE).
    pub multi_device: bool,
    /// True if port can block (ASYN_CANBLOCK).
    ///
    /// When `true`, the port gets a dedicated worker thread that serializes I/O via a
    /// priority queue (matching C asyn's per-port thread model).
    ///
    /// When `false`, requests execute synchronously inline on the caller's thread
    /// (no worker thread is spawned). This is appropriate for non-blocking drivers
    /// whose `io_*` methods return immediately (e.g., cache-based parameter access).
    pub can_block: bool,
    /// True if port can be destroyed via shutdown_port (ASYN_DESTRUCTIBLE).
    pub destructible: bool,
}

impl Default for PortFlags {
    fn default() -> Self {
        // `destructible: false` is the C asyn convention — see
        // asynDriver.h:97 (`#define ASYN_DESTRUCTIBLE 0x0004`) — the
        // attribute is opt-in via `pasynManager->registerPort(..., attr)`
        // and `asynManager::shutdownPort` refuses to act on ports
        // that did not opt in. Defaulting to `true` here over-applied
        // shutdown rights to every driver that built PortFlags via
        // `..PortFlags::default()`.
        Self {
            multi_device: false,
            can_block: false,
            destructible: false,
        }
    }
}

/// Base state shared by all port drivers.
/// Contains the parameter library, interrupt manager, and connection state.
///
/// # Interpose concurrency
///
/// `interpose_octet` requires `&mut self` for all operations (both `push` and
/// `dispatch_*`). Since `PortDriverBase` is always behind `Arc<Mutex<dyn PortDriver>>`,
/// any access to `interpose_octet` requires the port lock. This naturally
/// serializes interpose modifications with I/O dispatch — no additional
/// synchronization is needed. **Callers must never modify the interpose stack
/// without holding the port lock.**
/// Where a port's `connected` truth lives.
///
/// `Own` — the port opens and closes its own link (every driver that dials out:
/// IP, serial, USB-TMC, VXI-11, …), so its own cell is the truth.
///
/// `Shared` — the link belongs to another object and this port merely serves it.
/// C models the case with a real child port whose `connectIt`/`closeConnection`
/// the *owner* drives (`drvAsynIPServerPort.c:358-360` — the listener calls
/// `pasynCommonSyncIO->connectDevice` on the child the moment it hands it a
/// socket). Sharing the owner's cell is the same thing without the round trip,
/// and it is what makes "the owner holds a live socket, the port says
/// disconnected" unrepresentable rather than merely unlikely.
#[derive(Debug, Clone)]
enum Connection {
    Own(bool),
    Shared(Arc<AtomicBool>),
}

impl Connection {
    fn get(&self) -> bool {
        match self {
            Connection::Own(c) => *c,
            Connection::Shared(cell) => cell.load(Ordering::Acquire),
        }
    }
}

pub struct PortDriverBase {
    pub port_name: String,
    pub max_addr: usize,
    pub flags: PortFlags,
    pub params: ParamList,
    pub interrupts: InterruptManager,
    /// Whether the port's transport is up — read it with [`Self::is_connected`],
    /// move it with [`Self::set_connected`].
    ///
    /// It is not a plain `bool` because not every port *owns* its link. An
    /// IP-server child port serves a socket that lives in the parent's
    /// [`crate::drivers::ip_server_port::ClientSlot`]: the listener assigns and
    /// clears it, and the child cannot see either edge. A cached copy therefore
    /// went stale in exactly the way that matters — the slot held a live client
    /// while the child port still said `asynDisconnected` and refused every
    /// read and write, forever (R13-50). Such a port shares the owner's cell
    /// instead of copying it, so that state cannot be constructed.
    connected: Connection,
    /// The last value fanned out to listeners. Memory for the edge detector in
    /// [`Self::sync_connection_edge`], never an answer to "is the port up?" —
    /// [`Self::is_connected`] is the only thing that answers that, and it reads
    /// the truth.
    last_announced: bool,
    pub enabled: bool,
    pub auto_connect: bool,
    /// C `dpCommon.defunct` (asynManager.c:2284) — the port was torn down via
    /// `shutdownPort` and is gone for good. Read it with [`Self::is_defunct`].
    ///
    /// **Invariant: `defunct ⟹ !enabled`.** C establishes it in `shutdownPort`,
    /// which clears `enabled` *and* sets `defunct` in the same breath (:2283-2284)
    /// with the comment that disabling is what short-circuits `queueRequest` —
    /// and indeed `queueRequest` has no defunct branch at all (:1541-1552): a
    /// defunct port is refused as a *disabled* one, "port %s disabled". C asks
    /// `defunct` in nine places, and which dpCommon it asks is what our model
    /// deviates on: `report` (:1041), `findInterface` (:1487),
    /// `registerInterrupt` (:2424), `getInterruptPvt` (:2475),
    /// `addInterruptUser` (:2561) and `removeInterruptUser` (:2603) all read
    /// `pport->dpc.defunct`, while `enable` (:2238, so the port cannot be
    /// re-enabled), `lockPort` (:1792) and `shutdownPort`'s own idempotence
    /// check (:2268) read `findDpCommon`'s.
    ///
    /// **We model `defunct` at the port, C at whatever dpCommon the shutting-down
    /// user resolves to, and on a multi-device port that is observably
    /// different.** `asynShutdownPort` binds at addr 0
    /// (asynShellCommands.c:1331), so on an `ASYN_MULTIDEVICE` port C flags
    /// device 0 and never sets `pport->dpc.defunct` at all. Measured against
    /// libasyn at pin 731d616e, a port registered
    /// `ASYN_MULTIDEVICE|ASYN_DESTRUCTIBLE` and shut down through an addr-0
    /// user still answers `isEnabled`=1 at addr -1 and addr 1, still accepts
    /// `enable` and `lockPort` there, still hands out the interface from
    /// `findInterface` (with `drvPvt` now NULL — a latent deref, not a
    /// refusal), and `asynReport` prints the port normally instead of
    /// "destroyed". Only the single-device case, where `findDpCommon` resolves
    /// to the port's own dpc, refuses port-wide the way we always do. Taking
    /// the port down whole is deliberate: the interfaces C nulls belong to the
    /// port, so the addresses C leaves enabled are enabled onto a driver that
    /// is gone.
    ///
    /// The field is private so [`Self::shutdown_lifecycle`] is the only way to
    /// set it, which is what makes the invariant hold by construction: nothing can
    /// build a port that is defunct but still enabled, so no gate needs to ask
    /// about defunct to refuse it (R15-50).
    defunct: bool,
    /// This port's announcement channel — C `dpCommon.exceptionUserList` plus
    /// the `notifyPortThread` signal that `announceExceptionOccurred` ends with
    /// (asynManager.c:611-637). Detachable ([`Self::exception_announcer`]) so a
    /// worker thread the driver owns — the IP-server accept loop — announces
    /// through the same counter and the same list as the actor does, instead of
    /// reaching around them.
    pub(crate) announcer: ExceptionAnnouncer,
    pub options: HashMap<String, String>,
    /// The EOS terminators, keyed per device the way C keys them: an `eosPvt`
    /// is created per `asynInterposeEosConfig(portName, addr, ...)`
    /// (asynInterposeEos.c:84-120), and every EOS hook takes the `asynUser`
    /// that selects it (:288-296). Two devices on one multi-device port hold
    /// two different terminators — a single port-wide pair could not.
    ///
    /// Keyed by [`eos_device_key`], so a port that never declared
    /// `ASYN_MULTIDEVICE` collapses every addr onto one entry (C's
    /// `findDpCommon`/`findInterface` resolve any addr to the port itself).
    eos: HashMap<i32, DeviceEos>,
    pub interpose_octet: OctetInterposeStack,
    /// Trace configuration — C `dpCommon.trace`, inited by `dpCommonInit`
    /// (asynManager.c:528). Same
    /// owner as [`Self::announcer`]: bound by
    /// [`crate::services::PortServices::bind`] at port creation.
    pub(crate) trace: Option<Arc<TraceManager>>,
    /// C `octetPvt.interruptProcess` — the last argument of
    /// `pasynOctetBase->initialize` (asynOctetBase.c:161-169). When set, every
    /// successful octet read fans the data out to the port's octet interrupt
    /// users (`readIt` → `callInterruptUsers`, :224-238). The stream drivers set
    /// it — drvAsynIPPort.c:1055, drvAsynSerialPort.c:1125-1126,
    /// drvAsynSerialPortWin32.c:798-799, drvAsynFTDIPort.cpp:616 — and it is what
    /// makes a `stringin`/`waveform` with `SCAN="I/O Intr"` on such a port
    /// process at all. Parameter-cache ports (echoDriver, USBTMC, GPIB,
    /// IP-server) pass 0 and are unaffected.
    pub octet_interrupt_process: bool,
    /// Per-address device state for multi-device ports.
    pub device_states: HashMap<i32, DeviceState>,
    /// Timestamp source callback for custom timestamps.
    pub timestamp_source: Option<Arc<dyn Fn() -> SystemTime + Send + Sync>>,
    /// Port-level anchor for the 2s auto-reconnect throttle — the
    /// monotonic instant of the last connect/disconnect transition or
    /// auto-connect attempt (C `dpCommon.lastConnectDisconnect`). `None`
    /// = no transition yet, so the first attempt is always permitted.
    pub last_connect_disconnect: Option<Instant>,
    /// Deadline for the next *autonomous* connect attempt — the Rust
    /// equivalent of C's per-port `connectTimer` (`port.connectTimer`,
    /// asynManager.c:223). `None` = disarmed.
    ///
    /// Armed by [`Self::set_connected`] on a disconnect (C
    /// `exceptionDisconnect`, asynManager.c:2181-2182) and re-armed by the
    /// actor after a failed attempt; cleared on connect. The actor is what
    /// services it — see `PortActor::service_connect_timer` — so this field
    /// is the whole handoff between the transition owner and the timer.
    pub connect_retry_at: Option<Instant>,
    /// Back-off between failed autonomous connect attempts. C
    /// `port.secondsBetweenPortConnect`, initialised to
    /// `DEFAULT_SECONDS_BETWEEN_PORT_CONNECT` = 20 s (asynManager.c:48, 3249)
    /// and used to re-arm the timer at asynManager.c:3281.
    pub seconds_between_port_connect: Duration,
    /// How many times this port's link has come up — C `dpCommon.numberConnects`
    /// (asynManager.c:150), incremented by `exceptionConnect` (:2158) and printed
    /// by `asynReport` (:1057-1060). Its owner is
    /// [`Self::sync_connection_edge`], the same edge owner that raises the
    /// exception, so a connect that was never published is never counted.
    pub number_connects: u64,
}

/// The one way to announce a port exception — C `announceExceptionOccurred`
/// (asynManager.c:611-637): fan the event out over the port's exception list and
/// signal `notifyPortThread` (:635-636).
///
/// It is a detachable handle rather than a method on [`PortDriverBase`] because
/// the announcement is not the actor thread's private business: a driver-owned
/// worker (the IP-server accept loop) also transitions a device, and C's
/// `connectionListener` thread announces the same way the port thread does. A
/// clone of this handle is that capability; it carries the wake counter with it,
/// so an off-thread announcement still wakes the actor
/// ([`PortDriverBase::exceptions_announced`]).
#[derive(Clone)]
pub struct ExceptionAnnouncer {
    port_name: String,
    /// Set once by [`crate::services::PortServices::bind`] at port creation, so
    /// every clone taken afterwards carries the IOC's exception list.
    sink: Option<Arc<ExceptionManager>>,
    /// Shared so a clone's announcement is visible to the actor's count.
    announced: Arc<AtomicU64>,
}

impl ExceptionAnnouncer {
    fn new(port_name: &str) -> Self {
        Self {
            port_name: port_name.to_string(),
            sink: None,
            announced: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Announce. A port with no sink still counts the announcement — C's
    /// fan-out over an empty list still signals the port thread — so the count
    /// moves before the sink is consulted.
    pub fn announce(&self, exception: AsynException, addr: i32) {
        self.announced.fetch_add(1, Ordering::Release);
        if let Some(ref sink) = self.sink {
            sink.announce(&ExceptionEvent {
                port_name: self.port_name.clone(),
                exception,
                addr,
            });
        }
    }

    fn count(&self) -> u64 {
        self.announced.load(Ordering::Acquire)
    }
}

impl PortDriverBase {
    pub fn new(port_name: &str, max_addr: usize, flags: PortFlags) -> Self {
        Self {
            port_name: port_name.to_string(),
            max_addr: max_addr.max(1),
            flags,
            params: ParamList::new(max_addr),
            interrupts: InterruptManager::new(256),
            connected: Connection::Own(true),
            last_announced: true,
            enabled: true,
            auto_connect: true,
            defunct: false,
            announcer: ExceptionAnnouncer::new(port_name),
            options: HashMap::new(),
            eos: HashMap::new(),
            interpose_octet: OctetInterposeStack::new(flags.multi_device),
            trace: None,
            octet_interrupt_process: false,
            device_states: HashMap::new(),
            timestamp_source: None,
            last_connect_disconnect: None,
            connect_retry_at: None,
            seconds_between_port_connect: DEFAULT_SECONDS_BETWEEN_PORT_CONNECT,
            number_connects: 0,
        }
    }

    /// The EOS entry the given `asynUser` addr selects — see [`eos_device_key`].
    pub fn eos_key(&self, addr: i32) -> i32 {
        eos_device_key(self.flags.multi_device, addr)
    }

    /// This device's input EOS. An addr that has never been configured has an
    /// empty terminator, C's zero-initialised `eosPvt.eosInLen`.
    pub fn input_eos(&self, addr: i32) -> &[u8] {
        self.eos
            .get(&self.eos_key(addr))
            .map_or(&[][..], |e| &e.input)
    }

    /// This device's output EOS (see [`Self::input_eos`]).
    pub fn output_eos(&self, addr: i32) -> &[u8] {
        self.eos
            .get(&self.eos_key(addr))
            .map_or(&[][..], |e| &e.output)
    }

    /// Store this device's input terminator in the base's cache — the write
    /// companion of [`Self::input_eos`], for a driver that implements C's
    /// `asynOctet::setInputEos` itself instead of leaving it NULL for
    /// `asynInterposeEos` to serve. A multiDevice port has no choice: C
    /// refuses to install the interpose there at all (asynOctetBase.c:149-153).
    pub fn set_input_eos(&mut self, addr: i32, eos: &[u8]) {
        self.eos_entry(addr).input = eos.to_vec();
    }

    /// The output half of [`Self::set_input_eos`].
    pub fn set_output_eos(&mut self, addr: i32, eos: &[u8]) {
        self.eos_entry(addr).output = eos.to_vec();
    }

    /// The write owner for this device's terminators — the queryable cache the
    /// EOS readback (`get_input_eos`, the binary-suppress save/restore) reads.
    /// The forward to the interpose stack lives in the `PortDriver` hook and in
    /// the two public setters above.
    fn eos_entry(&mut self, addr: i32) -> &mut DeviceEos {
        let key = self.eos_key(addr);
        self.eos.entry(key).or_default()
    }

    /// Announce an exception through the global exception manager (if injected).
    ///
    /// C `announceExceptionOccurred` (asynManager.c:611-637) ends by signalling
    /// `notifyPortThread` on a CANBLOCK port (:635-636) — the announcement *is* a
    /// port-thread wake, which is why `asynEnable(port,1)` on a down port ends in
    /// a connect attempt. [`Self::exceptions_announced`] is how the actor sees
    /// that signal, so the count moves here and nowhere else: a port with no
    /// exception sink still announced (C's fan-out over an empty list still
    /// signals), so the count is bumped before the sink is even consulted.
    pub fn announce_exception(&self, exception: AsynException, addr: i32) {
        self.announcer.announce(exception, addr);
    }

    /// A clone of this port's announcement capability, for a worker thread the
    /// driver owns. See [`ExceptionAnnouncer`].
    pub fn exception_announcer(&self) -> ExceptionAnnouncer {
        self.announcer.clone()
    }

    /// Bind the IOC's exception list. [`crate::services::PortServices::bind`] is
    /// the production caller; tests that drive a driver without a runtime use it
    /// to stand in for that binding.
    pub(crate) fn bind_exception_sink(&mut self, sink: Arc<ExceptionManager>) {
        self.announcer.sink = Some(sink);
    }

    /// How many exceptions this port has announced. Monotonic; the actor compares
    /// it against the value it last saw to decide whether C would have signalled
    /// `notifyPortThread` (asynManager.c:635-636).
    pub fn exceptions_announced(&self) -> u64 {
        self.announcer.count()
    }

    /// How many callbacks are registered on this port's exception list — the
    /// `exceptionUsers` count `reportPrintPort` prints (asynManager.c:1068-1072).
    /// `asynReport` itself is the iocsh command (asynShellCommands.c:587); it
    /// reaches this through `asynManager`'s own `report` (:1136).
    pub fn exception_callback_count(&self) -> usize {
        self.announcer
            .sink
            .as_ref()
            .map_or(0, |m| m.callback_count())
    }

    /// Query whether the port is connected — the truth, wherever it lives.
    pub fn is_connected(&self) -> bool {
        self.connected.get()
    }

    /// The port's initial connection state, set while it is being constructed and
    /// before it can have a listener. Not a transition: no exception fan-out, no
    /// retry timer, no `lastConnectDisconnect` stamp. Every *transition* after
    /// construction goes through [`Self::set_connected`].
    ///
    /// A port whose link is owned elsewhere has no initial state of its own to
    /// set — the owner's cell already holds it — so this is a no-op there rather
    /// than a silent overwrite of the owner's truth.
    pub fn init_connected(&mut self, connected: bool) {
        if let Connection::Own(c) = &mut self.connected {
            *c = connected;
            self.last_announced = connected;
        }
    }

    /// Bind this port's connection to a cell owned by another object, making that
    /// cell the port's truth from now on — see [`Connection::Shared`]. Called at
    /// construction by a port that serves someone else's link (the IP-server
    /// child port and its `ClientSlot`).
    pub(crate) fn share_connection(&mut self, cell: Arc<AtomicBool>) {
        self.last_announced = cell.load(Ordering::Acquire);
        self.connected = Connection::Shared(cell);
    }

    /// C `asynPrint(pasynUserSelf, mask, ...)` — the port's own diagnostic
    /// channel, gated by this port's trace mask and routed to this port's
    /// trace file.
    ///
    /// Public because the drivers that need it are not all in this crate. C's
    /// drivers are separate modules too (`drvModbusAsyn.cpp`,
    /// `drvAsynIPPort.c`) and every one of them reports a fault it cannot
    /// return — a poller has no caller to return to — by `asynPrint`ing it at
    /// `ASYN_TRACE_ERROR`. With the [`TraceManager`] reachable only inside
    /// this crate, an out-of-crate driver had no way to say anything at all,
    /// and the failures it could not return became silent.
    ///
    /// A port whose services were never bound has no trace manager and the
    /// call is a no-op; that is the same port that has no trace file to write
    /// to and no mask to consult.
    pub fn trace_print(&self, mask: crate::trace::TraceMask, msg: &str) {
        if let Some(trace) = &self.trace {
            trace.output(&self.port_name, mask, msg);
        }
    }

    /// Single owner-API for the port-level `connected` transition.
    ///
    /// C parity: `exceptionConnect` (asynManager.c:2151-2160) and
    /// `exceptionDisconnect` (:2174-2185) fire
    /// `asynExceptionConnect` only when the state actually changes.
    /// All driver code that toggles connection state MUST go through
    /// this helper — the `connected` cell is private precisely so that a driver
    /// cannot assign it and then hand-roll an `announce_exception(Connect, -1)`,
    /// which bypasses the edge guard and fans spurious duplicates out to
    /// listeners (CA gateway shadow tasks, asynRecord, monitor relays).
    ///
    /// On a port whose link is owned elsewhere (`Connection::Shared`) the write
    /// is not this port's to make — the owner already moved the truth — so the
    /// call reduces to publishing whatever edge that produced.
    ///
    /// Returns `true` if the state actually changed (a fan-out
    /// happened); `false` if the call was a no-op.
    pub fn set_connected(&mut self, connected: bool) -> bool {
        if let Connection::Own(c) = &mut self.connected {
            *c = connected;
        }
        self.sync_connection_edge()
    }

    /// Publish the port's connection edge if the truth has moved since the last
    /// fan-out: the single owner of `exceptionConnect` (asynManager.c:2142-2161)
    /// and `exceptionDisconnect` (:2165-2188), of the interpose stack's connection
    /// reset and of the retry timer.
    ///
    /// [`Self::set_connected`] is one caller. The other is the actor, on a port
    /// whose link is owned elsewhere: the owner (an IP-server listener assigning a
    /// slot) moves the truth without this port's actor running, and C fans that
    /// edge out from the owner's thread — `pasynCommonSyncIO->connectDevice` on
    /// the child (drvAsynIPServerPort.c:358-360). Here it is published when the
    /// child's actor next touches the port, which is the moment it can matter.
    ///
    /// Returns `true` if an edge was published.
    pub fn sync_connection_edge(&mut self) -> bool {
        let connected = self.connected.get();
        if self.last_announced == connected {
            return false;
        }
        self.last_announced = connected;
        if !connected {
            // C `exceptionDisconnect` stamps `lastConnectDisconnect` on
            // every disconnect (asynManager.c:2184) so the auto-reconnect
            // throttle measures from the moment the link dropped.
            self.last_connect_disconnect = Some(Instant::now());
            // ...and arms the port's connect timer at .01 s when the port is
            // auto-connect (asynManager.c:2181-2182), which is what makes the
            // reconnect *autonomous*: it does not wait for queued traffic.
            if self.auto_connect {
                self.connect_retry_at = Some(Instant::now() + CONNECT_RETRY_INITIAL);
            }
        } else {
            // C `exceptionConnect` counts the connects it publishes
            // (`++pdpCommon->numberConnects`, asynManager.c:2158) — the count
            // `asynReport` prints, and the operator's only way to see a port that
            // is flapping. It belongs to this owner because C increments it in the
            // same function that raises the exception, so a connect that never
            // fanned out is never counted.
            self.number_connects += 1;
            // The link is up — nothing left to retry. (C leaves the timer
            // running and lets `portConnectTimerCallback` no-op on the
            // `!connected` guard, asynManager.c:3258; disarming here is the
            // same observable behaviour without the pointless wakeup.)
            self.connect_retry_at = None;
        }
        // The interpose stack is a subscriber of this transition, exactly as
        // in C: `asynInterposeEos` registers an exception callback
        // (asynInterposeEos.c:110) and drops its read-ahead buffer +
        // partial-EOS match on `asynExceptionConnect`
        // (asynInterposeEos.c:142-151). Both C edges — `exceptionConnect`
        // (asynManager.c:2158) and `exceptionDisconnect` (asynManager.c:2185)
        // — raise that same exception, so both edges reset here. Driving the
        // hook from this owner (rather than from an out-of-band subscriber)
        // keeps it impossible to change `connected` without the stack
        // hearing about it: `interpose_octet` and `connected` live in the
        // same struct behind the same lock.
        self.interpose_octet.connection_changed();
        self.announce_exception(AsynException::Connect, -1);
        true
    }

    /// Per-address variant — for multi-device ports. Same edge
    /// guarantee as [`Self::set_connected`].
    ///
    /// Deliberately does *not* reset the interpose stack. In C each
    /// interpose is installed on one (port, addr) pair and registers its
    /// exception callback on that address's `dpCommon`, so a device-level
    /// connect exception only resets *that* device's interpose
    /// (asynManager.c:611-625 fans out per-`dpCommon`). `interpose_octet`
    /// here is port-scoped, so clearing it from a per-device transition
    /// would discard read-ahead belonging to the port's other addresses.
    /// The port-level transition owner [`Self::set_connected`] carries the
    /// reset.
    pub fn set_addr_connected(&mut self, addr: i32, connected: bool) -> bool {
        // C reaches both levels through one `exceptionConnect`, and
        // `findDpCommon` is what decides which: an addr the port resolves to
        // its own `dpCommon` is a PORT transition, and the port owner carries
        // the interpose reset and the retry timer that belong to it.
        let Some(addr) = self.dp_addr(addr) else {
            return self.set_connected(connected);
        };
        let was = self.device_state(addr).connected;
        if was == connected {
            return false;
        }
        self.device_state(addr).connected = connected;
        if !connected {
            // Per-device disconnect stamp — same throttle anchor as the
            // port-level path (C `exceptionDisconnect`, asynManager.c:2184).
            self.device_state(addr).last_connect_disconnect = Some(Instant::now());
        }
        self.announce_exception(AsynException::Connect, addr);
        true
    }

    /// 2.0s auto-reconnect throttle gate — C `autoConnectDevice`
    /// (asynManager.c:712-713, 729-730).
    ///
    /// Returns `true` when a fresh auto-connect attempt is permitted:
    /// either no transition has been recorded yet (mirrors C's
    /// zero-initialised `lastConnectDisconnect`, whose diff against `now`
    /// is effectively infinite), or at least `AUTO_CONNECT_THROTTLE` has
    /// elapsed since the last transition or attempt. A disconnected
    /// `auto_connect` device that just dropped — or whose previous
    /// reconnect just failed — is refused until the window passes, so a
    /// burst of N queued requests triggers at most one full connect
    /// attempt per window instead of N back-to-back attempts.
    ///
    /// Uses monotonic [`Instant`], not wall clock: the throttle is purely
    /// internal timing, never serialised, so it must be immune to NTP
    /// steps. `addr` selects the anchor via [`Self::is_device_addr`].
    pub fn auto_connect_throttle_ok(&self, addr: i32, now: Instant) -> bool {
        let last = if self.is_device_addr(addr) {
            self.device_states
                .get(&addr)
                .and_then(|d| d.last_connect_disconnect)
        } else {
            self.last_connect_disconnect
        };
        match last {
            None => true,
            Some(t) => now.saturating_duration_since(t) >= AUTO_CONNECT_THROTTLE,
        }
    }

    /// Does `addr` name a *device* on this port, or the port itself?
    ///
    /// C `findDpCommon` resolves a `pasynUser` to `&pdevice->dpc` only when
    /// the port is multi-device AND the user is bound to a real address;
    /// otherwise to `&pport->dpc` (a connectDevice with `addr < 0` leaves
    /// `pdevice` null). This is the one owner of that resolution, so the
    /// throttle read [`Self::auto_connect_throttle_ok`] and the throttle
    /// write [`Self::stamp_auto_connect_attempt`] can never disagree about
    /// which anchor an address refers to. Keying on the address (rather than
    /// on `multi_device` alone) is what lets a multi-device port hold a
    /// *port-level* anchor at `addr = -1`: the old form sent `-1` into
    /// `device_states`, inventing a phantom device whose anchor no
    /// disconnect ever stamped.
    pub fn is_device_addr(&self, addr: i32) -> bool {
        self.dp_addr(addr).is_some()
    }

    /// The dpCommon slot this port resolves `addr` to — [`dp_common_key`]
    /// against the port's own `ASYN_MULTIDEVICE` attribute. The single owner
    /// of the resolution for every connection-state read and write below.
    pub fn dp_addr(&self, addr: i32) -> Option<i32> {
        dp_common_key(self.flags.multi_device, addr)
    }

    /// C `findDpCommon(puserPvt)->enabled` (asynManager.c:2352) — what
    /// `pasynManager->isEnabled` answers for this address.
    pub fn dp_enabled(&self, addr: i32) -> bool {
        self.device(addr).map_or(self.enabled, |ds| ds.enabled)
    }

    /// C `findDpCommon(puserPvt)->connected` (asynManager.c:2340) — what
    /// `pasynManager->isConnected` answers for this address.
    pub fn dp_connected(&self, addr: i32) -> bool {
        self.device(addr)
            .map_or_else(|| self.is_connected(), |ds| ds.connected)
    }

    /// C `findDpCommon(puserPvt)->autoConnect` (asynManager.c:2370) — what
    /// `pasynManager->isAutoConnect` answers for this address.
    pub fn dp_auto_connect(&self, addr: i32) -> bool {
        self.device(addr)
            .map_or(self.auto_connect, |ds| ds.auto_connect)
    }

    /// Single owner for the post-attempt throttle stamp. C
    /// `autoConnectDevice` stamps `lastConnectDisconnect` immediately after
    /// every `connectAttempt`, success or failure (asynManager.c:718,
    /// 735), so the window restarts from the end of the attempt — a failed
    /// reconnect is not retried until the throttle elapses again.
    pub fn stamp_auto_connect_attempt(&mut self, addr: i32, now: Instant) {
        if self.is_device_addr(addr) {
            self.device_state(addr).last_connect_disconnect = Some(now);
        } else {
            self.last_connect_disconnect = Some(now);
        }
    }

    /// Query whether the port is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Single owner-API for the port-level `enabled` transition.
    ///
    /// C `enable` (asynManager.c:2222-2249) refuses a shut-down port:
    /// when `defunct` it returns `asynDisabled` *without* touching
    /// `enabled` and *without* firing the `asynExceptionEnable` fan-out.
    /// Otherwise it sets `enabled` and announces unconditionally (no
    /// state-change guard). The actor's `SetEnable` op is a lifecycle op
    /// that bypasses [`Self::check_ready`], so this guard is the only
    /// thing that stops a defunct port from being re-enabled or fanning
    /// out a spurious exception — it must live here, in the one owner of
    /// the transition.
    pub fn set_enabled(&mut self, enabled: bool) -> AsynResult<()> {
        if self.defunct {
            return Err(Self::shut_down_error());
        }
        self.enabled = enabled;
        self.announce_exception(AsynException::Enable, -1);
        Ok(())
    }

    /// Per-address variant — same defunct refusal as [`Self::set_enabled`].
    /// `defunct` is modelled at the port level (a shut-down port takes its
    /// devices with it), so a defunct port refuses per-device enable/disable
    /// too, matching C's `dpCommon.defunct` check on the resolved device.
    pub fn set_addr_enabled(&mut self, addr: i32, enabled: bool) -> AsynResult<()> {
        // C `enable` writes `findDpCommon(puserPvt)->enabled` (asynManager.c:2245),
        // so an addr that resolves to the port's own slot is the port's enable —
        // not a device entry no reader would ever resolve to.
        let Some(addr) = self.dp_addr(addr) else {
            return self.set_enabled(enabled);
        };
        if self.defunct {
            return Err(Self::shut_down_error());
        }
        self.device_state(addr).enabled = enabled;
        self.announce_exception(AsynException::Enable, addr);
        Ok(())
    }

    /// Query whether auto-connect is enabled.
    pub fn is_auto_connect(&self) -> bool {
        self.auto_connect
    }

    /// Toggle the auto-connect flag at runtime.
    ///
    /// C parity: `autoConnectAsyn` (asynManager.c:2310-2324) always
    /// fires `asynExceptionAutoConnect` regardless of prior state
    /// (no state-change guard). Mirror that — every call announces.
    /// Driver constructors that initialise `base.auto_connect`
    /// directly during `PortDriver::new()` keep the silent path
    /// (the port is not yet registered, so no listeners exist).
    pub fn set_auto_connect(&mut self, yes: bool) {
        self.auto_connect = yes;
        // A DEVIATION from the pinned C, adopting asyn PR #217
        // (`87918f5e06b7`, "Queue connect timer on autoconnect enable"):
        // flipping auto-connect ON while the port is down starts the connect
        // timer, so the flip alone brings the port up. That PR is UNMERGED —
        // it is on no upstream branch, so `autoConnectAsyn` at the pin
        // (asynManager.c:2310-2324) sets `autoConnect` and announces, and
        // nothing else; its `:2322-2324` are `exceptionOccurred` and the
        // return. Stock asyn's only autonomous attempt is the one
        // exception-wake pass — one try, then silence until traffic or a
        // disconnect edge arms the timer (:2181-2182).
        if yes && !self.connected.get() {
            self.connect_retry_at = Some(Instant::now() + CONNECT_RETRY_INITIAL);
        }
        self.announce_exception(AsynException::AutoConnect, -1);
    }

    /// Per-address variant — for multi-device ports. C parity:
    /// `autoConnectAsyn` walks dpCommon via findDpCommon so a per-
    /// device pasynUser hits the device's dpc, otherwise the port's
    /// dpc (asynManager.c:2314 + findDpCommon).
    pub fn set_auto_connect_addr(&mut self, addr: i32, yes: bool) {
        // Same `findDpCommon` split as [`Self::set_addr_enabled`]
        // (asynManager.c:2314).
        let Some(addr) = self.dp_addr(addr) else {
            self.set_auto_connect(yes);
            return;
        };
        let device_down = {
            let dev = self.device_state(addr);
            dev.auto_connect = yes;
            !dev.connected
        };
        // Same PR #217 deviation as `set_auto_connect`, and the PR keys its
        // check on the dpCommon `findDpCommon` resolved — the DEVICE's —
        // while the timer it starts is the port's.
        if yes && device_down {
            self.connect_retry_at = Some(Instant::now() + CONNECT_RETRY_INITIAL);
        }
        self.announce_exception(AsynException::AutoConnect, addr);
    }

    /// Query whether the port has been marked defunct via
    /// [`Self::shutdown_lifecycle`] — once true the port is gone for
    /// good, mirroring C asynManager.c:2266-2269.
    pub fn is_defunct(&self) -> bool {
        self.defunct
    }

    /// The one place a shut-down port is named in an error message, and the
    /// only one C has: `enable` on a defunct device (asynManager.c:2236-2241).
    /// Every other gate refuses a defunct port as a disabled one.
    fn shut_down_error() -> AsynError {
        AsynError::Status {
            status: AsynStatus::Disabled,
            message: "asynManager:enable: port has been shut down".to_string(),
        }
    }

    /// C `queueRequest`'s gate (asynManager.c:1541-1552), and the single owner
    /// of the two refusals it is built from.
    ///
    /// They are independent, and the [`ConnectCheck`] the caller hands in
    /// selects between them — it can waive the *connected* refusal and nothing
    /// else. There is no argument, op class or priority that waives
    /// [`Self::check_enabled`]: C's `if(!pport->dpc.enabled) return asynDisabled`
    /// (:1541-1546) sits *above* `checkPortConnect` and is reached by every
    /// request, and its port thread refuses to run anything at all on a disabled
    /// port (`portThread`, :806-809).
    ///
    /// The `ConnectCheck` can only come from [`AsynUser::connect_check`], so the
    /// waiver is available exactly to the requests C gives it to.
    ///
    /// Its refusal is [`AsynError::QueueRefused`], not [`AsynError::Status`]:
    /// C's refusal is `queueRequest`'s *return value*, so the callback never
    /// runs and nothing it implies happened. A caller must be able to tell that
    /// from a driver error raised *inside* a callback that did run — the record
    /// writes the refusal to ERRS and stops, where a driver error still gets the
    /// callback's readback and `monitorStatus` tail (asynRecord.c:571-578 vs
    /// :788-900). This is the only place that stamps it.
    pub fn check_queue(&self, addr: i32, connect: ConnectCheck) -> AsynResult<()> {
        self.check_queue_inner(addr, connect)
            .map_err(AsynError::into_queue_refusal)
    }

    /// The gate's body, before the refusal is stamped as a queue refusal.
    ///
    /// Three blocks, in C's order:
    ///
    /// 1. `!pport->dpc.enabled → asynDisabled` (:1541-1546) — port level,
    ///    unconditional, reached by every request.
    /// 2. `checkPortConnect && !pport->dpc.connected → asynDisconnected`
    ///    (:1547-1552) — port level, waived by the request's own user. (C's
    ///    `checkPortConnect == FALSE` reads *no* connected flag: the port thread
    ///    drains the Connect queue before it ever calls `autoConnectDevice`,
    ///    :811-855.)
    /// 3. The **device** block (:1553-1575), which C runs only for a
    ///    *synchronous* port: it sits bodily inside
    ///    `if(!(pport->attributes & ASYN_CANBLOCK))` at :1553. A CANBLOCK port's
    ///    device-level enabled/connected checks belong to the port THREAD
    ///    (:875-886), which does something else entirely with them — a disabled
    ///    device's request *waits in the queue* (`continue`), it is not refused
    ///    — so applying them here refused requests C parks (R15-47). Every real
    ///    transport port is CANBLOCK.
    fn check_queue_inner(&self, addr: i32, connect: ConnectCheck) -> AsynResult<()> {
        self.check_enabled()?;
        if connect == ConnectCheck::Required {
            self.check_port_connected()?;
        }
        if !self.flags.can_block {
            // C :1561-1567 — the device-enabled refusal in the synchronous block
            // is *not* conditioned on `checkPortConnect`; only the connected one
            // below is (:1568).
            self.check_device_enabled(addr)?;
            if connect == ConnectCheck::Required {
                self.check_device_connected(addr)?;
            }
        }
        Ok(())
    }

    /// The unconditional half of the queue gate: a disabled port refuses every
    /// request (asynManager.c:1541-1546).
    ///
    /// There is no defunct branch here, and there is none in C's `queueRequest`
    /// either: `shutdownPort` clears `enabled` alongside setting `defunct`
    /// (:2283-2284) precisely so this one check answers for both. The invariant
    /// `defunct ⟹ !enabled` (see the field doc) is what makes that sound.
    pub fn check_enabled(&self) -> AsynResult<()> {
        if !self.enabled {
            return Err(AsynError::Status {
                status: AsynStatus::Disabled,
                message: format!("port {} disabled", self.port_name),
            });
        }
        Ok(())
    }

    /// The port-level connected refusal (asynManager.c:1547-1552).
    pub fn check_port_connected(&self) -> AsynResult<()> {
        if !self.is_connected() {
            return Err(AsynError::Status {
                status: AsynStatus::Disconnected,
                message: format!("port {} not connected", self.port_name),
            });
        }
        Ok(())
    }

    /// The device-level enabled refusal, C's text verbatim
    /// (asynManager.c:1561-1567). Its *use* differs by port class, which is why
    /// it is a check and not a gate: `queueRequest` returns it on a synchronous
    /// port, while on a CANBLOCK port the same condition makes `portThread` park
    /// the request instead (:875). Both callers ask this one function.
    ///
    /// A port that is not `ASYN_MULTIDEVICE`, or an address with no device state,
    /// resolves to the port's own `dpCommon` in C's `findDpCommon` — already
    /// checked above — so there is nothing device-level left to refuse.
    pub fn check_device_enabled(&self, addr: i32) -> AsynResult<()> {
        if let Some(ds) = self.device(addr) {
            if !ds.enabled {
                return Err(AsynError::Status {
                    status: AsynStatus::Disabled,
                    // C's double space is verbatim (asynManager.c:1564).
                    message: format!("port {}  or device {} not enabled", self.port_name, addr),
                });
            }
        }
        Ok(())
    }

    /// The device-level connected refusal (asynManager.c:1568-1575). Same
    /// split as [`Self::check_device_enabled`]: `queueRequest` returns it on a
    /// synchronous port; on a CANBLOCK port `portThread` answers the same
    /// condition with the request's timeout callback (:884-885).
    pub fn check_device_connected(&self, addr: i32) -> AsynResult<()> {
        if let Some(ds) = self.device(addr) {
            if !ds.connected {
                return Err(AsynError::Status {
                    status: AsynStatus::Disconnected,
                    message: format!("port {} or device {} not connected", self.port_name, addr),
                });
            }
        }
        Ok(())
    }

    /// The device `addr` resolves to, or `None` when C's `findDpCommon` would
    /// resolve it to the port's own `dpCommon` (not multi-device, or no device
    /// created at that address).
    fn device(&self, addr: i32) -> Option<&DeviceState> {
        self.dp_addr(addr).and_then(|a| self.device_states.get(&a))
    }

    /// Check that the port is enabled, connected, and not defunct.
    /// Returns `Err(Disabled)`, `Err(Disconnected)`, or `Err(Disabled)`
    /// (defunct => permanently disabled) otherwise.
    pub fn check_ready(&self) -> AsynResult<()> {
        self.check_enabled()?;
        self.check_port_connected()
    }

    /// Run the C `shutdownPort` lifecycle (asynManager.c:2251-2308):
    ///
    /// 1. Refuse if the port did not opt into `ASYN_DESTRUCTIBLE`
    ///    (returns `Err(Status::Error)`).
    /// 2. Short-circuit if already defunct (idempotent — returns Ok).
    /// 3. Set `enabled = false`, `defunct = true` — every subsequent
    ///    request through [`Self::check_ready`] fails.
    /// 4. Broadcast `AsynException::Shutdown` so registered observers
    ///    (CA gateways, monitor sinks) tear down their handles.
    ///
    /// Drivers should call this from their own shutdown plumbing and
    /// then release any hardware-owned resources via their
    /// [`PortDriver::shutdown`] implementation. Callers from outside
    /// the runtime can drive the same lifecycle via
    /// [`crate::manager::PortManager::shutdown_port`].
    pub fn shutdown_lifecycle(&mut self) -> AsynResult<()> {
        if self.defunct {
            // Idempotent — C asynManager.c:2266-2269 returns asynSuccess.
            return Ok(());
        }
        if !self.flags.destructible {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!(
                    "port {} does not support shutting down (ASYN_DESTRUCTIBLE not set)",
                    self.port_name
                ),
            });
        }
        self.enabled = false;
        // The invariant is `defunct ⟹ !enabled` on *every* dpCommon this port
        // owns, not only the port's own. Clearing the device slots here is what
        // makes it hold by construction: [`Self::device_state`] seeds a new slot
        // from the port's now-false `enabled`, and [`Self::set_addr_enabled`]
        // refuses while defunct, so nothing can raise a slot back — and
        // [`Self::dp_enabled`] needs no defunct branch to answer correctly.
        // Without this, a slot that happened to exist before the shutdown kept
        // `enabled = true`, so `isEnabled(ADDR)` answered true on a port that is
        // gone while `isEnabled(-1)` answered false — the same read disagreeing
        // with itself depending on whether anything had ever touched that
        // address.
        for ds in self.device_states.values_mut() {
            ds.enabled = false;
        }
        self.defunct = true;
        self.announce_exception(AsynException::Shutdown, -1);
        Ok(())
    }

    /// Check that port + device address are both ready — the whole of C's
    /// synchronous-port gate (asynManager.c:1541-1575) in one call. Drivers that
    /// re-check inside their own I/O use it; the queue gate reaches the same
    /// checks through [`Self::check_queue`], which splits them by port class.
    pub fn check_ready_addr(&self, addr: i32) -> AsynResult<()> {
        self.check_ready()?;
        self.check_device_enabled(addr)?;
        self.check_device_connected(addr)
    }

    /// Get or create a device state for the given address.
    ///
    /// A device created here inherits the port's own `dpCommon`, as C's
    /// `locateDevice` does: `dpCommonInit(pport, pdevice, pport->dpc.autoConnect)`
    /// (asynManager.c:586). Defaulting `auto_connect` to `true` on a
    /// manual-connect port would make `asynReport`'s per-device
    /// `autoConnect Yes` a lie, and would hand `autoConnectDevice` a device it
    /// may reconnect on a port whose operator turned auto-connect off.
    ///
    /// `enabled` and `connected` are seeded the same way, and for a stronger
    /// reason: until this slot exists the reads [`Self::dp_enabled`] /
    /// [`Self::dp_connected`] answer from the port's own, so seeding from
    /// anything else would let an unrelated write — disabling a device on a
    /// disconnected port, say — flip what a *different* field of that device
    /// reports. The slot's creation is then invisible: every field starts at
    /// the value the read it replaces was already giving.
    /// `pub(crate)`, not `pub`: this hands out the raw slot, so a caller that
    /// wrote through it would skip the `asynException` fan-out C raises from
    /// `enable` / `exceptionConnect` (asynManager.c:2247, 2158) *and* could
    /// create a device on a port that has none — a state C cannot represent,
    /// since `locateDevice` refuses to allocate one unless
    /// `attributes&ASYN_MULTIDEVICE` (:576). Outside callers use the three
    /// owners [`Self::set_addr_connected`], [`Self::set_addr_enabled`] and
    /// [`Self::set_auto_connect_addr`], each of which resolves the addr first.
    pub(crate) fn device_state(&mut self, addr: i32) -> &mut DeviceState {
        let seed = DeviceState {
            connected: self.is_connected(),
            enabled: self.enabled,
            auto_connect: self.auto_connect,
            last_connect_disconnect: None,
        };
        self.device_states.entry(addr).or_insert(seed)
    }

    /// Check if a specific device address is connected.
    pub fn is_device_connected(&self, addr: i32) -> bool {
        self.dp_connected(addr)
    }

    /// Set a specific device address as connected.
    ///
    /// C parity: announce only on actual transition
    /// (asynManager.c:2151-2160 — `exceptionConnect` rejects
    /// already-connected; we keep an Ok return for idempotency but
    /// suppress the duplicate fan-out so subscribers don't see
    /// spurious connect events). Thin wrapper over
    /// [`Self::set_addr_connected`] for callers that prefer the
    /// directional verb.
    pub fn connect_addr(&mut self, addr: i32) {
        self.set_addr_connected(addr, true);
    }

    /// Set a specific device address as disconnected.
    ///
    /// C parity: announce only on actual transition
    /// (asynManager.c:2174-2185). Thin wrapper over
    /// [`Self::set_addr_connected`].
    pub fn disconnect_addr(&mut self, addr: i32) {
        self.set_addr_connected(addr, false);
    }

    /// Enable a specific device address. Convenience facade over the
    /// guarded owner [`Self::set_addr_enabled`]; a defunct port no-ops.
    pub fn enable_addr(&mut self, addr: i32) {
        let _ = self.set_addr_enabled(addr, true);
    }

    /// Disable a specific device address. Convenience facade over the
    /// guarded owner [`Self::set_addr_enabled`]; a defunct port no-ops.
    pub fn disable_addr(&mut self, addr: i32) {
        let _ = self.set_addr_enabled(addr, false);
    }

    /// Set a custom timestamp source callback.
    pub fn register_timestamp_source<F>(&mut self, source: F)
    where
        F: Fn() -> SystemTime + Send + Sync + 'static,
    {
        self.timestamp_source = Some(Arc::new(source));
    }

    /// Drop a registered source — C `unregisterTimeStampSource`
    /// (asynManager.c:334), which restores `defaultTimeStampSource` (:332).
    pub fn unregister_timestamp_source(&mut self) {
        self.timestamp_source = None;
    }

    /// Get current timestamp from the registered source, or SystemTime::now().
    pub fn current_timestamp(&self) -> SystemTime {
        self.timestamp_source
            .as_ref()
            .map_or_else(SystemTime::now, |f| f())
    }

    pub fn create_param(&mut self, name: &str, param_type: ParamType) -> AsynResult<usize> {
        self.params.create_param(name, param_type)
    }

    pub fn find_param(&self, name: &str) -> Option<usize> {
        self.params.find_param(name)
    }

    // --- Convenience param accessors ---

    pub fn set_int32_param(&mut self, index: usize, addr: i32, value: i32) -> AsynResult<()> {
        self.params.set_int32(index, addr, value)
    }

    pub fn get_int32_param(&self, index: usize, addr: i32) -> AsynResult<i32> {
        self.params.get_int32(index, addr)
    }

    /// Strict variant — returns [`AsynError::ParamUndefined`] when the
    /// cache entry has never been set (C parity for `asynParamUndefined`).
    /// See [`crate::param::ParamList::get_int32_strict`].
    pub fn get_int32_param_strict(&self, index: usize, addr: i32) -> AsynResult<i32> {
        self.params.get_int32_strict(index, addr)
    }

    pub fn set_int64_param(&mut self, index: usize, addr: i32, value: i64) -> AsynResult<()> {
        self.params.set_int64(index, addr, value)
    }

    pub fn get_int64_param(&self, index: usize, addr: i32) -> AsynResult<i64> {
        self.params.get_int64(index, addr)
    }

    /// Strict variant — see [`crate::param::ParamList::get_int64_strict`].
    pub fn get_int64_param_strict(&self, index: usize, addr: i32) -> AsynResult<i64> {
        self.params.get_int64_strict(index, addr)
    }

    pub fn set_float64_param(&mut self, index: usize, addr: i32, value: f64) -> AsynResult<()> {
        self.params.set_float64(index, addr, value)
    }

    pub fn get_float64_param(&self, index: usize, addr: i32) -> AsynResult<f64> {
        self.params.get_float64(index, addr)
    }

    /// Strict variant — see [`crate::param::ParamList::get_float64_strict`].
    pub fn get_float64_param_strict(&self, index: usize, addr: i32) -> AsynResult<f64> {
        self.params.get_float64_strict(index, addr)
    }

    pub fn set_string_param(
        &mut self,
        index: usize,
        addr: i32,
        value: impl Into<Vec<u8>>,
    ) -> AsynResult<()> {
        self.params.set_string(index, addr, value)
    }

    pub fn get_string_param(&self, index: usize, addr: i32) -> AsynResult<&[u8]> {
        self.params.get_string(index, addr)
    }

    /// Strict variant — see [`crate::param::ParamList::get_string_strict`].
    pub fn get_string_param_strict(&self, index: usize, addr: i32) -> AsynResult<&[u8]> {
        self.params.get_string_strict(index, addr)
    }

    /// Set a UInt32Digital parameter. `interrupt_mask` mirrors C
    /// `setUIntDigitalParam(.., interruptMask)` (asynPortDriver.cpp:1369,
    /// 1381): bits to force into the I/O Intr callback mask even when the
    /// stored value did not change. Pass `0` for a plain value set (the
    /// 3-arg C overload, asynPortDriver.cpp:1347).
    pub fn set_uint32_param(
        &mut self,
        index: usize,
        addr: i32,
        value: u32,
        mask: u32,
        interrupt_mask: u32,
    ) -> AsynResult<()> {
        self.params
            .set_uint32(index, addr, value, mask, interrupt_mask)
    }

    pub fn get_uint32_param(&self, index: usize, addr: i32) -> AsynResult<u32> {
        self.params.get_uint32(index, addr)
    }

    /// Strict variant — see [`crate::param::ParamList::get_uint32_strict`].
    pub fn get_uint32_param_strict(&self, index: usize, addr: i32) -> AsynResult<u32> {
        self.params.get_uint32_strict(index, addr)
    }

    pub fn get_enum_param(&self, index: usize, addr: i32) -> AsynResult<(usize, Arc<[EnumEntry]>)> {
        self.params.get_enum(index, addr)
    }

    pub fn set_enum_index_param(
        &mut self,
        index: usize,
        addr: i32,
        value: usize,
    ) -> AsynResult<()> {
        self.params.set_enum_index(index, addr, value)
    }

    pub fn set_enum_choices_param(
        &mut self,
        index: usize,
        addr: i32,
        choices: Arc<[EnumEntry]>,
    ) -> AsynResult<()> {
        self.params.set_enum_choices(index, addr, choices)
    }

    pub fn get_generic_pointer_param(
        &self,
        index: usize,
        addr: i32,
    ) -> AsynResult<Arc<dyn Any + Send + Sync>> {
        self.params.get_generic_pointer(index, addr)
    }

    pub fn set_generic_pointer_param(
        &mut self,
        index: usize,
        addr: i32,
        value: Arc<dyn Any + Send + Sync>,
    ) -> AsynResult<()> {
        self.params.set_generic_pointer(index, addr, value)
    }

    pub fn set_param_timestamp(
        &mut self,
        index: usize,
        addr: i32,
        ts: SystemTime,
    ) -> AsynResult<()> {
        self.params.set_timestamp(index, addr, ts)
    }

    pub fn set_param_status(
        &mut self,
        index: usize,
        addr: i32,
        status: AsynStatus,
        alarm_status: u16,
        alarm_severity: u16,
    ) -> AsynResult<()> {
        self.params
            .set_param_status(index, addr, status, alarm_status, alarm_severity)
    }

    pub fn get_param_status(&self, index: usize, addr: i32) -> AsynResult<(AsynStatus, u16, u16)> {
        self.params.get_param_status(index, addr)
    }

    /// C `asynPortDriver::reportParams` (asynPortDriver.cpp:1799-1809) — the
    /// parameter block of the driver's report.
    ///
    /// `details` is the level `report` was called with, **unshifted**: C hands
    /// `reportParams` the same number it got (:3693), and the level decides one
    /// thing only — how many address lists are printed (`details >= 2` → all
    /// `maxAddr` of them, else list 0 alone, :1804). The values are *not* a
    /// deeper level: `paramVal::report` prints name, type, value and status for
    /// every parameter at every level (paramVal.cpp:296-372).
    ///
    /// Passing `level - 1` here put the whole block one level late — `asynReport
    /// 1` printed a bare count where C prints every parameter with its value, and
    /// values only appeared at 3 (R16-46/47).
    pub fn report_params(&self, out: &mut dyn std::fmt::Write, details: i32) {
        use std::fmt::Write as _;
        let num_addr = if details >= 2 {
            self.max_addr.max(1)
        } else {
            1
        };
        for addr in 0..num_addr {
            let _ = writeln!(out, "Parameter list {addr}");
            self.params.report(out, addr as i32);
        }
    }

    /// Push an interpose layer onto the **port's** octet I/O stack — C
    /// `interposeInterface(portName, -1, ...)`, which every driver's own
    /// configure-time install is (the layer serves every device on the port).
    ///
    /// **Concurrency**: requires `&mut self`, which means the caller must hold
    /// the port lock (`Arc<Mutex<dyn PortDriver>>`). This ensures
    /// interpose modifications are serialized with I/O dispatch.
    pub fn install_octet_interpose(&mut self, layer: Box<dyn OctetInterpose>) {
        self.install_octet_interpose_addr(crate::interpose::PORT_CHAIN, layer);
    }

    /// Push an interpose layer onto the stack of the device `addr` names — C
    /// `interposeInterface(portName, addr, ...)` (asynManager.c:2190-2220), which
    /// is what the `asynInterposeEcho` / `asynInterposeDelay` iocsh commands call
    /// (asynInterposeEcho.c:176, asynInterposeDelay.c:187,200). On a port that is
    /// not multi-device every addr resolves to the port itself.
    pub fn install_octet_interpose_addr(&mut self, addr: i32, layer: Box<dyn OctetInterpose>) {
        self.interpose_octet.install(addr, layer);
    }

    /// Flush changed parameters as interrupt notifications.
    /// Equivalent to C asyn's callParamCallbacks().
    pub fn call_param_callbacks(&mut self, addr: i32) -> AsynResult<()> {
        let changed = self.params.take_changed(addr)?;
        let now = self.current_timestamp();
        for reason in changed {
            let value = self.params.get_value(reason, addr)?.clone();
            // C asynPortDriver.cpp:845 — callCallbacks skips firing for an
            // undefined param even though its changed flag is consumed
            // (flags.clear() at :871). A status/alarm change or bare
            // mark_changed on a never-set scalar must not emit an I/O Intr.
            // Array/generic-pointer params have no callCallbacks analog
            // (:846-865 switch is scalar-only) and Rust fires them as a
            // read-trigger regardless, so gate scalars only.
            if !value.is_array() && !self.params.is_param_defined(reason, addr).unwrap_or(false) {
                continue;
            }
            let ts = self.params.get_timestamp(reason, addr)?.unwrap_or(now);
            // C parity: read the accumulated callback mask and reset it
            // (asynPortDriver.cpp:854-855 fires uint32Callback then sets
            // uInt32CallbackMask = 0). The flush is the single owner of
            // this consume, so accumulated bits never leak to the next.
            let uint32_mask = self
                .params
                .take_uint32_interrupt_mask(reason, addr)
                .unwrap_or(0);
            // C parity: asynPortDriver.cpp:633-637 sets
            // `pInterrupt->pasynUser->auxStatus/alarmStatus/alarmSeverity`
            // from the param's stored status before invoking each
            // subscriber callback. Pull those here so subscribers see
            // the same triplet C consumers do.
            let (aux_status, alarm_status, alarm_severity) = self
                .params
                .get_param_status(reason, addr)
                .unwrap_or((AsynStatus::Success, 0, 0));
            self.interrupts.notify(InterruptValue {
                reason,
                addr,
                value,
                timestamp: ts,
                uint32_changed_mask: uint32_mask,
                aux_status,
                alarm_status,
                alarm_severity,
                // Untyped: a single cached value per (reason,addr) reaches
                // every subscribing interface (the pre-per-interface path).
                iface: None,
            });
        }
        Ok(())
    }

    /// Flush a single parameter's changed flag and notify if dirty.
    /// Use this instead of `call_param_callbacks` when you want to avoid
    /// flushing unrelated parameters (e.g. rapidly-updating CP-linked params).
    pub fn call_param_callback(&mut self, addr: i32, reason: usize) -> AsynResult<()> {
        if self.params.take_changed_single(reason, addr)? {
            let value = self.params.get_value(reason, addr)?.clone();
            // C asynPortDriver.cpp:845 — see `call_param_callbacks`: an
            // undefined scalar consumes its changed flag but fires no
            // callback. Array/generic-pointer triggers fire regardless.
            if !value.is_array() && !self.params.is_param_defined(reason, addr).unwrap_or(false) {
                return Ok(());
            }
            let now = self.current_timestamp();
            let ts = self.params.get_timestamp(reason, addr)?.unwrap_or(now);
            // C parity: read the accumulated callback mask and reset it
            // (asynPortDriver.cpp:854-855 fires uint32Callback then sets
            // uInt32CallbackMask = 0). The flush is the single owner of
            // this consume, so accumulated bits never leak to the next.
            let uint32_mask = self
                .params
                .take_uint32_interrupt_mask(reason, addr)
                .unwrap_or(0);
            // C parity: see `call_param_callbacks` above.
            let (aux_status, alarm_status, alarm_severity) = self
                .params
                .get_param_status(reason, addr)
                .unwrap_or((AsynStatus::Success, 0, 0));
            self.interrupts.notify(InterruptValue {
                reason,
                addr,
                value,
                timestamp: ts,
                uint32_changed_mask: uint32_mask,
                aux_status,
                alarm_status,
                alarm_severity,
                // Untyped (see `call_param_callbacks`).
                iface: None,
            });
        }
        Ok(())
    }

    /// Mark a parameter as changed without modifying its value.
    ///
    /// Use this to trigger I/O Intr on params whose data is served via
    /// `read_*_array()` overrides rather than the param cache (e.g. pixel data).
    pub fn mark_param_changed(&mut self, index: usize, addr: i32) -> AsynResult<()> {
        self.params.mark_changed(index, addr)
    }

    /// Fire one per-interface I/O Intr callback carrying an interface-typed value.
    ///
    /// `call_param_callbacks` stores **one** value per `(reason, addr)` and
    /// notifies it untyped, so every record on that reason — whatever its DTYP's
    /// interface — receives the same value. For a driver whose single raw datum
    /// is exposed on several asyn interfaces at once (e.g. a Modbus register read
    /// by an `asynInt32` ai, an `asynUInt32Digital` bi, and an `asynFloat64` ai
    /// simultaneously), that collapse delivers a wrong-typed value to all but one
    /// of them. C's `drvModbusAsyn::readPoller` instead decodes the one register
    /// block **separately per interface** and invokes each interface's own
    /// interrupt list — `interruptStart` on `uInt32Digital`/`int32`/`float64`
    /// at drvModbusAsyn.cpp:1674/1716/1788. This is the analogue: the driver
    /// decodes per interface and fires each value tagged with its `iface`, so the
    /// interrupt filter routes it only to records on that interface
    /// ([`InterruptFilter::iface`](crate::interrupt::InterruptFilter::iface)). `uint32_changed_mask` is the changed-bit
    /// mask for the `UInt32Digital` interface (a record's `@asynMask` gates on it,
    /// `asynPortDriver.cpp:720`); pass `0` for the other interfaces, whose
    /// subscribers carry no mask filter.
    ///
    /// `aux_status` is the device I/O status this fire carries (C
    /// `pInterrupt->pasynUser->auxStatus`, set on every callback the poller
    /// emits — `drvModbusAsyn.cpp:1699/1738/1774/1810/1845/1880/1916`). A driver whose
    /// last acquisition failed still fires its interrupt lists, with the failing
    /// status, so I/O-Intr records go to READ/INVALID instead of freezing on the
    /// last good value; pass [`AsynStatus::Success`] on a clean acquisition.
    pub fn notify_interface_value(
        &self,
        reason: usize,
        addr: i32,
        iface: InterfaceType,
        value: ParamValue,
        uint32_changed_mask: u32,
        aux_status: AsynStatus,
    ) {
        let ts = self.current_timestamp();
        self.interrupts.notify(InterruptValue {
            reason,
            addr,
            value,
            timestamp: ts,
            uint32_changed_mask,
            aux_status,
            alarm_status: 0,
            alarm_severity: 0,
            iface: Some(iface),
        });
    }
}

/// Result of resolving a record's driver-info string at bind time — the
/// asyn-rs analogue of what C `drvUserCreate` writes into `pasynUser`.
///
/// `reason` is the shared parameter index (every record with the same drvInfo
/// resolves to it). The remaining fields carry **per-record** driver state the
/// lookup derived from this particular drvInfo string (C stashes the same in
/// `pasynUser->drvUser`), which the binding applies to that record's I/O.
#[derive(Debug, Default)]
pub struct DrvUserInfo {
    /// Shared parameter index for this drvInfo (C `pasynUser->reason`).
    pub reason: usize,
    /// Optional per-record octet length cap — the asyn-rs home for C's
    /// `modbusDrvUser_t.len` (`drvUserCreate` parses `TYPE=N`; `getStringLen`
    /// caps the asyn octet `maxLen` to it, drvModbusAsyn.cpp:2367-2377). `None`
    /// when the drvInfo carried no cap; the binding then uses the record buffer
    /// length alone. The binding applies `min(buffer_len, cap)`.
    pub max_octet_len: Option<usize>,
}

impl DrvUserInfo {
    /// A resolution carrying only the shared reason and no per-record cap — the
    /// default-lookup result.
    pub fn from_reason(reason: usize) -> Self {
        Self {
            reason,
            ..Self::default()
        }
    }
}

/// Everything a binding tells the driver when it resolves a drvInfo string —
/// the asyn-rs analogue of the `pasynUser` C `drvUserCreate` receives.
///
/// This is one carrier rather than a widening argument list because the bind
/// request has already grown once (`addr`, for C `checkOffset`) and is now
/// growing again (`iface`): a driver that resolves a drvInfo needs to know
/// *how the record will use the parameter*, not just its name. The struct is
/// `#[non_exhaustive]` so the next field is additive — build it with
/// [`DrvUserRequest::new`] and the builder methods.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DrvUserRequest {
    /// The record's driver-info string (C `pasynUser->drvUser` input).
    pub drv_info: String,
    /// The record's asyn `addr`, so a multi-device driver can reject an
    /// out-of-range address at bind time (C `drvUserCreate` `checkOffset`,
    /// drvModbusAsyn.cpp:378-384).
    pub addr: i32,
    /// The asyn interface the record will actually read and write this
    /// parameter through, derived from its DTYP (`asynFloat64` → [`InterfaceType::Float64`]).
    ///
    /// An on-demand driver (C Autoparam lazy creation) must create the
    /// parameter with the type the record will read it as, or every value the
    /// record takes from it is a coerced type mismatch: C
    /// `adsAsynPortDriver::getRecordInfoFromDrvInfo` derives the parameter's
    /// asyn type from the bound record's DTYP for exactly this reason — the
    /// same PLC symbol may bind as `asynInt32` from one record and
    /// `asynFloat64` from another.
    ///
    /// `None` when the bind has no record behind it (a port-level
    /// [`crate::sync_io`] resolve, a driver-to-driver handle call): the driver
    /// then has no record type to honour and falls back to its own default.
    pub iface: Option<InterfaceType>,
}

impl DrvUserRequest {
    /// A bind request for `drv_info` at `addr` with no record interface — the
    /// port-level resolve.
    pub fn new(drv_info: impl Into<String>, addr: i32) -> Self {
        Self {
            drv_info: drv_info.into(),
            addr,
            iface: None,
        }
    }

    /// Attach the bound record's asyn interface. Accepts an
    /// [`InterfaceType`] or an `Option<InterfaceType>`.
    pub fn with_iface(mut self, iface: impl Into<Option<InterfaceType>>) -> Self {
        self.iface = iface.into();
        self
    }
}

/// Port driver trait. All methods have default implementations that operate
/// on the parameter cache (no actual I/O).
///
/// Drivers performing real hardware I/O should:
/// 1. Run I/O in a background task (e.g., tokio::spawn)
/// 2. Update parameters via `base_mut().set_*_param()` + `call_param_callbacks()`
/// 3. Let the default `read_*` methods return cached values
///
/// # LockPort/UnlockPort
///
/// C asyn provides `lockPort`/`unlockPort` for direct mutex locking. In asyn-rs,
/// the port is always behind `Arc<Mutex<dyn PortDriver>>`, so callers hold the
/// parking_lot mutex directly. For multi-request exclusive access, use
/// `BlockProcess`/`UnblockProcess` via the worker queue.
/// C `epicsTimeToStrftime(buff, ..., "%Y/%m/%d %H:%M:%S.%03f", &timeStamp)` —
/// the port timestamp line of `asynPortDriver::report` (asynPortDriver.cpp:3682-3684).
/// Local time, as `epicsTimeToStrftime` renders it.
fn format_timestamp(ts: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(ts)
        .format("%Y/%m/%d %H:%M:%S%.3f")
        .to_string()
}

/// C's `details >= 3` block: one line per registered interrupt client
/// (`reportInterrupt`, asynPortDriver.cpp:1870-1894, called once per interface at
/// :3695-3708). C prints the callback and userPvt pointers with each client; Rust
/// mailboxes have no such pointers, so the line carries what identifies a client
/// here — the interface it bound, the address and reason it filtered on, and the
/// uint32 mask when it set one.
fn report_interrupt_clients(out: &mut dyn std::fmt::Write, base: &PortDriverBase) {
    use std::fmt::Write as _;
    for f in base.interrupts.clients() {
        let iface = f
            .iface
            .map_or("any", |i: crate::interfaces::InterfaceType| {
                i.interrupt_label()
            });
        let addr = f.addr.map_or("any".to_string(), |a| a.to_string());
        let reason = f.reason.map_or("any".to_string(), |r| r.to_string());
        let _ = write!(
            out,
            "    {iface} callback client addr={addr}, reason={reason}"
        );
        if let Some(mask) = f.uint32_mask {
            let _ = write!(out, ", mask=0x{mask:x}");
        }
        let _ = writeln!(out);
    }
}

/// C `showFailure` (asynOctetBase.c:370-380) — the message every Fail stub
/// produces: the port name, the method, and `not implemented`, returned as
/// `asynError`.
fn eos_not_implemented(port: &str, method: &str) -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message: format!("{port} {method} not implemented"),
    }
}

/// C's `switch (eoslen)` refusal, verbatim: `"%s illegal eoslen %d"` with the
/// port name and the length (asynInterposeEos.c:301-302, :354-355).
fn illegal_eoslen(port: &str, eoslen: usize) -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message: format!("{port} illegal eoslen {eoslen}"),
    }
}

pub trait PortDriver: Send + Sync + 'static {
    fn base(&self) -> &PortDriverBase;
    fn base_mut(&mut self) -> &mut PortDriverBase;

    // --- AsynCommon ---

    /// The trivial driver's `pasynCommon->connect`: C's own does its transport
    /// work and then calls `pasynManager->exceptionConnect(pasynUser)`, which
    /// resolves the slot it marks connected through `findDpCommon` on that same
    /// user (asynManager.c:2142-2162). So the addr the request carries selects
    /// the slot here too — [`PortDriverBase::set_addr_connected`] is the one
    /// owner of that resolution, and a user addressing no device marks the
    /// port's own. A driver that overrides this owns its transport's shape.
    fn connect(&mut self, user: &AsynUser) -> AsynResult<()> {
        // Single owner-API: edge-guarded fire is in PortDriverBase::set_connected.
        self.base_mut().set_addr_connected(user.addr, true);
        Ok(())
    }

    fn disconnect(&mut self, user: &AsynUser) -> AsynResult<()> {
        self.base_mut().set_addr_connected(user.addr, false);
        Ok(())
    }

    /// C `enable(pasynUser, 1)` (asynManager.c:2224-2251): ONE call, whose
    /// `findDpCommon(puserPvt)` picks the device's `dpCommon` or the port's from
    /// the addr the caller's user carries (:538-545). There is no second,
    /// device-only entry point in C and none here: [`PortDriverBase::
    /// set_addr_enabled`] is that resolution, and a user addressing no device
    /// lands on the port's own slot.
    fn enable(&mut self, user: &AsynUser) -> AsynResult<()> {
        // C `enable` refuses a defunct port (asynManager.c:2236-2241);
        // the guard lives in the single owner.
        self.base_mut().set_addr_enabled(user.addr, true)
    }

    fn disable(&mut self, user: &AsynUser) -> AsynResult<()> {
        self.base_mut().set_addr_enabled(user.addr, false)
    }

    fn connect_addr(&mut self, user: &AsynUser) -> AsynResult<()> {
        self.base_mut().connect_addr(user.addr);
        Ok(())
    }

    fn disconnect_addr(&mut self, user: &AsynUser) -> AsynResult<()> {
        self.base_mut().disconnect_addr(user.addr);
        Ok(())
    }

    fn get_option(&self, key: &str) -> AsynResult<String> {
        self.base()
            .options
            .get(key)
            .cloned()
            .ok_or_else(|| AsynError::OptionNotFound(key.to_string()))
    }

    /// C `asynOption::setOption(void *drvPvt, asynUser *pasynUser, key, val)`.
    ///
    /// `user` is the caller's, and its `timeout` is the one that bounds any wire
    /// traffic the option write causes — an RFC 2217 negotiation on a COM port
    /// runs under it (`asynInterposeCom.c:475,495`). The option layer has no
    /// timeout of its own: an asynRecord option put negotiates under TMOT, an
    /// iocsh `asynSetOption` under its own 2 s (`asynShellCommands.c:119`).
    fn set_option(&mut self, _user: &mut AsynUser, key: &str, value: &str) -> AsynResult<()> {
        self.base_mut()
            .options
            .insert(key.to_string(), value.to_string());
        Ok(())
    }

    /// The driver's own report — C `asynCommon::report`, which the manager calls
    /// last (`reportPrintPort`, asynManager.c:1113-1122) after printing the port's
    /// manager-level state itself.
    ///
    /// So this prints only what the *driver* owns. The port's enable / connect /
    /// queue / lock / exception / trace state is the manager's to print and is
    /// printed by `crate::port_actor::PortActor::report_port`; duplicating it
    /// here would give the operator two answers to the same question, from two
    /// owners, with no rule for which one wins.
    ///
    /// The default is C++ `asynPortDriver::report` (asynPortDriver.cpp:3677-3710):
    /// the port name; at `details >= 1` the timestamp, the EOS terminators *if the
    /// driver registered the octet interface* (`pasynStdInterfaces->octet.pinterface`,
    /// :3685) and the parameter library; at `details >= 3` the interrupt clients
    /// (:3695-3708).
    ///
    /// The report goes to `out`, C's `FILE *fp` — the driver never picks the
    /// stream. `crate::port_actor::PortActor::report_port` is the one owner that
    /// does, and it picks stdout, as `asynReport` does (asynShellCommands.c:589).
    fn report(&self, out: &mut dyn std::fmt::Write, level: i32) {
        use std::fmt::Write as _;
        let base = self.base();
        let _ = writeln!(out, "Port: {}", base.port_name);
        if level >= 1 {
            let _ = writeln!(
                out,
                "  Timestamp: {}",
                format_timestamp(base.current_timestamp())
            );
            // C prints the EOS pair only when the driver registered `asynOctet`
            // (:3685) — on a port with no octet interface the two terminators are
            // the constructor's zeroed fields, and reporting them invents an EOS
            // the port cannot have.
            if self.has_octet_interface() {
                // C escapes the terminator with `epicsStrPrintEscaped` (:3687,
                // :3690) — the whole libCom table, not just CR and LF. A private
                // two-case table wrote a binary terminator (`\x03`, ESC, TAB, NUL)
                // raw into stdout (R16-48); [`crate::escape`] is the one owner.
                let input = base.input_eos(0);
                let output = base.output_eos(0);
                let _ = writeln!(
                    out,
                    "  Input EOS[{}]: {}",
                    input.len(),
                    crate::escape::print_escaped(input)
                );
                let _ = writeln!(
                    out,
                    "  Output EOS[{}]: {}",
                    output.len(),
                    crate::escape::print_escaped(output)
                );
            }
            // C hands its own level straight to `reportParams` (:3693).
            base.report_params(out, level);
        }
        // There is no options block: `asynPortDriver::report` never prints one
        // (asynPortDriver.cpp:3677-3710), and it could not — `asynOption` is a
        // get/set pair keyed by a string the *driver* defines, with nothing to
        // enumerate. The `option: k = v` lines this printed at `details >= 2` had
        // no C source and no fixed key set to be complete over: they listed
        // whatever happened to have been written through `setOption`, which is not
        // the port's option state (R16-49).
        if level >= 3 {
            report_interrupt_clients(out, base);
        }
    }

    /// Whether this driver registered `asynOctet` — C's
    /// `pasynStdInterfaces->octet.pinterface != NULL`, which is set from the
    /// constructor's `interfaceMask` (asynPortDriver.cpp:4062). The Rust analogue
    /// of that mask is [`Self::capabilities`].
    fn has_octet_interface(&self) -> bool {
        self.capabilities()
            .iter()
            .any(|c| c.interface_type() == crate::interfaces::InterfaceType::Octet)
    }

    // --- Scalar I/O (cache-based defaults, timeout not applicable) ---

    // Cache-based defaults do NOT check connection state (C parity).
    // The port actor checks check_ready_addr() before dispatching, matching
    // C asyn where asynManager checks connection before calling the driver.

    // Default reads use the STRICT getter: an undefined parameter must
    // surface as ParamUndefined, not success/0. C parity — the default
    // asynPortDriver::read{Int32,Int64,Float64,Octet,UInt32Digital}
    // (asynPortDriver.cpp) calls get{Integer,Integer64,Double,String,
    // UIntDigital}Param, and every paramVal getter throws
    // ParamValNotDefined → asynParamUndefined for an unset value
    // (paramVal.cpp:152,181,235,264,292). devAsyn* then routes that status
    // through asynStatusToEpicsAlarm(READ_ALARM, INVALID_ALARM) instead of
    // updating RVAL/clearing UDF (e.g. devAsynUInt32Digital.c:898-901,
    // devAsynInt32.c:844-847). The lax get_*_param accessors stay for
    // internal callers that explicitly want default-zero behavior.

    fn read_int32(&mut self, user: &AsynUser) -> AsynResult<i32> {
        self.base().params.get_int32_strict(user.reason, user.addr)
    }

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_int32(user.reason, user.addr, value)?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn read_int64(&mut self, user: &AsynUser) -> AsynResult<i64> {
        self.base().params.get_int64_strict(user.reason, user.addr)
    }

    fn write_int64(&mut self, user: &mut AsynUser, value: i64) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_int64(user.reason, user.addr, value)?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    /// C `asynInt32Base.c:99` default: report `low = high = 0` so a
    /// driver that does not implement getBounds makes convertAi/convertAo
    /// skip the LINEAR ESLO/EOFF computation (`devAsynInt32.c:444`).
    fn get_bounds_int32(&self, _user: &AsynUser) -> AsynResult<(i32, i32)> {
        Ok((0, 0))
    }

    /// C `asynInt64Base.c:99` default: report `low = high = 0` (see
    /// `get_bounds_int32`).
    fn get_bounds_int64(&self, _user: &AsynUser) -> AsynResult<(i64, i64)> {
        Ok((0, 0))
    }

    fn read_float64(&mut self, user: &AsynUser) -> AsynResult<f64> {
        self.base()
            .params
            .get_float64_strict(user.reason, user.addr)
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_float64(user.reason, user.addr, value)?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        let bytes = self
            .base()
            .params
            .get_string_strict(user.reason, user.addr)?;
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(n)
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        self.base_mut()
            .params
            .set_string(user.reason, user.addr, data.to_vec())?;
        self.base_mut().call_param_callbacks(user.addr)?;
        Ok(data.len())
    }

    fn read_uint32_digital(&mut self, user: &AsynUser, mask: u32) -> AsynResult<u32> {
        let val = self
            .base()
            .params
            .get_uint32_strict(user.reason, user.addr)?;
        Ok(val & mask)
    }

    fn write_uint32_digital(
        &mut self,
        user: &mut AsynUser,
        value: u32,
        mask: u32,
    ) -> AsynResult<()> {
        // The asynUInt32Digital write interface carries no forced interrupt
        // mask — changed bits derive from value^old (interrupt_mask = 0).
        self.base_mut()
            .params
            .set_uint32(user.reason, user.addr, value, mask, 0)?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    /// Configure rising / falling interrupt masks for a
    /// UInt32Digital parameter. C parity:
    /// `asynPortDriver::setInterruptUInt32Digital`
    /// (`asynPortDriver.cpp:2346-2369`) → routes to
    /// `paramList::setUInt32Interrupt`. The default delegates to the
    /// param store; drivers that need to push the configuration to
    /// hardware (e.g. real GPIB cards toggling SRQ enable) override
    /// it.
    fn set_interrupt_uint32_digital(
        &mut self,
        user: &AsynUser,
        mask: u32,
        reason: InterruptReason,
    ) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_uint32_interrupt(user.reason, user.addr, mask, reason)
    }

    /// Clear bits from rising AND falling masks. C parity:
    /// `asynPortDriver::clearInterruptUInt32Digital`
    /// (`asynPortDriver.cpp:2392-2415`). Mirrors C — the call does
    /// not take an `interruptReason`; both masks are cleared.
    fn clear_interrupt_uint32_digital(&mut self, user: &AsynUser, mask: u32) -> AsynResult<()> {
        self.base_mut()
            .params
            .clear_uint32_interrupt(user.reason, user.addr, mask)
    }

    /// Read the configured rising / falling / combined mask. C
    /// parity: `asynPortDriver::getInterruptUInt32Digital`
    /// (`asynPortDriver.cpp:2438-2461`).
    fn get_interrupt_uint32_digital(
        &self,
        user: &AsynUser,
        reason: InterruptReason,
    ) -> AsynResult<u32> {
        self.base()
            .params
            .get_uint32_interrupt(user.reason, user.addr, reason)
    }

    // --- Enum I/O (cache-based defaults) ---

    fn read_enum(&mut self, user: &AsynUser) -> AsynResult<(usize, Arc<[EnumEntry]>)> {
        self.base().params.get_enum(user.reason, user.addr)
    }

    fn write_enum(&mut self, user: &mut AsynUser, index: usize) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_enum_index(user.reason, user.addr, index)?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn write_enum_choices(
        &mut self,
        user: &mut AsynUser,
        choices: Arc<[EnumEntry]>,
    ) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_enum_choices(user.reason, user.addr, choices)?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    // --- GenericPointer I/O (cache-based defaults) ---

    fn read_generic_pointer(&mut self, user: &AsynUser) -> AsynResult<Arc<dyn Any + Send + Sync>> {
        self.base()
            .params
            .get_generic_pointer(user.reason, user.addr)
    }

    fn write_generic_pointer(
        &mut self,
        user: &mut AsynUser,
        value: Arc<dyn Any + Send + Sync>,
    ) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_generic_pointer(user.reason, user.addr, value)?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    // --- Array I/O (default: not supported) ---

    fn read_float64_array(&mut self, _user: &AsynUser, _buf: &mut [f64]) -> AsynResult<usize> {
        Err(AsynError::InterfaceNotSupported("asynFloat64Array".into()))
    }

    fn write_float64_array(&mut self, user: &AsynUser, data: &[f64]) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_float64_array(user.reason, user.addr, data.to_vec())?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn read_int32_array(&mut self, _user: &AsynUser, _buf: &mut [i32]) -> AsynResult<usize> {
        Err(AsynError::InterfaceNotSupported("asynInt32Array".into()))
    }

    fn write_int32_array(&mut self, user: &AsynUser, data: &[i32]) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_int32_array(user.reason, user.addr, data.to_vec())?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn read_int8_array(&mut self, _user: &AsynUser, _buf: &mut [i8]) -> AsynResult<usize> {
        Err(AsynError::InterfaceNotSupported("asynInt8Array".into()))
    }

    fn write_int8_array(&mut self, user: &AsynUser, data: &[i8]) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_int8_array(user.reason, user.addr, data.to_vec())?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn read_int16_array(&mut self, _user: &AsynUser, _buf: &mut [i16]) -> AsynResult<usize> {
        Err(AsynError::InterfaceNotSupported("asynInt16Array".into()))
    }

    fn write_int16_array(&mut self, user: &AsynUser, data: &[i16]) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_int16_array(user.reason, user.addr, data.to_vec())?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn read_int64_array(&mut self, _user: &AsynUser, _buf: &mut [i64]) -> AsynResult<usize> {
        Err(AsynError::InterfaceNotSupported("asynInt64Array".into()))
    }

    fn write_int64_array(&mut self, user: &AsynUser, data: &[i64]) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_int64_array(user.reason, user.addr, data.to_vec())?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    fn read_float32_array(&mut self, _user: &AsynUser, _buf: &mut [f32]) -> AsynResult<usize> {
        Err(AsynError::InterfaceNotSupported("asynFloat32Array".into()))
    }

    fn write_float32_array(&mut self, user: &AsynUser, data: &[f32]) -> AsynResult<()> {
        self.base_mut()
            .params
            .set_float32_array(user.reason, user.addr, data.to_vec())?;
        self.base_mut().call_param_callbacks(user.addr)
    }

    // --- I/O methods (worker thread calls these) ---
    // Default: delegate to cache-based read_*/write_* for backward compat.
    // Real I/O drivers override these for actual hardware access.

    fn io_read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        self.read_octet(user, buf)
    }

    /// Octet read that also reports the end-of-message reason — C
    /// parity for `asynOctet::read(... int *eomReason)`
    /// (`asynOctet.h:38-40`). The default implementation delegates to
    /// [`Self::io_read_octet`] and reconstructs a synthetic
    /// [`EomReason`]: `CNT` when the buffer filled, `empty` otherwise.
    /// Drivers that have native EOM information
    /// (`asynOctetSyncIO::read`, GPIB END, EOS match) must
    /// override this method so consumers — `asynRecord::EOMR`,
    /// `asynOctetSyncIO::read` mirrors — receive the real flags.
    fn io_read_octet_eom(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
    ) -> AsynResult<(usize, EomReason)> {
        let cap = buf.len();
        let n = self.io_read_octet(user, buf)?;
        let eom = if n >= cap && cap > 0 {
            EomReason::CNT
        } else {
            EomReason::empty()
        };
        Ok((n, eom))
    }

    fn io_write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        self.write_octet(user, data)
    }

    fn io_read_int32(&mut self, user: &AsynUser) -> AsynResult<i32> {
        self.read_int32(user)
    }

    fn io_write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        self.write_int32(user, value)
    }

    fn io_read_int64(&mut self, user: &AsynUser) -> AsynResult<i64> {
        self.read_int64(user)
    }

    fn io_write_int64(&mut self, user: &mut AsynUser, value: i64) -> AsynResult<()> {
        self.write_int64(user, value)
    }

    fn io_read_float64(&mut self, user: &AsynUser) -> AsynResult<f64> {
        self.read_float64(user)
    }

    fn io_write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        self.write_float64(user, value)
    }

    fn io_read_uint32_digital(&mut self, user: &AsynUser, mask: u32) -> AsynResult<u32> {
        self.read_uint32_digital(user, mask)
    }

    fn io_write_uint32_digital(
        &mut self,
        user: &mut AsynUser,
        value: u32,
        mask: u32,
    ) -> AsynResult<()> {
        self.write_uint32_digital(user, value, mask)
    }

    fn io_flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
        Ok(())
    }

    // --- Octet EOS (delegates to interpose stack by default) ---
    //
    // ## EOS connect-wait policy (C asyn issue #103)
    //
    // C asyn `asynOctetSyncIO::setInputEos` / `setOutputEos`
    // (`asynOctetSyncIO.c:300-321`, `:346-367`) bracket the driver call in
    // `lockPort`/`unlockPort`. `lockPort` is a plain mutex take —
    // `defunct` check, then `synchronousLock` (asynManager.c:1789-1798) —
    // and waits for no connection, so an EOS set blocks only behind
    // whoever else holds the port. The connect wait lives elsewhere:
    // `registerPort` calls `waitConnect(pport->pconnectUser,
    // pasynBase->autoConnectTimeout)` on an autoConnect port
    // (asynManager.c:2135), and `waitConnect` `epicsEventWaitWithTimeout`s
    // on a connect event signalled from an exception handler (:3292-3336).
    // That is what issue #103 sees: IOC startup pausing for the whole
    // autoConnect timeout when the device is off-line.
    //
    // The Rust path here is purely in-memory: `set_input_eos` and
    // `set_output_eos` write the bytes into `PortDriverBase` and the
    // EOS interpose stack reads from those fields at next read/write
    // time. No port lock, no connect-wait — so neither stall can reach an
    // EOS call here. If a future refactor introduces a connect-gated EOS
    // path (e.g. a driver that owns the EOS state inside its
    // connect()-allocated resource), authors MUST keep the wait optional /
    // bounded so the connect-wait failure mode doesn't return.

    // Every hook takes the `asynUser`, because in C every one of them does
    // (`asynOctet::setInputEos(void *ppvt, asynUser *pasynUser, ...)`,
    // asynOctetBase.h; asynInterposeEos.c:288-296) and the addr it carries is
    // what picks the device's terminator. A port-wide EOS could not hold two.

    /// The default is C's **Fail stub**, not a silent cache. `initOverride`
    /// swaps a NULL `setInputEos` for `setInputEosFail` (asynOctetBase.c:115),
    /// which answers `"<port> setInputEos not implemented"` and `asynError`
    /// (:370-380, :401-403) — and serial, IP, IP-server and FTDI really do
    /// leave it NULL (drvAsynSerialPort.c:1003, drvAsynIPPort.c:1049-1051,
    /// drvAsynIPServerPort.c:118-123, drvAsynFTDIPort.cpp:610-612). What
    /// makes IEOS work on those ports is `asynInterposeEos`, installed above
    /// the driver by `asynOctetBase::initialize` when `processEosIn` is set
    /// (:169-171) — so the terminator is accepted exactly when a layer takes
    /// it, and refused otherwise. A `noProcessEos` port reporting success and
    /// then never terminating is the failure this refusal exists to make loud.
    ///
    /// A driver with its own EOS methods overrides this, which is the Rust
    /// shape of a non-NULL entry in C's `asynOctet` table.
    fn set_input_eos(&mut self, user: &AsynUser, eos: &[u8]) -> AsynResult<()> {
        // Single write owner for input EOS: the per-device cache is what
        // `get_input_eos` and the binary-suppress save/restore read, and the
        // same value is forwarded to the interpose stack so an installed
        // `EosInterpose` actually terminates *this device's* reads on it.
        let addr = user.addr;
        let port = self.base().port_name.clone();
        let base = self.base_mut();
        // The length rule belongs to the layer that stores the terminator
        // (asynInterposeEos.c:299-303), so a port with no EOS support answers
        // "not implemented" for every length, exactly as its Fail stub does,
        // and a layer that owns the terminator refuses an illegal one without
        // storing it.
        match base.interpose_octet.set_input_eos(addr, eos) {
            EosSet::NotTaken => return Err(eos_not_implemented(&port, "setInputEos")),
            EosSet::IllegalLength => return Err(illegal_eoslen(&port, eos.len())),
            EosSet::Stored => {}
        }
        base.eos_entry(addr).input = eos.to_vec();
        Ok(())
    }

    fn get_input_eos(&self, user: &AsynUser) -> Vec<u8> {
        self.base().input_eos(user.addr).to_vec()
    }

    /// The output half of [`Self::set_input_eos`] — C `setOutputEosFail`
    /// (asynOctetBase.c:117, :409-411).
    fn set_output_eos(&mut self, user: &AsynUser, eos: &[u8]) -> AsynResult<()> {
        // Single write owner for output EOS (see `set_input_eos`): cache per
        // device and forward to the interpose stack so `EosInterpose` appends
        // the terminator on that device's writes.
        let addr = user.addr;
        let port = self.base().port_name.clone();
        let base = self.base_mut();
        match base.interpose_octet.set_output_eos(addr, eos) {
            EosSet::NotTaken => return Err(eos_not_implemented(&port, "setOutputEos")),
            EosSet::IllegalLength => return Err(illegal_eoslen(&port, eos.len())),
            EosSet::Stored => {}
        }
        base.eos_entry(addr).output = eos.to_vec();
        Ok(())
    }

    fn get_output_eos(&self, user: &AsynUser) -> Vec<u8> {
        self.base().output_eos(user.addr).to_vec()
    }

    // --- asynGpib (IEEE-488 bus control) ---
    //
    // The four command methods of C's `asynGpib` interface (asynGpibDriver.h:47-51),
    // which asynGpib.c passes straight through to the driver's `asynGpibPort`
    // (asynGpib.c:472-496). A driver that implements them declares
    // [`crate::interfaces::Capability::Gpib`], and that declaration is what a
    // client's `findInterface(asynGpibType)` answers — asynRecord reads it into
    // GPIBIV and refuses UCMD/ACMD when it is 0 (asynRecord.c:1232-1241,
    // :1647-1651).
    //
    // The defaults refuse: a port that has not declared the capability can only
    // be reached here by a caller that skipped the registry, and C has nothing
    // to call in that case (the interface pointer is NULL).
    //
    // Not ported: the `asynGpibPort` methods that exist solely to drive
    // asynGpib's SRQ poll thread — `srqStatus`, `srqEnable`, `serialPollBegin`,
    // `serialPoll`, `serialPollEnd` (asynGpibDriver.h:88-92), plus `pollAddr` /
    // `srqHappened` (asynGpib.c:498-559, 633-656). Nothing in this tree polls
    // SRQ; asynRecord's own "Serial Poll" ACMD does not use them (it sends SPE,
    // reads one octet, sends SPD — asynRecord.c:1717-1746).

    /// C `asynGpib::universalCmd` — send one universal command byte with ATN
    /// asserted (asynGpib.c:480-484, `vxiUniversalCmd` drvVxi11.c:1406-1424).
    fn gpib_universal_cmd(&mut self, _user: &mut AsynUser, _cmd: u8) -> AsynResult<()> {
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "port has no asynGpib interface".into(),
        })
    }

    /// C `asynGpib::addressedCmd` — send an addressed-command frame with ATN
    /// asserted (asynGpib.c:472-478, `vxiAddressedCmd` drvVxi11.c:1360-1404).
    /// The frame is built by [`crate::interfaces::gpib::addressed_request`].
    fn gpib_addressed_cmd(&mut self, _user: &mut AsynUser, _data: &[u8]) -> AsynResult<()> {
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "port has no asynGpib interface".into(),
        })
    }

    /// C `asynGpib::ifc` — assert Interface Clear (asynGpib.c:486-490).
    fn gpib_ifc(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "port has no asynGpib interface".into(),
        })
    }

    /// C `asynGpib::ren` — set the Remote Enable line (asynGpib.c:492-496).
    fn gpib_ren(&mut self, _user: &mut AsynUser, _enable: bool) -> AsynResult<()> {
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "port has no asynGpib interface".into(),
        })
    }

    // --- Lifecycle ---

    /// Called when the port is being shut down. Drivers override this
    /// to release hardware resources. Matches C asynPortDriver::shutdownPortDriver().
    fn shutdown(&mut self) -> AsynResult<()> {
        Ok(())
    }

    // --- drvUser ---

    /// Resolve a record's bind request ([`DrvUserRequest`]: drvInfo string, asyn
    /// `addr`, and the record's asyn interface) to a [`DrvUserInfo`] — the
    /// asyn-rs analogue of C `drvUserCreate`.
    ///
    /// Takes `&mut self` so a driver can register a parameter on demand from the
    /// resolved drvInfo (C Autoparam lazy creation) rather than requiring it be
    /// declared up front. Such a driver must create the parameter with the type
    /// the record will read it as — [`DrvUserRequest::iface`] — the way C
    /// `adsAsynPortDriver::getRecordInfoFromDrvInfo` derives the parameter's
    /// asyn type from the bound record's DTYP.
    ///
    /// [`DrvUserRequest::addr`] lets a multi-device driver reject an
    /// out-of-range address at bind time (C `drvUserCreate` runs `checkOffset`,
    /// drvModbusAsyn.cpp:378-384) instead of alarming on every I/O.
    ///
    /// Default: look up the shared reason by parameter name; ignore the rest.
    fn drv_user_create(&mut self, req: &DrvUserRequest) -> AsynResult<DrvUserInfo> {
        let reason = self
            .base()
            .params
            .find_param(&req.drv_info)
            .ok_or_else(|| AsynError::ParamNotFound(req.drv_info.clone()))?;
        Ok(DrvUserInfo::from_reason(reason))
    }

    // --- Capabilities ---

    /// Declare the capabilities this driver supports.
    /// Default implementation includes all scalar read/write operations.
    fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
        crate::interfaces::default_capabilities()
    }

    /// Check if this driver supports a specific capability.
    fn supports(&self, cap: crate::interfaces::Capability) -> bool {
        self.capabilities().contains(&cap)
    }

    fn init(&mut self) -> AsynResult<()> {
        Ok(())
    }
}

/// The driver sitting at the **bottom** of a port's octet interpose chain — C's
/// driver-registered `asynOctet` interface, the one `interposeInterface` keeps
/// as `pPrev` when it pushes a layer on top (asynManager.c:2190-2220).
///
/// A driver never knows it is being interposed in C: `findInterface` hands the
/// caller the topmost layer, and the layer calls down. The Rust equivalent of
/// "the caller" is the port actor, so the chain runs there
/// ([`octet_read_chain`] and friends) and the driver's `io_*_octet` are the raw
/// device transfer, nothing more.
struct DriverOctetLink<'a> {
    driver: &'a mut dyn PortDriver,
}

impl OctetNext for DriverOctetLink<'_> {
    /// C `asynOctetBase::readIt` (asynOctetBase.c:224-238): call the driver's
    /// own `read`, and on success fan the result out to the port's octet
    /// interrupt users.
    ///
    /// This is *below* the EOS layer by construction, and that is the whole
    /// point. C interposes `octetBase` directly on the driver
    /// (asynOctetBase.c:156-159) and only then pushes `asynInterposeEos` on top
    /// of it (:170-171), so the stack is `EOS → octetBase → driver`: every
    /// interrupt user sees the RAW DRIVER CHUNK — terminator included, with the
    /// driver's own CNT/END eomReason — and gets one callback per lower-level
    /// read, not one per EOS-completed message. Firing above the chain (as the
    /// port actor used to) handed an I/O-Intr record `"abc"`/EOMR=EOS where C
    /// hands it `"abc\r\n"`/EOMR=CNT|END.
    fn read(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
        let (nbytes_transferred, eom_reason) = self.driver.io_read_octet_eom(user, buf)?;
        if self.driver.base().octet_interrupt_process {
            let raw = &buf[..nbytes_transferred];
            // C's rule for a device read: `addr` decides, `reason` is never
            // consulted (asynOctetBase.c:202-210). `reason` still rides on the
            // value — C leaves `pasynUser->reason` on the callback's user — but
            // it selects nobody.
            self.driver.base().interrupts.notify_octet(
                OctetFanOut::ByAddr(user.addr),
                InterruptValue {
                    reason: user.reason,
                    addr: user.addr,
                    value: ParamValue::Octet(raw.to_vec()),
                    timestamp: SystemTime::now(),
                    iface: Some(InterfaceType::Octet),
                    ..Default::default()
                },
            );
        }
        Ok(OctetReadResult {
            nbytes_transferred,
            eom_reason,
        })
    }

    fn write(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        self.driver.io_write_octet(user, data)
    }

    fn flush(&mut self, user: &mut AsynUser) -> AsynResult<()> {
        self.driver.io_flush(user)
    }
}

/// Run `f` with the port's chain and the driver below it, both borrowed at once.
///
/// The chain lives inside the driver (`base.interpose_octet`) while the driver
/// is the chain's base, so the two are lifted apart for the duration of one
/// transfer and the chain is put straight back. The actor owns the driver and is
/// the only caller, so no other code can observe the port between the take and
/// the restore.
fn with_octet_chain<T>(
    driver: &mut dyn PortDriver,
    f: impl FnOnce(&mut OctetInterposeStack, &mut DriverOctetLink<'_>) -> T,
) -> T {
    let multi_device = driver.base().flags.multi_device;
    let mut chain = std::mem::replace(
        &mut driver.base_mut().interpose_octet,
        OctetInterposeStack::new(multi_device),
    );
    let result = {
        let mut link = DriverOctetLink {
            driver: &mut *driver,
        };
        f(&mut chain, &mut link)
    };
    driver.base_mut().interpose_octet = chain;
    result
}

/// One octet read on the port: through every interpose layer installed on the
/// addressed device, ending at the driver.
///
/// C `asynOctet::read` on the interface `findInterface` resolves — which is the
/// outermost interpose whenever one is installed. The chain belongs to the
/// **port**, not to the driver: `asynInterposeEos` / `asynInterposeEcho` /
/// `asynInterposeDelay` are pushed by the manager (asynManager.c:2190-2220) and
/// the driver below is not consulted and cannot opt out. Dispatching inside each
/// driver instead — as this crate did — made the chain per-driver opt-in, so the
/// EOS layer `drvAsynFTDIPortConfigure` installs (drvAsynFTDIPort.cpp:622-623,
/// ftdi.rs) was never run by anything.
pub(crate) fn octet_read_chain(
    driver: &mut dyn PortDriver,
    user: &AsynUser,
    buf: &mut [u8],
) -> AsynResult<(usize, EomReason)> {
    with_octet_chain(driver, |chain, link| chain.dispatch_read(user, buf, link))
        .map(|r| (r.nbytes_transferred, r.eom_reason))
}

/// One octet write on the port, through the addressed device's chain
/// (see [`octet_read_chain`]).
pub(crate) fn octet_write_chain(
    driver: &mut dyn PortDriver,
    user: &mut AsynUser,
    data: &[u8],
) -> AsynResult<usize> {
    with_octet_chain(driver, |chain, link| chain.dispatch_write(user, data, link))
}

/// One octet flush on the port, through the addressed device's chain — C
/// `asynInterposeEos::flushIt` resets the layer's read-ahead buffer and then
/// flushes the layer below (asynInterposeEos.c:256-268), which only happens if
/// the flush enters the chain at the top (see [`octet_read_chain`]).
pub(crate) fn octet_flush_chain(
    driver: &mut dyn PortDriver,
    user: &mut AsynUser,
) -> AsynResult<()> {
    with_octet_chain(driver, |chain, link| chain.dispatch_flush(user, link))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A driver whose `io_read_octet_eom` hands back one scripted chunk per
    /// call, with the driver-level eomReason C would report (CNT when the
    /// caller's buffer filled, END never — a stream driver has no message
    /// boundary). The EOS layer above it is what turns chunks into messages.
    struct ChunkDriver {
        base: PortDriverBase,
        chunks: Vec<Vec<u8>>,
        next: usize,
    }

    impl ChunkDriver {
        fn new(chunks: Vec<&[u8]>) -> Self {
            let mut base = PortDriverBase::new("chunk", 1, PortFlags::default());
            base.octet_interrupt_process = true;
            base.init_connected(true);
            Self {
                base,
                chunks: chunks.into_iter().map(|c| c.to_vec()).collect(),
                next: 0,
            }
        }
    }

    impl PortDriver for ChunkDriver {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        fn io_read_octet_eom(
            &mut self,
            _user: &AsynUser,
            buf: &mut [u8],
        ) -> AsynResult<(usize, EomReason)> {
            let chunk = self.chunks.get(self.next).cloned().unwrap_or_default();
            self.next += 1;
            let n = chunk.len().min(buf.len());
            buf[..n].copy_from_slice(&chunk[..n]);
            let eom = if n == buf.len() {
                EomReason::CNT
            } else {
                EomReason::empty()
            };
            Ok((n, eom))
        }
    }

    /// The octet interrupt fan-out runs BELOW the interpose chain, at the
    /// driver link — C's `asynOctetBase::readIt` (asynOctetBase.c:224-238),
    /// which is interposed directly on the driver (:156-159) with the EOS layer
    /// pushed on top of it (:170-171).
    ///
    /// Two boundaries, both invisible when the fan-out ran above the chain:
    /// (1) the payload an interrupt user receives is the RAW driver chunk —
    /// terminator included — not the EOS-stripped message the caller gets; and
    /// (2) a message assembled from two lower-level reads fires TWO callbacks,
    /// one per driver read, not one per completed message.
    #[test]
    fn octet_interrupt_fans_out_below_the_interpose_chain() {
        use crate::interpose::eos::EosInterpose;
        use crate::interrupt::{InterruptFilter, InterruptValue};
        use std::sync::{Arc, Mutex};

        // The message "abc\r\n" arrives split across two driver reads.
        let mut drv = ChunkDriver::new(vec![b"ab", b"c\r\n"]);
        drv.base_mut()
            .install_octet_interpose(Box::new(EosInterpose::default()));
        drv.set_input_eos(&AsynUser::default(), b"\r\n").unwrap();

        let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_cb = seen.clone();
        let _sub = drv.base().interrupts.register_sync_callback(
            InterruptFilter::default(),
            move |iv: &InterruptValue| {
                if let ParamValue::Octet(s) = &iv.value {
                    seen_cb.lock().unwrap().push(s.clone());
                }
            },
        );

        let user = AsynUser::default();
        let mut buf = [0u8; 32];
        let (n, eom) = octet_read_chain(&mut drv, &user, &mut buf).unwrap();

        // The CALLER gets the EOS-terminated message, terminator stripped.
        assert_eq!(&buf[..n], b"abc");
        assert!(eom.contains(EomReason::EOS), "caller sees EOS, got {eom:?}");

        // The INTERRUPT USERS get the raw driver chunks, terminator included,
        // one callback per lower-level read.
        assert_eq!(
            *seen.lock().unwrap(),
            vec![b"ab".to_vec(), b"c\r\n".to_vec()],
            "each driver read fans out its raw chunk"
        );
    }

    /// A device answer is bytes. C's octet callback hands the subscriber
    /// `pasynUser`'s buffer verbatim (`asynOctetBase.c:224-238`) and
    /// `asynPortDriver::doCallbacksOctet` hands it `val.c_str()`
    /// (asynPortDriver.cpp:2011-2027) — a `char *`, which carries any byte.
    /// Carrying the payload in a Rust `String` could not: the fan-out decoded
    /// it lossily, so a binary protocol's `0x80..=0xff` reached every I/O Intr
    /// record as U+FFFD (`ef bf bd`), longer than the byte it replaced and no
    /// longer the device's answer.
    #[test]
    fn an_octet_interrupt_carries_a_non_utf8_device_byte_verbatim() {
        use crate::interrupt::{InterruptFilter, InterruptValue};
        use std::sync::{Arc, Mutex};

        // A Modbus-shaped answer: a high byte, an embedded NUL, a lone 0xff.
        const RAW: &[u8] = &[0x01, 0x80, 0x00, 0xfe, 0xff, 0x41];
        let mut drv = ChunkDriver::new(vec![RAW]);

        let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_cb = seen.clone();
        let _sub = drv.base().interrupts.register_sync_callback(
            InterruptFilter::default(),
            move |iv: &InterruptValue| {
                if let ParamValue::Octet(bytes) = &iv.value {
                    seen_cb.lock().unwrap().push(bytes.clone());
                }
            },
        );

        let user = AsynUser::default();
        let mut buf = [0u8; 32];
        let (n, _eom) = octet_read_chain(&mut drv, &user, &mut buf).unwrap();

        assert_eq!(&buf[..n], RAW, "the caller gets the device bytes");
        assert_eq!(
            *seen.lock().unwrap(),
            vec![RAW.to_vec()],
            "so does the interrupt user — no lossy decode on the fan-out"
        );
    }

    struct TestDriver {
        base: PortDriverBase,
    }

    impl TestDriver {
        fn new() -> Self {
            let mut base = PortDriverBase::new("test", 1, PortFlags::default());
            base.create_param("VAL", ParamType::Int32).unwrap();
            base.create_param("TEMP", ParamType::Float64).unwrap();
            base.create_param("MSG", ParamType::Octet).unwrap();
            base.create_param("BITS", ParamType::UInt32Digital).unwrap();
            Self { base }
        }
    }

    impl PortDriver for TestDriver {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
    }

    #[test]
    fn test_default_read_write_int32() {
        let mut drv = TestDriver::new();
        let mut user = AsynUser::new(0);
        drv.write_int32(&mut user, 42).unwrap();
        let user = AsynUser::new(0);
        assert_eq!(drv.read_int32(&user).unwrap(), 42);
    }

    #[test]
    fn test_default_read_write_float64() {
        let mut drv = TestDriver::new();
        let mut user = AsynUser::new(1);
        drv.write_float64(&mut user, 3.14).unwrap();
        let user = AsynUser::new(1);
        assert!((drv.read_float64(&user).unwrap() - 3.14).abs() < 1e-10);
    }

    #[test]
    fn test_default_read_write_octet() {
        let mut drv = TestDriver::new();
        let mut user = AsynUser::new(2);
        drv.write_octet(&mut user, b"hello").unwrap();
        let user = AsynUser::new(2);
        let mut buf = [0u8; 32];
        let n = drv.read_octet(&user, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
    }

    #[test]
    fn test_default_read_write_uint32() {
        let mut drv = TestDriver::new();
        let mut user = AsynUser::new(3);
        drv.write_uint32_digital(&mut user, 0xFF, 0x0F).unwrap();
        let user = AsynUser::new(3);
        assert_eq!(drv.read_uint32_digital(&user, 0xFF).unwrap(), 0x0F);
    }

    #[test]
    fn test_connect_disconnect() {
        let mut drv = TestDriver::new();
        let user = AsynUser::default();
        assert!(drv.base().is_connected());
        drv.disconnect(&user).unwrap();
        assert!(!drv.base().is_connected());
        drv.connect(&user).unwrap();
        assert!(drv.base().is_connected());
    }

    #[test]
    fn test_drv_user_create() {
        let mut drv = TestDriver::new();
        assert_eq!(
            drv.drv_user_create(&DrvUserRequest::new("VAL", 0))
                .unwrap()
                .reason,
            0
        );
        assert_eq!(
            drv.drv_user_create(&DrvUserRequest::new("TEMP", 0))
                .unwrap()
                .reason,
            1
        );
        assert!(
            drv.drv_user_create(&DrvUserRequest::new("NOPE", 0))
                .is_err()
        );
    }

    #[test]
    fn test_call_param_callbacks() {
        let mut drv = TestDriver::new();
        let mut rx = drv.base_mut().interrupts.subscribe_async();

        drv.base_mut().set_int32_param(0, 0, 100).unwrap();
        drv.base_mut().set_float64_param(1, 0, 2.0).unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();

        let v1 = rx.try_recv().unwrap();
        assert_eq!(v1.reason, 0);
        let v2 = rx.try_recv().unwrap();
        assert_eq!(v2.reason, 1);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn flush_skips_undefined_scalar_but_keeps_array_trigger() {
        // C asynPortDriver.cpp:845 — a status/alarm change (or bare
        // mark_changed) on a never-set scalar consumes the changed flag
        // but fires no callback. Array/generic-pointer triggers have no
        // callCallbacks analog (:846-865 switch is scalar-only) and must
        // still fire even while undefined.
        let mut drv = TestDriver::new();
        let arr = drv
            .base_mut()
            .create_param("ARR", ParamType::Int32Array)
            .unwrap();
        let mut rx = drv.base_mut().interrupts.subscribe_async();

        // Status change on the never-set scalar VAL (index 0): marks it
        // changed but it stays undefined.
        drv.base_mut()
            .params
            .set_param_status(0, 0, AsynStatus::Error, 0, 0)
            .unwrap();
        // Mark the never-set array param changed: an override-served trigger.
        drv.base_mut().mark_param_changed(arr, 0).unwrap();

        drv.base_mut().call_param_callbacks(0).unwrap();

        // Only the array trigger is delivered; the undefined scalar is gated.
        let iv = rx.try_recv().unwrap();
        assert_eq!(
            iv.reason, arr,
            "array trigger must still fire while undefined"
        );
        assert!(
            rx.try_recv().is_err(),
            "undefined scalar must not emit an I/O Intr"
        );

        // Once the scalar is defined, a subsequent change does fire.
        drv.base_mut().set_int32_param(0, 0, 7).unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();
        let iv2 = rx.try_recv().unwrap();
        assert_eq!(iv2.reason, 0, "defined scalar must fire");
    }

    #[test]
    fn uint32_callback_mask_does_not_leak_across_flushes() {
        // C resets uInt32CallbackMask = 0 after each uint32Callback
        // (asynPortDriver.cpp:855): a second flush must deliver only the
        // bits changed since the first, never the accumulated history.
        let mut drv = TestDriver::new();
        let mut rx = drv.base_mut().interrupts.subscribe_async();

        // flush 1: change bit 0 on BITS (param index 3).
        drv.base_mut()
            .params
            .set_uint32(3, 0, 0x01, 0x01, 0)
            .unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();
        let iv1 = rx.try_recv().unwrap();
        assert_eq!(iv1.reason, 3);
        assert_eq!(iv1.uint32_changed_mask, 0x01);

        // flush 2: change bit 1 only — must deliver 0x02, not 0x03.
        drv.base_mut()
            .params
            .set_uint32(3, 0, 0x02, 0x02, 0)
            .unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();
        let iv2 = rx.try_recv().unwrap();
        assert_eq!(
            iv2.uint32_changed_mask, 0x02,
            "second flush must not leak flush-1 bits via an un-reset mask"
        );
        assert_eq!(
            drv.base().params.get_uint32_interrupt_mask(3, 0).unwrap(),
            0,
            "the flush must consume (reset) the callback mask"
        );
    }

    #[test]
    fn test_call_param_callbacks_propagates_aux_status_and_alarm() {
        // C parity: asynPortDriver.cpp:633-637 writes the param's stored
        // status / alarmStatus / alarmSeverity onto the subscriber's
        // pasynUser before invoking the callback. The Rust port carries
        // those fields on InterruptValue.
        let mut drv = TestDriver::new();
        let mut rx = drv.base_mut().interrupts.subscribe_async();

        drv.base_mut().set_int32_param(0, 0, 99).unwrap();
        drv.base_mut()
            .params
            .set_param_status(0, 0, crate::error::AsynStatus::Timeout, 4, 2)
            .unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();

        let iv = rx.try_recv().unwrap();
        assert_eq!(iv.reason, 0);
        assert!(matches!(iv.aux_status, crate::error::AsynStatus::Timeout));
        assert_eq!(iv.alarm_status, 4);
        assert_eq!(iv.alarm_severity, 2);
    }

    #[test]
    fn test_call_param_callback_single_propagates_aux_status() {
        // Mirror for the single-flush path (call_param_callback).
        let mut drv = TestDriver::new();
        let mut rx = drv.base_mut().interrupts.subscribe_async();

        drv.base_mut().set_int32_param(0, 0, 1).unwrap();
        drv.base_mut()
            .params
            .set_param_status(0, 0, crate::error::AsynStatus::Disconnected, 7, 3)
            .unwrap();
        drv.base_mut().call_param_callback(0, 0).unwrap();

        let iv = rx.try_recv().unwrap();
        assert!(matches!(
            iv.aux_status,
            crate::error::AsynStatus::Disconnected
        ));
        assert_eq!(iv.alarm_status, 7);
        assert_eq!(iv.alarm_severity, 3);
    }

    #[test]
    fn test_no_callback_for_unchanged() {
        let mut drv = TestDriver::new();
        let mut rx = drv.base_mut().interrupts.subscribe_async();

        drv.base_mut().set_int32_param(0, 0, 5).unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();
        let _ = rx.try_recv().unwrap(); // consume

        // Set same value — no interrupt
        drv.base_mut().set_int32_param(0, 0, 5).unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_array_not_supported_by_default() {
        let mut drv = TestDriver::new();
        let user = AsynUser::new(0);
        let mut buf = [0f64; 10];
        assert!(drv.read_float64_array(&user, &mut buf).is_err());
        assert!(drv.write_float64_array(&user, &[1.0]).is_err());
    }

    #[test]
    fn test_option_set_get() {
        let mut drv = TestDriver::new();
        drv.set_option(&mut AsynUser::default(), "baud", "9600")
            .unwrap();
        assert_eq!(drv.get_option("baud").unwrap(), "9600");
        drv.set_option(&mut AsynUser::default(), "baud", "115200")
            .unwrap();
        assert_eq!(drv.get_option("baud").unwrap(), "115200");
    }

    #[test]
    fn test_option_not_found() {
        let drv = TestDriver::new();
        let err = drv.get_option("nonexistent").unwrap_err();
        assert!(matches!(err, AsynError::OptionNotFound(_)));
    }

    #[test]
    fn test_report_no_panic() {
        let mut drv = TestDriver::new();
        drv.set_option(&mut AsynUser::default(), "testkey", "testval")
            .unwrap();
        drv.base_mut().set_int32_param(0, 0, 42).unwrap();
        for level in 0..=3 {
            let mut out = String::new();
            drv.report(&mut out, level);
        }
    }

    #[test]
    fn test_callback_uses_param_timestamp() {
        let mut drv = TestDriver::new();
        let mut rx = drv.base_mut().interrupts.subscribe_async();

        let custom_ts = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        drv.base_mut().set_int32_param(0, 0, 77).unwrap();
        drv.base_mut().set_param_timestamp(0, 0, custom_ts).unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();

        let v = rx.try_recv().unwrap();
        assert_eq!(v.reason, 0);
        assert_eq!(v.timestamp, custom_ts);
    }

    #[test]
    fn test_default_read_write_enum() {
        use crate::param::EnumEntry;

        let mut base = PortDriverBase::new("test_enum", 1, PortFlags::default());
        base.create_param("MODE", ParamType::Enum).unwrap();

        struct EnumDriver {
            base: PortDriverBase,
        }
        impl PortDriver for EnumDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let mut drv = EnumDriver { base };
        let choices: Arc<[EnumEntry]> = Arc::from(vec![
            EnumEntry {
                string: "Off".into(),
                value: 0,
                severity: 0,
            },
            EnumEntry {
                string: "On".into(),
                value: 1,
                severity: 0,
            },
        ]);
        let mut user = AsynUser::new(0);
        drv.write_enum_choices(&mut user, choices).unwrap();
        drv.write_enum(&mut user, 1).unwrap();
        let (idx, ch) = drv.read_enum(&AsynUser::new(0)).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(ch[1].string, "On");
    }

    #[test]
    fn test_enum_callback() {
        use crate::param::{EnumEntry, ParamValue};

        let mut base = PortDriverBase::new("test_enum_cb", 1, PortFlags::default());
        base.create_param("MODE", ParamType::Enum).unwrap();
        let mut rx = base.interrupts.subscribe_async();

        struct EnumDriver {
            base: PortDriverBase,
        }
        impl PortDriver for EnumDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let mut drv = EnumDriver { base };
        let choices: Arc<[EnumEntry]> = Arc::from(vec![
            EnumEntry {
                string: "A".into(),
                value: 0,
                severity: 0,
            },
            EnumEntry {
                string: "B".into(),
                value: 1,
                severity: 0,
            },
        ]);
        drv.base_mut()
            .set_enum_choices_param(0, 0, choices)
            .unwrap();
        drv.base_mut().set_enum_index_param(0, 0, 1).unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();

        let v = rx.try_recv().unwrap();
        assert_eq!(v.reason, 0);
        assert!(matches!(v.value, ParamValue::Enum { index: 1, .. }));
    }

    #[test]
    fn test_default_read_write_generic_pointer() {
        let mut base = PortDriverBase::new("test_gp", 1, PortFlags::default());
        base.create_param("PTR", ParamType::GenericPointer).unwrap();

        struct GpDriver {
            base: PortDriverBase,
        }
        impl PortDriver for GpDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let mut drv = GpDriver { base };
        let data: Arc<dyn std::any::Any + Send + Sync> = Arc::new(99i32);
        let mut user = AsynUser::new(0);
        drv.write_generic_pointer(&mut user, data).unwrap();
        let val = drv.read_generic_pointer(&AsynUser::new(0)).unwrap();
        assert_eq!(*val.downcast_ref::<i32>().unwrap(), 99);
    }

    #[test]
    fn test_generic_pointer_callback() {
        use crate::param::ParamValue;

        let mut base = PortDriverBase::new("test_gp_cb", 1, PortFlags::default());
        base.create_param("PTR", ParamType::GenericPointer).unwrap();
        let mut rx = base.interrupts.subscribe_async();

        struct GpDriver {
            base: PortDriverBase,
        }
        impl PortDriver for GpDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let mut drv = GpDriver { base };
        let data: Arc<dyn std::any::Any + Send + Sync> = Arc::new(vec![1, 2, 3]);
        drv.base_mut()
            .set_generic_pointer_param(0, 0, data)
            .unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();

        let v = rx.try_recv().unwrap();
        assert_eq!(v.reason, 0);
        assert!(matches!(v.value, ParamValue::GenericPointer(_)));
    }

    #[test]
    fn test_interpose_push_requires_lock() {
        use crate::interpose::{OctetInterpose, OctetNext, OctetReadResult};
        use parking_lot::Mutex;
        use std::sync::Arc;

        struct NoopInterpose;
        impl OctetInterpose for NoopInterpose {
            fn read(
                &mut self,
                user: &AsynUser,
                buf: &mut [u8],
                next: &mut dyn OctetNext,
            ) -> AsynResult<OctetReadResult> {
                next.read(user, buf)
            }
            fn write(
                &mut self,
                user: &mut AsynUser,
                data: &[u8],
                next: &mut dyn OctetNext,
            ) -> AsynResult<usize> {
                next.write(user, data)
            }
            fn flush(&mut self, user: &mut AsynUser, next: &mut dyn OctetNext) -> AsynResult<()> {
                next.flush(user)
            }
        }

        let port: Arc<Mutex<dyn PortDriver>> = Arc::new(Mutex::new(TestDriver::new()));

        {
            let mut guard = port.lock();
            guard
                .base_mut()
                .install_octet_interpose(Box::new(NoopInterpose));
            assert_eq!(guard.base().interpose_octet.len(), 1);
        }
    }

    /// The `set_input_eos` write owner must forward the terminator to an
    /// installed `EosInterpose`, not just cache it in `base.input_eos` —
    /// otherwise a runtime IEOS change never terminates reads (the F7 gap).
    #[test]
    fn test_set_input_eos_reaches_installed_interpose() {
        use crate::interpose::eos::EosInterpose;
        use crate::interpose::{EomReason, OctetNext, OctetReadResult};

        struct RawSource {
            data: Vec<u8>,
            pos: usize,
        }
        impl OctetNext for RawSource {
            fn read(&mut self, _u: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
                let avail = self.data.len() - self.pos;
                let n = avail.min(buf.len());
                buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                self.pos += n;
                Ok(OctetReadResult {
                    nbytes_transferred: n,
                    eom_reason: EomReason::CNT,
                })
            }
            fn write(&mut self, _u: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                Ok(data.len())
            }
            fn flush(&mut self, _u: &mut AsynUser) -> AsynResult<()> {
                Ok(())
            }
        }

        let mut drv = TestDriver::new();
        drv.base_mut()
            .install_octet_interpose(Box::new(EosInterpose::default()));

        // Set IEOS through the driver trait: caches in base AND must reach
        // the interpose.
        drv.set_input_eos(&AsynUser::default(), b"\n").unwrap();
        assert_eq!(drv.base().input_eos(0), b"\n");

        let user = AsynUser::default();
        // "ab\n" exactly: the EOS read returns "ab" and leaves no read-ahead
        // in the interpose buffer, so the cleared-EOS read below genuinely
        // reads the next source fresh.
        let mut src = RawSource {
            data: b"ab\n".to_vec(),
            pos: 0,
        };
        let mut buf = [0u8; 16];
        let r = drv
            .base_mut()
            .interpose_octet
            .dispatch_read(&user, &mut buf, &mut src)
            .unwrap();
        assert_eq!(&buf[..r.nbytes_transferred], b"ab");
        assert!(r.eom_reason.contains(EomReason::EOS));

        // Clearing IEOS (binary-suppress path) must also reach the interpose:
        // the read then passes through with no EOS termination.
        drv.set_input_eos(&AsynUser::default(), b"").unwrap();
        assert_eq!(drv.base().input_eos(0), b"");
        let mut src2 = RawSource {
            data: b"xy\nz".to_vec(),
            pos: 0,
        };
        let mut buf2 = [0u8; 16];
        let r2 = drv
            .base_mut()
            .interpose_octet
            .dispatch_read(&user, &mut buf2, &mut src2)
            .unwrap();
        assert_eq!(&buf2[..r2.nbytes_transferred], b"xy\nz");
        assert!(!r2.eom_reason.contains(EomReason::EOS));
    }

    /// R14-49: the EOS hooks take the `asynUser`, so a multi-device port holds
    /// one terminator per device — C's `eosPvt` is created per (port, addr)
    /// (asynInterposeEos.c:84-120) and every hook takes the user that selects it
    /// (:288-296). A port-wide pair could not answer two devices.
    #[test]
    fn each_device_on_a_multi_device_port_holds_its_own_eos() {
        struct MultiDriver {
            base: PortDriverBase,
        }
        impl PortDriver for MultiDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }

            // A multiDevice port cannot carry `asynInterposeEos` — C refuses
            // to install it (asynOctetBase.c:149-153) — so such a driver owns
            // its EOS methods, which is the non-NULL `asynOctet` entry the
            // Fail-stub default stands in for (R18-71).
            fn set_input_eos(&mut self, user: &AsynUser, eos: &[u8]) -> AsynResult<()> {
                self.base.set_input_eos(user.addr, eos);
                Ok(())
            }
            fn set_output_eos(&mut self, user: &AsynUser, eos: &[u8]) -> AsynResult<()> {
                self.base.set_output_eos(user.addr, eos);
                Ok(())
            }
        }
        let mut drv = MultiDriver {
            base: PortDriverBase::new(
                "eos_multi",
                4,
                PortFlags {
                    multi_device: true,
                    ..PortFlags::default()
                },
            ),
        };

        let dev1 = AsynUser::default().with_addr(1);
        let dev2 = AsynUser::default().with_addr(2);
        drv.set_input_eos(&dev1, b"\n").unwrap();
        drv.set_output_eos(&dev1, b"\r\n").unwrap();
        drv.set_input_eos(&dev2, b";").unwrap();

        assert_eq!(drv.get_input_eos(&dev1), b"\n");
        assert_eq!(drv.get_input_eos(&dev2), b";");
        assert_eq!(drv.get_output_eos(&dev1), b"\r\n");
        // A device that was never configured has no terminator — C's
        // zero-initialised `eosInLen`.
        assert!(
            drv.get_input_eos(&AsynUser::default().with_addr(3))
                .is_empty()
        );
        assert!(drv.get_output_eos(&dev2).is_empty());
    }

    /// The other boundary: a port that never declared `ASYN_MULTIDEVICE` has no
    /// devices to key by — C's `findDpCommon` (asynManager.c:536-544) and
    /// `findInterface` resolve *every* addr to the port itself, so
    /// `asynSetEos(port, -1, ...)` and a record at ADDR 0 must reach the same
    /// terminator. Splitting them by raw addr would leave the record reading
    /// with no EOS at all.
    #[test]
    fn a_single_device_port_collapses_every_addr_onto_one_eos() {
        let mut drv = TestDriver::new();
        // A single-device port takes its EOS from `asynInterposeEos`, which C
        // installs above the driver when the port is configured with
        // `processEosIn/Out` (asynOctetBase.c:170-172); without it the
        // driver's own NULL method answers "not implemented" (R18-71).
        drv.base
            .install_octet_interpose(Box::new(crate::interpose::eos::EosInterpose::default()));
        drv.set_input_eos(&AsynUser::default().with_addr(-1), b"\n")
            .unwrap();
        assert_eq!(drv.get_input_eos(&AsynUser::default().with_addr(0)), b"\n");
        assert_eq!(drv.get_input_eos(&AsynUser::default().with_addr(7)), b"\n");
    }

    /// R18-71. C's `asynOctetBase::initOverride` (asynOctetBase.c:115-118)
    /// replaces a NULL `setInputEos`/`setOutputEos` with a Fail stub, and the
    /// stub calls `showFailure` (:370-380) — `asynError` and
    /// `"<port> <method> not implemented"`. A `noProcessEos` port therefore
    /// *refuses* a terminator; it does not accept one and then never
    /// terminate. The two boundaries are the whole rule: no layer at all, and
    /// a layer that takes only one direction (`asynInterposeEosConfig` stores
    /// both flags, asynInterposeEos.c:128-138). C delegates only the *input*
    /// direction down when `processEosIn == 0` — setInputEos :293-295,
    /// getInputEos :318-320; its output pair (:344-363, :365-390) has no
    /// `processEosOut` test and stores unconditionally, that flag gating
    /// `writeIt` alone (:161-163).
    #[test]
    fn a_port_with_no_eos_layer_refuses_a_terminator() {
        use crate::interpose::eos::{EosConfig, EosInterpose};

        // Boundary 1: no layer took it — both directions are Fail stubs.
        let mut bare = TestDriver::new();
        let err = bare
            .set_input_eos(&AsynUser::default(), b"\n")
            .expect_err("a noProcessEos port must refuse IEOS");
        assert_eq!(
            err.to_string(),
            AsynError::Status {
                status: AsynStatus::Error,
                message: "test setInputEos not implemented".to_string(),
            }
            .to_string()
        );
        let err = bare
            .set_output_eos(&AsynUser::default(), b"\r\n")
            .expect_err("a noProcessEos port must refuse OEOS");
        assert!(
            err.to_string().contains("setOutputEos not implemented"),
            "{err}"
        );
        // Refused means nothing was cached: a later read must not silently
        // believe the port terminates.
        assert!(bare.get_input_eos(&AsynUser::default()).is_empty());
        assert!(bare.get_output_eos(&AsynUser::default()).is_empty());

        // Boundary 2: the layer takes input only — and OEOS still lands.
        // C's `setOutputEos` (:344-363) has no `processEosOut` test; the flag
        // gates `writeIt` alone (:161-163), so the terminator is stored, read
        // back, and simply never appended.
        let mut half = TestDriver::new();
        half.base_mut()
            .install_octet_interpose(Box::new(EosInterpose::with_processing(
                EosConfig::default(),
                true,
                false,
            )));
        half.set_input_eos(&AsynUser::default(), b"\n").unwrap();
        assert_eq!(half.get_input_eos(&AsynUser::default()), b"\n");
        half.set_output_eos(&AsynUser::default(), b"\r\n")
            .expect("processOut = 0 still stores OEOS");
        assert_eq!(half.get_output_eos(&AsynUser::default()), b"\r\n");

        // Boundary 3: a length the storing layer refuses. C's `switch
        // (eoslen)` answers `"<port> illegal eoslen <n>"` *before* assigning
        // (:299-310, :352-362), so the terminator already stored still stands.
        let err = half
            .set_output_eos(&AsynUser::default(), b"\r\n\0")
            .expect_err("eoslen 3 is past eosOut[2]");
        assert!(err.to_string().contains("test illegal eoslen 3"), "{err}");
        assert_eq!(half.get_output_eos(&AsynUser::default()), b"\r\n");
        let err = half
            .set_input_eos(&AsynUser::default(), b"abc")
            .expect_err("eoslen 3 is past eosIn[2]");
        assert!(err.to_string().contains("test illegal eoslen 3"), "{err}");
        assert_eq!(half.get_input_eos(&AsynUser::default()), b"\n");

        // Boundary 4: no layer at all refuses every length with the Fail
        // stub's words, not the length rule's — the rule belongs to the layer
        // that stores, and there is none.
        let err = bare
            .set_output_eos(&AsynUser::default(), b"\r\n\0")
            .expect_err("a noProcessEos port refuses whatever the length");
        assert!(
            err.to_string().contains("setOutputEos not implemented"),
            "{err}"
        );
    }

    /// R6-46 owner path: `set_connected` is the single transition owner, so
    /// every driver that reconnects through it (serial, IP, prologix …) gets
    /// the interpose reset for free. C wires this as an exception callback
    /// (`asynInterposeEos.c:110,142-151`); here the owner drives the stack
    /// directly. Boundaries: both edges reset (C's `asynExceptionConnect`
    /// fires from `exceptionConnect` AND `exceptionDisconnect`), and a
    /// no-op call (same state) must not.
    #[test]
    fn set_connected_resets_interpose_link_state() {
        use crate::interpose::{OctetInterpose, OctetNext, OctetReadResult};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingInterpose(Arc<AtomicUsize>);
        impl OctetInterpose for CountingInterpose {
            fn read(
                &mut self,
                user: &AsynUser,
                buf: &mut [u8],
                next: &mut dyn OctetNext,
            ) -> AsynResult<OctetReadResult> {
                next.read(user, buf)
            }
            fn write(
                &mut self,
                user: &mut AsynUser,
                data: &[u8],
                next: &mut dyn OctetNext,
            ) -> AsynResult<usize> {
                next.write(user, data)
            }
            fn flush(&mut self, user: &mut AsynUser, next: &mut dyn OctetNext) -> AsynResult<()> {
                next.flush(user)
            }
            fn connection_changed(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let resets = Arc::new(AtomicUsize::new(0));
        let mut base = PortDriverBase::new("reset_test", 1, PortFlags::default());
        base.install_octet_interpose(Box::new(CountingInterpose(resets.clone())));

        // Port starts connected. Disconnect edge → reset (C exceptionDisconnect).
        assert!(base.set_connected(false));
        assert_eq!(resets.load(Ordering::Relaxed), 1);

        // Redundant call, no state change → no fan-out, no reset.
        assert!(!base.set_connected(false));
        assert_eq!(resets.load(Ordering::Relaxed), 1);

        // Reconnect edge → reset again (C exceptionConnect).
        assert!(base.set_connected(true));
        assert_eq!(resets.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_default_read_write_int64() {
        let mut base = PortDriverBase::new("test_i64", 1, PortFlags::default());
        base.create_param("BIG", ParamType::Int64).unwrap();

        struct I64Driver {
            base: PortDriverBase,
        }
        impl PortDriver for I64Driver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let mut drv = I64Driver { base };
        let mut user = AsynUser::new(0);
        drv.write_int64(&mut user, i64::MAX).unwrap();
        assert_eq!(drv.read_int64(&AsynUser::new(0)).unwrap(), i64::MAX);
    }

    #[test]
    fn test_get_bounds_int64_default() {
        let base = PortDriverBase::new("test_bounds", 1, PortFlags::default());
        struct BoundsDriver {
            base: PortDriverBase,
        }
        impl PortDriver for BoundsDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }
        let drv = BoundsDriver { base };
        let (lo, hi) = drv.get_bounds_int64(&AsynUser::default()).unwrap();
        // C asynInt64Base.c:99 default: *low = *high = 0 (so a driver
        // that does not implement getBounds skips LINEAR ESLO/EOFF).
        assert_eq!(lo, 0);
        assert_eq!(hi, 0);
    }

    #[test]
    fn test_per_addr_device_state() {
        let mut base = PortDriverBase::new(
            "multi",
            4,
            PortFlags {
                multi_device: true,
                can_block: false,
                destructible: true,
            },
        );
        base.create_param("V", ParamType::Int32).unwrap();

        // Default: all connected
        assert!(base.is_device_connected(0));
        assert!(base.is_device_connected(1));

        // Disable addr 1
        base.device_state(1).enabled = false;
        assert!(base.check_ready_addr(0).is_ok());
        let err = base.check_ready_addr(1).unwrap_err();
        assert!(format!("{err}").contains("not enabled"));

        // Disconnect addr 2
        base.device_state(2).connected = false;
        let err = base.check_ready_addr(2).unwrap_err();
        assert!(format!("{err}").contains("not connected"));
    }

    #[test]
    fn test_per_addr_single_device_ignored() {
        let mut base = PortDriverBase::new("single", 1, PortFlags::default());
        base.create_param("V", ParamType::Int32).unwrap();
        // For single-device, per-addr check passes even if no device state
        assert!(base.check_ready_addr(0).is_ok());
    }

    #[test]
    fn test_timestamp_source() {
        let mut base = PortDriverBase::new("ts_test", 1, PortFlags::default());
        base.create_param("V", ParamType::Int32).unwrap();

        let fixed_ts = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(999999);
        base.register_timestamp_source(move || fixed_ts);

        assert_eq!(base.current_timestamp(), fixed_ts);
    }

    #[test]
    fn test_timestamp_source_in_callbacks() {
        let mut base = PortDriverBase::new("ts_cb", 1, PortFlags::default());
        base.create_param("V", ParamType::Int32).unwrap();
        let mut rx = base.interrupts.subscribe_async();

        let fixed_ts = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(123456);
        base.register_timestamp_source(move || fixed_ts);

        struct TsDriver {
            base: PortDriverBase,
        }
        impl PortDriver for TsDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }
        let mut drv = TsDriver { base };
        drv.base_mut().set_int32_param(0, 0, 42).unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();

        let v = rx.try_recv().unwrap();
        // Should use fixed_ts since no per-param timestamp is set
        assert_eq!(v.timestamp, fixed_ts);
    }

    #[test]
    fn test_queue_priority_connect() {
        assert!(QueuePriority::Connect > QueuePriority::High);
    }

    #[test]
    fn test_port_flags_destructible_default_is_opt_in() {
        // C asyn parity: ASYN_DESTRUCTIBLE (0x0004, asynDriver.h:97) is
        // a `registerPort` attribute that callers opt into. Default
        // must be false so drivers don't accidentally accept a
        // shutdownPort call. PortDriver authors that want shutdown
        // support set `destructible: true` explicitly.
        let flags = PortFlags::default();
        assert!(
            !flags.destructible,
            "destructible must be opt-in (C parity)"
        );
    }

    #[test]
    fn shutdown_lifecycle_refuses_non_destructible() {
        let mut base = PortDriverBase::new(
            "p_nondestr",
            1,
            PortFlags {
                multi_device: false,
                can_block: false,
                destructible: false,
            },
        );
        match base.shutdown_lifecycle() {
            Err(AsynError::Status { message, .. }) => {
                assert!(message.contains("ASYN_DESTRUCTIBLE"), "msg={message}");
            }
            other => panic!("expected ASYN_DESTRUCTIBLE refusal, got {other:?}"),
        }
        assert!(
            !base.is_defunct(),
            "non-destructible port must not flip defunct"
        );
        assert!(base.is_enabled(), "non-destructible port must stay enabled");
    }

    #[test]
    fn shutdown_lifecycle_marks_destructible_defunct_and_idempotent() {
        let mut base = PortDriverBase::new(
            "p_destr",
            1,
            PortFlags {
                multi_device: false,
                can_block: false,
                destructible: true,
            },
        );
        assert!(base.is_enabled());
        assert!(!base.is_defunct());
        base.shutdown_lifecycle().unwrap();
        assert!(
            !base.is_enabled(),
            "shutdown_lifecycle must flip enabled=false"
        );
        assert!(
            base.is_defunct(),
            "shutdown_lifecycle must flip defunct=true"
        );
        // Idempotent — second call is Ok and leaves state unchanged.
        base.shutdown_lifecycle().unwrap();
        assert!(base.is_defunct());
        // A shut-down port is refused by the queue gate as a *disabled* one:
        // C's `queueRequest` has no defunct branch (asynManager.c:1541-1552),
        // it only sees the `enabled=FALSE` that `shutdownPort` left behind
        // (:2283). "port %s disabled" is the message an operator gets.
        match base.check_ready() {
            Err(AsynError::Status { status, message }) => {
                assert_eq!(status, AsynStatus::Disabled);
                assert_eq!(message, "port p_destr disabled");
            }
            other => panic!("expected the disabled refusal, got {other:?}"),
        }
        // The one place C names the shutdown is `enable` (:2236-2241).
        match base.set_enabled(true) {
            Err(AsynError::Status { status, message }) => {
                assert_eq!(status, AsynStatus::Disabled);
                assert_eq!(message, "asynManager:enable: port has been shut down");
            }
            other => panic!("expected the shut-down refusal, got {other:?}"),
        }
    }

    // --- Phase 2B: per-addr connect/disconnect/enable/disable ---

    #[test]
    fn test_connect_addr() {
        let mut base = PortDriverBase::new(
            "multi_conn",
            4,
            PortFlags {
                multi_device: true,
                can_block: false,
                destructible: true,
            },
        );
        base.create_param("V", ParamType::Int32).unwrap();

        base.disconnect_addr(1);
        assert!(!base.is_device_connected(1));
        assert!(base.check_ready_addr(1).is_err());

        base.connect_addr(1);
        assert!(base.is_device_connected(1));
        assert!(base.check_ready_addr(1).is_ok());
    }

    #[test]
    fn test_enable_disable_addr() {
        let mut base = PortDriverBase::new(
            "multi_en",
            4,
            PortFlags {
                multi_device: true,
                can_block: false,
                destructible: true,
            },
        );
        base.create_param("V", ParamType::Int32).unwrap();

        base.disable_addr(2);
        let err = base.check_ready_addr(2).unwrap_err();
        assert!(format!("{err}").contains("not enabled"));

        base.enable_addr(2);
        assert!(base.check_ready_addr(2).is_ok());
    }

    #[test]
    fn test_port_level_overrides_addr() {
        let mut base = PortDriverBase::new(
            "multi_override",
            4,
            PortFlags {
                multi_device: true,
                can_block: false,
                destructible: true,
            },
        );
        base.create_param("V", ParamType::Int32).unwrap();

        // Port-level disabled overrides addr-level enabled
        base.enabled = false;
        base.enable_addr(0); // addr 0 is enabled, but port is disabled
        let err = base.check_ready_addr(0).unwrap_err();
        assert!(format!("{err}").contains("disabled"));
    }

    #[test]
    fn test_per_addr_exception_announced() {
        use std::sync::atomic::{AtomicI32, Ordering};

        let mut base = PortDriverBase::new(
            "multi_exc",
            4,
            PortFlags {
                multi_device: true,
                can_block: false,
                destructible: true,
            },
        );
        base.create_param("V", ParamType::Int32).unwrap();

        let exc_mgr = Arc::new(crate::exception::ExceptionManager::new());
        base.bind_exception_sink(exc_mgr.clone());

        let last_addr = Arc::new(AtomicI32::new(-99));
        let last_addr2 = last_addr.clone();
        exc_mgr.add_callback(move |event| {
            last_addr2.store(event.addr, Ordering::Relaxed);
        });

        base.disconnect_addr(3);
        assert_eq!(last_addr.load(Ordering::Relaxed), 3);

        base.enable_addr(2);
        assert_eq!(last_addr.load(Ordering::Relaxed), 2);
    }

    /// C parity (asynManager.c:2151-2160 exceptionConnect,
    /// :2174-2185 exceptionDisconnect): redundant connect/disconnect
    /// on a port already in that state must NOT fan out a duplicate
    /// `asynExceptionConnect`. Subscribers depend on the event
    /// edge — duplicate fan-out causes them to e.g. re-subscribe or
    /// re-arm timers that should fire exactly once per transition.
    #[test]
    fn test_connect_disconnect_announce_only_on_transition() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut base = PortDriverBase::new(
            "edge",
            4,
            PortFlags {
                multi_device: true,
                can_block: false,
                destructible: true,
            },
        );
        base.create_param("V", ParamType::Int32).unwrap();
        let exc_mgr = Arc::new(crate::exception::ExceptionManager::new());
        base.bind_exception_sink(exc_mgr.clone());

        let connect_hits = Arc::new(AtomicUsize::new(0));
        let hits2 = connect_hits.clone();
        exc_mgr.add_callback(move |event| {
            if event.exception == AsynException::Connect {
                hits2.fetch_add(1, Ordering::Relaxed);
            }
        });

        // device starts connected by DeviceState::default — a redundant
        // connect_addr is a no-op.
        base.connect_addr(2);
        assert_eq!(
            connect_hits.load(Ordering::Relaxed),
            0,
            "redundant connect_addr must not fan out"
        );

        // First transition fires once.
        base.disconnect_addr(2);
        assert_eq!(connect_hits.load(Ordering::Relaxed), 1);

        // Redundant disconnect is silent.
        base.disconnect_addr(2);
        assert_eq!(
            connect_hits.load(Ordering::Relaxed),
            1,
            "redundant disconnect_addr must not fan out"
        );

        // Re-connect fires the transition.
        base.connect_addr(2);
        assert_eq!(connect_hits.load(Ordering::Relaxed), 2);
    }

    /// C parity: `autoConnectAsyn` (asynManager.c:2310-2324) fires
    /// `asynExceptionAutoConnect` unconditionally — even setting the
    /// same value as the current one. Rust mirrors that so observers
    /// can refresh their UI after a re-confirmation, not just an edge.
    #[test]
    fn test_set_auto_connect_fires_unconditionally() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut base = PortDriverBase::new("ac", 1, PortFlags::default());
        let exc_mgr = Arc::new(crate::exception::ExceptionManager::new());
        base.bind_exception_sink(exc_mgr.clone());
        let hits = Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        exc_mgr.add_callback(move |event| {
            if event.exception == AsynException::AutoConnect {
                hits2.fetch_add(1, Ordering::Relaxed);
            }
        });
        // base.auto_connect defaults to true — setting true again
        // still must fire (no state-change guard in C).
        base.set_auto_connect(true);
        base.set_auto_connect(false);
        base.set_auto_connect(false);
        assert_eq!(hits.load(Ordering::Relaxed), 3);
    }

    /// asyn PR #217 (asynManager.c:2322-2324): enabling auto-connect on a
    /// down port/device arms the connect timer; a connected port, or a
    /// flip to OFF, arms nothing. Pre-fix no flip armed the timer, so a
    /// never-connected port enabled late made one exception-wake attempt
    /// and then sat silent.
    #[test]
    fn auto_connect_enable_arms_connect_timer_boundaries() {
        // dropped while auto-connect was OFF (the disconnect edge arms
        // nothing then, port.rs sync_connection_edge), enabled late →
        // the flip itself must arm
        let mut base = PortDriverBase::new("act", 1, PortFlags::default());
        base.set_auto_connect(false);
        base.set_connected(false);
        assert!(base.connect_retry_at.is_none());
        base.set_auto_connect(true);
        assert!(base.connect_retry_at.is_some());

        // down + OFF → untouched (C's timer callback no-ops on
        // !autoConnect; the flip itself must not arm)
        let mut base = PortDriverBase::new("act2", 1, PortFlags::default());
        base.set_auto_connect(false);
        base.set_connected(false);
        base.set_auto_connect(false);
        assert!(base.connect_retry_at.is_none());

        // connected + ON → nothing to retry (a fresh base is born
        // `Connection::Own(true)`)
        let mut base = PortDriverBase::new("act3", 1, PortFlags::default());
        base.set_auto_connect(true);
        assert!(base.connect_retry_at.is_none());

        // device down + ON via the addr variant → the PORT timer arms,
        // keyed on the device's dpCommon exactly as C's findDpCommon
        // resolution is; the port itself stays connected
        let multi = PortFlags {
            multi_device: true,
            ..PortFlags::default()
        };
        let mut base = PortDriverBase::new("act4", 2, multi);
        base.device_state(1).connected = false;
        assert!(base.connect_retry_at.is_none());
        base.set_auto_connect_addr(1, true);
        assert!(base.connect_retry_at.is_some());

        // the same call on a SINGLE-device port is a port write, because
        // `findDpCommon` has no device to give it (asynManager.c:541-544): the
        // port is connected, so it arms nothing at all.
        let mut base = PortDriverBase::new("act5", 2, PortFlags::default());
        base.set_auto_connect_addr(1, true);
        assert!(base.connect_retry_at.is_none());
        assert!(
            base.dp_auto_connect(1),
            "and it landed on the port's own slot"
        );
    }

    #[test]
    fn auto_connect_throttle_gate_boundaries() {
        // C autoConnectDevice 2.0s gate (asynManager.c:712-713). Boundary
        // cases, not narrative: never-stamped, exactly-2s, just-under-2s.
        let mut base = PortDriverBase::new("thr", 1, PortFlags::default());

        // No transition recorded yet => always permitted (C's
        // zero-initialised lastConnectDisconnect).
        let t0 = Instant::now();
        assert!(base.auto_connect_throttle_ok(-1, t0));

        // Stamp at t0; only `+` arithmetic on Instant (no `- Duration`,
        // which panics on Windows when uptime < the span).
        base.last_connect_disconnect = Some(t0);
        // elapsed 0 < 2s => refused.
        assert!(!base.auto_connect_throttle_ok(-1, t0));
        // elapsed just under 2s => refused.
        assert!(!base.auto_connect_throttle_ok(-1, t0 + Duration::from_millis(1999)));
        // elapsed exactly 2s => permitted (>=).
        assert!(base.auto_connect_throttle_ok(-1, t0 + Duration::from_secs(2)));
        // elapsed well past => permitted.
        assert!(base.auto_connect_throttle_ok(-1, t0 + Duration::from_secs(5)));
    }

    #[test]
    fn auto_connect_throttle_stamps_on_disconnect_not_connect() {
        // C exceptionDisconnect stamps lastConnectDisconnect (asynManager.c
        // :2184); exceptionConnect does not (:2157-2159). Mirror both edges.
        let mut base = PortDriverBase::new("thr", 1, PortFlags::default());
        // Starts connected, no stamp.
        assert!(base.last_connect_disconnect.is_none());

        // Disconnect edge stamps.
        assert!(base.set_connected(false));
        assert!(base.last_connect_disconnect.is_some());

        // Clear, then connect edge must NOT re-stamp.
        base.last_connect_disconnect = None;
        assert!(base.set_connected(true));
        assert!(base.last_connect_disconnect.is_none());
    }

    #[test]
    fn auto_connect_throttle_per_device_anchor() {
        // Multi-device ports throttle per address (C dpCommon is per-device).
        let flags = PortFlags {
            multi_device: true,
            ..PortFlags::default()
        };
        let mut base = PortDriverBase::new("thr", 4, flags);
        let t0 = Instant::now();

        // addr 1 disconnect stamps only addr 1's anchor.
        assert!(base.set_addr_connected(1, false));
        assert!(base.device_state(1).last_connect_disconnect.is_some());
        // addr 1 is throttled; addr 2 (never stamped) is still permitted.
        assert!(!base.auto_connect_throttle_ok(1, t0));
        assert!(base.auto_connect_throttle_ok(2, t0));

        // Post-attempt stamp restarts addr 2's window.
        base.stamp_auto_connect_attempt(2, t0);
        assert!(!base.auto_connect_throttle_ok(2, t0));
        assert!(base.auto_connect_throttle_ok(2, t0 + Duration::from_secs(2)));
    }

    #[test]
    fn set_enabled_refuses_defunct_port() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // C `enable` on a defunct port: asynDisabled, no `enabled` toggle,
        // no asynExceptionEnable fan-out (asynManager.c:2236-2241).
        let flags = PortFlags {
            destructible: true,
            ..PortFlags::default()
        };
        let mut base = PortDriverBase::new("def", 1, flags);
        let exc_mgr = Arc::new(crate::exception::ExceptionManager::new());
        base.bind_exception_sink(exc_mgr.clone());
        let enable_hits = Arc::new(AtomicUsize::new(0));
        let h = enable_hits.clone();
        exc_mgr.add_callback(move |event| {
            if event.exception == AsynException::Enable {
                h.fetch_add(1, Ordering::Relaxed);
            }
        });

        // Shut the port down → defunct (shutdown sets enabled=false).
        base.shutdown_lifecycle().unwrap();
        assert!(base.is_defunct());
        assert!(!base.is_enabled());

        let err = base.set_enabled(true).unwrap_err();
        match err {
            AsynError::Status { status, .. } => assert_eq!(status, AsynStatus::Disabled),
            other => panic!("expected Disabled, got {other:?}"),
        }
        assert!(!base.is_enabled(), "defunct port must not re-enable");
        assert_eq!(
            enable_hits.load(Ordering::Relaxed),
            0,
            "no Enable exception may fire on a defunct port"
        );
    }

    #[test]
    fn set_addr_enabled_refuses_defunct_port() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let flags = PortFlags {
            multi_device: true,
            destructible: true,
            ..PortFlags::default()
        };
        let mut base = PortDriverBase::new("def", 4, flags);
        let exc_mgr = Arc::new(crate::exception::ExceptionManager::new());
        base.bind_exception_sink(exc_mgr.clone());
        let enable_hits = Arc::new(AtomicUsize::new(0));
        let h = enable_hits.clone();
        exc_mgr.add_callback(move |event| {
            if event.exception == AsynException::Enable {
                h.fetch_add(1, Ordering::Relaxed);
            }
        });

        base.shutdown_lifecycle().unwrap();

        let err = base.set_addr_enabled(1, false).unwrap_err();
        match err {
            AsynError::Status { status, .. } => assert_eq!(status, AsynStatus::Disabled),
            other => panic!("expected Disabled, got {other:?}"),
        }
        // The guard returns before `device_state(addr)` would insert an
        // entry, so the refused call mutates no per-device state.
        assert!(
            !base.device_states.contains_key(&1),
            "refused per-device enable must not create device state"
        );
        // The `()` convenience facade also no-ops on a defunct port.
        base.disable_addr(1);
        assert!(!base.device_states.contains_key(&1));
        assert_eq!(
            enable_hits.load(Ordering::Relaxed),
            0,
            "no Enable exception may fire on a defunct port"
        );
    }

    /// The `defunct ⟹ !enabled` invariant is over every dpCommon the port
    /// owns, so `dp_enabled` must answer the same at every address. The two
    /// boundaries are the two ways a device slot can be in the map at
    /// shutdown time: one that already existed, and one created afterwards.
    #[test]
    fn shutdown_lifecycle_disables_every_device_dp_common() {
        let flags = PortFlags {
            multi_device: true,
            destructible: true,
            ..PortFlags::default()
        };

        // Boundary 1: a slot that exists before the shutdown.
        let mut base = PortDriverBase::new("shut", 4, flags);
        base.set_addr_enabled(1, true).unwrap();
        assert!(base.device_states.contains_key(&1));
        base.shutdown_lifecycle().unwrap();
        assert!(!base.dp_enabled(1), "pre-existing device slot must go down");
        assert!(!base.dp_enabled(-1), "the port's own dpCommon goes down");
        assert!(!base.dp_enabled(0));

        // Boundary 2: no slot at shutdown, one created afterwards — the seed
        // in `device_state` takes the port's `enabled`, which is now false.
        let mut base = PortDriverBase::new("shut2", 4, flags);
        base.shutdown_lifecycle().unwrap();
        assert!(!base.dp_enabled(1));
        assert!(
            !base.device_state(1).enabled,
            "a slot seeded after the shutdown must come up disabled"
        );
    }

    /// C parity: `asynPortDriver::setInterruptUInt32Digital` /
    /// `clearInterruptUInt32Digital` / `getInterruptUInt32Digital`
    /// (`asynPortDriver.cpp:2346-2461`) route through paramList. The
    /// PortDriver trait default delegates to the param store; we
    /// verify the round-trip end-to-end through the trait surface.
    #[test]
    fn test_port_driver_uint32_interrupt_round_trip() {
        struct UInt32Drv {
            base: PortDriverBase,
        }
        impl PortDriver for UInt32Drv {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let mut base = PortDriverBase::new("uint32_int", 1, PortFlags::default());
        let idx = base
            .params
            .create_param("BITS", ParamType::UInt32Digital)
            .unwrap();
        let mut drv = UInt32Drv { base };
        let user = AsynUser::new(idx).with_addr(0);

        drv.set_interrupt_uint32_digital(&user, 0xF0, InterruptReason::ZeroToOne)
            .unwrap();
        drv.set_interrupt_uint32_digital(&user, 0x0F, InterruptReason::OneToZero)
            .unwrap();
        assert_eq!(
            drv.get_interrupt_uint32_digital(&user, InterruptReason::Both)
                .unwrap(),
            0xFF
        );
        drv.clear_interrupt_uint32_digital(&user, 0x11).unwrap();
        assert_eq!(
            drv.get_interrupt_uint32_digital(&user, InterruptReason::ZeroToOne)
                .unwrap(),
            0xE0
        );
        assert_eq!(
            drv.get_interrupt_uint32_digital(&user, InterruptReason::OneToZero)
                .unwrap(),
            0x0E
        );
    }

    /// C parity: the default `read_int32` / `read_int64` / `read_float64` /
    /// `read_octet` / `read_uint32_digital` must surface an *unset*
    /// parameter as `ParamUndefined`, not success/0. The default
    /// `asynPortDriver::read{Int32,Int64,Float64,Octet,UInt32Digital}` calls
    /// `get{Integer,Integer64,Double,String,UIntDigital}Param`, every
    /// `paramVal` getter throws `ParamValNotDefined` → `asynParamUndefined`
    /// for an unset value (paramVal.cpp:152,181,235,264,292), and the
    /// `devAsyn*` device support routes that status through
    /// `asynStatusToEpicsAlarm(READ_ALARM, INVALID_ALARM)`. After a write
    /// the same reads succeed with the stored value.
    #[test]
    fn default_scalar_reads_report_undefined_until_set() {
        struct AllTypesDrv {
            base: PortDriverBase,
        }
        impl PortDriver for AllTypesDrv {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let mut base = PortDriverBase::new("undef_read", 1, PortFlags::default());
        let i32_idx = base.params.create_param("I32", ParamType::Int32).unwrap();
        let i64_idx = base.params.create_param("I64", ParamType::Int64).unwrap();
        let f64_idx = base.params.create_param("F64", ParamType::Float64).unwrap();
        let oct_idx = base.params.create_param("OCT", ParamType::Octet).unwrap();
        let u32_idx = base
            .params
            .create_param("BITS", ParamType::UInt32Digital)
            .unwrap();
        let mut drv = AllTypesDrv { base };

        // Unset → every default scalar read is ParamUndefined, NOT Ok(0).
        assert!(matches!(
            drv.read_int32(&AsynUser::new(i32_idx).with_addr(0)),
            Err(AsynError::ParamUndefined(_))
        ));
        assert!(matches!(
            drv.read_int64(&AsynUser::new(i64_idx).with_addr(0)),
            Err(AsynError::ParamUndefined(_))
        ));
        assert!(matches!(
            drv.read_float64(&AsynUser::new(f64_idx).with_addr(0)),
            Err(AsynError::ParamUndefined(_))
        ));
        let mut buf = [0u8; 16];
        assert!(matches!(
            drv.read_octet(&AsynUser::new(oct_idx).with_addr(0), &mut buf),
            Err(AsynError::ParamUndefined(_))
        ));
        assert!(matches!(
            drv.read_uint32_digital(&AsynUser::new(u32_idx).with_addr(0), 0xFFFF_FFFF),
            Err(AsynError::ParamUndefined(_))
        ));

        // After a write the same reads succeed with the stored value.
        drv.base_mut().params.set_int32(i32_idx, 0, 7).unwrap();
        drv.base_mut().params.set_int64(i64_idx, 0, 9).unwrap();
        drv.base_mut().params.set_float64(f64_idx, 0, 1.5).unwrap();
        drv.base_mut()
            .params
            .set_string(oct_idx, 0, b"hi".to_vec())
            .unwrap();
        drv.base_mut()
            .params
            .set_uint32(u32_idx, 0, 0x05, 0xFFFF_FFFF, 0)
            .unwrap();

        assert_eq!(
            drv.read_int32(&AsynUser::new(i32_idx).with_addr(0))
                .unwrap(),
            7
        );
        assert_eq!(
            drv.read_int64(&AsynUser::new(i64_idx).with_addr(0))
                .unwrap(),
            9
        );
        assert_eq!(
            drv.read_float64(&AsynUser::new(f64_idx).with_addr(0))
                .unwrap(),
            1.5
        );
        let n = drv
            .read_octet(&AsynUser::new(oct_idx).with_addr(0), &mut buf)
            .unwrap();
        assert_eq!(&buf[..n], b"hi");
        assert_eq!(
            drv.read_uint32_digital(&AsynUser::new(u32_idx).with_addr(0), 0xFFFF_FFFF)
                .unwrap(),
            0x05
        );
    }

    /// R16-47: the parameter block is one level *late* no longer.
    ///
    /// C `asynPortDriver::report` hands `reportParams` the level it was given,
    /// unchanged (asynPortDriver.cpp:3693); `reportParams` prints list 0 at any
    /// level and all `maxAddr` lists at `details >= 2` (:1804); and
    /// `paramVal::report` prints name, type, value and status for every parameter
    /// at every level (paramVal.cpp:296-372). Passing `level - 1` made
    /// `asynReport 1` print a bare count, and values appear only at 3.
    ///
    /// One case per threshold boundary: details 0 (no block), 1 (list 0, with
    /// values), 2 (every address list).
    #[test]
    fn report_prints_the_parameter_block_at_the_c_detail_levels() {
        struct Drv {
            base: PortDriverBase,
        }
        impl PortDriver for Drv {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let mut base = PortDriverBase::new(
            "rep",
            2,
            PortFlags {
                multi_device: true,
                ..PortFlags::default()
            },
        );
        let n = base.params.create_param("N", ParamType::Int32).unwrap();
        let x = base.params.create_param("X", ParamType::Float64).unwrap();
        let s_idx = base.params.create_param("S", ParamType::Octet).unwrap();
        let bits = base
            .params
            .create_param("BITS", ParamType::UInt32Digital)
            .unwrap();
        base.params.set_int32(n, 0, 7).unwrap();
        base.params.set_float64(x, 0, 0.1 + 0.2).unwrap();
        base.params.set_string(s_idx, 0, b"hello".to_vec()).unwrap();
        base.params.set_uint32(bits, 0, 0xa5, 0xff, 0).unwrap();
        base.params
            .set_uint32_interrupt(bits, 0, 0x0f, InterruptReason::ZeroToOne)
            .unwrap();
        // Addr 1 is left untouched: its parameters must report as undefined.
        let drv = Drv { base };

        // details 0 — C prints the port line and stops (:3678-3680).
        let mut out = String::new();
        drv.report(&mut out, 0);
        assert!(
            !out.contains("Parameter"),
            "details 0 has no parameter block: {out}"
        );

        // details 1 — list 0, the count, and every parameter WITH its value.
        let mut out = String::new();
        drv.report(&mut out, 1);
        assert!(
            out.contains("Parameter list 0\nNumber of parameters is: 4\n"),
            "C's paramList::report header (asynPortDriver.cpp:887): {out}"
        );
        assert!(
            out.contains("Parameter 0 type=asynInt32, name=N, value=7, status=0\n"),
            "{out}"
        );
        // C's `%g`: six significant digits, so 0.1+0.2 is `0.3`, not
        // `0.30000000000000004`.
        assert!(
            out.contains("Parameter 1 type=asynFloat64, name=X, value=0.3, status=0\n"),
            "{out}"
        );
        assert!(
            out.contains("Parameter 2 type=string, name=S, value=hello, status=0\n"),
            "C calls an octet parameter `string` (paramVal.cpp:328): {out}"
        );
        assert!(
            out.contains(
                "Parameter 3 type=asynUInt32Digital, name=BITS, value=0xa5, status=0, \
                 risingMask=0xf, fallingMask=0x0, callbackMask=0xa5\n"
            ),
            "C prints the three masks with the value (paramVal.cpp:314-316): {out}"
        );
        assert!(
            !out.contains("Parameter list 1"),
            "below details 2 C reports one address list (asynPortDriver.cpp:1804): {out}"
        );

        // details 2 — every address list, and addr 1 is undefined.
        let mut out = String::new();
        drv.report(&mut out, 2);
        assert!(out.contains("Parameter list 1\n"), "{out}");
        assert!(
            out.contains("Parameter 0 type=asynInt32, name=N, value is undefined\n"),
            "an unset parameter prints C's undefined line (paramVal.cpp:304): {out}"
        );
    }

    /// R16-48: the report escapes a terminator the way C does — the whole libCom
    /// table, not just CR and LF.
    ///
    /// C prints the EOS pair with `epicsStrPrintEscaped` (asynPortDriver.cpp:3687,
    /// 3690), whose table is `\a \b \f \n \r \t \v \\ \' \"`, the byte itself
    /// when `isprint`, and `\xNN` otherwise (epicsString.c:230-269). The report's
    /// own two-case table wrote a binary terminator raw into stdout.
    #[test]
    fn report_escapes_the_eos_with_the_c_table() {
        struct Drv {
            base: PortDriverBase,
        }
        impl PortDriver for Drv {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
                crate::interfaces::octet_transport_capabilities()
            }
        }

        let mut base = PortDriverBase::new("eos_rep", 1, PortFlags::default());
        // See `a_single_device_port_collapses_every_addr_onto_one_eos`: the
        // EOS layer is what makes a terminator settable at all (R18-71).
        base.install_octet_interpose(Box::new(crate::interpose::eos::EosInterpose::default()));
        let mut drv = Drv { base };
        // A real binary terminator (C caps an EOS at two bytes): ESC, then NUL —
        // neither of which the old two-case table escaped.
        drv.set_input_eos(&AsynUser::default(), b"\x1b\0").unwrap();
        // …and the named escapes beyond CR/LF: TAB and BEL.
        drv.set_output_eos(&AsynUser::default(), b"\t\x07").unwrap();

        let mut out = String::new();
        drv.report(&mut out, 1);
        assert!(
            out.contains("  Input EOS[2]: \\x1b\\0\n"),
            "CBUG-D4 refused: C's epicsStrPrintEscaped lacked a `case 0` (epicsString.c:255-260) and printed \\x00; the port supplies it so NUL renders \\0, matching epicsStrnEscapedFromRaw: {out}"
        );
        assert!(
            out.contains("  Output EOS[2]: \\t\\a\n"),
            "BEL is `\\a` in C's table, not `\\x07` (epicsString.c:245): {out}"
        );
    }

    /// R16-49: the report has no options block, at any level.
    ///
    /// C `asynPortDriver::report` (asynPortDriver.cpp:3677-3710) prints the port
    /// name, the timestamp, the EOS pair, the parameter lists and — at
    /// `details >= 3` — the interrupt clients. It never prints options, and it
    /// could not: `asynOption` is a `getOption`/`setOption` pair keyed by a
    /// driver-defined string, with no enumeration to walk.
    #[test]
    fn report_prints_no_options_block() {
        struct Drv {
            base: PortDriverBase,
        }
        impl PortDriver for Drv {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
        }

        let mut drv = Drv {
            base: PortDriverBase::new("opt_rep", 1, PortFlags::default()),
        };
        drv.set_option(&mut AsynUser::default(), "baud", "9600")
            .unwrap();

        for level in 0..=4 {
            let mut out = String::new();
            drv.report(&mut out, level);
            assert!(
                !out.contains("option") && !out.contains("baud"),
                "asynReport {level} must print no options block: {out}"
            );
        }
    }
}
