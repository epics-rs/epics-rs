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

use parking_lot::Mutex;

/// C `autoConnectDevice` reconnect throttle window (asynManager.c:715).
/// A disconnected `auto_connect` device is refused a fresh connect attempt
/// until this much time has elapsed since its last connect/disconnect
/// transition or attempt, bounding reconnect storms to one attempt per
/// window.
const AUTO_CONNECT_THROTTLE: Duration = Duration::from_secs(2);

/// First autonomous connect-retry delay after a port drops. C
/// `exceptionDisconnect` arms the port's connect timer at `.01` seconds
/// (asynManager.c:2183-2184), so the reconnect is attempted essentially
/// immediately and then backs off to [`DEFAULT_SECONDS_BETWEEN_PORT_CONNECT`].
const CONNECT_RETRY_INITIAL: Duration = Duration::from_millis(10);

/// C `DEFAULT_SECONDS_BETWEEN_PORT_CONNECT` (asynManager.c:48) — the interval
/// `portConnectProcessCallback` re-arms the connect timer at after a failed
/// attempt (asynManager.c:3312).
const DEFAULT_SECONDS_BETWEEN_PORT_CONNECT: Duration = Duration::from_secs(20);

/// One C `dpCommon`'s `connected` / `enabled` / `autoConnect`, as cells only
/// the port's actor writes.
///
/// C's `isConnected` / `isEnabled` / `isAutoConnect` read the user's
/// `dpCommon` directly and never queue (asynManager.c:2331-2373). A
/// [`crate::manager::DeviceUser`] keeps the cells of the `dpCommon` it bound
/// to, as C keeps `pdevice`, so those reads wait on no driver call.
#[derive(Debug, Clone)]
pub struct DpCells {
    connected: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
    auto_connect: Arc<AtomicBool>,
}

impl DpCells {
    fn new(connected: bool, enabled: bool, auto_connect: bool) -> Self {
        Self {
            connected: Arc::new(AtomicBool::new(connected)),
            enabled: Arc::new(AtomicBool::new(enabled)),
            auto_connect: Arc::new(AtomicBool::new(auto_connect)),
        }
    }

    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn auto_connect(&self) -> bool {
        self.auto_connect.load(Ordering::Acquire)
    }
}

/// Per-address device state for multi-device ports.
#[derive(Debug, Clone)]
pub struct DeviceState {
    cells: DpCells,
    /// Monotonic instant of the last connect/disconnect transition or
    /// auto-connect attempt for this device — the anchor for the 2s
    /// reconnect throttle (C `dpCommon.lastConnectDisconnect`). `None`
    /// mirrors C's zero-initialised timestamp: the first attempt is
    /// always permitted.
    pub last_connect_disconnect: Option<Instant>,
    /// This device's announces owed — see [`OwedAnnounces`].
    owed: OwedAnnounces,
}

impl Default for DeviceState {
    fn default() -> Self {
        Self {
            cells: DpCells::new(true, true, true),
            last_connect_disconnect: None,
            owed: OwedAnnounces::default(),
        }
    }
}

impl DeviceState {
    pub fn connected(&self) -> bool {
        self.cells.connected()
    }

    pub fn enabled(&self) -> bool {
        self.cells.enabled()
    }

    pub fn auto_connect(&self) -> bool {
        self.cells.auto_connect()
    }

    /// Raw stores, with no `asynException`: the owners are
    /// [`PortDriverBase::set_addr_connected`], [`PortDriverBase::set_addr_enabled`]
    /// and [`PortDriverBase::set_auto_connect_addr`].
    pub(crate) fn put_connected(&self, yes: bool) {
        self.cells.connected.store(yes, Ordering::Release);
    }

    pub(crate) fn put_enabled(&self, yes: bool) {
        self.cells.enabled.store(yes, Ordering::Release);
    }

    pub(crate) fn put_auto_connect(&self, yes: bool) {
        self.cells.auto_connect.store(yes, Ordering::Release);
    }
}

/// C `locateDevice(pport, addr, TRUE)` (asynManager.c:576-589): the device at
/// `addr`, created on first use with its `dpCommon` seeded from the port's
/// (`dpCommonInit(pport, pdevice, pport->dpc.autoConnect)`, :586).
fn locate_device(
    devices: &mut HashMap<i32, DeviceState>,
    addr: i32,
    port: impl FnOnce() -> DpCells,
) -> &mut DeviceState {
    devices.entry(addr).or_insert_with(|| {
        let port = port();
        DeviceState {
            cells: DpCells::new(port.connected(), port.enabled(), port.auto_connect()),
            last_connect_disconnect: None,
            owed: OwedAnnounces::default(),
        }
    })
}

/// A port's device list as C keeps it: one list under `asynManagerLock`, which
/// `connectDevice` reaches from the caller's thread (asynManager.c:1324-1356).
/// The actor and every [`crate::port_handle::PortHandle`] share it, so binding a
/// user — or C `enable` / `autoConnect`, the other manager calls asynRecord
/// makes under the record's lock — is a short lock on the calling thread and
/// never a request to the actor. The lock is never held across a driver call or
/// an exception announce.
#[derive(Clone)]
pub(crate) struct DeviceTable {
    multi_device: bool,
    port: DpCells,
    devices: Arc<Mutex<HashMap<i32, DeviceState>>>,
    /// C `dpCommon.defunct`; written only under the `devices` lock, in
    /// [`PortDriverBase::shutdown_lifecycle`], so [`Self::enable`]'s check and
    /// store cannot straddle a shutdown.
    defunct: Arc<AtomicBool>,
    owed: Arc<Mutex<PortOwed>>,
    lock_port_timeout: Arc<LockPortTimeout>,
}

impl DeviceTable {
    /// The table of a single-device port no actor owns — for a test fixture.
    #[cfg(test)]
    pub(crate) fn detached() -> Self {
        Self {
            multi_device: false,
            port: DpCells::new(true, true, true),
            devices: Arc::default(),
            defunct: Arc::default(),
            owed: Arc::default(),
            lock_port_timeout: Arc::default(),
        }
    }

    /// C `connectDevice`'s `locateDevice` (asynManager.c:1349-1352): the cells of
    /// the `dpCommon` a user bound to `addr` reads — the device's on a
    /// multi-device port, created if new, the port's own otherwise.
    pub(crate) fn connect_user(&self, addr: i32) -> DpCells {
        match dp_common_key(self.multi_device, addr) {
            Some(addr) => {
                let port = &self.port;
                locate_device(&mut self.devices.lock(), addr, || port.clone())
                    .cells
                    .clone()
            }
            None => self.port.clone(),
        }
    }

    /// C `pport->dpc.defunct`, read without the actor as C's `findInterface`
    /// reads it under `asynManagerLock` (asynManager.c:1487).
    pub(crate) fn is_defunct(&self) -> bool {
        self.defunct.load(Ordering::Acquire)
    }

    /// C `port.queueLockPortTimeout`, the queue deadline every SyncIO call on
    /// the port gets at least — see [`crate::user::AsynUser::locking_port`].
    pub(crate) fn queue_lock_port_timeout(&self) -> Duration {
        Duration::from_nanos(self.lock_port_timeout.0.load(Ordering::Relaxed))
    }

    /// C `setQueueLockPortTimeout` (asynManager.c:2393-2406).
    pub(crate) fn set_queue_lock_port_timeout(&self, timeout: Duration) {
        let nanos = u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX);
        self.lock_port_timeout.0.store(nanos, Ordering::Relaxed);
    }

    /// C `enable`'s locked half (asynManager.c:2236-2247): refuse a shut-down
    /// port, else store `enabled` on the `dpCommon` `addr` resolves to and owe
    /// its `asynExceptionEnable` (:2249) — see [`PortDriverBase::drain_announces`].
    pub(crate) fn enable(&self, addr: i32, yes: bool) -> AsynResult<()> {
        let mut devices = self.devices.lock();
        if self.defunct.load(Ordering::Acquire) {
            return Err(PortDriverBase::shut_down_error());
        }
        let mut owed = self.owed.lock();
        let stamp = owed.stamp();
        match dp_common_key(self.multi_device, addr) {
            Some(addr) => {
                let dev = locate_device(&mut devices, addr, || self.port.clone());
                dev.put_enabled(yes);
                dev.owed.enable = stamp;
            }
            None => {
                self.port.enabled.store(yes, Ordering::Release);
                owed.port.enable = stamp;
            }
        }
        Ok(())
    }

    /// C `autoConnectAsyn`'s store (asynManager.c:2318-2323), owing its connect
    /// timer and `asynExceptionAutoConnect` — see
    /// [`PortDriverBase::drain_announces`].
    pub(crate) fn auto_connect(&self, addr: i32, yes: bool) {
        let mut devices = self.devices.lock();
        let mut owed = self.owed.lock();
        let stamp = owed.stamp();
        // C checks the dpCommon `findDpCommon` resolved — on a device, the
        // DEVICE's — while the timer it starts is the port's.
        let (slot, connected) = match dp_common_key(self.multi_device, addr) {
            Some(addr) => {
                let dev = locate_device(&mut devices, addr, || self.port.clone());
                dev.put_auto_connect(yes);
                (&mut dev.owed, dev.cells.connected())
            }
            None => {
                self.port.auto_connect.store(yes, Ordering::Release);
                (&mut owed.port, self.port.connected())
            }
        };
        slot.auto_connect = stamp;
        if yes && !connected {
            owed.start_timer = true;
        }
    }

    /// The oldest announce owed on any dpCommon, taken off it: its stamp
    /// cleared, and for an AutoConnect the timer start owed with it.
    fn take_oldest_owed(&self) -> Option<(AsynException, i32, bool)> {
        let mut devices = self.devices.lock();
        let mut owed = self.owed.lock();
        if !owed.any {
            return None;
        }
        let port = &mut owed.port;
        let mut oldest: Option<(u64, AsynException, i32, &mut u64)> = None;
        let slots = std::iter::once((-1, port))
            .chain(devices.iter_mut().map(|(addr, d)| (*addr, &mut d.owed)));
        for (addr, slot) in slots {
            for (kind, stamp) in [
                (AsynException::Enable, &mut slot.enable),
                (AsynException::AutoConnect, &mut slot.auto_connect),
            ] {
                if *stamp != 0 && oldest.as_ref().is_none_or(|o| *stamp < o.0) {
                    oldest = Some((*stamp, kind, addr, stamp));
                }
            }
        }
        let Some((_, kind, addr, stamp)) = oldest else {
            owed.any = false;
            return None;
        };
        *stamp = 0;
        let start_timer =
            kind == AsynException::AutoConnect && std::mem::take(&mut owed.start_timer);
        Some((kind, addr, start_timer))
    }
}

/// A port's `queueLockPortTimeout` in nanoseconds, C's default until set.
struct LockPortTimeout(AtomicU64);

impl Default for LockPortTimeout {
    fn default() -> Self {
        Self(AtomicU64::new(
            crate::user::QUEUE_LOCK_PORT_TIMEOUT.as_nanos() as u64,
        ))
    }
}

/// The announces C `enable` and `autoConnectAsyn` raise inline on one
/// dpCommon (asynManager.c:2249, :2323-2326), owed to the port's actor: the
/// port stamp of the latest call of each kind, 0 for none. Owed in place, so a
/// call under a record's hold allocates nothing and is never refused; repeated
/// calls of one kind on one dpCommon coalesce into one announce at the latest
/// call's place, which the one consumer (asynRecord's exception callback,
/// re-reading the state as C `exceptCallback` does) cannot tell apart.
#[derive(Debug, Clone, Copy, Default)]
struct OwedAnnounces {
    enable: u64,
    auto_connect: u64,
}

/// The port's half of the owed announces, under the lock taken after the
/// device list's: the port dpCommon's own, the stamp counter and the
/// AutoConnect timer start.
#[derive(Default)]
struct PortOwed {
    port: OwedAnnounces,
    seq: u64,
    start_timer: bool,
    /// Some dpCommon may owe an announce.
    any: bool,
}

