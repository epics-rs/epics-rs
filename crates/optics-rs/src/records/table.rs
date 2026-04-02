// table.rs — EPICS 6-DOF optical table record
//
// Ported from EPICS synApps optics `tableRecord.c` (v5.14, Tim Mooney, APS).
// Supports SRI, GEOCARS, NEWPORT, and PNC geometry modes with full coordinate
// transformation between user space (Ax,Ay,Az,X,Y,Z) and motor space
// (M0X,M0Y,M1Y,M2X,M2Y,M2Z).

use std::any::Any;
use std::f64::consts::PI;
use std::sync::LazyLock;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::record::{FieldDesc, ProcessAction, ProcessOutcome, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const VERSION: f32 = 5.14;

/// Degrees to radians.  The table record uses its own constant, separate from
/// the orient module.
const D2R: f64 = PI / 180.0;

/// Threshold below which a value is considered zero for limit checks.
const SMALL: f64 = 1.0e-6;

/// "Infinite" sentinel for user limits.
const LARGE: f64 = 1.0e9;

/// Number of trajectory samples used in the binary-search limit finder.
const NTRAJ: usize = 13;

/// Initial angular step (degrees) for the limit search.
const DELTA_START: f64 = 10.0;

// User-coordinate indices
const AX_6: usize = 0;
const AY_6: usize = 1;
const AZ_6: usize = 2;
const X_6: usize = 3;
const Y_6: usize = 4;
const Z_6: usize = 5;

// Pivot-point component indices
const X: usize = 0;
const Y: usize = 1;
const Z: usize = 2;

// Motor indices
const M0X: usize = 0;
const M0Y: usize = 1;
const M1Y: usize = 2;
const M2X: usize = 3;
const M2Y: usize = 4;
const M2Z: usize = 5;

// Motor field name lookup tables
const MOTOR_DRIVE_LINK: [&str; 6] = ["M0XL", "M0YL", "M1YL", "M2XL", "M2YL", "M2ZL"];
#[allow(dead_code)]
const MOTOR_DRIVE_VAL: [&str; 6] = ["M0X", "M0Y", "M1Y", "M2X", "M2Y", "M2Z"];
const MOTOR_RBV_LINK: [&str; 6] = ["R0XI", "R0YI", "R1YI", "R2XI", "R2YI", "R2ZI"];
const MOTOR_RBV_VAL: [&str; 6] = ["R0X", "R0Y", "R1Y", "R2X", "R2Y", "R2Z"];
const ENCODER_LINK: [&str; 6] = ["E0XI", "E0YI", "E1YI", "E2XI", "E2YI", "E2ZI"];
const ENCODER_VAL: [&str; 6] = ["E0X", "E0Y", "E1Y", "E2X", "E2Y", "E2Z"];
const SPEED_OUT_LINK: [&str; 6] = ["V0XL", "V0YL", "V1YL", "V2XL", "V2YL", "V2ZL"];
const SPEED_VAL: [&str; 6] = ["V0X", "V0Y", "V1Y", "V2X", "V2Y", "V2Z"];
const SPEED_IN_LINK: [&str; 6] = ["V0XI", "V0YI", "V1YI", "V2XI", "V2YI", "V2ZI"];
const HI_LIMIT_LINK: [&str; 6] = ["H0XL", "H0YL", "H1YL", "H2XL", "H2YL", "H2ZL"];
const HI_LIMIT_VAL: [&str; 6] = ["H0X", "H0Y", "H1Y", "H2X", "H2Y", "H2Z"];
const LO_LIMIT_LINK: [&str; 6] = ["L0XL", "L0YL", "L1YL", "L2XL", "L2YL", "L2ZL"];
const LO_LIMIT_VAL: [&str; 6] = ["L0X", "L0Y", "L1Y", "L2X", "L2Y", "L2Z"];

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Table SET mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum SetMode {
    Use = 0,
    Set = 1,
}

impl SetMode {
    fn from_u16(v: u16) -> Self {
        if v == 0 { SetMode::Use } else { SetMode::Set }
    }
}

/// Geometry selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Geometry {
    Sri = 0,
    Geocars = 1,
    Newport = 2,
    Pnc = 3,
}

impl Geometry {
    fn from_u16(v: u16) -> Self {
        match v {
            0 => Geometry::Sri,
            1 => Geometry::Geocars,
            2 => Geometry::Newport,
            3 => Geometry::Pnc,
            _ => Geometry::Sri,
        }
    }
}

/// Angle unit selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum AngleUnit {
    Degrees = 0,
    Microradians = 1,
}

impl AngleUnit {
    fn from_u16(v: u16) -> Self {
        if v == 0 {
            AngleUnit::Degrees
        } else {
            AngleUnit::Microradians
        }
    }

    fn torad(self) -> f64 {
        match self {
            AngleUnit::Degrees => PI / 180.0,
            AngleUnit::Microradians => 1.0e-6,
        }
    }

    fn label(self) -> &'static str {
        match self {
            AngleUnit::Degrees => "degrees",
            AngleUnit::Microradians => "ur",
        }
    }
}

// ---------------------------------------------------------------------------
// Link status per motor
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
struct LinkStatus {
    can_rw_drive: bool,
    can_read_position: bool,
    can_read_limits: bool,
    can_rw_speed: bool,
}

// ---------------------------------------------------------------------------
// Trajectory point (for limit search)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Trajectory {
    user: f64,
    motor: [f64; 6],
    lvio: bool,
}

impl Default for Trajectory {
    fn default() -> Self {
        Self {
            user: 0.0,
            motor: [0.0; 6],
            lvio: false,
        }
    }
}

// ---------------------------------------------------------------------------
// TableRecord
// ---------------------------------------------------------------------------

/// EPICS 6-DOF optical table record.
pub struct TableRecord {
    // --- Version ---
    pub vers: f32,
    pub val: f64,

    // --- Geometry parameters ---
    pub lx: f64,
    pub lz: f64,
    pub sx: f64,
    pub sy: f64,
    pub sz: f64,
    pub rx: f64,
    pub ry: f64,
    pub rz: f64,
    pub yang: f64,

    // --- User coordinates (drive) ---
    pub ax: f64,
    pub ay: f64,
    pub az: f64,
    pub x: f64,
    pub y: f64,
    pub z: f64,

    // --- User offsets ---
    pub ax0: f64,
    pub ay0: f64,
    pub az0: f64,
    pub x0: f64,
    pub y0: f64,
    pub z0: f64,

    // --- True values (drive + offset) ---
    pub axl: f64,
    pub ayl: f64,
    pub azl: f64,
    pub xl: f64,
    pub yl: f64,
    pub zl: f64,

    // --- Readback (user coords) ---
    pub axrb: f64,
    pub ayrb: f64,
    pub azrb: f64,
    pub xrb: f64,
    pub yrb: f64,
    pub zrb: f64,

    // --- Encoder user coords ---
    pub eax: f64,
    pub eay: f64,
    pub eaz: f64,
    pub ex: f64,
    pub ey: f64,
    pub ez: f64,

    // --- Calculated user limits (from motor limits) ---
    pub hlax: f64,
    pub hlay: f64,
    pub hlaz: f64,
    pub hlx: f64,
    pub hly: f64,
    pub hlz: f64,
    pub llax: f64,
    pub llay: f64,
    pub llaz: f64,
    pub llx: f64,
    pub lly: f64,
    pub llz: f64,

    // --- User limits (absolute) ---
    pub uhax: f64,
    pub uhay: f64,
    pub uhaz: f64,
    pub uhx: f64,
    pub uhy: f64,
    pub uhz: f64,
    pub ulax: f64,
    pub ulay: f64,
    pub ulaz: f64,
    pub ulx: f64,
    pub uly: f64,
    pub ulz: f64,

    // --- User limits (relative, varies with offset) ---
    pub uhaxr: f64,
    pub uhayr: f64,
    pub uhazr: f64,
    pub uhxr: f64,
    pub uhyr: f64,
    pub uhzr: f64,
    pub ulaxr: f64,
    pub ulayr: f64,
    pub ulazr: f64,
    pub ulxr: f64,
    pub ulyr: f64,
    pub ulzr: f64,

    // --- Motor drive output links (PV names) ---
    pub m0xl: String,
    pub m0yl: String,
    pub m1yl: String,
    pub m2xl: String,
    pub m2yl: String,
    pub m2zl: String,

    // --- Motor drive values ---
    pub m0x: f64,
    pub m0y: f64,
    pub m1y: f64,
    pub m2x: f64,
    pub m2y: f64,
    pub m2z: f64,

    // --- Motor readback input links ---
    pub r0xi: String,
    pub r0yi: String,
    pub r1yi: String,
    pub r2xi: String,
    pub r2yi: String,
    pub r2zi: String,

    // --- Motor readback values ---
    pub r0x: f64,
    pub r0y: f64,
    pub r1y: f64,
    pub r2x: f64,
    pub r2y: f64,
    pub r2z: f64,

    // --- Encoder input links ---
    pub e0xi: String,
    pub e0yi: String,
    pub e1yi: String,
    pub e2xi: String,
    pub e2yi: String,
    pub e2zi: String,

    // --- Encoder values ---
    pub e0x: f64,
    pub e0y: f64,
    pub e1y: f64,
    pub e2x: f64,
    pub e2y: f64,
    pub e2z: f64,

    // --- Speed output links ---
    pub v0xl: String,
    pub v0yl: String,
    pub v1yl: String,
    pub v2xl: String,
    pub v2yl: String,
    pub v2zl: String,

    // --- Speed values ---
    pub v0x: f64,
    pub v0y: f64,
    pub v1y: f64,
    pub v2x: f64,
    pub v2y: f64,
    pub v2z: f64,

    // --- Speed input links ---
    pub v0xi: String,
    pub v0yi: String,
    pub v1yi: String,
    pub v2xi: String,
    pub v2yi: String,
    pub v2zi: String,

    // --- Motor high limit input links ---
    pub h0xl: String,
    pub h0yl: String,
    pub h1yl: String,
    pub h2xl: String,
    pub h2yl: String,
    pub h2zl: String,

    // --- Motor high limit values ---
    pub h0x: f64,
    pub h0y: f64,
    pub h1y: f64,
    pub h2x: f64,
    pub h2y: f64,
    pub h2z: f64,

    // --- Motor low limit input links ---
    pub l0xl: String,
    pub l0yl: String,
    pub l1yl: String,
    pub l2xl: String,
    pub l2yl: String,
    pub l2zl: String,

    // --- Motor low limit values ---
    pub l0x: f64,
    pub l0y: f64,
    pub l1y: f64,
    pub l2x: f64,
    pub l2y: f64,
    pub l2z: f64,

    // --- Control fields ---
    pub init: i16,
    pub zero: i16,
    pub sync: i16,
    pub read: i16,
    pub set: SetMode,
    pub sset: i16,
    pub suse: i16,
    pub lvio: i16,

    // --- Display ---
    pub legu: String,
    pub aegu: String,
    pub prec: i16,
    pub mmap: u32,
    pub geom: Geometry,
    pub torad: f64,
    pub aunit: AngleUnit,

    // --- Internal: pivot points ---
    pp0: [f64; 3],
    pp1: [f64; 3],
    pp2: [f64; 3],
    ppo0: [f64; 3],
    ppo1: [f64; 3],
    ppo2: [f64; 3],

    // --- Internal: rotation matrix (user→motor) and inverse ---
    a: [[f64; 3]; 3],
    b: [[f64; 3]; 3],

    // --- Internal: link status ---
    lnk_stat: [LinkStatus; 6],

    // --- Internal: previous angle unit (for conversion) ---
    curr_aunit: AngleUnit,
}

impl Default for TableRecord {
    fn default() -> Self {
        Self {
            vers: VERSION,
            val: 0.0,
            lx: 0.0,
            lz: 0.0,
            sx: 0.0,
            sy: 0.0,
            sz: 0.0,
            rx: 0.0,
            ry: 0.0,
            rz: 0.0,
            yang: 0.0,
            ax: 0.0,
            ay: 0.0,
            az: 0.0,
            x: 0.0,
            y: 0.0,
            z: 0.0,
            ax0: 0.0,
            ay0: 0.0,
            az0: 0.0,
            x0: 0.0,
            y0: 0.0,
            z0: 0.0,
            axl: 0.0,
            ayl: 0.0,
            azl: 0.0,
            xl: 0.0,
            yl: 0.0,
            zl: 0.0,
            axrb: 0.0,
            ayrb: 0.0,
            azrb: 0.0,
            xrb: 0.0,
            yrb: 0.0,
            zrb: 0.0,
            eax: 0.0,
            eay: 0.0,
            eaz: 0.0,
            ex: 0.0,
            ey: 0.0,
            ez: 0.0,
            hlax: 0.0,
            hlay: 0.0,
            hlaz: 0.0,
            hlx: 0.0,
            hly: 0.0,
            hlz: 0.0,
            llax: 0.0,
            llay: 0.0,
            llaz: 0.0,
            llx: 0.0,
            lly: 0.0,
            llz: 0.0,
            uhax: 0.0,
            uhay: 0.0,
            uhaz: 0.0,
            uhx: 0.0,
            uhy: 0.0,
            uhz: 0.0,
            ulax: 0.0,
            ulay: 0.0,
            ulaz: 0.0,
            ulx: 0.0,
            uly: 0.0,
            ulz: 0.0,
            uhaxr: 0.0,
            uhayr: 0.0,
            uhazr: 0.0,
            uhxr: 0.0,
            uhyr: 0.0,
            uhzr: 0.0,
            ulaxr: 0.0,
            ulayr: 0.0,
            ulazr: 0.0,
            ulxr: 0.0,
            ulyr: 0.0,
            ulzr: 0.0,
            m0xl: String::new(),
            m0yl: String::new(),
            m1yl: String::new(),
            m2xl: String::new(),
            m2yl: String::new(),
            m2zl: String::new(),
            m0x: 0.0,
            m0y: 0.0,
            m1y: 0.0,
            m2x: 0.0,
            m2y: 0.0,
            m2z: 0.0,
            r0xi: String::new(),
            r0yi: String::new(),
            r1yi: String::new(),
            r2xi: String::new(),
            r2yi: String::new(),
            r2zi: String::new(),
            r0x: 0.0,
            r0y: 0.0,
            r1y: 0.0,
            r2x: 0.0,
            r2y: 0.0,
            r2z: 0.0,
            e0xi: String::new(),
            e0yi: String::new(),
            e1yi: String::new(),
            e2xi: String::new(),
            e2yi: String::new(),
            e2zi: String::new(),
            e0x: 0.0,
            e0y: 0.0,
            e1y: 0.0,
            e2x: 0.0,
            e2y: 0.0,
            e2z: 0.0,
            v0xl: String::new(),
            v0yl: String::new(),
            v1yl: String::new(),
            v2xl: String::new(),
            v2yl: String::new(),
            v2zl: String::new(),
            v0x: 0.0,
            v0y: 0.0,
            v1y: 0.0,
            v2x: 0.0,
            v2y: 0.0,
            v2z: 0.0,
            v0xi: String::new(),
            v0yi: String::new(),
            v1yi: String::new(),
            v2xi: String::new(),
            v2yi: String::new(),
            v2zi: String::new(),
            h0xl: String::new(),
            h0yl: String::new(),
            h1yl: String::new(),
            h2xl: String::new(),
            h2yl: String::new(),
            h2zl: String::new(),
            h0x: 0.0,
            h0y: 0.0,
            h1y: 0.0,
            h2x: 0.0,
            h2y: 0.0,
            h2z: 0.0,
            l0xl: String::new(),
            l0yl: String::new(),
            l1yl: String::new(),
            l2xl: String::new(),
            l2yl: String::new(),
            l2zl: String::new(),
            l0x: 0.0,
            l0y: 0.0,
            l1y: 0.0,
            l2x: 0.0,
            l2y: 0.0,
            l2z: 0.0,
            init: 0,
            zero: 0,
            sync: 0,
            read: 0,
            set: SetMode::Use,
            sset: 0,
            suse: 0,
            lvio: 0,
            legu: String::new(),
            aegu: "degrees".into(),
            prec: 0,
            mmap: 0,
            geom: Geometry::Sri,
            torad: D2R,
            aunit: AngleUnit::Degrees,
            pp0: [0.0; 3],
            pp1: [0.0; 3],
            pp2: [0.0; 3],
            ppo0: [0.0; 3],
            ppo1: [0.0; 3],
            ppo2: [0.0; 3],
            a: [[0.0; 3]; 3],
            b: [[0.0; 3]; 3],
            lnk_stat: [LinkStatus::default(); 6],
            curr_aunit: AngleUnit::Degrees,
        }
    }
}

// ===========================================================================
// Helper: array access for the 6-element user/motor fields
// ===========================================================================

#[allow(dead_code)]
impl TableRecord {
    /// Get the 6-element user drive array [ax, ay, az, x, y, z].
    fn user_drive(&self) -> [f64; 6] {
        [self.ax, self.ay, self.az, self.x, self.y, self.z]
    }

    fn set_user_drive(&mut self, u: &[f64; 6]) {
        self.ax = u[0];
        self.ay = u[1];
        self.az = u[2];
        self.x = u[3];
        self.y = u[4];
        self.z = u[5];
    }

    fn user_offset(&self) -> [f64; 6] {
        [self.ax0, self.ay0, self.az0, self.x0, self.y0, self.z0]
    }

    fn set_user_offset(&mut self, o: &[f64; 6]) {
        self.ax0 = o[0];
        self.ay0 = o[1];
        self.az0 = o[2];
        self.x0 = o[3];
        self.y0 = o[4];
        self.z0 = o[5];
    }

    fn user_last(&self) -> [f64; 6] {
        [self.axl, self.ayl, self.azl, self.xl, self.yl, self.zl]
    }

    fn set_user_last(&mut self, l: &[f64; 6]) {
        self.axl = l[0];
        self.ayl = l[1];
        self.azl = l[2];
        self.xl = l[3];
        self.yl = l[4];
        self.zl = l[5];
    }

    fn user_readback(&self) -> [f64; 6] {
        [
            self.axrb, self.ayrb, self.azrb, self.xrb, self.yrb, self.zrb,
        ]
    }

    fn set_user_readback(&mut self, rb: &[f64; 6]) {
        self.axrb = rb[0];
        self.ayrb = rb[1];
        self.azrb = rb[2];
        self.xrb = rb[3];
        self.yrb = rb[4];
        self.zrb = rb[5];
    }

    fn encoder_user(&self) -> [f64; 6] {
        [self.eax, self.eay, self.eaz, self.ex, self.ey, self.ez]
    }

    fn set_encoder_user(&mut self, eu: &[f64; 6]) {
        self.eax = eu[0];
        self.eay = eu[1];
        self.eaz = eu[2];
        self.ex = eu[3];
        self.ey = eu[4];
        self.ez = eu[5];
    }

    fn motor_drive(&self) -> [f64; 6] {
        [self.m0x, self.m0y, self.m1y, self.m2x, self.m2y, self.m2z]
    }

    fn set_motor_drive(&mut self, m: &[f64; 6]) {
        self.m0x = m[0];
        self.m0y = m[1];
        self.m1y = m[2];
        self.m2x = m[3];
        self.m2y = m[4];
        self.m2z = m[5];
    }

    fn motor_readback(&self) -> [f64; 6] {
        [self.r0x, self.r0y, self.r1y, self.r2x, self.r2y, self.r2z]
    }

    fn set_motor_readback(&mut self, r: &[f64; 6]) {
        self.r0x = r[0];
        self.r0y = r[1];
        self.r1y = r[2];
        self.r2x = r[3];
        self.r2y = r[4];
        self.r2z = r[5];
    }

    fn encoder_motor(&self) -> [f64; 6] {
        [self.e0x, self.e0y, self.e1y, self.e2x, self.e2y, self.e2z]
    }

    fn set_encoder_motor(&mut self, e: &[f64; 6]) {
        self.e0x = e[0];
        self.e0y = e[1];
        self.e1y = e[2];
        self.e2x = e[3];
        self.e2y = e[4];
        self.e2z = e[5];
    }

    fn speed_val(&self) -> [f64; 6] {
        [self.v0x, self.v0y, self.v1y, self.v2x, self.v2y, self.v2z]
    }

