//! C `asynMotorController`'s own asyn port: the parameter library a
//! controller's records reach through `@asyn(PORT,addr)`, at the controller
//! address and at each axis, beside the motor records' `asynMotor` path.
//!
//! A write runs the driver's [`ControllerParams`] hook first, as a C subclass
//! `writeInt32` / `writeFloat64` does before it calls the base, then the base
//! (asynMotorController.cpp:191-256, :269-430): store the value, hand a base
//! function's command to the axis at that address, run the callbacks. The
//! profile functions are the controller's, over every axis, through the
//! [`ControllerParams`] profile hooks a driver overrides as C's virtuals.
//!
//! A base function's driver call runs on the port thread under the axis's
//! lock, as C's runs under the controller lock, and its status is the
//! write's; the axis's poller posts what it set. A motor record's writes
//! reach the same C subclass write, but here run on the axis's poller; what
//! the subclass does around the base on both paths is its [`WriteHook`].

use std::sync::Arc;

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::interfaces::motor::PortSet;
use asyn_rs::manager::PortManager;
use asyn_rs::param::{ParamType, ParamValue};
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{DrvUserInfo, DrvUserRequest, PortDriver, PortDriverBase, PortFlags};
use asyn_rs::port_handle::PortHandle;
use asyn_rs::registry::claim_port;
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime;
use asyn_rs::trace::TraceMask;
use asyn_rs::user::AsynUser;
use epics_base_rs::runtime::interrupt_accept::InterruptGate;
use epics_libcom_rs::cfmt::CFloat;

use crate::device_support::MotorAxis;
use crate::flags::{MotorCommand, PidGainKind};
use crate::poll_loop::{AxisWrite, Written};
use crate::trace::MotorTrace;

/// C `asynMotorController`'s `createParam` calls (asynMotorController.cpp:
/// 56-129), in order: index `i` is the `i`-th parameter, as in C.
pub const BASE_PARAMS: &[(&str, ParamType)] = &[
    ("MOTOR_MOVE_REL", ParamType::Float64),
    ("MOTOR_MOVE_ABS", ParamType::Float64),
    ("MOTOR_MOVE_VEL", ParamType::Float64),
    ("MOTOR_HOME", ParamType::Float64),
    ("MOTOR_STOP_AXIS", ParamType::Int32),
    ("MOTOR_VELOCITY", ParamType::Float64),
    ("MOTOR_ACT_VELOCITY", ParamType::Float64),
    ("MOTOR_VEL_BASE", ParamType::Float64),
    ("MOTOR_ACCEL", ParamType::Float64),
    ("MOTOR_POSITION", ParamType::Float64),
    ("MOTOR_ENCODER_POSITION", ParamType::Float64),
    ("MOTOR_DEFER_MOVES", ParamType::Int32),
    ("MOTOR_MOVE_HOME", ParamType::Int32),
    ("MOTOR_RESOLUTION", ParamType::Float64),
    ("MOTOR_ENCODER_RATIO", ParamType::Float64),
    ("MOTOR_PGAIN", ParamType::Float64),
    ("MOTOR_IGAIN", ParamType::Float64),
    ("MOTOR_DGAIN", ParamType::Float64),
    ("MOTOR_HIGH_LIMIT", ParamType::Float64),
    ("MOTOR_LOW_LIMIT", ParamType::Float64),
    ("MOTOR_CLOSED_LOOP", ParamType::Int32),
    ("MOTOR_POWER_AUTO_ONOFF", ParamType::Int32),
    ("MOTOR_POWER_ON_DELAY", ParamType::Float64),
    ("MOTOR_POWER_OFF_DELAY", ParamType::Float64),
    ("MOTOR_POWER_OFF_FRACTION", ParamType::Int32),
    ("MOTOR_POST_MOVE_DELAY", ParamType::Float64),
    ("MOTOR_STATUS", ParamType::Int32),
    ("MOTOR_UPDATE_STATUS", ParamType::Int32),
    ("MOTOR_STATUS_DIRECTION", ParamType::Int32),
    ("MOTOR_STATUS_DONE", ParamType::Int32),
    ("MOTOR_STATUS_HIGH_LIMIT", ParamType::Int32),
    ("MOTOR_STATUS_AT_HOME", ParamType::Int32),
    ("MOTOR_STATUS_SLIP", ParamType::Int32),
    ("MOTOR_STATUS_POWERED", ParamType::Int32),
    ("MOTOR_STATUS_FOLLOWING_ERROR", ParamType::Int32),
    ("MOTOR_STATUS_HOME", ParamType::Int32),
    ("MOTOR_STATUS_HAS_ENCODER", ParamType::Int32),
    ("MOTOR_STATUS_PROBLEM", ParamType::Int32),
    ("MOTOR_STATUS_MOVING", ParamType::Int32),
    ("MOTOR_STATUS_GAIN_SUPPORT", ParamType::Int32),
    ("MOTOR_STATUS_COMMS_ERROR", ParamType::Int32),
    ("MOTOR_STATUS_LOW_LIMIT", ParamType::Int32),
    ("MOTOR_STATUS_HOMED", ParamType::Int32),
    ("MOTOR_REC_RESOLUTION", ParamType::Float64),
    ("MOTOR_REC_DIRECTION", ParamType::Int32),
    ("MOTOR_REC_OFFSET", ParamType::Float64),
    ("PROFILE_NUM_AXES", ParamType::Int32),
    ("PROFILE_NUM_POINTS", ParamType::Int32),
    ("PROFILE_CURRENT_POINT", ParamType::Int32),
    ("PROFILE_NUM_PULSES", ParamType::Int32),
    ("PROFILE_START_PULSES", ParamType::Int32),
    ("PROFILE_END_PULSES", ParamType::Int32),
    ("PROFILE_ACTUAL_PULSES", ParamType::Int32),
    ("PROFILE_NUM_READBACKS", ParamType::Int32),
    ("PROFILE_TIME_MODE", ParamType::Int32),
    ("PROFILE_FIXED_TIME", ParamType::Float64),
    ("PROFILE_TIME_ARRAY", ParamType::Float64Array),
    ("PROFILE_ACCELERATION", ParamType::Float64),
    ("PROFILE_MOVE_MODE", ParamType::Int32),
    ("PROFILE_BUILD", ParamType::Int32),
    ("PROFILE_BUILD_STATE", ParamType::Int32),
    ("PROFILE_BUILD_STATUS", ParamType::Int32),
    ("PROFILE_BUILD_MESSAGE", ParamType::Octet),
    ("PROFILE_EXECUTE", ParamType::Int32),
    ("PROFILE_EXECUTE_STATE", ParamType::Int32),
    ("PROFILE_EXECUTE_STATUS", ParamType::Int32),
    ("PROFILE_EXECUTE_MESSAGE", ParamType::Octet),
    ("PROFILE_ABORT", ParamType::Int32),
    ("PROFILE_READBACK", ParamType::Int32),
    ("PROFILE_READBACK_STATE", ParamType::Int32),
    ("PROFILE_READBACK_STATUS", ParamType::Int32),
    ("PROFILE_READBACK_MESSAGE", ParamType::Octet),
    ("PROFILE_USE_AXIS", ParamType::Int32),
    ("PROFILE_POSITIONS", ParamType::Float64Array),
    ("PROFILE_READBACKS", ParamType::Float64Array),
    ("PROFILE_FOLLOWING_ERRORS", ParamType::Float64Array),
];

