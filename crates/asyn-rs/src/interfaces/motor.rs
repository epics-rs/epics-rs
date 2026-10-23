//! Motor interface definitions.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::AsynResult;
use crate::user::AsynUser;

/// One step a driver makes on its own controller port: C's
/// `setXxxParam(addr, …)` or `callParamCallbacks(addr)`, `pAxis->setXxxParam`
/// on the axis's own address, a status field set and called back on axis
/// `addr`, or `epicsEventSignal(pollEventId_)`.
#[derive(Debug, Clone)]
pub enum PortSet {
    Set {
        addr: i32,
        reason: usize,
        value: crate::param::ParamValue,
    },
    Axis {
        reason: usize,
        value: crate::param::ParamValue,
    },
    Callbacks(i32),
    Status {
        addr: i32,
        field: StatusWrite,
    },
    WakeupPoller,
}

/// The steps a driver made on its own controller port, in the order made;
/// see [`AsynMotor::take_port_sets`].
#[derive(Debug, Clone, Default)]
pub struct PortSets(Vec<PortSet>);

impl PortSets {
    pub fn set(&mut self, addr: i32, reason: usize, value: crate::param::ParamValue) {
        self.0.push(PortSet::Set {
            addr,
            reason,
            value,
        });
    }

    pub fn call_param_callbacks(&mut self, addr: i32) {
        self.0.push(PortSet::Callbacks(addr));
    }

    /// C's `pAxis->setXxxParam(…)` beyond the status, such as phytron's
    /// `axisStatus_` (phytronAxisMotor.cpp:2084): on the axis's own
    /// address, posted by the `callParamCallbacks` that ends the poll or
    /// the write (asynMotorController.cpp:319).
    pub fn axis(&mut self, reason: usize, value: crate::param::ParamValue) {
        self.0.push(PortSet::Axis { reason, value });
    }

    /// C's `pAxis->setIntegerParam(motorStatusXxx_, …)` then
    /// `pAxis->callParamCallbacks()` on axis `addr`, which may be another
    /// axis than the caller's.
    pub fn status(&mut self, addr: i32, field: StatusWrite) {
        self.0.push(PortSet::Status { addr, field });
    }

    /// C `epicsEventSignal(pollEventId_)`: the controller's poller passes.
    pub fn wakeup_poller(&mut self) {
        self.0.push(PortSet::WakeupPoller);
    }

