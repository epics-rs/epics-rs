use bitflags::bitflags;
use epics_base_rs::types::PvString;

pub use asyn_rs::interfaces::motor::{PidGainKind, PostMessage};

bitflags! {
    /// MIP (Motion In Progress) flags — exposed as a PV field.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct MipFlags: u16 {
        const JOGF      = 0x0001;
        const JOGR      = 0x0002;
        const JOG_BL1   = 0x0004;
        const HOMF      = 0x0008;
        const HOMR      = 0x0010;
        const MOVE      = 0x0020;
        const RETRY     = 0x0040;
        const LOAD_P    = 0x0080;
        const MOVE_BL   = 0x0100;
        const STOP      = 0x0200;
        const DELAY_REQ = 0x0400;
        const DELAY_ACK = 0x0800;
        const JOG_REQ   = 0x1000;
        const JOG_STOP  = 0x2000;
        const JOG_BL2   = 0x4000;
        const EXTERNAL  = 0x8000;
    }
}

bitflags! {
    /// Motor status flags (MSTA field).
    /// Bit positions match C motorRecord msta_field for wire compatibility.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct MstaFlags: u32 {
        const DIRECTION       = 0x0001; // bit 0: RA_DIRECTION
        const DONE            = 0x0002; // bit 1: RA_DONE
        const PLUS_LS         = 0x0004; // bit 2: RA_PLUS_LS
        const HOME_LS         = 0x0008; // bit 3: RA_HOME
        const SLIP            = 0x0010; // bit 4: EA_SLIP
        const POSITION        = 0x0020; // bit 5: EA_POSITION
        const SLIP_STALL      = 0x0040; // bit 6: EA_SLIP_STALL
        const EA_HOME         = 0x0080; // bit 7: EA_HOME
        const ENCODER_PRESENT = 0x0100; // bit 8: EA_PRESENT
        const PROBLEM         = 0x0200; // bit 9: RA_PROBLEM
        const MOVING          = 0x0400; // bit 10: RA_MOVING
        const GAIN_SUPPORT    = 0x0800; // bit 11: GAIN_SUPPORT
        const COMM_ERR        = 0x1000; // bit 12: CNTRL_COMM_ERR
        const MINUS_LS        = 0x2000; // bit 13: RA_MINUS_LS
        const HOMED           = 0x4000; // bit 14: RA_HOMED
    }
}

impl MstaFlags {
    /// The driver's status word, C `MotorStatus.status`: what
    /// `update_values` copies to MSTA (devMotorAsyn.c:467).
    pub fn from_status(status: &asyn_rs::interfaces::motor::MotorStatus) -> Self {
        let mut msta = Self::empty();
        if status.direction {
            msta |= Self::DIRECTION;
        }
        if status.done {
            msta |= Self::DONE;
        }
        if status.moving {
            msta |= Self::MOVING;
        }
        if status.high_limit {
            msta |= Self::PLUS_LS;
        }
        if status.low_limit {
            msta |= Self::MINUS_LS;
        }
        if status.home {
            msta |= Self::HOME_LS;
        }
        if status.encoder_home {
            msta |= Self::EA_HOME;
        }
        if status.powered {
            msta |= Self::POSITION;
        }
        if status.problem {
            msta |= Self::PROBLEM;
        }
        if status.slip_stall {
            msta |= Self::SLIP_STALL;
        }
        if status.comms_error {
            msta |= Self::COMM_ERR;
        }
        if status.gain_support {
            msta |= Self::GAIN_SUPPORT;
        }
        if status.has_encoder {
            msta |= Self::ENCODER_PRESENT;
        }
        // RA_HOMED (bit 14) mirrors the driver status word like every
        // other MSTA bit. C copies pmr->msta wholesale from the driver
        // each poll (devMotorAsyn.c:467) and never record-manages
        // RA_HOMED — motorRecord.cc writes it nowhere. A record-side
        // sticky latch would report a permanently-homed axis after the
        // driver de-asserts homed (re-home, controller reset, a
        // SetPosition redefine the controller treats as un-homed).
        if status.homed {
            msta |= Self::HOMED;
        }
        msta
    }
}