impl PortOwed {
    fn stamp(&mut self) -> u64 {
        self.seq += 1;
        self.any = true;
        self.seq
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
/// are no devices to pick from: `findDpCommon` (asynManager.c:538-546) and
/// `findInterface` resolve *every* addr to the port itself, so `asynSetEos`
/// with addr 0 and with addr -1 must reach the same terminator. That collapse
/// is what the `-1` key below is.
pub fn eos_device_key(multi_device: bool, addr: i32) -> i32 {
    dp_common_key(multi_device, addr).unwrap_or(-1)
}

/// C `findDpCommon` (asynManager.c:538-546) as a key: which `dpCommon` a
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
/// (through `findTracePvt`, :548-553). Separate copies of the rule are how the
/// trace mask came to be per-device while `enabled` beside it stayed port-wide.
pub fn dp_common_key(multi_device: bool, addr: i32) -> Option<i32> {
    (multi_device && addr >= 0).then_some(addr)
}

/// The parameter list C's `getAddress` (asynPortDriver.cpp:1901-1906) takes
/// for a user at `addr`: `pasynManager->getAddr` is -1 on a port without
/// `ASYN_MULTIDEVICE`, whatever the user asked for, and -1 is list 0. A
/// request and an interrupt client resolve through this one rule.
pub fn param_list(multi_device: bool, addr: i32) -> i32 {
    if multi_device && addr != -1 { addr } else { 0 }
}

use crate::error::{AsynError, AsynResult, AsynStatus, QueueRefusal};
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
/// the *owner* drives (`drvAsynIPServerPort.c:372-375` — the listener calls
/// `pasynCommonSyncIO->connectDevice` on the child the moment it hands it a
/// socket). Sharing the owner's cell is the same thing without the round trip,
/// and it is what makes "the owner holds a live socket, the port says
/// disconnected" unrepresentable rather than merely unlikely.
#[derive(Debug, Clone)]
enum Connection {
    Own(Arc<AtomicBool>),
    Shared(Arc<AtomicBool>),
}

impl Connection {
    fn cell(&self) -> &Arc<AtomicBool> {
        match self {
            Connection::Own(cell) | Connection::Shared(cell) => cell,
        }
    }

    fn get(&self) -> bool {
        self.cell().load(Ordering::Acquire)
    }
}

/// The port-level half of C `queueRequest`'s gate (asynManager.c:1543-1554)
/// over the port's own `enabled` and `connected` cells, which only the actor
/// writes. [`PortDriverBase::check_queue`] asks it from the actor, and a
/// [`crate::port_handle::PortHandle`] holding a clone asks it from the thread
/// that queues, as C's `queueRequest` does, without a round trip.
#[derive(Debug, Clone)]
pub(crate) struct QueueGate {
    port_name: Arc<str>,
    enabled: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
}

impl QueueGate {
    /// The gate of an enabled, connected port no actor owns — for a test
    /// fixture's handle whose channel has nothing behind it. It never refuses,
    /// so it never names a port.
    #[cfg(test)]
    pub(crate) fn open() -> Self {
        Self {
            port_name: "".into(),
            enabled: Arc::new(AtomicBool::new(true)),
            connected: Arc::new(AtomicBool::new(true)),
        }
    }

    /// `!pport->dpc.enabled → asynDisabled`, then, unless waived,
    /// `!pport->dpc.connected → asynDisconnected`.
    pub(crate) fn check(&self, connect: ConnectCheck) -> Result<(), QueueRefusal<'_>> {
        if !self.enabled.load(Ordering::Acquire) {
            return Err(QueueRefusal::PortDisabled(&self.port_name));
        }
        if connect == ConnectCheck::Required && !self.connected.load(Ordering::Acquire) {
            return Err(QueueRefusal::PortNotConnected(&self.port_name));
        }
        Ok(())
    }
}

/// Runs on every Int32 store of one reason, before the value lands: what a C
/// driver overrides the virtual `setIntegerParam` for (asynNDArrayDriver.cpp
/// :636-663 keeps `ADAcquireBusy` in step with `ADAcquire`). Takes the
/// parameter list and the value being stored.
pub type Int32StoreRule =
    Arc<dyn Fn(&mut PortDriverBase, i32, i32) -> AsynResult<()> + Send + Sync>;

pub struct PortDriverBase {
    pub port_name: Arc<str>,
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
    /// Whether the port is enabled — read it with [`Self::is_enabled`], move it
    /// with [`Self::set_enabled`]. A cell so that [`QueueGate`] reads it.
    enabled: Arc<AtomicBool>,
    /// Read it with [`Self::is_auto_connect`], move it with
    /// [`Self::set_auto_connect`]. A cell so that [`DpCells`] reads it.
    auto_connect: Arc<AtomicBool>,
    /// C `dpCommon.defunct` (asynManager.c:2286) — the port was torn down via
    /// `shutdownPort` and is gone for good. Read it with [`Self::is_defunct`].
    ///
    /// **Invariant: `defunct ⟹ !enabled`.** C establishes it in `shutdownPort`,
    /// which clears `enabled` *and* sets `defunct` in the same breath (:2285-2286)
    /// with the comment that disabling is what short-circuits `queueRequest` —
    /// and indeed `queueRequest` has no defunct branch at all (:1543-1554): a
    /// defunct port is refused as a *disabled* one, "port %s disabled". C asks
    /// `defunct` in nine places, and which dpCommon it asks is what our model
    /// deviates on: `report` (:1041), `findInterface` (:1487),
    /// `registerInterrupt` (:2424), `getInterruptPvt` (:2475),
    /// `addInterruptUser` (:2561) and `removeInterruptUser` (:2603) all read
    /// `pport->dpc.defunct`, while `enable` (:2240, so the port cannot be
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
    defunct: Arc<AtomicBool>,
    /// The announces [`DeviceTable`] owes — see [`Self::drain_announces`].
    owed: Arc<Mutex<PortOwed>>,
    /// C `port.queueLockPortTimeout` — see [`DeviceTable::queue_lock_port_timeout`].
    lock_port_timeout: Arc<LockPortTimeout>,
    /// This port's announcement channel — C `dpCommon.exceptionUserList` plus
    /// the `notifyPortThread` signal that `announceExceptionOccurred` ends with
    /// (asynManager.c:613-639). Detachable ([`Self::exception_announcer`]) so a
    /// worker thread the driver owns — the IP-server accept loop — announces
    /// through the same counter and the same list as the actor does, instead of
    /// reaching around them.
    pub(crate) announcer: ExceptionAnnouncer,
    pub options: HashMap<String, String>,
    /// The EOS terminators, keyed per device the way C keys them: an `eosPvt`
    /// is created per `asynInterposeEosConfig(portName, addr, ...)`
    /// (asynInterposeEos.c:84-120), and every EOS hook takes the `asynUser`
    /// that selects it (:289-297). Two devices on one multi-device port hold
    /// two different terminators — a single port-wide pair could not.
    ///
    /// Keyed by [`eos_device_key`], so a port that never declared
    /// `ASYN_MULTIDEVICE` collapses every addr onto one entry (C's
    /// `findDpCommon`/`findInterface` resolve any addr to the port itself).
    eos: HashMap<i32, DeviceEos>,
    pub interpose_octet: OctetInterposeStack,
    /// Trace configuration — C `dpCommon.trace`, inited by `dpCommonInit`
    /// (asynManager.c:530). Same
    /// owner as [`Self::announcer`]: bound by
    /// [`crate::services::PortServices::bind`] at port creation.
    pub(crate) trace: Option<Arc<TraceManager>>,
    /// C `octetPvt.interruptProcess` — the last argument of
    /// `pasynOctetBase->initialize` (asynOctetBase.c:161-169). When set, every
    /// successful octet read fans the data out to the port's octet interrupt
    /// users (`readIt` → `callInterruptUsers`, :224-238). The stream drivers set
    /// it — drvAsynIPPort.c:1109, drvAsynSerialPort.c:1125-1126,
    /// drvAsynSerialPortWin32.c:798-799, drvAsynFTDIPort.cpp:616 — and it is what
    /// makes a `stringin`/`waveform` with `SCAN="I/O Intr"` on such a port
    /// process at all. Parameter-cache ports (echoDriver, USBTMC, GPIB,
    /// IP-server) pass 0 and are unaffected.
    pub octet_interrupt_process: bool,
    /// Per-address device state for multi-device ports — see [`DeviceTable`].
    device_states: Arc<Mutex<HashMap<i32, DeviceState>>>,
    /// The source [`Self::update_time_stamp`] asks; `None` is C's
    /// `defaultTimeStampSource`, the current time.
    pub timestamp_source: Option<Arc<dyn Fn() -> SystemTime + Send + Sync>>,
    /// C `port.timeStamp` (asynManager.c:227): what every callback and
    /// parameter read carries. Zero-initialised in C, so the EPICS epoch
    /// until the driver updates or sets it.
    time_stamp: SystemTime,
    /// By reason; empty until a rule is registered, so a port without one
    /// pays a bounds check per store.
    int32_rules: Vec<Option<Int32StoreRule>>,
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
    /// `exceptionDisconnect`, asynManager.c:2183-2184) and re-armed by the
    /// actor after a failed attempt; cleared on connect. The actor is what
    /// services it — see `PortActor::service_connect_timer` — so this field
    /// is the whole handoff between the transition owner and the timer.
    pub connect_retry_at: Option<Instant>,
    /// Back-off between failed autonomous connect attempts. C
    /// `port.secondsBetweenPortConnect`, initialised to
    /// `DEFAULT_SECONDS_BETWEEN_PORT_CONNECT` = 20 s (asynManager.c:48, 3280)
    /// and used to re-arm the timer at asynManager.c:3312.
    pub seconds_between_port_connect: Duration,
    /// How many times this port's link has come up — C `dpCommon.numberConnects`
    /// (asynManager.c:150), incremented by `exceptionConnect` (:2160) and printed
    /// by `asynReport` (:1059-1062). Its owner is
    /// [`Self::sync_connection_edge`], the same edge owner that raises the
    /// exception, so a connect that was never published is never counted.
    pub number_connects: u64,
}

/// The one way to announce a port exception — C `announceExceptionOccurred`
/// (asynManager.c:613-639): fan the event out over the port's exception list and
/// signal `notifyPortThread` (:637-638).
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
    /// An `asynPortDriver` base: more than one address makes the port
    /// multi-device even when the caller left the flag off
    /// (asynPortDriver.cpp:4013-4014).
    pub fn new(port_name: &str, max_addr: usize, mut flags: PortFlags) -> Self {
        flags.multi_device |= max_addr > 1;
        Self::new_exact_flags(port_name, max_addr, flags)
    }

