//! The motor record's `device()` lines, and the axes their DTYPs reach.
//!
//! A row is a static dbd line, there whether or not any controller was
//! created. The axis is found at `init_record`, from OUT: `devMotorAsyn` by
//! `@asyn(PORT,ADDR)` through asynManager (devMotorAsyn.c:276-291), a model-1
//! dset by `#C card S signal` through its driver's card table
//! (motordevCom.cc:217-258), devMotorSim by `#C card S signal @drvet`
//! through the `motor_interface.h` driver its parm names (devMotorSim.c:
//! 131-200). A record whose axis is missing still loads and fails at init
//! as those do.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::device_support::{DeviceInitOutcome, DeviceSupport};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::record::{
    DbLinkType, DevSup, Dset, HwLink, ParsedLink, Record, ScanType, parse_link_v2,
};

use asyn_rs::port_handle::PortHandle;
use epics_base_rs::runtime::interrupt_accept::InterruptGate;

use crate::controller_port::{ControllerParams, WriteHook};
use crate::device_support::{AxisSlot, MotorAxis, MotorDeviceSupport};
use crate::poll_loop::MissingAxis;
use crate::poll_loop::SharedControllerPoll;
use crate::trace::{MotorTrace, TraceUser};

/// `device(motor,INST_IO,devMotorAsyn,"asynMotor")` (motorSupport.dbd:8).
pub static DEV_MOTOR_ASYN: DevSup = DevSup {
    name: "devMotorAsyn",
    choice: "asynMotor",
    link_type: DbLinkType::InstIo,
    dset: MOTOR_DSET,
};

/// `struct motor_dset`: 8 slots, `report` and `get_ioint_info` NULL in
/// `devMotorAsyn` (devMotorAsyn.c:83-95) and in every model-1 dset.
const MOTOR_DSET: Dset = Dset::new(8, &[2, 4, 5, 6, 7], false).with_init(motor_init);

/// devMotorAsyn's `init(after)`, `dbScanLockOK = (after!=0)`
/// (devMotorAsyn.c:160-164): the latch is per database here, the
/// `ScanControl::dev_sup_gate` the build's `finish_dev_sup` opens.
fn motor_init(_after: bool) {}

/// The rows [`device_support_factory`] serves through
/// `register_dynamic_device_support`; model-1 rows are added by their
/// drivers through [`register_model1_device`].
pub fn devices() -> [(&'static str, &'static [DevSup]); 1] {
    [("motor", std::slice::from_ref(&DEV_MOTOR_ASYN))]
}

/// A model-1 driver's `device(motor,VME_IO,<dset>,"<choice>")` line, e.g.
/// `model1_device("devSC800", "SC-800")` (devKohzuMotor.dbd:4).
pub const fn model1_device(dset: &'static str, choice: &'static str) -> DevSup {
    DevSup {
        name: dset,
        choice,
        link_type: DbLinkType::VmeIo,
        dset: MOTOR_DSET,
    }
}

/// Add a model-1 driver's row to the motor device menu and let
/// [`device_support_factory`] serve its DTYP. Called where the driver
/// registers, not where a card is configured: the row is the dbd's.
pub fn register_model1_device(row: &'static DevSup) {
    register_model1(row, false);
}

/// [`register_model1_device`] for a dset whose init_record sets the
/// record's NTM to NO before `motor_init_record_com`, so whether or not its
/// card or signal exists (devPIE816.cc:126-131, devEMC18011.cc:127-129,
/// devMDT695.cc:129).
pub fn register_model1_device_disabling_ntm(row: &'static DevSup) {
    register_model1(row, true);
}

fn register_model1(row: &'static DevSup, disables_ntm: bool) {
    epics_base_rs::server::record::register_device_row("motor", row);
    lock(model1_choices()).insert(row.choice, disables_ntm);
}

/// devMotorSim's `device(motor,VME_IO,devMotorSim,"Motor Simulation")`
/// (motorSimSupport.dbd:1): records on it bind a `motor_interface.h` driver.
pub const fn motor_interface_device(dset: &'static str, choice: &'static str) -> DevSup {
    DevSup {
        name: dset,
        choice,
        link_type: DbLinkType::VmeIo,
        dset: MOTOR_DSET,
    }
}

/// Add devMotorSim's row to the motor device menu and let
/// [`device_support_factory`] serve its DTYP.
pub fn register_motor_interface_device(row: &'static DevSup) {
    epics_base_rs::server::record::register_device_row("motor", row);
    lock(interface_choices()).insert(row.choice);
}

/// A drvet's `open(card, signal)`: register the axis there as a
/// [`TraceUser::MotorInterface`] ([`register_axis`]), installing it into the
/// driver's poller, and say whether the driver has it.
type Open = Arc<dyn Fn(i32, i32) -> bool + Send + Sync>;

