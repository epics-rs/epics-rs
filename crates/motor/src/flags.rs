use bitflags::bitflags;

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
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct MstaFlags: u32 {
        const DIRECTION       = 0x0001;
        const DONE            = 0x0002;
        const PLUS_LS         = 0x0004;
        const HOME_LS         = 0x0008;
        const POSITION        = 0x0020;
        const ENCODER_PRESENT = 0x0040;
        const PROBLEM         = 0x0080;
        const MOVING          = 0x0100;
        const GAIN_SUPPORT    = 0x0200;
        const COMM_ERR        = 0x0400;
        const MINUS_LS        = 0x0800;
        const HOMED           = 0x1000;
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
}

/// Commands to send to the motor driver.
#[derive(Debug, Clone, PartialEq)]
pub enum MotorCommand {
    MoveAbsolute {
        position: f64,
        velocity: f64,
        acceleration: f64,
    },
    MoveVelocity {
        direction: bool,
        velocity: f64,
        acceleration: f64,
    },
    Home {
        forward: bool,
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
    Poll,
}

/// Effects returned by process logic.
#[derive(Debug, Default)]
pub struct ProcessEffects {
    pub commands: Vec<MotorCommand>,
    pub schedule_delay: Option<std::time::Duration>,
    pub request_poll: bool,
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