    fn set_speed_val(&mut self, v: &[f64; 6]) {
        self.v0x = v[0];
        self.v0y = v[1];
        self.v1y = v[2];
        self.v2x = v[3];
        self.v2y = v[4];
        self.v2z = v[5];
    }

    fn hi_motor_limit(&self) -> [f64; 6] {
        [self.h0x, self.h0y, self.h1y, self.h2x, self.h2y, self.h2z]
    }

    fn set_hi_motor_limit(&mut self, h: &[f64; 6]) {
        self.h0x = h[0];
        self.h0y = h[1];
        self.h1y = h[2];
        self.h2x = h[3];
        self.h2y = h[4];
        self.h2z = h[5];
    }

    fn lo_motor_limit(&self) -> [f64; 6] {
        [self.l0x, self.l0y, self.l1y, self.l2x, self.l2y, self.l2z]
    }

    fn set_lo_motor_limit(&mut self, l: &[f64; 6]) {
        self.l0x = l[0];
        self.l0y = l[1];
        self.l1y = l[2];
        self.l2x = l[3];
        self.l2y = l[4];
        self.l2z = l[5];
    }

    fn user_hi_abs(&self) -> [f64; 6] {
        [
            self.uhax, self.uhay, self.uhaz, self.uhx, self.uhy, self.uhz,
        ]
    }

    fn set_user_hi_abs(&mut self, u: &[f64; 6]) {
        self.uhax = u[0];
        self.uhay = u[1];
        self.uhaz = u[2];
        self.uhx = u[3];
        self.uhy = u[4];
        self.uhz = u[5];
    }

    fn user_lo_abs(&self) -> [f64; 6] {
        [
            self.ulax, self.ulay, self.ulaz, self.ulx, self.uly, self.ulz,
        ]
    }

    fn set_user_lo_abs(&mut self, u: &[f64; 6]) {
        self.ulax = u[0];
        self.ulay = u[1];
        self.ulaz = u[2];
        self.ulx = u[3];
        self.uly = u[4];
        self.ulz = u[5];
    }

    fn user_hi_rel(&self) -> [f64; 6] {
        [
            self.uhaxr, self.uhayr, self.uhazr, self.uhxr, self.uhyr, self.uhzr,
        ]
    }

    fn set_user_hi_rel(&mut self, u: &[f64; 6]) {
        self.uhaxr = u[0];
        self.uhayr = u[1];
        self.uhazr = u[2];
        self.uhxr = u[3];
        self.uhyr = u[4];
        self.uhzr = u[5];
    }

    fn user_lo_rel(&self) -> [f64; 6] {
        [
            self.ulaxr, self.ulayr, self.ulazr, self.ulxr, self.ulyr, self.ulzr,
        ]
    }

    fn set_user_lo_rel(&mut self, u: &[f64; 6]) {
        self.ulaxr = u[0];
        self.ulayr = u[1];
        self.ulazr = u[2];
        self.ulxr = u[3];
        self.ulyr = u[4];
        self.ulzr = u[5];
    }

    fn calc_hi_limit(&self) -> [f64; 6] {
        [
            self.hlax, self.hlay, self.hlaz, self.hlx, self.hly, self.hlz,
        ]
    }

    fn set_calc_hi_limit(&mut self, h: &[f64; 6]) {
        self.hlax = h[0];
        self.hlay = h[1];
        self.hlaz = h[2];
        self.hlx = h[3];
        self.hly = h[4];
        self.hlz = h[5];
    }

    fn calc_lo_limit(&self) -> [f64; 6] {
        [
            self.llax, self.llay, self.llaz, self.llx, self.lly, self.llz,
        ]
    }

    fn set_calc_lo_limit(&mut self, l: &[f64; 6]) {
        self.llax = l[0];
        self.llay = l[1];
        self.llaz = l[2];
        self.llx = l[3];
        self.lly = l[4];
        self.llz = l[5];
    }
}

// ===========================================================================
// Core geometry/math — pure functions
// ===========================================================================

/// Initialize pivot-point vectors and inverse matrix based on geometry.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn init_geometry(
    geom: Geometry,
    lx: f64,
    lz: f64,
    sx: f64,
    sy: f64,
    sz: f64,
    rx: f64,
    ry: f64,
    rz: f64,
) -> (
    [f64; 3],
    [f64; 3],
    [f64; 3],
    [f64; 3],
    [f64; 3],
    [f64; 3],
    [[f64; 3]; 3],
) {
    let fx = rx + sx;
    let fy = ry + sy;
    let fz = rz + sz;

    let (pp0, pp1, pp2) = match geom {
        Geometry::Geocars => (
            [-fx, -fy, lz / 2.0 - fz],
            [lx - fx, -fy, lz - fz],
            [lx - fx, -fy, -fz],
        ),
        Geometry::Newport => (
            [lx - fx, -fy, -fz],
            [-fx, -fy, lz / 2.0 - fz],
            [lx - fx, -fy, lz - fz],
        ),
        Geometry::Pnc => (
            [-fx, -fy, -fz],
            [lx - fx, -fy, -fz],
            [lx / 2.0 - fx, -fy, lz - fz],
        ),
        Geometry::Sri => (
            [lx - fx, -fy, -fz],
            [-fx, -fy, -fz],
            [lx / 2.0 - fx, -fy, lz - fz],
        ),
    };

    let ppo0 = pp0;
    let ppo1 = pp1;
    let ppo2 = pp2;

    // Build matrix from vectors in the space spanned by pivot-point vectors
    let av = ppo1[X] - ppo0[X];
    let bv = ppo1[Y] - ppo0[Y];
    let cv = ppo1[Z] - ppo0[Z];
    let dv = ppo2[X] - ppo1[X];
    let ev = ppo2[Y] - ppo1[Y];
    let fv = ppo2[Z] - ppo1[Z];
    let gv = bv * fv - cv * ev;
    let hv = cv * dv - av * fv;
    let iv = av * ev - bv * dv;

    // Inverse matrix
    let det = av * (ev * iv - hv * fv) + bv * (fv * gv - iv * dv) + cv * (dv * hv - gv * ev);

    let bb = [
        [
            (ev * iv - fv * hv) / det,
            (cv * hv - bv * iv) / det,
            (bv * fv - cv * ev) / det,
        ],
        [
            (fv * gv - dv * iv) / det,
            (av * iv - cv * gv) / det,
            (cv * dv - av * fv) / det,
        ],
        [
            (dv * hv - ev * gv) / det,
            (bv * gv - av * hv) / det,
            (av * ev - bv * dv) / det,
        ],
    ];

    (pp0, pp1, pp2, ppo0, ppo1, ppo2, bb)
}

/// Build the 3x3 rotation matrix from user angles (in user units).
fn make_rotation_matrix(torad: f64, u: &[f64; 6]) -> [[f64; 3]; 3] {
    let cx = (torad * u[AX_6]).cos();
    let sx = (torad * u[AX_6]).sin();
    let cy = (torad * u[AY_6]).cos();
    let sy = (torad * u[AY_6]).sin();
    let cz = (torad * u[AZ_6]).cos();
    let sz = (torad * u[AZ_6]).sin();

    [
        [cy * cz, cy * sz, -sy],
        [sx * sy * cz - cx * sz, sx * sy * sz + cx * cz, sx * cy],
        [cx * sy * cz + sx * sz, cx * sy * sz - sx * cz, cx * cy],
    ]
}

/// Y-axis rotation of a 6-element user vector (rotates X/Z and AX/AZ).
fn rot_y(inp: &[f64; 6], angle: f64) -> [f64; 6] {
    let ca = angle.cos();
    let sa = angle.sin();
    let mut out = *inp;
    out[X_6] = inp[X_6] * ca + inp[Z_6] * sa;
    out[AX_6] = inp[AX_6] * ca + inp[AZ_6] * sa;
    out[Z_6] = inp[X_6] * (-sa) + inp[Z_6] * ca;
    out[AZ_6] = inp[AX_6] * (-sa) + inp[AZ_6] * ca;
    out[Y_6] = inp[Y_6];
    out[AY_6] = inp[AY_6];
    out
}

/// Convert from lab coordinates to local table coordinates.
fn lab_to_local(yang: f64, lab: &[f64; 6]) -> [f64; 6] {
    rot_y(lab, yang * D2R)
}

/// Convert from local table coordinates to lab coordinates.
fn local_to_lab(yang: f64, local: &[f64; 6]) -> [f64; 6] {
    rot_y(local, -yang * D2R)
}

/// Naive motor-to-pivot-point-vector mapping.  For non-Newport geometries this
/// gives the directly measurable components; the rest must be solved for.
/// For Newport, this requires a pre-computed rotation matrix.
fn naive_motor_to_pivot_point_vector(
    geom: Geometry,
    ppo0: &[f64; 3],
    ppo1: &[f64; 3],
    ppo2: &[f64; 3],
    a: &[[f64; 3]; 3],
    m: &[f64; 6],
) -> ([f64; 3], [f64; 3], [f64; 3]) {
    match geom {
        Geometry::Sri | Geometry::Pnc | Geometry::Geocars => {
            let q0 = [
                ppo0[X] + m[M0X],
                ppo0[Y] + m[M0Y],
                0.0, /* to be solved */
            ];
            let q1 = [
                0.0, /* to be solved */
                ppo1[Y] + m[M1Y],
                0.0, /* to be solved */
            ];
            let q2 = [ppo2[X] + m[M2X], ppo2[Y] + m[M2Y], ppo2[Z] + m[M2Z]];
            (q0, q1, q2)
        }
        Geometry::Newport => {
            let norm = [a[X][Y], a[Y][Y], a[Z][Y]];
            let q0 = [
                ppo0[X] + m[M0X] + norm[X] * m[M0Y],
                ppo0[Y] + norm[Y] * m[M0Y],
                0.0, // to be calculated
            ];
            let q1 = [
                0.0, // to be calculated
                ppo1[Y] + norm[Y] * m[M1Y],
                0.0, // to be calculated
            ];
            let q2 = [
                ppo2[X] + m[M2X] + norm[X] * m[M2Y],
                ppo2[Y] + norm[Y] * m[M2Y],
                ppo2[Z] + m[M2Z] + norm[Z] * m[M2Y],
            ];
            (q0, q1, q2)
        }
    }
}

/// Full motor-to-pivot-point-vector with constraint solving.
/// Used for SRI, GEOCARS, PNC (not Newport).
fn motor_to_pivot_point_vector(
    geom: Geometry,
    ppo0: &[f64; 3],
    ppo1: &[f64; 3],
    ppo2: &[f64; 3],
    a: &[[f64; 3]; 3],
    m: &[f64; 6],
) -> ([f64; 3], [f64; 3], [f64; 3]) {
    let (mut q0, mut q1, q2) = naive_motor_to_pivot_point_vector(geom, ppo0, ppo1, ppo2, a, m);

    // Solve q0[Z] from |q0-q2| == |p0-p2|
    let d0x = ppo2[X] - ppo0[X];
    let d0y = ppo2[Y] - ppo0[Y];
    let d0z = ppo2[Z] - ppo0[Z];
    let dx = q2[X] - q0[X];
    let dy = q2[Y] - q0[Y];

    let dist_sq = d0x * d0x + d0y * d0y + d0z * d0z - (dx * dx + dy * dy);
    let dist = dist_sq.max(0.0).sqrt();

    q0[Z] = match geom {
        Geometry::Geocars => q2[Z] + dist, // root where q2[Z] < q0[Z]
        _ => q2[Z] - dist,                 // root where q2[Z] > q0[Z]
    };

    // Solve q1[X] and q1[Z] from:
    //   (q1-q0).(q2-q0) == (p1-p0).(p2-p0)
    //   |q1-q0| == |p1-p0|
    let mut p10p20: f64 = 0.0;
    for i in X..=Z {
        p10p20 += (ppo1[i] - ppo0[i]) * (ppo2[i] - ppo0[i]);
    }

    let s = -(q0[Z] - q2[Z]) / (q0[X] - q2[X]);
    let t = (-p10p20
        + q0[X] * (q0[X] - q2[X])
        + (q0[Y] - q1[Y]) * (q0[Y] - q2[Y])
        + q0[Z] * (q0[Z] - q2[Z]))
        / (q0[X] - q2[X]);

    let mut p10p10: f64 = 0.0;
    for i in X..=Z {
        p10p10 += (ppo1[i] - ppo0[i]) * (ppo1[i] - ppo0[i]);
    }

    let discriminant = (2.0 * s * t - 2.0 * s * q0[X] - 2.0 * q0[Z]).powi(2)
        - 4.0
            * (1.0 + s * s)
            * (t * t - p10p10 - 2.0 * t * q0[X] + q0[X] * q0[X] + q0[Y] * q0[Y] + q0[Z] * q0[Z]
                - 2.0 * q0[Y] * q1[Y]
                + q1[Y] * q1[Y]);
    let alpha = discriminant.max(0.0).sqrt();

    let denom = 2.0 * (1.0 + s * s);
    let q1z_p = (-2.0 * s * t + 2.0 * s * q0[X] + 2.0 * q0[Z] + alpha) / denom;
    let q1z_m = (-2.0 * s * t + 2.0 * s * q0[X] + 2.0 * q0[Z] - alpha) / denom;

    // Take root representing smaller motion
    q1[Z] = if (q1z_p - ppo1[Z]).abs() > (q1z_m - ppo1[Z]).abs() {
        q1z_m
    } else {
        q1z_p
    };
    q1[X] = s * q1[Z] + t;

    (q0, q1, q2)
}

/// Extract local user angles from pivot-point vectors.
/// Used for SRI, GEOCARS, PNC geometries.
fn pivot_point_vector_to_local_user_angles(
    bb: &[[f64; 3]; 3],
    torad: f64,
    q0: &[f64; 3],
    q1: &[f64; 3],
    q2: &[f64; 3],
) -> [f64; 6] {
    let av = q1[X] - q0[X];
    let bv = q1[Y] - q0[Y];
    let cv = q1[Z] - q0[Z];
    let dv = q2[X] - q1[X];
    let ev = q2[Y] - q1[Y];
    let fv = q2[Z] - q1[Z];
    let gv = bv * fv - cv * ev;
    let hv = cv * dv - av * fv;
    let _iv = av * ev - bv * dv;

    // Rotated y axis (jp)
    let jp_x = bb[1][0] * av + bb[1][1] * dv + bb[1][2] * gv;
    // Rotated z axis (kp)
    let kp_x = bb[2][0] * av + bb[2][1] * dv + bb[2][2] * gv;
    let kp_y = bb[2][0] * bv + bb[2][1] * ev + bb[2][2] * hv;

    let mut u = [0.0f64; 6];
    u[AY_6] = (-kp_x).asin();
    let cos_ay = u[AY_6].cos();
    if cos_ay.abs() > SMALL {
        u[AX_6] = (kp_y / cos_ay).clamp(-1.0, 1.0).asin() / torad;
        u[AZ_6] = (jp_x / cos_ay).clamp(-1.0, 1.0).asin() / torad;
    }
    u[AY_6] /= torad;
    u
}

/// Newport-specific: extract angles from motor positions using
/// Mathematica-derived formulas.
fn motor_to_local_user_angles(
    ppo0: &[f64; 3],
    ppo1: &[f64; 3],
    ppo2: &[f64; 3],
    torad: f64,
    m: &[f64; 6],
) -> [f64; 6] {
    let p0x = ppo0[X];
    let p0y = ppo0[Y];
    let p0z = ppo0[Z];
    let p1x = ppo1[X];
    let p1y = ppo1[Y];
    let p1z = ppo1[Z];
    let p2x = ppo2[X];
    let p2y = ppo2[Y];
    let p2z = ppo2[Z];
    let p10x = p1x - p0x;
    let p10y = p1y - p0y;
    let p10z = p1z - p0z;
    let p20x = p2x - p0x;
    let p20y = p2y - p0y;
    let p20z = p2z - p0z;
    let p02x = p0x - p2x;
    let p02y = p0y - p2y;
    let p02z = p0z - p2z;
    let p02x_2 = p02x * p02x;
    let p02y_2 = p02y * p02y;
    let p02z_2 = p02z * p02z;

    let l0 = m[M0Y];
    let _l1 = m[M1Y];
    let _l2 = m[M2Y];
    let l10 = m[M1Y] - m[M0Y];
    let l20 = m[M2Y] - m[M0Y];
    let l02 = m[M0Y] - m[M2Y];
    let l02_2 = l02 * l02;

    let n0x = m[M0X];
    let n2x = m[M2X];
    let n02x = n0x - n2x;
    let n02x_2 = n02x * n02x;

    // Normal vector to table
    let npx = p10y * p20z - p10z * p20y - p20z * l10 + p10z * l20;
    let npy = p10z * p20x - p10x * p20z;
    let npz = p10x * p20y - p10y * p20x + p20x * l10 - p10x * l20;

    let ryy = npy / (npx * npx + npy * npy + npz * npz).sqrt();
    let ryy_2 = ryy * ryy;

    // Determinant for Mathematica solutions
    let det_base = p0z * (p1x - p2x) + p1z * p2x - p1x * p2z + p0x * (-p1z + p2z);

    let ryx = (p1z * p2y - p1y * p2z + p0y * (p1z - p2z) * (-1.0 + ryy)
        - (p1z * (l0 - _l2 + p2y) - (l0 - _l1 + p1y) * p2z) * ryy
        + p0z * (p1y - p2y + (_l1 - _l2 - p1y + p2y) * ryy))
        / det_base;
    let ryx_2 = ryx * ryx;

    let ryz = (p1y * p2x - p1x * p2y - p0y * (p1x - p2x) * (-1.0 + ryy)
        + (l0 * p1x - _l2 * p1x - l0 * p2x + _l1 * p2x - p1y * p2x + p1x * p2y) * ryy
        + p0x * (-p1y + p2y + (-_l1 + _l2 + p1y - p2y) * ryy))
        / det_base;
    let ryz_2 = ryz * ryz;

    let mut u = [0.0f64; 6];
    u[Y_6] = (-(p0x * p1z * p2y) + p0x * p1y * p2z
        - p0y * (p1z * p2x - p1x * p2z) * (-1.0 + ryy)
        - _l2 * p0x * p1z * ryy
        + l0 * p1z * p2x * ryy
        + p0x * p1z * p2y * ryy
        + _l1 * p0x * p2z * ryy
        - l0 * p1x * p2z * ryy
        - p0x * p1y * p2z * ryy
        + p0z
            * (p1y * p2x * (-1.0 + ryy) - (_l1 * p2x + p1x * p2y) * ryy + p1x * (p2y + _l2 * ryy)))
        / det_base;

    // Solve for Rxx, Rxy, Rxz using rotation-matrix identities
    let a_coef = (n02x + p02x)
        * (l02 * ryx * ryy - p02y * ryx * ryy - p02z * ryx * ryz + p02x * (ryy_2 + ryz_2));

    let b_coef = -p02x_2 * ryx_2 + p02y_2 * ryx_2 + p02z_2 * ryx_2 - 2.0 * p02x * p02y * ryx * ryy
        + p02z_2 * ryy_2
        - 2.0 * p02z * (p02x * ryx + p02y * ryy) * ryz
        + p02y_2 * ryz_2
        + l02_2 * (ryx_2 + ryz_2)
        - n02x_2 * (ryx_2 + ryy_2 + ryz_2)
        - 2.0 * n02x * p02x * (ryx_2 + ryy_2 + ryz_2)
        - 2.0 * l02 * (-ryy * (p02x * ryx + p02z * ryz) + p02y * (ryx_2 + ryz_2));

    let c_coef = l02_2 * ryx_2 - 2.0 * l02 * p02y * ryx_2
        + p02y_2 * ryx_2
        + p02z_2 * ryx_2
        + 2.0 * l02 * p02x * ryx * ryy
        - 2.0 * p02x * p02y * ryx * ryy
        + p02x_2 * ryy_2
        + p02z_2 * ryy_2
        - 2.0 * p02z * (p02x * ryx + (-l02 + p02y) * ryy) * ryz
        + (p02x_2 + (l02 - p02y) * (l02 - p02y)) * ryz_2;

    let sqrt_b = b_coef.max(0.0).sqrt();

    let rxx_1 = (a_coef - (p02z * ryy + (l02 - p02y) * ryz) * sqrt_b) / c_coef;
    let rxx_2 = (a_coef + (p02z * ryy + (l02 - p02y) * ryz) * sqrt_b) / c_coef;
    let rxx = if (rxx_2 - 1.0).abs() < (rxx_1 - 1.0).abs() {
        rxx_2
    } else {
        rxx_1
    };

    let a2 = (n02x + p02x) * (p02z * (ryx_2 + ryy_2) - (p02x * ryx + (-l02 + p02y) * ryy) * ryz);
    let rxz = (a2 + (l02 * ryx - p02y * ryx + p02x * ryy) * sqrt_b) / c_coef;

    let a3 = -(n02x + p02x)
        * (ryx * (l02 * ryx - p02y * ryx + p02x * ryy) + p02z * ryy * ryz + (l02 - p02y) * ryz_2);
    let rxy_1 = (a3 + (p02z * ryx - p02x * ryz) * sqrt_b) / c_coef;
    let rxy_2 = (a3 - (p02z * ryx - p02x * ryz) * sqrt_b) / c_coef;
    let tmp1 = (1.0 - (rxx * rxx + rxz * rxz)).max(0.0).sqrt();
    let rxy = if (rxy_1.abs() - tmp1).abs() - (rxy_2.abs() - tmp1).abs() > 1.0e-6
        && (rxy_2.abs() - tmp1).abs() < (rxy_1.abs() - tmp1).abs()
    {
        rxy_2
    } else {
        rxy_1
    };

    u[AY_6] = (-rxz).clamp(-1.0, 1.0).asin();
    let cos_ay = u[AY_6].cos();
    if cos_ay.abs() > SMALL {
        u[AX_6] = (ryz / cos_ay).clamp(-1.0, 1.0).asin() / torad;
        u[AZ_6] = (rxy / cos_ay).clamp(-1.0, 1.0).asin() / torad;
    }
    u[AY_6] /= torad;
    u
}