    pub fn append(&mut self, mut other: PortSets) {
        self.0.append(&mut other.0);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl IntoIterator for PortSets {
    type Item = PortSet;
    type IntoIter = std::vec::IntoIter<PortSet>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// A controller poll that sets nothing on its port.
impl From<()> for PortSets {
    fn from((): ()) -> Self {
        Self::default()
    }
}

/// A model-1 record's POST field, shared with the device: C arms a move with
/// `postmsgptr = &mr->post` and the driver copies it out when the move is
/// done, so a POST put during the move is the one sent. A clone is the same
/// field.
#[derive(Debug, Clone, Default)]
pub struct PostMessage(Arc<Mutex<Vec<u8>>>);

impl PostMessage {
    pub fn get(&self) -> Vec<u8> {
        self.0.lock().clone()
    }

    pub fn set(&self, bytes: &[u8]) {
        let mut post = self.0.lock();
        post.clear();
        post.extend_from_slice(bytes);
    }

    pub fn is_empty(&self) -> bool {
        self.0.lock().is_empty()
    }
}

impl PartialEq for PostMessage {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || *self.0.lock() == *other.0.lock()
    }
}

/// Motor axis status.
/// Fields match the C asynMotorController MotorStatus structure.
///
/// `PartialEq` backs the poll loop's idle change-detection: the loop notifies
/// the record only when a freshly polled status differs from the last one
/// delivered, the analogue of C `asynMotorAxis::callParamCallbacks`
/// (asynMotorAxis.cpp:316-322) firing the generic-pointer status callback only
/// when `statusChanged_` is set. f64 `!=` matches C's field comparisons
/// (`value != status_.position`), including the `NaN != NaN` always-changed
/// case.
#[derive(Debug, Clone, PartialEq)]
pub struct MotorStatus {
    /// Motor position, steps.
    pub position: f64,
    /// Encoder position, counts.
    pub encoder_position: f64,
    /// Actual velocity, steps/s.
    pub velocity: f64,
    /// True if the last move has completed.
    pub done: bool,
    /// True if the motor is currently moving: the poller's pace (C
    /// `poll(&moving)`), not a status bit. A failed poll paces on what it
    /// left here, as phytronAxis::poll still reports `!motorStatusDone_`
    /// (phytronAxisMotor.cpp:1868-1869).
    pub moving: bool,
    /// MSTA bit 10 RA_MOVING, C `motorStatusMoving_`. Off by default: C
    /// starts it at 0 and only drivers that set that parameter raise it.
    pub msta_moving: bool,
    /// True if a positive (raw) limit switch is active.
    pub high_limit: bool,
    /// True if a negative (raw) limit switch is active.
    pub low_limit: bool,
    /// True if the motor's home switch is active (C asynMotorController
    /// `motorStatusAtHome_`, MSTA bit 3 RA_HOME).
    pub home: bool,
    /// True if the encoder's home signal is active (C asynMotorController
    /// `motorStatusHome_`, MSTA bit 7 EA_HOME). The motor record reads
    /// this one for ATHM when UEIP=Yes.
    pub encoder_home: bool,
    /// True if the motor is powered on / closed-loop is active (C
    /// `motorStatusPowerOn_`, MSTA bit 5). Off by default: C starts every
    /// status bit at 0, and only drivers that read the drive state set it.
    pub powered: bool,
    /// True if a problem was detected (driver stopped polling).
    pub problem: bool,
    /// Direction of last motion (true = positive).
    pub direction: bool,
    /// MSTA bit 4 (EA_SLIP), C's `motorStatusSlip_`.
    pub slip: bool,
    /// MSTA bit 6 (EA_SLIP_STALL), C's `motorStatusFollowingError_`.
    pub slip_stall: bool,
    /// True if communication error was detected.
    pub comms_error: bool,
    /// True if the axis has been homed.
    pub homed: bool,
    /// True if the controller supports closed-loop gain.
    pub gain_support: bool,
    /// True if an encoder is present.
    pub has_encoder: bool,
    /// True if the controller exposes a usable base velocity (VBAS).
    /// epics-modules/motor issue #76. Drivers that ignore VBAS should set
    /// this to `false`; the record-level layer then treats VBAS as 0 in
    /// acceleration math.
    pub vbas_supported: bool,
}

impl Default for MotorStatus {
    fn default() -> Self {
        Self {
            position: 0.0,
            encoder_position: 0.0,
            velocity: 0.0,
            done: false,
            moving: false,
            msta_moving: false,
            high_limit: false,
            low_limit: false,
            home: false,
            encoder_home: false,
            powered: false,
            problem: false,
            direction: false,
            slip: false,
            slip_stall: false,
            comms_error: false,
            homed: false,
            gain_support: false,
            has_encoder: false,
            vbas_supported: true,
        }
    }
}

/// The status bits a driver's C `poll()` sets on its error label before
/// `callParamCallbacks`. Every other field keeps its value, as the parameter
/// library does for fields the failed poll never wrote. A driver that returns
/// before `callParamCallbacks` raises [`PollFailure::NONE`]: the background
/// poller then posts nothing, and a forced poll posts the unchanged status,
/// exactly as for a failure that sets no bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollFailure {
    pub problem: bool,
    pub comms_error: bool,
    /// C's poll still returns asynSuccess, so a GET_INFO write does too
    /// (phytronAxisMotor.cpp:1842-1872).
    pub succeeds: bool,
}

impl PollFailure {
    pub const NONE: Self = Self {
        problem: false,
        comms_error: false,
        succeeds: false,
    };
    pub const PROBLEM: Self = Self {
        problem: true,
        comms_error: false,
        succeeds: false,
    };
    pub const COMMS_ERROR: Self = Self {
        problem: false,
        comms_error: true,
        succeeds: false,
    };
    pub const PROBLEM_AND_COMMS_ERROR: Self = Self {
        problem: true,
        comms_error: true,
        succeeds: false,
    };