    /// A base whose flags are registered as given, for the drivers that, like
    /// C's `drvAsyn*` ports, call `registerPort` themselves rather than derive
    /// from `asynPortDriver`.
    pub fn new_exact_flags(port_name: &str, max_addr: usize, flags: PortFlags) -> Self {
        let max_addr = max_addr.max(1);
        Self {
            port_name: port_name.into(),
            max_addr,
            flags,
            params: ParamList::new(max_addr),
            interrupts: InterruptManager::new(),
            connected: Connection::Own(Arc::new(AtomicBool::new(true))),
            last_announced: true,
            enabled: Arc::new(AtomicBool::new(true)),
            auto_connect: Arc::new(AtomicBool::new(true)),
            defunct: Arc::default(),
            owed: Arc::default(),
            lock_port_timeout: Arc::default(),
            announcer: ExceptionAnnouncer::new(port_name),
            options: HashMap::new(),
            eos: HashMap::new(),
            interpose_octet: OctetInterposeStack::new(flags.multi_device),
            trace: None,
            octet_interrupt_process: false,
            device_states: Arc::default(),
            timestamp_source: None,
            time_stamp: SystemTime::UNIX_EPOCH
                + Duration::from_secs(
                    epics_libcom_rs::runtime::general_time::EPICS_EPOCH_UNIX_SECS,
                ),
            int32_rules: Vec::new(),
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
    /// C `announceExceptionOccurred` (asynManager.c:613-639) ends by signalling
    /// `notifyPortThread` on a CANBLOCK port (:637-638) — the announcement *is* a
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
    /// `notifyPortThread` (asynManager.c:637-638).
    pub fn exceptions_announced(&self) -> u64 {
        self.announcer.count()
    }

    /// How many callbacks are registered on this port's exception list — the
    /// `exceptionUsers` count `reportPrintPort` prints (asynManager.c:1070-1074).
    /// `asynReport` itself is the iocsh command (asynShellCommands.c:587); it
    /// reaches this through `asynManager`'s own `report` (:1138).
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
        if let Connection::Own(c) = &self.connected {
            c.store(connected, Ordering::Release);
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
    ///
    /// `args` is formatted only when `mask` is enabled, onto the stack: a
    /// driver write that fails under a record's lock allocates nothing.
    pub fn trace_print(&self, mask: crate::trace::TraceMask, args: std::fmt::Arguments<'_>) {
        if let Some(trace) = &self.trace {
            trace.print(Some(&self.port_name), None, 0, mask, args);
        }
    }

    /// The `ASYN_TRACEIO_DRIVER` line C's `asynPortDriver::read*` and
    /// `write*` print on success (asynPortDriver.cpp:1988, :2127, :2267,
    /// :2516, :2626; :2038, :2177, :2318, :2565, :2681): `driverName:
    /// functionName: function=N, name=PARAM, value=V`, through the user's
    /// device-level trace config.
    fn trace_io(&self, user: &AsynUser, function_name: &str, value: std::fmt::Arguments<'_>) {
        user.print(
            crate::trace::TraceMask::IO_DRIVER,
            file!(),
            line!(),
            format_args!(
                "asynPortDriver:{}: function={}, name={}, value={}",
                function_name,
                user.reason,
                self.params.param_name(user.reason).unwrap_or(""),
                value
            ),
        );
    }

    /// C `asynPortDriver::writeInt32`: store the value, run the callbacks for
    /// `user.addr`, trace the write. The [`PortDriver::write_int32`] default,
    /// and the fall-through a driver override takes for a reason it does not
    /// handle itself.
    pub fn write_int32_param(&mut self, user: &AsynUser, value: i32) -> AsynResult<()> {
        let addr = self.parse_user(user)?;
        let _ = self.set_int32_param(user.reason, addr, value);
        self.call_param_callbacks(addr).map_err(|e| {
            param_access_error(
                &self.params,
                user,
                "writeInt32",
                e,
                format_args!("value={value}"),
            )
        })?;
        self.trace_io(user, "writeInt32", format_args!("{value}"));
        Ok(())
    }

    /// C `asynPortDriver::writeInt64`; see [`Self::write_int32_param`].
    pub fn write_int64_param(&mut self, user: &AsynUser, value: i64) -> AsynResult<()> {
        let addr = self.parse_user(user)?;
        let _ = self.set_int64_param(user.reason, addr, value);
        self.call_param_callbacks(addr).map_err(|e| {
            param_access_error(
                &self.params,
                user,
                "writeInt64",
                e,
                format_args!("value={value}"),
            )
        })?;
        self.trace_io(user, "writeInt64", format_args!("{value}"));
        Ok(())
    }

    /// C `asynPortDriver::writeUInt32Digital`; see [`Self::write_int32_param`].
    /// The write carries no forced interrupt mask: the changed bits derive
    /// from value^old.
    pub fn write_uint32_digital_param(
        &mut self,
        user: &AsynUser,
        value: u32,
        mask: u32,
    ) -> AsynResult<()> {
        let addr = self.parse_user(user)?;
        let _ = self.set_uint32_param(user.reason, addr, value, mask, 0);
        self.call_param_callbacks(addr).map_err(|e| {
            param_access_error(
                &self.params,
                user,
                "writeUInt32Digital",
                e,
                format_args!("value={value}, mask={mask}"),
            )
        })?;
        self.trace_io(
            user,
            "writeUInt32Digital",
            format_args!("{}, mask={mask}", value as i32),
        );
        Ok(())
    }

    /// Single owner-API for the port-level `connected` transition.
    ///
    /// C parity: `exceptionConnect` (asynManager.c:2153-2162) and
    /// `exceptionDisconnect` (:2176-2187) fire
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
        if let Connection::Own(c) = &self.connected {
            c.store(connected, Ordering::Release);
        }
        self.sync_connection_edge()
    }

    /// Publish the port's connection edge if the truth has moved since the last
    /// fan-out: the single owner of `exceptionConnect` (asynManager.c:2142-2163)
    /// and `exceptionDisconnect` (:2165-2190), of the interpose stack's connection
    /// reset and of the retry timer.
    ///
    /// [`Self::set_connected`] is one caller. The other is the actor, on a port
    /// whose link is owned elsewhere: the owner (an IP-server listener assigning a
    /// slot) moves the truth without this port's actor running, and C fans that
    /// edge out from the owner's thread — `pasynCommonSyncIO->connectDevice` on
    /// the child (drvAsynIPServerPort.c:372-375). Here it is published when the
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
            // every disconnect (asynManager.c:2186) so the auto-reconnect
            // throttle measures from the moment the link dropped.
            self.last_connect_disconnect = Some(Instant::now());
            // ...and arms the port's connect timer at .01 s when the port is
            // auto-connect (asynManager.c:2183-2185), which is what makes the
            // reconnect *autonomous*: it does not wait for queued traffic.
            if self.is_auto_connect() {
                self.connect_retry_at = Some(Instant::now() + CONNECT_RETRY_INITIAL);
            }
        } else {
            // C `exceptionConnect` counts the connects it publishes
            // (`++pdpCommon->numberConnects`, asynManager.c:2160) — the count
            // `asynReport` prints, and the operator's only way to see a port that
            // is flapping. It belongs to this owner because C increments it in the
            // same function that raises the exception, so a connect that never
            // fanned out is never counted.
            self.number_connects += 1;
            // The link is up — nothing left to retry. (C leaves the timer
            // running and lets `portConnectTimerCallback` no-op on the
            // `!connected` guard, asynManager.c:3289; disarming here is the
            // same observable behaviour without the pointless wakeup.)
            self.connect_retry_at = None;
        }
        // The interpose stack is a subscriber of this transition, exactly as
        // in C: `asynInterposeEos` registers an exception callback
        // (asynInterposeEos.c:110) and drops its read-ahead buffer +
        // partial-EOS match on `asynExceptionConnect`
        // (asynInterposeEos.c:142-151). Both C edges — `exceptionConnect`
        // (asynManager.c:2161) and `exceptionDisconnect` (asynManager.c:2187)
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
    /// (asynManager.c:613-627 fans out per-`dpCommon`). `interpose_octet`
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
        let changed = self.device_mut(addr, |d| {
            if d.connected() == connected {
                return false;
            }
            d.put_connected(connected);
            if !connected {
                // Per-device disconnect stamp — same throttle anchor as the
                // port-level path (C `exceptionDisconnect`, asynManager.c:2184).
                d.last_connect_disconnect = Some(Instant::now());
            }
            true
        });
        if !changed {
            return false;
        }
        self.announce_exception(AsynException::Connect, addr);
        true
    }

    /// 2.0s auto-reconnect throttle gate — C `autoConnectDevice`
    /// (asynManager.c:712-713, 731-732).
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
                .lock()
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

    /// C `findDpCommon(puserPvt)->enabled` (asynManager.c:2356) — what
    /// `pasynManager->isEnabled` answers for this address.
    pub fn dp_enabled(&self, addr: i32) -> bool {
        self.device(addr, DeviceState::enabled)
            .unwrap_or_else(|| self.is_enabled())
    }

    /// C `findDpCommon(puserPvt)->connected` (asynManager.c:2340) — what
    /// `pasynManager->isConnected` answers for this address.
    pub fn dp_connected(&self, addr: i32) -> bool {
        self.device(addr, DeviceState::connected)
            .unwrap_or_else(|| self.is_connected())
    }

    /// C `findDpCommon(puserPvt)->autoConnect` (asynManager.c:2370) — what
    /// `pasynManager->isAutoConnect` answers for this address.
    pub fn dp_auto_connect(&self, addr: i32) -> bool {
        self.device(addr, DeviceState::auto_connect)
            .unwrap_or_else(|| self.is_auto_connect())
    }

    /// Single owner for the post-attempt throttle stamp. C
    /// `autoConnectDevice` stamps `lastConnectDisconnect` immediately after
    /// every `connectAttempt`, success or failure (asynManager.c:718,
    /// 735), so the window restarts from the end of the attempt — a failed
    /// reconnect is not retried until the throttle elapses again.
    pub fn stamp_auto_connect_attempt(&mut self, addr: i32, now: Instant) {
        if self.is_device_addr(addr) {
            self.device_mut(addr, |d| d.last_connect_disconnect = Some(now));
        } else {
            self.last_connect_disconnect = Some(now);
        }
    }

    /// Query whether the port is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// The port-level queue gate over this port's cells — see [`QueueGate`].
    pub(crate) fn queue_gate(&self) -> QueueGate {
        QueueGate {
            port_name: Arc::clone(&self.port_name),
            enabled: Arc::clone(&self.enabled),
            connected: Arc::clone(self.connected.cell()),
        }
    }

    /// Single owner-API for the port-level `enabled` transition.
    ///
    /// C `enable` (asynManager.c:2224-2251) refuses a shut-down port:
    /// when `defunct` it returns `asynDisabled` *without* touching
    /// `enabled` and *without* firing the `asynExceptionEnable` fan-out.
    /// Otherwise it sets `enabled` and announces unconditionally (no
    /// state-change guard). The actor's `SetEnable` op is a lifecycle op
    /// that bypasses [`Self::check_ready`], so this guard is the only
    /// thing that stops a defunct port from being re-enabled or fanning
    /// out a spurious exception — it must live here, in the one owner of
    /// the transition.
    pub fn set_enabled(&mut self, enabled: bool) -> AsynResult<()> {
        self.set_addr_enabled(-1, enabled)
    }

    /// Per-address variant — same defunct refusal as [`Self::set_enabled`].
    /// `defunct` is modelled at the port level (a shut-down port takes its
    /// devices with it), so a defunct port refuses per-device enable/disable
    /// too, matching C's `dpCommon.defunct` check on the resolved device.
    pub fn set_addr_enabled(&mut self, addr: i32, enabled: bool) -> AsynResult<()> {
        // C `enable` writes `findDpCommon(puserPvt)->enabled` (asynManager.c:2245),
        // so an addr that resolves to the port's own slot is the port's enable —
        // not a device entry no reader would ever resolve to.
        self.device_table().enable(addr, enabled)?;
        self.drain_announces();
        Ok(())
    }

    /// Query whether auto-connect is enabled.
    pub fn is_auto_connect(&self) -> bool {
        self.auto_connect.load(Ordering::Acquire)
    }

    /// The port's initial auto-connect flag, set while it is being
    /// constructed: no `asynExceptionAutoConnect` and no retry timer. Every
    /// change after that goes through [`Self::set_auto_connect`].
    pub fn init_auto_connect(&mut self, yes: bool) {
        self.auto_connect.store(yes, Ordering::Release);
    }

    /// Toggle the auto-connect flag at runtime.
    ///
    /// C parity: `autoConnectAsyn` (asynManager.c:2312-2329) always
    /// fires `asynExceptionAutoConnect` regardless of prior state
    /// (no state-change guard). Mirror that — every call announces.
    /// Driver constructors that initialise it through `init_auto_connect`
    /// during `PortDriver::new()` keep the silent path
    /// (the port is not yet registered, so no listeners exist).
    pub fn set_auto_connect(&mut self, yes: bool) {
        self.set_auto_connect_addr(-1, yes);
    }

    /// What C `enable` and `autoConnectAsyn` do after their store
    /// (asynManager.c:2249, :2323-2326) — start the port's connect timer and
    /// raise the exception on the dpCommon it was made on — for every store
    /// [`DeviceTable`] recorded, on this thread or a handle's, oldest first.
    pub(crate) fn drain_announces(&mut self) {
        if !self.owed.lock().any {
            return;
        }
        let table = self.device_table();
        while let Some((kind, addr, start_timer)) = table.take_oldest_owed() {
            if start_timer {
                self.connect_retry_at = Some(Instant::now() + CONNECT_RETRY_INITIAL);
            }
            self.announce_exception(kind, addr);
        }
    }

    /// Per-address variant — for multi-device ports. C parity:
    /// `autoConnectAsyn` walks dpCommon via findDpCommon so a per-
    /// device pasynUser hits the device's dpc, otherwise the port's
    /// dpc (asynManager.c:2316 + findDpCommon).
    pub fn set_auto_connect_addr(&mut self, addr: i32, yes: bool) {
        // Same `findDpCommon` split as [`Self::set_addr_enabled`]
        // (asynManager.c:2316).
        self.device_table().auto_connect(addr, yes);
        self.drain_announces();
    }

    /// Query whether the port has been marked defunct via
    /// [`Self::shutdown_lifecycle`] — once true the port is gone for
    /// good, mirroring C asynManager.c:2268-2271.
    pub fn is_defunct(&self) -> bool {
        self.defunct.load(Ordering::Acquire)
    }

    /// The one place a shut-down port is named in an error message, and the
    /// only one C has: `enable` on a defunct device (asynManager.c:2238-2243).
    /// Every other gate refuses a defunct port as a disabled one.
    fn shut_down_error() -> AsynError {
        AsynError::Status {
            status: AsynStatus::Disabled,
            message: "asynManager:enable: port has been shut down".to_string(),
        }
    }

    /// C `queueRequest`'s gate (asynManager.c:1543-1554), and the single owner
    /// of the two refusals it is built from.
    ///
    /// They are independent, and the [`ConnectCheck`] the caller hands in
    /// selects between them — it can waive the *connected* refusal and nothing
    /// else. There is no argument, op class or priority that waives the
    /// enabled refusal: C's `if(!pport->dpc.enabled) return asynDisabled`
    /// (:1543-1548) sits *above* `checkPortConnect` and is reached by every
    /// request, and its port thread refuses to run anything at all on a disabled
    /// port (`portThread`, :808-811).
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
    /// :808-901). This is the only place that stamps it.
    pub fn check_queue(&self, addr: i32, connect: ConnectCheck) -> AsynResult<()> {
        self.check_queue_inner(addr, connect)
            .map_err(AsynError::into_queue_refusal)
    }