/// The [`BASE_PARAMS`] indices a write acts on.
pub mod index {
    pub const MOVE_REL: usize = 0;
    pub const MOVE_ABS: usize = 1;
    pub const MOVE_VEL: usize = 2;
    pub const HOME: usize = 3;
    pub const STOP: usize = 4;
    pub const VELOCITY: usize = 5;
    pub const ACT_VELOCITY: usize = 6;
    pub const VEL_BASE: usize = 7;
    pub const ACCEL: usize = 8;
    pub const POSITION: usize = 9;
    pub const ENCODER_POSITION: usize = 10;
    pub const DEFER_MOVES: usize = 11;
    pub const MOVE_TO_HOME: usize = 12;
    pub const RESOLUTION: usize = 13;
    pub const ENCODER_RATIO: usize = 14;
    pub const PGAIN: usize = 15;
    pub const IGAIN: usize = 16;
    pub const DGAIN: usize = 17;
    pub const HIGH_LIMIT: usize = 18;
    pub const LOW_LIMIT: usize = 19;
    pub const CLOSED_LOOP: usize = 20;
    pub const POWER_AUTO_ONOFF: usize = 21;
    pub const POWER_ON_DELAY: usize = 22;
    pub const POWER_OFF_DELAY: usize = 23;
    pub const STATUS: usize = 26;
    pub const UPDATE_STATUS: usize = 27;
    /// The first of the 15 MOTOR_STATUS_* bits, in MSTA bit order.
    pub const STATUS_DIRECTION: usize = 28;
    pub const STATUS_COMMS_ERROR: usize = 40;
    pub const REC_RESOLUTION: usize = 43;
    pub const REC_DIRECTION: usize = 44;
    pub const REC_OFFSET: usize = 45;
    pub const PROFILE_NUM_POINTS: usize = 47;
    pub const PROFILE_CURRENT_POINT: usize = 48;
    pub const PROFILE_NUM_PULSES: usize = 49;
    pub const PROFILE_START_PULSES: usize = 50;
    pub const PROFILE_END_PULSES: usize = 51;
    pub const PROFILE_ACTUAL_PULSES: usize = 52;
    pub const PROFILE_TIME_MODE: usize = 54;
    pub const PROFILE_NUM_READBACKS: usize = 53;
    pub const PROFILE_FIXED_TIME: usize = 55;
    pub const PROFILE_TIME_ARRAY: usize = 56;
    pub const PROFILE_ACCELERATION: usize = 57;
    pub const PROFILE_MOVE_MODE: usize = 58;
    pub const PROFILE_BUILD: usize = 59;
    pub const PROFILE_BUILD_STATE: usize = 60;
    pub const PROFILE_BUILD_STATUS: usize = 61;
    pub const PROFILE_BUILD_MESSAGE: usize = 62;
    pub const PROFILE_EXECUTE: usize = 63;
    pub const PROFILE_EXECUTE_STATE: usize = 64;
    pub const PROFILE_EXECUTE_STATUS: usize = 65;
    pub const PROFILE_EXECUTE_MESSAGE: usize = 66;
    pub const PROFILE_ABORT: usize = 67;
    pub const PROFILE_READBACK: usize = 68;
    pub const PROFILE_READBACK_STATE: usize = 69;
    pub const PROFILE_READBACK_STATUS: usize = 70;
    pub const PROFILE_READBACK_MESSAGE: usize = 71;
    pub const PROFILE_USE_AXIS: usize = 72;
    pub const PROFILE_POSITIONS: usize = 73;
    pub const PROFILE_READBACKS: usize = 74;
    pub const PROFILE_FOLLOWING_ERRORS: usize = 75;
}

/// C `PROFILE_TIME_MODE_FIXED`.
pub const PROFILE_TIME_MODE_FIXED: i32 = 0;

/// C `ProfileMoveMode`.
pub const PROFILE_MOVE_MODE_ABSOLUTE: i32 = 0;
pub const PROFILE_MOVE_MODE_RELATIVE: i32 = 1;

/// C `ProfileBuildState` and `ProfileReadbackState`; the PROFILE_*_STATE
/// mbbi records index these.
pub const PROFILE_BUILD_DONE: i32 = 0;
pub const PROFILE_BUILD_BUSY: i32 = 1;
pub const PROFILE_READBACK_DONE: i32 = 0;
pub const PROFILE_READBACK_BUSY: i32 = 1;

/// C `ProfileExecuteState`.
pub const PROFILE_EXECUTE_DONE: i32 = 0;
pub const PROFILE_EXECUTE_MOVE_START: i32 = 1;
pub const PROFILE_EXECUTE_EXECUTING: i32 = 2;
pub const PROFILE_EXECUTE_FLYBACK: i32 = 3;

/// C `ProfileStatus`, for build, execute and readback.
pub const PROFILE_STATUS_UNDEFINED: i32 = 0;
pub const PROFILE_STATUS_SUCCESS: i32 = 1;
pub const PROFILE_STATUS_FAILURE: i32 = 2;
pub const PROFILE_STATUS_ABORT: i32 = 3;
pub const PROFILE_STATUS_TIMEOUT: i32 = 4;

/// Whether a C subclass write calls `asynMotorController::writeInt32` /
/// `writeFloat64` after its own code.
#[must_use]
#[derive(Debug)]
pub enum ParamWrite {
    /// The base runs: stores the value, runs its function and the
    /// callbacks; an `Err` here is the write's once it has.
    PassToBase(AsynResult<()>),
    /// The driver's own write, as LC400's registers: the base does not run,
    /// so nothing is stored and no callback fires unless the driver does it.
    Handled(AsynResult<()>),
}

impl From<AsynResult<()>> for ParamWrite {
    fn from(result: AsynResult<()>) -> Self {
        Self::PassToBase(result)
    }
}

/// The value a C `writeInt32` / `writeFloat64` receives.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WriteValue {
    Float64(f64),
    Int32(i32),
}

/// A write the base has finished, for [`WriteHook::after_write`].
pub struct FinishedWrite<'a> {
    pub addr: i32,
    pub function: usize,
    pub value: WriteValue,
    /// The write's status, as the C subclass's `status` holds it after
    /// the base returns.
    pub status: &'a AsynResult<()>,
    print: &'a dyn Fn(TraceMask, std::fmt::Arguments<'_>),
}

impl<'a> FinishedWrite<'a> {
    pub(crate) fn new(
        addr: i32,
        function: usize,
        value: WriteValue,
        status: &'a AsynResult<()>,
        print: &'a dyn Fn(TraceMask, std::fmt::Arguments<'_>),
    ) -> Self {
        Self {
            addr,
            function,
            value,
            status,
            print,
        }
    }

    /// `asynPrint(pasynUser, ...)` on the writer's user.
    pub fn print(&self, mask: TraceMask, args: std::fmt::Arguments<'_>) {
        (self.print)(mask, args)
    }
}

/// The parameters a [`WriteHook::after_write`] sets at the write's address,
/// as a C subclass's `setIntegerParam` / `setDoubleParam` after the base
/// returns. The hook runs inside the port's write, so it cannot reach the
/// port; the write's owner applies these, and its next `callParamCallbacks`
/// on that address posts them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParamDelta(Vec<(usize, WriteValue)>);

impl ParamDelta {
    /// `setIntegerParam(addr, function, value)`.
    pub fn set_int32(&mut self, function: usize, value: i32) {
        self.0.push((function, WriteValue::Int32(value)));
    }

    /// `setDoubleParam(addr, function, value)`.
    pub fn set_float64(&mut self, function: usize, value: f64) {
        self.0.push((function, WriteValue::Float64(value)));
    }