    /// Whether the failed poll sets any status bit.
    pub const fn raises(self) -> bool {
        self.problem || self.comms_error
    }

    /// These bits, raised by a poll that C returns asynSuccess from.
    pub const fn succeeding(self) -> Self {
        Self {
            succeeds: true,
            ..self
        }
    }
}

/// Which closed-loop gain a [`AsynMotor::set_pid_gain`] call addresses.
/// C devMotorAsyn build_trans maps SET_PGAIN/SET_IGAIN/SET_DGAIN onto
/// the motorPGain/motorIGain/motorDGain float64 parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidGainKind {
    Proportional,
    Integral,
    Derivative,
}

/// A [`MotorStatus`] field a write method set, C `asynMotorAxis::
/// setDoubleParam` / `setIntegerParam` on a `status_` member
/// (asynMotorAxis.cpp:250-300) or a model-2 driver's `motorParam->set*`,
/// for the poller to post after the write.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StatusWrite {
    Position(f64),
    EncoderPosition(f64),
    Done(bool),
    PowerOn(bool),
    Problem(bool),
    /// `motorAxisDirection`, which model-2 moves set before they post
    /// (drvEnsembleAsyn.cc:537, drvANC150Asyn.cc:453).
    Direction(bool),
    /// `motorAxisHighHardLimit`, MSTA bit 2 RA_PLUS_LS.
    HighHardLimit(bool),
    /// `motorAxisHomeSignal`, MSTA bit 3 RA_HOME.
    HomeSignal(bool),
    /// `motorStatusHome_` / `motorAxisHomeEncoder`, MSTA bit 7 EA_HOME.
    EncoderHome(bool),
    /// `motorAxisMoving`, MSTA bit 10 RA_MOVING (drvMotorAsyn.c:847-852).
    Moving(bool),
    /// `motorAxisLowHardLimit`, MSTA bit 13 RA_MINUS_LS.
    LowHardLimit(bool),
    /// `motorStatusCommsError_`, MSTA bit 12 CNTRL_COMM_ERR, which a C
    /// axis sets when a controller write fails (smarActMCSMotorDriver.cpp:
    /// 622-627, smarActSCUMotorDriver.cpp:446-448). A driver that sets the
    /// param on its controller instead names it in
    /// [`AsynMotor::param_owned_msta_bits`].
    CommsError(bool),
    /// `motorStatusHomed_` / `motorAxisHomed`, MSTA bit 14 RA_HOMED.
    Homed(bool),
    /// `motorStatusHasEncoder_` / `motorAxisHasEncoder`, MSTA bit 8
    /// EA_PRESENT, which C axes set once in their constructor
    /// (phytronAxisMotor.cpp:1352).
    HasEncoder(bool),
    /// `motorStatusGainSupport_` / `motorAxisHasClosedLoop`, MSTA bit 11
    /// GAIN_SUPPORT.
    GainSupport(bool),
}