/// C `driver(<name>)` for a `motor_interface.h` drvet, which a devMotorSim
/// record finds by its OUT parm (`registryDriverSupportFind`,
/// devMotorSim.c:135). The first record to name an axis calls `open`, as
/// devMotorSim's init_record calls the drvet's (devMotorSim.c:143).
pub fn register_motor_interface_driver(
    name: &'static str,
    open: impl Fn(i32, i32) -> bool + Send + Sync + 'static,
) {
    lock(interface_drivers()).insert(name, Arc::new(open));
}

/// The controller an axis belongs to: an asyn port, or one card of a model-1
/// driver's table.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Controller {
    Port(String),
    Card {
        device: &'static str,
        card: i32,
    },
    /// One card of a `motor_interface.h` driver, by its `drvet` name.
    Driver {
        driver: &'static str,
        card: i32,
    },
}

/// A controller's axes, each with whether a record has bound it.
type Axes = HashMap<i32, (MotorAxis, bool)>;

fn controllers() -> &'static Mutex<HashMap<Controller, Axes>> {
    static CONTROLLERS: OnceLock<Mutex<HashMap<Controller, Axes>>> = OnceLock::new();
    CONTROLLERS.get_or_init(Default::default)
}

/// The driver behind an asyn motor port, which decides how it answers for
/// an axis it does not have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PortDriver {
    /// `asynMotorController` (model 3).
    Controller,
    /// `drvMotorAsyn.c` over a `motorAxisDrvSET_t` (model 2).
    DrvMotorAsyn,
}

/// An asyn motor port: its driver and `numAxes`, and a model-3 port's
/// `pAxes_`, one slot per address.
struct Port {
    driver: PortDriver,
    num_axes: i32,
    axes: Vec<Arc<AxisSlot>>,
}

impl Port {
    /// Whether the port takes an axis at `user`.
    fn admit(&self, port: &str, user: &TraceUser) -> Result<(), String> {
        let (driver, (_, addr)) = (self.driver, split(user));
        if (driver == PortDriver::DrvMotorAsyn) != matches!(user, TraceUser::Model2 { .. }) {
            return Err(format!("motor port {port} has a {driver:?} driver"));
        }
        if !(0..self.num_axes).contains(&addr) {
            return Err(format!("motor port {port} has no axis {addr}"));
        }
        Ok(())
    }

    /// The slot at `addr`; `None` off a model-3 port's addresses.
    fn slot(&self, addr: i32) -> Option<Arc<AxisSlot>> {
        self.axes.get(usize::try_from(addr).ok()?).cloned()
    }
}

/// Asyn motor ports by name.
fn ports() -> &'static Mutex<HashMap<String, Port>> {
    static PORTS: OnceLock<Mutex<HashMap<String, Port>>> = OnceLock::new();
    PORTS.get_or_init(Default::default)
}

fn register_port(port: &str, driver: PortDriver, num_axes: i32) -> Result<(), String> {
    let mut ports = lock(ports());
    if ports.contains_key(port) {
        return Err(format!("motor port {port} is already registered"));
    }
    let axes = match driver {
        PortDriver::Controller => (0..num_axes).map(|_| Arc::default()).collect(),
        PortDriver::DrvMotorAsyn => Vec::new(),
    };
    let entry = Port {
        driver,
        num_axes,
        axes,
    };
    ports.insert(port.to_owned(), entry);
    Ok(())
}

/// Create `asynMotorController` port `port` with axes `0..num_axes`: the
/// asyn port its records reach, with C's base parameters and the driver's
/// `params`, gated on `owner` (the IOC's interruptAccept). The create
/// command calls this before [`register_axis`] for each axis it creates; a
/// second port of the same name is refused. The handle is the driver's, to
/// post the values its poll reads.
pub fn register_controller_port<P: ControllerParams>(
    owner: &Arc<InterruptGate>,
    port: &str,
    num_axes: i32,
    params: P,
) -> Result<PortHandle, String> {
    if lock(ports()).contains_key(port) {
        return Err(format!("motor port {port} is already registered"));
    }
    let published = crate::controller_port::publish(owner, port, num_axes, params)?;
    register_port(port, PortDriver::Controller, num_axes)?;
    let handle = published.handle.clone();
    lock(controller_ports()).insert(port.to_owned(), published);
    Ok(handle)
}

/// What a controller port's axes' pollers need of it for the base calls
/// they run.
fn controller_ports() -> &'static Mutex<HashMap<String, crate::controller_port::Published>> {
    static PORTS: OnceLock<Mutex<HashMap<String, crate::controller_port::Published>>> =
        OnceLock::new();
    PORTS.get_or_init(Default::default)
}

/// Controller port `port`'s [`WriteHook`].
pub(crate) fn write_hook(port: &str) -> Option<Arc<dyn WriteHook>> {
    lock(controller_ports()).get(port)?.hook.clone()
}

/// Controller port `port`'s parameter library, as its poller reads it.
pub(crate) fn controller_params(port: &str) -> Option<crate::controller_port::ParamRead> {
    lock(controller_ports())
        .get(port)
        .map(|entry| entry.params.clone())
}

