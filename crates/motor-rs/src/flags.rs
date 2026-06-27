use bitflags::bitflags;

pub use asyn_rs::interfaces::motor::PidGainKind;

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
        /// Driver does not support a base velocity (VBAS).
        /// epics-modules/motor issue #76 (proposal PR #80/#81, unmerged).
        /// When set, record-level acceleration calculations should treat
        /// VBAS as effectively 0.
        const VBAS_UNSUPPORTED = 0x8000; // bit 15
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
    Startup,
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
    /// PCO_ENABLE write — pushes PCO config then enable/disable (PR #248).
    PcoEnable,
}

/// Commands to send to the motor driver.
#[derive(Debug, Clone, PartialEq)]
pub enum MotorCommand {
    MoveAbsolute {
        position: f64,
        /// Base/start velocity (VBAS, EGU/sec) sent to the driver ahead of the
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
        /// DIAL-frame forward. The jog button is USER-frame; the
        /// planner folds DIR before emitting (C motorRecord.cc:2119
        /// commands `jogv = (jvel * dir) / mres` — the velocity sign
        /// carries DIR).
        direction: bool,
        min_velocity: f64,
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
    SetClosedLoop {
        enable: bool,
    },
    DeferMoves {
        defer: bool,
    },
    Poll,
    ProfileInitialize {
        max_points: usize,
    },
    ProfileBuild,
    ProfileExecute,
    ProfileAbort,
    ProfileReadback,
    /// Enable or disable position-compare output (C: `05b25c1d`, PR #248).
    EnablePco {
        enable: bool,
    },
    /// Configure position-compare output (C: `05b25c1d`, PR #248).
    SetPcoConfig {
        start: f64,
        end: f64,
        increment: f64,
        pulse_width_us: f64,
    },
    /// Forward a closed-loop gain coefficient (C special pidcof,
    /// motorRecord.cc 3003-3026: GAIN_SUPPORT-gated, clamped 0.0–1.0
    /// before emission → SET_PGAIN/SET_IGAIN/SET_DGAIN).
    SetPidGain {
        kind: PidGainKind,
        gain: f64,
    },
    /// Forward the high soft-travel limit, dial-frame EGU (C
    /// set_dial_highlimit, motorRecord.cc 4236-4277). C's wire value is
    /// raw steps (dhlm/mres) with an MRES-sign register swap; the
    /// AsynMotor boundary speaks dial EGU (like SetPosition), so the
    /// dial high limit is forwarded as the high limit unconditionally —
    /// the raw-frame fold lives where the raw pair is tracked
    /// (RHLM/RLLM).
    SetHighLimit {
        position: f64,
    },
    /// Forward the low soft-travel limit, dial-frame EGU (C
    /// set_dial_lowlimit, motorRecord.cc 4287-4328).
    SetLowLimit {
        position: f64,
    },
}

/// Effects returned by process logic.
#[derive(Debug, Default)]
pub struct ProcessEffects {
    pub commands: Vec<MotorCommand>,
    pub schedule_delay: Option<std::time::Duration>,
    pub request_poll: bool,
    /// Not consulted: FLNK is gated solely on DMOV at process exit
    /// (C motorRecord.cc:1509-1510, `if (dmov != 0) recGblFwdLink`) —
    /// the record core neither sets nor reads this. Retained only so
    /// the public struct shape is unchanged.
    pub suppress_forward_link: bool,
    pub status_refresh: bool,
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