    pub(crate) fn into_values(self) -> impl Iterator<Item = (usize, ParamValue)> {
        self.0.into_iter().map(|(function, value)| {
            let value = match value {
                WriteValue::Float64(v) => ParamValue::Float64(v),
                WriteValue::Int32(v) => ParamValue::Int32(v),
            };
            (function, value)
        })
    }
}

/// What a [`WriteHook::after_write`] leaves of the write.
#[derive(Debug, Default)]
pub struct WriteOutcome {
    pub delta: ParamDelta,
    /// What the subclass's write returns when it is not the base's status,
    /// as parker `OEMController::writeInt32` returns its MR command's;
    /// `None` keeps the base's.
    pub status: Option<AsynResult<()>>,
}

/// The user a subclass's code runs for, at the address C `getAddress`
/// gives its `getAxis`: the parameter list, `-1` read as 0. C refuses an
/// address out of range at connect (asynPortDriver.cpp:3772), so no
/// subclass code runs there.
#[derive(Clone, Copy)]
pub struct AxisUser<'a> {
    pub reason: usize,
    /// The axis, never `-1`.
    pub addr: i32,
    asyn: &'a AsynUser,
}

impl<'a> AxisUser<'a> {
    /// `getAddress(pasynUser, &addr)`, printing a refused address.
    pub fn new(base: &PortDriverBase, asyn: &'a AsynUser) -> AsynResult<Self> {
        Ok(Self {
            reason: asyn.reason,
            addr: base.get_address(asyn.addr, asyn.reason)?,
            asyn,
        })
    }

    /// The asyn user itself, for asynPortDriver's own calls and
    /// `asynPrint`, which take the user's address as it is.
    pub fn asyn(&self) -> &'a AsynUser {
        self.asyn
    }

    /// Run `command` on the axis's driver now and return its status, as a
    /// C subclass write calls its axis under the controller lock.
    pub fn run(&self, base: &mut PortDriverBase, command: MotorCommand) -> AsynResult<()> {
        let axis = crate::devices::axis_at(&base.port_name, self.addr).ok_or_else(base_error)?;
        let c = crate::poll_loop::c_write(&command);
        run_on_axis(base, None, &axis, self.addr, &command, c)
    }
}

/// What a C subclass `writeInt32` / `writeFloat64` does around
/// `asynMotorController`'s, on every write the base runs: the controller
/// port's and a motor record's, whose base call runs on the axis's poller.
pub trait WriteHook: Send + Sync + 'static {
    /// The subclass's refusal before the base, as LC400's for an axis
    /// with no controller address: the base does not run. A port write the
    /// base hands to an axis is checked again there, so this depends only
    /// on its arguments.
    fn check(&self, _addr: i32, _function: usize) -> AsynResult<()> {
        Ok(())
    }

    /// After the write, with its status: on the controller port once its
    /// write returns, for a base call an axis runs once the driver has.
    fn after_write(&self, _write: &FinishedWrite<'_>) -> WriteOutcome {
        WriteOutcome::default()
    }

    /// Whether the subclass's write takes `function` itself and returns
    /// without calling the base, as omsBaseController's gains do
    /// (omsBaseController.cpp:361-406). The driver call stands for its
    /// branch, and the base prints none of its lines.
    fn handles(&self, _function: usize) -> bool {
        false
    }
}

/// The interfaces `asynMotorController` ORs into its subclass's
/// (asynMotorController.cpp:48).
pub const CONTROLLER_INTERFACES: InterfaceMask = InterfaceMask::OCTET
    .or(InterfaceMask::INT32)
    .or(InterfaceMask::FLOAT64)
    .or(InterfaceMask::FLOAT64_ARRAY)
    .or(InterfaceMask::GENERIC_POINTER)
    .or(InterfaceMask::DRV_USER);

/// The interrupts `asynMotorController` ORs into its subclass's
/// (asynMotorController.cpp:49).
pub const CONTROLLER_INTERRUPTS: InterfaceMask = InterfaceMask::OCTET
    .or(InterfaceMask::INT32)
    .or(InterfaceMask::FLOAT64)
    .or(InterfaceMask::FLOAT64_ARRAY)
    .or(InterfaceMask::GENERIC_POINTER);

/// What a C `asynMotorController` subclass adds to the base port.
pub trait ControllerParams: Send + Sync + 'static {
    /// The `interfaceMask` / `interruptMask` the C subclass hands its
    /// `asynMotorController` constructor. No default, as C's constructor
    /// argument has none.
    const INTERFACE_MASK: InterfaceMask;
    const INTERRUPT_MASK: InterfaceMask;

    /// The subclass constructor's `createParam` calls, after the base's.
    fn create_params(&mut self, _base: &mut PortDriverBase) -> AsynResult<()> {
        Ok(())
    }

    /// An address C answers for with an axis no motor record drives, such
    /// as pmac's controller axis 0. A base function written there runs no
    /// driver call, as C's `asynMotorAxis` defaults do nothing.
    fn is_controller_axis(&self, _addr: i32) -> bool {
        false
    }

    /// The subclass's code around the base on every path; see
    /// [`WriteHook`]. Asked once, when the port is created.
    fn write_hook(&self) -> Option<Arc<dyn WriteHook>> {
        None
    }

    /// The subclass `writeInt32`'s own code on the controller port, and
    /// whether it calls the base after.
    fn write_int32(
        &mut self,
        _base: &mut PortDriverBase,
        _user: AxisUser<'_>,
        _value: i32,
    ) -> ParamWrite {
        ParamWrite::PassToBase(Ok(()))
    }

    /// The subclass `writeFloat64`; see [`Self::write_int32`].
    fn write_float64(
        &mut self,
        _base: &mut PortDriverBase,
        _user: AxisUser<'_>,
        _value: f64,
    ) -> ParamWrite {
        ParamWrite::PassToBase(Ok(()))
    }

    /// The subclass `drvUserCreate`, as SPiiPlusController's `START_<name>`;
    /// an override calls [`PortDriverBase::drv_user_create`] as C calls the
    /// base. asynMotorController has none.
    fn drv_user_create(
        &mut self,
        base: &mut PortDriverBase,
        req: &DrvUserRequest,
    ) -> AsynResult<DrvUserInfo> {
        base.drv_user_create(req)
    }

    /// The subclass `readInt32`; an override calls
    /// [`PortDriverBase::read_int32_param`] as C calls the base.
    /// asynMotorController has none, so the base is asynPortDriver's.
    fn read_int32(&mut self, base: &mut PortDriverBase, user: AxisUser<'_>) -> AsynResult<i32> {
        base.read_int32_param(user.asyn())
    }

    /// The subclass `readFloat64`; see [`Self::read_int32`].
    fn read_float64(&mut self, base: &mut PortDriverBase, user: AxisUser<'_>) -> AsynResult<f64> {
        base.read_float64_param(user.asyn())
    }

    /// The subclass `writeOctet`, as omsBaseController's string commands;
    /// see [`Self::write_int32`]. asynMotorController has none, so the
    /// base is asynPortDriver's. A handled write reports every byte taken.
    fn write_octet(
        &mut self,
        _base: &mut PortDriverBase,
        _user: AxisUser<'_>,
        _data: &[u8],
    ) -> ParamWrite {
        ParamWrite::PassToBase(Ok(()))
    }

    /// The subclass `readOption`, as phytronController's poll and homing
    /// options; `None` reads the base's. asynMotorController has none.
    fn read_option(
        &self,
        _base: &PortDriverBase,
        _user: AxisUser<'_>,
        _key: &str,
    ) -> Option<AsynResult<String>> {
        None
    }

    /// The subclass `writeOption`; `None` passes the option to the base.
    fn write_option(
        &mut self,
        _base: &mut PortDriverBase,
        _user: AxisUser<'_>,
        _key: &str,
        _value: &str,
    ) -> Option<AsynResult<()>> {
        None
    }

    /// The subclass `writeUInt32Digital`; see [`Self::write_int32`].
    /// asynMotorController has none, so the base is asynPortDriver's.
    fn write_uint32_digital(
        &mut self,
        _base: &mut PortDriverBase,
        _user: AxisUser<'_>,
        _value: u32,
        _mask: u32,
    ) -> ParamWrite {
        ParamWrite::PassToBase(Ok(()))
    }

    /// The subclass `initializeProfile`; an override calls
    /// [`ControllerCtx::initialize_profile`] as C calls the base.
    fn initialize_profile(
        &mut self,
        ctx: &mut ControllerCtx<'_>,
        max_points: usize,
    ) -> AsynResult<()> {
        ctx.initialize_profile(max_points)
    }

    /// The subclass `buildProfile`; see [`Self::initialize_profile`].
    fn build_profile(&mut self, ctx: &mut ControllerCtx<'_>) -> AsynResult<()> {
        ctx.build_profile()
    }

    /// The subclass `abortProfile`; see [`Self::initialize_profile`].
    fn abort_profile(&mut self, ctx: &mut ControllerCtx<'_>) -> AsynResult<()> {
        ctx.abort_profile()
    }

    /// The subclass `readbackProfile`; see [`Self::initialize_profile`].
    fn readback_profile(&mut self, ctx: &mut ControllerCtx<'_>) -> AsynResult<()> {
        ctx.readback_profile()
    }

    /// The subclass `writeFloat64Array`; an override calls
    /// [`ControllerCtx::write_float64_array`] for what it leaves to the base.
    fn write_float64_array(
        &mut self,
        ctx: &mut ControllerCtx<'_>,
        user: AxisUser<'_>,
        value: &[f64],
    ) -> AsynResult<()> {
        ctx.write_float64_array(self, user, value)
    }

    /// The axis subclass's `defineProfile`; an override calls
    /// [`ControllerCtx::define_profile`] as C calls the base.
    fn define_profile(
        &mut self,
        ctx: &mut ControllerCtx<'_>,
        addr: i32,
        positions: &[f64],
    ) -> AsynResult<()> {
        ctx.define_profile(addr, positions)
    }

    /// The subclass `executeProfile`; see [`Self::initialize_profile`].
    fn execute_profile(&mut self, ctx: &mut ControllerCtx<'_>) -> AsynResult<()> {
        ctx.execute_profile()
    }

    /// The subclass `report(fp, level)`; an override calls
    /// [`ControllerReport::report`] as C calls the base.
    fn report(&self, ctx: &ControllerReport<'_>, out: &mut dyn std::fmt::Write, level: i32) {
        ctx.report(out, level);
    }
}