    /// The gate's body, before the refusal is stamped as a queue refusal.
    ///
    /// Three blocks, in C's order:
    ///
    /// 1. `!pport->dpc.enabled → asynDisabled` (:1543-1548) — port level,
    ///    unconditional, reached by every request.
    /// 2. `checkPortConnect && !pport->dpc.connected → asynDisconnected`
    ///    (:1549-1554) — port level, waived by the request's own user. (C's
    ///    `checkPortConnect == FALSE` reads *no* connected flag: the port thread
    ///    drains the Connect queue before it ever calls `autoConnectDevice`,
    ///    :813-857.)
    /// 3. The **device** block (:1555-1577), which C runs only for a
    ///    *synchronous* port: it sits bodily inside
    ///    `if(!(pport->attributes & ASYN_CANBLOCK))` at :1555. A CANBLOCK port's
    ///    device-level enabled/connected checks belong to the port THREAD
    ///    (:877-888), which does something else entirely with them — a disabled
    ///    device's request *waits in the queue* (`continue`), it is not refused
    ///    — so applying them here refused requests C parks (R15-47). Every real
    ///    transport port is CANBLOCK.
    fn check_queue_inner(&self, addr: i32, connect: ConnectCheck) -> AsynResult<()> {
        self.queue_gate().check(connect)?;
        if !self.flags.can_block {
            // C :1563-1569 — the device-enabled refusal in the synchronous block
            // is *not* conditioned on `checkPortConnect`; only the connected one
            // below is (:1570).
            self.check_device_enabled(addr)?;
            if connect == ConnectCheck::Required {
                self.check_device_connected(addr)?;
            }
        }
        Ok(())
    }

    /// The device-level enabled refusal, C's text verbatim
    /// (asynManager.c:1563-1569). Its *use* differs by port class, which is why
    /// it is a check and not a gate: `queueRequest` returns it on a synchronous
    /// port, while on a CANBLOCK port the same condition makes `portThread` park
    /// the request instead (:877). Both callers ask this one function.
    ///
    /// A port that is not `ASYN_MULTIDEVICE`, or an address with no device state,
    /// resolves to the port's own `dpCommon` in C's `findDpCommon` — already
    /// checked above — so there is nothing device-level left to refuse.
    pub fn check_device_enabled(&self, addr: i32) -> AsynResult<()> {
        if self.device(addr, DeviceState::enabled) == Some(false) {
            return Err(AsynError::Status {
                status: AsynStatus::Disabled,
                // C's double space is verbatim (asynManager.c:1566).
                message: format!("port {}  or device {} not enabled", self.port_name, addr),
            });
        }
        Ok(())
    }

    /// The device-level connected refusal (asynManager.c:1570-1577). Same
    /// split as [`Self::check_device_enabled`]: `queueRequest` returns it on a
    /// synchronous port; on a CANBLOCK port `portThread` answers the same
    /// condition with the request's timeout callback (:886-887).
    pub fn check_device_connected(&self, addr: i32) -> AsynResult<()> {
        if self.device(addr, DeviceState::connected) == Some(false) {
            return Err(AsynError::Status {
                status: AsynStatus::Disconnected,
                message: format!("port {} or device {} not connected", self.port_name, addr),
            });
        }
        Ok(())
    }

    /// The device `addr` resolves to, or `None` when C's `findDpCommon` would
    /// resolve it to the port's own `dpCommon` (not multi-device, or no device
    /// created at that address).
    fn device<R>(&self, addr: i32, read: impl FnOnce(&DeviceState) -> R) -> Option<R> {
        let addr = self.dp_addr(addr)?;
        self.device_states.lock().get(&addr).map(read)
    }

    /// The devices created so far, in address order, as `asynReport` walks them.
    pub(crate) fn devices(&self) -> Vec<(i32, DeviceState)> {
        let mut devices: Vec<_> = self
            .device_states
            .lock()
            .iter()
            .map(|(addr, ds)| (*addr, ds.clone()))
            .collect();
        devices.sort_unstable_by_key(|(addr, _)| *addr);
        devices
    }

    /// Check that the port is enabled, connected, and not defunct.
    /// Returns `Err(Disabled)`, `Err(Disconnected)`, or `Err(Disabled)`
    /// (defunct => permanently disabled) otherwise.
    ///
    /// There is no defunct branch: `shutdownPort` clears `enabled` alongside
    /// setting `defunct` (asynManager.c:2285-2286), and the invariant
    /// `defunct ⟹ !enabled` (see the field doc) lets the one check answer for
    /// both.
    pub fn check_ready(&self) -> AsynResult<()> {
        self.queue_gate()
            .check(ConnectCheck::Required)
            .map_err(|refusal| AsynError::Status {
                status: refusal.status(),
                message: refusal.to_string(),
            })
    }

    /// Run the C `shutdownPort` lifecycle (asynManager.c:2253-2310):
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
        // C holds `asynManagerLock` from the defunct check to the store
        // (asynManager.c:2264-2306), which is what [`DeviceTable::enable`]
        // takes: no enable can land between them.
        let devices = Arc::clone(&self.device_states);
        let devices = devices.lock();
        if self.is_defunct() {
            // Idempotent — C asynManager.c:2268-2271 returns asynSuccess.
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
        self.enabled.store(false, Ordering::Release);
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
        for ds in devices.values() {
            ds.put_enabled(false);
        }
        self.defunct.store(true, Ordering::Release);
        drop(devices);
        self.announce_exception(AsynException::Shutdown, -1);
        Ok(())
    }

    /// Check that port + device address are both ready — the whole of C's
    /// synchronous-port gate (asynManager.c:1563-1579) in one call. Drivers that
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
    /// `enable` / `exceptionConnect` (asynManager.c:2247, 2160) *and* could
    /// create a device on a port that has none — a state C cannot represent,
    /// since `locateDevice` refuses to allocate one unless
    /// `attributes&ASYN_MULTIDEVICE` (:576). Outside callers use the three
    /// owners [`Self::set_addr_connected`], [`Self::set_addr_enabled`] and
    /// [`Self::set_auto_connect_addr`], each of which resolves the addr first.
    #[cfg(test)]
    pub(crate) fn device_state(&self, addr: i32) -> DeviceState {
        self.device_mut(addr, |d| d.clone())
    }

    /// The device at `addr`, created if new, under the [`DeviceTable`] lock.
    fn device_mut<R>(&self, addr: i32, f: impl FnOnce(&mut DeviceState) -> R) -> R {
        f(locate_device(&mut self.device_states.lock(), addr, || {
            self.port_cells()
        }))
    }

    /// The port's own `dpCommon` cells — C `pport->dpc`.
    fn port_cells(&self) -> DpCells {
        DpCells {
            connected: Arc::clone(self.connected.cell()),
            enabled: Arc::clone(&self.enabled),
            auto_connect: Arc::clone(&self.auto_connect),
        }
    }

    /// This port's [`DeviceTable`], for a [`crate::port_handle::PortHandle`].
    /// Like [`Self::queue_gate`] it captures the port's cells as they are now.
    pub(crate) fn device_table(&self) -> DeviceTable {
        DeviceTable {
            multi_device: self.flags.multi_device,
            port: self.port_cells(),
            devices: Arc::clone(&self.device_states),
            defunct: Arc::clone(&self.defunct),
            owed: Arc::clone(&self.owed),
            lock_port_timeout: Arc::clone(&self.lock_port_timeout),
        }
    }

    /// C `connectDevice`'s `locateDevice(pport, addr, TRUE)`
    /// (asynManager.c:1349-1352, :576-589): a user binding to a device on a
    /// multi-device port creates that device's dpCommon, seeded from the
    /// port's, if it is not there yet. The resolution is [`Self::dp_addr`]'s:
    /// a single-device port and an `addr < 0` bind to the port's own
    /// dpCommon and allocate nothing, as C's `locateDevice` returns null for
    /// both. This is what makes `asynReport`'s `nDevices` and per-device
    /// block count a device the moment a record or `asynRecord` connects
    /// to it, before any I/O or connection transition touches it.
    ///
    /// Answers the cells of the `dpCommon` the user is now bound to — C's
    /// `findDpCommon` (asynManager.c:538-546) resolved once, as `pdevice` is.
    pub fn connect_user(&self, addr: i32) -> DpCells {
        self.device_table().connect_user(addr)
    }

    /// Check if a specific device address is connected.
    pub fn is_device_connected(&self, addr: i32) -> bool {
        self.dp_connected(addr)
    }

    /// Set a specific device address as connected.
    ///
    /// C parity: announce only on actual transition
    /// (asynManager.c:2153-2162 — `exceptionConnect` rejects
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
    /// (asynManager.c:2176-2187). Thin wrapper over
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

    /// Set the source [`Self::update_time_stamp`] asks — C
    /// `registerTimeStampSource` (asynManager.c:2681-2696).
    pub fn register_timestamp_source<F>(&mut self, source: F)
    where
        F: Fn() -> SystemTime + Send + Sync + 'static,
    {
        self.timestamp_source = Some(Arc::new(source));
    }

    /// Drop a registered source — C `unregisterTimeStampSource`
    /// (asynManager.c:335), which restores `defaultTimeStampSource` (:333).
    pub fn unregister_timestamp_source(&mut self) {
        self.timestamp_source = None;
    }

    /// C `updateTimeStamp` (asynManager.c:2715-2734): ask the source and keep
    /// its answer as the port's timestamp, which is returned.
    pub fn update_time_stamp(&mut self) -> SystemTime {
        self.time_stamp = self
            .timestamp_source
            .as_ref()
            .map_or_else(SystemTime::now, |f| f());
        self.time_stamp
    }

    /// C `getTimeStamp` (asynManager.c:2736-2751): the last updated or set
    /// timestamp, never a fresh one.
    pub fn get_time_stamp(&self) -> SystemTime {
        self.time_stamp
    }

    /// C `setTimeStamp` (asynManager.c:2753-2768).
    pub fn set_time_stamp(&mut self, ts: SystemTime) {
        self.time_stamp = ts;
    }

    pub fn create_param(&mut self, name: &str, param_type: ParamType) -> AsynResult<usize> {
        self.params.create_param(name, param_type)
    }

    pub fn find_param(&self, name: &str) -> Option<usize> {
        self.params.find_param(name)
    }

    /// C `asynPortDriver::drvUserCreate` (asynPortDriver.cpp:3555-3580): the
    /// reason named by `drv_info`. An override that does not recognise a
    /// drvInfo falls back here, as C subclasses call the base class.
    pub fn drv_user_create(&self, req: &DrvUserRequest) -> AsynResult<DrvUserInfo> {
        let addr = self.get_address(req.addr, 0)?;
        let Some(reason) = self.params.find_param(&req.drv_info) else {
            self.print_device(
                req.addr,
                crate::trace::TraceMask::ERROR,
                line!(),
                format_args!(
                    "asynPortDriver:drvUserCreate: addr={addr}, cannot find parameter {}",
                    req.drv_info
                ),
            );
            return Err(AsynError::ParamNotFound(req.drv_info.clone()));
        };
        self.print_device(
            req.addr,
            crate::trace::TraceMask::FLOW,
            line!(),
            format_args!(
                "asynPortDriver:drvUserCreate: drvInfo={}, index={reason}",
                req.drv_info
            ),
        );
        Ok(DrvUserInfo::from_reason(reason))
    }

    /// C `asynPrint(pasynUser, …)` for a user on device `addr` of this port.
    fn print_device(
        &self,
        addr: i32,
        mask: crate::trace::TraceMask,
        line: u32,
        args: std::fmt::Arguments<'_>,
    ) {
        if let Some(trace) = &self.trace
            && trace.is_enabled_device(&self.port_name, addr, mask)
        {
            trace.output_device_with_source(
                &self.port_name,
                Some(addr),
                0,
                mask,
                file!(),
                line,
                &args.to_string(),
            );
        }
    }