/// Motor motion phase — internal state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MotionPhase {
    #[default]
    Idle,
    MainMove,
    BacklashFinal,
    Retry,
    Jog,
    JogStopping,
    JogBacklash,
    Homing,
    DelayWait,
}

/// SPMG mode — command gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpmgMode {
    Stop = 0,
    Pause = 1,
    Move = 2,
    #[default]
    Go = 3,
}

impl SpmgMode {
    pub fn from_i16(v: i16) -> Self {
        match v {
            0 => Self::Stop,
            1 => Self::Pause,
            2 => Self::Move,
            _ => Self::Go,
        }
    }
}

/// Motor direction for coordinate transform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MotorDir {
    #[default]
    Pos = 0,
    Neg = 1,
}

impl MotorDir {
    pub fn from_i16(v: i16) -> Self {
        match v {
            1 => Self::Neg,
            _ => Self::Pos,
        }
    }

    pub fn sign(&self) -> f64 {
        match self {
            Self::Pos => 1.0,
            Self::Neg => -1.0,
        }
    }
}

/// Freeze offset mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FreezeOffset {
    #[default]
    Variable = 0,
    Frozen = 1,
}

impl FreezeOffset {
    pub fn from_i16(v: i16) -> Self {
        match v {
            1 => Self::Frozen,
            _ => Self::Variable,
        }
    }
}

/// A jog or home command queued behind an in-progress motion (stop-then-X).
/// Tracked outside the MIP bitflags so it cannot be confused with an active
/// jog/home that a plain STOP has just halted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuedMotion {
    Jog { forward: bool },
    Home { forward: bool },
}

/// Restore mode for autosaved position at IOC startup.
/// C: `2906f3d8` (2020-06) / PR #160 — `menu(motorRSTM)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestoreMode {
    Never = 0,
    Always = 1,
    #[default]
    NearZero = 2,
    Conditional = 3,
}

impl RestoreMode {
    pub fn from_i16(v: i16) -> Self {
        match v {
            0 => Self::Never,
            1 => Self::Always,
            3 => Self::Conditional,
            _ => Self::NearZero,
        }
    }

    /// Whether the device support should write the autosaved DVAL back to the
    /// driver at init. `use_rel` is true when the driver supports incremental
    /// (relative) moves only; `dval_non_zero_pos_near_zero` is true when the
    /// autosaved DVAL is non-zero and the driver's current readback is near
    /// zero (matches C: `devMotorAsyn.c:199`).
    pub fn should_restore(self, use_rel: bool, dval_non_zero_pos_near_zero: bool) -> bool {
        match self {
            Self::Never => false,
            Self::Always => true,
            Self::NearZero => dval_non_zero_pos_near_zero,
            Self::Conditional => use_rel || dval_non_zero_pos_near_zero,
        }
    }
}

/// Which acceleration field drives motion — matches C motorRecord
/// `menu(motorACCSused)`: `Accl` (time-to-velocity, sec) or `Accs` (EGU/sec²).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccsUsed {
    #[default]
    Accl = 0,
    Accs = 1,
}

impl AccsUsed {
    pub fn from_i16(v: i16) -> Self {
        match v {
            1 => Self::Accs,
            _ => Self::Accl,
        }
    }
}

/// Retry mode — matches C motorRecord RMOD enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RetryMode {
    #[default]
    Default = 0,
    Arithmetic = 1,
    Geometric = 2,
    InPosition = 3,
}

impl RetryMode {
    pub fn from_i16(v: i16) -> Self {
        match v {
            1 => Self::Arithmetic,
            2 => Self::Geometric,
            3 => Self::InPosition,
            _ => Self::Default,
        }
    }
}

/// Motor event — why was process() called?
#[derive(Debug, Clone)]
pub enum MotorEvent {
    UserWrite(CommandSource),
    DeviceUpdate(asyn_rs::interfaces::motor::MotorStatus),
    DelayExpired,
}

/// Which field triggered the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    Val,
    Dval,
    Rval,
    Rlv,
    Stop,
    Jogf,
    Jogr,
    Homf,
    Homr,
    Twf,
    Twr,
    Spmg,
    Sync,
    Set,
    Cnen,
}