/// C `asynMotorController` is its port: the poller of controller port
/// `port`, found by name as `findAsynPortDriver` finds the controller.
fn controller_polls() -> &'static Mutex<HashMap<String, SharedControllerPoll>> {
    static POLLS: OnceLock<Mutex<HashMap<String, SharedControllerPoll>>> = OnceLock::new();
    POLLS.get_or_init(Default::default)
}

/// Name `poll` controller port `port`'s poller; a port has one. The port
/// may be registered later (simMotorCreate names its poller first), so the
/// name holds until then and [`controller_poll`] gates on the port.
pub(crate) fn link_controller_poll(port: &str, poll: &SharedControllerPoll) {
    lock(controller_polls())
        .entry(port.to_owned())
        .or_insert_with(|| poll.clone());
}

/// Controller port `port`'s poller: `None` when there is no such port,
/// `Some(None)` before its poller is named.
pub(crate) fn controller_poll(port: &str) -> Option<Option<SharedControllerPoll>> {
    lock(controller_ports())
        .contains_key(port)
        .then(|| lock(controller_polls()).get(port).cloned())
}

/// [`crate::builder::ControllerAxes::first_polls_pending`] of controller
/// port `port`'s poller, 0 before one is named; `None` when there is no
/// such port.
pub fn first_polls_pending(port: &str) -> Option<usize> {
    controller_poll(port).map(|poll| poll.map_or(0, |poll| poll.first_polls_pending()))
}

/// Controller port `port` itself.
pub(crate) fn controller_port_handle(port: &str) -> Option<PortHandle> {
    lock(controller_ports())
        .get(port)
        .map(|entry| entry.handle.clone())
}

/// Make `drvMotorAsyn` port `port` reachable from a record's OUT, with axes
/// `0..num_axes`; see [`register_controller_port`]. `handle` is the port
/// itself, which a record's writes go through.
pub fn register_drv_motor_asyn_port(
    port: &str,
    num_axes: i32,
    handle: PortHandle,
) -> Result<(), String> {
    register_port(port, PortDriver::DrvMotorAsyn, num_axes)?;
    lock(drv_motor_asyn_ports()).insert(port.to_owned(), handle);
    Ok(())
}

fn drv_motor_asyn_ports() -> &'static Mutex<HashMap<String, PortHandle>> {
    static PORTS: OnceLock<Mutex<HashMap<String, PortHandle>>> = OnceLock::new();
    PORTS.get_or_init(Default::default)
}

/// `drvMotorAsyn` port `port` itself.
pub(crate) fn drv_motor_asyn_port_handle(port: &str) -> Option<PortHandle> {
    lock(drv_motor_asyn_ports()).get(port).cloned()
}

/// The axis registered at `addr` of asyn port `port`.
pub(crate) fn axis_at(port: &str, addr: i32) -> Option<MotorAxis> {
    let controllers = lock(controllers());
    let (axis, _) = controllers
        .get(&Controller::Port(port.to_owned()))?
        .get(&addr)?;
    Some(axis.clone())
}

/// Every axis registered on asyn port `port`.
pub(crate) fn axes_of(port: &str) -> Vec<MotorAxis> {
    lock(controllers())
        .get(&Controller::Port(port.to_owned()))
        .map(|axes| axes.values().map(|(axis, _)| axis.clone()).collect())
        .unwrap_or_default()
}

/// The parameter library of model-2 axis `addr` of `drvMotorAsyn` port
/// `port`, which the driver's report dumps (drvMotorSim.c:186).
pub fn model2_params(port: &str, addr: i32) -> Option<crate::device_state::Model2Params> {
    let axis = axis_at(port, addr)?;
    matches!(axis.address(), TraceUser::Model2 { .. }).then(|| axis.params().clone())
}

/// The parameter library of axis `signal` of card `card` of
/// `motor_interface.h` driver `driver`, which its report dumps by card and
/// axis (`drv.pFirst`, drvMotorSim.c:178-190) whether or not a `drvMotorAsyn`
/// port opened the axis too.
pub fn motor_interface_params(
    driver: &str,
    card: i32,
    signal: i32,
) -> Option<crate::device_state::Model2Params> {
    let controllers = lock(controllers());
    controllers
        .iter()
        .find_map(|(controller, axes)| match controller {
            Controller::Driver { driver: d, card: c } if *d == driver && *c == card => {
                axes.get(&signal).map(|(axis, _)| axis.params().clone())
            }
            _ => None,
        })
}

fn interface_choices() -> &'static Mutex<HashSet<&'static str>> {
    static CHOICES: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    CHOICES.get_or_init(Default::default)
}

fn interface_drivers() -> &'static Mutex<HashMap<&'static str, Open>> {
    static DRIVERS: OnceLock<Mutex<HashMap<&'static str, Open>>> = OnceLock::new();
    DRIVERS.get_or_init(Default::default)
}