    /// C `asynPortDriver::getAddress` (asynPortDriver.cpp:1901-1914): the
    /// parameter list `addr` selects ([`param_list`]), printing a refused one
    /// through the device's trace.
    pub fn get_address(&self, addr: i32, reason: usize) -> AsynResult<i32> {
        let address = param_list(self.flags.multi_device, addr);
        if address >= 0 && (address as usize) < self.max_addr {
            return Ok(address);
        }
        let mask = crate::trace::TraceMask::ERROR;
        if let Some(trace) = &self.trace
            && trace.is_enabled_device(&self.port_name, addr, mask)
        {
            trace.output_device_with_source(
                &self.port_name,
                Some(addr),
                reason as i32,
                mask,
                file!(),
                line!(),
                &format!(
                    "asynPortDriver:getAddress: {} invalid address={address}, must be in range 0 to {}",
                    self.port_name,
                    self.max_addr as i64 - 1
                ),
            );
        }
        Err(AsynError::AddressOutOfRange(address))
    }

    /// C `asynPortDriver::parseAsynUser` (asynPortDriver.cpp:1924-1933),
    /// which every default read/write runs first: the address `getAddress`
    /// resolves, then the reason.
    pub(crate) fn parse_user(&self, user: &AsynUser) -> AsynResult<i32> {
        let addr = self.get_address(user.addr, user.reason)?;
        if self.params.param_name(user.reason).is_none() {
            return Err(AsynError::ParamIndexOutOfRange(user.reason));
        }
        Ok(addr)
    }

    // --- Convenience param accessors ---

    /// C `reportSetParamErrors` (asynPortDriver.cpp:1099-1115), which every
    /// `set*Param` runs on a refused set; other refusals print nothing.
    fn report_set_param(&self, function_name: &str, index: usize, addr: i32, e: &AsynError) {
        let why = match e {
            AsynError::ParamIndexOutOfRange(_) => "bad index",
            AsynError::TypeMismatch { .. } => "wrong type",
            AsynError::AddressOutOfRange(_) => "invalid list",
            _ => return,
        };
        self.trace_print(
            crate::trace::TraceMask::ERROR,
            format_args!(
                "asynPortDriver:{function_name}: port={} error setting parameter {index} in list {addr}, {why}",
                self.port_name
            ),
        );
    }

    /// C `reportGetParamErrors` (asynPortDriver.cpp:1575-1597): a bad
    /// index or wrong type at `ASYN_TRACE_ERROR`, an undefined value at
    /// `ASYN_TRACE_FLOW`; an invalid list prints nothing.
    fn report_get_param(&self, function_name: &str, index: usize, addr: i32, e: &AsynError) {
        let (mask, why) = match e {
            AsynError::ParamIndexOutOfRange(_) => (crate::trace::TraceMask::ERROR, "bad index"),
            AsynError::TypeMismatch { .. } => (crate::trace::TraceMask::ERROR, "wrong type"),
            AsynError::ParamUndefined(_) => (crate::trace::TraceMask::FLOW, "value undefined"),
            _ => return,
        };
        // An undefined read is routine; nothing is formatted unless it prints.
        if !self
            .trace
            .as_ref()
            .is_some_and(|t| t.is_enabled(&self.port_name, mask))
        {
            return;
        }
        let port = &self.port_name;
        match self.params.param_name(index) {
            Some(name) if why != "bad index" => self.trace_print(
                mask,
                format_args!(
                    "asynPortDriver:{function_name}: port={port} error getting parameter {index} {name}, in list {addr}, {why}"
                ),
            ),
            _ => self.trace_print(
                mask,
                format_args!(
                    "asynPortDriver:{function_name}: port={port} error getting parameter {index} in list {addr}, {why}"
                ),
            ),
        }
    }

    fn reported(
        &self,
        function_name: &str,
        index: usize,
        addr: i32,
        result: AsynResult<()>,
    ) -> AsynResult<()> {
        if let Err(e) = &result {
            self.report_set_param(function_name, index, addr, e);
        }
        result
    }

    /// Run `rule` on every Int32 store of `reason`, replacing any rule
    /// registered for it.
    pub fn set_int32_rule(&mut self, reason: usize, rule: Int32StoreRule) {
        if self.int32_rules.len() <= reason {
            self.int32_rules.resize(reason + 1, None);
        }
        self.int32_rules[reason] = Some(rule);
    }

    /// The one Int32 store: every setter of an Int32 value comes through here,
    /// so a registered rule cannot be skipped. As in C, the value is stored
    /// whatever the rule returns.
    fn store_int32(&mut self, index: usize, addr: i32, value: i32) -> AsynResult<()> {
        let ruled = match self.int32_rules.get(index) {
            Some(Some(rule)) => rule.clone()(self, addr, value),
            _ => Ok(()),
        };
        let result = self.params.set_int32(index, addr, value);
        self.reported("setIntegerParam", index, addr, result)?;
        ruled
    }

    pub fn set_int32_param(&mut self, index: usize, addr: i32, value: i32) -> AsynResult<()> {
        self.store_int32(index, addr, value)
    }

    pub fn get_int32_param(&self, index: usize, addr: i32) -> AsynResult<i32> {
        let result = self.params.get_int32(index, addr);
        if let Err(e) = &result {
            self.report_get_param("getIntegerParam", index, addr, e);
        }
        result
    }

    pub fn set_int64_param(&mut self, index: usize, addr: i32, value: i64) -> AsynResult<()> {
        let result = self.params.set_int64(index, addr, value);
        self.reported("setInteger64Param", index, addr, result)
    }

    pub fn get_int64_param(&self, index: usize, addr: i32) -> AsynResult<i64> {
        let result = self.params.get_int64(index, addr);
        if let Err(e) = &result {
            self.report_get_param("getInteger64Param", index, addr, e);
        }
        result
    }

    pub fn set_float64_param(&mut self, index: usize, addr: i32, value: f64) -> AsynResult<()> {
        let result = self.params.set_float64(index, addr, value);
        self.reported("setDoubleParam", index, addr, result)
    }

    pub fn get_float64_param(&self, index: usize, addr: i32) -> AsynResult<f64> {
        let result = self.params.get_float64(index, addr);
        if let Err(e) = &result {
            self.report_get_param("getDoubleParam", index, addr, e);
        }
        result
    }

    pub fn set_string_param(
        &mut self,
        index: usize,
        addr: i32,
        value: impl AsRef<[u8]>,
    ) -> AsynResult<()> {
        let result = self.params.set_string(index, addr, value);
        self.reported("setStringParam", index, addr, result)
    }

    pub fn get_string_param(&self, index: usize, addr: i32) -> AsynResult<&[u8]> {
        let result = self.params.get_string(index, addr);
        if let Err(e) = &result {
            self.report_get_param("getStringParam", index, addr, e);
        }
        result
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
        let result = self
            .params
            .set_uint32(index, addr, value, mask, interrupt_mask);
        self.reported("setUIntDigitalParam", index, addr, result)
    }

    /// One value of any type, reported under the C `set*Param` that stores
    /// that type; a type C has no setter for reports nothing.
    pub fn set_param_value(
        &mut self,
        index: usize,
        addr: i32,
        value: ParamValue,
    ) -> AsynResult<()> {
        let function_name = match &value {
            ParamValue::Int32(v) => return self.store_int32(index, addr, *v),
            ParamValue::Int64(_) => "setInteger64Param",
            ParamValue::Float64(_) => "setDoubleParam",
            ParamValue::Octet(_) => "setStringParam",
            ParamValue::UInt32Digital(_) => "setUIntDigitalParam",
            _ => return self.params.set_value(index, addr, value),
        };
        let result = self.params.set_value(index, addr, value);
        self.reported(function_name, index, addr, result)
    }