/// What C `asynMotorController::report` reaches: the port and its axes.
pub struct ControllerReport<'a> {
    port: &'a dyn PortDriver,
}

impl ControllerReport<'_> {
    pub fn port_name(&self) -> &str {
        &self.port.base().port_name
    }

    /// C `numAxes_`.
    pub fn num_axes(&self) -> usize {
        self.port.base().max_addr
    }

    /// C `getDoubleParam(addr, reason, &value)`.
    pub fn float64_param(&self, addr: i32, reason: usize) -> AsynResult<f64> {
        self.port.base().get_float64_param(reason, addr)
    }

    /// C `getIntegerParam(addr, reason, &value)`.
    pub fn int32_param(&self, addr: i32, reason: usize) -> AsynResult<i32> {
        self.port.base().get_int32_param(reason, addr)
    }

    /// C `asynMotorController::report` (asynMotorController.cpp:164-177):
    /// each axis's report, then `asynPortDriver::report`.
    pub fn report(&self, out: &mut dyn std::fmt::Write, level: i32) {
        for addr in 0..self.num_axes() as i32 {
            if let Some(axis) = crate::devices::axis_at(self.port_name(), addr) {
                axis.report(out, level);
            }
        }
        self.port.asyn_port_driver_report(out, level);
    }
}

/// A controller with no parameters of its own, whose C constructor passes
/// masks of 0.
impl ControllerParams for () {
    const INTERFACE_MASK: InterfaceMask = InterfaceMask::NONE;
    const INTERRUPT_MASK: InterfaceMask = InterfaceMask::NONE;
}

/// C's profile arrays: the controller's `profileTimes_` and, by address,
/// each axis's `profilePositions_`, `profileReadbacks_` and
/// `profileFollowingErrors_`.
#[derive(Default)]
pub struct Profile {
    pub max_points: usize,
    pub times: Vec<f64>,
    pub axes: Vec<AxisProfile>,
}

#[derive(Default, Clone)]
pub struct AxisProfile {
    pub positions: Vec<f64>,
    pub readbacks: Vec<f64>,
    pub following_errors: Vec<f64>,
}

/// What the base `asynMotorController` functions reach: the parameter
/// library, the profile arrays and the axes C's `getAxis(i)` finds.
pub struct ControllerCtx<'a> {
    pub base: &'a mut PortDriverBase,
    pub profile: &'a mut Profile,
    /// `(address, axis)` for each address with an axis, `None` at a
    /// controller axis.
    axes: Vec<(i32, Option<MotorAxis>)>,
}