/// Full motor-to-user conversion.  Returns user coordinates in lab frame,
/// with offsets subtracted.
#[allow(clippy::too_many_arguments)]
fn motor_to_user(
    geom: Geometry,
    torad: f64,
    yang: f64,
    pp0: &mut [f64; 3],
    pp1: &mut [f64; 3],
    pp2: &mut [f64; 3],
    ppo0: &[f64; 3],
    ppo1: &[f64; 3],
    ppo2: &[f64; 3],
    a: &mut [[f64; 3]; 3],
    b: &[[f64; 3]; 3],
    lnk_stat: &[LinkStatus; 6],
    ax0: &[f64; 6],
    m: &[f64; 6],
) -> [f64; 6] {
    let mut u = match geom {
        Geometry::Sri | Geometry::Geocars | Geometry::Pnc => {
            let (q0, q1, q2) = motor_to_pivot_point_vector(geom, ppo0, ppo1, ppo2, a, m);
            pivot_point_vector_to_local_user_angles(b, torad, &q0, &q1, &q2)
        }
        Geometry::Newport => motor_to_local_user_angles(ppo0, ppo1, ppo2, torad, m),
    };

    // Recover rotation matrix to get translations
    *a = make_rotation_matrix(torad, &u);

    // Rotate pivot points
    let mut rpp0 = [0.0; 3];
    let mut rpp1 = [0.0; 3];
    let mut rpp2 = [0.0; 3];
    for j in X..=Z {
        for k in X..=Z {
            rpp0[j] += ppo0[k] * a[j][k];
            rpp1[j] += ppo1[k] * a[j][k];
            rpp2[j] += ppo2[k] * a[j][k];
        }
    }

    if geom == Geometry::Newport {
        rpp0[Y] += u[Y_6];
        rpp1[Y] += u[Y_6];
        rpp2[Y] += u[Y_6];
    }

    // Compare rotated pivot points with motors to get translations
    let (m_try, u_from_pp) =
        pivot_point_vector_to_motor(geom, ppo0, ppo1, ppo2, a, lnk_stat, &rpp0, &rpp1, &rpp2, &u);

    // Check special case of 5-motor Newport table
    if geom == Geometry::Newport && !lnk_stat[M2X].can_rw_drive {
        u[X_6] = u_from_pp[X_6]; // calculated by PivotPointVectorToMotor
    } else {
        u[X_6] = m[M2X] - m_try[M2X];
    }

    if geom != Geometry::Newport {
        u[Y_6] = m[M2Y] - m_try[M2Y];
    }

    if !lnk_stat[M2Z].can_rw_drive {
        u[Z_6] = u_from_pp[Z_6]; // calculated by PivotPointVectorToMotor
    } else {
        u[Z_6] = m[M2Z] - m_try[M2Z];
    }

    // Convert to lab frame
    u = local_to_lab(yang, &u);

    // Subtract user offsets
    for i in 0..6 {
        u[i] -= ax0[i];
    }

    // Update stored pivot points
    *pp0 = rpp0;
    *pp1 = rpp1;
    *pp2 = rpp2;

    u
}

/// Go from local user coordinates to rotated, translated pivot-point vectors.
fn local_user_to_pivot_point_vector(
    _torad: f64,
    ppo0: &[f64; 3],
    ppo1: &[f64; 3],
    ppo2: &[f64; 3],
    u: &[f64; 6],
    a: &[[f64; 3]; 3],
) -> ([f64; 3], [f64; 3], [f64; 3]) {
    let mut pp0 = [0.0; 3];
    let mut pp1 = [0.0; 3];
    let mut pp2 = [0.0; 3];

    for i in X..=Z {
        let k = i + X_6; // translation index
        for j in X..=Z {
            pp0[i] += ppo0[j] * a[i][j];
            pp1[i] += ppo1[j] * a[i][j];
            pp2[i] += ppo2[j] * a[i][j];
        }
        pp0[i] += u[k];
        pp1[i] += u[k];
        pp2[i] += u[k];
    }

    (pp0, pp1, pp2)
}

/// Calculate motor positions from rotated, translated pivot-point vectors
/// (in local table coordinates).  Enforces constraints from missing motors.
/// Returns (motor_values, user_values_possibly_modified).
#[allow(clippy::too_many_arguments)]
fn pivot_point_vector_to_motor(
    geom: Geometry,
    ppo0: &[f64; 3],
    ppo1: &[f64; 3],
    ppo2: &[f64; 3],
    a: &[[f64; 3]; 3],
    lnk_stat: &[LinkStatus; 6],
    pp0: &[f64; 3],
    pp1: &[f64; 3],
    pp2: &[f64; 3],
    u: &[f64; 6],
) -> ([f64; 6], [f64; 6]) {
    let mut m = [0.0f64; 6];
    let mut u_out = *u;

    match geom {
        Geometry::Sri | Geometry::Geocars | Geometry::Pnc => {
            m[M0X] = pp0[X] - ppo0[X];
            m[M0Y] = pp0[Y] - ppo0[Y];
            m[M1Y] = pp1[Y] - ppo1[Y];
            m[M2X] = pp2[X] - ppo2[X];
            m[M2Y] = pp2[Y] - ppo2[Y];
            if lnk_stat[M2Z].can_rw_drive {
                m[M2Z] = pp2[Z] - ppo2[Z];
            } else {
                u_out[Z_6] = -(a[Z][X] * ppo2[X] + a[Z][Y] * ppo2[Y] + (a[Z][Z] - 1.0) * ppo2[Z]);
                m[M2Z] = 0.0;
            }
        }
        Geometry::Newport => {
            let norm = [a[X][Y], a[Y][Y], a[Z][Y]];

            m[M0Y] = (pp0[Y] - ppo0[Y]) / norm[Y];
            m[M1Y] = (pp1[Y] - ppo1[Y]) / norm[Y];
            m[M2Y] = (pp2[Y] - ppo2[Y]) / norm[Y];

            m[M2Z] = (pp2[Z] - ppo2[Z]) - norm[Z] * m[M2Y];

            if lnk_stat[M2X].can_rw_drive {
                // 6-motor table
                m[M0X] = (pp0[X] - ppo0[X]) - norm[X] * m[M0Y];
                m[M2X] = (pp2[X] - ppo2[X]) - norm[X] * m[M2Y];
            } else {
                // 5-motor table: x is constrained by missing motor
                u_out[X_6] = -((a[X][X] - 1.0) * ppo2[X] + a[X][Y] * ppo2[Y] + a[X][Z] * ppo2[Z]
                    - norm[X] * m[M2Y]);
                m[M0X] = (a[X][X] - 1.0) * ppo0[X] + a[X][Y] * ppo0[Y] + a[X][Z] * ppo0[Z]
                    - norm[X] * m[M0Y]
                    + u_out[X_6];
                m[M2X] = 0.0;
            }
        }
    }

    (m, u_out)
}

/// Full user-to-motor conversion.
#[allow(clippy::too_many_arguments)]
fn user_to_motor(
    geom: Geometry,
    torad: f64,
    yang: f64,
    pp0: &mut [f64; 3],
    pp1: &mut [f64; 3],
    pp2: &mut [f64; 3],
    ppo0: &[f64; 3],
    ppo1: &[f64; 3],
    ppo2: &[f64; 3],
    a: &mut [[f64; 3]; 3],
    lnk_stat: &[LinkStatus; 6],
    ax0: &[f64; 6],
    user: &[f64; 6],
) -> [f64; 6] {
    // Get user coordinates into local coordinate system
    let mut u = [0.0f64; 6];
    for i in 0..6 {
        u[i] = user[i] + ax0[i];
    }
    u = lab_to_local(yang, &u);

    *a = make_rotation_matrix(torad, &u);
    let (lpp0, lpp1, lpp2) = local_user_to_pivot_point_vector(torad, ppo0, ppo1, ppo2, &u, a);
    *pp0 = lpp0;
    *pp1 = lpp1;
    *pp2 = lpp2;

    let (m, _u_out) =
        pivot_point_vector_to_motor(geom, ppo0, ppo1, ppo2, a, lnk_stat, pp0, pp1, pp2, &u);
    m
}

/// Zero the table: make current rotation/translation appear as zero.
fn zero_table(
    ax: &mut [f64; 6],
    ax0: &mut [f64; 6],
    axl: &[f64; 6],
    uhax: &[f64; 6],
    ulax: &[f64; 6],
    uhaxr: &mut [f64; 6],
    ulaxr: &mut [f64; 6],
) {
    for i in 0..6 {
        ax[i] = 0.0;
        ax0[i] = axl[i];
        uhaxr[i] = uhax[i] - ax0[i];
        ulaxr[i] = ulax[i] - ax0[i];
    }
}

/// Neville polynomial interpolation (Numerical Recipes).
/// 0-based indexing Rust port.
fn polint(xa: &[f64], ya: &[f64], x: f64) -> Option<(f64, f64)> {
    let n = xa.len().min(NTRAJ);
    if n == 0 {
        return None;
    }
    let mut c = vec![0.0f64; n];
    let mut d = vec![0.0f64; n];

    let mut ns = 0usize;
    let mut dif = (x - xa[0]).abs();

    for i in 0..n {
        let dift = (x - xa[i]).abs();
        if dift < dif {
            ns = i;
            dif = dift;
        }
        c[i] = ya[i];
        d[i] = ya[i];
    }

    let mut y = ya[ns];
    ns = ns.saturating_sub(1);
    let mut dy = 0.0f64;

    for m in 1..n {
        for i in 0..n - m {
            let ho = xa[i] - x;
            let hp = xa[i + m] - x;
            let w = c[i + 1] - d[i];
            let den = ho - hp;
            if den == 0.0 {
                return None;
            }
            let den = w / den;
            d[i] = hp * den;
            c[i] = ho * den;
        }
        // Adjust ns for 0-based indexing: in the C code ns starts at 1-based
        // and the test is `2*ns < (n-m)`.  Here ns is 0-based so the equivalent
        // test becomes `2*(ns+1) < (n-m)`, i.e. `2*ns + 2 < n - m`.
        dy = if 2 * (ns + 1) < n - m {
            c[ns + 1]
        } else {
            let v = d[ns];
            ns = ns.saturating_sub(1);
            v
        };
        y += dy;
    }

    Some((y, dy))
}

/// Sort trajectory array.  First element is already in place; determines
/// ascending/descending order.
fn sort_trajectory(traj: &mut [Trajectory]) {
    if traj.len() < 3 {
        return;
    }
    let ascending = traj[1].user > traj[0].user;
    // Insertion sort starting from index 2 (index 0 is anchor, index 1 in place)
    for j in 2..traj.len() {
        let key = traj[j].clone();
        let mut i = j as isize - 1;
        while i >= 0 && (traj[i as usize].user > key.user) == ascending {
            traj[(i + 1) as usize] = traj[i as usize].clone();
            i -= 1;
        }
        traj[(i + 1) as usize] = key;
    }
}

/// Find the user-coordinate value at which a motor limit is first crossed.
fn find_limit(
    hm: &[f64; 6],
    lm: &[f64; 6],
    lnk_stat: &[LinkStatus; 6],
    traj: &mut [Trajectory],
    n: usize,
) -> Option<f64> {
    sort_trajectory(&mut traj[..n]);

    // Make sure a limit violation occurred somewhere
    if !traj[..n].iter().any(|t| t.lvio) {
        return None;
    }

    let user: Vec<f64> = traj[..n].iter().map(|t| t.user).collect();
    let mut user_limit = user[n - 1];
    let mut found = false;

    for i in M0X..=M2Z {
        if !lnk_stat[i].can_read_limits {
            continue;
        }
        let motor: Vec<f64> = traj[..n].iter().map(|t| t.motor[i]).collect();

        // Check if high motor limit was crossed
        if (hm[i] > motor[0]) != (hm[i] > motor[n - 1])
            && let Some((limit, _err)) = polint(&motor, &user, hm[i])
        {
            found = true;
            if (limit - user[0]).abs() < (user_limit - user[0]).abs() {
                user_limit = limit;
            }
        }

        // Check if low motor limit was crossed
        if (lm[i] > motor[0]) != (lm[i] > motor[n - 1])
            && let Some((limit, _err)) = polint(&motor, &user, lm[i])
        {
            found = true;
            if (limit - user[0]).abs() < (user_limit - user[0]).abs() {
                user_limit = limit;
            }
        }
    }

    if found { Some(user_limit) } else { None }
}

/// Check if any motor drive value violates its limits.
fn motor_limit_viol(
    m: &[f64; 6],
    hm: &[f64; 6],
    lm: &[f64; 6],
    lnk_stat: &[LinkStatus; 6],
) -> bool {
    for i in 0..6 {
        if lnk_stat[i].can_read_limits
            && lnk_stat[i].can_rw_drive
            && (hm[i].abs() > SMALL || lm[i].abs() > SMALL)
            && (m[i] > hm[i] || m[i] < lm[i])
        {
            return true;
        }
    }
    false
}

/// Check if any user coordinate violates the calculated user limits.
fn user_limit_viol(ax: &[f64; 6], hlax: &[f64; 6], llax: &[f64; 6]) -> bool {
    for i in 0..6 {
        if (hlax[i].abs() > SMALL || llax[i].abs() > SMALL) && (ax[i] < llax[i] || ax[i] > hlax[i])
        {
            return true;
        }
    }
    false
}

/// Calculate local user limits via trajectory-based binary search.
#[allow(clippy::too_many_arguments)]
fn calc_local_user_limits(
    geom: Geometry,
    torad: f64,
    yang: f64,
    aunit: AngleUnit,
    pp0: &mut [f64; 3],
    pp1: &mut [f64; 3],
    pp2: &mut [f64; 3],
    ppo0: &[f64; 3],
    ppo1: &[f64; 3],
    ppo2: &[f64; 3],
    a: &mut [[f64; 3]; 3],
    _b: &[[f64; 3]; 3],
    lnk_stat: &[LinkStatus; 6],
    ax: &mut [f64; 6],
    ax0: &[f64; 6],
    m0x: &mut [f64; 6],
    hm: &[f64; 6],
    lm: &[f64; 6],
) -> ([f64; 6], [f64; 6]) {
    // Recompute motors from current user coords
    *m0x = user_to_motor(
        geom, torad, yang, pp0, pp1, pp2, ppo0, ppo1, ppo2, a, lnk_stat, ax0, ax,
    );

    // Get naive pivot points for motor hi and lo limits
    let (pp0h, pp1h, pp2h) = naive_motor_to_pivot_point_vector(geom, ppo0, ppo1, ppo2, a, hm);
    let (pp0l, pp1l, pp2l) = naive_motor_to_pivot_point_vector(geom, ppo0, ppo1, ppo2, a, lm);

    let mut hu = [LARGE; 6];
    let mut lu = [-LARGE; 6];

    let mut u = [0.0f64; 6];
    for i in 0..6 {
        u[i] = ax[i] + ax0[i];
    }
    u = lab_to_local(yang, &u);

    // Translation limits from motor limits
    if lnk_stat[M0X].can_read_limits {
        hu[X_6] = hu[X_6].min(u[X_6] + pp0h[X] - pp0[X]);
        lu[X_6] = lu[X_6].max(u[X_6] + pp0l[X] - pp0[X]);
    }
    if lnk_stat[M2X].can_read_limits {
        hu[X_6] = hu[X_6].min(u[X_6] + pp2h[X] - pp2[X]);
        lu[X_6] = lu[X_6].max(u[X_6] + pp2l[X] - pp2[X]);
    }
    if lnk_stat[M0Y].can_read_limits {
        hu[Y_6] = hu[Y_6].min(u[Y_6] + pp0h[Y] - pp0[Y]);
        lu[Y_6] = lu[Y_6].max(u[Y_6] + pp0l[Y] - pp0[Y]);
    }
    if lnk_stat[M1Y].can_read_limits {
        hu[Y_6] = hu[Y_6].min(u[Y_6] + pp1h[Y] - pp1[Y]);
        lu[Y_6] = lu[Y_6].max(u[Y_6] + pp1l[Y] - pp1[Y]);
    }
    if lnk_stat[M2Y].can_read_limits {
        hu[Y_6] = hu[Y_6].min(u[Y_6] + pp2h[Y] - pp2[Y]);
        lu[Y_6] = lu[Y_6].max(u[Y_6] + pp2l[Y] - pp2[Y]);
    }
    if lnk_stat[M2Z].can_read_limits {
        hu[Z_6] = hu[Z_6].min(u[Z_6] + pp2h[Z] - pp2[Z]);
        lu[Z_6] = lu[Z_6].max(u[Z_6] + pp2l[Z] - pp2[Z]);
    }

    // Add offsets and limit sentinel values
    for i in X_6..=Z_6 {
        hu[i] -= ax0[i];
        lu[i] -= ax0[i];
        if hu[i] >= LARGE {
            hu[i] = u[i] + SMALL;
        }
        if lu[i] <= -LARGE {
            lu[i] = u[i] - SMALL;
        }
    }

    // Rotation limits via trajectory-based binary search
    let delta_init = DELTA_START * D2R / torad;
    let angle_max = match aunit {
        AngleUnit::Degrees => 89.0,
        AngleUnit::Microradians => 1.55e6,
    };

    for i in AX_6..=AZ_6 {
        let save = ax[i];

        // Try to find a legal value for this coordinate
        if motor_limit_viol(m0x, hm, lm, lnk_stat) {
            ax[i] = 0.0;
            *m0x = user_to_motor(
                geom, torad, yang, pp0, pp1, pp2, ppo0, ppo1, ppo2, a, lnk_stat, ax0, ax,
            );
            if motor_limit_viol(m0x, hm, lm, lnk_stat) {
                hu[i] = save;
                lu[i] = save;
                ax[i] = save;
                *m0x = user_to_motor(
                    geom, torad, yang, pp0, pp1, pp2, ppo0, ppo1, ppo2, a, lnk_stat, ax0, ax,
                );
                continue;
            }
        }

        // Search for high limit, then low limit
        for (ii, sign) in [(0usize, 1.0f64), (1usize, -1.0f64)] {
            let mut delta = sign * delta_init;
            let mut traj = vec![Trajectory::default(); NTRAJ];
            let mut limit_crossings: u16 = 0;
            let mut j = 0;

            while j < NTRAJ && limit_crossings < 2 {
                traj[j].user = ax[i];
                traj[j].motor = *m0x;
                traj[j].lvio = motor_limit_viol(m0x, hm, lm, lnk_stat);

                if j > 0 {
                    if traj[j].lvio != traj[j - 1].lvio {
                        limit_crossings += 1;
                        delta = -delta;
                    }
                    if limit_crossings > 0 {
                        delta *= 0.5;
                    }
                }

                ax[i] += delta;
                // Clamp to max angle
                if ax[i].abs() > angle_max {
                    ax[i] = angle_max * ax[i].signum();
                }

                *m0x = user_to_motor(
                    geom, torad, yang, pp0, pp1, pp2, ppo0, ppo1, ppo2, a, lnk_stat, ax0, ax,
                );
                j += 1;
            }

            if limit_crossings > 0 {
                if let Some(limit) = find_limit(hm, lm, lnk_stat, &mut traj, j) {
                    if ii == 0 {
                        hu[i] = limit;
                    } else {
                        lu[i] = limit;
                    }
                } else {
                    let val = if ii == 1 { -angle_max } else { angle_max };
                    if ii == 0 {
                        hu[i] = val;
                    } else {
                        lu[i] = val;
                    }
                }
            } else {
                let val = if ii == 1 { -angle_max } else { angle_max };
                if ii == 0 {
                    hu[i] = val;
                } else {
                    lu[i] = val;
                }
            }

            // Restore user coordinate
            ax[i] = save;
            *m0x = user_to_motor(
                geom, torad, yang, pp0, pp1, pp2, ppo0, ppo1, ppo2, a, lnk_stat, ax0, ax,
            );
        }
    }

    (hu, lu)
}

