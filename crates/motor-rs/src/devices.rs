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
use crate::device_support::{MotorAxis, MotorDeviceSupport};
use crate::poll_loop::MissingAxis;
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
const MOTOR_DSET: Dset = Dset::new(8, &[1, 2, 4, 5, 6, 7], false);

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
    epics_base_rs::server::record::register_device_row("motor", row);
    lock(model1_choices()).insert(row.choice);
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

/// Asyn motor ports by name: their driver and `numAxes`.
fn ports() -> &'static Mutex<HashMap<String, (PortDriver, i32)>> {
    static PORTS: OnceLock<Mutex<HashMap<String, (PortDriver, i32)>>> = OnceLock::new();
    PORTS.get_or_init(Default::default)
}

fn register_port(port: &str, driver: PortDriver, num_axes: i32) -> Result<(), String> {
    let mut ports = lock(ports());
    if ports.contains_key(port) {
        return Err(format!("motor port {port} is already registered"));
    }
    ports.insert(port.to_owned(), (driver, num_axes));
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

/// Controller port `port`'s store for a [`WriteHook`]'s sets.
pub(crate) fn param_store(port: &str) -> Option<crate::controller_port::ParamStore> {
    lock(controller_ports())
        .get(port)
        .map(|entry| entry.store.clone())
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

fn model1_choices() -> &'static Mutex<HashSet<&'static str>> {
    static CHOICES: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
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
/// second axis at the same address is refused, and an asyn axis needs its
/// port registered with that driver and the address in range.
pub fn register_axis(axis: MotorAxis) -> Result<(), String> {
    let (controller, addr) = split(axis.address());
    if let Controller::Port(port) = &controller {
        let model2 = matches!(axis.address(), TraceUser::Model2 { .. });
        match lock(ports()).get(port) {
            None => return Err(format!("motor port {port} is not registered")),
            Some(&(driver, n)) => {
                if (driver == PortDriver::DrvMotorAsyn) != model2 {
                    return Err(format!("motor port {port} has a {driver:?} driver"));
                }
                if !(0..n).contains(&addr) {
                    return Err(format!("motor port {port} has no axis {addr}"));
                }
            }
        }
    }
    let mut controllers = lock(controllers());
    let axes = controllers.entry(controller.clone()).or_default();
    if axes.contains_key(&addr) {
        return Err(format!(
            "axis {addr} of {controller:?} is already registered"
        ));
    }
    axes.insert(addr, (axis, false));
    drop(controllers);
    if let Controller::Port(port) = &controller {
        if let Some(handle) = controller_port_handle(port) {
            init_axis_params(&handle, addr);
        }
    }
    Ok(())
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
        let (dtyp, bound) = if ctx.dtyp == DEV_MOTOR_ASYN.choice {
            (DEV_MOTOR_ASYN.choice, bind_asyn(ctx.out))
        } else if let Some(device) = interface {
            (device, bind_motor_interface(ctx.out))
        } else {
            let device = lock(model1_choices()).get(ctx.dtyp).copied()?;
            (device, bind_model1(device, ctx.out))
        };
        Some(match bound {
            Ok(axis) => Box::new(axis) as Box<dyn DeviceSupport>,
            Err(failure) => Box::new(Unbound {
                dtyp,
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
    /// `asynPortDriver::getAddress` refuses an address past `numAxes`, so
    /// the first `findDrvInfo` fails (asynPortDriver.cpp:1901-1915, 3569).
    BadAddress {
        port: String,
        addr: i32,
        num_axes: i32,
    },
    /// `motor_init_record_com`'s four refusals.
    BadBus,
    NoCard,
    NoSignal,
    InUse,
    /// devMotorSim's three refusals: OUT not VME_IO, no drvet by the parm's
    /// name, and the driver's `open` returning NULL.
    IllegalInp,
    NoDrvet,
    NoOpen,
}

/// Bind a record to the axis. An asyn axis serves every record that names
/// it; a model-1 signal serves one (motordevCom.cc:244-247).
fn take(controller: Controller, addr: i32) -> Result<MotorDeviceSupport, Taken> {
    let mut controllers = lock(controllers());
    let axes = controllers
        .get_mut(&controller)
        .ok_or(Taken::NoController)?;
    let (axis, bound) = axes.get_mut(&addr).ok_or(Taken::NoAxis)?;
    if *bound && matches!(controller, Controller::Card { .. }) {
        return Err(Taken::InUse);
    }
    *bound = true;
    Ok(axis.attach())
}

enum Taken {
    NoController,
    NoAxis,
    InUse,
}

/// devMotorAsyn's binding: a live port reached at an axis it lacks still
/// serves the record, with the driver refusing every request.
fn bind_asyn(out: &str) -> Result<MotorDeviceSupport, Failure> {
    let (port, addr) = parse_link(out).map_err(Failure::BadLink)?;
    let Some((driver, num_axes)) = lock(ports()).get(&port).copied() else {
        return Err(Failure::NoPort(port));
    };
    let in_range = (0..num_axes).contains(&addr);
    let (user, missing) = match driver {
        PortDriver::Controller if !in_range => {
            return Err(Failure::BadAddress {
                port,
                addr,
                num_axes,
            });
        }
        PortDriver::Controller => (
            TraceUser::Asyn {
                port: port.clone(),
                addr,
            },
            MissingAxis::Controller,
        ),
        PortDriver::DrvMotorAsyn => (
            TraceUser::Model2 {
                port: port.clone(),
                addr,
            },
            MissingAxis::DrvMotorAsyn { invalid: !in_range },
        ),
    };
    take(Controller::Port(port), addr).or_else(|_| {
        Ok(MotorDeviceSupport::missing(
            Arc::new(MotorTrace::with_user(user)),
            missing,
        ))
    })
}

fn bind_model1(device: &'static str, out: &str) -> Result<MotorDeviceSupport, Failure> {
    let ParsedLink::Hw(HwLink::VmeIo { card, signal, parm }) = parse_link_v2(out) else {
        return Err(Failure::BadBus);
    };
    let card = i32::from(card);
    let support =
        take(Controller::Card { device, card }, i32::from(signal)).map_err(
            |taken| match taken {
                Taken::NoController => Failure::NoCard,
                Taken::NoAxis => Failure::NoSignal,
                Taken::InUse => Failure::InUse,
            },
        )?;
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

/// `pasynEpicsUtils->parseLink` (asynEpicsUtils.c:56-148): the port and
/// address, or the `errorMessage` it writes.
fn parse_link(out: &str) -> Result<(String, i32), &'static str> {
    const INVALID: &str = "invalid INST_IO Must be asyn(<port> <addr> <timeout>)userParams";
    match parse_link_v2(out) {
        ParsedLink::Hw(HwLink::VmeIo { signal, parm, .. }) => {
            let port: String = parm
                .trim_start()
                .chars()
                .take_while(|c| !c.is_ascii_whitespace() && *c != ',')
                .collect();
            if port.is_empty() {
                return Err("parm is null. Must be <port> <addr> ...");
            }
            Ok((port, i32::from(signal)))
        }
        ParsedLink::Hw(HwLink::InstIo { string }) => parse_inst_io(&string).ok_or(INVALID),
        _ => Err("Link must be INST_IO or VME_IO"),
    }
}

/// The INST_IO arm: `asyn(<port> <addr> <timeout>)`, separators blanks or
/// commas, addr and timeout optional.
fn parse_inst_io(s: &str) -> Option<(String, i32)> {
    let skip = |p: &str, comma: bool| -> usize {
        p.find(|c: char| !(c.is_ascii_whitespace() || (comma && c == ',')))
            .unwrap_or(p.len())
    };
    let mut p = &s[s.find("asyn(")? + 5..];
    p = &p[skip(p, false)..];
    let len = p
        .find(|c: char| c.is_ascii_whitespace() || c == ',' || c == ')')
        .unwrap_or(p.len());
    if len == p.len() {
        return None;
    }
    let port = p[..len].to_owned();
    p = &p[len..];
    p = &p[skip(p, true)..];
    if p.is_empty() {
        return None;
    }
    if p.starts_with(')') {
        return Some((port, 0));
    }
    let (addr, used) = strtol(p)?;
    p = &p[used..];
    p = &p[skip(p, true)..];
    if p.is_empty() {
        return None;
    }
    if p.starts_with(')') {
        return Some((port, addr));
    }
    p = &p[strtod_len(p)..];
    p = &p[skip(p, false)..];
    p.starts_with(')').then_some((port, addr))
}

/// `strtol(p, &endp, 0)` into an `int`: the value and the bytes consumed,
/// `None` where C's `errno` is set (out of `long` range). No digits consume
/// nothing and read 0, as C's does.
fn strtol(p: &str) -> Option<(i32, usize)> {
    let b = p.as_bytes();
    let mut i = b.iter().take_while(|c| c.is_ascii_whitespace()).count();
    let neg = matches!(b.get(i), Some(b'-'));
    if matches!(b.get(i), Some(b'-' | b'+')) {
        i += 1;
    }
    let radix = if b.get(i) == Some(&b'0')
        && matches!(b.get(i + 1), Some(b'x' | b'X'))
        && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
    {
        i += 2;
        16
    } else if b.get(i) == Some(&b'0') {
        8
    } else {
        10
    };
    let start = i;
    let mut value: i64 = 0;
    while let Some(d) = b.get(i).and_then(|c| (*c as char).to_digit(radix)) {
        value = value
            .checked_mul(i64::from(radix))?
            .checked_add(i64::from(d))?;
        i += 1;
    }
    if i == start {
        return Some((0, 0));
    }
    let value = if neg { -value } else { value };
    Some((value as i32, i))
}

/// The bytes `strtod` consumes for a decimal floating constant.
fn strtod_len(p: &str) -> usize {
    let b = p.as_bytes();
    let mut i = b.iter().take_while(|c| c.is_ascii_whitespace()).count();
    if matches!(b.get(i), Some(b'-' | b'+')) {
        i += 1;
    }
    let mantissa = i;
    i += b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    if b.get(i) == Some(&b'.') {
        i += 1;
        i += b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    }
    if i == mantissa || (i == mantissa + 1 && b[mantissa] == b'.') {
        return 0;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        if matches!(b.get(j), Some(b'-' | b'+')) {
            j += 1;
        }
        let digits = b[j..].iter().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 {
            i = j + digits;
        }
    }
    i
}

/// A motor record whose OUT reached no axis. Its init prints what C's does
/// and fails the same way.
struct Unbound {
    dtyp: &'static str,
    failure: Failure,
    record: String,
}

impl DeviceSupport for Unbound {
    fn init(&mut self, record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        use asyn_rs::manager::PortManager;
        use asyn_rs::trace::TraceMask;
        use epics_base_rs::server::recgbl::rec_gbl_record_error;

        let name = &self.record;
        let message = match &self.failure {
            Failure::BadLink(why) => {
                epics_base_rs::runtime::log::errlog_vprintf(format_args!(
                    "devMotorAsyn::init_record {name} bad link {why}\n"
                ));
                return Ok(bad(record));
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
                return Ok(bad(record));
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
                return Ok(bad(record));
            }
            Failure::BadBus => {
                // S_dev_badBus: M_devSup has no symbol table.
                rec_gbl_record_error(
                    "Error (514,13)",
                    name,
                    "motor_init_record_com(): Illegal OUT Bus Type",
                );
                failed_init(record, false);
                return Err(CaError::InvalidValue("Illegal OUT Bus Type".into()));
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
                return Err(CaError::InvalidValue(message.into()));
            }
            Failure::NoCard => "motor_init_record_com(): card does not exist!",
            Failure::NoSignal => "motor_init_record_com(): signal does not exist!",
            Failure::InUse => "motor_init_record_com(): motor already in use!",
        };
        rec_gbl_record_error("Illegal field value", name, message);
        failed_init(record, true);
        Err(CaError::InvalidValue(message.into()))
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
        assert_eq!(parse_inst_io("asyn(M1,3)"), Some(("M1".into(), 3)));
        assert_eq!(parse_inst_io("asyn(M1 3 0.5)x"), Some(("M1".into(), 3)));
        assert_eq!(parse_inst_io("asyn( M1)"), Some(("M1".into(), 0)));
        assert_eq!(parse_inst_io("asyn(M1, 0x10)"), Some(("M1".into(), 16)));
        assert_eq!(parse_inst_io("asyn(M1"), None);
        assert_eq!(parse_inst_io("asyn(M1,"), None);
        assert_eq!(parse_inst_io("asyn(M1,1,2"), None);
        assert_eq!(parse_inst_io("M1,1"), None);
    }

    #[test]
    fn a_vme_io_link_names_the_port_in_parm() {
        assert_eq!(parse_link("#C0 S2 @M1 x"), Ok(("M1".into(), 2)));
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