/// Commands to send to the motor driver, in C's raw units as
/// motorRecord's `build_trans` calls carry them: positions in steps
/// (`dial / mres`), speeds in steps/s and accelerations in steps/s²
/// (`egu / fabs(mres)`).
#[derive(Debug, Clone, PartialEq)]
pub enum MotorCommand {
    MoveAbsolute {
        position: f64,
        /// Base/start velocity (VBAS) sent to the driver ahead of the
        /// move — canonical asyn `SET_VEL_BASE`/`minVelocity`.
        min_velocity: f64,
        velocity: f64,
        acceleration: f64,
    },
    MoveRelative {
        distance: f64,
        min_velocity: f64,
        velocity: f64,
        acceleration: f64,
    },
    MoveVelocity {
        min_velocity: f64,
        /// Signed: C's `jogv = (jvel * dir) / mres` (motorRecord.cc:2119).
        velocity: f64,
        acceleration: f64,
    },
    Home {
        forward: bool,
        min_velocity: f64,
        velocity: f64,
        acceleration: f64,
    },
    /// Absolute move to a controller-defined home position (C: a6f64591).
    MoveToHome {
        position: f64,
        min_velocity: f64,
        velocity: f64,
        acceleration: f64,
    },
    Stop {
        acceleration: f64,
    },
    SetPosition {
        position: f64,
    },
    /// C init_controller's position load (devMotorAsyn.c:216-231): a
    /// SyncIO write that init_controller reports itself rather than through
    /// asynCallback.
    InitPosition {
        position: f64,
    },
    /// MOTOR_ENCODER_POSITION written to the controller port
    /// (asynMotorController.cpp:367-372); no record command sends it.
    SetEncoderPosition {
        position: f64,
    },
    /// do_work's SET_ENC_RATIO (motorRecord.cc:1973-1978): encoder and
    /// motor pulses per scaled count, C's `ep_mp`, which devMotorAsyn's
    /// build_trans divides.
    SetEncoderRatio {
        pulses: [f64; 2],
    },
    /// C init_controller's encoder-ratio write (devMotorAsyn.c:186-198),
    /// reported by init_controller like [`InitPosition`](Self::InitPosition).
    InitEncoderRatio {
        ratio: f64,
    },
    SetClosedLoop {
        enable: bool,
    },
    DeferMoves {
        defer: bool,
    },
    Poll,
    /// A model-1 record's OUT parm, `mr->out.value.vmeio.parm`, sent when
    /// the record binds its axis.
    OutParm {
        parm: PvString,
    },
    /// A model-1 record's INIT text, C `build_trans(PRIMITIVE)` at init
    /// (motordevCom.cc:309-327); no model-2/3 device sends it.
    Primitive {
        init: PvString,
    },
    /// The record's PREM and POST, ahead of the move they belong to: C
    /// `build_trans` reads PREM at each move it builds, and arms the move
    /// with the POST field itself when it is set then.
    MoveStrings {
        prem: PvString,
        post: Option<PostMessage>,
    },
    /// The record's CDIR, ahead of the move it belongs to: what a model-1
    /// driver polling that move reads as `mr->cdir`.
    MoveDirection {
        forward: bool,
    },
    ProfileInitialize {
        max_points: usize,
    },
    /// The axis's profile in controller units, as a C `defineProfile`
    /// override has it once the base has converted it.
    ProfileDefine {
        positions: Vec<f64>,
    },
    ProfileBuild,
    ProfileExecute,
    ProfileAbort,
    ProfileReadback,
    /// Forward a closed-loop gain coefficient (C special pidcof,
    /// motorRecord.cc 3003-3026: GAIN_SUPPORT-gated, clamped 0.0–1.0
    /// before emission → SET_PGAIN/SET_IGAIN/SET_DGAIN).
    SetPidGain {
        kind: PidGainKind,
        gain: f64,
    },
    /// The controller's high raw limit (SET_HIGH_LIMIT): the dial high
    /// limit, or the dial low one under a negative MRES (C
    /// set_dial_highlimit / set_dial_lowlimit, motorRecord.cc 4236-4328).
    SetHighLimit {
        position: f64,
    },
    /// The controller's low raw limit (SET_LOW_LIMIT).
    SetLowLimit {
        position: f64,
    },
    /// The record's SET_VELOCITY: stored on the axis for its next move
    /// (devMotorAsyn.c:549-552).
    SetVelocity {
        velocity: f64,
    },
    /// The record's SET_VEL_BASE (devMotorAsyn.c:545-548).
    SetVelBase {
        velocity: f64,
    },
    /// The record's SET_ACCEL (devMotorAsyn.c:553-556).
    SetAccel {
        acceleration: f64,
    },
}