impl StatusWrite {
    /// Write the field into `status`.
    pub fn apply(self, status: &mut MotorStatus) {
        match self {
            StatusWrite::Position(v) => status.position = v,
            StatusWrite::EncoderPosition(v) => status.encoder_position = v,
            StatusWrite::Done(v) => status.done = v,
            StatusWrite::PowerOn(v) => status.powered = v,
            StatusWrite::Problem(v) => status.problem = v,
            StatusWrite::Direction(v) => status.direction = v,
            StatusWrite::HighHardLimit(v) => status.high_limit = v,
            StatusWrite::HomeSignal(v) => status.home = v,
            StatusWrite::EncoderHome(v) => status.encoder_home = v,
            StatusWrite::Moving(v) => status.msta_moving = v,
            StatusWrite::LowHardLimit(v) => status.low_limit = v,
            StatusWrite::CommsError(v) => status.comms_error = v,
            StatusWrite::Homed(v) => status.homed = v,
            StatusWrite::HasEncoder(v) => status.has_encoder = v,
            StatusWrite::GainSupport(v) => status.gain_support = v,
        }
    }
}

/// Motor interface trait.
///
/// The motor record fields a C model-1 device reads through `mr` in
/// `build_trans`: a faulhaber HOME runs at VELO, not at the HVEL the record
/// sent, and counts revolutions in SREV (devMCDC2805.cc:224-227).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordFields {
    /// VELO, in steps/s.
    pub velocity: f64,
    /// SREV, steps per revolution.
    pub srev: i32,
}