/// Convert translation limits from local user coords to lab user coords,
/// accounting for the YANG rotation quadrant.
fn user_limits_local_to_lab(yang: f64, hu: &mut [f64; 6], lu: &mut [f64; 6]) {
    let sa = (yang * D2R).sin();
    let ca = (yang * D2R).cos();

    let quadrant: u8 =
        ((if sa >= 0.0 { 0x10 } else { 0x00 }) | (if ca >= 0.0 { 0x01 } else { 0x00 })) as u8;

    let (hi_x, lo_x, hi_z, lo_z) = match quadrant {
        0x11 => (
            ca * hu[X_6] - sa * lu[Z_6],
            ca * lu[X_6] - sa * hu[Z_6],
            sa * hu[X_6] + ca * hu[Z_6],
            sa * lu[X_6] + ca * lu[Z_6],
        ),
        0x10 => (
            ca * lu[X_6] - sa * lu[Z_6],
            ca * hu[X_6] - sa * hu[Z_6],
            sa * hu[X_6] + ca * lu[Z_6],
            sa * lu[X_6] + ca * hu[Z_6],
        ),
        0x00 => (
            ca * lu[X_6] - sa * hu[Z_6],
            ca * hu[X_6] - sa * lu[Z_6],
            sa * lu[X_6] + ca * lu[Z_6],
            sa * hu[X_6] + ca * hu[Z_6],
        ),
        _ => (
            // 0x01 and default
            ca * hu[X_6] - sa * hu[Z_6],
            ca * lu[X_6] - sa * lu[Z_6],
            sa * lu[X_6] + ca * hu[Z_6],
            sa * hu[X_6] + ca * lu[Z_6],
        ),
    };

    hu[X_6] = hi_x;
    lu[X_6] = lo_x;
    hu[Z_6] = hi_z;
    lu[Z_6] = lo_z;
}

/// Full user limits calculation: local limits + lab conversion + user limit enforcement.
#[allow(clippy::too_many_arguments)]
fn calc_user_limits(
    geom: Geometry,
    torad: f64,
    yang: f64,
    aunit: AngleUnit,
    pp0: &mut [f64; 3],
    pp1: &mut [f64; 3],
    pp2: &mut [f64; 3],
    ppo0: &[f64; 3],
    ppo1: &[f64; 3],
    ppo2: &[f64; 3],
    a: &mut [[f64; 3]; 3],
    _b: &[[f64; 3]; 3],
    lnk_stat: &[LinkStatus; 6],
    ax: &mut [f64; 6],
    ax0: &[f64; 6],
    m0x: &mut [f64; 6],
    hm: &[f64; 6],
    lm: &[f64; 6],
    uhax: &[f64; 6],
    ulax: &[f64; 6],
    uhaxr: &[f64; 6],
    ulaxr: &[f64; 6],
) -> ([f64; 6], [f64; 6]) {
    let (mut hu, mut lu) = calc_local_user_limits(
        geom, torad, yang, aunit, pp0, pp1, pp2, ppo0, ppo1, ppo2, a, _b, lnk_stat, ax, ax0, m0x,
        hm, lm,
    );

    user_limits_local_to_lab(yang, &mut hu, &mut lu);

    // Enforce user's limits
    for i in 0..6 {
        if uhax[i].abs() > SMALL || ulax[i].abs() > SMALL {
            hu[i] = hu[i].min(uhaxr[i]);
            lu[i] = lu[i].max(ulaxr[i]);
        }
    }

    (hu, lu)
}

// ===========================================================================
// Record impl - process and link I/O
// ===========================================================================

impl TableRecord {
    pub fn new() -> Self {
        Self::default()
    }

    /// Initialize geometry from current parameters.
    fn do_init_geometry(&mut self) {
        let (pp0, pp1, pp2, ppo0, ppo1, ppo2, bb) = init_geometry(
            self.geom, self.lx, self.lz, self.sx, self.sy, self.sz, self.rx, self.ry, self.rz,
        );
        self.pp0 = pp0;
        self.pp1 = pp1;
        self.pp2 = pp2;
        self.ppo0 = ppo0;
        self.ppo1 = ppo1;
        self.ppo2 = ppo2;
        self.b = bb;
    }

    /// Perform MotorToUser on the record's current state.
    fn do_motor_to_user(&mut self, m: &[f64; 6]) -> [f64; 6] {
        let ax0 = self.user_offset();
        motor_to_user(
            self.geom,
            self.torad,
            self.yang,
            &mut self.pp0,
            &mut self.pp1,
            &mut self.pp2,
            &self.ppo0,
            &self.ppo1,
            &self.ppo2,
            &mut self.a,
            &self.b,
            &self.lnk_stat,
            &ax0,
            m,
        )
    }

    /// Perform UserToMotor on the record's current state.
    fn do_user_to_motor(&mut self, user: &[f64; 6]) -> [f64; 6] {
        let ax0 = self.user_offset();
        user_to_motor(
            self.geom,
            self.torad,
            self.yang,
            &mut self.pp0,
            &mut self.pp1,
            &mut self.pp2,
            &self.ppo0,
            &self.ppo1,
            &self.ppo2,
            &mut self.a,
            &self.lnk_stat,
            &ax0,
            user,
        )
    }

    /// Build ReadDbLink actions to read motor readbacks.
    fn build_read_rbv_actions(&self) -> Vec<ProcessAction> {
        let mut actions = Vec::new();
        for i in 0..6 {
            if self.lnk_stat[i].can_rw_drive {
                actions.push(ProcessAction::ReadDbLink {
                    link_field: MOTOR_RBV_LINK[i],
                    target_field: MOTOR_RBV_VAL[i],
                });
            }
        }
        actions
    }

    /// Build ReadDbLink actions to read encoders.
    fn build_read_encoder_actions(&self) -> Vec<ProcessAction> {
        let mut actions = Vec::new();
        for i in 0..6 {
            if self.lnk_stat[i].can_read_position {
                actions.push(ProcessAction::ReadDbLink {
                    link_field: ENCODER_LINK[i],
                    target_field: ENCODER_VAL[i],
                });
            }
        }
        actions
    }

    /// Build ReadDbLink actions to read motor limits.
    fn build_read_limit_actions(&self) -> Vec<ProcessAction> {
        let mut actions = Vec::new();
        for i in 0..6 {
            if self.lnk_stat[i].can_read_limits {
                actions.push(ProcessAction::ReadDbLink {
                    link_field: HI_LIMIT_LINK[i],
                    target_field: HI_LIMIT_VAL[i],
                });
                actions.push(ProcessAction::ReadDbLink {
                    link_field: LO_LIMIT_LINK[i],
                    target_field: LO_LIMIT_VAL[i],
                });
            }
        }
        actions
    }

    /// Build ReadDbLink actions to read motor speeds.
    fn build_read_speed_actions(&self) -> Vec<ProcessAction> {
        let mut actions = Vec::new();
        for i in 0..6 {
            if self.lnk_stat[i].can_rw_speed {
                actions.push(ProcessAction::ReadDbLink {
                    link_field: SPEED_IN_LINK[i],
                    target_field: SPEED_VAL[i],
                });
            }
        }
        actions
    }

    /// Build WriteDbLink actions to write speed then drive values.
    fn build_output_actions(
        &self,
        motor_move_mask: u8,
        saved_speeds: &[f64; 6],
    ) -> Vec<ProcessAction> {
        let mut actions = Vec::new();
        let speeds = self.speed_val();
        let motors = self.motor_drive();

        // Write speeds for motors that need to move
        for i in 0..6 {
            if (motor_move_mask & (1 << i)) != 0 && self.lnk_stat[i].can_rw_speed {
                actions.push(ProcessAction::WriteDbLink {
                    link_field: SPEED_OUT_LINK[i],
                    value: EpicsValue::Double(speeds[i]),
                });
            }
        }

        // Write drive values for all connected motors
        for i in 0..6 {
            if self.lnk_stat[i].can_rw_drive {
                actions.push(ProcessAction::WriteDbLink {
                    link_field: MOTOR_DRIVE_LINK[i],
                    value: EpicsValue::Double(motors[i]),
                });
            }
        }

        // Restore original speeds
        for i in 0..6 {
            if (motor_move_mask & (1 << i)) != 0 && self.lnk_stat[i].can_rw_speed {
                actions.push(ProcessAction::WriteDbLink {
                    link_field: SPEED_OUT_LINK[i],
                    value: EpicsValue::Double(saved_speeds[i]),
                });
            }
        }

        actions
    }

    /// Determine link status by checking if link fields are non-empty.
    fn check_links(&mut self) {
        let drive_links = [
            &self.m0xl, &self.m0yl, &self.m1yl, &self.m2xl, &self.m2yl, &self.m2zl,
        ];
        let rbv_links = [
            &self.r0xi, &self.r0yi, &self.r1yi, &self.r2xi, &self.r2yi, &self.r2zi,
        ];
        let enc_links = [
            &self.e0xi, &self.e0yi, &self.e1yi, &self.e2xi, &self.e2yi, &self.e2zi,
        ];
        let spd_out = [
            &self.v0xl, &self.v0yl, &self.v1yl, &self.v2xl, &self.v2yl, &self.v2zl,
        ];
        let spd_in = [
            &self.v0xi, &self.v0yi, &self.v1yi, &self.v2xi, &self.v2yi, &self.v2zi,
        ];
        let hlm_links = [
            &self.h0xl, &self.h0yl, &self.h1yl, &self.h2xl, &self.h2yl, &self.h2zl,
        ];
        let llm_links = [
            &self.l0xl, &self.l0yl, &self.l1yl, &self.l2xl, &self.l2yl, &self.l2zl,
        ];

        for i in 0..6 {
            self.lnk_stat[i].can_rw_drive = !drive_links[i].is_empty() && !rbv_links[i].is_empty();
            self.lnk_stat[i].can_read_limits = !hlm_links[i].is_empty() && !llm_links[i].is_empty();
            self.lnk_stat[i].can_read_position = !enc_links[i].is_empty();
            self.lnk_stat[i].can_rw_speed = !spd_out[i].is_empty() && !spd_in[i].is_empty();
        }
    }

    /// Perform the full user-limit calculation and store results.
    fn do_calc_user_limits(&mut self) {
        let mut ax = self.user_drive();
        let ax0 = self.user_offset();
        let mut m0x = self.motor_drive();
        let hm = self.hi_motor_limit();
        let lm = self.lo_motor_limit();
        let uhax = self.user_hi_abs();
        let ulax = self.user_lo_abs();
        let uhaxr = self.user_hi_rel();
        let ulaxr = self.user_lo_rel();

        let (hu, lu) = calc_user_limits(
            self.geom,
            self.torad,
            self.yang,
            self.aunit,
            &mut self.pp0,
            &mut self.pp1,
            &mut self.pp2,
            &self.ppo0,
            &self.ppo1,
            &self.ppo2,
            &mut self.a,
            &self.b,
            &self.lnk_stat,
            &mut ax,
            &ax0,
            &mut m0x,
            &hm,
            &lm,
            &uhax,
            &ulax,
            &uhaxr,
            &ulaxr,
        );

        self.set_calc_hi_limit(&hu);
        self.set_calc_lo_limit(&lu);
        // Restore user drive and motors (calc_local_user_limits may have modified them temporarily)
        self.set_user_drive(&ax);
        self.set_motor_drive(&m0x);
    }
}

// ===========================================================================
// Field list (LazyLock + Box::leak)
// ===========================================================================

