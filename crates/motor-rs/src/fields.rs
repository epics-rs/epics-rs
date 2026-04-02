use crate::flags::*;

/// Position-related fields.
#[derive(Debug, Clone)]
pub struct PositionFields {
    pub val: f64,
    pub rbv: f64,
    pub rlv: f64,
    pub off: f64,
    pub diff: f64,
    pub rdif: f64,
    pub dval: f64,
    pub drbv: f64,
    pub rval: i32,
    pub rrbv: i32,
    pub rmp: i32,
    pub rep: i32,
}

impl Default for PositionFields {
    fn default() -> Self {
        Self {
            val: 0.0,
            rbv: 0.0,
            rlv: 0.0,
            off: 0.0,
            diff: 0.0,
            rdif: 0.0,
            dval: 0.0,
            drbv: 0.0,
            rval: 0,
            rrbv: 0,
            rmp: 0,
            rep: 0,
        }
    }
}

/// Coordinate conversion fields.
#[derive(Debug, Clone)]
pub struct ConversionFields {
    pub dir: MotorDir,
    pub foff: FreezeOffset,
    pub set: bool,
    pub igset: bool,
    pub mres: f64,
    pub eres: f64,
    pub srev: i32,
    pub urev: f64,
    pub ueip: bool,
    pub urip: bool,
    pub rres: f64,
}

impl Default for ConversionFields {
    fn default() -> Self {
        Self {
            dir: MotorDir::Pos,
            foff: FreezeOffset::Variable,
            set: false,
            igset: false,
            mres: 1.0,
            eres: 0.0,
            srev: 200,
            urev: 1.0,
            ueip: false,
            urip: false,
            rres: 0.0,
        }
    }
}

/// Velocity and acceleration fields.
#[derive(Debug, Clone)]
pub struct VelocityFields {
    pub velo: f64,
    pub vbas: f64,
    pub vmax: f64,
    pub s: f64,
    pub sbas: f64,
    pub smax: f64,
    pub accl: f64,
    pub bvel: f64,
    pub bacc: f64,
    pub hvel: f64,
    pub jvel: f64,
    pub jar: f64,
    pub sbak: f64,
}

impl Default for VelocityFields {
    fn default() -> Self {
        Self {
            velo: 1.0,
            vbas: 0.0,
            vmax: 0.0,
            s: 0.0,
            sbas: 0.0,
            smax: 0.0,
            accl: 0.5,
            bvel: 1.0,
            bacc: 0.5,
            hvel: 1.0,
            jvel: 1.0,
            jar: 0.0,
            sbak: 0.0,
        }
    }
}

/// Retry and backlash fields.
#[derive(Debug, Clone)]
pub struct RetryFields {
    pub bdst: f64,
    pub frac: f64,
    pub rdbd: f64,
    pub spdb: f64,
    pub rtry: i16,
    pub rmod: RetryMode,
    pub rcnt: i16,
    pub miss: bool,
}

impl Default for RetryFields {
    fn default() -> Self {
        Self {
            bdst: 0.0,
            frac: 1.0,
            rdbd: 0.0,
            spdb: 0.0,
            rtry: 10,
            rmod: RetryMode::Arithmetic,
            rcnt: 0,
            miss: false,
        }
    }
}

/// Limit fields.
#[derive(Debug, Clone)]
pub struct LimitFields {
    pub hlm: f64,
    pub llm: f64,
    pub dhlm: f64,
    pub dllm: f64,
    pub lvio: bool,
    pub hls: bool,
    pub lls: bool,
    pub hlsv: i16,
}

impl Default for LimitFields {
    fn default() -> Self {
        Self {
            hlm: 0.0,
            llm: 0.0,
            dhlm: 0.0,
            dllm: 0.0,
            lvio: true,
            hls: false,
            lls: false,
            hlsv: 0,
        }
    }
}

/// Control fields (user commands).
#[derive(Debug, Clone)]
pub struct ControlFields {
    pub spmg: SpmgMode,
    pub stop: bool,
    pub homf: bool,
    pub homr: bool,
    pub jogf: bool,
    pub jogr: bool,
    pub twf: bool,
    pub twr: bool,
    pub twv: f64,
    pub cnen: bool,
}

impl Default for ControlFields {
    fn default() -> Self {
        Self {
            spmg: SpmgMode::Go,
            stop: false,
            homf: false,
            homr: false,
            jogf: false,
            jogr: false,
            twf: false,
            twr: false,
            twv: 1.0,
            cnen: false,
        }
    }
}

/// Status fields.
#[derive(Debug, Clone)]
pub struct StatusFields {
    pub dmov: bool,
    pub movn: bool,
    pub msta: MstaFlags,
    pub mip: MipFlags,
    pub phase: MotionPhase,
    pub cdir: bool,
    pub tdir: bool,
    pub athm: bool,
    pub stup: i16,
}

impl Default for StatusFields {
    fn default() -> Self {
        Self {
            dmov: true,
            movn: false,
            msta: MstaFlags::empty(),
            mip: MipFlags::empty(),
            phase: MotionPhase::Idle,
            cdir: false,
            tdir: false,
            athm: false,
            stup: 0,
        }
    }
}

/// PID fields (placeholder).
#[derive(Debug, Clone, Default)]
pub struct PidFields {
    pub pcof: f64,
    pub icof: f64,
    pub dcof: f64,
}

/// Display fields.
#[derive(Debug, Clone)]
pub struct DisplayFields {
    pub egu: String,
    pub prec: i16,
    pub adel: f64,
    pub mdel: f64,
    pub alst: f64,
    pub mlst: f64,
}

impl Default for DisplayFields {
    fn default() -> Self {
        Self {
            egu: String::new(),
            prec: 0,
            adel: 0.0,
            mdel: 0.0,
            alst: 0.0,
            mlst: 0.0,
        }
    }
}

/// Timing fields.
#[derive(Debug, Clone)]
pub struct TimingFields {
    pub dly: f64,
    pub ntm: bool,
    pub ntmf: f64,
}

impl Default for TimingFields {
    fn default() -> Self {
        Self {
            dly: 0.0,
            ntm: true,
            ntmf: 2.0,
        }
    }
}

/// Internal bookkeeping fields (not directly exposed as PVs).
#[derive(Debug, Clone, Default)]
pub struct InternalFields {
    pub lval: f64,
    pub ldvl: f64,
    pub lrvl: i32,
    pub lspg: SpmgMode,
    pub pp: bool,
    pub sync: bool,
    /// Backlash final move pending after MainMove completes
    pub backlash_pending: bool,
    /// Pending retarget value (for NTM stop-and-replan)
    pub pending_retarget: Option<f64>,
}