/// Provides motor axis control for motor-capable drivers.
///
/// Units are C `asynMotorAxis`'s: positions in steps, velocities in
/// steps/s, accelerations in steps/s². The motor record owns the MRES/ERES
/// conversion, as motorRecord.cc does; a driver does no scaling.
///
/// `Any`, so a driver's call queued on its axis (motor-rs `DriverCall`)
/// can reach its own axis type from the `&mut dyn AsynMotor` it is given.
pub trait AsynMotor: std::any::Any + Send + Sync {
    /// Move to an absolute position.
    ///
    /// `min_velocity` is the base/start velocity (motor record VBAS):
    /// canonical `asynMotorAxis::move(position, relative, minVelocity,
    /// maxVelocity, acceleration)` carries it, and the motor record emits it as
    /// `SET_VEL_BASE` ahead of the move (`devMotorAsyn.c` `motorVelBase`).
    fn move_absolute(
        &mut self,
        user: &AsynUser,
        position: f64,
        min_velocity: f64,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()>;

    /// Move a relative distance. No default: a C driver's `move(relative=1)`
    /// sends its own relative command, never a poll-then-absolute move.
    fn move_relative(
        &mut self,
        user: &AsynUser,
        distance: f64,
        min_velocity: f64,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()>;

    /// Move at a constant velocity (jog).
    /// Default implementation uses move_absolute to a very large target.
    /// `min_velocity` is the base velocity (VBAS); canonical
    /// `asynMotorAxis::moveVelocity(minVelocity, maxVelocity, acceleration)`.
    fn move_velocity(
        &mut self,
        user: &AsynUser,
        min_velocity: f64,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()> {
        let target = if velocity >= 0.0 { 1e9 } else { -1e9 };
        self.move_absolute(user, target, min_velocity, velocity.abs(), acceleration)
    }

    /// Start a homing sequence. `min_velocity` is the base velocity (VBAS) and
    /// `acceleration` the home ramp rate; canonical
    /// `asynMotorAxis::home(minVelocity, maxVelocity, acceleration, forwards)`.
    fn home(
        &mut self,
        user: &AsynUser,
        min_velocity: f64,
        velocity: f64,
        acceleration: f64,
        forward: bool,
    ) -> AsynResult<()>;

    /// Stop motion.
    fn stop(&mut self, user: &AsynUser, acceleration: f64) -> AsynResult<()>;

    /// Set the current position without moving.
    fn set_position(&mut self, user: &AsynUser, position: f64) -> AsynResult<()>;

    /// Set the encoder position without moving (C
    /// `asynMotorAxis::setEncoderPosition`, a no-op unless the driver
    /// overrides it).
    fn set_encoder_position(&mut self, _user: &AsynUser, _position: f64) -> AsynResult<()> {
        Ok(())
    }

    /// Set the encoder ratio, motor steps per encoder count (C
    /// `asynMotorAxis::setEncoderRatio`, a no-op unless the driver needs it).
    fn set_encoder_ratio(&mut self, _user: &AsynUser, _ratio: f64) -> AsynResult<()> {
        Ok(())
    }

    /// The MSTA bits (`1 << 12` is CNTRL_COMM_ERR) whose
    /// `MOTOR_STATUS_*` parameter the driver sets on its controller port
    /// itself, as IMS sets COMMS_ERROR (ImsMDrivePlusMotorController.cpp:
    /// 403-406). The poller never mirrors those from the status.
    fn param_owned_msta_bits(&self) -> u32 {
        0
    }

    /// Enable or disable closed-loop control.
    fn set_closed_loop(&mut self, _user: &AsynUser, _enable: bool) -> AsynResult<()> {
        Ok(())
    }

    /// Set a closed-loop gain coefficient (0.0 ..= 1.0, unitless — the
    /// motor record clamps before calling; C special pidcof,
    /// motorRecord.cc 3003-3026). Only meaningful for controllers that
    /// report gain support.
    fn set_pid_gain(&mut self, _user: &AsynUser, _kind: PidGainKind, _gain: f64) -> AsynResult<()> {
        Ok(())
    }

    /// Set the controller's high soft-travel limit (C `motorHighLimit`):
    /// the record's dial high limit over MRES, or its dial low limit
    /// under a negative MRES (motorRecord.cc 4236-4328).
    fn set_high_limit(&mut self, _user: &AsynUser, _position: f64) -> AsynResult<()> {
        Ok(())
    }

    /// Set the controller's low soft-travel limit (C `motorLowLimit`).
    fn set_low_limit(&mut self, _user: &AsynUser, _position: f64) -> AsynResult<()> {
        Ok(())
    }

    /// Enable or disable deferred moves for coordinated multi-axis motion.
    fn set_deferred_moves(&mut self, _user: &AsynUser, _defer: bool) -> AsynResult<()> {
        Ok(())
    }

    /// A model-1 record's OUT parm, the text after `@` in `#C card S signal
    /// @parm`, given once when the record binds the axis: C devices read it
    /// from the record (devMVP2001.cc:262, :361). Not given when empty.
    fn set_out_parm(&mut self, _user: &AsynUser, _parm: &[u8]) -> AsynResult<()> {
        Ok(())
    }

    /// A model-1 device's `PRIMITIVE`: the record's INIT text, sent once at
    /// record init before anything else (motordevCom.cc:309-327). Each C
    /// device formats it its own way (`"? "` + INIT on an IM483PL).
    fn primitive(&mut self, _user: &AsynUser, _init: &[u8]) -> AsynResult<()> {
        Ok(())
    }

    /// A model-1 record's PREM and POST for the move called next, which C
    /// `build_trans` reads at every MOVE_ABS, MOVE_REL, HOME and JOG
    /// (devIM483PL.cc:191-205): the device puts PREM ahead of that move its
    /// own way. `post` is given when POST was set at that build, and the
    /// device sends what it holds once the move is done
    /// (drvIM483PL.cc:361-366). Called only when either is set, so they
    /// belong to that one move.
    fn set_move_strings(
        &mut self,
        _user: &AsynUser,
        _prem: &[u8],
        _post: Option<PostMessage>,
    ) -> AsynResult<()> {
        Ok(())
    }

    /// A model-1 record's [`RecordFields`], given ahead of each transaction
    /// that moves.
    fn set_record_fields(&mut self, _user: &AsynUser, _fields: &RecordFields) -> AsynResult<()> {
        Ok(())
    }

    /// Poll the motor into `status`: the axis's status parameters as the
    /// poller last set them, which a C poll sets in place. What it wrote
    /// before an `Err` stays set, as a C poll's sets before it bails, with
    /// [`AsynMotor::poll_failure`]'s bits raised over them.
    fn poll(&mut self, user: &AsynUser, status: &mut MotorStatus) -> AsynResult<()>;

    /// A model-1 `set_status` with a move in motion (`nodeptr != 0`):
    /// `cdir` is the moving record's CDIR, which the C driver reads and may
    /// overwrite through `nodeptr->mrecord` (drvSC800.cc:279-282,
    /// drvOms58.cc:642-649); the record reads what is left. The motor task
    /// calls it in place of [`AsynMotor::poll`] while it holds the move.
    fn poll_in_motion(
        &mut self,
        user: &AsynUser,
        cdir: &mut bool,
        status: &mut MotorStatus,
    ) -> AsynResult<()> {
        let _ = cdir;
        self.poll(user, status)
    }

    /// Whether the model-1 device support forces the record's NTM to NO at
    /// init_record because the device does its own backlash
    /// (devEMC18011.cc:127-129, devPIE816.cc:130, devMDT695.cc:129).
    fn disables_ntm(&self) -> bool {
        false
    }

    /// Whether a model-1 `set_status` counts a pass whose position did not
    /// change as no motion (`no_motion_count++`), which motor_task does not
    /// call back. drvEMC18011.cc:356-358 disables it, so every pass of a
    /// held move is called back.
    fn no_motion_test(&self) -> bool {
        true
    }

    /// What the C driver's `poll()` posts when it fails, i.e. when
    /// [`AsynMotor::poll`] returns `Err`. No default: drivers differ (an
    /// SMC100 raises only `motorStatusProblem_`, an XPS axis also
    /// `motorStatusCommsError_`), and the bits reach the record as MSTA.
    fn poll_failure(&self) -> PollFailure;

    /// Whether the C axis constructor ends in `callParamCallbacks`, which
    /// lowers the `statusChanged_` asynMotorAxis's constructor raised
    /// (asynMotorAxis.cpp:29, :316-322). Raised, the first poll that calls
    /// back posts even a status equal to the constructor's.
    fn constructor_calls_param_callbacks(&self) -> bool {
        false
    }

    /// The status fields the write method just called set, in order. The
    /// poller drains this after every write and posts the result when it
    /// changed the status, as `callParamCallbacks` ends every C
    /// `asynMotorController` write. None by default.
    fn take_status_writes(&mut self) -> Vec<StatusWrite> {
        Vec::new()
    }

    /// What the poll or write method just did to its own controller port,
    /// in C's program order: its `setXxxParam` calls on the controller or
    /// on the axis, and `callParamCallbacks(addr)`, made under the lock the
    /// method holds. The axis's owner applies them once it lets go of the
    /// axis; a driver never calls its own port from under the axis lock.
    /// None by default.
    fn take_port_sets(&mut self) -> PortSets {
        PortSets::default()
    }

    /// A model-2 driver's `motorAxisforceCallback`, which drvMotorAsyn
    /// calls for a GET_INFO (drvMotorAsyn.c:617-619) ahead of the poll the
    /// poller runs for it; a driver prints its FLOW line here
    /// (drvEnsembleAsyn.cc:676). Nothing by default.
    fn force_callback(&mut self, _user: &AsynUser) {}

    /// Initialize profile move with maximum number of points.
    fn initialize_profile(&mut self, _user: &AsynUser, _max_points: usize) -> AsynResult<()> {
        Ok(())
    }

    /// Define profile positions for this axis.
    fn define_profile(&mut self, _user: &AsynUser, _positions: &[f64]) -> AsynResult<()> {
        Ok(())
    }

    /// Build the profile trajectory.
    fn build_profile(&mut self, _user: &AsynUser) -> AsynResult<()> {
        Ok(())
    }

    /// Execute the profile move.
    fn execute_profile(&mut self, _user: &AsynUser) -> AsynResult<()> {
        Ok(())
    }

    /// Abort the profile move.
    fn abort_profile(&mut self, _user: &AsynUser) -> AsynResult<()> {
        Ok(())
    }

    /// Read back actual positions after profile execution.
    fn readback_profile(&mut self, _user: &AsynUser) -> AsynResult<Vec<f64>> {
        Ok(vec![])
    }

    /// C `doMoveToHome`, which the controller's moveToHome thread runs
    /// without the controller lock (asynMotorController.cpp:723-742): taken
    /// under the lock, run once it is released. None is C's default
    /// `asynMotorAxis::doMoveToHome`, which only prints that the axis has
    /// no implementation (asynMotorAxis.cpp:209-217). The argument is C's
    /// `getReferencingModeMove()`, 0 until `asynMotorEnableMoveToHome`.
    fn move_to_home_task(&mut self, _referencing_mode_move: i32) -> Option<MoveToHomeTask> {
        None
    }
}

/// A model-1 driver's axis (motordrvCom.cc), which only a motor task runs.
pub trait Model1Motor: AsynMotor {
    /// What the driver's `motor_init` read with `set_status`, at the point
    /// it reads it among the axis's init queries (drvSoloist.cc:734,
    /// drvMVP2001.cc:720). C motor_task makes no read of its own before the
    /// records initialise: this is the axis's first status, taken once. A
    /// failed read is what `set_status`'s failure exit stores: the status
    /// `motor_init` prepared (EA_PRESENT, GAIN_SUPPORT) with that driver's
    /// RETRY or COMM_ERR bits (drvMCDC2805.cc:236-251, :368-369).
    fn take_init_status(&mut self) -> MotorStatus;
}

/// [`AsynMotor::move_to_home_task`]'s work. It applies sets on the
/// controller port through its argument, which takes the controller lock as
/// C's `pC_->lock(); setXxxParam(); callParamCallbacks(); pC_->unlock();`.
pub type MoveToHomeTask = Box<dyn FnOnce(&dyn Fn(PortSets)) -> AsynResult<()> + Send>;

// Position-compare output (PCO) is intentionally NOT part of this trait: the C
// change adding `enablePCO`/PCO params to the base asynMotorAxis class
// (`05b25c1d`, motor PR #248) is an open, unmerged PR — not upstream API.
// PCO-capable drivers (Aerotech, Newport XPS, Galil, ACSMotion) expose it
// driver-privately (e.g. iocsh commands) until upstream actually merges a base
// interface to mirror.

#[cfg(test)]
mod tests {
    use super::*;

    struct Axis(f64);

    impl AsynMotor for Axis {
        fn move_absolute(
            &mut self,
            _: &AsynUser,
            p: f64,
            _: f64,
            _: f64,
            _: f64,
        ) -> AsynResult<()> {
            self.0 = p;
            Ok(())
        }
        fn move_relative(
            &mut self,
            _: &AsynUser,
            _distance: f64,
            _: f64,
            _: f64,
            _: f64,
        ) -> AsynResult<()> {
            Ok(())
        }
        fn home(&mut self, _: &AsynUser, _: f64, _: f64, _: f64, _: bool) -> AsynResult<()> {
            Ok(())
        }
        fn stop(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
            Ok(())
        }
        fn set_position(&mut self, _: &AsynUser, _: f64) -> AsynResult<()> {
            Ok(())
        }
        fn poll(&mut self, _: &AsynUser, _: &mut MotorStatus) -> AsynResult<()> {
            Ok(())
        }
        fn poll_failure(&self) -> PollFailure {
            PollFailure::PROBLEM_AND_COMMS_ERROR
        }
    }

    #[test]
    fn a_motor_downcasts_to_its_own_type() {
        let mut axis = Axis(0.0);
        let motor: &mut dyn AsynMotor = &mut axis;
        let any: &mut dyn std::any::Any = motor;
        any.downcast_mut::<Axis>().unwrap().0 = 5.0;
        assert_eq!(axis.0, 5.0);
    }

    /// asynMotorAxis.cpp:46 zeroes `status_`, so a status no poll has set
    /// is not done.
    #[test]
    fn the_status_before_any_poll_is_not_done() {
        assert!(!MotorStatus::default().done);
    }
}