    pub fn get_uint32_param(&self, index: usize, addr: i32) -> AsynResult<u32> {
        let result = self.params.get_uint32(index, addr);
        if let Err(e) = &result {
            self.report_get_param("getUIntDigitalParam", index, addr, e);
        }
        result
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

    pub fn set_param_status(
        &mut self,
        index: usize,
        addr: i32,
        status: AsynStatus,
        alarm_status: u16,
        alarm_severity: u16,
    ) -> AsynResult<()> {
        let result =
            self.params
                .set_param_status(index, addr, status, alarm_status, alarm_severity);
        self.reported("setParamStatus", index, addr, result)
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
    /// `interposeInterface(portName, addr, ...)` (asynManager.c:2192-2222), which
    /// is what the `asynInterposeEcho` / `asynInterposeDelay` iocsh commands call
    /// (asynInterposeEcho.c:176, asynInterposeDelay.c:187,200). On a port that is
    /// not multi-device every addr resolves to the port itself.
    pub fn install_octet_interpose_addr(&mut self, addr: i32, layer: Box<dyn OctetInterpose>) {
        self.interpose_octet.install(addr, layer);
    }

    /// Flush changed parameters as interrupt notifications.
    /// Equivalent to C asyn's callParamCallbacks().
    pub fn call_param_callbacks(&mut self, addr: i32) -> AsynResult<()> {
        // C asynPortDriver.cpp:838 — `callCallbacks` returns before the loop
        // AND before `flags.clear()` (:871) while `interruptAccept` is false,
        // so every callback fired during IOC build is a no-op that PRESERVES
        // the accumulated changed-flags. Returning here, ahead of `take_changed`
        // (which both reads and clears), is that early return: a driver's
        // acquisition/array task can call this before `iocInit` has wired the
        // `_RBV` records, and a read-only seed must survive to the one-shot
        // flush the scan facility fires when the gate goes true.
        if !self.interrupts.accepting() {
            return Ok(());
        }
        // C stamps every callback with getTimeStamp (asynPortDriver.cpp:610-637).
        let ts = self.time_stamp;
        let interrupts = &self.interrupts;
        // C `callCallbacks` (asynPortDriver.cpp:833-873): every changed flag
        // is walked and cleared at the end; only a bad index returns early,
        // with the flags kept.
        self.params.flush_changed(addr, |params, reason| {
            let value = params.get_value(reason, addr)?.clone();
            // C asynPortDriver.cpp:845 — callCallbacks skips firing for an
            // undefined param even though its changed flag is consumed
            // (flags.clear() at :871). A status/alarm change or bare
            // mark_changed on a never-set scalar must not emit an I/O Intr.
            // Array/generic-pointer params have no callCallbacks analog
            // (:846-865 switch is scalar-only) and Rust fires them as a
            // read-trigger regardless, so gate scalars only.
            if !value.is_array() && !params.is_param_defined(reason, addr).unwrap_or(false) {
                return Ok(false);
            }
            // C parity: read the accumulated callback mask and reset it
            // (asynPortDriver.cpp:854-855 fires uint32Callback then sets
            // uInt32CallbackMask = 0). The flush is the single owner of
            // this consume, so accumulated bits never leak to the next.
            let uint32_mask = params.take_uint32_interrupt_mask(reason, addr).unwrap_or(0);
            // C parity: asynPortDriver.cpp:633-637 sets
            // `pInterrupt->pasynUser->auxStatus/alarmStatus/alarmSeverity`
            // from the param's stored status before invoking each
            // subscriber callback. Pull those here so subscribers see
            // the same triplet C consumers do.
            let (aux_status, alarm_status, alarm_severity) = params
                .get_param_status(reason, addr)
                .unwrap_or((AsynStatus::Success, 0, 0));
            interrupts.notify(InterruptValue {
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
            Ok(true)
        })
    }

    /// Flush a single parameter's changed flag and notify if dirty.
    /// Use this instead of `call_param_callbacks` when you want to avoid
    /// flushing unrelated parameters (e.g. rapidly-updating CP-linked params).
    pub fn call_param_callback(&mut self, addr: i32, reason: usize) -> AsynResult<()> {
        // C `interruptAccept` gate — see `call_param_callbacks`: preserve the
        // changed-flag (do not `take_changed_single`) until the IOC accepts
        // interrupts, so a pre-`iocInit` flush cannot strand a seeded value.
        if !self.interrupts.accepting() {
            return Ok(());
        }
        if self.params.take_changed_single(reason, addr)? {
            let value = self.params.get_value(reason, addr)?.clone();
            // C asynPortDriver.cpp:845 — see `call_param_callbacks`: an
            // undefined scalar consumes its changed flag but fires no
            // callback. Array/generic-pointer triggers fire regardless.
            if !value.is_array() && !self.params.is_param_defined(reason, addr).unwrap_or(false) {
                return Ok(());
            }
            let ts = self.time_stamp;
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
        let ts = self.time_stamp;
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

    /// Publish a new enum table for a parameter — C
    /// `asynPortDriver::doCallbacksEnum`. Records bound to the parameter rewrite
    /// their state strings/values/severities and post `DBE_PROPERTY`.
    ///
    /// An `Enum` parameter owns its table, so the table is stored and goes out
    /// with the parameter's own callback. A parameter of any other type (C's
    /// everyday case: an `asynParamInt32` behind an mbbi) has nowhere to keep
    /// one; the table is fired on the asynEnum interface alone, where no value
    /// subscriber sees it.
    pub fn do_callbacks_enum(
        &mut self,
        index: usize,
        addr: i32,
        choices: Arc<[EnumEntry]>,
    ) -> AsynResult<()> {
        if self.params.get_enum(index, addr).is_ok() {
            self.params.set_enum_choices(index, addr, choices)?;
            return self.call_param_callbacks(addr);
        }
        self.notify_interface_value(
            index,
            addr,
            InterfaceType::Enum,
            ParamValue::Enum { index: 0, choices },
            0,
            AsynStatus::Success,
        );
        Ok(())
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
/// port name and the length (asynInterposeEos.c:302-303, :355-356).
fn illegal_eoslen(port: &str, eoslen: usize) -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message: format!("{port} illegal eoslen {eoslen}"),
    }
}

/// C `asynPortDriver`'s `errorMessage` for a failed param read/write default,
/// `"asynPortDriver:<function>: status=%d, function=%d, name=%s, <value>"`. A
/// bad address or reason fails in `parseAsynUser` before it, where C writes
/// none, so those pass through unwrapped.
fn param_access_error(
    params: &crate::param::ParamList,
    user: &AsynUser,
    function: &str,
    e: AsynError,
    value: std::fmt::Arguments<'_>,
) -> AsynError {
    if matches!(
        e,
        AsynError::AddressOutOfRange(_) | AsynError::ParamIndexOutOfRange(_)
    ) {
        return e;
    }
    let message = format!(
        "asynPortDriver:{function}: status={}, function={}, name={}, {value}",
        e.param_status_code(),
        user.reason,
        params.param_name(user.reason).unwrap_or(""),
    );
    e.in_param_access(message)
}

/// C's `writeArray` template default (asynPortDriver.cpp:1820-1826): a port
/// that does not override `writeXxxArray` refuses the put.
fn write_array_not_implemented() -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message: "asynPortDriver:writeArray not implemented".into(),
    }
}

/// C's `readArray` template default (asynPortDriver.cpp:1812-1818).
fn read_array_not_implemented() -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message: "asynPortDriver:readArray not implemented".into(),
    }
}

pub trait PortDriver: Any + Send + Sync {
    fn base(&self) -> &PortDriverBase;
    fn base_mut(&mut self) -> &mut PortDriverBase;

    // --- AsynCommon ---

    /// The trivial driver's `pasynCommon->connect`: C's own does its transport
    /// work and then calls `pasynManager->exceptionConnect(pasynUser)`, which
    /// resolves the slot it marks connected through `findDpCommon` on that same
    /// user (asynManager.c:2142-2163). So the addr the request carries selects
    /// the slot here too — [`PortDriverBase::set_addr_connected`] is the one
    /// owner of that resolution, and a user addressing no device marks the
    /// port's own. A driver that overrides this owns its transport's shape.
    /// C `asynPortDriver::connect` (asynPortDriver.cpp:3766-3778) refuses an
    /// address `getAddress` refuses; so do `disconnect` and `drvUserCreate`.
    fn connect(&mut self, user: &AsynUser) -> AsynResult<()> {
        self.base().get_address(user.addr, user.reason)?;
        // Single owner-API: edge-guarded fire is in PortDriverBase::set_connected.
        self.base_mut().set_addr_connected(user.addr, true);
        user.print(
            crate::trace::TraceMask::FLOW,
            file!(),
            line!(),
            format_args!("asynPortDriver:connect:, pasynUser={user:p}"),
        );
        Ok(())
    }

    fn disconnect(&mut self, user: &AsynUser) -> AsynResult<()> {
        self.base().get_address(user.addr, user.reason)?;
        self.base_mut().set_addr_connected(user.addr, false);
        user.print(
            crate::trace::TraceMask::FLOW,
            file!(),
            line!(),
            format_args!("asynPortDriver:disconnect:, pasynUser={user:p}"),
        );
        Ok(())
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
    /// runs under it (`asynInterposeCom.c:488,508`). The option layer has no
    /// timeout of its own: an asynRecord option put negotiates under TMOT, an
    /// iocsh `asynSetOption` under its own 2 s (`asynShellCommands.c:119`).
    fn set_option(&mut self, _user: &mut AsynUser, key: &str, value: &str) -> AsynResult<()> {
        self.base_mut()
            .options
            .insert(key.to_string(), value.to_string());
        Ok(())
    }

    /// The driver's own report — C `asynCommon::report`, which the manager calls
    /// last (`reportPrintPort`, asynManager.c:1115-1124) after printing the port's
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
                format_timestamp(base.get_time_stamp())
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
    // devAsynInt32.c:844-847).

    // A failure past parseAsynUser carries C's errorMessage for the
    // function, built by param_access_error; the value C prints on a failed
    // read is the untouched caller buffer, which is 0 / "" here.

    fn read_int32(&mut self, user: &AsynUser) -> AsynResult<i32> {
        let base = self.base();
        let addr = base.parse_user(user)?;
        let value = base.get_int32_param(user.reason, addr).map_err(|e| {
            param_access_error(&base.params, user, "readInt32", e, format_args!("value=0"))
        })?;
        base.trace_io(user, "readInt32", format_args!("{value}"));
        Ok(value)
    }

    // The default typed writes are C `asynPortDriver::write*`
    // (asynPortDriver.cpp:2016-2040, :2155-2179, :2296-2320, :2544-2568,
    // :2659-2683): set the parameter, run the callbacks, and report the
    // write at `ASYN_TRACEIO_DRIVER` — the line `asynSetTraceMask port -1
    // 0x2` shows for every value a driver accepted. Their status is
    // callParamCallbacks', which overwrites set*Param's: a set the store
    // refuses (wrong type) is a success, as in C. `writeFloat64` alone keeps
    // the set status and prints its failure at `ASYN_TRACE_ERROR`
    // (:2554-2563); the others put it in `errorMessage`, which is the `Err`.

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        self.base_mut().write_int32_param(user, value)
    }

    fn read_int64(&mut self, user: &AsynUser) -> AsynResult<i64> {
        let base = self.base();
        let addr = base.parse_user(user)?;
        let value = base.get_int64_param(user.reason, addr).map_err(|e| {
            param_access_error(&base.params, user, "readInt64", e, format_args!("value=0"))
        })?;
        base.trace_io(user, "readInt64", format_args!("{value}"));
        Ok(value)
    }

    fn write_int64(&mut self, user: &mut AsynUser, value: i64) -> AsynResult<()> {
        self.base_mut().write_int64_param(user, value)
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
        let base = self.base();
        let addr = base.parse_user(user)?;
        let params = &base.params;
        let value = base.get_float64_param(user.reason, addr).map_err(|e| {
            // readFloat64 alone names an undefined value (:2505-2513).
            let value = if matches!(e, AsynError::ParamUndefined(_)) {
                format_args!("value is undefined")
            } else {
                format_args!("value=0.000000")
            };
            param_access_error(params, user, "readFloat64", e, value)
        })?;
        base.trace_io(user, "readFloat64", format_args!("{value:.6}"));
        Ok(value)
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        let base = self.base_mut();
        let addr = base.parse_user(user)?;
        let status = base.set_float64_param(user.reason, addr, value);
        let _ = base.call_param_callbacks(addr);
        let base = self.base();
        if let Err(e) = &status {
            user.print(
                crate::trace::TraceMask::ERROR,
                file!(),
                line!(),
                format_args!(
                    "asynPortDriver:writeFloat64: error, status={} function={}, name={}, value={:.6}",
                    e.param_status_code(),
                    user.reason,
                    base.params.param_name(user.reason).unwrap_or(""),
                    value
                ),
            );
            return status;
        }
        base.trace_io(user, "writeFloat64", format_args!("{value:.6}"));
        Ok(())
    }

    fn read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        let base = self.base();
        let addr = base.parse_user(user)?;
        let params = &base.params;
        let bytes = base.get_string_param(user.reason, addr).map_err(|e| {
            param_access_error(params, user, "readOctet", e, format_args!("value="))
        })?;
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        base.trace_io(
            user,
            "readOctet",
            format_args!("{}", crate::fixed_text::LossyText(&buf[..n])),
        );
        Ok(n)
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        let base = self.base_mut();
        let addr = base.parse_user(user)?;
        let _ = base.set_string_param(user.reason, addr, data);
        base.call_param_callbacks(addr).map_err(|e| {
            param_access_error(
                &base.params,
                user,
                "writeOctet",
                e,
                format_args!("value={}", crate::fixed_text::LossyText(data)),
            )
        })?;
        self.base().trace_io(
            user,
            "writeOctet",
            format_args!("{}", crate::fixed_text::LossyText(data)),
        );
        Ok(data.len())
    }

    fn read_uint32_digital(&mut self, user: &AsynUser, mask: u32) -> AsynResult<u32> {
        let base = self.base();
        let addr = base.parse_user(user)?;
        let params = &base.params;
        let val = base.get_uint32_param(user.reason, addr).map_err(|e| {
            param_access_error(
                params,
                user,
                "readUInt32Digital",
                e,
                format_args!("value=0 mask={mask}"),
            )
        })?;
        let value = val & mask;
        base.trace_io(
            user,
            "readUInt32Digital",
            format_args!("{value}, mask={mask}"),
        );
        Ok(value)
    }

    fn write_uint32_digital(
        &mut self,
        user: &mut AsynUser,
        value: u32,
        mask: u32,
    ) -> AsynResult<()> {
        self.base_mut()
            .write_uint32_digital_param(user, value, mask)
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
        let base = self.base_mut();
        let addr = base.parse_user(user)?;
        base.params
            .set_uint32_interrupt(user.reason, addr, mask, reason)
    }

    /// Clear bits from rising AND falling masks. C parity:
    /// `asynPortDriver::clearInterruptUInt32Digital`
    /// (`asynPortDriver.cpp:2392-2415`). Mirrors C — the call does
    /// not take an `interruptReason`; both masks are cleared.
    fn clear_interrupt_uint32_digital(&mut self, user: &AsynUser, mask: u32) -> AsynResult<()> {
        let base = self.base_mut();
        let addr = base.parse_user(user)?;
        base.params.clear_uint32_interrupt(user.reason, addr, mask)
    }

    /// Read the configured rising / falling / combined mask. C
    /// parity: `asynPortDriver::getInterruptUInt32Digital`
    /// (`asynPortDriver.cpp:2438-2461`).
    fn get_interrupt_uint32_digital(
        &self,
        user: &AsynUser,
        reason: InterruptReason,
    ) -> AsynResult<u32> {
        let base = self.base();
        let addr = base.parse_user(user)?;
        base.params.get_uint32_interrupt(user.reason, addr, reason)
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

    // --- Array I/O (default: not implemented, as in C) ---

    fn read_float64_array(&mut self, _user: &AsynUser, _buf: &mut [f64]) -> AsynResult<usize> {
        Err(read_array_not_implemented())
    }

    fn write_float64_array(&mut self, _user: &AsynUser, _data: &[f64]) -> AsynResult<()> {
        Err(write_array_not_implemented())
    }

    fn read_int32_array(&mut self, _user: &AsynUser, _buf: &mut [i32]) -> AsynResult<usize> {
        Err(read_array_not_implemented())
    }

    fn write_int32_array(&mut self, _user: &AsynUser, _data: &[i32]) -> AsynResult<()> {
        Err(write_array_not_implemented())
    }

    fn read_int8_array(&mut self, _user: &AsynUser, _buf: &mut [i8]) -> AsynResult<usize> {
        Err(read_array_not_implemented())
    }

    fn write_int8_array(&mut self, _user: &AsynUser, _data: &[i8]) -> AsynResult<()> {
        Err(write_array_not_implemented())
    }

    fn read_int16_array(&mut self, _user: &AsynUser, _buf: &mut [i16]) -> AsynResult<usize> {
        Err(read_array_not_implemented())
    }

    fn write_int16_array(&mut self, _user: &AsynUser, _data: &[i16]) -> AsynResult<()> {
        Err(write_array_not_implemented())
    }

    fn read_int64_array(&mut self, _user: &AsynUser, _buf: &mut [i64]) -> AsynResult<usize> {
        Err(read_array_not_implemented())
    }

    fn write_int64_array(&mut self, _user: &AsynUser, _data: &[i64]) -> AsynResult<()> {
        Err(write_array_not_implemented())
    }

    fn read_float32_array(&mut self, _user: &AsynUser, _buf: &mut [f32]) -> AsynResult<usize> {
        Err(read_array_not_implemented())
    }

    fn write_float32_array(&mut self, _user: &AsynUser, _data: &[f32]) -> AsynResult<()> {
        Err(write_array_not_implemented())
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
    // `defunct` check, then `synchronousLock` (asynManager.c:1791-1800) —
    // and waits for no connection, so an EOS set blocks only behind
    // whoever else holds the port. The connect wait lives elsewhere:
    // `registerPort` calls `waitConnect(pport->pconnectUser,
    // pasynBase->autoConnectTimeout)` on an autoConnect port
    // (asynManager.c:2137), and `waitConnect` `epicsEventWaitWithTimeout`s
    // on a connect event signalled from an exception handler (:3323-3367).
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
    // asynOctetBase.h; asynInterposeEos.c:289-297) and the addr it carries is
    // what picks the device's terminator. A port-wide EOS could not hold two.

    /// The default is C's **Fail stub**, not a silent cache. `initOverride`
    /// swaps a NULL `setInputEos` for `setInputEosFail` (asynOctetBase.c:115),
    /// which answers `"<port> setInputEos not implemented"` and `asynError`
    /// (:370-380, :401-403) — and serial, IP, IP-server and FTDI really do
    /// leave it NULL (drvAsynSerialPort.c:1003, drvAsynIPPort.c:1103-1105,
    /// drvAsynIPServerPort.c:118-123, drvAsynFTDIPort.cpp:610-612). What
    /// makes IEOS work on those ports is `asynInterposeEos`, installed above
    /// the driver by `asynOctetBase::initialize` when `processEosIn` is set
    /// (:170-171) — so the terminator is accepted exactly when a layer takes
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
    /// Default: [`PortDriverBase::drv_user_create`].
    fn drv_user_create(&mut self, req: &DrvUserRequest) -> AsynResult<DrvUserInfo> {
        self.base().drv_user_create(req)
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
/// as `pPrev` when it pushes a layer on top (asynManager.c:2192-2222).
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
            // Both sides of the addr test are getAddr's (-1 on a single-device
            // port), resolved through the rule interrupt clients use.
            let base = self.driver.base();
            base.interrupts.notify_octet(
                OctetFanOut::ByAddr(param_list(base.flags.multi_device, user.addr)),
                InterruptValue {
                    reason: user.reason,
                    addr: user.addr,
                    value: ParamValue::Octet(raw.into()),
                    timestamp: base.get_time_stamp(),
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
/// `asynInterposeDelay` are pushed by the manager (asynManager.c:2192-2222) and
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
/// flushes the layer below (asynInterposeEos.c:257-269), which only happens if
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
    use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;

    #[test]
    fn more_than_one_address_forces_multi_device() {
        for (max_addr, multi) in [(0, false), (1, false), (2, true)] {
            let base = PortDriverBase::new("md", max_addr, PortFlags::default());
            assert_eq!(base.flags.multi_device, multi, "maxAddr {max_addr}");
        }
    }

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
                    seen_cb.lock().unwrap().push(s.to_vec());
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
                    seen_cb.lock().unwrap().push(bytes.to_vec());
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

    /// The `interruptAccept` gate: a `call_param_callbacks` fired before the
    /// IOC accepts interrupts must PRESERVE the changed-flag, not consume it,
    /// so a value seeded at construction still reaches the record on the
    /// one-shot flush the scan facility fires at `iocInit`. C
    /// `asynPortDriver.cpp:838` returns before `flags.clear()` (`:871`) while
    /// `interruptAccept` is false.
    ///
    /// Before this gate existed, the first `call_param_callbacks` — a driver's
    /// own acquisition/array task can fire it before the `_RBV` records
    /// subscribe — cleared the flag, and a read-only seed
    /// (`Manufacturer`, `MaxSizeX`) was lost for the life of the process.
    ///
    /// The gate is the record's own database's, so the test does not touch
    /// any process-wide state.
    #[test]
    fn call_param_callbacks_before_interrupt_accept_preserves_the_seed() {
        use crate::interrupt::{InterruptFilter, InterruptValue};
        use std::sync::{Arc, Mutex};

        // The record's database: its gate is closed until `scan_run`.
        let gate = InterruptGate::new();
        let mut base = PortDriverBase::new("gate", 1, PortFlags::default());
        base.interrupts = crate::interrupt::InterruptManager::owned_by(gate.clone());
        let val = base.create_param("VAL", ParamType::Int32).unwrap();

        let seen: Arc<Mutex<Vec<i32>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let _sub = base.interrupts.register_sync_callback(
            InterruptFilter {
                reason: Some(val),
                addr: Some(0),
                uint32_mask: None,
                iface: None,
            },
            move |iv: &InterruptValue| {
                if let ParamValue::Int32(v) = &iv.value {
                    sink.lock().unwrap().push(*v);
                }
            },
        );

        // Pre-iocInit: interrupts not accepted. The subscriber exists (the
        // record registered), the driver seeds a read-only value, and its own
        // task fires a callback — which must deliver NOTHING and leave the flag
        // pending.
        gate.set(false);
        base.set_int32_param(val, 0, 640).unwrap();
        base.call_param_callbacks(0).unwrap();
        assert!(
            seen.lock().unwrap().is_empty(),
            "a callback fired before interruptAccept must not deliver — got {:?}",
            *seen.lock().unwrap()
        );

        // iocInit accepts interrupts; the flush the scan facility now performs
        // delivers the preserved seed exactly once.
        gate.set(true);
        base.call_param_callbacks(0).unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![640],
            "the seed preserved across the gated call must reach the subscriber \
             once interrupts are accepted"
        );
    }

    struct TestDriver {
        base: PortDriverBase,
    }

    impl TestDriver {
        fn new() -> Self {
            // These tests exercise `call_param_callbacks` delivery with no
            // `iocInit` to open the interruptAccept gate. Establish the
            // precondition a real port has by the time records process — the
            // scan facility has run `scan_run` — which is the direct analogue
            // of C's non-IOC translation unit that unit tests link with
            // `interruptAccept = 1` (`asynPortDriver.cpp:23-25`). The seed-
            // preservation regression test builds its own `PortDriverBase` and
            // drives the gate itself, so it is unaffected.
            let mut base = PortDriverBase::new("test", 1, PortFlags::default());
            base.interrupts = crate::interrupt::InterruptManager::owned_by(InterruptGate::opened());
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
        let mut rx = drv.base_mut().interrupts.subscribe_async(256, 4096);

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
        let mut rx = drv.base_mut().interrupts.subscribe_async(256, 4096);

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
        let mut rx = drv.base_mut().interrupts.subscribe_async(256, 4096);

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
        let mut rx = drv.base_mut().interrupts.subscribe_async(256, 4096);

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
        let mut rx = drv.base_mut().interrupts.subscribe_async(256, 4096);

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
        let mut rx = drv.base_mut().interrupts.subscribe_async(256, 4096);

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
    fn test_callback_carries_the_port_time_stamp() {
        let mut drv = TestDriver::new();
        let mut rx = drv.base_mut().interrupts.subscribe_async(256, 4096);

        let custom_ts = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        drv.base_mut().set_int32_param(0, 0, 77).unwrap();
        drv.base_mut().set_time_stamp(custom_ts);
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

        // Post-`iocInit` precondition — see `TestDriver::new`.
        let mut base = PortDriverBase::new("test_enum_cb", 1, PortFlags::default());
        base.interrupts = crate::interrupt::InterruptManager::owned_by(InterruptGate::opened());
        base.create_param("MODE", ParamType::Enum).unwrap();
        let mut rx = base.interrupts.subscribe_async(256, 4096);

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

    /// `do_callbacks_enum` by parameter type: an `Enum` param keeps the table
    /// and fires it with its value; any other param has the table fired on the
    /// asynEnum interface and keeps its own value.
    #[test]
    fn do_callbacks_enum_by_param_type() {
        use crate::param::{EnumEntry, ParamValue};

        let mut base = PortDriverBase::new("test_cb_enum", 1, PortFlags::default());
        base.interrupts = crate::interrupt::InterruptManager::owned_by(InterruptGate::opened());
        let owner = base.create_param("MODE", ParamType::Enum).unwrap();
        let plain = base.create_param("GAIN", ParamType::Int32).unwrap();
        base.set_int32_param(plain, 0, 7).unwrap();
        base.call_param_callbacks(0).unwrap();
        let mut rx = base.interrupts.subscribe_async(256, 4096);
        let choices: Arc<[EnumEntry]> = Arc::from(vec![EnumEntry {
            string: "A".into(),
            value: 3,
            severity: 0,
        }]);

        base.do_callbacks_enum(owner, 0, choices.clone()).unwrap();
        let v = rx.try_recv().unwrap();
        assert_eq!((v.reason, v.iface), (owner, None));
        assert_eq!(base.get_enum_param(owner, 0).unwrap().1, choices);

        base.do_callbacks_enum(plain, 0, choices.clone()).unwrap();
        let v = rx.try_recv().unwrap();
        assert_eq!((v.reason, v.iface), (plain, Some(InterfaceType::Enum)));
        assert!(matches!(v.value, ParamValue::Enum { choices: c, .. } if c == choices));
        assert_eq!(base.get_int32_param(plain, 0).unwrap(), 7);
        assert!(rx.try_recv().is_err());
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

        // Post-`iocInit` precondition — see `TestDriver::new`.
        let mut base = PortDriverBase::new("test_gp_cb", 1, PortFlags::default());
        base.interrupts = crate::interrupt::InterruptManager::owned_by(InterruptGate::opened());
        base.create_param("PTR", ParamType::GenericPointer).unwrap();
        let mut rx = base.interrupts.subscribe_async(256, 4096);
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let log = seen.clone();
        let _sync = base
            .interrupts
            .register_sync_callback(crate::interrupt::InterruptFilter::default(), move |iv| {
                log.lock().push((iv.reason, iv.value.clone()))
            });

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

        // C: doCallbacksGenericPointer reaches the registered callbacks
        // inline and nothing queues it (asynPortDriver.cpp:3340-3375).
        let seen = seen.lock();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, 0);
        assert!(matches!(seen[0].1, ParamValue::GenericPointer(_)));
        assert!(rx.try_recv().is_err());
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
    /// (:289-297). A port-wide pair could not answer two devices.
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
    /// devices to key by — C's `findDpCommon` (asynManager.c:538-546) and
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
    /// direction down when `processEosIn == 0` — setInputEos :294-296,
    /// getInputEos :319-321; its output pair (:345-364, :366-391) has no
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
        // C's `setOutputEos` (:345-364) has no `processEosOut` test; the flag
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
        // (:300-311, :353-363), so the terminator already stored still stands.
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
        base.device_state(1).put_enabled(false);
        assert!(base.check_ready_addr(0).is_ok());
        let err = base.check_ready_addr(1).unwrap_err();
        assert!(format!("{err}").contains("not enabled"));

        // Disconnect addr 2
        base.device_state(2).put_connected(false);
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

        // C zero-initialises the port's timeStamp: the EPICS epoch.
        let epics_epoch = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(631_152_000);
        assert_eq!(base.get_time_stamp(), epics_epoch);

        let fixed_ts = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(999999);
        base.register_timestamp_source(move || fixed_ts);
        assert_eq!(
            base.get_time_stamp(),
            epics_epoch,
            "registering a source does not update the timestamp"
        );
        assert_eq!(base.update_time_stamp(), fixed_ts);
        assert_eq!(base.get_time_stamp(), fixed_ts);

        base.unregister_timestamp_source();
        let before = SystemTime::now();
        assert!(
            base.update_time_stamp() >= before,
            "the default source is the current time"
        );
    }

    #[test]
    fn test_timestamp_source_in_callbacks() {
        // Post-`iocInit` precondition — see `TestDriver::new`.
        let mut base = PortDriverBase::new("ts_cb", 1, PortFlags::default());
        base.interrupts = crate::interrupt::InterruptManager::owned_by(InterruptGate::opened());
        base.create_param("V", ParamType::Int32).unwrap();
        let mut rx = base.interrupts.subscribe_async(256, 4096);

        // A source that answers differently on every call: a callback must
        // carry the last update's answer, not ask the source itself.
        let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let source_calls = calls.clone();
        base.register_timestamp_source(move || {
            let n = source_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(123456 + n)
        });
        let updated = base.update_time_stamp();

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

        drv.base_mut().set_int32_param(0, 0, 43).unwrap();
        drv.base_mut().call_param_callbacks(0).unwrap();

        assert_eq!(rx.try_recv().unwrap().timestamp, updated);
        assert_eq!(rx.try_recv().unwrap().timestamp, updated);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
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
        // C's `queueRequest` has no defunct branch (asynManager.c:1543-1554),
        // it only sees the `enabled=FALSE` that `shutdownPort` left behind
        // (:2285). "port %s disabled" is the message an operator gets.
        match base.check_ready() {
            Err(AsynError::Status { status, message }) => {
                assert_eq!(status, AsynStatus::Disabled);
                assert_eq!(message, "port p_destr disabled");
            }
            other => panic!("expected the disabled refusal, got {other:?}"),
        }
        // The one place C names the shutdown is `enable` (:2238-2243).
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
        base.set_enabled(false).unwrap();
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

    /// C parity (asynManager.c:2153-2162 exceptionConnect,
    /// :2176-2187 exceptionDisconnect): redundant connect/disconnect
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

    /// C parity: `autoConnectAsyn` (asynManager.c:2312-2329) fires
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
        // base.is_auto_connect() defaults to true — setting true again
        // still must fire (no state-change guard in C).
        base.set_auto_connect(true);
        base.set_auto_connect(false);
        base.set_auto_connect(false);
        assert_eq!(hits.load(Ordering::Relaxed), 3);
    }

    /// `autoConnectAsyn` (asynManager.c:2323-2325): enabling auto-connect on a
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
        base.device_state(1).put_connected(false);
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
        // C autoConnectDevice 2.0s gate (asynManager.c:714-715). Boundary
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
        // :2186); exceptionConnect does not (:2159-2161). Mirror both edges.
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
        // no asynExceptionEnable fan-out (asynManager.c:2238-2243).
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
            !base.device_states.lock().contains_key(&1),
            "refused per-device enable must not create device state"
        );
        // The `()` convenience facade also no-ops on a defunct port.
        base.disable_addr(1);
        assert!(!base.device_states.lock().contains_key(&1));
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
        assert!(base.device_states.lock().contains_key(&1));
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
            !base.device_state(1).enabled(),
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
            drv.read_int32(&AsynUser::new(i32_idx).with_addr(0))
                .as_ref()
                .map_err(AsynError::param_failure),
            Err(AsynError::ParamUndefined(_))
        ));
        assert!(matches!(
            drv.read_int64(&AsynUser::new(i64_idx).with_addr(0))
                .as_ref()
                .map_err(AsynError::param_failure),
            Err(AsynError::ParamUndefined(_))
        ));
        assert!(matches!(
            drv.read_float64(&AsynUser::new(f64_idx).with_addr(0))
                .as_ref()
                .map_err(AsynError::param_failure),
            Err(AsynError::ParamUndefined(_))
        ));
        let mut buf = [0u8; 16];
        assert!(matches!(
            drv.read_octet(&AsynUser::new(oct_idx).with_addr(0), &mut buf)
                .as_ref()
                .map_err(AsynError::param_failure),
            Err(AsynError::ParamUndefined(_))
        ));
        assert!(matches!(
            drv.read_uint32_digital(&AsynUser::new(u32_idx).with_addr(0), 0xFFFF_FFFF)
                .as_ref()
                .map_err(AsynError::param_failure),
            Err(AsynError::ParamUndefined(_))
        ));

        // After a write the same reads succeed with the stored value.
        drv.base_mut().params.set_int32(i32_idx, 0, 7).unwrap();
        drv.base_mut().params.set_int64(i64_idx, 0, 9).unwrap();
        drv.base_mut().params.set_float64(f64_idx, 0, 1.5).unwrap();
        drv.base_mut().params.set_string(oct_idx, 0, b"hi").unwrap();
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

    /// The default read/write paths of a plain asynPortDriver, one param per
    /// type, for the errorMessage tests below.
    struct ErrmsgDrv {
        base: PortDriverBase,
    }
    impl PortDriver for ErrmsgDrv {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
    }
    const I32: usize = 0;
    const I64: usize = 1;
    const F64: usize = 2;
    const OCT: usize = 3;
    const BITS: usize = 4;

    fn errmsg_drv() -> ErrmsgDrv {
        let flags = PortFlags {
            multi_device: true,
            ..PortFlags::default()
        };
        let mut base = PortDriverBase::new("errmsg", 1, flags);
        for (name, t) in [
            ("I32", ParamType::Int32),
            ("I64", ParamType::Int64),
            ("F64", ParamType::Float64),
            ("SYSTEM_INFO", ParamType::Octet),
            ("BITS", ParamType::UInt32Digital),
        ] {
            base.params.create_param(name, t).unwrap();
        }
        ErrmsgDrv { base }
    }

    fn at(reason: usize) -> AsynUser {
        AsynUser::new(reason).with_addr(0)
    }

    fn text(e: AsynError) -> String {
        e.message_display().to_string()
    }

    // A failed default read/write carries C asynPortDriver's errorMessage,
    // which the record trace prints (asynPortDriver.cpp:1983, :2034).
    #[test]
    fn int32_param_errors_carry_the_c_error_message() {
        let mut drv = errmsg_drv();
        assert_eq!(
            text(drv.read_int32(&at(I32)).unwrap_err()),
            "asynPortDriver:readInt32: status=10, function=0, name=I32, value=0"
        );
        // A bad reason fails in parseAsynUser, before any errorMessage.
        assert!(matches!(
            drv.read_int32(&at(99)),
            Err(AsynError::ParamIndexOutOfRange(99))
        ));
    }

    // asynPortDriver.cpp:2122, :2173.
    #[test]
    fn int64_param_errors_carry_the_c_error_message() {
        let mut drv = errmsg_drv();
        assert_eq!(
            text(drv.read_int64(&at(I64)).unwrap_err()),
            "asynPortDriver:readInt64: status=10, function=1, name=I64, value=0"
        );
    }

    // asynPortDriver.cpp:2262 (no comma before mask), :2314.
    #[test]
    fn uint32_digital_param_errors_carry_the_c_error_message() {
        let mut drv = errmsg_drv();
        assert_eq!(
            text(drv.read_uint32_digital(&at(BITS), 0xF0).unwrap_err()),
            "asynPortDriver:readUInt32Digital: status=10, function=4, name=BITS, value=0 mask=240"
        );
    }

    // asynPortDriver.cpp:2506 names an undefined value, :2511 prints any
    // other; writeFloat64 writes no errorMessage (:2561).
    #[test]
    fn float64_param_errors_carry_the_c_error_message() {
        let mut drv = errmsg_drv();
        assert_eq!(
            text(drv.read_float64(&at(F64)).unwrap_err()),
            "asynPortDriver:readFloat64: status=10, function=2, name=F64, value is undefined"
        );
        assert_eq!(
            text(drv.read_float64(&at(I32)).unwrap_err()),
            "asynPortDriver:readFloat64: status=8, function=0, name=I32, value=0.000000"
        );
        assert!(matches!(
            drv.write_float64(&mut at(I32), 1.5),
            Err(AsynError::TypeMismatch { .. })
        ));
    }

    // asynPortDriver.cpp:2621 (the pixirad SystemInfo read), :2675.
    #[test]
    fn octet_param_errors_carry_the_c_error_message() {
        let mut drv = errmsg_drv();
        assert_eq!(
            text(drv.read_octet(&at(OCT), &mut [0u8; 16]).unwrap_err()),
            "asynPortDriver:readOctet: status=10, function=3, name=SYSTEM_INFO, value="
        );
    }

    // C's write* status is callParamCallbacks', not set*Param's
    // (asynPortDriver.cpp:2028-2031, :2167-2170, :2308-2311, :2672-2675): a
    // value the store refuses is dropped and the write succeeds. A bad
    // address or reason still fails in parseAsynUser.
    fn assert_c_write_outcome(write: impl Fn(&mut ErrmsgDrv, &mut AsynUser) -> AsynResult<()>) {
        let mut drv = errmsg_drv();
        write(&mut drv, &mut at(F64)).unwrap();
        assert!(
            drv.base()
                .params
                .get_float64(F64, 0)
                .is_err_and(|e| matches!(e, AsynError::ParamUndefined(_))),
            "the refused value was not stored"
        );
        assert!(matches!(
            write(&mut drv, &mut at(99)),
            Err(AsynError::ParamIndexOutOfRange(99))
        ));
        assert!(matches!(
            write(&mut drv, &mut AsynUser::new(F64).with_addr(1)),
            Err(AsynError::AddressOutOfRange(1))
        ));
    }

    #[test]
    fn int32_write_of_a_refused_value_succeeds_as_in_c() {
        assert_c_write_outcome(|d, u| d.write_int32(u, 5));
    }

    #[test]
    fn int64_write_of_a_refused_value_succeeds_as_in_c() {
        assert_c_write_outcome(|d, u| d.write_int64(u, -3));
    }

    #[test]
    fn uint32_digital_write_of_a_refused_value_succeeds_as_in_c() {
        assert_c_write_outcome(|d, u| d.write_uint32_digital(u, 5, 0xF));
    }

    #[test]
    fn octet_write_of_a_refused_value_succeeds_as_in_c() {
        assert_c_write_outcome(|d, u| d.write_octet(u, b"abc").map(|_| ()));
    }

    // writeFloat64 returns setDoubleParam's status (:2554-2567).
    #[test]
    fn float64_write_of_a_refused_value_fails_as_in_c() {
        let mut drv = errmsg_drv();
        assert!(matches!(
            drv.write_float64(&mut at(I32), 1.5),
            Err(AsynError::TypeMismatch { .. })
        ));
        assert!(matches!(
            drv.write_float64(&mut at(99), 1.5),
            Err(AsynError::ParamIndexOutOfRange(99))
        ));
    }

    // The writeArray template default (asynPortDriver.cpp:1824): a port that
    // does not override writeXxxArray refuses the put and stores nothing.
    #[test]
    fn array_writes_carry_the_c_error_message() {
        let mut drv = errmsg_drv();
        let arr = drv
            .base
            .params
            .create_param("ARR", ParamType::Float64Array)
            .unwrap();
        for e in [
            drv.write_float64_array(&at(arr), &[1.0]).unwrap_err(),
            drv.write_int32_array(&at(arr), &[1]).unwrap_err(),
            drv.write_int8_array(&at(arr), &[1]).unwrap_err(),
            drv.write_int16_array(&at(arr), &[1]).unwrap_err(),
            drv.write_int64_array(&at(arr), &[1]).unwrap_err(),
            drv.write_float32_array(&at(arr), &[1.0]).unwrap_err(),
        ] {
            assert_eq!(e.status(), AsynStatus::Error);
            assert_eq!(text(e), "asynPortDriver:writeArray not implemented");
        }
        assert!(
            drv.base()
                .params
                .get_float64_array(arr, 0)
                .unwrap()
                .is_empty()
        );
    }

    // The readArray template default (asynPortDriver.cpp:1816).
    #[test]
    fn array_reads_carry_the_c_error_message() {
        let mut drv = errmsg_drv();
        for e in [
            drv.read_float64_array(&at(F64), &mut [0f64; 2])
                .unwrap_err(),
            drv.read_int32_array(&at(I32), &mut [0i32; 2]).unwrap_err(),
            drv.read_int8_array(&at(I32), &mut [0i8; 2]).unwrap_err(),
            drv.read_int16_array(&at(I32), &mut [0i16; 2]).unwrap_err(),
            drv.read_int64_array(&at(I32), &mut [0i64; 2]).unwrap_err(),
            drv.read_float32_array(&at(I32), &mut [0f32; 2])
                .unwrap_err(),
        ] {
            assert_eq!(e.status(), AsynStatus::Error);
            assert_eq!(text(e), "asynPortDriver:readArray not implemented");
        }
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
        base.params.set_string(s_idx, 0, b"hello").unwrap();
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

    /// `trace_print` formats nothing while its mask is off, and with it on
    /// writes the one line.
    #[test]
    fn trace_print_formats_only_when_the_mask_is_on() {
        use crate::trace::{TraceFile, TraceMask};
        use std::sync::atomic::AtomicUsize;

        struct Counted<'a>(&'a AtomicUsize);
        impl std::fmt::Display for Counted<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fetch_add(1, Ordering::Relaxed);
                f.write_str("why")
            }
        }

        let services = crate::services::PortServices::new();
        let mut base = PortDriverBase::new("tprint", 1, PortFlags::default());
        services.bind(&mut base);
        let trace = services.trace();
        trace.register_port("tprint", false);
        let dir = tempfile::tempdir().expect("fixture root");
        let path = dir.path().join("tprint.log");
        let file = std::fs::File::create(&path).unwrap();
        trace.set_trace_file(
            Some("tprint"),
            TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        let n = AtomicUsize::new(0);

        trace.set_trace_mask(Some("tprint"), TraceMask::empty());
        base.trace_print(TraceMask::ERROR, format_args!("failed: {}", Counted(&n)));
        assert_eq!(n.load(Ordering::Relaxed), 0, "formatted with ERROR off");

        trace.set_trace_mask(Some("tprint"), TraceMask::ERROR);
        base.trace_print(TraceMask::ERROR, format_args!("failed: {}", Counted(&n)));
        assert_eq!(n.load(Ordering::Relaxed), 1);
        let log = std::fs::read_to_string(&path).unwrap();
        assert_eq!(log.matches("failed: why\n").count(), 1, "{log:?}");
    }
}