/// Which of SET_VELOCITY, SET_VEL_BASE and SET_ACCEL a motorRecord call site
/// writes ahead of its move or jog.
#[derive(Debug, Clone, Copy)]
pub struct SpeedWrites {
    velocity: bool,
    vel_base: bool,
    /// SET_ACCEL only at a positive acceleration, else always.
    accel_if_positive: bool,
}

impl SpeedWrites {
    /// A move, a home, and the jog backlash take-out leg (motorRecord.cc:
    /// 872-876, :938-942, :2052-2056, :2527-2531).
    pub const MOVE: Self = Self {
        velocity: true,
        vel_base: true,
        accel_if_positive: true,
    };
    /// A backlash final leg, which writes no SET_VEL_BASE (:955-958,
    /// :1004-1007).
    pub const BACKLASH: Self = Self {
        velocity: true,
        vel_base: false,
        accel_if_positive: true,
    };
    /// A jog start (:2140-2142).
    pub const JOG: Self = Self {
        velocity: false,
        vel_base: true,
        accel_if_positive: false,
    };
    /// A JVEL write retuning a running jog (:3068-3069).
    pub const JOG_RETUNE: Self = Self {
        velocity: false,
        vel_base: false,
        accel_if_positive: false,
    };
}

/// The speeds an axis moves on: what the record's speed writes stored
/// last, C's `motorVelBase_` / `motorVelocity_` / `motorAccel_` parameters
/// or drvMotorAsyn's `min_velocity` / `max_velocity` / `accel`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StoredSpeeds {
    pub min_velocity: f64,
    pub velocity: f64,
    pub acceleration: f64,
}

impl MotorCommand {
    /// devMotorAsyn's `commandIsMove` (devMotorAsyn.c:686-691): a command
    /// whose completion processes the record, counted in
    /// `moveRequestPending` from `build_trans` on.
    pub fn is_move(&self) -> bool {
        matches!(
            self,
            Self::MoveAbsolute { .. }
                | Self::MoveRelative { .. }
                | Self::MoveVelocity { .. }
                | Self::Home { .. }
                | Self::SetPosition { .. }
        )
    }

    /// The ratio an encoder-ratio write carries.
    pub fn encoder_ratio(&self) -> Option<f64> {
        match *self {
            Self::SetEncoderRatio {
                pulses: [encoder, motor],
            } => Some(encoder / motor),
            Self::InitEncoderRatio { ratio } => Some(ratio),
            _ => None,
        }
    }

    /// The record's STOP_AXIS, which carries no SET_ACCEL (motorRecord.cc:
    /// 1337, :1886, :1909, :2031, :2114, :2156): the axis stops on its
    /// stored acceleration.
    pub const fn stop_axis() -> Self {
        Self::Stop { acceleration: 0.0 }
    }

    /// `self` preceded by the speed writes its call site makes, valued from
    /// its own fields, in motorRecord.cc's order.
    pub fn after_speeds(self, writes: SpeedWrites) -> Vec<MotorCommand> {
        let (min_velocity, velocity, acceleration) = match self {
            Self::MoveAbsolute {
                min_velocity,
                velocity,
                acceleration,
                ..
            }
            | Self::MoveRelative {
                min_velocity,
                velocity,
                acceleration,
                ..
            }
            | Self::Home {
                min_velocity,
                velocity,
                acceleration,
                ..
            } => (min_velocity, Some(velocity), acceleration),
            // A jog's velocity is its own message's value.
            Self::MoveVelocity {
                min_velocity,
                acceleration,
                ..
            } => (min_velocity, None, acceleration),
            _ => return vec![self],
        };
        let mut commands = Vec::with_capacity(4);
        if let (true, Some(velocity)) = (writes.velocity, velocity) {
            commands.push(Self::SetVelocity { velocity });
        }
        if writes.vel_base {
            commands.push(Self::SetVelBase {
                velocity: min_velocity,
            });
        }
        if !writes.accel_if_positive || acceleration > 0.0 {
            commands.push(Self::SetAccel { acceleration });
        }
        commands.push(self);
        commands
    }