impl ControllerCtx<'_> {
    /// The addresses C's `for (axis=0; axis<numAxes_; axis++)` loops find
    /// an axis at.
    pub fn axes(&self) -> impl Iterator<Item = i32> + '_ {
        self.axes.iter().map(|&(addr, _)| addr)
    }

    /// Each motor axis's driver: the `asynMotorAxis` virtual a base loop
    /// calls, whose status C drops.
    fn send_to_axes(&mut self, command: MotorCommand) {
        for (addr, axis) in &self.axes {
            if let Some(axis) = axis {
                let _ = run_on_axis(self.base, None, axis, *addr, &command, None);
            }
        }
    }

    /// C `asynMotorController::wakeupPoller`: every axis's poller passes
    /// now and polls at the moving rate for its forced fast polls.
    pub fn wakeup_poller(&self) {
        for axis in self.axes.iter().filter_map(|(_, axis)| axis.as_ref()) {
            axis.wakeup();
        }
    }

    /// C `asynMotorController::initializeProfile` (asynMotorController.cpp:
    /// 826-840) with each axis's `initializeProfile`: zeroed arrays of
    /// `max_points`.
    pub fn initialize_profile(&mut self, max_points: usize) -> AsynResult<()> {
        let zeroed = vec![0.0; max_points];
        self.profile.max_points = max_points;
        self.profile.times = zeroed.clone();
        let axis = AxisProfile {
            positions: zeroed.clone(),
            readbacks: zeroed.clone(),
            following_errors: zeroed,
        };
        self.profile.axes = vec![axis; self.base.max_addr];
        self.send_to_axes(MotorCommand::ProfileInitialize { max_points });
        Ok(())
    }

    /// C `asynMotorController::buildProfile` (asynMotorController.cpp:
    /// 844-870): in fixed time mode every point takes PROFILE_FIXED_TIME.
    pub fn build_profile(&mut self) -> AsynResult<()> {
        let time_mode = self.base.get_int32_param(index::PROFILE_TIME_MODE, 0);
        let time = self.base.get_float64_param(index::PROFILE_FIXED_TIME, 0);
        let num_points = self.base.get_int32_param(index::PROFILE_NUM_POINTS, 0);
        let (Ok(time_mode), Ok(time), Ok(num_points)) = (time_mode, time, num_points) else {
            return Err(base_error());
        };
        if time_mode == PROFILE_TIME_MODE_FIXED {
            // C writes past profileTimes_ when NUM_POINTS exceeds it.
            let n = (num_points.max(0) as usize).min(self.profile.times.len());
            self.profile.times.fill(0.0);
            self.profile.times[..n].fill(time);
        }
        self.send_to_axes(MotorCommand::ProfileBuild);
        Ok(())
    }

    /// C `asynMotorController::abortProfile` (asynMotorController.cpp:
    /// 888-900).
    pub fn abort_profile(&mut self) -> AsynResult<()> {
        self.send_to_axes(MotorCommand::ProfileAbort);
        Ok(())
    }

    /// C `asynMotorController::readbackProfile` (asynMotorController.cpp:
    /// 903-915), each axis's base conversion beside its driver's call.
    pub fn readback_profile(&mut self) -> AsynResult<()> {
        for addr in self.axes().collect::<Vec<_>>() {
            // C drops the axis's status.
            let _ = self.axis_readback_profile(addr);
        }
        self.send_to_axes(MotorCommand::ProfileReadback);
        Ok(())
    }

    /// C `asynMotorController::writeFloat64Array` (asynMotorController.cpp:
    /// 436-461): PROFILE_TIME_ARRAY into the times, PROFILE_POSITIONS through
    /// the axis's `defineProfile`, both cut to the profile's points.
    pub fn write_float64_array<P: ControllerParams + ?Sized>(
        &mut self,
        params: &mut P,
        user: AxisUser<'_>,
        value: &[f64],
    ) -> AsynResult<()> {
        let addr = user.addr;
        if !self.axes().any(|a| a == addr) {
            return Err(base_error());
        }
        let value = &value[..value.len().min(self.profile.max_points)];
        match user.reason {
            index::PROFILE_TIME_ARRAY => self.profile.times[..value.len()].copy_from_slice(value),
            index::PROFILE_POSITIONS => {
                // C drops the axis's status.
                if params.define_profile(self, addr, value).is_ok() {
                    self.send_profile(addr, value.len());
                }
            }
            function => {
                self.base.trace_print(
                    TraceMask::ERROR,
                    format_args!(
                        "asynMotorController:writeFloat64Array: unknown parameter number {function}\n"
                    ),
                );
                return Err(base_error());
            }
        }
        Ok(())
    }

    /// The axis driver's `defineProfile` with the first `n` converted
    /// positions.
    fn send_profile(&mut self, addr: i32, n: usize) {
        let axis = self.axes.iter().find(|(a, _)| *a == addr);
        let positions = self.profile.axes.get(addr as usize).map(|p| &p.positions);
        if let (Some((_, Some(axis))), Some(positions)) = (axis, positions) {
            let command = MotorCommand::ProfileDefine {
                positions: positions[..n.min(positions.len())].to_vec(),
            };
            // C drops the axis's status.
            let _ = run_on_axis(self.base, None, axis, addr, &command, None);
        }
    }

    /// C `asynMotorAxis::defineProfile` (asynMotorAxis.cpp:346-380) for the
    /// axis at `addr`: `positions` from user to controller units.
    pub fn define_profile(&mut self, addr: i32, positions: &[f64]) -> AsynResult<()> {
        if positions.len() > self.profile.max_points {
            return Err(base_error());
        }
        let (resolution, offset, direction) = self.record_units(addr)?;
        if resolution == 0.0 {
            return Err(base_error());
        }
        let scale = if direction != 0 {
            -1.0 / resolution
        } else {
            1.0 / resolution
        };
        if let Some(axis) = self.profile.axes.get_mut(addr as usize) {
            for (out, p) in axis.positions.iter_mut().zip(positions) {
                *out = (p - offset) * scale;
            }
        }
        Ok(())
    }

    /// The axis's MOTOR_REC_RESOLUTION, MOTOR_REC_OFFSET and
    /// MOTOR_REC_DIRECTION, as C's profile conversions read them.
    fn record_units(&self, addr: i32) -> AsynResult<(f64, f64, i32)> {
        let resolution = self.base.get_float64_param(index::REC_RESOLUTION, addr);
        let offset = self.base.get_float64_param(index::REC_OFFSET, addr);
        let direction = self.base.get_int32_param(index::REC_DIRECTION, addr);
        match (resolution, offset, direction) {
            (Ok(r), Ok(o), Ok(d)) => Ok((r, o, d)),
            _ => Err(base_error()),
        }
    }

    /// C `asynMotorAxis::readbackProfile` (asynMotorAxis.cpp:421-446) for
    /// the axis at `addr`: the first PROFILE_NUM_READBACKS readbacks and
    /// following errors from controller to user units, in place, then
    /// posted. A driver converting its own units first calls this after.
    pub fn axis_readback_profile(&mut self, addr: i32) -> AsynResult<()> {
        let (mut resolution, offset, direction) = self.record_units(addr)?;
        let num_readbacks = self
            .base
            .get_int32_param(index::PROFILE_NUM_READBACKS, 0)
            .map_err(|_| base_error())?;
        if direction != 0 {
            resolution = -resolution;
        }
        // C indexes arrays initializeProfile never made, or made shorter.
        let Some(axis) = self.profile.axes.get_mut(addr as usize) else {
            return Ok(());
        };
        let n = (num_readbacks.max(0) as usize).min(axis.readbacks.len());
        for (r, f) in axis.readbacks[..n]
            .iter_mut()
            .zip(&mut axis.following_errors[..n])
        {
            *r = *r * resolution + offset;
            *f *= resolution;
        }
        // C drops both statuses.
        for (reason, array) in [
            (index::PROFILE_READBACKS, &axis.readbacks[..n]),
            (index::PROFILE_FOLLOWING_ERRORS, &axis.following_errors[..n]),
        ] {
            let _ =
                self.base
                    .do_callbacks_array(reason, addr, ParamValue::Float64Array(array.into()));
        }
        Ok(())
    }

    /// C `asynMotorController::executeProfile` (asynMotorController.cpp:
    /// 873-885).
    pub fn execute_profile(&mut self) -> AsynResult<()> {
        self.send_to_axes(MotorCommand::ProfileExecute);
        Ok(())
    }
}

/// The bare `asynError` a C base function returns, with no errorMessage,
/// as each does where `getAxis` finds no axis
/// (asynMotorController.cpp:199-200, :532-556).
fn base_error() -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message: String::new(),
    }
}

/// C `getAxis(pasynUser)` for a driver's own write code, which now runs
/// at any address: whether a motor axis sits at the user's. A
/// [`ControllerParams::is_controller_axis`] is the driver's to add.
pub fn has_motor_axis(base: &PortDriverBase, user: AxisUser<'_>) -> bool {
    crate::devices::axis_at(&base.port_name, user.addr).is_some()
}

/// The asyn driver behind a controller port.
pub struct ControllerPort<P> {
    base: PortDriverBase,
    params: P,
    profile: Profile,
    hook: Option<Arc<dyn WriteHook>>,
}