static ALL_FIELDS: LazyLock<Vec<FieldDesc>> = LazyLock::new(|| {
    vec![
        FieldDesc {
            name: "VERS",
            dbf_type: DbFieldType::Float,
            read_only: true,
        },
        FieldDesc {
            name: "VAL",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        // Geometry parameters
        FieldDesc {
            name: "LX",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "LZ",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "SX",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "SY",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "SZ",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "RX",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "RY",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "RZ",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "YANG",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        // User coordinates
        FieldDesc {
            name: "AX",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "AY",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "AZ",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "X",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "Y",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "Z",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        // Offsets
        FieldDesc {
            name: "AX0",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "AY0",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "AZ0",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "X0",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "Y0",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "Z0",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        // True values
        FieldDesc {
            name: "AXL",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "AYL",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "AZL",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "XL",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "YL",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "ZL",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        // Readbacks
        FieldDesc {
            name: "AXRB",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "AYRB",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "AZRB",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "XRB",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "YRB",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "ZRB",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        // Encoder user
        FieldDesc {
            name: "EAX",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "EAY",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "EAZ",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "EX",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "EY",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "EZ",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        // Calculated user limits
        FieldDesc {
            name: "HLAX",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "HLAY",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "HLAZ",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "HLX",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "HLY",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "HLZ",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "LLAX",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "LLAY",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "LLAZ",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "LLX",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "LLY",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "LLZ",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        // User limits (absolute)
        FieldDesc {
            name: "UHAX",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "UHAY",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "UHAZ",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "UHX",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "UHY",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "UHZ",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "ULAX",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "ULAY",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "ULAZ",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "ULX",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "ULY",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "ULZ",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        // User limits (relative)
        FieldDesc {
            name: "UHAXR",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "UHAYR",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "UHAZR",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "UHXR",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "UHYR",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "UHZR",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "ULAXR",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "ULAYR",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "ULAZR",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "ULXR",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "ULYR",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "ULZR",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        // Motor drive links
        FieldDesc {
            name: "M0XL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "M0YL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "M1YL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "M2XL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "M2YL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "M2ZL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        // Motor drive values
        FieldDesc {
            name: "M0X",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "M0Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "M1Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "M2X",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "M2Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "M2Z",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        // Motor readback links
        FieldDesc {
            name: "R0XI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "R0YI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "R1YI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "R2XI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "R2YI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "R2ZI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        // Motor readback values
        FieldDesc {
            name: "R0X",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "R0Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "R1Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "R2X",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "R2Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "R2Z",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        // Encoder links
        FieldDesc {
            name: "E0XI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "E0YI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "E1YI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "E2XI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "E2YI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "E2ZI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        // Encoder motor values
        FieldDesc {
            name: "E0X",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "E0Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "E1Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "E2X",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "E2Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "E2Z",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        // Speed output links
        FieldDesc {
            name: "V0XL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "V0YL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "V1YL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "V2XL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "V2YL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "V2ZL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        // Speed values
        FieldDesc {
            name: "V0X",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "V0Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "V1Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "V2X",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "V2Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "V2Z",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        // Speed input links
        FieldDesc {
            name: "V0XI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "V0YI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "V1YI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "V2XI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "V2YI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "V2ZI",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        // Motor hi limit links
        FieldDesc {
            name: "H0XL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "H0YL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "H1YL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "H2XL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "H2YL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "H2ZL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        // Motor hi limit values
        FieldDesc {
            name: "H0X",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "H0Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "H1Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "H2X",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "H2Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "H2Z",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        // Motor lo limit links
        FieldDesc {
            name: "L0XL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "L0YL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "L1YL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "L2XL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "L2YL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "L2ZL",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        // Motor lo limit values
        FieldDesc {
            name: "L0X",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "L0Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "L1Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "L2X",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "L2Y",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "L2Z",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        // Control fields
        FieldDesc {
            name: "INIT",
            dbf_type: DbFieldType::Short,
            read_only: false,
        },
        FieldDesc {
            name: "ZERO",
            dbf_type: DbFieldType::Short,
            read_only: false,
        },
        FieldDesc {
            name: "SYNC",
            dbf_type: DbFieldType::Short,
            read_only: false,
        },
        FieldDesc {
            name: "READ",
            dbf_type: DbFieldType::Short,
            read_only: false,
        },
        FieldDesc {
            name: "SET",
            dbf_type: DbFieldType::Enum,
            read_only: false,
        },
        FieldDesc {
            name: "SSET",
            dbf_type: DbFieldType::Short,
            read_only: false,
        },
        FieldDesc {
            name: "SUSE",
            dbf_type: DbFieldType::Short,
            read_only: false,
        },
        FieldDesc {
            name: "LVIO",
            dbf_type: DbFieldType::Short,
            read_only: true,
        },
        // Display / config
        FieldDesc {
            name: "LEGU",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "AEGU",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "PREC",
            dbf_type: DbFieldType::Short,
            read_only: false,
        },
        FieldDesc {
            name: "MMAP",
            dbf_type: DbFieldType::Long,
            read_only: true,
        },
        FieldDesc {
            name: "GEOM",
            dbf_type: DbFieldType::Enum,
            read_only: false,
        },
        FieldDesc {
            name: "TORAD",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "AUNIT",
            dbf_type: DbFieldType::Enum,
            read_only: false,
        },
    ]
});

// ===========================================================================
// Record trait implementation
// ===========================================================================

impl Record for TableRecord {
    fn record_type(&self) -> &'static str {
        "table"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let mut actions = Vec::new();
        self.check_links();
        self.lvio = 0;

        if self.zero != 0 {
            // --- Zero table ---
            let mut ax = self.user_drive();
            let mut ax0 = self.user_offset();
            let axl = self.user_last();
            let uhax = self.user_hi_abs();
            let ulax = self.user_lo_abs();
            let mut uhaxr = self.user_hi_rel();
            let mut ulaxr = self.user_lo_rel();

            zero_table(
                &mut ax, &mut ax0, &axl, &uhax, &ulax, &mut uhaxr, &mut ulaxr,
            );

            self.set_user_drive(&ax);
            self.set_user_offset(&ax0);
            self.set_user_hi_rel(&uhaxr);
            self.set_user_lo_rel(&ulaxr);
            self.zero = 0;
        } else if self.read != 0 {
            // --- Read motors (readback done below) ---
            self.read = 0;
        } else if self.sync != 0 {
            // --- Sync: read motor readbacks and set drive values to match ---
            let r = self.motor_readback();
            let rb = self.do_motor_to_user(&r);
            self.set_user_readback(&rb);

            let ax0 = self.user_offset();
            let mut ax = [0.0; 6];
            let mut axl = [0.0; 6];
            let mut m0x = [0.0; 6];
            for i in 0..6 {
                m0x[i] = r[i];
                ax[i] = rb[i];
                axl[i] = ax[i] + ax0[i];
            }
            self.set_user_drive(&ax);
            self.set_motor_drive(&m0x);
            self.set_user_last(&axl);
            self.sync = 0;
        } else if self.init != 0 {
            // --- Init: read motors, zero offsets, recalculate ---
            let ax0 = [0.0f64; 6];
            let uhax = self.user_hi_abs();
            let ulax = self.user_lo_abs();
            let mut uhaxr = [0.0f64; 6];
            let mut ulaxr = [0.0f64; 6];
            for i in 0..6 {
                uhaxr[i] = uhax[i] - ax0[i];
                ulaxr[i] = ulax[i] - ax0[i];
            }
            self.set_user_offset(&ax0);
            self.set_user_hi_rel(&uhaxr);
            self.set_user_lo_rel(&ulaxr);

            let r = self.motor_readback();
            let rb = self.do_motor_to_user(&r);
            self.set_user_readback(&rb);

            let mut ax = [0.0; 6];
            let mut axl = [0.0; 6];
            let mut m0x = [0.0; 6];
            for i in 0..6 {
                m0x[i] = r[i];
                ax[i] = rb[i];
                axl[i] = ax[i] + ax0[i];
            }
            self.set_user_drive(&ax);
            self.set_motor_drive(&m0x);
            self.set_user_last(&axl);
            self.init = 0;
        } else if self.set == SetMode::Set {
            // --- SET mode: update offsets ---
            let ax = self.user_drive();
            let axl = self.user_last();
            let uhax = self.user_hi_abs();
            let ulax = self.user_lo_abs();
            let mut ax0 = [0.0; 6];
            let mut uhaxr = [0.0; 6];
            let mut ulaxr = [0.0; 6];
            for i in 0..6 {
                ax0[i] = axl[i] - ax[i];
                uhaxr[i] = uhax[i] - ax0[i];
                ulaxr[i] = ulax[i] - ax0[i];
            }
            self.set_user_offset(&ax0);
            self.set_user_hi_rel(&uhaxr);
            self.set_user_lo_rel(&ulaxr);
        } else {
            // --- Calc & Move ---
            let sm = self.motor_drive();
            let ax = self.user_drive();
            let m = self.do_user_to_motor(&ax);
            self.set_motor_drive(&m);

            let hlax = self.calc_hi_limit();
            let llax = self.calc_lo_limit();
            let hm = self.hi_motor_limit();
            let lm = self.lo_motor_limit();

            if user_limit_viol(&ax, &hlax, &llax) || motor_limit_viol(&m, &hm, &lm, &self.lnk_stat)
            {
                // Limit violation: restore
                self.lvio = 1;
                self.set_motor_drive(&sm);
                let axl = self.user_last();
                let ax0 = self.user_offset();
                let mut ax_restored = [0.0; 6];
                for i in 0..6 {
                    ax_restored[i] = axl[i] - ax0[i];
                }
                self.set_user_drive(&ax_restored);
            } else {
                // Save motor speeds, coordinate speeds, write outputs
                let sv0x = self.speed_val();

                // Find which motors must move and max distance/speed
                let mut motor_move_mask: u8 = 0;
                let mut velo: f64 = 0.0;
                let mut move_max: f64 = 0.0;

                for i in 0..6 {
                    if self.lnk_stat[i].can_rw_speed && (m[i] - sm[i]).abs() > SMALL {
                        motor_move_mask |= 1 << i;
                        move_max = move_max.max((m[i] - sm[i]).abs());
                        velo = velo.max(sv0x[i]);
                    }
                }

                if move_max > SMALL {
                    let mut v = [0.0f64; 6];
                    let mut speed_ratio = LARGE;
                    for i in 0..6 {
                        if self.lnk_stat[i].can_rw_speed {
                            v[i] = velo * (m[i] - sm[i]).abs() / move_max;
                            if v[i] > 0.0 {
                                speed_ratio = speed_ratio.min(sv0x[i] / v[i]);
                            }
                        }
                    }
                    if speed_ratio < 1.0 {
                        for vi in v.iter_mut() {
                            *vi *= speed_ratio;
                        }
                    }
                    self.set_speed_val(&v);
                } else {
                    self.set_speed_val(&sv0x);
                }

                actions.extend(self.build_output_actions(motor_move_mask, &sv0x));
            }
        }

        // Save user-coordinate values (ignore offsets)
        let ax = self.user_drive();
        let ax0 = self.user_offset();
        let mut axl = [0.0; 6];
        for i in 0..6 {
            axl[i] = ax[i] + ax0[i];
        }
        self.set_user_last(&axl);

        // Calculate user limits
        self.do_calc_user_limits();

        // Read motor drive values and transform to offset user coordinates
        let r = self.motor_readback();
        let rb = self.do_motor_to_user(&r);
        self.set_user_readback(&rb);

        // Read encoders and transform to offset user coordinates
        let e = self.encoder_motor();
        let eu = self.do_motor_to_user(&e);
        self.set_encoder_user(&eu);

        // Pre-process actions (read links) are done via pre_process_actions
        Ok(ProcessOutcome::complete_with(actions))
    }

    fn pre_process_actions(&mut self) -> Vec<ProcessAction> {
        let mut actions = Vec::new();
        actions.extend(self.build_read_rbv_actions());
        actions.extend(self.build_read_encoder_actions());
        actions.extend(self.build_read_limit_actions());
        actions.extend(self.build_read_speed_actions());
        actions
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VERS" => Some(EpicsValue::Float(self.vers)),
            "VAL" => Some(EpicsValue::Double(self.val)),
            "LX" => Some(EpicsValue::Double(self.lx)),
            "LZ" => Some(EpicsValue::Double(self.lz)),
            "SX" => Some(EpicsValue::Double(self.sx)),
            "SY" => Some(EpicsValue::Double(self.sy)),
            "SZ" => Some(EpicsValue::Double(self.sz)),
            "RX" => Some(EpicsValue::Double(self.rx)),
            "RY" => Some(EpicsValue::Double(self.ry)),
            "RZ" => Some(EpicsValue::Double(self.rz)),
            "YANG" => Some(EpicsValue::Double(self.yang)),
            "AX" => Some(EpicsValue::Double(self.ax)),
            "AY" => Some(EpicsValue::Double(self.ay)),
            "AZ" => Some(EpicsValue::Double(self.az)),
            "X" => Some(EpicsValue::Double(self.x)),
            "Y" => Some(EpicsValue::Double(self.y)),
            "Z" => Some(EpicsValue::Double(self.z)),
            "AX0" => Some(EpicsValue::Double(self.ax0)),
            "AY0" => Some(EpicsValue::Double(self.ay0)),
            "AZ0" => Some(EpicsValue::Double(self.az0)),
            "X0" => Some(EpicsValue::Double(self.x0)),
            "Y0" => Some(EpicsValue::Double(self.y0)),
            "Z0" => Some(EpicsValue::Double(self.z0)),
            "AXL" => Some(EpicsValue::Double(self.axl)),
            "AYL" => Some(EpicsValue::Double(self.ayl)),
            "AZL" => Some(EpicsValue::Double(self.azl)),
            "XL" => Some(EpicsValue::Double(self.xl)),
            "YL" => Some(EpicsValue::Double(self.yl)),
            "ZL" => Some(EpicsValue::Double(self.zl)),
            "AXRB" => Some(EpicsValue::Double(self.axrb)),
            "AYRB" => Some(EpicsValue::Double(self.ayrb)),
            "AZRB" => Some(EpicsValue::Double(self.azrb)),
            "XRB" => Some(EpicsValue::Double(self.xrb)),
            "YRB" => Some(EpicsValue::Double(self.yrb)),
            "ZRB" => Some(EpicsValue::Double(self.zrb)),
            "EAX" => Some(EpicsValue::Double(self.eax)),
            "EAY" => Some(EpicsValue::Double(self.eay)),
            "EAZ" => Some(EpicsValue::Double(self.eaz)),
            "EX" => Some(EpicsValue::Double(self.ex)),
            "EY" => Some(EpicsValue::Double(self.ey)),
            "EZ" => Some(EpicsValue::Double(self.ez)),
            "HLAX" => Some(EpicsValue::Double(self.hlax)),
            "HLAY" => Some(EpicsValue::Double(self.hlay)),
            "HLAZ" => Some(EpicsValue::Double(self.hlaz)),
            "HLX" => Some(EpicsValue::Double(self.hlx)),
            "HLY" => Some(EpicsValue::Double(self.hly)),
            "HLZ" => Some(EpicsValue::Double(self.hlz)),
            "LLAX" => Some(EpicsValue::Double(self.llax)),
            "LLAY" => Some(EpicsValue::Double(self.llay)),
            "LLAZ" => Some(EpicsValue::Double(self.llaz)),
            "LLX" => Some(EpicsValue::Double(self.llx)),
            "LLY" => Some(EpicsValue::Double(self.lly)),
            "LLZ" => Some(EpicsValue::Double(self.llz)),
            "UHAX" => Some(EpicsValue::Double(self.uhax)),
            "UHAY" => Some(EpicsValue::Double(self.uhay)),
            "UHAZ" => Some(EpicsValue::Double(self.uhaz)),
            "UHX" => Some(EpicsValue::Double(self.uhx)),
            "UHY" => Some(EpicsValue::Double(self.uhy)),
            "UHZ" => Some(EpicsValue::Double(self.uhz)),
            "ULAX" => Some(EpicsValue::Double(self.ulax)),
            "ULAY" => Some(EpicsValue::Double(self.ulay)),
            "ULAZ" => Some(EpicsValue::Double(self.ulaz)),
            "ULX" => Some(EpicsValue::Double(self.ulx)),
            "ULY" => Some(EpicsValue::Double(self.uly)),
            "ULZ" => Some(EpicsValue::Double(self.ulz)),
            "UHAXR" => Some(EpicsValue::Double(self.uhaxr)),
            "UHAYR" => Some(EpicsValue::Double(self.uhayr)),
            "UHAZR" => Some(EpicsValue::Double(self.uhazr)),
            "UHXR" => Some(EpicsValue::Double(self.uhxr)),
            "UHYR" => Some(EpicsValue::Double(self.uhyr)),
            "UHZR" => Some(EpicsValue::Double(self.uhzr)),
            "ULAXR" => Some(EpicsValue::Double(self.ulaxr)),
            "ULAYR" => Some(EpicsValue::Double(self.ulayr)),
            "ULAZR" => Some(EpicsValue::Double(self.ulazr)),
            "ULXR" => Some(EpicsValue::Double(self.ulxr)),
            "ULYR" => Some(EpicsValue::Double(self.ulyr)),
            "ULZR" => Some(EpicsValue::Double(self.ulzr)),
            "M0XL" => Some(EpicsValue::String(self.m0xl.clone())),
            "M0YL" => Some(EpicsValue::String(self.m0yl.clone())),
            "M1YL" => Some(EpicsValue::String(self.m1yl.clone())),
            "M2XL" => Some(EpicsValue::String(self.m2xl.clone())),
            "M2YL" => Some(EpicsValue::String(self.m2yl.clone())),
            "M2ZL" => Some(EpicsValue::String(self.m2zl.clone())),
            "M0X" => Some(EpicsValue::Double(self.m0x)),
            "M0Y" => Some(EpicsValue::Double(self.m0y)),
            "M1Y" => Some(EpicsValue::Double(self.m1y)),
            "M2X" => Some(EpicsValue::Double(self.m2x)),
            "M2Y" => Some(EpicsValue::Double(self.m2y)),
            "M2Z" => Some(EpicsValue::Double(self.m2z)),
            "R0XI" => Some(EpicsValue::String(self.r0xi.clone())),
            "R0YI" => Some(EpicsValue::String(self.r0yi.clone())),
            "R1YI" => Some(EpicsValue::String(self.r1yi.clone())),
            "R2XI" => Some(EpicsValue::String(self.r2xi.clone())),
            "R2YI" => Some(EpicsValue::String(self.r2yi.clone())),
            "R2ZI" => Some(EpicsValue::String(self.r2zi.clone())),
            "R0X" => Some(EpicsValue::Double(self.r0x)),
            "R0Y" => Some(EpicsValue::Double(self.r0y)),
            "R1Y" => Some(EpicsValue::Double(self.r1y)),
            "R2X" => Some(EpicsValue::Double(self.r2x)),
            "R2Y" => Some(EpicsValue::Double(self.r2y)),
            "R2Z" => Some(EpicsValue::Double(self.r2z)),
            "E0XI" => Some(EpicsValue::String(self.e0xi.clone())),
            "E0YI" => Some(EpicsValue::String(self.e0yi.clone())),
            "E1YI" => Some(EpicsValue::String(self.e1yi.clone())),
            "E2XI" => Some(EpicsValue::String(self.e2xi.clone())),
            "E2YI" => Some(EpicsValue::String(self.e2yi.clone())),
            "E2ZI" => Some(EpicsValue::String(self.e2zi.clone())),
            "E0X" => Some(EpicsValue::Double(self.e0x)),
            "E0Y" => Some(EpicsValue::Double(self.e0y)),
            "E1Y" => Some(EpicsValue::Double(self.e1y)),
            "E2X" => Some(EpicsValue::Double(self.e2x)),
            "E2Y" => Some(EpicsValue::Double(self.e2y)),
            "E2Z" => Some(EpicsValue::Double(self.e2z)),
            "V0XL" => Some(EpicsValue::String(self.v0xl.clone())),
            "V0YL" => Some(EpicsValue::String(self.v0yl.clone())),
            "V1YL" => Some(EpicsValue::String(self.v1yl.clone())),
            "V2XL" => Some(EpicsValue::String(self.v2xl.clone())),
            "V2YL" => Some(EpicsValue::String(self.v2yl.clone())),
            "V2ZL" => Some(EpicsValue::String(self.v2zl.clone())),
            "V0X" => Some(EpicsValue::Double(self.v0x)),
            "V0Y" => Some(EpicsValue::Double(self.v0y)),
            "V1Y" => Some(EpicsValue::Double(self.v1y)),
            "V2X" => Some(EpicsValue::Double(self.v2x)),
            "V2Y" => Some(EpicsValue::Double(self.v2y)),
            "V2Z" => Some(EpicsValue::Double(self.v2z)),
            "V0XI" => Some(EpicsValue::String(self.v0xi.clone())),
            "V0YI" => Some(EpicsValue::String(self.v0yi.clone())),
            "V1YI" => Some(EpicsValue::String(self.v1yi.clone())),
            "V2XI" => Some(EpicsValue::String(self.v2xi.clone())),
            "V2YI" => Some(EpicsValue::String(self.v2yi.clone())),
            "V2ZI" => Some(EpicsValue::String(self.v2zi.clone())),
            "H0XL" => Some(EpicsValue::String(self.h0xl.clone())),
            "H0YL" => Some(EpicsValue::String(self.h0yl.clone())),
            "H1YL" => Some(EpicsValue::String(self.h1yl.clone())),
            "H2XL" => Some(EpicsValue::String(self.h2xl.clone())),
            "H2YL" => Some(EpicsValue::String(self.h2yl.clone())),
            "H2ZL" => Some(EpicsValue::String(self.h2zl.clone())),
            "H0X" => Some(EpicsValue::Double(self.h0x)),
            "H0Y" => Some(EpicsValue::Double(self.h0y)),
            "H1Y" => Some(EpicsValue::Double(self.h1y)),
            "H2X" => Some(EpicsValue::Double(self.h2x)),
            "H2Y" => Some(EpicsValue::Double(self.h2y)),
            "H2Z" => Some(EpicsValue::Double(self.h2z)),
            "L0XL" => Some(EpicsValue::String(self.l0xl.clone())),
            "L0YL" => Some(EpicsValue::String(self.l0yl.clone())),
            "L1YL" => Some(EpicsValue::String(self.l1yl.clone())),
            "L2XL" => Some(EpicsValue::String(self.l2xl.clone())),
            "L2YL" => Some(EpicsValue::String(self.l2yl.clone())),
            "L2ZL" => Some(EpicsValue::String(self.l2zl.clone())),
            "L0X" => Some(EpicsValue::Double(self.l0x)),
            "L0Y" => Some(EpicsValue::Double(self.l0y)),
            "L1Y" => Some(EpicsValue::Double(self.l1y)),
            "L2X" => Some(EpicsValue::Double(self.l2x)),
            "L2Y" => Some(EpicsValue::Double(self.l2y)),
            "L2Z" => Some(EpicsValue::Double(self.l2z)),
            "INIT" => Some(EpicsValue::Short(self.init)),
            "ZERO" => Some(EpicsValue::Short(self.zero)),
            "SYNC" => Some(EpicsValue::Short(self.sync)),
            "READ" => Some(EpicsValue::Short(self.read)),
            "SET" => Some(EpicsValue::Enum(self.set as u16)),
            "SSET" => Some(EpicsValue::Short(self.sset)),
            "SUSE" => Some(EpicsValue::Short(self.suse)),
            "LVIO" => Some(EpicsValue::Short(self.lvio)),
            "LEGU" => Some(EpicsValue::String(self.legu.clone())),
            "AEGU" => Some(EpicsValue::String(self.aegu.clone())),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "MMAP" => Some(EpicsValue::Long(self.mmap as i32)),
            "GEOM" => Some(EpicsValue::Enum(self.geom as u16)),
            "TORAD" => Some(EpicsValue::Double(self.torad)),
            "AUNIT" => Some(EpicsValue::Enum(self.aunit as u16)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        macro_rules! put_double {
            ($field:expr) => {
                match value {
                    EpicsValue::Double(v) => {
                        $field = v;
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch(name.into())),
                }
            };
        }
        macro_rules! put_short {
            ($field:expr) => {
                match value {
                    EpicsValue::Short(v) => {
                        $field = v;
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch(name.into())),
                }
            };
        }
        macro_rules! put_string {
            ($field:expr) => {
                match value {
                    EpicsValue::String(v) => {
                        $field = v;
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch(name.into())),
                }
            };
        }
        macro_rules! put_enum {
            ($field:expr, $conv:expr) => {
                match value {
                    EpicsValue::Enum(v) => {
                        $field = $conv(v);
                        Ok(())
                    }
                    EpicsValue::Short(v) => {
                        $field = $conv(v as u16);
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch(name.into())),
                }
            };
        }

        match name {
            // Read-only fields
            "VERS" | "AXL" | "AYL" | "AZL" | "XL" | "YL" | "ZL" | "AXRB" | "AYRB" | "AZRB"
            | "XRB" | "YRB" | "ZRB" | "EAX" | "EAY" | "EAZ" | "EX" | "EY" | "EZ" | "HLAX"
            | "HLAY" | "HLAZ" | "HLX" | "HLY" | "HLZ" | "LLAX" | "LLAY" | "LLAZ" | "LLX"
            | "LLY" | "LLZ" | "M0X" | "M0Y" | "M1Y" | "M2X" | "M2Y" | "M2Z" | "R0X" | "R0Y"
            | "R1Y" | "R2X" | "R2Y" | "R2Z" | "E0X" | "E0Y" | "E1Y" | "E2X" | "E2Y" | "E2Z"
            | "V0X" | "V0Y" | "V1Y" | "V2X" | "V2Y" | "V2Z" | "H0X" | "H0Y" | "H1Y" | "H2X"
            | "H2Y" | "H2Z" | "L0X" | "L0Y" | "L1Y" | "L2X" | "L2Y" | "L2Z" | "LVIO" | "MMAP"
            | "TORAD" => Err(CaError::ReadOnlyField(name.into())),

            "VAL" => put_double!(self.val),
            "LX" => put_double!(self.lx),
            "LZ" => put_double!(self.lz),
            "SX" => put_double!(self.sx),
            "SY" => put_double!(self.sy),
            "SZ" => put_double!(self.sz),
            "RX" => put_double!(self.rx),
            "RY" => put_double!(self.ry),
            "RZ" => put_double!(self.rz),
            "YANG" => put_double!(self.yang),
            "AX" => put_double!(self.ax),
            "AY" => put_double!(self.ay),
            "AZ" => put_double!(self.az),
            "X" => put_double!(self.x),
            "Y" => put_double!(self.y),
            "Z" => put_double!(self.z),
            "AX0" => put_double!(self.ax0),
            "AY0" => put_double!(self.ay0),
            "AZ0" => put_double!(self.az0),
            "X0" => put_double!(self.x0),
            "Y0" => put_double!(self.y0),
            "Z0" => put_double!(self.z0),
            "UHAX" => put_double!(self.uhax),
            "UHAY" => put_double!(self.uhay),
            "UHAZ" => put_double!(self.uhaz),
            "UHX" => put_double!(self.uhx),
            "UHY" => put_double!(self.uhy),
            "UHZ" => put_double!(self.uhz),
            "ULAX" => put_double!(self.ulax),
            "ULAY" => put_double!(self.ulay),
            "ULAZ" => put_double!(self.ulaz),
            "ULX" => put_double!(self.ulx),
            "ULY" => put_double!(self.uly),
            "ULZ" => put_double!(self.ulz),
            "UHAXR" => put_double!(self.uhaxr),
            "UHAYR" => put_double!(self.uhayr),
            "UHAZR" => put_double!(self.uhazr),
            "UHXR" => put_double!(self.uhxr),
            "UHYR" => put_double!(self.uhyr),
            "UHZR" => put_double!(self.uhzr),
            "ULAXR" => put_double!(self.ulaxr),
            "ULAYR" => put_double!(self.ulayr),
            "ULAZR" => put_double!(self.ulazr),
            "ULXR" => put_double!(self.ulxr),
            "ULYR" => put_double!(self.ulyr),
            "ULZR" => put_double!(self.ulzr),
            "M0XL" => put_string!(self.m0xl),
            "M0YL" => put_string!(self.m0yl),
            "M1YL" => put_string!(self.m1yl),
            "M2XL" => put_string!(self.m2xl),
            "M2YL" => put_string!(self.m2yl),
            "M2ZL" => put_string!(self.m2zl),
            "R0XI" => put_string!(self.r0xi),
            "R0YI" => put_string!(self.r0yi),
            "R1YI" => put_string!(self.r1yi),
            "R2XI" => put_string!(self.r2xi),
            "R2YI" => put_string!(self.r2yi),
            "R2ZI" => put_string!(self.r2zi),
            "E0XI" => put_string!(self.e0xi),
            "E0YI" => put_string!(self.e0yi),
            "E1YI" => put_string!(self.e1yi),
            "E2XI" => put_string!(self.e2xi),
            "E2YI" => put_string!(self.e2yi),
            "E2ZI" => put_string!(self.e2zi),
            "V0XL" => put_string!(self.v0xl),
            "V0YL" => put_string!(self.v0yl),
            "V1YL" => put_string!(self.v1yl),
            "V2XL" => put_string!(self.v2xl),
            "V2YL" => put_string!(self.v2yl),
            "V2ZL" => put_string!(self.v2zl),
            "V0XI" => put_string!(self.v0xi),
            "V0YI" => put_string!(self.v0yi),
            "V1YI" => put_string!(self.v1yi),
            "V2XI" => put_string!(self.v2xi),
            "V2YI" => put_string!(self.v2yi),
            "V2ZI" => put_string!(self.v2zi),
            "H0XL" => put_string!(self.h0xl),
            "H0YL" => put_string!(self.h0yl),
            "H1YL" => put_string!(self.h1yl),
            "H2XL" => put_string!(self.h2xl),
            "H2YL" => put_string!(self.h2yl),
            "H2ZL" => put_string!(self.h2zl),
            "L0XL" => put_string!(self.l0xl),
            "L0YL" => put_string!(self.l0yl),
            "L1YL" => put_string!(self.l1yl),
            "L2XL" => put_string!(self.l2xl),
            "L2YL" => put_string!(self.l2yl),
            "L2ZL" => put_string!(self.l2zl),
            "INIT" => put_short!(self.init),
            "ZERO" => put_short!(self.zero),
            "SYNC" => put_short!(self.sync),
            "READ" => put_short!(self.read),
            "SET" => put_enum!(self.set, SetMode::from_u16),
            "SSET" => put_short!(self.sset),
            "SUSE" => put_short!(self.suse),
            "LEGU" => put_string!(self.legu),
            "AEGU" => put_string!(self.aegu),
            "PREC" => put_short!(self.prec),
            "GEOM" => put_enum!(self.geom, Geometry::from_u16),
            "AUNIT" => put_enum!(self.aunit, AngleUnit::from_u16),
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }

    fn put_field_internal(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        // Allow the framework to write read-only motor/encoder/limit/speed fields
        macro_rules! put_double_internal {
            ($field:expr) => {
                match value {
                    EpicsValue::Double(v) => {
                        $field = v;
                        Ok(())
                    }
                    _ => Err(CaError::TypeMismatch(name.into())),
                }
            };
        }
        match name {
            "M0X" => put_double_internal!(self.m0x),
            "M0Y" => put_double_internal!(self.m0y),
            "M1Y" => put_double_internal!(self.m1y),
            "M2X" => put_double_internal!(self.m2x),
            "M2Y" => put_double_internal!(self.m2y),
            "M2Z" => put_double_internal!(self.m2z),
            "R0X" => put_double_internal!(self.r0x),
            "R0Y" => put_double_internal!(self.r0y),
            "R1Y" => put_double_internal!(self.r1y),
            "R2X" => put_double_internal!(self.r2x),
            "R2Y" => put_double_internal!(self.r2y),
            "R2Z" => put_double_internal!(self.r2z),
            "E0X" => put_double_internal!(self.e0x),
            "E0Y" => put_double_internal!(self.e0y),
            "E1Y" => put_double_internal!(self.e1y),
            "E2X" => put_double_internal!(self.e2x),
            "E2Y" => put_double_internal!(self.e2y),
            "E2Z" => put_double_internal!(self.e2z),
            "V0X" => put_double_internal!(self.v0x),
            "V0Y" => put_double_internal!(self.v0y),
            "V1Y" => put_double_internal!(self.v1y),
            "V2X" => put_double_internal!(self.v2x),
            "V2Y" => put_double_internal!(self.v2y),
            "V2Z" => put_double_internal!(self.v2z),
            "H0X" => put_double_internal!(self.h0x),
            "H0Y" => put_double_internal!(self.h0y),
            "H1Y" => put_double_internal!(self.h1y),
            "H2X" => put_double_internal!(self.h2x),
            "H2Y" => put_double_internal!(self.h2y),
            "H2Z" => put_double_internal!(self.h2z),
            "L0X" => put_double_internal!(self.l0x),
            "L0Y" => put_double_internal!(self.l0y),
            "L1Y" => put_double_internal!(self.l1y),
            "L2X" => put_double_internal!(self.l2x),
            "L2Y" => put_double_internal!(self.l2y),
            "L2Z" => put_double_internal!(self.l2z),
            _ => self.put_field(name, value),
        }
    }

    fn on_put(&mut self, field: &str) {
        match field {
            // Geometry parameter changes -> re-initialize
            "LX" | "LZ" | "SX" | "SY" | "SZ" | "RX" | "RY" | "RZ" => {
                self.do_init_geometry();
                self.sync = 1;
            }
            "GEOM" => {
                self.do_init_geometry();
                self.sync = 1;
            }
            "YANG" => {
                // YANG changed: convert offsets lab->local (pre), then local->lab (post).
                // Since on_put is called after, we do the full re-sync.
                // The C code does: before: LabToLocal(ax0), after: LocalToLab(ax0).
                // In the Rust model we handle this as a sync.
                self.sync = 1;
            }
            // Absolute user limits -> update relative
            "UHAX" | "UHAY" | "UHAZ" | "UHX" | "UHY" | "UHZ" | "ULAX" | "ULAY" | "ULAZ" | "ULX"
            | "ULY" | "ULZ" => {
                // Map field name to index in the 12-element absolute limit space
                let abs_hi = ["UHAX", "UHAY", "UHAZ", "UHX", "UHY", "UHZ"];
                let abs_lo = ["ULAX", "ULAY", "ULAZ", "ULX", "ULY", "ULZ"];
                let ax0 = self.user_offset();

                if let Some(idx) = abs_hi.iter().position(|&f| f == field) {
                    let uhax = self.user_hi_abs();
                    let mut uhaxr = self.user_hi_rel();
                    uhaxr[idx] = uhax[idx] - ax0[idx];
                    self.set_user_hi_rel(&uhaxr);
                } else if let Some(idx) = abs_lo.iter().position(|&f| f == field) {
                    let ulax = self.user_lo_abs();
                    let mut ulaxr = self.user_lo_rel();
                    ulaxr[idx] = ulax[idx] - ax0[idx];
                    self.set_user_lo_rel(&ulaxr);
                }
            }
            // Relative user limits -> update absolute
            "UHAXR" | "UHAYR" | "UHAZR" | "UHXR" | "UHYR" | "UHZR" | "ULAXR" | "ULAYR"
            | "ULAZR" | "ULXR" | "ULYR" | "ULZR" => {
                let rel_hi = ["UHAXR", "UHAYR", "UHAZR", "UHXR", "UHYR", "UHZR"];
                let rel_lo = ["ULAXR", "ULAYR", "ULAZR", "ULXR", "ULYR", "ULZR"];
                let ax0 = self.user_offset();

                if let Some(idx) = rel_hi.iter().position(|&f| f == field) {
                    let uhaxr = self.user_hi_rel();
                    let mut uhax = self.user_hi_abs();
                    uhax[idx] = uhaxr[idx] + ax0[idx];
                    self.set_user_hi_abs(&uhax);
                } else if let Some(idx) = rel_lo.iter().position(|&f| f == field) {
                    let ulaxr = self.user_lo_rel();
                    let mut ulax = self.user_lo_abs();
                    ulax[idx] = ulaxr[idx] + ax0[idx];
                    self.set_user_lo_abs(&ulax);
                }
            }
            "SSET" => {
                self.set = SetMode::Set;
            }
            "SUSE" => {
                self.set = SetMode::Use;
            }
            "SYNC" => {
                self.sync = 1;
            }
            "INIT" => {
                self.init = 1;
            }
            "ZERO" => {
                self.zero = 1;
            }
            "READ" => {
                self.read = 1;
            }
            "AUNIT" => {
                // Update angle unit label and conversion
                self.aegu = self.aunit.label().into();
                self.torad = self.aunit.torad();

                // Convert user limits if unit changed
                if self.curr_aunit != self.aunit {
                    let convert_fact = match self.curr_aunit {
                        AngleUnit::Degrees => 1.0e6 * D2R,
                        AngleUnit::Microradians => 1.0e-6 / D2R,
                    };
                    // Convert angle-related absolute/relative limits and offsets
                    let mut uhax = self.user_hi_abs();
                    let mut ulax = self.user_lo_abs();
                    let mut uhaxr = self.user_hi_rel();
                    let mut ulaxr = self.user_lo_rel();
                    let mut ax0 = self.user_offset();

                    for i in 0..3 {
                        uhax[i] *= convert_fact;
                        ulax[i] *= convert_fact;
                        uhaxr[i] *= convert_fact;
                        ulaxr[i] *= convert_fact;
                        ax0[i] *= convert_fact;
                    }

                    self.set_user_hi_abs(&uhax);
                    self.set_user_lo_abs(&ulax);
                    self.set_user_hi_rel(&uhaxr);
                    self.set_user_lo_rel(&ulaxr);
                    self.set_user_offset(&ax0);
                }

                self.sync = 1;
                self.curr_aunit = self.aunit;
            }
            _ => {}
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        let fields: &Vec<FieldDesc> = &ALL_FIELDS;
        unsafe { std::slice::from_raw_parts(fields.as_ptr(), fields.len()) }
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            self.vers = VERSION;
            self.val = 0.0;

            if self.aunit == AngleUnit::Degrees {
                self.aegu = "degrees".into();
                self.torad = D2R;
            } else {
                self.aegu = "ur".into();
                self.torad = 1.0e-6;
            }
            self.curr_aunit = self.aunit;

            // Initialize geometry
            self.do_init_geometry();

            // Init user and internal motor values to zero
            let ax0 = self.user_offset();
            let uhax = self.user_hi_abs();
            let ulax = self.user_lo_abs();
            let mut uhaxr = [0.0; 6];
            let mut ulaxr = [0.0; 6];
            for i in 0..6 {
                uhaxr[i] = uhax[i] - ax0[i];
                ulaxr[i] = ulax[i] - ax0[i];
            }

            self.set_user_drive(&[0.0; 6]);
            self.set_motor_drive(&[0.0; 6]);

            let mut axl = [0.0; 6];
            axl.copy_from_slice(&ax0);
            self.set_user_last(&axl);

            self.set_user_hi_rel(&uhaxr);
            self.set_user_lo_rel(&ulaxr);

            return Ok(());
        }

        // Pass 1: check links, read initial motor values
        self.check_links();

        // Read motors and set initial user-coordinate values
        let r = self.motor_readback();
        let u = self.do_motor_to_user(&r);
        self.set_user_drive(&u);
        self.set_user_readback(&u);

        // Read encoders
        let e = self.encoder_motor();
        let eu = self.do_motor_to_user(&e);
        self.set_encoder_user(&eu);

        // Propagate
        let ax0 = self.user_offset();
        let mut m0x = [0.0; 6];
        let mut axl = [0.0; 6];
        for i in 0..6 {
            m0x[i] = r[i];
            axl[i] = u[i] + ax0[i];
        }
        self.set_motor_drive(&m0x);
        self.set_user_last(&axl);

        // Calculate user limits
        self.do_calc_user_limits();

        Ok(())
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
#[allow(clippy::excessive_precision, clippy::needless_range_loop)]
mod tests {
    use super::*;

    /// Helper: create a table record with SRI geometry and typical dimensions.
    fn make_sri_table() -> TableRecord {
        let mut t = TableRecord::new();
        t.geom = Geometry::Sri;
        t.lx = 200.0;
        t.lz = 300.0;
        t.sx = 100.0;
        t.sy = 50.0;
        t.sz = 150.0;
        t.yang = 0.0;
        t.torad = D2R;
        t.aunit = AngleUnit::Degrees;
        // All motors connected
        for s in &mut t.lnk_stat {
            s.can_rw_drive = true;
            s.can_read_limits = true;
            s.can_read_position = true;
            s.can_rw_speed = true;
        }
        t.do_init_geometry();
        t
    }

    fn make_geocars_table() -> TableRecord {
        let mut t = make_sri_table();
        t.geom = Geometry::Geocars;
        t.do_init_geometry();
        t
    }

    fn make_pnc_table() -> TableRecord {
        let mut t = make_sri_table();
        t.geom = Geometry::Pnc;
        t.do_init_geometry();
        t
    }

    fn make_newport_table() -> TableRecord {
        let mut t = make_sri_table();
        t.geom = Geometry::Newport;
        t.do_init_geometry();
        t
    }

    #[test]
    fn test_rotation_matrix_identity() {
        let u = [0.0; 6];
        let a = make_rotation_matrix(D2R, &u);
        for i in 0..3 {
            for j in 0..3 {
                let expected = if i == j { 1.0 } else { 0.0 };
                assert!(
                    (a[i][j] - expected).abs() < 1e-12,
                    "a[{}][{}] = {}, expected {}",
                    i,
                    j,
                    a[i][j],
                    expected
                );
            }
        }
    }

    #[test]
    fn test_rotation_matrix_small_angle() {
        let ax = 1.0; // 1 degree
        let u = [ax, 0.0, 0.0, 0.0, 0.0, 0.0];
        let a = make_rotation_matrix(D2R, &u);
        // a[1][2] = sin(ax) * cos(0) = sin(1 deg)
        assert!((a[1][2] - (1.0f64).to_radians().sin()).abs() < 1e-12);
        // a[2][2] = cos(ax) * cos(0) = cos(1 deg)
        assert!((a[2][2] - (1.0f64).to_radians().cos()).abs() < 1e-12);
    }

    #[test]
    fn test_lab_to_local_roundtrip() {
        let yang = 30.0;
        let orig = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let local = lab_to_local(yang, &orig);
        let back = local_to_lab(yang, &local);
        for i in 0..6 {
            assert!(
                (back[i] - orig[i]).abs() < 1e-12,
                "component {} mismatch: {} vs {}",
                i,
                back[i],
                orig[i]
            );
        }
    }

    #[test]
    fn test_rot_y_preserves_y() {
        let v = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let out = rot_y(&v, 0.5);
        assert!((out[Y_6] - v[Y_6]).abs() < 1e-12);
        assert!((out[AY_6] - v[AY_6]).abs() < 1e-12);
    }

    #[test]
    fn test_polint_linear() {
        // Interpolate y = 2x + 1
        let xa = [0.0, 1.0, 2.0, 3.0];
        let ya = [1.0, 3.0, 5.0, 7.0];
        let (y, _dy) = polint(&xa, &ya, 1.5).unwrap();
        assert!((y - 4.0).abs() < 1e-10, "polint linear: got {}", y);
    }

    #[test]
    fn test_polint_quadratic() {
        // Interpolate y = x^2
        let xa = [0.0, 1.0, 2.0, 3.0, 4.0];
        let ya = [0.0, 1.0, 4.0, 9.0, 16.0];
        let (y, _dy) = polint(&xa, &ya, 2.5).unwrap();
        assert!((y - 6.25).abs() < 1e-10, "polint quadratic: got {}", y);
    }

    #[test]
    fn test_sri_zero_motors_give_zero_user() {
        let mut t = make_sri_table();
        let m = [0.0; 6];
        let u = t.do_motor_to_user(&m);
        for i in 0..6 {
            assert!(u[i].abs() < 1e-10, "SRI zero motors: u[{}] = {}", i, u[i]);
        }
    }

    #[test]
    fn test_geocars_zero_motors_give_zero_user() {
        let mut t = make_geocars_table();
        let m = [0.0; 6];
        let u = t.do_motor_to_user(&m);
        for i in 0..6 {
            assert!(
                u[i].abs() < 1e-10,
                "GEOCARS zero motors: u[{}] = {}",
                i,
                u[i]
            );
        }
    }

    #[test]
    fn test_pnc_zero_motors_give_zero_user() {
        let mut t = make_pnc_table();
        let m = [0.0; 6];
        let u = t.do_motor_to_user(&m);
        for i in 0..6 {
            assert!(u[i].abs() < 1e-10, "PNC zero motors: u[{}] = {}", i, u[i]);
        }
    }

    #[test]
    fn test_newport_zero_motors_give_zero_user() {
        let mut t = make_newport_table();
        let m = [0.0; 6];
        let u = t.do_motor_to_user(&m);
        for i in 0..6 {
            assert!(
                u[i].abs() < 1e-10,
                "Newport zero motors: u[{}] = {}",
                i,
                u[i]
            );
        }
    }

    #[test]
    fn test_sri_user_to_motor_roundtrip() {
        let mut t = make_sri_table();
        let user_orig = [0.5, -0.3, 0.2, 1.0, -0.5, 0.8];
        let m = t.do_user_to_motor(&user_orig);
        let user_back = t.do_motor_to_user(&m);
        for i in 0..6 {
            assert!(
                (user_back[i] - user_orig[i]).abs() < 1e-6,
                "SRI roundtrip: u[{}] = {} vs {}",
                i,
                user_back[i],
                user_orig[i]
            );
        }
    }

    #[test]
    fn test_geocars_user_to_motor_roundtrip() {
        let mut t = make_geocars_table();
        let user_orig = [0.5, -0.3, 0.2, 1.0, -0.5, 0.8];
        let m = t.do_user_to_motor(&user_orig);
        let user_back = t.do_motor_to_user(&m);
        for i in 0..6 {
            assert!(
                (user_back[i] - user_orig[i]).abs() < 1e-6,
                "GEOCARS roundtrip: u[{}] = {} vs {}",
                i,
                user_back[i],
                user_orig[i]
            );
        }
    }

    #[test]
    fn test_pnc_user_to_motor_roundtrip() {
        let mut t = make_pnc_table();
        let user_orig = [0.5, -0.3, 0.2, 1.0, -0.5, 0.8];
        let m = t.do_user_to_motor(&user_orig);
        let user_back = t.do_motor_to_user(&m);
        for i in 0..6 {
            assert!(
                (user_back[i] - user_orig[i]).abs() < 1e-6,
                "PNC roundtrip: u[{}] = {} vs {}",
                i,
                user_back[i],
                user_orig[i]
            );
        }
    }

    #[test]
    fn test_newport_user_to_motor_roundtrip() {
        let mut t = make_newport_table();
        let user_orig = [0.5, -0.3, 0.2, 1.0, -0.5, 0.8];
        let m = t.do_user_to_motor(&user_orig);
        let user_back = t.do_motor_to_user(&m);
        for i in 0..6 {
            assert!(
                (user_back[i] - user_orig[i]).abs() < 1e-5,
                "Newport roundtrip: u[{}] = {} vs {}",
                i,
                user_back[i],
                user_orig[i]
            );
        }
    }

    #[test]
    fn test_sri_motor_to_user_roundtrip() {
        let mut t = make_sri_table();
        let motor_orig = [0.5, 0.3, -0.2, 0.1, -0.4, 0.6];
        let u = t.do_motor_to_user(&motor_orig);
        let motor_back = t.do_user_to_motor(&u);
        for i in 0..6 {
            assert!(
                (motor_back[i] - motor_orig[i]).abs() < 1e-6,
                "SRI motor roundtrip: m[{}] = {} vs {}",
                i,
                motor_back[i],
                motor_orig[i]
            );
        }
    }

    #[test]
    fn test_geocars_motor_to_user_roundtrip() {
        let mut t = make_geocars_table();
        let motor_orig = [0.5, 0.3, -0.2, 0.1, -0.4, 0.6];
        let u = t.do_motor_to_user(&motor_orig);
        let motor_back = t.do_user_to_motor(&u);
        for i in 0..6 {
            assert!(
                (motor_back[i] - motor_orig[i]).abs() < 1e-6,
                "GEOCARS motor roundtrip: m[{}] = {} vs {}",
                i,
                motor_back[i],
                motor_orig[i]
            );
        }
    }

    #[test]
    fn test_pnc_motor_to_user_roundtrip() {
        let mut t = make_pnc_table();
        let motor_orig = [0.5, 0.3, -0.2, 0.1, -0.4, 0.6];
        let u = t.do_motor_to_user(&motor_orig);
        let motor_back = t.do_user_to_motor(&u);
        for i in 0..6 {
            assert!(
                (motor_back[i] - motor_orig[i]).abs() < 1e-6,
                "PNC motor roundtrip: m[{}] = {} vs {}",
                i,
                motor_back[i],
                motor_orig[i]
            );
        }
    }

    #[test]
    fn test_newport_motor_to_user_roundtrip() {
        let mut t = make_newport_table();
        let motor_orig = [0.5, 0.3, -0.2, 0.1, -0.4, 0.6];
        let u = t.do_motor_to_user(&motor_orig);
        let motor_back = t.do_user_to_motor(&u);
        for i in 0..6 {
            assert!(
                (motor_back[i] - motor_orig[i]).abs() < 1e-4,
                "Newport motor roundtrip: m[{}] = {} vs {}",
                i,
                motor_back[i],
                motor_orig[i]
            );
        }
    }

    #[test]
    fn test_pure_translation_y() {
        let mut t = make_sri_table();
        let user = [0.0, 0.0, 0.0, 0.0, 2.0, 0.0]; // Y translation only
        let m = t.do_user_to_motor(&user);
        // All Y motors should be 2.0, X and Z motors should be 0
        assert!((m[M0Y] - 2.0).abs() < 1e-10, "M0Y = {}", m[M0Y]);
        assert!((m[M1Y] - 2.0).abs() < 1e-10, "M1Y = {}", m[M1Y]);
        assert!((m[M2Y] - 2.0).abs() < 1e-10, "M2Y = {}", m[M2Y]);
        assert!(m[M0X].abs() < 1e-10, "M0X = {}", m[M0X]);
        assert!(m[M2X].abs() < 1e-10, "M2X = {}", m[M2X]);
        assert!(m[M2Z].abs() < 1e-10, "M2Z = {}", m[M2Z]);
    }

    #[test]
    fn test_motor_limit_viol() {
        let m = [5.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let hm = [3.0, 10.0, 10.0, 10.0, 10.0, 10.0];
        let lm = [-3.0, -10.0, -10.0, -10.0, -10.0, -10.0];
        let mut lnk = [LinkStatus::default(); 6];
        for s in &mut lnk {
            s.can_rw_drive = true;
            s.can_read_limits = true;
        }
        assert!(motor_limit_viol(&m, &hm, &lm, &lnk));

        let m2 = [2.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        assert!(!motor_limit_viol(&m2, &hm, &lm, &lnk));
    }

    #[test]
    fn test_user_limit_viol() {
        let ax = [5.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let hl = [3.0, 10.0, 10.0, 10.0, 10.0, 10.0];
        let ll = [-3.0, -10.0, -10.0, -10.0, -10.0, -10.0];
        assert!(user_limit_viol(&ax, &hl, &ll));

        let ax2 = [2.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        assert!(!user_limit_viol(&ax2, &hl, &ll));
    }

    #[test]
    fn test_zero_table() {
        let mut ax = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let mut ax0 = [0.0; 6];
        let axl = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let uhax = [10.0; 6];
        let ulax = [-10.0; 6];
        let mut uhaxr = [0.0; 6];
        let mut ulaxr = [0.0; 6];

        zero_table(
            &mut ax, &mut ax0, &axl, &uhax, &ulax, &mut uhaxr, &mut ulaxr,
        );

        for i in 0..6 {
            assert!(ax[i].abs() < 1e-12);
            assert!((ax0[i] - axl[i]).abs() < 1e-12);
            assert!((uhaxr[i] - (uhax[i] - ax0[i])).abs() < 1e-12);
            assert!((ulaxr[i] - (ulax[i] - ax0[i])).abs() < 1e-12);
        }
    }

    #[test]
    fn test_yang_rotation() {
        let mut t = make_sri_table();
        t.yang = 45.0;
        let user = [0.0, 0.0, 0.0, 1.0, 0.0, 0.0]; // X translation
        let m = t.do_user_to_motor(&user);
        let u_back = t.do_motor_to_user(&m);
        for i in 0..6 {
            assert!(
                (u_back[i] - user[i]).abs() < 1e-6,
                "YANG roundtrip: u[{}] = {} vs {}",
                i,
                u_back[i],
                user[i]
            );
        }
    }

    #[test]
    fn test_init_geometry_produces_valid_inverse() {
        let t = make_sri_table();
        // b is the inverse of the matrix formed from pivot-point vectors.
        // Verify that b * (pp1-pp0, pp2-pp1, cross) gives identity-like results.
        let ppo0 = t.ppo0;
        let ppo1 = t.ppo1;
        let ppo2 = t.ppo2;

        let av = ppo1[X] - ppo0[X];
        let bv = ppo1[Y] - ppo0[Y];
        let cv = ppo1[Z] - ppo0[Z];
        let dv = ppo2[X] - ppo1[X];
        let ev = ppo2[Y] - ppo1[Y];
        let fv = ppo2[Z] - ppo1[Z];
        let gv = bv * fv - cv * ev;
        let hv = cv * dv - av * fv;
        let iv = av * ev - bv * dv;

        // b * [row0, row1, row2] should give identity
        let mat = [[av, bv, cv], [dv, ev, fv], [gv, hv, iv]];
        for i in 0..3 {
            for j in 0..3 {
                let mut val = 0.0;
                for k in 0..3 {
                    val += t.b[i][k] * mat[k][j];
                }
                let expected = if i == j { 1.0 } else { 0.0 };
                assert!(
                    (val - expected).abs() < 1e-10,
                    "b*M[{}][{}] = {}, expected {}",
                    i,
                    j,
                    val,
                    expected
                );
            }
        }
    }

    #[test]
    fn test_get_put_field() {
        let mut t = TableRecord::new();
        t.put_field("AX", EpicsValue::Double(1.5)).unwrap();
        assert_eq!(t.get_field("AX"), Some(EpicsValue::Double(1.5)));

        // Read-only field
        assert!(t.put_field("AXRB", EpicsValue::Double(1.0)).is_err());

        // put_field_internal bypasses read-only
        assert!(t.put_field_internal("R0X", EpicsValue::Double(3.0)).is_ok());
        assert_eq!(t.get_field("R0X"), Some(EpicsValue::Double(3.0)));

        // Field not found
        assert!(t.put_field("NOPE", EpicsValue::Double(0.0)).is_err());
        assert_eq!(t.get_field("NOPE"), None);
    }

    #[test]
    fn test_set_mode_updates_offsets() {
        let mut t = make_sri_table();
        // Simulate having moved: set AXL to something nonzero
        t.axl = 1.0;
        t.ax = 0.5; // current drive
        // Switch to SET mode
        t.set = SetMode::Set;
        // Process should update offsets
        let _ = t.process();
        // Offset should be AXL - AX = 1.0 - 0.5 = 0.5
        assert!((t.ax0 - 0.5).abs() < 1e-10, "ax0 = {}", t.ax0);
    }

    #[test]
    fn test_aunit_conversion_on_put() {
        let mut t = TableRecord::new();
        t.aunit = AngleUnit::Degrees;
        t.curr_aunit = AngleUnit::Degrees;
        t.uhax = 10.0; // 10 degrees
        t.ulax = -10.0;

        // Switch to microradians
        t.put_field("AUNIT", EpicsValue::Enum(1)).unwrap();
        t.on_put("AUNIT");

        let expected_factor = 1.0e6 * D2R;
        assert!(
            (t.uhax - 10.0 * expected_factor).abs() < 1e-3,
            "uhax after conversion: {}",
            t.uhax
        );
        assert!((t.torad - 1.0e-6).abs() < 1e-15);
        assert_eq!(t.aegu, "ur");
    }

    #[test]
    fn test_field_list_completeness() {
        let t = TableRecord::new();
        let fields = t.field_list();
        // Should have a substantial number of fields
        assert!(fields.len() > 100, "field count = {}", fields.len());

        // Spot-check some fields exist
        let names: Vec<&str> = fields.iter().map(|f| f.name).collect();
        assert!(names.contains(&"AX"));
        assert!(names.contains(&"M2ZL"));
        assert!(names.contains(&"GEOM"));
        assert!(names.contains(&"AUNIT"));
        assert!(names.contains(&"VERS"));
    }

    #[test]
    fn test_user_limits_local_to_lab_identity() {
        // With yang=0, local and lab are the same
        let mut hu = [10.0, 20.0, 30.0, 40.0, 50.0, 60.0];
        let mut lu = [-10.0, -20.0, -30.0, -40.0, -50.0, -60.0];
        let hu_save = hu;
        let lu_save = lu;
        user_limits_local_to_lab(0.0, &mut hu, &mut lu);
        for i in 0..6 {
            assert!((hu[i] - hu_save[i]).abs() < 1e-10);
            assert!((lu[i] - lu_save[i]).abs() < 1e-10);
        }
    }

    #[test]
    fn test_sort_trajectory() {
        let mut traj = vec![
            Trajectory {
                user: 0.0,
                motor: [0.0; 6],
                lvio: false,
            },
            Trajectory {
                user: 3.0,
                motor: [0.0; 6],
                lvio: false,
            },
            Trajectory {
                user: 1.0,
                motor: [0.0; 6],
                lvio: false,
            },
            Trajectory {
                user: 2.0,
                motor: [0.0; 6],
                lvio: false,
            },
        ];
        sort_trajectory(&mut traj);
        assert!((traj[0].user - 0.0).abs() < 1e-10);
        assert!((traj[1].user - 1.0).abs() < 1e-10);
        assert!((traj[2].user - 2.0).abs() < 1e-10);
        assert!((traj[3].user - 3.0).abs() < 1e-10);
    }

    #[test]
    fn test_record_type() {
        let t = TableRecord::new();
        assert_eq!(t.record_type(), "table");
    }

    #[test]
    fn test_microradians_roundtrip() {
        let mut t = make_sri_table();
        t.aunit = AngleUnit::Microradians;
        t.torad = 1.0e-6;
        // 1000 microradians ~ 0.057 degrees
        let user = [1000.0, -500.0, 200.0, 1.0, -0.5, 0.8];
        let m = t.do_user_to_motor(&user);
        let u_back = t.do_motor_to_user(&m);
        for i in 0..6 {
            let tol = if i < 3 { 1.0 } else { 1e-5 };
            assert!(
                (u_back[i] - user[i]).abs() < tol,
                "urad SRI roundtrip: u[{}] = {} vs {}",
                i,
                u_back[i],
                user[i]
            );
        }
    }

    // =======================================================================
    // Golden tests — Rust output compared against C tableRecord.c reference
    // =======================================================================
    //
    // Reference values generated by golden_gen.c compiled from the original
    // EPICS tableRecord.c implementation. See tests/golden_values.txt.
    //
    // All geometries use: lx=200, lz=300, sx=100, sy=50, sz=150, yang=0
    // Tolerance for motor comparison: 1e-10
    // Round-trip check: C round-trip must be close to original user (eps < 1e-6)

    /// Helper: assert motor values match C reference, with round-trip check.
    fn assert_golden(
        label: &str,
        t: &mut TableRecord,
        user: &[f64; 6],
        c_motor: &[f64; 6],
        c_roundtrip: &[f64; 6],
    ) {
        let m = t.do_user_to_motor(user);
        let eps = 1e-10;
        for i in 0..6 {
            assert!(
                (m[i] - c_motor[i]).abs() < eps,
                "{}: motor[{}] = {:.15e}, C = {:.15e}, diff = {:.2e}",
                label,
                i,
                m[i],
                c_motor[i],
                (m[i] - c_motor[i]).abs()
            );
        }
        // Verify round-trip: motor->user should recover original user coords
        // Only check where C round-trip itself is close to original (eps < 1e-6)
        let u_back = t.do_motor_to_user(&m);
        let rt_eps = 1e-6;
        for i in 0..6 {
            if (c_roundtrip[i] - user[i]).abs() < rt_eps {
                assert!(
                    (u_back[i] - user[i]).abs() < rt_eps,
                    "{}: roundtrip[{}] = {:.15e}, expected {:.15e}",
                    label,
                    i,
                    u_back[i],
                    user[i]
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // SRI geometry: 9 test cases
    // -----------------------------------------------------------------------

    #[test]
    fn golden_c_sri_zeros() {
        // SRI geometry, user=[0,0,0,0,0,0]
        let mut t = make_sri_table();
        let user = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let c_motor = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let c_rt = [
            0.000000000000000e+00,
            -8.945310041616140e-16,
            0.000000000000000e+00,
            -2.341876692568690e-15,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("SRI zeros", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_sri_ax2() {
        // SRI geometry, user=[2,0,0,0,0,0] — pure AX rotation
        let mut t = make_sri_table();
        let user = [2.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let c_motor = [
            0.000000000000000e+00,
            -5.204465856329932e+00,
            -5.204465856329932e+00,
            0.000000000000000e+00,
            5.265383154420363e+00,
            1.653598887989403e+00,
        ];
        let c_rt = [
            2.000000000000001e+00,
            -8.945310041616140e-16,
            0.000000000000000e+00,
            -2.341876692568690e-15,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("SRI AX=2", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_sri_ay1p5() {
        // SRI geometry, user=[0,1.5,0,0,0,0] — pure AY rotation
        let mut t = make_sri_table();
        let user = [0.0, 1.5, 0.0, 0.0, 0.0, 0.0];
        let c_motor = [
            3.892274743736692e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            -3.926542246180973e+00,
            0.000000000000000e+00,
            -5.140125366639836e-02,
        ];
        let c_rt = [
            0.000000000000000e+00,
            1.499999999999996e+00,
            0.000000000000000e+00,
            -1.110223024625157e-14,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("SRI AY=1.5", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_sri_az_neg1() {
        // SRI geometry, user=[0,0,-1,0,0,0] — pure AZ rotation
        let mut t = make_sri_table();
        let user = [0.0, 0.0, -1.0, 0.0, 0.0, 0.0];
        let c_motor = [
            8.573898375033053e-01,
            1.752855885908787e+00,
            -1.737625401547916e+00,
            8.726203218641756e-01,
            7.615242180435189e-03,
            0.000000000000000e+00,
        ];
        let c_rt = [
            1.372478793193544e-17,
            1.607318644072144e-15,
            -1.000000000000000e+00,
            4.107825191113079e-15,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("SRI AZ=-1", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_sri_x3() {
        // SRI geometry, user=[0,0,0,3,0,0] — pure X translation
        let mut t = make_sri_table();
        let user = [0.0, 0.0, 0.0, 3.0, 0.0, 0.0];
        let c_motor = [
            3.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            3.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            -8.945310041616140e-16,
            0.000000000000000e+00,
            2.999999999999998e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("SRI X=3", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_sri_y5() {
        // SRI geometry, user=[0,0,0,0,5,0] — pure Y translation
        let mut t = make_sri_table();
        let user = [0.0, 0.0, 0.0, 0.0, 5.0, 0.0];
        let c_motor = [
            0.000000000000000e+00,
            5.000000000000000e+00,
            5.000000000000000e+00,
            0.000000000000000e+00,
            5.000000000000000e+00,
            0.000000000000000e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            -8.945310041616140e-16,
            0.000000000000000e+00,
            -2.341876692568690e-15,
            5.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("SRI Y=5", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_sri_z7() {
        // SRI geometry, user=[0,0,0,0,0,7] — pure Z translation
        let mut t = make_sri_table();
        let user = [0.0, 0.0, 0.0, 0.0, 0.0, 7.0];
        let c_motor = [
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            7.000000000000000e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            -8.945310041616140e-16,
            0.000000000000000e+00,
            -2.341876692568690e-15,
            0.000000000000000e+00,
            7.000000000000000e+00,
        ];
        assert_golden("SRI Z=7", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_sri_combined() {
        // SRI geometry, user=[1,-0.5,0.3,2,1,-1] — combined motion
        let mut t = make_sri_table();
        let user = [1.0, -0.5, 0.3, 2.0, 1.0, -1.0];
        let c_motor = [
            4.240530355006058e-01,
            -2.148167220972560e+00,
            -1.070674555893262e+00,
            3.047192101658052e+00,
            3.626101682504157e+00,
            -1.536637935618046e-01,
        ];
        let c_rt = [
            1.000000000000001e+00,
            -5.000000000000027e-01,
            3.000000000000005e-01,
            1.999999999999993e+00,
            1.000000000000000e+00,
            -1.000000000000000e+00,
        ];
        assert_golden("SRI combined", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_sri_large_combined() {
        // SRI geometry, user=[3,2,-1,-2,3,1.5] — large combined motion
        let mut t = make_sri_table();
        let user = [3.0, 2.0, -1.0, -2.0, 3.0, 1.5];
        let c_motor = [
            4.030874746064924e+00,
            -2.842418394250295e+00,
            -6.693360167962638e+00,
            -6.362835760233637e+00,
            1.092333309663479e+01,
            3.849991028658991e+00,
        ];
        let c_rt = [
            3.000000000000000e+00,
            2.000000000000009e+00,
            -1.000000000000001e+00,
            -1.999999999999978e+00,
            3.000000000000000e+00,
            1.500000000000000e+00,
        ];
        assert_golden("SRI large combined", &mut t, &user, &c_motor, &c_rt);
    }

    // -----------------------------------------------------------------------
    // GEOCARS geometry: 9 test cases
    // -----------------------------------------------------------------------

    #[test]
    fn golden_c_geocars_zeros() {
        // GEOCARS geometry, user=[0,0,0,0,0,0]
        let mut t = make_geocars_table();
        let user = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let c_motor = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let c_rt = [
            0.000000000000000e+00,
            -0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("GEOCARS zeros", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_geocars_ax2() {
        // GEOCARS geometry, user=[2,0,0,0,0,0] — pure AX rotation
        let mut t = make_geocars_table();
        let user = [2.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let c_motor = [
            0.000000000000000e+00,
            3.045864904521522e-02,
            5.265383154420363e+00,
            0.000000000000000e+00,
            -5.204465856329932e+00,
            1.836350782260695e+00,
        ];
        let c_rt = [
            2.000000000000001e+00,
            -0.000000000000000e+00,
            -6.425168390553124e-17,
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("GEOCARS AX=2", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_geocars_ay1p5() {
        // GEOCARS geometry, user=[0,1.5,0,0,0,0] — pure AY rotation
        let mut t = make_geocars_table();
        let user = [0.0, 1.5, 0.0, 0.0, 0.0, 0.0];
        let c_motor = [
            3.426750244427978e-02,
            0.000000000000000e+00,
            0.000000000000000e+00,
            3.892274743736692e+00,
            0.000000000000000e+00,
            2.669096084453713e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            1.499999999999989e+00,
            0.000000000000000e+00,
            2.842170943040401e-14,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("GEOCARS AY=1.5", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_geocars_az_neg1() {
        // GEOCARS geometry, user=[0,0,-1,0,0,0] — pure AZ rotation
        let mut t = make_geocars_table();
        let user = [0.0, 0.0, -1.0, 0.0, 0.0, 0.0];
        let c_motor = [
            8.878508062250461e-01,
            -1.737625401547916e+00,
            1.752855885908787e+00,
            8.573898375033053e-01,
            1.752855885908787e+00,
            0.000000000000000e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            2.714073328182201e-15,
            -1.000000000000000e+00,
            -1.421085471520200e-14,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("GEOCARS AZ=-1", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_geocars_x3() {
        // GEOCARS geometry, user=[0,0,0,3,0,0] — pure X translation
        let mut t = make_geocars_table();
        let user = [0.0, 0.0, 0.0, 3.0, 0.0, 0.0];
        let c_motor = [
            3.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            3.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            -0.000000000000000e+00,
            0.000000000000000e+00,
            3.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("GEOCARS X=3", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_geocars_y5() {
        // GEOCARS geometry, user=[0,0,0,0,5,0] — pure Y translation
        let mut t = make_geocars_table();
        let user = [0.0, 0.0, 0.0, 0.0, 5.0, 0.0];
        let c_motor = [
            0.000000000000000e+00,
            5.000000000000000e+00,
            5.000000000000000e+00,
            0.000000000000000e+00,
            5.000000000000000e+00,
            0.000000000000000e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            -0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            5.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("GEOCARS Y=5", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_geocars_z7() {
        // GEOCARS geometry, user=[0,0,0,0,0,7] — pure Z translation
        let mut t = make_geocars_table();
        let user = [0.0, 0.0, 0.0, 0.0, 0.0, 7.0];
        let c_motor = [
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            7.000000000000000e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            -0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            7.000000000000000e+00,
        ];
        assert_golden("GEOCARS Z=7", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_geocars_combined() {
        // GEOCARS geometry, user=[1,-0.5,0.3,2,1,-1] — combined motion
        let mut t = make_geocars_table();
        let user = [1.0, -0.5, 0.3, 2.0, 1.0, -1.0];
        let c_motor = [
            1.743390193547228e+00,
            1.547086729575270e+00,
            3.087355349964504e+00,
            4.240530355006058e-01,
            -2.148167220972560e+00,
            -9.599216628112970e-01,
        ];
        let c_rt = [
            1.000000000000000e+00,
            -4.999999999999927e-01,
            3.000000000000004e-01,
            1.999999999999972e+00,
            1.000000000000000e+00,
            -1.000000000000000e+00,
        ];
        assert_golden("GEOCARS combined", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_geocars_large_combined() {
        // GEOCARS geometry, user=[3,2,-1,-2,3,1.5] — large combined motion
        let mut t = make_geocars_table();
        let user = [3.0, 2.0, -1.0, -2.0, 3.0, 1.5];
        let c_motor = [
            -1.051772750406755e+00,
            1.152251020907990e+00,
            1.284880398349096e+01,
            4.030874746064924e+00,
            -2.842418394250295e+00,
            7.836929211206836e+00,
        ];
        let c_rt = [
            3.000000000000000e+00,
            2.000000000000009e+00,
            -1.000000000000000e+00,
            -2.000000000000028e+00,
            3.000000000000000e+00,
            1.499999999999972e+00,
        ];
        assert_golden("GEOCARS large combined", &mut t, &user, &c_motor, &c_rt);
    }

    // -----------------------------------------------------------------------
    // NEWPORT geometry: 9 test cases
    // -----------------------------------------------------------------------

    #[test]
    fn golden_c_newport_zeros() {
        // NEWPORT geometry, user=[0,0,0,0,0,0]
        let mut t = make_newport_table();
        let user = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let c_motor = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let c_rt = [
            0.000000000000000e+00,
            -0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("NEWPORT zeros", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_newport_ax2() {
        // NEWPORT geometry, user=[2,0,0,0,0,0] — pure AX rotation
        let mut t = make_newport_table();
        let user = [2.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let c_motor = [
            0.000000000000000e+00,
            -5.207638208821071e+00,
            3.047721494109055e-02,
            0.000000000000000e+00,
            5.268592638703252e+00,
            1.837470119410648e+00,
        ];
        let c_rt = [
            2.000000000000001e+00,
            -1.201757375809365e-17,
            3.441382859828896e-16,
            3.164499123031242e-17,
            3.387867764104158e-15,
            0.000000000000000e+00,
        ];
        assert_golden("NEWPORT AX=2", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_newport_ay1p5() {
        // NEWPORT geometry, user=[0,1.5,0,0,0,0] — pure AY rotation
        let mut t = make_newport_table();
        let user = [0.0, 1.5, 0.0, 0.0, 0.0, 0.0];
        let c_motor = [
            3.892274743736692e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            -3.960809748625252e+00,
            0.000000000000000e+00,
            2.566293577120916e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            1.500000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("NEWPORT AY=1.5", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_newport_az_neg1() {
        // NEWPORT geometry, user=[0,0,-1,0,0,0] — pure AZ rotation
        let mut t = make_newport_table();
        let user = [0.0, 0.0, -1.0, 0.0, 0.0, 0.0];
        let c_motor = [
            8.879860508016516e-01,
            1.753122895017139e+00,
            -1.737890090626378e+00,
            8.879860508016516e-01,
            1.753122895017139e+00,
            0.000000000000000e+00,
        ];
        let c_rt = [
            1.373999622392239e-16,
            -0.000000000000000e+00,
            -9.999999999999990e-01,
            2.220446049250313e-16,
            -8.932380760976835e-15,
            -4.203491680913012e-18,
        ];
        assert_golden("NEWPORT AZ=-1", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_newport_x3() {
        // NEWPORT geometry, user=[0,0,0,3,0,0] — pure X translation
        let mut t = make_newport_table();
        let user = [0.0, 0.0, 0.0, 3.0, 0.0, 0.0];
        let c_motor = [
            3.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            3.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            -0.000000000000000e+00,
            0.000000000000000e+00,
            3.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("NEWPORT X=3", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_newport_y5() {
        // NEWPORT geometry, user=[0,0,0,0,5,0] — pure Y translation
        let mut t = make_newport_table();
        let user = [0.0, 0.0, 0.0, 0.0, 5.0, 0.0];
        let c_motor = [
            0.000000000000000e+00,
            5.000000000000000e+00,
            5.000000000000000e+00,
            0.000000000000000e+00,
            5.000000000000000e+00,
            0.000000000000000e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            -0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            5.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("NEWPORT Y=5", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_newport_z7() {
        // NEWPORT geometry, user=[0,0,0,0,0,7] — pure Z translation
        let mut t = make_newport_table();
        let user = [0.0, 0.0, 0.0, 0.0, 0.0, 7.0];
        let c_motor = [
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            7.000000000000000e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            -0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            7.000000000000000e+00,
        ];
        assert_golden("NEWPORT Z=7", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_newport_combined() {
        // NEWPORT geometry, user=[1,-0.5,0.3,2,1,-1] — combined motion
        let mut t = make_newport_table();
        let user = [1.0, -0.5, 0.3, 2.0, 1.0, -1.0];
        let c_motor = [
            4.353022095460802e-01,
            -2.148525612105483e+00,
            1.547344838981432e+00,
            3.025846322745595e+00,
            3.087870431272331e+00,
            -9.630033568368596e-01,
        ];
        let c_rt = [
            1.000000000000000e+00,
            -5.000000000000008e-01,
            3.000000000000010e-01,
            2.000000000000000e+00,
            1.000000000000003e+00,
            -1.000000000000000e+00,
        ];
        assert_golden("NEWPORT combined", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_newport_large_combined() {
        // NEWPORT geometry, user=[3,2,-1,-2,3,1.5] — large combined motion
        let mut t = make_newport_table();
        let user = [3.0, 2.0, -1.0, -2.0, 3.0, 1.5];
        let c_motor = [
            3.981220740171423e+00,
            -2.846843636621184e+00,
            1.154044912352661e+00,
            -6.214519416654817e+00,
            1.286880774926943e+01,
            7.924514374289184e+00,
        ];
        let c_rt = [
            3.000000000000000e+00,
            2.000000000000001e+00,
            -1.000000000000001e+00,
            -2.000000000000000e+00,
            3.000000000000000e+00,
            1.500000000000000e+00,
        ];
        assert_golden("NEWPORT large combined", &mut t, &user, &c_motor, &c_rt);
    }

    // -----------------------------------------------------------------------
    // PNC geometry: 9 test cases
    // -----------------------------------------------------------------------

    #[test]
    fn golden_c_pnc_zeros() {
        // PNC geometry, user=[0,0,0,0,0,0]
        let mut t = make_pnc_table();
        let user = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let c_motor = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let c_rt = [
            0.000000000000000e+00,
            8.945310041616140e-16,
            0.000000000000000e+00,
            2.341876692568690e-15,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("PNC zeros", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_pnc_ax2() {
        // PNC geometry, user=[2,0,0,0,0,0] — pure AX rotation
        let mut t = make_pnc_table();
        let user = [2.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let c_motor = [
            0.000000000000000e+00,
            -5.204465856329932e+00,
            -5.204465856329932e+00,
            0.000000000000000e+00,
            5.265383154420363e+00,
            1.653598887989403e+00,
        ];
        let c_rt = [
            2.000000000000001e+00,
            8.945310041616140e-16,
            0.000000000000000e+00,
            2.341876692568690e-15,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("PNC AX=2", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_pnc_ay1p5() {
        // PNC geometry, user=[0,1.5,0,0,0,0] — pure AY rotation
        let mut t = make_pnc_table();
        let user = [0.0, 1.5, 0.0, 0.0, 0.0, 0.0];
        let c_motor = [
            3.960809748625252e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            -3.926542246180973e+00,
            0.000000000000000e+00,
            -5.140125366639836e-02,
        ];
        let c_rt = [
            0.000000000000000e+00,
            1.499999999999992e+00,
            0.000000000000000e+00,
            -2.131628207280301e-14,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("PNC AY=1.5", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_pnc_az_neg1() {
        // PNC geometry, user=[0,0,-1,0,0,0] — pure AZ rotation
        let mut t = make_pnc_table();
        let user = [0.0, 0.0, -1.0, 0.0, 0.0, 0.0];
        let c_motor = [
            8.878508062250461e-01,
            -1.737625401547916e+00,
            1.752855885908787e+00,
            8.726203218641756e-01,
            7.615242180435189e-03,
            0.000000000000000e+00,
        ];
        let c_rt = [
            -1.372478793193544e-17,
            -1.607318644072144e-15,
            -1.000000000000000e+00,
            -4.329869796038111e-15,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("PNC AZ=-1", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_pnc_x3() {
        // PNC geometry, user=[0,0,0,3,0,0] — pure X translation
        let mut t = make_pnc_table();
        let user = [0.0, 0.0, 0.0, 3.0, 0.0, 0.0];
        let c_motor = [
            3.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            3.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            8.945310041616140e-16,
            0.000000000000000e+00,
            3.000000000000002e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("PNC X=3", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_pnc_y5() {
        // PNC geometry, user=[0,0,0,0,5,0] — pure Y translation
        let mut t = make_pnc_table();
        let user = [0.0, 0.0, 0.0, 0.0, 5.0, 0.0];
        let c_motor = [
            0.000000000000000e+00,
            5.000000000000000e+00,
            5.000000000000000e+00,
            0.000000000000000e+00,
            5.000000000000000e+00,
            0.000000000000000e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            8.945310041616140e-16,
            0.000000000000000e+00,
            2.341876692568690e-15,
            5.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("PNC Y=5", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_pnc_z7() {
        // PNC geometry, user=[0,0,0,0,0,7] — pure Z translation
        let mut t = make_pnc_table();
        let user = [0.0, 0.0, 0.0, 0.0, 0.0, 7.0];
        let c_motor = [
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            7.000000000000000e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            8.945310041616140e-16,
            0.000000000000000e+00,
            2.341876692568690e-15,
            0.000000000000000e+00,
            7.000000000000000e+00,
        ];
        assert_golden("PNC Z=7", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_pnc_combined() {
        // PNC geometry, user=[1,-0.5,0.3,2,1,-1] — combined motion
        let mut t = make_pnc_table();
        let user = [1.0, -0.5, 0.3, 2.0, 1.0, -1.0];
        let c_motor = [
            4.344098687911355e-01,
            -1.070674555893262e+00,
            -2.148167220972560e+00,
            3.047192101658052e+00,
            3.626101682504157e+00,
            -1.536637935618046e-01,
        ];
        let c_rt = [
            1.000000000000001e+00,
            -5.000000000000121e-01,
            3.000000000000005e-01,
            1.999999999999969e+00,
            1.000000000000000e+00,
            -1.000000000000000e+00,
        ];
        assert_golden("PNC combined", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_pnc_large_combined() {
        // PNC geometry, user=[3,2,-1,-2,3,1.5] — large combined motion
        let mut t = make_pnc_table();
        let user = [3.0, 2.0, -1.0, -2.0, 3.0, 1.5];
        let c_motor = [
            4.183151754968392e+00,
            -6.693360167962638e+00,
            -2.842418394250295e+00,
            -6.362835760233637e+00,
            1.092333309663479e+01,
            3.849991028658991e+00,
        ];
        let c_rt = [
            3.000000000000000e+00,
            2.000000000000004e+00,
            -1.000000000000001e+00,
            -1.999999999999990e+00,
            3.000000000000000e+00,
            1.500000000000000e+00,
        ];
        assert_golden("PNC large combined", &mut t, &user, &c_motor, &c_rt);
    }

    // -----------------------------------------------------------------------
    // SRI with YANG=30: 9 test cases
    // -----------------------------------------------------------------------

    #[test]
    fn golden_c_yang30_zeros() {
        // SRI with YANG=30, user=[0,0,0,0,0,0]
        let mut t = make_sri_table();
        t.yang = 30.0;
        let user = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let c_motor = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let c_rt = [
            0.000000000000000e+00,
            -8.945310041616140e-16,
            0.000000000000000e+00,
            -2.028124708295165e-15,
            0.000000000000000e+00,
            -1.170938346284345e-15,
        ];
        assert_golden("YANG30 zeros", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_yang30_ax2() {
        // SRI with YANG=30, user=[2,0,0,0,0,0] — pure AX rotation
        let mut t = make_sri_table();
        t.yang = 30.0;
        let user = [2.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let c_motor = [
            8.573898375033053e-01,
            -2.758908210602897e+00,
            -6.247794727408483e+00,
            8.726203218641754e-01,
            4.564264128072779e+00,
            1.442505392476704e+00,
        ];
        let c_rt = [
            1.999999999999999e+00,
            4.787873325535661e-15,
            -1.761482671256913e-15,
            1.009555410351371e-14,
            0.000000000000000e+00,
            5.828670879282071e-15,
        ];
        assert_golden("YANG30 AX=2", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_yang30_ay1p5() {
        // SRI with YANG=30, user=[0,1.5,0,0,0,0] — pure AY rotation
        let mut t = make_sri_table();
        t.yang = 30.0;
        let user = [0.0, 1.5, 0.0, 0.0, 0.0, 0.0];
        let c_motor = [
            3.892274743736692e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            -3.926542246180973e+00,
            0.000000000000000e+00,
            -5.140125366639836e-02,
        ];
        let c_rt = [
            0.000000000000000e+00,
            1.499999999999996e+00,
            0.000000000000000e+00,
            -9.614813431917821e-15,
            0.000000000000000e+00,
            -5.551115123125782e-15,
        ];
        assert_golden("YANG30 AY=1.5", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_yang30_az_neg1() {
        // SRI with YANG=30, user=[0,0,-1,0,0,0] — pure AZ rotation
        let mut t = make_sri_table();
        t.yang = 30.0;
        let user = [0.0, 0.0, -1.0, 0.0, 0.0, 0.0];
        let c_motor = [
            7.442980228465785e-01,
            2.827979788095959e+00,
            -1.947889441883532e-01,
            7.557209586095223e-01,
            -1.301365227558378e+00,
            -4.419884739657789e-01,
        ];
        let c_rt = [
            6.810079829184314e-16,
            -5.644299355257259e-15,
            -1.000000000000002e+00,
            -1.403762761060002e-14,
            0.000000000000000e+00,
            -8.104628079763641e-15,
        ];
        assert_golden("YANG30 AZ=-1", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_yang30_x3() {
        // SRI with YANG=30, user=[0,0,0,3,0,0] — pure X translation
        let mut t = make_sri_table();
        t.yang = 30.0;
        let user = [0.0, 0.0, 0.0, 3.0, 0.0, 0.0];
        let c_motor = [
            2.598076211353316e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            2.598076211353316e+00,
            0.000000000000000e+00,
            -1.500000000000000e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            -8.945310041616140e-16,
            0.000000000000000e+00,
            2.999999999999998e+00,
            0.000000000000000e+00,
            -1.254445226103924e-15,
        ];
        assert_golden("YANG30 X=3", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_yang30_y5() {
        // SRI with YANG=30, user=[0,0,0,0,5,0] — pure Y translation
        let mut t = make_sri_table();
        t.yang = 30.0;
        let user = [0.0, 0.0, 0.0, 0.0, 5.0, 0.0];
        let c_motor = [
            0.000000000000000e+00,
            5.000000000000000e+00,
            5.000000000000000e+00,
            0.000000000000000e+00,
            5.000000000000000e+00,
            0.000000000000000e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            -8.945310041616140e-16,
            0.000000000000000e+00,
            -2.028124708295165e-15,
            5.000000000000000e+00,
            -1.170938346284345e-15,
        ];
        assert_golden("YANG30 Y=5", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_yang30_z7() {
        // SRI with YANG=30, user=[0,0,0,0,0,7] — pure Z translation
        let mut t = make_sri_table();
        t.yang = 30.0;
        let user = [0.0, 0.0, 0.0, 0.0, 0.0, 7.0];
        let c_motor = [
            3.500000000000000e+00,
            0.000000000000000e+00,
            0.000000000000000e+00,
            3.500000000000000e+00,
            0.000000000000000e+00,
            6.062177826491080e+00,
        ];
        let c_rt = [
            0.000000000000000e+00,
            2.752505030583218e-15,
            0.000000000000000e+00,
            1.605551716806355e-15,
            0.000000000000000e+00,
            7.000000000000012e+00,
        ];
        assert_golden("YANG30 Z=7", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_yang30_combined() {
        // SRI with YANG=30, user=[1,-0.5,0.3,2,1,-1] — combined motion
        let mut t = make_sri_table();
        t.yang = 30.0;
        let user = [1.0, -0.5, 0.3, 2.0, 1.0, -1.0];
        let c_motor = [
            1.279829161379098e-01,
            -1.247765652272335e+00,
            -2.055113327510050e+00,
            2.750629929667285e+00,
            3.667975718941413e+00,
            -1.010553527862072e+00,
        ];
        let c_rt = [
            9.999999999999991e-01,
            -5.000000000000132e-01,
            2.999999999999990e-01,
            1.999999999999974e+00,
            1.000000000000000e+00,
            -1.000000000000026e+00,
        ];
        assert_golden("YANG30 combined", &mut t, &user, &c_motor, &c_rt);
    }

    #[test]
    fn golden_c_yang30_large_combined() {
        // SRI with YANG=30, user=[3,2,-1,-2,3,1.5] — large combined motion
        let mut t = make_sri_table();
        t.yang = 30.0;
        let user = [3.0, 2.0, -1.0, -2.0, 3.0, 1.5];
        let c_motor = [
            6.169659339879800e+00,
            1.843786315304051e+00,
            -6.662635570021543e+00,
            -4.154072844876737e+00,
            8.566931234521761e+00,
            4.008104005764238e+00,
        ];
        let c_rt = [
            3.000000000000000e+00,
            1.999999999999986e+00,
            -9.999999999999988e-01,
            -2.000000000000029e+00,
            3.000000000000000e+00,
            1.499999999999983e+00,
        ];
        assert_golden("YANG30 large combined", &mut t, &user, &c_motor, &c_rt);
    }

    // -----------------------------------------------------------------------
    // SRI with offset ax0=5: 1 test case
    // -----------------------------------------------------------------------

    #[test]
    fn golden_c_offset_ax0_5() {
        // SRI with ax0=[5,0,0,0,0,0], user=[0,0,0,0,0,0]
        // The offset makes user=0 correspond to a real angle of 5 degrees
        let mut t = make_sri_table();
        t.ax0 = 5.0;
        let user = [0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let c_motor = [
            0.000000000000000e+00,
            -1.288309631673601e+01,
            -1.288309631673601e+01,
            0.000000000000000e+00,
            1.326362650756145e+01,
            3.786991851144734e+00,
        ];
        let c_rt = [
            1.776356839400250e-15,
            -8.655084426932594e-15,
            0.000000000000000e+00,
            -2.265895804320905e-14,
            0.000000000000000e+00,
            0.000000000000000e+00,
        ];
        assert_golden("SRI offset ax0=5", &mut t, &user, &c_motor, &c_rt);
    }
}