/// Each model-1 DTYP and whether its init_record disables NTM.
fn model1_choices() -> &'static Mutex<HashMap<&'static str, bool>> {
    static CHOICES: OnceLock<Mutex<HashMap<&'static str, bool>>> = OnceLock::new();
    CHOICES.get_or_init(Default::default)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn split(user: &TraceUser) -> (Controller, i32) {
    match user {
        TraceUser::Asyn { port, addr } | TraceUser::Model2 { port, addr } => {
            (Controller::Port(port.clone()), *addr)
        }
        TraceUser::Model1 {
            device,
            card,
            signal,
        } => (
            Controller::Card {
                device,
                card: *card,
            },
            *signal,
        ),
        TraceUser::MotorInterface {
            driver,
            card,
            signal,
            ..
        } => (
            Controller::Driver {
                driver,
                card: *card,
            },
            *signal,
        ),
    }
}

/// Make `axis` reachable from a record's OUT at its address
/// ([`MotorAxis::address`]). The create command calls this; a
/// second axis at the same address replaces the first, as C's constructor
/// sets `pC->pAxes_[axisNo]` (asynMotorAxis.cpp:46), and an asyn axis needs
/// its port registered with that driver and the address in range.
pub fn register_axis(axis: MotorAxis) -> Result<(), String> {
    let (controller, addr) = split(axis.address());
    if let Controller::Port(port) = &controller {
        match lock(ports()).get(port) {
            None => return Err(format!("motor port {port} is not registered")),
            Some(entry) => entry.admit(port, axis.address())?,
        }
    }
    lock(controllers())
        .entry(controller.clone())
        .or_default()
        .insert(addr, (axis, false));
    if let Controller::Port(port) = &controller {
        if let Some(handle) = controller_port_handle(port) {
            init_axis_params(&handle, addr);
        }
    }
    Ok(())
}

/// The slot `user`'s axis takes on its port, C's `pAxes_[axisNo]`, or `None`
/// off a model-3 port. A registered port's refusal comes here, before the
/// axis joins a poller, as C's constructor returns before it sets `pAxes_`
/// (asynMotorAxis.cpp:35-44); a port not registered yet, a harness's or a
/// drvMotorAsyn port its axes wait for, is [`register_axis`]'s to check.
pub(crate) fn claim(user: &TraceUser) -> Result<Option<Arc<AxisSlot>>, String> {
    let (Controller::Port(port), addr) = split(user) else {
        return Ok(None);
    };
    let ports = lock(ports());
    let Some(entry) = ports.get(&port) else {
        return Ok(None);
    };
    entry.admit(&port, user)?;
    Ok(entry.slot(addr))
}

/// The parameters C's `asynMotorAxis` constructor sets on its axis address
/// (asynMotorAxis.cpp:65-67); its `setIntegerParam` also sets MOTOR_STATUS
/// to 0 (:267).
fn init_axis_params(handle: &PortHandle, addr: i32) {
    use crate::controller_port::index;
    use asyn_rs::param::ParamValue;
    use asyn_rs::request::ParamSetValue;
    let updates = vec![
        ParamSetValue::new(index::POWER_AUTO_ONOFF, addr, ParamValue::Int32(0)),
        ParamSetValue::new(index::STATUS, addr, ParamValue::Int32(0)),
        ParamSetValue::new(index::POWER_OFF_DELAY, addr, ParamValue::Float64(0.0)),
        ParamSetValue::new(index::POWER_ON_DELAY, addr, ParamValue::Float64(0.0)),
    ];
    // C ignores the status of these sets.
    let _ = handle.set_params_and_notify_blocking(addr, updates);
}

/// The motor DTYPs' device support: `asynMotor` and every registered model-1
/// choice, each bound to the axis its record's OUT names.
pub fn device_support_factory()
-> impl Fn(&DeviceSupportContext) -> Option<Box<dyn DeviceSupport>> + Send + Sync + 'static {
    |ctx: &DeviceSupportContext| {
        if ctx.record_type != "motor" {
            return None;
        }
        let interface = lock(interface_choices()).get(ctx.dtyp).copied();
        let (dtyp, disables_ntm, bound) = if ctx.dtyp == DEV_MOTOR_ASYN.choice {
            (DEV_MOTOR_ASYN.choice, false, bind_asyn(ctx.out))
        } else if let Some(device) = interface {
            (device, false, bind_motor_interface(ctx.out))
        } else {
            let (device, disables_ntm) = lock(model1_choices())
                .get_key_value(ctx.dtyp)
                .map(|(&device, &disables_ntm)| (device, disables_ntm))?;
            (device, disables_ntm, bind_model1(device, ctx.out))
        };
        Some(match bound {
            Ok(axis) => Box::new(axis.disabling_ntm(disables_ntm)) as Box<dyn DeviceSupport>,
            Err(failure) => Box::new(Unbound {
                dtyp,
                disables_ntm,
                failure,
                record: String::new(),
            }),
        })
    }
}