impl<P: ControllerParams> ControllerPort<P> {
    /// C's constructor: `num_axes` addresses, the base parameters, then the
    /// driver's.
    pub fn new(port: &str, num_axes: i32, mut params: P) -> AsynResult<Self> {
        let flags = PortFlags {
            multi_device: true,
            can_block: true,
            ..PortFlags::default()
        };
        let mut base = PortDriverBase::new(
            port,
            num_axes.max(1) as usize,
            flags,
            P::INTERFACE_MASK | CONTROLLER_INTERFACES,
            P::INTERRUPT_MASK | CONTROLLER_INTERRUPTS,
        );
        for &(name, param_type) in BASE_PARAMS {
            base.create_param(name, param_type)?;
        }
        // A profile record reads the execute state before any run.
        base.set_int32_param(index::PROFILE_EXECUTE_STATE, 0, PROFILE_EXECUTE_DONE)?;
        params.create_params(&mut base)?;
        let hook = params.write_hook();
        Ok(Self {
            base,
            params,
            profile: Profile::default(),
            hook,
        })
    }

    /// The driver's parameters and the base functions' context, for a
    /// driver's own entry such as an iocsh profile command.
    pub fn with_params<R>(&mut self, f: impl FnOnce(&mut P, &mut ControllerCtx<'_>) -> R) -> R {
        let port = &self.base.port_name;
        let axes = (0..self.base.max_addr as i32)
            .filter_map(|addr| match crate::devices::axis_at(port, addr) {
                Some(axis) => Some((addr, Some(axis))),
                None => self.params.is_controller_axis(addr).then_some((addr, None)),
            })
            .collect();
        let mut ctx = ControllerCtx {
            base: &mut self.base,
            profile: &mut self.profile,
            axes,
        };
        f(&mut self.params, &mut ctx)
    }

    /// C `initializeProfile(maxPoints)`, through the driver's override.
    pub fn initialize_profile(&mut self, max_points: usize) -> AsynResult<()> {
        self.with_params(|params, ctx| params.initialize_profile(ctx, max_points))
    }

    /// C `getAxis(pasynUser)`: the motor axis at the user's address, `None`
    /// at a controller axis, an error where the controller has no axis.
    fn axis(&self, user: &AsynUser) -> AsynResult<(i32, Option<MotorAxis>)> {
        self.find_axis(self.base.get_address(user.addr, user.reason)?)
    }

    /// [`Self::axis`] at `getAddress`'s address.
    fn find_axis(&self, addr: i32) -> AsynResult<(i32, Option<MotorAxis>)> {
        if let Some(axis) = crate::devices::axis_at(&self.base.port_name, addr) {
            return Ok((addr, Some(axis)));
        }
        if self.params.is_controller_axis(addr) {
            return Ok((addr, None));
        }
        Err(base_error())
    }

    /// A C subclass write at an address with no axis: its own code has run,
    /// and the base it calls returns asynError from `getAxis` before
    /// storing anything.
    fn no_axis_write(&self, own: ParamWrite, refused: AsynError) -> AsynResult<()> {
        match own {
            ParamWrite::Handled(status) => status,
            ParamWrite::PassToBase(own) => own.and(Err(refused)),
        }
    }

    fn float(&self, index: usize, addr: i32) -> f64 {
        self.base.get_float64_param(index, addr).unwrap_or(0.0)
    }

    /// The base `writeInt32`'s driver call for `function`, if any.
    fn int32_command(&self, addr: i32, function: usize, value: i32) -> Option<MotorCommand> {
        Some(match function {
            index::STOP => MotorCommand::Stop {
                acceleration: self.float(index::ACCEL, addr),
            },
            index::DEFER_MOVES => MotorCommand::DeferMoves { defer: value != 0 },
            index::CLOSED_LOOP => MotorCommand::SetClosedLoop { enable: value != 0 },
            index::MOVE_TO_HOME if value == 1 => MotorCommand::MoveToHome,
            // C polls inside the write, before its callbacks and closing
            // line (asynMotorController.cpp:217-221, :245-254).
            index::UPDATE_STATUS => MotorCommand::Poll,
            _ => return None,
        })
    }

    /// The base `writeFloat64`'s driver call for `function`; a move reads
    /// its speeds from the parameters in its own arm, as C does
    /// (asynMotorController.cpp:291-350), so no other write reports them
    /// undefined.
    fn float64_command(&self, addr: i32, function: usize, value: f64) -> Option<MotorCommand> {
        let speeds = || {
            (
                self.float(index::VEL_BASE, addr),
                self.float(index::VELOCITY, addr),
                self.float(index::ACCEL, addr),
            )
        };
        let gain = |kind| MotorCommand::SetPidGain { kind, gain: value };
        let command = match function {
            index::MOVE_REL => {
                let (min_velocity, velocity, acceleration) = speeds();
                MotorCommand::MoveRelative {
                    distance: value,
                    min_velocity,
                    velocity,
                    acceleration,
                }
            }
            index::MOVE_ABS => {
                let (min_velocity, velocity, acceleration) = speeds();
                MotorCommand::MoveAbsolute {
                    position: value,
                    min_velocity,
                    velocity,
                    acceleration,
                }
            }
            index::MOVE_VEL => MotorCommand::MoveVelocity {
                min_velocity: self.float(index::VEL_BASE, addr),
                velocity: value,
                acceleration: self.float(index::ACCEL, addr),
            },
            index::HOME => {
                let (min_velocity, velocity, acceleration) = speeds();
                MotorCommand::Home {
                    forward: value != 0.0,
                    min_velocity,
                    velocity,
                    acceleration,
                }
            }
            index::POSITION => MotorCommand::SetPosition { position: value },
            index::ENCODER_POSITION => MotorCommand::SetEncoderPosition { position: value },
            index::HIGH_LIMIT => MotorCommand::SetHighLimit { position: value },
            index::LOW_LIMIT => MotorCommand::SetLowLimit { position: value },
            index::PGAIN => gain(PidGainKind::Proportional),
            index::IGAIN => gain(PidGainKind::Integral),
            index::DGAIN => gain(PidGainKind::Derivative),
            index::ENCODER_RATIO => MotorCommand::SetEncoderRatio {
                pulses: [value, 1.0],
            },
            _ => return None,
        };
        Some(command)
    }

    /// C's closing `asynPrint` of a base write (asynMotorController.cpp:
    /// 247-254, :419-426), through the writer's user; none for one the
    /// subclass [`WriteHook::handles`].
    fn print_write(
        &self,
        addr: i32,
        function: usize,
        method: &str,
        status: &AsynResult<()>,
        value: std::fmt::Arguments<'_>,
    ) {
        if self
            .hook
            .as_ref()
            .is_some_and(|hook| hook.handles(function))
        {
            return;
        }
        let print = |mask, args: std::fmt::Arguments<'_>| {
            PortManager::global().trace_manager().print(
                Some(&self.base.port_name),
                Some(addr),
                function as i32,
                mask,
                args,
            )
        };
        match status {
            Err(e) => print(
                TraceMask::ERROR,
                format_args!(
                    "asynMotorController:{method} error, status={} axis={addr}, function={function}, value={value}\n",
                    e.c_status_code()
                ),
            ),
            Ok(()) => print(
                TraceMask::IO_DRIVER,
                format_args!(
                    "asynMotorController:{method}:: axis={addr}, function={function}, value={value}\n"
                ),
            ),
        }
    }
}

