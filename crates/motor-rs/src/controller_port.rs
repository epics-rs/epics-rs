//! C `asynMotorController`'s own asyn port: the parameter library a
//! controller's records reach through `@asyn(PORT,addr)`, at the controller
//! address and at each axis, beside the motor records' `asynMotor` path.
//!
//! A write runs the driver's [`ControllerParams`] hook first, as a C subclass
//! `writeInt32` / `writeFloat64` does before it calls the base, then the base
//! (asynMotorController.cpp:191-256, :269-430): store the value, hand a base
//! function's command to the axis at that address, run the callbacks.

use std::sync::Arc;

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::param::ParamType;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::port_handle::PortHandle;
use asyn_rs::registry::claim_port;
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime;
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
    pub const VEL_BASE: usize = 7;
    pub const ACCEL: usize = 8;
    pub const POSITION: usize = 9;
    pub const DEFER_MOVES: usize = 11;
    pub const MOVE_TO_HOME: usize = 12;
    pub const ENCODER_RATIO: usize = 14;
    pub const PGAIN: usize = 15;
    pub const IGAIN: usize = 16;
    pub const DGAIN: usize = 17;
    pub const HIGH_LIMIT: usize = 18;
    pub const LOW_LIMIT: usize = 19;
    pub const CLOSED_LOOP: usize = 20;
    pub const UPDATE_STATUS: usize = 27;
    pub const PROFILE_BUILD: usize = 59;
    pub const PROFILE_EXECUTE: usize = 63;
    pub const PROFILE_ABORT: usize = 67;
    pub const PROFILE_READBACK: usize = 68;
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

    /// The subclass `writeInt32` ahead of its call to the base; its error is
    /// the write's, after the base has run.
    fn write_int32(
        &mut self,
        _base: &mut PortDriverBase,
        _user: &AsynUser,
        _value: i32,
    ) -> AsynResult<()> {
        Ok(())
    }

    /// The subclass `writeFloat64`; see [`Self::write_int32`].
    fn write_float64(
        &mut self,
        _base: &mut PortDriverBase,
        _user: &AsynUser,
        _value: f64,
    ) -> AsynResult<()> {
        Ok(())
    }
}

/// A controller with no parameters of its own.
impl ControllerParams for () {}

/// The asyn driver behind a controller port.
pub struct ControllerPort<P> {
    base: PortDriverBase,
    params: P,
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
        params.create_params(&mut base)?;
        Ok(Self { base, params })
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
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: format!("{} has no axis {addr}", self.base.port_name),
        })
    }

    fn float(&self, index: usize, addr: i32) -> f64 {
        self.base.get_float64_param(index, addr).unwrap_or(0.0)
    }

    /// The base `writeInt32`'s driver call for `function`.
    fn int32_command(&self, axis: &MotorAxis, addr: i32, function: usize, value: i32) {
        let command = match function {
            index::STOP => MotorCommand::Stop {
                acceleration: self.float(index::ACCEL, addr),
            },
            index::DEFER_MOVES => MotorCommand::DeferMoves { defer: value != 0 },
            index::CLOSED_LOOP => MotorCommand::SetClosedLoop { enable: value != 0 },
            index::UPDATE_STATUS => return axis.force_poll(),
            index::PROFILE_BUILD => MotorCommand::ProfileBuild,
            index::PROFILE_EXECUTE => MotorCommand::ProfileExecute,
            index::PROFILE_ABORT => MotorCommand::ProfileAbort,
            index::PROFILE_READBACK => MotorCommand::ProfileReadback,
            _ => return,
        };
        axis.send(command);
    }

    /// The base `writeFloat64`'s driver call for `function`; a move takes
    /// its speeds from the parameters, as C reads them.
    fn float64_command(&self, axis: &MotorAxis, addr: i32, function: usize, value: f64) {
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
            index::HIGH_LIMIT => MotorCommand::SetHighLimit { position: value },
            index::LOW_LIMIT => MotorCommand::SetLowLimit { position: value },
            index::PGAIN => gain(PidGainKind::Proportional),
            index::IGAIN => gain(PidGainKind::Integral),
            index::DGAIN => gain(PidGainKind::Derivative),
            index::ENCODER_RATIO => MotorCommand::SetEncoderRatio { ratio: value },
            _ => return,
        };
        axis.send(command);
    }
}

impl<P: ControllerParams> PortDriver for ControllerPort<P> {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        let (addr, axis) = self.axis(user)?;
        let own = self.params.write_int32(&mut self.base, user, value);
        let _ = self.base.set_int32_param(user.reason, addr, value);
        if let Some(axis) = &axis {
            self.int32_command(axis, addr, user.reason, value);
        }
        let _ = self.base.call_param_callbacks(addr);
        own
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        let (addr, axis) = self.axis(user)?;
        let own = self.params.write_float64(&mut self.base, user, value);
        let _ = self.base.set_float64_param(user.reason, addr, value);
        if let Some(axis) = &axis {
            self.float64_command(axis, addr, user.reason, value);
        }
        let _ = self.base.call_param_callbacks(addr);
        own
    }
}

/// Publish controller port `port` with axes `0..num_axes` and the driver's
/// `params`, gated on `owner` (the IOC's interruptAccept). The returned
/// handle is the driver's for posting the values its poll reads.
pub(crate) fn publish<P: ControllerParams>(
    owner: &Arc<InterruptGate>,
    port: &str,
    num_axes: i32,
    params: P,
) -> Result<PortHandle, String> {
    let claim = claim_port(port).map_err(|e| format!("motor port {port}: {e}"))?;
    let driver = ControllerPort::new(port, num_axes, params)
        .map_err(|e| format!("motor port {port}: {e}"))?;
    let (runtime, _actor) = create_port_runtime(&claim, driver, RuntimeConfig::default())
        .map_err(|e| format!("motor port {port}: {e}"))?;
    let handle = runtime.port_handle().clone();
    if let Err(e) = claim.publish(handle.clone(), owner) {
        runtime.shutdown();
        return Err(format!("motor port {port}: {e}"));
    }
    Ok(handle)
}