    /// This move or stop on `speeds` in place of its own; a jog keeps its
    /// velocity, which is its message's value.
    pub fn on_speeds(&self, speeds: StoredSpeeds) -> Self {
        let mut command = self.clone();
        match &mut command {
            Self::MoveAbsolute {
                min_velocity,
                velocity,
                acceleration,
                ..
            }
            | Self::MoveRelative {
                min_velocity,
                velocity,
                acceleration,
                ..
            }
            | Self::Home {
                min_velocity,
                velocity,
                acceleration,
                ..
            } => {
                *min_velocity = speeds.min_velocity;
                *velocity = speeds.velocity;
                *acceleration = speeds.acceleration;
            }
            Self::MoveVelocity {
                min_velocity,
                acceleration,
                ..
            } => {
                *min_velocity = speeds.min_velocity;
                *acceleration = speeds.acceleration;
            }
            Self::Stop { acceleration } => *acceleration = speeds.acceleration,
            _ => {}
        }
        command
    }

    /// A move or stop, which an axis with a speed store runs on its stored
    /// speeds.
    pub fn runs_on_speeds(&self) -> bool {
        matches!(
            self,
            Self::MoveAbsolute { .. }
                | Self::MoveRelative { .. }
                | Self::MoveVelocity { .. }
                | Self::Home { .. }
                | Self::Stop { .. }
        )
    }

    /// The record's SET_VELOCITY, SET_VEL_BASE or SET_ACCEL.
    pub fn is_speed_write(&self) -> bool {
        matches!(
            self,
            Self::SetVelocity { .. } | Self::SetVelBase { .. } | Self::SetAccel { .. }
        )
    }

    /// Store this speed write in `speeds`; whether it is one.
    pub fn store_speed(&self, speeds: &mut StoredSpeeds) -> bool {
        match *self {
            Self::SetVelocity { velocity } => speeds.velocity = velocity,
            Self::SetVelBase { velocity } => speeds.min_velocity = velocity,
            Self::SetAccel { acceleration } => speeds.acceleration = acceleration,
            _ => return false,
        }
        true
    }
}

/// Effects returned by process logic.
#[derive(Debug, Default)]
pub struct ProcessEffects {
    pub commands: Vec<MotorCommand>,
    pub schedule_delay: Option<std::time::Duration>,
    /// A forced poll no C GET_INFO asks for: it prints nothing and posts
    /// even an unchanged status. A C GET_INFO is a [`MotorCommand::Poll`].
    pub request_poll: bool,
    /// Not consulted: FLNK is gated solely on DMOV at process exit
    /// (C motorRecord.cc:1509-1510, `if (dmov != 0) recGblFwdLink`) —
    /// the record core neither sets nor reads this. Retained only so
    /// the public struct shape is unchanged.
    pub suppress_forward_link: bool,
}

/// Retarget action when a new target arrives during motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetargetAction {
    Ignore,
    StopAndReplan,
    ExtendMove,
}

/// Motor record errors.
#[derive(Debug)]
pub enum MotorError {
    CommunicationError(String),
    InvalidStateTransition { from: MotionPhase, event: String },
    LimitViolation,
    InvalidFieldValue(String),
}

impl std::fmt::Display for MotorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CommunicationError(s) => write!(f, "communication error: {s}"),
            Self::InvalidStateTransition { from, event } => {
                write!(f, "invalid state transition from {from:?} on {event}")
            }
            Self::LimitViolation => write!(f, "soft limit violation"),
            Self::InvalidFieldValue(s) => write!(f, "invalid field value: {s}"),
        }
    }
}

impl std::error::Error for MotorError {}