impl<P: ControllerParams> ControllerPort<P> {
    /// The driver's own code, its hook's check, then the base `writeInt32`.
    fn write_int32_base(
        &mut self,
        user: AxisUser<'_>,
        addr: i32,
        axis: Option<MotorAxis>,
        value: i32,
    ) -> AsynResult<()> {
        let own = match self.params.write_int32(&mut self.base, user, value) {
            ParamWrite::PassToBase(own) => own,
            ParamWrite::Handled(status) => return status,
        };
        if let Some(Err(e)) = self.hook.as_ref().map(|h| h.check(addr, user.reason)) {
            return Err(e);
        }
        let _ = self.base.set_int32_param(user.reason, addr, value);
        let status = match (axis.as_ref(), self.int32_command(addr, user.reason, value)) {
            (Some(axis), Some(command)) => {
                let c = Some((user.reason as i32, WriteValue::Int32(value)));
                run_on_axis(&mut self.base, self.hook.as_ref(), axis, addr, &command, c)
            }
            (axis, _) => {
                let status = match user.reason {
                    index::PROFILE_BUILD => {
                        self.with_params(|params, ctx| params.build_profile(ctx))
                    }
                    index::PROFILE_ABORT => {
                        self.with_params(|params, ctx| params.abort_profile(ctx))
                    }
                    index::PROFILE_READBACK => {
                        self.with_params(|params, ctx| params.readback_profile(ctx))
                    }
                    index::PROFILE_EXECUTE => {
                        self.with_params(|params, ctx| params.execute_profile(ctx))
                    }
                    _ => Ok(()),
                };
                call_back_axis(axis);
                status
            }
        };
        let _ = self.base.call_param_callbacks(addr);
        self.print_write(
            addr,
            user.reason,
            "writeInt32",
            &status,
            format_args!("{value}"),
        );
        own.and(status)
    }

    /// The driver's own code, its hook's check, then the base
    /// `writeFloat64`.
    fn write_float64_base(
        &mut self,
        user: AxisUser<'_>,
        addr: i32,
        axis: Option<MotorAxis>,
        value: f64,
    ) -> AsynResult<()> {
        let own = match self.params.write_float64(&mut self.base, user, value) {
            ParamWrite::PassToBase(own) => own,
            ParamWrite::Handled(status) => return status,
        };
        if let Some(Err(e)) = self.hook.as_ref().map(|h| h.check(addr, user.reason)) {
            return Err(e);
        }
        let _ = self.base.set_float64_param(user.reason, addr, value);
        let status = match (
            axis.as_ref(),
            self.float64_command(addr, user.reason, value),
        ) {
            (Some(axis), Some(command)) => {
                let c = Some((user.reason as i32, WriteValue::Float64(value)));
                run_on_axis(&mut self.base, self.hook.as_ref(), axis, addr, &command, c)
            }
            (axis, _) => {
                call_back_axis(axis);
                Ok(())
            }
        };
        let _ = self.base.call_param_callbacks(addr);
        self.print_write(
            addr,
            user.reason,
            "writeFloat64",
            &status,
            format_args!("{:.6}", CFloat(value)),
        );
        own.and(status)
    }

    /// The hook's code after the base's write returned `status`.
    fn after_write(
        &mut self,
        addr: i32,
        function: usize,
        value: WriteValue,
        status: AsynResult<()>,
    ) -> AsynResult<()> {
        let Some(hook) = &self.hook else {
            return status;
        };
        let outcome = {
            let print = |mask, args: std::fmt::Arguments<'_>| {
                PortManager::global().trace_manager().print(
                    Some(&self.base.port_name),
                    Some(addr),
                    function as i32,
                    mask,
                    args,
                )
            };
            hook.after_write(&FinishedWrite::new(addr, function, value, &status, &print))
        };
        // Set after the base's callParamCallbacks, as C's are: the next one
        // posts them.
        self.store(addr, outcome.delta);
        outcome.status.unwrap_or(status)
    }

    /// C's `setIntegerParam` / `setDoubleParam` of `delta` on axis `addr`
    /// with no `callParamCallbacks`: whichever runs next on the axis posts
    /// them, the port's or the axis poller's.
    fn store(&mut self, addr: i32, delta: ParamDelta) {
        for (function, value) in delta.into_values() {
            let _ = self.base.set_param_value(function, addr, value);
        }
    }

    /// The driver's [`WriteHook`], for the axes' pollers.
    pub(crate) fn write_hook(&self) -> Option<Arc<dyn WriteHook>> {
        self.hook.clone()
    }
}

impl<P: ControllerParams> PortDriver for ControllerPort<P> {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn report(&mut self, out: &mut dyn std::fmt::Write, level: i32) {
        let this: &Self = self;
        this.params
            .report(&ControllerReport { port: this }, out, level);
    }

    fn drv_user_create(&mut self, req: &DrvUserRequest) -> AsynResult<DrvUserInfo> {
        self.params.drv_user_create(&mut self.base, req)
    }

    fn read_int32(&mut self, user: &AsynUser) -> AsynResult<i32> {
        let user = AxisUser::new(&self.base, user)?;
        self.params.read_int32(&mut self.base, user)
    }

    fn read_float64(&mut self, user: &AsynUser) -> AsynResult<f64> {
        let user = AxisUser::new(&self.base, user)?;
        self.params.read_float64(&mut self.base, user)
    }

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        let user = AxisUser::new(&self.base, user)?;
        let (addr, axis) = match self.find_axis(user.addr) {
            Ok(found) => found,
            Err(refused) => {
                let own = self.params.write_int32(&mut self.base, user, value);
                return self.no_axis_write(own, refused);
            }
        };
        let status = self.write_int32_base(user, addr, axis, value);
        self.after_write(addr, user.reason, WriteValue::Int32(value), status)
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        let user = AxisUser::new(&self.base, user)?;
        let (addr, axis) = match self.find_axis(user.addr) {
            Ok(found) => found,
            Err(refused) => {
                let own = self.params.write_float64(&mut self.base, user, value);
                return self.no_axis_write(own, refused);
            }
        };
        let status = self.write_float64_base(user, addr, axis, value);
        self.after_write(addr, user.reason, WriteValue::Float64(value), status)
    }

    fn write_uint32_digital(
        &mut self,
        user: &mut AsynUser,
        value: u32,
        mask: u32,
    ) -> AsynResult<()> {
        let axis_user = AxisUser::new(&self.base, user)?;
        match self
            .params
            .write_uint32_digital(&mut self.base, axis_user, value, mask)
        {
            ParamWrite::Handled(status) => status,
            ParamWrite::PassToBase(own) => {
                let status = self.base.write_uint32_digital_param(user, value, mask);
                own.and(status)
            }
        }
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        let axis_user = AxisUser::new(&self.base, user)?;
        match self.params.write_octet(&mut self.base, axis_user, data) {
            ParamWrite::Handled(status) => status.map(|()| data.len()),
            ParamWrite::PassToBase(own) => {
                let written = self.base.write_octet(user, data);
                own.and(written)
            }
        }
    }

    fn get_option(&self, user: &AsynUser, key: &str) -> AsynResult<String> {
        let axis_user = AxisUser::new(&self.base, user)?;
        self.params
            .read_option(&self.base, axis_user, key)
            .unwrap_or_else(|| self.base.read_option(user, key))
    }

    fn set_option(&mut self, user: &mut AsynUser, key: &str, value: &str) -> AsynResult<()> {
        let axis_user = AxisUser::new(&self.base, user)?;
        if let Some(written) = self
            .params
            .write_option(&mut self.base, axis_user, key, value)
        {
            return written;
        }
        self.base.write_option(user, key, value)
    }

    fn write_float64_array(&mut self, user: &AsynUser, data: &[f64]) -> AsynResult<()> {
        let user = AxisUser::new(&self.base, user)?;
        self.with_params(|params, ctx| params.write_float64_array(ctx, user, data))
    }

    /// C `asynMotorController::readFloat64Array` (asynMotorController.cpp:
    /// 469-497): the axis's first PROFILE_NUM_READBACKS readbacks or
    /// following errors.
    fn read_float64_array(&mut self, user: &AsynUser, buf: &mut [f64]) -> AsynResult<usize> {
        let (addr, _) = self.axis(user)?;
        // C reads an uninitialized count when the parameter is undefined.
        let num_readbacks = self
            .base
            .get_int32_param(index::PROFILE_NUM_READBACKS, 0)
            .unwrap_or(0);
        let axis = self.profile.axes.get(addr as usize);
        let array = match user.reason {
            index::PROFILE_READBACKS => axis.map(|a| &a.readbacks[..]),
            index::PROFILE_FOLLOWING_ERRORS => axis.map(|a| &a.following_errors[..]),
            function => {
                self.base.trace_print(
                    TraceMask::ERROR,
                    format_args!(
                        "asynMotorController:readFloat64Array: unknown parameter number {function}\n"
                    ),
                );
                return Err(base_error());
            }
        }
        .unwrap_or_default();
        // C copies past arrays initializeProfile never made, or made shorter.
        let n = (num_readbacks.max(0) as usize)
            .min(buf.len())
            .min(array.len());
        buf[..n].copy_from_slice(&array[..n]);
        Ok(n)
    }
}

