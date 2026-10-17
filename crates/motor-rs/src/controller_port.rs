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
//! A motor record's writes reach the same C subclass write, but here go to
//! the axis's poller; what the subclass does around the base on both paths
//! is its [`WriteHook`].

use std::sync::Arc;

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::interfaces::InterfaceType;
use asyn_rs::manager::PortManager;
use asyn_rs::param::{ParamType, ParamValue};
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::port_handle::PortHandle;
use asyn_rs::registry::claim_port;
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime;
use asyn_rs::trace::TraceMask;
use asyn_rs::user::AsynUser;
use epics_base_rs::runtime::interrupt_accept::InterruptGate;

use crate::device_support::MotorAxis;
use crate::flags::{MotorCommand, PidGainKind};

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

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
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
}

/// What a C `asynMotorController` subclass adds to the base port.
pub trait ControllerParams: Send + Sync + 'static {
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
        _user: &AsynUser,
        _value: i32,
    ) -> ParamWrite {
        ParamWrite::PassToBase(Ok(()))
    }

    /// The subclass `writeFloat64`; see [`Self::write_int32`].
    fn write_float64(
        &mut self,
        _base: &mut PortDriverBase,
        _user: &AsynUser,
        _value: f64,
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
        user: &AsynUser,
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

/// A controller with no parameters of its own.
impl ControllerParams for () {}

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
    /// calls.
    fn send_to_axes(&self, command: impl Fn() -> MotorCommand) {
        for axis in self.axes.iter().filter_map(|(_, axis)| axis.as_ref()) {
            axis.send(command());
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
        self.send_to_axes(|| MotorCommand::ProfileInitialize { max_points });
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
        self.send_to_axes(|| MotorCommand::ProfileBuild);
        Ok(())
    }

    /// C `asynMotorController::abortProfile` (asynMotorController.cpp:
    /// 888-900).
    pub fn abort_profile(&mut self) -> AsynResult<()> {
        self.send_to_axes(|| MotorCommand::ProfileAbort);
        Ok(())
    }

    /// C `asynMotorController::readbackProfile` (asynMotorController.cpp:
    /// 903-915), each axis's base conversion beside its driver's call.
    pub fn readback_profile(&mut self) -> AsynResult<()> {
        for addr in self.axes().collect::<Vec<_>>() {
            // C drops the axis's status.
            let _ = self.axis_readback_profile(addr);
        }
        self.send_to_axes(|| MotorCommand::ProfileReadback);
        Ok(())
    }

    /// C `asynMotorController::writeFloat64Array` (asynMotorController.cpp:
    /// 436-461): PROFILE_TIME_ARRAY into the times, PROFILE_POSITIONS through
    /// the axis's `defineProfile`, both cut to the profile's points.
    pub fn write_float64_array<P: ControllerParams + ?Sized>(
        &mut self,
        params: &mut P,
        user: &AsynUser,
        value: &[f64],
    ) -> AsynResult<()> {
        let addr = self.base.get_address(user.addr, user.reason)?;
        if !self.axes().any(|a| a == addr) {
            return Err(no_axis(&self.base.port_name, addr));
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
    fn send_profile(&self, addr: i32, n: usize) {
        let axis = self.axes.iter().find(|(a, _)| *a == addr);
        let positions = self.profile.axes.get(addr as usize).map(|p| &p.positions);
        if let (Some((_, Some(axis))), Some(positions)) = (axis, positions) {
            axis.send(MotorCommand::ProfileDefine {
                positions: positions[..n.min(positions.len())].to_vec(),
            });
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
        for (reason, array) in [
            (index::PROFILE_READBACKS, &axis.readbacks[..n]),
            (index::PROFILE_FOLLOWING_ERRORS, &axis.following_errors[..n]),
        ] {
            self.base.notify_interface_value(
                reason,
                addr,
                InterfaceType::Float64Array,
                ParamValue::Float64Array(array.into()),
                0,
                AsynStatus::Success,
            );
        }
        Ok(())
    }

    /// C `asynMotorController::executeProfile` (asynMotorController.cpp:
    /// 873-885).
    pub fn execute_profile(&mut self) -> AsynResult<()> {
        self.send_to_axes(|| MotorCommand::ProfileExecute);
        Ok(())
    }
}

/// The bare `asynError` a C base function returns, with no errorMessage.
fn base_error() -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message: String::new(),
    }
}

/// C `getAxis`'s failure: the controller has no axis at `addr`.
fn no_axis(port: &str, addr: i32) -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message: format!("{port} has no axis {addr}"),
    }
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
        let mut base = PortDriverBase::new(port, num_axes.max(1) as usize, flags);
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
        let addr = self.base.get_address(user.addr, user.reason)?;
        if let Some(axis) = crate::devices::axis_at(&self.base.port_name, addr) {
            return Ok((addr, Some(axis)));
        }
        if self.params.is_controller_axis(addr) {
            return Ok((addr, None));
        }
        Err(no_axis(&self.base.port_name, addr))
    }

    fn float(&self, index: usize, addr: i32) -> f64 {
        self.base.get_float64_param(index, addr).unwrap_or(0.0)
    }

    /// The base `writeInt32`'s driver call for `function`.
    /// Whether the poller runs it, which prints the write's closing line
    /// with the driver's status.
    fn int32_command(&self, axis: &MotorAxis, addr: i32, function: usize, value: i32) -> bool {
        let command = match function {
            index::STOP => MotorCommand::Stop {
                acceleration: self.float(index::ACCEL, addr),
            },
            index::DEFER_MOVES => MotorCommand::DeferMoves { defer: value != 0 },
            index::CLOSED_LOOP => MotorCommand::SetClosedLoop { enable: value != 0 },
            index::UPDATE_STATUS => {
                axis.force_poll();
                return false;
            }
            _ => return false,
        };
        axis.send(command);
        true
    }

    /// The base `writeFloat64`'s driver call for `function`; a move takes
    /// its speeds from the parameters, as C reads them.
    fn float64_command(&self, axis: &MotorAxis, addr: i32, function: usize, value: f64) -> bool {
        let min_velocity = self.float(index::VEL_BASE, addr);
        let velocity = self.float(index::VELOCITY, addr);
        let acceleration = self.float(index::ACCEL, addr);
        let gain = |kind| MotorCommand::SetPidGain { kind, gain: value };
        let command = match function {
            index::MOVE_REL => MotorCommand::MoveRelative {
                distance: value,
                min_velocity,
                velocity,
                acceleration,
            },
            index::MOVE_ABS => MotorCommand::MoveAbsolute {
                position: value,
                min_velocity,
                velocity,
                acceleration,
            },
            index::MOVE_VEL => MotorCommand::MoveVelocity {
                min_velocity,
                velocity: value,
                acceleration,
            },
            index::HOME => MotorCommand::Home {
                forward: value != 0.0,
                min_velocity,
                velocity,
                acceleration,
            },
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
            _ => return false,
        };
        axis.send(command);
        true
    }

    /// C's closing `asynPrint` of a base write (asynMotorController.cpp:
    /// 247-254, :419-426), through the writer's user.
    fn print_write(
        &self,
        addr: i32,
        function: usize,
        method: &str,
        status: &AsynResult<()>,
        value: std::fmt::Arguments<'_>,
    ) {
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

/// Where a controller-port write's base call finished.
enum BaseWrite {
    /// On the port: the write's status.
    Finished(AsynResult<()>),
    /// Handed to an axis, whose poller finishes it; the port's status so
    /// far.
    OnAxis(AsynResult<()>),
}

impl<P: ControllerParams> ControllerPort<P> {
    /// The driver's own code, its hook's check, then the base `writeInt32`.
    fn write_int32_base(
        &mut self,
        user: &AsynUser,
        addr: i32,
        axis: Option<MotorAxis>,
        value: i32,
    ) -> BaseWrite {
        let own = match self.params.write_int32(&mut self.base, user, value) {
            ParamWrite::PassToBase(own) => own,
            ParamWrite::Handled(status) => return BaseWrite::Finished(status),
        };
        if let Some(Err(e)) = self.hook.as_ref().map(|h| h.check(addr, user.reason)) {
            return BaseWrite::Finished(Err(e));
        }
        let _ = self.base.set_int32_param(user.reason, addr, value);
        let status = match user.reason {
            index::PROFILE_BUILD => self.with_params(|params, ctx| params.build_profile(ctx)),
            index::PROFILE_ABORT => self.with_params(|params, ctx| params.abort_profile(ctx)),
            index::PROFILE_READBACK => self.with_params(|params, ctx| params.readback_profile(ctx)),
            index::PROFILE_EXECUTE => self.with_params(|params, ctx| params.execute_profile(ctx)),
            _ => {
                let sent = axis
                    .as_ref()
                    .is_some_and(|axis| self.int32_command(axis, addr, user.reason, value));
                if sent {
                    let _ = self.base.call_param_callbacks(addr);
                    return BaseWrite::OnAxis(own);
                }
                Ok(())
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
        BaseWrite::Finished(own.and(status))
    }

    /// The driver's own code, its hook's check, then the base
    /// `writeFloat64`.
    fn write_float64_base(
        &mut self,
        user: &AsynUser,
        addr: i32,
        axis: Option<MotorAxis>,
        value: f64,
    ) -> BaseWrite {
        let own = match self.params.write_float64(&mut self.base, user, value) {
            ParamWrite::PassToBase(own) => own,
            ParamWrite::Handled(status) => return BaseWrite::Finished(status),
        };
        if let Some(Err(e)) = self.hook.as_ref().map(|h| h.check(addr, user.reason)) {
            return BaseWrite::Finished(Err(e));
        }
        let _ = self.base.set_float64_param(user.reason, addr, value);
        let sent = axis
            .as_ref()
            .is_some_and(|axis| self.float64_command(axis, addr, user.reason, value));
        let _ = self.base.call_param_callbacks(addr);
        if sent {
            return BaseWrite::OnAxis(own);
        }
        self.print_write(
            addr,
            user.reason,
            "writeFloat64",
            &Ok(()),
            format_args!("{value:.6}"),
        );
        BaseWrite::Finished(own)
    }

    /// The hook's code after a write the port finished; the axis's poller
    /// runs it for one it handed on.
    fn after_write(
        &mut self,
        addr: i32,
        function: usize,
        value: WriteValue,
        write: BaseWrite,
    ) -> AsynResult<()> {
        let status = match write {
            BaseWrite::OnAxis(status) => return status,
            BaseWrite::Finished(status) => status,
        };
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

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        let (addr, axis) = self.axis(user)?;
        let status = self.write_int32_base(user, addr, axis, value);
        self.after_write(addr, user.reason, WriteValue::Int32(value), status)
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        let (addr, axis) = self.axis(user)?;
        let status = self.write_float64_base(user, addr, axis, value);
        self.after_write(addr, user.reason, WriteValue::Float64(value), status)
    }

    fn write_float64_array(&mut self, user: &AsynUser, data: &[f64]) -> AsynResult<()> {
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

/// What an axis's poller runs a [`WriteHook`]'s sets through:
/// [`ControllerPort::store`] on the port's actor.
pub(crate) type ParamStore = Arc<dyn Fn(i32, ParamDelta) + Send + Sync>;

/// A published controller port, as the axes' pollers reach it.
pub(crate) struct Published {
    /// The driver's, for posting the values its poll reads; the parameter
    /// library the base writes as on the port.
    pub handle: PortHandle,
    pub hook: Option<Arc<dyn WriteHook>>,
    pub store: ParamStore,
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
    let store: ParamStore = Arc::new(move |addr, delta: ParamDelta| {
        if delta.is_empty() {
            return;
        }
        // C drivers ignore the status of setXxxParam.
        let _ = port
            .with_driver_blocking::<ControllerPort<P>, _>(move |driver| driver.store(addr, delta));
    });
    Ok(Published {
        handle,
        hook,
        store,
    })
}