/// Why a record reached no axis, and so how its init fails.
#[derive(Debug)]
enum Failure {
    /// `parseLink` refused OUT; the text is its `errorMessage`.
    BadLink(&'static str),
    /// `connectDevice`: no such port.
    NoPort(String),
    /// An asyn port with no motor axes, which `connectDevice` reaches.
    NotMotor {
        port: String,
        addr: i32,
        user_param: Option<String>,
    },
    /// `asynPortDriver::getAddress` refuses an address past `numAxes`, so
    /// the first `findDrvInfo` fails (asynPortDriver.cpp:1901-1915, 3569).
    BadAddress {
        port: String,
        addr: i32,
        num_axes: i32,
    },
    /// `motor_init_record_com`'s refusal of a non-VME_IO OUT; it keeps
    /// the record of every other refusal.
    BadBus,
    /// devMotorSim's three refusals: OUT not VME_IO, no drvet by the parm's
    /// name, and the driver's `open` returning NULL.
    IllegalInp,
    NoDrvet,
    NoOpen,
}

/// Bind a record to the axis. An asyn axis serves every record that names
/// it; a model-1 signal serves the first, and each later record on it is
/// bound refused as in use (motordevCom.cc:244-247).
fn take(controller: Controller, addr: i32) -> Result<MotorDeviceSupport, Taken> {
    let mut controllers = lock(controllers());
    let axes = controllers
        .get_mut(&controller)
        .ok_or(Taken::NoController)?;
    let (axis, bound) = axes.get_mut(&addr).ok_or(Taken::NoAxis)?;
    if *bound && matches!(controller, Controller::Card { .. }) {
        return Ok(axis.attach_in_use());
    }
    *bound = true;
    Ok(axis.attach())
}

enum Taken {
    NoController,
    NoAxis,
}

/// devMotorAsyn's binding: a live port reached at an axis it lacks still
/// serves the record, with the driver refusing every request.
fn bind_asyn(out: &str) -> Result<MotorDeviceSupport, Failure> {
    let (port, addr, user_param) = parse_link(out).map_err(Failure::BadLink)?;
    let entry = lock(ports())
        .get(&port)
        .map(|p| (p.driver, p.num_axes, p.slot(addr)));
    let Some((driver, num_axes, slot)) = entry else {
        if asyn_rs::registry::get_port(&port).is_some() {
            return Err(Failure::NotMotor {
                port,
                addr,
                user_param,
            });
        }
        return Err(Failure::NoPort(port));
    };
    let in_range = (0..num_axes).contains(&addr);
    match (driver, slot) {
        // An address past numAxes has no slot.
        (PortDriver::Controller, None) => Err(Failure::BadAddress {
            port,
            addr,
            num_axes,
        }),
        // The record reaches whatever axis holds the address when it sends,
        // none yet included (asynMotorController.cpp:532-537).
        (PortDriver::Controller, Some(slot)) => {
            let user = TraceUser::Asyn {
                port: port.clone(),
                addr,
            };
            take(Controller::Port(port), addr)
                .or_else(|_| Ok(slot.attach(user, std::time::Duration::ZERO, None)))
        }
        (PortDriver::DrvMotorAsyn, _) => {
            let user = TraceUser::Model2 {
                port: port.clone(),
                addr,
            };
            let missing = MissingAxis::DrvMotorAsyn { invalid: !in_range };
            take(Controller::Port(port), addr).or_else(|_| {
                Ok(MotorDeviceSupport::missing(
                    Arc::new(MotorTrace::with_user(user)),
                    missing,
                ))
            })
        }
    }
}

fn bind_model1(device: &'static str, out: &str) -> Result<MotorDeviceSupport, Failure> {
    let ParsedLink::Hw(HwLink::VmeIo { card, signal, parm }) = parse_link_v2(out) else {
        return Err(Failure::BadBus);
    };
    let (card, signal) = (i32::from(card), i32::from(signal));
    let support = match take(Controller::Card { device, card }, signal) {
        Ok(support) => support,
        Err(Taken::NoController) => return Ok(MotorDeviceSupport::no_card(device, card, signal)),
        Err(Taken::NoAxis) => return Ok(MotorDeviceSupport::no_signal(device, card, signal)),
    };
    support.send_out_parm(parm);
    Ok(support)
}

/// devMotorSim's init_record (devMotorSim.c:131-200): the drvet the parm
/// names, then its `open(card, signal)`. Any number of records may open one
/// axis; the first one's open registers it.
fn bind_motor_interface(out: &str) -> Result<MotorDeviceSupport, Failure> {
    let ParsedLink::Hw(HwLink::VmeIo { card, signal, parm }) = parse_link_v2(out) else {
        return Err(Failure::IllegalInp);
    };
    let Some((driver, open)) = lock(interface_drivers())
        .get_key_value(parm.as_str())
        .map(|(name, open)| (*name, open.clone()))
    else {
        return Err(Failure::NoDrvet);
    };
    let (card, signal) = (i32::from(card), i32::from(signal));
    let controller = Controller::Driver { driver, card };
    take(controller.clone(), signal)
        .or_else(|_| {
            if open(card, signal) {
                take(controller, signal)
            } else {
                Err(Taken::NoAxis)
            }
        })
        .map_err(|_| Failure::NoOpen)
}

/// `pasynEpicsUtils->parseLink` (asynEpicsUtils.c:56-148): the port,
/// address and userParam, or the `errorMessage` it writes.
fn parse_link(out: &str) -> Result<(String, i32, Option<String>), &'static str> {
    const INVALID: &str = "invalid INST_IO Must be asyn(<port> <addr> <timeout>)userParams";
    match parse_link_v2(out) {
        ParsedLink::Hw(HwLink::VmeIo { signal, parm, .. }) => {
            use epics_base_rs::runtime::stdlib::c_isspace;
            // C defect not copied: asynEpicsUtils.c:82 copies the name from
            // the parm's start, so a leading blank makes `@ M1` port ` M`.
            let parm = parm.trim_start_matches(c_isspace);
            let len = parm
                .find(|c| c_isspace(c) || c == ',')
                .unwrap_or(parm.len());
            if len == 0 {
                return Err("parm is null. Must be <port> <addr> ...");
            }
            let user_param = parm[len..].trim_start_matches(c_isspace);
            Ok((
                parm[..len].to_owned(),
                i32::from(signal),
                (!user_param.is_empty()).then(|| user_param.to_owned()),
            ))
        }
        // asyn's own scanner, with C's strtol/strtod and their ERANGE.
        ParsedLink::Hw(HwLink::InstIo { .. }) => asyn_rs::adapter::parse_asyn_link(out)
            .map(|link| {
                let user_param = (!link.drv_info.is_empty()).then_some(link.drv_info);
                (link.port_name, link.addr, user_param)
            })
            .map_err(|_| INVALID),
        _ => Err("Link must be INST_IO or VME_IO"),
    }
}