/// C's base calling the axis driver for `command` on the port thread, under
/// the controller's lock as C's: its status is the
/// write's. What the call set on the axis lands in `base` and goes to the
/// axis's poller to post; a move wakes the poller (asynMotorController.cpp:
/// 291-355). `c` is the base function and value the write stored.
fn run_on_axis(
    base: &mut PortDriverBase,
    hook: Option<&Arc<dyn WriteHook>>,
    axis: &MotorAxis,
    addr: i32,
    command: &MotorCommand,
    c: Option<(i32, WriteValue)>,
) -> AsynResult<()> {
    let trace = MotorTrace::with_user(axis.address().clone());
    let quiet =
        hook.is_some_and(|hook| c.is_some_and(|(function, _)| hook.handles(function as usize)));
    let moves = command.is_move() && !matches!(command, MotorCommand::SetPosition { .. });
    // C writeFloat64 powers the axis on before a move and waits out the
    // delay under the lock (asynMotorController.cpp:291-295).
    let on_delay = moves
        .then(|| {
            let on = base.get_int32_param(index::POWER_AUTO_ONOFF, addr);
            (on.unwrap_or(0) == 1).then(|| {
                base.get_float64_param(index::POWER_ON_DELAY, addr)
                    .unwrap_or(0.0)
            })
        })
        .flatten()
        .map(|secs| std::time::Duration::try_from_secs_f64(secs).unwrap_or_default());
    let held = axis.controller_poll().hold();
    axis.controller_poll().drain_on_port(&held, base);
    // UPDATE_STATUS runs `poll()` before the axis's (:217-221).
    if matches!(command, MotorCommand::Poll) {
        apply_port_sets(base, None, axis.controller_poll().run(&held));
    }
    let driven = {
        let port = base.port_name.clone();
        let Some(mut motor) = axis
            .motor()
            .and_then(|motor| crate::poll_loop::lock_axis(motor, Some(&port), &held))
        else {
            return Err(base_error());
        };
        if let Some(delay) = on_delay {
            let _ = motor.set_closed_loop(&AsynUser::new(0), true);
            std::thread::sleep(delay);
        }
        crate::poll_loop::drive(&mut *motor, &trace, command, c, quiet, axis.records())
    };
    if matches!(command, MotorCommand::MoveToHome) {
        axis.controller_poll().signal_move_to_home(addr);
    }
    // Queued before the lock goes, so a poller pass waiting on it posts
    // them before its poll, as C's write posts before it unlocks.
    apply_port_sets(base, Some(addr), driven.port_sets);
    axis.post_written(Written::new(driven.write, on_delay.is_some()));
    if moves {
        axis.wakeup();
    }
    drop(held);
    // C axis methods get the controller's asynUser, so the writer's
    // errorMessage is left as it was: empty.
    driven
        .result
        .map_err(|e| AsynError::from_c_status_code(e.c_status_code(), String::new()).unwrap_or(e))
}

/// C's `pAxis->callParamCallbacks()` ending a write that ran no axis
/// method (asynMotorController.cpp:246, :418): it posts a status a bailing
/// poll left.
fn call_back_axis(axis: Option<&MotorAxis>) {
    if let Some(axis) = axis {
        axis.post_written(Written::new(AxisWrite::default(), false));
    }
}

/// A driver's own-port steps on the port's thread, as C makes them: on the
/// parameter library itself. `axis` is the address a [`PortSet::Axis`]
/// lands on, None for a controller poll's.
pub(crate) fn apply_port_sets(
    base: &mut PortDriverBase,
    axis: Option<i32>,
    sets: impl IntoIterator<Item = PortSet>,
) {
    for set in sets {
        let _ = match set {
            PortSet::Set {
                addr,
                reason,
                value,
            } => base.set_param_value(reason, addr, value),
            PortSet::Axis { reason, value } => match axis {
                Some(addr) => base.set_param_value(reason, addr, value),
                None => Ok(()),
            },
            PortSet::Callbacks(addr) => base.call_param_callbacks(addr),
            other => {
                crate::poll_loop::apply_axis_set(&base.port_name, other);
                Ok(())
            }
        };
    }
}

/// What an axis's poller reads the port's parameters through: C's
/// getIntegerParam / getDoubleParam in one call on the port's actor, so a
/// read prints the parameter library's report and no readInt32/readFloat64
/// interface line.
#[derive(Clone)]
pub(crate) struct ParamRead(Arc<dyn Fn(ParamCall) -> AsynResult<()> + Send + Sync>);

type ParamCall = Box<dyn FnOnce(&PortDriverBase) + Send>;

impl ParamRead {
    /// `read` on the parameter library; `None` once the port is gone.
    pub fn read<R: Send + 'static>(
        &self,
        read: impl FnOnce(&PortDriverBase) -> R + Send + 'static,
    ) -> Option<R> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        (self.0)(Box::new(move |base| {
            let _ = tx.send(read(base));
        }))
        .ok()?;
        rx.try_recv().ok()
    }
}

/// A published controller port, as the axes' pollers reach it.
pub(crate) struct Published {
    /// The driver's, for posting the values its poll reads; the parameter
    /// library the base writes as on the port.
    pub handle: PortHandle,
    pub hook: Option<Arc<dyn WriteHook>>,
    pub params: ParamRead,
}

/// Publish controller port `port` with axes `0..num_axes` and the driver's
/// `params`, gated on `owner` (the IOC's interruptAccept).
pub(crate) fn publish<P: ControllerParams>(
    owner: &Arc<InterruptGate>,
    port: &str,
    num_axes: i32,
    params: P,
) -> Result<Published, String> {
    let claim = claim_port(port).map_err(|e| format!("motor port {port}: {e}"))?;
    let driver = ControllerPort::new(port, num_axes, params)
        .map_err(|e| format!("motor port {port}: {e}"))?;
    let hook = driver.write_hook();
    let (runtime, _actor) = create_port_runtime(&claim, driver, RuntimeConfig::default())
        .map_err(|e| format!("motor port {port}: {e}"))?;
    let handle = runtime.port_handle().clone();
    if let Err(e) = claim.publish(handle.clone(), owner) {
        runtime.shutdown();
        return Err(format!("motor port {port}: {e}"));
    }
    let port = handle.clone();
    let params = ParamRead(Arc::new(move |read: ParamCall| {
        port.with_driver_blocking::<ControllerPort<P>, _>(move |driver| read(&driver.base))
    }));
    Ok(Published {
        handle,
        hook,
        params,
    })
}
