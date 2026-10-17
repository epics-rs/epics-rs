//! Motor interface definitions.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::AsynResult;
use crate::user::AsynUser;

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
    /// `poll(&moving)`), not a status bit.
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
    /// True if encoder slip is detected.
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
            done: true,
            moving: false,
            msta_moving: false,
            high_limit: false,
            low_limit: false,
            home: false,
            encoder_home: false,
            powered: false,
            problem: false,
            direction: false,
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
}

impl PollFailure {
    pub const NONE: Self = Self {
        problem: false,
        comms_error: false,
    };
    pub const PROBLEM: Self = Self {
        problem: true,
        comms_error: false,
    };
    pub const COMMS_ERROR: Self = Self {
        problem: false,
        comms_error: true,
    };
    pub const PROBLEM_AND_COMMS_ERROR: Self = Self {
        problem: true,
        comms_error: true,
    };
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
pub trait AsynMotor: Send + Sync {
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

    /// Move a relative distance.
    fn move_relative(
        &mut self,
        user: &AsynUser,
        distance: f64,
        min_velocity: f64,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()> {
        // Default: convert to absolute using current position from poll
        let status = self.poll(user)?;
        self.move_absolute(
            user,
            status.position + distance,
            min_velocity,
            velocity,
            acceleration,
        )
    }

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

    /// Poll the motor for current status.
    fn poll(&mut self, user: &AsynUser) -> AsynResult<MotorStatus>;

    /// A model-1 `set_status` with a move in motion (`nodeptr != 0`):
    /// `cdir` is the moving record's CDIR, which the C driver reads and may
    /// overwrite through `nodeptr->mrecord` (drvSC800.cc:279-282,
    /// drvOms58.cc:642-649); the record reads what is left. The motor task
    /// calls it in place of [`AsynMotor::poll`] while it holds the move.
    fn poll_in_motion(&mut self, user: &AsynUser, cdir: &mut bool) -> AsynResult<MotorStatus> {
        let _ = cdir;
        self.poll(user)
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

    /// After a failed poll, the status it had set before it bailed, without
    /// [`AsynMotor::poll_failure`]'s bits: C `set_status` storing
    /// `motor_info->status` at its exit on RETRY (drvMM3000.cc:313-334), or
    /// an asyn poll's `setIntegerParam` calls before its error label. The
    /// next post carries it. None leaves the last status.
    fn take_failed_poll_status(&mut self) -> Option<MotorStatus> {
        None
    }

    /// The status fields the write method just called set, in order. The
    /// poller drains this after every write and posts the result when it
    /// changed the status, as `callParamCallbacks` ends every C
    /// `asynMotorController` write. None by default.
    fn take_status_writes(&mut self) -> Vec<StatusWrite> {
        Vec::new()
    }

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

    /// Move to a named "home" position defined by the controller.
    /// Distinct from `home()` (limit-switch seek): this is an absolute move
    /// to a pre-defined position, typical of model-3 controllers.
    /// C: `a6f64591` + `5f421e9a` (2011-07).
    fn move_to_home(
        &mut self,
        user: &AsynUser,
        position: f64,
        min_velocity: f64,
        velocity: f64,
        acceleration: f64,
    ) -> AsynResult<()> {
        self.move_absolute(user, position, min_velocity, velocity, acceleration)
    }
}

// Position-compare output (PCO) is intentionally NOT part of this trait: the C
// change adding `enablePCO`/PCO params to the base asynMotorAxis class
// (`05b25c1d`, motor PR #248) is an open, unmerged PR — not upstream API.
// PCO-capable drivers (Aerotech, Newport XPS, Galil, ACSMotion) expose it
// driver-privately (e.g. iocsh commands) until upstream actually merges a base
// interface to mirror.