/// A motor record whose OUT reached no axis. Its init prints what C's does
/// and fails the same way.
struct Unbound {
    dtyp: &'static str,
    disables_ntm: bool,
    failure: Failure,
    record: String,
}

impl DeviceSupport for Unbound {
    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        use asyn_rs::manager::PortManager;
        use asyn_rs::trace::TraceMask;
        use epics_base_rs::server::recgbl::rec_gbl_record_error;

        // The dset sets NTM before motor_init_record_com refuses the bus.
        if self.disables_ntm
            && let Some(motor) = record
                .as_any_mut()
                .and_then(|a| a.downcast_mut::<crate::record::MotorRecord>())
        {
            motor.timing.ntm = false;
        }
        let name = &self.record;
        match &self.failure {
            Failure::BadLink(why) => {
                epics_base_rs::runtime::log::errlog_vprintf(format_args!(
                    "devMotorAsyn::init_record {name} bad link {why}\n"
                ));
                unqueued_dial_limits(name);
                Ok(bad(record))
            }
            Failure::NoPort(port) => {
                // An unconnected asynUser prints on the global trace.
                PortManager::global().trace_manager().print(
                    None,
                    None,
                    0,
                    TraceMask::ERROR,
                    format_args!(
                        "devMotorAsyn::init_record, {name} connectDevice failed to {port}\n"
                    ),
                );
                unqueued_dial_limits(name);
                Ok(bad(record))
            }
            Failure::NotMotor {
                port,
                addr,
                user_param,
            } => {
                refuse_foreign_port(name, port, *addr, user_param.as_deref());
                Ok(bad(record))
            }
            Failure::BadAddress {
                port,
                addr,
                num_axes,
            } => {
                let print = |args: std::fmt::Arguments<'_>| {
                    PortManager::global().trace_manager().print(
                        Some(port),
                        Some(*addr),
                        0,
                        TraceMask::ERROR,
                        args,
                    )
                };
                print(format_args!(
                    "asynPortDriver:getAddress: {port} invalid address={addr}, must be in range 0 to {}\n",
                    num_axes - 1
                ));
                print(format_args!(
                    "devMotorAsyn::findDrvInfo, {name} drvUserCreate failed for MOTOR_MOVE_REL\n"
                ));
                let refused = format!(
                    "asynPortDriver:getAddress: {port} invalid address={addr}, must be in range 0 to {}\n",
                    num_axes - 1
                );
                // motorRecord's init_record still sends both dial limits
                // (motorRecord.cc:717-718). The port thread runs a request
                // for a device that fails autoConnect anyway, and the
                // driver's getAddress refuses it with no errorMessage
                // (asynManager.c:877-916).
                for _ in 0..2 {
                    if auto_connect_due(port, *addr) {
                        print(format_args!("{refused}"));
                        print(format_args!(
                            "{port} {addr} autoConnect could not connect: \n"
                        ));
                    }
                    print(format_args!("{refused}"));
                    print(format_args!(
                        "devMotorAsyn::asynCallback: {name} pasyn{{Float64,Int32}}->write returned \n"
                    ));
                }
                Ok(bad(record))
            }
            Failure::BadBus => {
                // S_dev_badBus: M_devSup has no symbol table.
                rec_gbl_record_error(
                    "Error (514,13)",
                    name,
                    "motor_init_record_com(): Illegal OUT Bus Type",
                );
                failed_init(record, false);
                Err(CaError::InvalidValue("Illegal OUT Bus Type".into()))
            }
            Failure::IllegalInp | Failure::NoDrvet | Failure::NoOpen => {
                // M_drvSup has no symbol table either.
                let (status, message) = match self.failure {
                    Failure::IllegalInp => (
                        "Illegal field value",
                        "devMotor (init_record) Illegal INP field",
                    ),
                    Failure::NoDrvet => (
                        "Error (513,3)",
                        "devMotor (init_record) cannot find device support entry table",
                    ),
                    _ => (
                        "Error (513,1)",
                        "devMotor (init_record) cannot open device support",
                    ),
                };
                rec_gbl_record_error(status, name, message);
                failed_init(record, false);
                Err(CaError::InvalidValue(message.into()))
            }
        }
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        self.dtyp
    }

    fn set_record_info(&mut self, name: &str, _scan: ScanType) {
        self.record = name.to_owned();
    }
}

