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
use std::sync::atomic::{AtomicBool, Ordering};
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
/// are no devices to pick from: `findDpCommon` (asynManager.c:496-509) and
/// `findInterface` resolve *every* addr to the port itself, so `asynSetEos`
/// with addr 0 and with addr -1 must reach the same terminator. That collapse
/// is what the `-1` key below is.
pub fn eos_device_key(multi_device: bool, addr: i32) -> i32 {
    if multi_device { addr } else { -1 }
}

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::exception::{AsynException, ExceptionEvent, ExceptionManager};
use crate::interfaces::InterfaceType;
use crate::interpose::{EomReason, OctetInterpose, OctetInterposeStack};
use crate::interrupt::{InterruptManager, InterruptValue};
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
/// the *owner* drives (`drvAsynIPServerPort.c:357-367` — the listener calls
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
    /// `defunct` — set by [`Self::shutdown_lifecycle`] when a
    /// destructible port is torn down via `shutdown_port`. Once true,
    /// the port refuses every new request through [`Self::check_ready`].
    /// Mirrors the `dpCommon.defunct` flag at C asynManager.c:2284
    /// — once defunct, the port cannot be re-enabled.
    pub defunct: bool,
    /// Exception sink injected by [`crate::manager::PortManager`] on registration.
    pub exception_sink: Option<Arc<ExceptionManager>>,
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
    pub trace: Option<Arc<TraceManager>>,
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