/// motorRecord's init_record still sends both dial limits after `bad:`
/// (motorRecord.cc:717-718); a user that never connected to a port cannot
/// queue them (devMotorAsyn.c:641-645, asynManager.c:1528-1531).
fn unqueued_dial_limits(name: &str) {
    use asyn_rs::manager::PortManager;
    use asyn_rs::trace::TraceMask;
    for _ in 0..2 {
        PortManager::global().trace_manager().print(
            None,
            None,
            0,
            TraceMask::ERROR,
            format_args!(
                "devMotorAsyn::build_trans: {name} error calling queueRequest, asynManager::queueRequest asynUser not associated with a port\n"
            ),
        );
    }
}

/// asynManager's `autoConnectDevice`: one attempt per device in any 2 s
/// (asynManager.c:727-737), shared by every record on that address.
fn auto_connect_due(port: &str, addr: i32) -> bool {
    use std::time::{Duration, Instant};
    static LAST: OnceLock<Mutex<HashMap<(String, i32), Instant>>> = OnceLock::new();
    let mut last = lock(LAST.get_or_init(Default::default));
    let now = Instant::now();
    let key = (port.to_owned(), addr);
    if last
        .get(&key)
        .is_some_and(|t| now.duration_since(*t) < Duration::from_secs(2))
    {
        return false;
    }
    last.insert(key, now);
    true
}

/// devMotorAsyn's init_record on asyn port `port`, which has no motor axes
/// (devMotorAsyn.c:293-337): its line for the first interface the port
/// lacks or the first motor drvInfo its drvUserCreate refuses. C's `bad:`
/// still lets motorRecord queue both dial limits on reason 0 of that port,
/// or on a NULL interface (motorRecord.cc:717-718); that C defect is not
/// copied.
fn refuse_foreign_port(name: &str, port: &str, addr: i32, user_param: Option<&str>) {
    use asyn_rs::interfaces::InterfaceType;
    use asyn_rs::manager::PortManager;
    use asyn_rs::port::DrvUserRequest;
    use asyn_rs::trace::TraceMask;

    let Some(handle) = asyn_rs::registry::get_port(port).map(|entry| entry.handle) else {
        return;
    };
    let print = |args: std::fmt::Arguments<'_>| {
        PortManager::global().trace_manager().print(
            Some(port),
            Some(addr),
            0,
            TraceMask::ERROR,
            args,
        )
    };
    let lacks = |iface| !handle.has_interface(iface);
    let creates = |drv_info: &str| {
        handle
            .drv_user_create_blocking(&DrvUserRequest::new(drv_info, addr))
            .is_ok()
    };
    for (iface, which) in [
        (InterfaceType::Int32, "int32"),
        (InterfaceType::Float64, "float64"),
    ] {
        if lacks(iface) {
            print(format_args!(
                "devMotorAsyn::init_record, {name} find {which} interface failed\n"
            ));
            return;
        }
    }
    // pasynFloat64SyncIO->connect: asynCommon, then the userParam's
    // drvUserCreate where the port has asynDrvUser (asynFloat64SyncIO.c).
    if lacks(InterfaceType::Common)
        || user_param.is_some_and(|p| !lacks(InterfaceType::DrvUser) && !creates(p))
    {
        print(format_args!(
            "devMotorAsyn::init_record, {name} connect Float64SyncIO interface failed\n"
        ));
        return;
    }
    if lacks(InterfaceType::DrvUser) {
        print(format_args!(
            "devMotorAsyn::init_record, {name} find drvUser interface failed\n"
        ));
        return;
    }
    if let Some(drv_info) = MOTOR_DRV_INFO.iter().find(|drv_info| !creates(drv_info)) {
        print(format_args!(
            "devMotorAsyn::findDrvInfo, {name} drvUserCreate failed for {drv_info}\n"
        ));
    }
}

/// The drvInfo strings devMotorAsyn's init_record looks up, in its order
/// (devMotorAsyn.c:333-351).
const MOTOR_DRV_INFO: [&str; 19] = [
    "MOTOR_MOVE_REL",
    "MOTOR_MOVE_ABS",
    "MOTOR_MOVE_VEL",
    "MOTOR_HOME",
    "MOTOR_STOP_AXIS",
    "MOTOR_VELOCITY",
    "MOTOR_VEL_BASE",
    "MOTOR_ACCEL",
    "MOTOR_POSITION",
    "MOTOR_RESOLUTION",
    "MOTOR_ENCODER_RATIO",
    "MOTOR_PGAIN",
    "MOTOR_IGAIN",
    "MOTOR_DGAIN",
    "MOTOR_HIGH_LIMIT",
    "MOTOR_LOW_LIMIT",
    "MOTOR_CLOSED_LOOP",
    "MOTOR_STATUS",
    "MOTOR_UPDATE_STATUS",
];

/// devMotorAsyn's `bad:` arm: `pmr->pact = 1; return 0`, after which
/// motorRecord's init_record runs on to its closing `monitor()`, through
/// set_dial_highlimit/lowlimit (717-718).
fn bad(record: &mut dyn Record) -> DeviceInitOutcome {
    if let Some(motor) = record
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<crate::record::MotorRecord>())
    {
        motor.finish_init();
        motor.set_dial_limit(true);
        motor.set_dial_limit(false);
    }
    DeviceInitOutcome::dead()
}

/// A model-1 init that returned a status: `motor_init_record_com`'s
/// readback reset when the card or signal is refused (motordevCom.cc:250-256),
/// then `motorRecord`'s `pmr->card = -1` (motorRecord.cc:648-651).
fn failed_init(record: &mut dyn Record, readback: bool) {
    if let Some(motor) = record
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<crate::record::MotorRecord>())
    {
        crate::record::motor_failed_dset_init(motor, readback);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inst_io_reads_port_and_addr_as_parse_link_does() {
        let ok = |port: &str, addr| Ok((port.into(), addr, None));
        let invalid = Err("invalid INST_IO Must be asyn(<port> <addr> <timeout>)userParams");
        assert_eq!(parse_link("@asyn(M1,3)"), ok("M1", 3));
        assert_eq!(
            parse_link("@asyn(M1 3 0.5) x"),
            Ok(("M1".into(), 3, Some("x".into())))
        );
        assert_eq!(parse_link("@asyn( M1)"), ok("M1", 0));
        assert_eq!(parse_link("@asyn(M1, 0x10)"), ok("M1", 16));
        assert_eq!(parse_link("@asyn(M1"), invalid);
        assert_eq!(parse_link("@asyn(M1,"), invalid);
        assert_eq!(parse_link("@asyn(M1,1,2"), invalid);
        assert_eq!(parse_link("@M1,1"), invalid);
        // C strtod reads a hex or spelled timeout, and refuses one out of
        // range (ERANGE).
        assert_eq!(parse_link("@asyn(M1 3 0x1p1)"), ok("M1", 3));
        assert_eq!(parse_link("@asyn(M1 3 inf)"), ok("M1", 3));
        assert_eq!(parse_link("@asyn(M1 3 1e999)"), invalid);
        // C strtol takes LONG_MIN where long is 64 bits, and the int it lands
        // in keeps the low bits; a 32-bit long overflows it (ERANGE).
        let lp64 = std::ffi::c_long::BITS == 64;
        assert_eq!(
            parse_link("@asyn(M1 -9223372036854775808)"),
            if lp64 { ok("M1", 0) } else { invalid }
        );
        // C isspace separates on a vertical tab.
        assert_eq!(parse_link("@asyn(M1\x0b3)"), ok("M1", 3));
    }

    #[test]
    fn a_vme_io_link_names_the_port_in_parm() {
        let ok = |port: &str, user_param: &str| Ok((port.into(), 2, Some(user_param.into())));
        assert_eq!(parse_link("#C0 S2 @M1 x"), ok("M1", "x"));
        assert_eq!(parse_link("#C0 S2 @M1,0"), ok("M1", ",0"));
        // C isspace ends the port at a vertical tab.
        assert_eq!(parse_link("#C0 S2 @M1\x0bx"), ok("M1", "x"));
        assert_eq!(
            parse_link("#C0 S2 @"),
            Err("parm is null. Must be <port> <addr> ...")
        );
        assert_eq!(
            parse_link("OTHER.VAL"),
            Err("Link must be INST_IO or VME_IO")
        );
    }
}