impl PortDriverBase {
    pub fn new(port_name: &str, max_addr: usize, flags: PortFlags) -> Self {
        Self {
            port_name: port_name.to_string(),
            max_addr: max_addr.max(1),
            flags,
            params: ParamList::new(max_addr, flags.multi_device),
            interrupts: InterruptManager::new(256),
            connected: Connection::Own(true),
            last_announced: true,
            enabled: true,
            auto_connect: true,
            defunct: false,
            exception_sink: None,
            options: HashMap::new(),
            eos: HashMap::new(),
            interpose_octet: OctetInterposeStack::new(flags.multi_device),
            trace: None,
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

    /// The write owner for this device's terminators — the queryable cache the
    /// EOS readback (`get_input_eos`, the binary-suppress save/restore) reads.
    /// The forward to the interpose stack lives in the `PortDriver` hook, which
    /// is the only caller.
    fn eos_entry(&mut self, addr: i32) -> &mut DeviceEos {
        let key = self.eos_key(addr);
        self.eos.entry(key).or_default()
    }

    /// Announce an exception through the global exception manager (if injected).
    pub fn announce_exception(&self, exception: AsynException, addr: i32) {
        if let Some(ref sink) = self.exception_sink {
            sink.announce(&ExceptionEvent {
                port_name: self.port_name.clone(),
                exception,
                addr,
            });
        }
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
    /// On a port whose link is owned elsewhere ([`Connection::Shared`]) the write
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
    /// fan-out: the single owner of `exceptionConnect`/`exceptionDisconnect`
    /// (asynManager.c:2151-2185), of the interpose stack's connection reset and of
    /// the retry timer.
    ///
    /// [`Self::set_connected`] is one caller. The other is the actor, on a port
    /// whose link is owned elsewhere: the owner (an IP-server listener assigning a
    /// slot) moves the truth without this port's actor running, and C fans that
    /// edge out from the owner's thread — `pasynCommonSyncIO->connectDevice` on
    /// the child (drvAsynIPServerPort.c:357-367). Here it is published when the
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
            // `!connected` guard, asynManager.c:3257; disarming here is the
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
    /// is effectively infinite), or at least [`AUTO_CONNECT_THROTTLE`] has
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
        self.flags.multi_device && addr >= 0
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
            return Err(AsynError::Status {
                status: AsynStatus::Disabled,
                message: format!("port {} has been shut down (defunct)", self.port_name),
            });
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
        if self.defunct {
            return Err(AsynError::Status {
                status: AsynStatus::Disabled,
                message: format!("port {} has been shut down (defunct)", self.port_name),
            });
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
        self.announce_exception(AsynException::AutoConnect, -1);
    }

    /// Per-address variant — for multi-device ports. C parity:
    /// `autoConnectAsyn` walks dpCommon via findDpCommon so a per-
    /// device pasynUser hits the device's dpc, otherwise the port's
    /// dpc (asynManager.c:2314 + findDpCommon).
    pub fn set_auto_connect_addr(&mut self, addr: i32, yes: bool) {
        self.device_state(addr).auto_connect = yes;
        self.announce_exception(AsynException::AutoConnect, addr);
    }

    /// Query whether the port has been marked defunct via
    /// [`Self::shutdown_lifecycle`] — once true the port is gone for
    /// good, mirroring C asynManager.c:2266-2269.
    pub fn is_defunct(&self) -> bool {
        self.defunct
    }

    /// C `queueRequest`'s gate (asynManager.c:1539-1552), and the single owner
    /// of the two refusals it is built from.
    ///
    /// They are independent, and the [`ConnectCheck`] the caller hands in
    /// selects between them — it can waive the *connected* refusal and nothing
    /// else. There is no argument, op class or priority that waives
    /// [`Self::check_enabled`]: C's `if(!pport->dpc.enabled) return asynDisabled`
    /// (:1541-1546) sits *above* `checkPortConnect` and is reached by every
    /// request, and its port thread refuses to run anything at all on a disabled
    /// port (`portThread`, :802-805).
    ///
    /// The `ConnectCheck` can only come from [`AsynUser::connect_check`], so the
    /// waiver is available exactly to the requests C gives it to.
    ///
    /// Its refusal is [`AsynError::QueueRefused`], not [`AsynError::Status`]:
    /// C's refusal is `queueRequest`'s *return value*, so the callback never
    /// runs and nothing it implies happened. A caller must be able to tell that
    /// from a driver error raised *inside* a callback that did run — the record
    /// writes the refusal to ERRS and stops, where a driver error still gets the
    /// callback's readback and `monitorStatus` tail (asynRecord.c:571-576 vs
    /// :788-900). This is the only place that stamps it.
    pub fn check_queue(&self, addr: i32, connect: ConnectCheck) -> AsynResult<()> {
        self.check_enabled()
            .and_then(|()| match connect {
                // C `checkPortConnect == FALSE`: neither the port's nor the
                // device's connected flag is read — the port thread drains the
                // Connect queue before it ever calls `autoConnectDevice`
                // (asynManager.c:812-856), and the device-level checks live in
                // the lower-priority loop it has not reached yet (:864-874).
                ConnectCheck::Waived => Ok(()),
                ConnectCheck::Required => self.check_ready_addr(addr),
            })
            .map_err(AsynError::into_queue_refusal)
    }

    /// The unconditional half of the queue gate: a defunct or disabled port
    /// refuses every request (asynManager.c:1541-1546).
    pub fn check_enabled(&self) -> AsynResult<()> {
        // C asyn parity: a defunct port short-circuits queueRequest
        // (asynManager.c:2283 comment). Reject *before* the enabled
        // check so the error message names the lifecycle phase, not
        // just "disabled".
        if self.defunct {
            return Err(AsynError::Status {
                status: AsynStatus::Disabled,
                message: format!("port {} has been shut down (defunct)", self.port_name),
            });
        }
        if !self.enabled {
            return Err(AsynError::Status {
                status: AsynStatus::Disabled,
                message: format!("port {} is disabled", self.port_name),
            });
        }
        Ok(())
    }

    /// Check that the port is enabled, connected, and not defunct.
    /// Returns `Err(Disabled)`, `Err(Disconnected)`, or `Err(Disabled)`
    /// (defunct => permanently disabled) otherwise.
    pub fn check_ready(&self) -> AsynResult<()> {
        self.check_enabled()?;
        if !self.is_connected() {
            return Err(AsynError::Status {
                status: AsynStatus::Disconnected,
                message: format!("port {} is disconnected", self.port_name),
            });
        }
        Ok(())
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
        self.defunct = true;
        self.announce_exception(AsynException::Shutdown, -1);
        Ok(())
    }

    /// Check that port + device address are both ready.
    /// For multi-device ports, checks per-address state in addition to port-level state.
    pub fn check_ready_addr(&self, addr: i32) -> AsynResult<()> {
        self.check_ready()?;
        if self.flags.multi_device {
            if let Some(ds) = self.device_states.get(&addr) {
                if !ds.enabled {
                    return Err(AsynError::Status {
                        status: AsynStatus::Disabled,
                        message: format!("port {} addr {} is disabled", self.port_name, addr),
                    });
                }
                if !ds.connected {
                    return Err(AsynError::Status {
                        status: AsynStatus::Disconnected,
                        message: format!("port {} addr {} is disconnected", self.port_name, addr),
                    });
                }
            }
        }
        Ok(())
    }

    /// Get or create a device state for the given address.
    pub fn device_state(&mut self, addr: i32) -> &mut DeviceState {
        self.device_states.entry(addr).or_default()
    }

    /// Check if a specific device address is connected.
    pub fn is_device_connected(&self, addr: i32) -> bool {
        self.device_states
            .get(&addr)
            .map_or(true, |ds| ds.connected)
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

    pub fn set_string_param(&mut self, index: usize, addr: i32, value: String) -> AsynResult<()> {
        self.params.set_string(index, addr, value)
    }

    pub fn get_string_param(&self, index: usize, addr: i32) -> AsynResult<&str> {
        self.params.get_string(index, addr)
    }

    /// Strict variant — see [`crate::param::ParamList::get_string_strict`].
    pub fn get_string_param_strict(&self, index: usize, addr: i32) -> AsynResult<&str> {
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

    /// Detailed parameter report matching C asynPortDriver::reportParams.
    pub fn report_params(&self, level: i32) {
        eprintln!("  Number of parameters is {}", self.params.len());
        if level < 1 {
            return;
        }
        for i in 0..self.params.len() {
            let name = self.params.param_name(i).unwrap_or("?");
            let ptype = self
                .params
                .param_type(i)
                .map(|t| format!("{t:?}"))
                .unwrap_or("?".into());
            if level >= 2 {
                for addr in 0..self.max_addr.max(1) {
                    let val = self
                        .params
                        .get_value(i, addr as i32)
                        .map(|v| format!("{v:?}"))
                        .unwrap_or("undefined".into());
                    let (status, alarm_st, alarm_sev) = self
                        .params
                        .get_param_status(i, addr as i32)
                        .unwrap_or((AsynStatus::Success, 0, 0));
                    eprintln!(
                        "  param[{i}] name={name} type={ptype} addr={addr} val={val} status={status:?} alarm=({alarm_st},{alarm_sev})"
                    );
                }
            } else {
                eprintln!("  param[{i}] name={name} type={ptype}");
            }
        }
    }

    /// Push an interpose layer onto the octet I/O stack.
    ///
    /// **Concurrency**: requires `&mut self`, which means the caller must hold
    /// the port lock (`Arc<Mutex<dyn PortDriver>>`). This ensures
    /// interpose modifications are serialized with I/O dispatch.
    pub fn install_octet_interpose(&mut self, layer: Box<dyn OctetInterpose>) {
        self.interpose_octet.install(layer);
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
            // C parity: asynPortDriver.cpp:631-642 sets
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
    /// interrupt list (`int32`/`uInt32Digital`/`float64`,
    /// drvModbusAsyn.cpp:1706/1736/1808). This is the analogue: the driver
    /// decodes per interface and fires each value tagged with its `iface`, so the
    /// interrupt filter routes it only to records on that interface
    /// ([`InterruptFilter::iface`]). `uint32_changed_mask` is the changed-bit
    /// mask for the `UInt32Digital` interface (a record's `@asynMask` gates on it,
    /// `asynPortDriver.cpp:720`); pass `0` for the other interfaces, whose
    /// subscribers carry no mask filter.
    ///
    /// `aux_status` is the device I/O status this fire carries (C
    /// `pInterrupt->pasynUser->auxStatus`, set on every callback the poller
    /// emits — `drvModbusAsyn.cpp:1697/1738/1774/1810/1880/1915`). A driver whose
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
pub trait PortDriver: Send + Sync + 'static {
    fn base(&self) -> &PortDriverBase;
    fn base_mut(&mut self) -> &mut PortDriverBase;

    // --- AsynCommon ---

    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        // Single owner-API: edge-guarded fire is in PortDriverBase::set_connected.
        self.base_mut().set_connected(true);
        Ok(())
    }

    fn disconnect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        self.base_mut().set_connected(false);
        Ok(())
    }

    fn enable(&mut self, _user: &AsynUser) -> AsynResult<()> {
        // C `enable` refuses a defunct port (asynManager.c:2236-2241);
        // the guard lives in the single owner.
        self.base_mut().set_enabled(true)
    }

    fn disable(&mut self, _user: &AsynUser) -> AsynResult<()> {
        self.base_mut().set_enabled(false)
    }

    fn connect_addr(&mut self, user: &AsynUser) -> AsynResult<()> {
        self.base_mut().connect_addr(user.addr);
        Ok(())
    }

    fn disconnect_addr(&mut self, user: &AsynUser) -> AsynResult<()> {
        self.base_mut().disconnect_addr(user.addr);
        Ok(())
    }

    fn enable_addr(&mut self, user: &AsynUser) -> AsynResult<()> {
        // Guarded owner — propagates asynDisabled on a defunct port.
        self.base_mut().set_addr_enabled(user.addr, true)
    }

    fn disable_addr(&mut self, user: &AsynUser) -> AsynResult<()> {
        self.base_mut().set_addr_enabled(user.addr, false)
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
    /// printed by [`crate::port_actor::PortActor::report_port`]; duplicating it
    /// here would give the operator two answers to the same question, from two
    /// owners, with no rule for which one wins.
    ///
    /// The default is C++ `asynPortDriver::report` (asynPortDriver.cpp:3677-3694):
    /// the port name, and at `details >= 1` the EOS terminators and the parameter
    /// library.
    fn report(&self, level: i32) {
        let base = self.base();
        eprintln!("Port: {}", base.port_name);
        if level >= 1 {
            let esc = |eos: &[u8]| {
                eos.iter()
                    .map(|b| match b {
                        b'\r' => "\\r".to_string(),
                        b'\n' => "\\n".to_string(),
                        c => (*c as char).to_string(),
                    })
                    .collect::<String>()
            };
            let input = base.input_eos(0);
            let output = base.output_eos(0);
            eprintln!("  Input EOS[{}]: {}", input.len(), esc(input));
            eprintln!("  Output EOS[{}]: {}", output.len(), esc(output));
            base.report_params(level.saturating_sub(1));
        }
        if level >= 2 {
            for (k, v) in &base.options {
                eprintln!("  option: {k} = {v}");
            }
        }
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
        let s = self
            .base()
            .params
            .get_string_strict(user.reason, user.addr)?;
        let bytes = s.as_bytes();
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(n)
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        let s = String::from_utf8_lossy(data).into_owned();
        self.base_mut()
            .params
            .set_string(user.reason, user.addr, s)?;
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
    /// (`asynOctetSyncIO::readRaw`, GPIB END, EOS match) must
    /// override this method so consumers — `asynRecord::EOMR`,
    /// `asynOctetSyncIO::readRaw` mirrors — receive the real flags.
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
    // (`asynOctetSyncIO.c:300-321`, 346-367) call `lockPort` ahead of
    // the actual `setInputEos` — `lockPort` waits up to the user's
    // timeout for the port to be connected, by `epicsEventWait`-ing
    // on the connect event published from `connectIt`. On IOC init
    // and exit this serialises EOS configuration against the connect
    // task, but it also means a `setInputEos` issued before the port
    // has ever connected blocks the calling thread (issue #103
    // captured the symptom: IOC startup pauses for the full asyn
    // timeout when the device is off-line).
    //
    // The Rust path here is purely in-memory: `set_input_eos` and
    // `set_output_eos` write the bytes into `PortDriverBase` and the
    // EOS interpose stack reads from those fields at next read/write
    // time. No connect-wait, no lock contention with the connect
    // task — so issue #103's symptom cannot reproduce. If a future
    // refactor introduces a connect-gated EOS path (e.g. a driver
    // that owns the EOS state inside its connect()-allocated
    // resource), authors MUST keep the wait optional / bounded so
    // the connect-wait failure mode doesn't return.

    // Every hook takes the `asynUser`, because in C every one of them does
    // (`asynOctet::setInputEos(void *ppvt, asynUser *pasynUser, ...)`,
    // asynOctetBase.h; asynInterposeEos.c:288-296) and the addr it carries is
    // what picks the device's terminator. A port-wide EOS could not hold two.

    fn set_input_eos(&mut self, user: &AsynUser, eos: &[u8]) -> AsynResult<()> {
        if eos.len() > 2 {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("illegal eoslen {}", eos.len()),
            });
        }
        // Single write owner for input EOS: the per-device cache is what
        // `get_input_eos` and the binary-suppress save/restore read, and the
        // same value is forwarded to the interpose stack so an installed
        // `EosInterpose` actually terminates *this device's* reads on it. Empty
        // stack = no-op forward; C routes `setInputEos` the same way.
        let addr = user.addr;
        let base = self.base_mut();
        base.eos_entry(addr).input = eos.to_vec();
        base.interpose_octet.set_input_eos(addr, eos);
        Ok(())
    }

    fn get_input_eos(&self, user: &AsynUser) -> Vec<u8> {
        self.base().input_eos(user.addr).to_vec()
    }

    fn set_output_eos(&mut self, user: &AsynUser, eos: &[u8]) -> AsynResult<()> {
        if eos.len() > 2 {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("illegal eoslen {}", eos.len()),
            });
        }
        // Single write owner for output EOS (see `set_input_eos`): cache per
        // device and forward to the interpose stack so `EosInterpose` appends
        // the terminator on that device's writes.
        let addr = user.addr;
        let base = self.base_mut();
        base.eos_entry(addr).output = eos.to_vec();
        base.interpose_octet.set_output_eos(addr, eos);
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
    // GPIBIV and refuses UCMD/ACMD when it is 0 (asynRecord.c:1231-1241,
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

    /// Resolve a record's driver-info string (and its asyn `addr`) to a
    /// [`DrvUserInfo`] at bind time — the asyn-rs analogue of C `drvUserCreate`.
    ///
    /// Takes `&mut self` so a driver can register a parameter on demand from the
    /// resolved drvInfo (C Autoparam lazy creation) rather than requiring it be
    /// declared up front, and `addr` so a multi-device driver can reject an
    /// out-of-range address at bind time (C `drvUserCreate` runs `checkOffset`,
    /// drvModbusAsyn.cpp:378-384) instead of alarming on every I/O.
    ///
    /// Default: look up the shared reason by parameter name; ignore `addr`.
    fn drv_user_create(&mut self, drv_info: &str, _addr: i32) -> AsynResult<DrvUserInfo> {
        let reason = self
            .base()
            .params
            .find_param(drv_info)
            .ok_or_else(|| AsynError::ParamNotFound(drv_info.to_string()))?;
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

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(drv.drv_user_create("VAL", 0).unwrap().reason, 0);
        assert_eq!(drv.drv_user_create("TEMP", 0).unwrap().reason, 1);
        assert!(drv.drv_user_create("NOPE", 0).is_err());
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
        // C parity: asynPortDriver.cpp:631-642 writes the param's stored
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
            drv.report(level);
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
    /// devices to key by — C's `findDpCommon` (asynManager.c:496-509) and
    /// `findInterface` resolve *every* addr to the port itself, so
    /// `asynSetEos(port, -1, ...)` and a record at ADDR 0 must reach the same
    /// terminator. Splitting them by raw addr would leave the record reading
    /// with no EOS at all.
    #[test]
    fn a_single_device_port_collapses_every_addr_onto_one_eos() {
        let mut drv = TestDriver::new();
        drv.set_input_eos(&AsynUser::default().with_addr(-1), b"\n")
            .unwrap();
        assert_eq!(drv.get_input_eos(&AsynUser::default().with_addr(0)), b"\n");
        assert_eq!(drv.get_input_eos(&AsynUser::default().with_addr(7)), b"\n");
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
        assert!(format!("{err}").contains("disabled"));

        // Disconnect addr 2
        base.device_state(2).connected = false;
        let err = base.check_ready_addr(2).unwrap_err();
        assert!(format!("{err}").contains("disconnected"));
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
        // check_ready surfaces the defunct state for every request.
        match base.check_ready() {
            Err(AsynError::Status { message, .. }) => {
                assert!(message.contains("defunct"), "msg={message}");
            }
            other => panic!("expected defunct error, got {other:?}"),
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
        assert!(format!("{err}").contains("disabled"));

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
        base.exception_sink = Some(exc_mgr.clone());

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
        base.exception_sink = Some(exc_mgr.clone());

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
        base.exception_sink = Some(exc_mgr.clone());
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
        base.exception_sink = Some(exc_mgr.clone());
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
        base.exception_sink = Some(exc_mgr.clone());
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
            .set_string(oct_idx, 0, "hi".to_string())
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
}
