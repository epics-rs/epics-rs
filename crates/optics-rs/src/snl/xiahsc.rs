//! XIA HSC-1 High-Speed Slit Controller — native Rust port of `xiahsc.st`.
//!
//! Implements the serial command protocol and state machine for the XIA HSC-1
//! slit controller. Each HSC-1 module controls two stepper motors (A and B)
//! for a single blade pair (horizontal or vertical).
//!
//! # Serial Protocol
//!
//! Commands are sent as `!<ID> <CMD>\r` and responses come back as
//! `%<ID> <DATA>\r\n`. The `<ID>` is the module identifier, e.g. `H-1234`.
//!
//! # Coordinate System
//!
//! The controller works in raw motor steps. Dial (mm) coordinates use:
//! - `dial = (raw - origin) / STEPS_PER_MM`
//! - `raw  = dial * STEPS_PER_MM + 0.5 + origin`
//!
//! Gap and center are derived:
//! - `width  = left + right`
//! - `center = (right - left) / 2`
//! - `height = top + bottom`
//! - `v_center = (top - bottom) / 2`

use std::fmt;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Motor steps per millimeter for the HSC-1.
pub const STEPS_PER_MM: f64 = 400.0;

/// Default origin position in raw steps.
pub const DEFAULT_ORIGIN: i32 = 400;

/// Default outer motion limit in raw steps.
pub const DEFAULT_OUTER_LIMIT: i32 = 4400;

/// Polling interval while motors are moving.
pub const MOVING_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Polling interval when idle.
pub const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Maximum time to wait for a move to complete.
pub const MOVE_TIMEOUT: Duration = Duration::from_secs(300);

/// Small hesitation before starting a move to coalesce requests.
pub const MOVE_HESITATION: Duration = Duration::from_millis(100);

/// Serial response timeout.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_millis(250);

/// Reconnect interval after communication error.
pub const ERROR_RECONNECT_INTERVAL: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------------------
// Error codes (matching the 14 HSC error codes, shifted to 1-14)
// ---------------------------------------------------------------------------

/// Error code: no error.
pub const NO_ERROR: i32 = 0;
/// Error code: soft limits exceeded.
pub const ERROR_SOFT_LIMITS: i32 = 15;
/// Error code: unknown error.
pub const ERROR_UNKNOWN: i32 = 16;
/// Error code: bad module ID.
pub const ERROR_BAD_ID: i32 = 17;
/// Error code: communication error.
pub const ERROR_COMM_ERROR: i32 = 18;

/// HSC error messages, indexed 0..13 (device error codes 0-13).
pub const HSC_ERROR_MESSAGES: [&str; 14] = [
    "Missing Command",
    "Unrecognized Command",
    "Input Buffer Overflow",
    "No new Alias Given",
    "Alias too long",
    "Invalid Field Parameter",
    "Value Out of Range",
    "Parameter is read-only",
    "Invalid/Missing Argument",
    "No Movement Required",
    "Uncalibrated: no motion allowed",
    "Motion out of range",
    "Invalid/missing direction character",
    "Invalid Motor Specified",
];

// ---------------------------------------------------------------------------
// HSC1 Control/Status Word bits
// ---------------------------------------------------------------------------

/// Power level mask (bits 0-1): 0=lo, 1=med, 2=hi.
pub const CSW_PWRLVL: i32 = 0x03;
/// Limits enabled bit.
pub const CSW_LIMITS: i32 = 0x04;
/// Print intro banner bit.
pub const CSW_BANNER: i32 = 0x08;
/// Command echo bit.
pub const CSW_ECHO: i32 = 0x10;
/// Lock buttons bit.
pub const CSW_LOCK: i32 = 0x20;
/// Use alias as ID bit.
pub const CSW_ALIAS: i32 = 0x40;
/// Print error text bit.
pub const CSW_TEXT: i32 = 0x80;

// ---------------------------------------------------------------------------
// Blade orientation
// ---------------------------------------------------------------------------

/// Horizontal slit orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HOrient {
    /// Motor A = left blade, Motor B = right blade.
    #[default]
    LeftRight,
    /// Motor A = right blade, Motor B = left blade.
    RightLeft,
}

/// Vertical slit orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VOrient {
    /// Motor A = top blade, Motor B = bottom blade.
    #[default]
    TopBottom,
    /// Motor A = bottom blade, Motor B = top blade.
    BottomTop,
}

// ---------------------------------------------------------------------------
// Serial Protocol — Commands
// ---------------------------------------------------------------------------

/// HSC-1 serial commands.
#[derive(Debug, Clone, PartialEq)]
pub enum HscCommand {
    /// Kill all movement: `!ALL K`
    KillAll,
    /// Kill movement for one module: `!<ID> K`
    Kill(String),
    /// Position inquiry: `!<ID> P`
    PositionInquiry(String),
    /// Module inquiry: `!<ID> I`
    ModuleInquiry(String),
    /// Move to absolute positions: `!<ID> M <posA> <posB>`
    Move(String, i32, i32),
    /// Calibrate immediate (zero positions): `!ALL 0 I`
    CalibrateImmediate,
    /// Read memory register: `!<ID> R <reg>`
    ReadRegister(String, u8),
    /// Write memory register: `!<ID> W <reg> <value>`
    WriteRegister(String, u8, i32),
}

/// HSC-1 memory registers.
pub mod register {
    /// Outer motion limit (steps).
    pub const OUTER_MOTION_LIMIT: u8 = 1;
    /// Origin position (steps).
    pub const ORIGIN_POSITION: u8 = 2;
    /// Motor A position (read-only).
    pub const MOTOR_A_POSITION: u8 = 3;
    /// Motor B position (read-only).
    pub const MOTOR_B_POSITION: u8 = 4;
    /// Motor step delay.
    pub const MOTOR_STEP_DELAY: u8 = 5;
    /// Gear backlash.
    pub const GEAR_BACKLASH: u8 = 6;
    /// Control word.
    pub const CONTROL_WORD: u8 = 7;
}

impl HscCommand {
    /// Format the command as a serial string (without trailing `\r`).
    pub fn to_serial(&self) -> String {
        match self {
            HscCommand::KillAll => "!ALL K".to_string(),
            HscCommand::Kill(id) => format!("!{id} K"),
            HscCommand::PositionInquiry(id) => format!("!{id} P"),
            HscCommand::ModuleInquiry(id) => format!("!{id} I"),
            HscCommand::Move(id, a, b) => format!("!{id} M {a} {b}"),
            HscCommand::CalibrateImmediate => "!ALL 0 I".to_string(),
            HscCommand::ReadRegister(id, reg) => format!("!{id} R {reg}"),
            HscCommand::WriteRegister(id, reg, val) => format!("!{id} W {reg} {val}"),
        }
    }

    /// Format the command as bytes ready to send (with trailing `\r`).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut s = self.to_serial();
        s.push('\r');
        s.into_bytes()
    }
}

impl fmt::Display for HscCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_serial())
    }
}

// ---------------------------------------------------------------------------
// Serial Protocol — Responses
// ---------------------------------------------------------------------------

/// Parsed HSC-1 response.
#[derive(Debug, Clone, PartialEq)]
pub enum HscResponse {
    /// Command acknowledged: `%<ID> OK;`
    Ok(String),
    /// Device is busy: `%<ID> BUSY;`
    Busy(String),
    /// Position report with DONE status: `%<ID> <posA> <posB> DONE;`
    Position { id: String, pos_a: i32, pos_b: i32 },
    /// Position report with OK status: `%<ID> OK <posA> <posB>`
    PositionOk { id: String, pos_a: i32, pos_b: i32 },
    /// Error report: `%<ID> ERROR; [<code>]`
    Error { id: String, code: Option<i32> },
    /// Register read response: `%<ID> R <value>`
    /// The actual format is `%<ID> R <reg> <value>` but typically
    /// the parsed form after skipping the first two words yields the value.
    RegisterValue { id: String, value: i32 },
    /// Module identity response.
    Identity { id: String, info: String },
    /// Unparseable response.
    Unknown(String),
}

impl HscResponse {
    /// Extract the module ID from the response, if present.
    pub fn id(&self) -> Option<&str> {
        match self {
            HscResponse::Ok(id)
            | HscResponse::Busy(id)
            | HscResponse::Position { id, .. }
            | HscResponse::PositionOk { id, .. }
            | HscResponse::Error { id, .. }
            | HscResponse::RegisterValue { id, .. }
            | HscResponse::Identity { id, .. } => Some(id),
            HscResponse::Unknown(_) => None,
        }
    }
}

/// Parse a raw HSC response line into a structured response.
///
/// HSC responses start with `%<ID>` followed by space-delimited fields.
/// Leading/trailing whitespace is stripped before parsing.
pub fn parse_response(line: &str) -> HscResponse {
    let line = line.trim();
    if line.is_empty() {
        return HscResponse::Unknown(String::new());
    }

    let words: Vec<&str> = line.split_whitespace().collect();
    if words.is_empty() {
        return HscResponse::Unknown(line.to_string());
    }

    // First word should start with '%'
    let id = if let Some(stripped) = words[0].strip_prefix('%') {
        stripped.to_string()
    } else {
        // Some responses may not have the '%' prefix
        return HscResponse::Unknown(line.to_string());
    };

    match words.len() {
        1 => HscResponse::Unknown(line.to_string()),
        2 => match words[1] {
            "OK;" => HscResponse::Ok(id),
            "BUSY;" => HscResponse::Busy(id),
            s if s.starts_with("ERROR;") => HscResponse::Error { id, code: None },
            _ => HscResponse::Unknown(line.to_string()),
        },
        3 => {
            if words[1] == "ERROR;" {
                let code = words[2].parse::<i32>().ok();
                HscResponse::Error { id, code }
            } else {
                // Could be register read: %<ID> <reg_word> <value>
                // Format from HSC: `%<ID> R <value>` after R command
                if let Ok(value) = words[2].parse::<i32>() {
                    HscResponse::RegisterValue { id, value }
                } else {
                    HscResponse::Unknown(line.to_string())
                }
            }
        }
        4 => {
            if words[3] == "DONE;" {
                // Position report: %<ID> <posA> <posB> DONE;
                if let (Ok(a), Ok(b)) = (words[1].parse::<i32>(), words[2].parse::<i32>()) {
                    HscResponse::Position {
                        id,
                        pos_a: a,
                        pos_b: b,
                    }
                } else {
                    HscResponse::Unknown(line.to_string())
                }
            } else {
                HscResponse::Unknown(line.to_string())
            }
        }
        5 => {
            if words[1] == "OK" {
                // Position with OK status: %<ID> OK <posA> <posB> DONE;
                if let (Ok(a), Ok(b)) = (words[2].parse::<i32>(), words[3].parse::<i32>()) {
                    HscResponse::PositionOk {
                        id,
                        pos_a: a,
                        pos_b: b,
                    }
                } else {
                    HscResponse::Unknown(line.to_string())
                }
            } else {
                HscResponse::Unknown(line.to_string())
            }
        }
        _ => HscResponse::Unknown(line.to_string()),
    }
}

/// Validate that a response line matches the expected command ID.
///
/// Compares characters of the command (after '!') with the response (after '%')
/// up to the first space. Returns `true` if the ID portion matches.
pub fn validate_response(command: &str, response: &str) -> bool {
    let response = response.trim();
    if response.is_empty() {
        return false;
    }

    // Extract command ID: skip '!' prefix, take up to first space
    let cmd_id = command
        .strip_prefix('!')
        .unwrap_or(command)
        .split_whitespace()
        .next()
        .unwrap_or("");

    // Extract response ID: skip '%' prefix, take up to first space
    let resp_id = response
        .strip_prefix('%')
        .unwrap_or(response)
        .split_whitespace()
        .next()
        .unwrap_or("");

    cmd_id == resp_id
}

// ---------------------------------------------------------------------------
// Coordinate conversion (pure functions)
// ---------------------------------------------------------------------------

/// Convert raw motor steps to dial (mm) position.
pub fn raw_to_dial(raw: i32, origin: i32) -> f64 {
    (raw as f64 - origin as f64) / STEPS_PER_MM
}

/// Convert dial (mm) position to raw motor steps.
pub fn dial_to_raw(dial: f64, origin: i32) -> i32 {
    (dial * STEPS_PER_MM + 0.5 + origin as f64) as i32
}

/// Validate an HSC module ID string.
///
/// Valid formats: `XIAHSC-C-NNNN`, `C-NNNN`, or `CNNNN`
/// where C is a letter and NNNN is a number.
pub fn validate_hsc_id(id: &str) -> bool {
    if id.is_empty() {
        return false;
    }
    // Try XIAHSC-C-NNNN
    if let Some(rest) = id.strip_prefix("XIAHSC-") {
        return parse_id_suffix(rest);
    }
    // Try C-NNNN
    parse_id_suffix(id)
}

/// Parse the `C-NNNN` or `CNNNN` portion of an HSC ID.
fn parse_id_suffix(s: &str) -> bool {
    let mut chars = s.chars();
    let first = match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => c,
        _ => return false,
    };
    let rest: String = chars.collect();
    // With hyphen: C-NNNN
    if let Some(num_str) = rest.strip_prefix('-') {
        return num_str.parse::<i32>().is_ok() && !num_str.is_empty();
    }
    // Without hyphen: CNNNN
    let _ = first; // already consumed
    rest.parse::<i32>().is_ok() && !rest.is_empty()
}

/// Decode the HSC control/status word into individual flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlStatusWord {
    /// Power level: 0=low, 1=medium, 2=high.
    pub power_level: i32,
    /// Limits enabled.
    pub limits: bool,
    /// Print intro banner.
    pub banner: bool,
    /// Command echo.
    pub echo: bool,
    /// Lock buttons.
    pub lock: bool,
    /// Use alias as ID.
    pub alias: bool,
    /// Print error text.
    pub text: bool,
}

impl ControlStatusWord {
    /// Decode a raw control/status word integer.
    pub fn from_raw(csw: i32) -> Self {
        Self {
            power_level: csw & CSW_PWRLVL,
            limits: (csw & CSW_LIMITS) != 0,
            banner: (csw & CSW_BANNER) != 0,
            echo: (csw & CSW_ECHO) != 0,
            lock: (csw & CSW_LOCK) != 0,
            alias: (csw & CSW_ALIAS) != 0,
            text: (csw & CSW_TEXT) != 0,
        }
    }

    /// Encode back to a raw integer.
    pub fn to_raw(&self) -> i32 {
        let mut v = self.power_level & CSW_PWRLVL;
        if self.limits {
            v |= CSW_LIMITS;
        }
        if self.banner {
            v |= CSW_BANNER;
        }
        if self.echo {
            v |= CSW_ECHO;
        }
        if self.lock {
            v |= CSW_LOCK;
        }
        if self.alias {
            v |= CSW_ALIAS;
        }
        if self.text {
            v |= CSW_TEXT;
        }
        v
    }
}

// ---------------------------------------------------------------------------
// Slit geometry calculations (pure functions)
// ---------------------------------------------------------------------------

/// Slit blade positions (in mm).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BladePositions {
    pub left: f64,
    pub right: f64,
    pub top: f64,
    pub bottom: f64,
}

/// Derived slit geometry from blade positions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlitGeometry {
    pub width: f64,
    pub height: f64,
    pub h_center: f64,
    pub v_center: f64,
}

/// Compute slit width from left and right blade positions.
pub fn width_from_blades(left: f64, right: f64) -> f64 {
    left + right
}

/// Compute horizontal center from left and right blade positions.
pub fn h_center_from_blades(left: f64, right: f64) -> f64 {
    (right - left) / 2.0
}

/// Compute slit height from top and bottom blade positions.
pub fn height_from_blades(top: f64, bottom: f64) -> f64 {
    top + bottom
}

/// Compute vertical center from top and bottom blade positions.
pub fn v_center_from_blades(top: f64, bottom: f64) -> f64 {
    (top - bottom) / 2.0
}

/// Compute left and right blade positions from width and horizontal center.
pub fn blades_from_width_center(width: f64, h_center: f64) -> (f64, f64) {
    let left = width / 2.0 - h_center;
    let right = width / 2.0 + h_center;
    (left, right)
}

/// Compute top and bottom blade positions from height and vertical center.
pub fn blades_from_height_center(height: f64, v_center: f64) -> (f64, f64) {
    let top = height / 2.0 + v_center;
    let bottom = height / 2.0 - v_center;
    (top, bottom)
}

/// Compute full slit geometry from blade positions.
pub fn geometry_from_blades(blades: &BladePositions) -> SlitGeometry {
    SlitGeometry {
        width: width_from_blades(blades.left, blades.right),
        height: height_from_blades(blades.top, blades.bottom),
        h_center: h_center_from_blades(blades.left, blades.right),
        v_center: v_center_from_blades(blades.top, blades.bottom),
    }
}

/// Compute axis limits from origin and outer limit in raw steps.
pub fn compute_axis_limits(origin: i32, outer_limit: i32) -> (f64, f64) {
    let lo = raw_to_dial(0, origin);
    let hi = raw_to_dial(outer_limit, origin);
    (lo, hi)
}

/// Compute width limits from individual blade limits.
pub fn compute_width_limits(blade_lo: f64, blade_hi: f64) -> (f64, f64) {
    let width_lo = blade_lo.max(0.0);
    let width_hi = blade_hi * 2.0;
    (width_lo, width_hi)
}

/// Compute center limits from individual blade limits.
pub fn compute_center_limits(blade_lo: f64, blade_hi: f64) -> (f64, f64) {
    let center_lo = (blade_lo - blade_hi) / 2.0;
    let center_hi = (blade_hi - blade_lo) / 2.0;
    (center_lo, center_hi)
}

// ---------------------------------------------------------------------------
// Limit checking
// ---------------------------------------------------------------------------

/// Check if a value is within [lo, hi] inclusive.
pub fn limit_test(lo: f64, val: f64, hi: f64) -> bool {
    lo <= val && val <= hi
}

/// Axis limits for a blade pair.
#[derive(Debug, Clone, Copy)]
pub struct AxisLimits {
    pub blade_a_lo: f64,
    pub blade_a_hi: f64,
    pub blade_b_lo: f64,
    pub blade_b_hi: f64,
    pub gap_lo: f64,
    pub gap_hi: f64,
    pub center_lo: f64,
    pub center_hi: f64,
}

impl AxisLimits {
    /// Compute limits from origin and outer limit.
    pub fn from_hsc_params(origin: i32, outer_limit: i32) -> Self {
        let (lo, hi) = compute_axis_limits(origin, outer_limit);
        let (gap_lo, gap_hi) = compute_width_limits(lo, hi);
        let (center_lo, center_hi) = compute_center_limits(lo, hi);
        Self {
            blade_a_lo: lo,
            blade_a_hi: hi,
            blade_b_lo: lo,
            blade_b_hi: hi,
            gap_lo,
            gap_hi,
            center_lo,
            center_hi,
        }
    }
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

/// States of the HSC controller state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HscState {
    /// Initial startup.
    Startup,
    /// Software disabled.
    Disable,
    /// Communication error (will retry).
    CommError,
    /// Initializing serial port and controller.
    Init,
    /// Reading initial limits from controller.
    InitLimits,
    /// Idle, waiting for commands or periodic poll.
    Idle,
    /// Pre-move: validate targets and compute new positions.
    PreMove,
    /// Reading current positions from controller.
    GetReadback,
}

impl fmt::Display for HscState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HscState::Startup => write!(f, "startup"),
            HscState::Disable => write!(f, "disable"),
            HscState::CommError => write!(f, "comm_error"),
            HscState::Init => write!(f, "init"),
            HscState::InitLimits => write!(f, "init_limits"),
            HscState::Idle => write!(f, "idle"),
            HscState::PreMove => write!(f, "premove"),
            HscState::GetReadback => write!(f, "get_readback"),
        }
    }
}

/// Axis readback data.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AxisReadback {
    pub blade_a: f64,
    pub blade_b: f64,
    pub gap: f64,
    pub center: f64,
}

/// Full HSC controller state.
pub struct HscController {
    pub state: HscState,
    /// Horizontal module ID string.
    pub h_id: String,
    /// Vertical module ID string.
    pub v_id: String,
    /// Horizontal blade orientation.
    pub h_orient: HOrient,
    /// Vertical blade orientation.
    pub v_orient: VOrient,
    /// Horizontal axis origin (raw steps).
    pub h_origin: i32,
    /// Vertical axis origin (raw steps).
    pub v_origin: i32,
    /// Horizontal outer limit (raw steps).
    pub h_outer_limit: i32,
    /// Vertical outer limit (raw steps).
    pub v_outer_limit: i32,
    /// Horizontal axis limits.
    pub h_limits: AxisLimits,
    /// Vertical axis limits.
    pub v_limits: AxisLimits,
    /// Horizontal axis targets (left, right).
    pub h_target: (f64, f64),
    /// Vertical axis targets (top, bottom).
    pub v_target: (f64, f64),
    /// Horizontal axis readback.
    pub h_readback: AxisReadback,
    /// Vertical axis readback.
    pub v_readback: AxisReadback,
    /// Horizontal motor busy.
    pub h_busy: bool,
    /// Vertical motor busy.
    pub v_busy: bool,
    /// Current error code.
    pub error: i32,
    /// Current error message.
    pub error_msg: String,
    /// Controller enabled.
    pub enabled: bool,
    /// Initialization requested.
    pub init_requested: bool,
    /// Calibration requested.
    pub calibrate_requested: bool,
    /// Position locate (readback) requested.
    pub locate_requested: bool,
    /// Stop requested.
    pub stop_requested: bool,
    /// Horizontal move pending.
    pub h_move_pending: bool,
    /// Vertical move pending.
    pub v_move_pending: bool,
    /// Previous target values (for change detection).
    h_target_old: (f64, f64),
    v_target_old: (f64, f64),
    h_gap_old: f64,
    v_gap_old: f64,
    h_center_old: f64,
    v_center_old: f64,
}

impl Default for HscController {
    fn default() -> Self {
        let h_limits = AxisLimits::from_hsc_params(DEFAULT_ORIGIN, DEFAULT_OUTER_LIMIT);
        let v_limits = AxisLimits::from_hsc_params(DEFAULT_ORIGIN, DEFAULT_OUTER_LIMIT);
        Self {
            state: HscState::Startup,
            h_id: String::new(),
            v_id: String::new(),
            h_orient: HOrient::default(),
            v_orient: VOrient::default(),
            h_origin: DEFAULT_ORIGIN,
            v_origin: DEFAULT_ORIGIN,
            h_outer_limit: DEFAULT_OUTER_LIMIT,
            v_outer_limit: DEFAULT_OUTER_LIMIT,
            h_limits,
            v_limits,
            h_target: (0.0, 0.0),
            v_target: (0.0, 0.0),
            h_readback: AxisReadback::default(),
            v_readback: AxisReadback::default(),
            h_busy: false,
            v_busy: false,
            error: NO_ERROR,
            error_msg: "no error".to_string(),
            enabled: true,
            init_requested: true,
            calibrate_requested: false,
            locate_requested: false,
            stop_requested: false,
            h_move_pending: false,
            v_move_pending: false,
            h_target_old: (0.0, 0.0),
            v_target_old: (0.0, 0.0),
            h_gap_old: 0.0,
            v_gap_old: 0.0,
            h_center_old: 0.0,
            v_center_old: 0.0,
        }
    }
}

/// Actions emitted by the state machine for the I/O layer to execute.
#[derive(Debug, Clone, PartialEq)]
pub enum HscAction {
    /// No action this step.
    None,
    /// Send a command to the serial port.
    SendCommand(HscCommand),
    /// Send a command and wait for a response.
    SendCommandReadResponse(HscCommand),
    /// Wait for a specified duration.
    Wait(Duration),
    /// Report updated horizontal readback.
    UpdateHReadback(AxisReadback),
    /// Report updated vertical readback.
    UpdateVReadback(AxisReadback),
    /// Report error.
    ReportError(i32, String),
    /// Report that H axis motor is now idle.
    HMotorIdle,
    /// Report that V axis motor is now idle.
    VMotorIdle,
    /// Report that H axis motor is now busy.
    HMotorBusy,
    /// Report that V axis motor is now busy.
    VMotorBusy,
}

impl HscController {
    /// Create a new controller with the given module IDs.
    pub fn new(h_id: String, v_id: String) -> Self {
        Self {
            h_id,
            v_id,
            ..Default::default()
        }
    }

    /// Set the horizontal target using blade positions (left, right).
    /// Returns true if the target changed and a move should be initiated.
    pub fn set_h_target_blades(&mut self, left: f64, right: f64) -> bool {
        let width = width_from_blades(left, right);
        let center = h_center_from_blades(left, right);

        // Check limits
        if !limit_test(self.h_limits.blade_a_lo, left, self.h_limits.blade_a_hi)
            || !limit_test(self.h_limits.blade_b_lo, right, self.h_limits.blade_b_hi)
            || !limit_test(self.h_limits.gap_lo, width, self.h_limits.gap_hi)
            || !limit_test(self.h_limits.center_lo, center, self.h_limits.center_hi)
        {
            self.error = ERROR_SOFT_LIMITS;
            self.error_msg = "H soft limits exceeded".to_string();
            return false;
        }

        self.error = NO_ERROR;
        self.error_msg = "no error".to_string();
        self.h_target = (left, right);
        self.h_move_pending = true;
        true
    }

    /// Set the horizontal target using gap (width) and center.
    /// Returns true if the target changed and a move should be initiated.
    pub fn set_h_target_gap_center(&mut self, width: f64, center: f64) -> bool {
        let (left, right) = blades_from_width_center(width, center);
        self.set_h_target_blades(left, right)
    }

    /// Set the vertical target using blade positions (top, bottom).
    /// Returns true if the target changed and a move should be initiated.
    pub fn set_v_target_blades(&mut self, top: f64, bottom: f64) -> bool {
        let height = height_from_blades(top, bottom);
        let center = v_center_from_blades(top, bottom);

        if !limit_test(self.v_limits.blade_a_lo, top, self.v_limits.blade_a_hi)
            || !limit_test(self.v_limits.blade_b_lo, bottom, self.v_limits.blade_b_hi)
            || !limit_test(self.v_limits.gap_lo, height, self.v_limits.gap_hi)
            || !limit_test(self.v_limits.center_lo, center, self.v_limits.center_hi)
        {
            self.error = ERROR_SOFT_LIMITS;
            self.error_msg = "V soft limits exceeded".to_string();
            return false;
        }

        self.error = NO_ERROR;
        self.error_msg = "no error".to_string();
        self.v_target = (top, bottom);
        self.v_move_pending = true;
        true
    }

    /// Set the vertical target using gap (height) and center.
    /// Returns true if the target changed and a move should be initiated.
    pub fn set_v_target_gap_center(&mut self, height: f64, center: f64) -> bool {
        let (top, bottom) = blades_from_height_center(height, center);
        self.set_v_target_blades(top, bottom)
    }

    /// Compute the raw motor positions for a horizontal move, accounting for orientation.
    pub fn h_raw_positions(&self) -> (i32, i32) {
        let (left, right) = self.h_target;
        match self.h_orient {
            HOrient::LeftRight => (
                dial_to_raw(left, self.h_origin),
                dial_to_raw(right, self.h_origin),
            ),
            HOrient::RightLeft => (
                dial_to_raw(right, self.h_origin),
                dial_to_raw(left, self.h_origin),
            ),
        }
    }

    /// Compute the raw motor positions for a vertical move, accounting for orientation.
    pub fn v_raw_positions(&self) -> (i32, i32) {
        let (top, bottom) = self.v_target;
        match self.v_orient {
            VOrient::TopBottom => (
                dial_to_raw(top, self.v_origin),
                dial_to_raw(bottom, self.v_origin),
            ),
            VOrient::BottomTop => (
                dial_to_raw(bottom, self.v_origin),
                dial_to_raw(top, self.v_origin),
            ),
        }
    }

    /// Process a position response for the horizontal axis.
    pub fn process_h_position(&mut self, pos_a: i32, pos_b: i32) {
        let a_dial = raw_to_dial(pos_a, self.h_origin);
        let b_dial = raw_to_dial(pos_b, self.h_origin);
        let (left, right) = match self.h_orient {
            HOrient::LeftRight => (a_dial, b_dial),
            HOrient::RightLeft => (b_dial, a_dial),
        };
        self.h_readback = AxisReadback {
            blade_a: left,
            blade_b: right,
            gap: width_from_blades(left, right),
            center: h_center_from_blades(left, right),
        };
        self.h_busy = false;
        self.h_target_old = (left, right);
        self.h_gap_old = self.h_readback.gap;
        self.h_center_old = self.h_readback.center;
    }

    /// Process a position response for the vertical axis.
    pub fn process_v_position(&mut self, pos_a: i32, pos_b: i32) {
        let a_dial = raw_to_dial(pos_a, self.v_origin);
        let b_dial = raw_to_dial(pos_b, self.v_origin);
        let (top, bottom) = match self.v_orient {
            VOrient::TopBottom => (a_dial, b_dial),
            VOrient::BottomTop => (b_dial, a_dial),
        };
        self.v_readback = AxisReadback {
            blade_a: top,
            blade_b: bottom,
            gap: height_from_blades(top, bottom),
            center: v_center_from_blades(top, bottom),
        };
        self.v_busy = false;
        self.v_target_old = (top, bottom);
        self.v_gap_old = self.v_readback.gap;
        self.v_center_old = self.v_readback.center;
    }

    /// Update horizontal axis limits from newly read parameters.
    pub fn update_h_limits(&mut self) {
        self.h_limits = AxisLimits::from_hsc_params(self.h_origin, self.h_outer_limit);
    }

    /// Update vertical axis limits from newly read parameters.
    pub fn update_v_limits(&mut self) {
        self.v_limits = AxisLimits::from_hsc_params(self.v_origin, self.v_outer_limit);
    }

    /// Process a response from the serial port, updating internal state.
    pub fn process_response(&mut self, response: &HscResponse) {
        match response {
            HscResponse::Ok(_) => {
                // Command acknowledged, nothing to do.
            }
            HscResponse::Busy(id) => {
                if id == &self.h_id {
                    self.h_busy = true;
                } else if id == &self.v_id {
                    self.v_busy = true;
                }
            }
            HscResponse::Position { id, pos_a, pos_b } => {
                if id == &self.h_id {
                    self.process_h_position(*pos_a, *pos_b);
                } else if id == &self.v_id {
                    self.process_v_position(*pos_a, *pos_b);
                }
            }
            HscResponse::PositionOk { id, pos_a, pos_b } => {
                if id == &self.h_id {
                    self.process_h_position(*pos_a, *pos_b);
                } else if id == &self.v_id {
                    self.process_v_position(*pos_a, *pos_b);
                }
            }
            HscResponse::Error { id, code } => {
                let code_val = code.unwrap_or(0);
                let msg = if (0..14).contains(&code_val) {
                    HSC_ERROR_MESSAGES[code_val as usize].to_string()
                } else {
                    format!("{}: unknown error", id)
                };
                self.error = code.unwrap_or(ERROR_UNKNOWN);
                self.error_msg = msg;
                // Mark the axis as idle on error
                if id == &self.h_id {
                    self.h_busy = false;
                } else if id == &self.v_id {
                    self.v_busy = false;
                }
            }
            HscResponse::RegisterValue { .. } => {
                // Handled inline during init.
            }
            HscResponse::Identity { .. } => {
                // Informational only.
            }
            HscResponse::Unknown(_) => {
                // Unparseable, ignore.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Async actor
// ---------------------------------------------------------------------------

/// Configuration for the HSC actor.
pub struct HscActorConfig {
    /// Horizontal module ID (e.g. "H-1234").
    pub h_id: String,
    /// Vertical module ID (e.g. "V-1234").
    pub v_id: String,
    /// Horizontal blade orientation.
    pub h_orient: HOrient,
    /// Vertical blade orientation.
    pub v_orient: VOrient,
}

/// Command sent to the HSC actor via its command channel.
#[derive(Debug, Clone)]
pub enum HscActorCommand {
    /// Initialize the controller.
    Init,
    /// Enable/disable the controller.
    SetEnabled(bool),
    /// Stop all motors.
    Stop,
    /// Set horizontal target by blade positions (left, right).
    SetHBlades(f64, f64),
    /// Set horizontal target by width and center.
    SetHGapCenter(f64, f64),
    /// Set vertical target by blade positions (top, bottom).
    SetVBlades(f64, f64),
    /// Set vertical target by height and center.
    SetVGapCenter(f64, f64),
    /// Request position readback.
    Locate,
    /// Calibrate (zero) the controller.
    Calibrate,
    /// Shutdown the actor.
    Shutdown,
}

/// Status published by the HSC actor.
#[derive(Debug, Clone)]
pub struct HscActorStatus {
    pub state: HscState,
    pub h_readback: AxisReadback,
    pub v_readback: AxisReadback,
    pub h_busy: bool,
    pub v_busy: bool,
    pub error: i32,
    pub error_msg: String,
    pub enabled: bool,
}

/// Run the HSC actor, communicating over a serial-like stream.
///
/// This is the main async entry point. It owns the serial read/write halves
/// and a command receiver. It publishes status via a watch channel.
///
/// `R` and `W` are the async read and write halves of the serial connection
/// (e.g., from `tokio::io::split` on a `tokio_serial::SerialStream`).
pub async fn run<R, W>(
    config: HscActorConfig,
    reader: R,
    writer: W,
    mut cmd_rx: tokio::sync::mpsc::Receiver<HscActorCommand>,
    status_tx: watch::Sender<HscActorStatus>,
) where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    let mut ctrl = HscController::new(config.h_id.clone(), config.v_id.clone());
    ctrl.h_orient = config.h_orient;
    ctrl.v_orient = config.v_orient;

    let mut buf_reader = BufReader::new(reader);
    let mut writer = writer;
    let mut line_buf = String::new();

    // Helper closure to send and optionally read
    async fn send_cmd<W2: tokio::io::AsyncWrite + Unpin>(
        writer: &mut W2,
        cmd: &HscCommand,
    ) -> Result<(), std::io::Error> {
        let bytes = cmd.to_bytes();
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn read_response<R2: tokio::io::AsyncBufRead + Unpin>(
        reader: &mut R2,
        buf: &mut String,
    ) -> Result<HscResponse, std::io::Error> {
        buf.clear();
        let n = tokio::time::timeout(RESPONSE_TIMEOUT, reader.read_line(buf)).await;
        match n {
            Ok(Ok(0)) => Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "serial port closed",
            )),
            Ok(Ok(_)) => Ok(parse_response(buf)),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "response timeout",
            )),
        }
    }

    async fn send_and_read<
        R2: tokio::io::AsyncBufRead + Unpin,
        W2: tokio::io::AsyncWrite + Unpin,
    >(
        writer: &mut W2,
        reader: &mut R2,
        buf: &mut String,
        cmd: &HscCommand,
    ) -> Result<HscResponse, std::io::Error> {
        send_cmd(writer, cmd).await?;
        read_response(reader, buf).await
    }

    fn publish_status(ctrl: &HscController, tx: &watch::Sender<HscActorStatus>) {
        let _ = tx.send(HscActorStatus {
            state: ctrl.state,
            h_readback: ctrl.h_readback,
            v_readback: ctrl.v_readback,
            h_busy: ctrl.h_busy,
            v_busy: ctrl.v_busy,
            error: ctrl.error,
            error_msg: ctrl.error_msg.clone(),
            enabled: ctrl.enabled,
        });
    }

    info!("HSC actor starting: h_id={}, v_id={}", ctrl.h_id, ctrl.v_id);
    ctrl.state = HscState::Init;
    publish_status(&ctrl, &status_tx);

    loop {
        match ctrl.state {
            HscState::Startup | HscState::Init => {
                // Validate IDs
                if ctrl.h_id == ctrl.v_id {
                    ctrl.error = ERROR_BAD_ID;
                    ctrl.error_msg = "H & V IDs must be different".to_string();
                    publish_status(&ctrl, &status_tx);
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
                if !validate_hsc_id(&ctrl.h_id) {
                    ctrl.error = ERROR_BAD_ID;
                    ctrl.error_msg = "H ID not a valid HSC ID".to_string();
                    publish_status(&ctrl, &status_tx);
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
                if !validate_hsc_id(&ctrl.v_id) {
                    ctrl.error = ERROR_BAD_ID;
                    ctrl.error_msg = "V ID not a valid HSC ID".to_string();
                    publish_status(&ctrl, &status_tx);
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }

                ctrl.h_busy = false;
                ctrl.v_busy = false;
                ctrl.error = NO_ERROR;
                ctrl.error_msg = "no error".to_string();

                // Kill all movement
                if let Err(e) = send_cmd(&mut writer, &HscCommand::KillAll).await {
                    error!("Failed to send kill command: {e}");
                    ctrl.state = HscState::CommError;
                    publish_status(&ctrl, &status_tx);
                    continue;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;

                ctrl.state = HscState::InitLimits;
                publish_status(&ctrl, &status_tx);
            }

            HscState::InitLimits => {
                // Read horizontal parameters
                let mut read_error = false;

                // Read H outer limit
                match send_and_read(
                    &mut writer,
                    &mut buf_reader,
                    &mut line_buf,
                    &HscCommand::ReadRegister(ctrl.h_id.clone(), register::OUTER_MOTION_LIMIT),
                )
                .await
                {
                    Ok(HscResponse::RegisterValue { value, .. }) => {
                        ctrl.h_outer_limit = value;
                    }
                    Ok(resp) => {
                        debug!("Unexpected response reading H outer limit: {:?}", resp);
                        read_error = true;
                    }
                    Err(e) => {
                        warn!("Error reading H outer limit: {e}");
                        read_error = true;
                    }
                }

                // Read H origin
                match send_and_read(
                    &mut writer,
                    &mut buf_reader,
                    &mut line_buf,
                    &HscCommand::ReadRegister(ctrl.h_id.clone(), register::ORIGIN_POSITION),
                )
                .await
                {
                    Ok(HscResponse::RegisterValue { value, .. }) => {
                        ctrl.h_origin = value;
                    }
                    Ok(resp) => {
                        debug!("Unexpected response reading H origin: {:?}", resp);
                        read_error = true;
                    }
                    Err(e) => {
                        warn!("Error reading H origin: {e}");
                        read_error = true;
                    }
                }

                if !read_error {
                    ctrl.update_h_limits();
                }

                // Read vertical parameters
                read_error = false;

                // Read V outer limit
                match send_and_read(
                    &mut writer,
                    &mut buf_reader,
                    &mut line_buf,
                    &HscCommand::ReadRegister(ctrl.v_id.clone(), register::OUTER_MOTION_LIMIT),
                )
                .await
                {
                    Ok(HscResponse::RegisterValue { value, .. }) => {
                        ctrl.v_outer_limit = value;
                    }
                    Ok(resp) => {
                        debug!("Unexpected response reading V outer limit: {:?}", resp);
                        read_error = true;
                    }
                    Err(e) => {
                        warn!("Error reading V outer limit: {e}");
                        read_error = true;
                    }
                }

                // Read V origin
                match send_and_read(
                    &mut writer,
                    &mut buf_reader,
                    &mut line_buf,
                    &HscCommand::ReadRegister(ctrl.v_id.clone(), register::ORIGIN_POSITION),
                )
                .await
                {
                    Ok(HscResponse::RegisterValue { value, .. }) => {
                        ctrl.v_origin = value;
                    }
                    Ok(resp) => {
                        debug!("Unexpected response reading V origin: {:?}", resp);
                        read_error = true;
                    }
                    Err(e) => {
                        warn!("Error reading V origin: {e}");
                        read_error = true;
                    }
                }

                if !read_error {
                    ctrl.update_v_limits();
                }

                ctrl.locate_requested = true;
                ctrl.state = HscState::Idle;
                publish_status(&ctrl, &status_tx);
            }

            HscState::Disable => {
                publish_status(&ctrl, &status_tx);
                // Wait for enable command
                loop {
                    match cmd_rx.recv().await {
                        Some(HscActorCommand::SetEnabled(true)) => {
                            ctrl.enabled = true;
                            ctrl.init_requested = true;
                            ctrl.state = HscState::Init;
                            break;
                        }
                        Some(HscActorCommand::Shutdown) => {
                            info!("HSC actor shutting down");
                            return;
                        }
                        None => {
                            info!("HSC actor command channel closed");
                            return;
                        }
                        _ => {} // Ignore other commands while disabled
                    }
                }
                publish_status(&ctrl, &status_tx);
            }

            HscState::CommError => {
                ctrl.error = ERROR_COMM_ERROR;
                ctrl.error_msg = "communications error".to_string();
                publish_status(&ctrl, &status_tx);

                // Wait before retrying
                tokio::select! {
                    _ = tokio::time::sleep(ERROR_RECONNECT_INTERVAL) => {
                        ctrl.state = HscState::Init;
                    }
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(HscActorCommand::Init) => {
                                ctrl.state = HscState::Init;
                            }
                            Some(HscActorCommand::Shutdown) => {
                                info!("HSC actor shutting down");
                                return;
                            }
                            None => {
                                info!("HSC actor command channel closed");
                                return;
                            }
                            _ => {}
                        }
                    }
                }
                publish_status(&ctrl, &status_tx);
            }

            HscState::Idle => {
                if !ctrl.enabled {
                    ctrl.state = HscState::Disable;
                    publish_status(&ctrl, &status_tx);
                    continue;
                }

                if ctrl.init_requested {
                    ctrl.init_requested = false;
                    ctrl.state = HscState::Init;
                    publish_status(&ctrl, &status_tx);
                    continue;
                }

                if ctrl.stop_requested {
                    ctrl.stop_requested = false;
                    if let Err(e) = send_cmd(&mut writer, &HscCommand::KillAll).await {
                        error!("Failed to send kill command: {e}");
                        ctrl.state = HscState::CommError;
                        publish_status(&ctrl, &status_tx);
                        continue;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    ctrl.locate_requested = true;
                }

                if ctrl.calibrate_requested {
                    ctrl.calibrate_requested = false;
                    if let Err(e) = send_cmd(&mut writer, &HscCommand::CalibrateImmediate).await {
                        error!("Failed to send calibrate command: {e}");
                        ctrl.state = HscState::CommError;
                        publish_status(&ctrl, &status_tx);
                        continue;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    ctrl.locate_requested = true;
                }

                // Process pending H move
                if ctrl.h_move_pending {
                    ctrl.h_move_pending = false;
                    if ctrl.h_busy {
                        // Interrupt active move
                        let _ = send_cmd(&mut writer, &HscCommand::Kill(ctrl.h_id.clone())).await;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    ctrl.h_busy = true;
                    let (pos_a, pos_b) = ctrl.h_raw_positions();
                    let cmd = HscCommand::Move(ctrl.h_id.clone(), pos_a, pos_b);
                    if let Err(e) = send_cmd(&mut writer, &cmd).await {
                        error!("Failed to send H move: {e}");
                        ctrl.state = HscState::CommError;
                        publish_status(&ctrl, &status_tx);
                        continue;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    ctrl.locate_requested = true;
                    publish_status(&ctrl, &status_tx);
                }

                // Process pending V move
                if ctrl.v_move_pending {
                    ctrl.v_move_pending = false;
                    if ctrl.v_busy {
                        let _ = send_cmd(&mut writer, &HscCommand::Kill(ctrl.v_id.clone())).await;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    ctrl.v_busy = true;
                    let (pos_a, pos_b) = ctrl.v_raw_positions();
                    let cmd = HscCommand::Move(ctrl.v_id.clone(), pos_a, pos_b);
                    if let Err(e) = send_cmd(&mut writer, &cmd).await {
                        error!("Failed to send V move: {e}");
                        ctrl.state = HscState::CommError;
                        publish_status(&ctrl, &status_tx);
                        continue;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    ctrl.locate_requested = true;
                    publish_status(&ctrl, &status_tx);
                }

                if ctrl.locate_requested {
                    ctrl.locate_requested = false;
                    ctrl.state = HscState::GetReadback;
                    publish_status(&ctrl, &status_tx);
                    continue;
                }

                // Wait for commands or poll timeout
                let poll = if ctrl.h_busy || ctrl.v_busy {
                    MOVING_POLL_INTERVAL
                } else {
                    IDLE_POLL_INTERVAL
                };

                tokio::select! {
                    _ = tokio::time::sleep(poll) => {
                        ctrl.locate_requested = true;
                    }
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(HscActorCommand::Init) => {
                                ctrl.init_requested = true;
                            }
                            Some(HscActorCommand::SetEnabled(en)) => {
                                ctrl.enabled = en;
                            }
                            Some(HscActorCommand::Stop) => {
                                ctrl.stop_requested = true;
                            }
                            Some(HscActorCommand::SetHBlades(l, r)) => {
                                ctrl.set_h_target_blades(l, r);
                            }
                            Some(HscActorCommand::SetHGapCenter(w, c)) => {
                                ctrl.set_h_target_gap_center(w, c);
                            }
                            Some(HscActorCommand::SetVBlades(t, b)) => {
                                ctrl.set_v_target_blades(t, b);
                            }
                            Some(HscActorCommand::SetVGapCenter(h, c)) => {
                                ctrl.set_v_target_gap_center(h, c);
                            }
                            Some(HscActorCommand::Locate) => {
                                ctrl.locate_requested = true;
                            }
                            Some(HscActorCommand::Calibrate) => {
                                ctrl.calibrate_requested = true;
                            }
                            Some(HscActorCommand::Shutdown) => {
                                info!("HSC actor shutting down");
                                return;
                            }
                            None => {
                                info!("HSC actor command channel closed");
                                return;
                            }
                        }
                    }
                }
                publish_status(&ctrl, &status_tx);
            }

            HscState::GetReadback => {
                // Read H position
                match send_and_read(
                    &mut writer,
                    &mut buf_reader,
                    &mut line_buf,
                    &HscCommand::PositionInquiry(ctrl.h_id.clone()),
                )
                .await
                {
                    Ok(ref resp) => {
                        let cmd_str = HscCommand::PositionInquiry(ctrl.h_id.clone()).to_serial();
                        if validate_response(&cmd_str, &line_buf) {
                            ctrl.process_response(resp);
                        } else {
                            debug!("H position response ID mismatch");
                        }
                    }
                    Err(e) => {
                        warn!("Error reading H position: {e}");
                    }
                }

                // Read V position
                match send_and_read(
                    &mut writer,
                    &mut buf_reader,
                    &mut line_buf,
                    &HscCommand::PositionInquiry(ctrl.v_id.clone()),
                )
                .await
                {
                    Ok(ref resp) => {
                        let cmd_str = HscCommand::PositionInquiry(ctrl.v_id.clone()).to_serial();
                        if validate_response(&cmd_str, &line_buf) {
                            ctrl.process_response(resp);
                        } else {
                            debug!("V position response ID mismatch");
                        }
                    }
                    Err(e) => {
                        warn!("Error reading V position: {e}");
                    }
                }

                ctrl.state = HscState::Idle;
                publish_status(&ctrl, &status_tx);
            }

            HscState::PreMove => {
                // PreMove is handled inline in Idle via set_*_target_* methods
                ctrl.state = HscState::Idle;
                publish_status(&ctrl, &status_tx);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    // -- Coordinate conversion tests --

    #[test]
    fn raw_to_dial_at_origin() {
        let origin = 400;
        assert!((raw_to_dial(400, origin) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn raw_to_dial_positive() {
        let origin = 400;
        // 800 steps = (800-400)/400 = 1.0 mm
        assert!((raw_to_dial(800, origin) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn raw_to_dial_negative() {
        let origin = 400;
        // 0 steps = (0-400)/400 = -1.0 mm
        assert!((raw_to_dial(0, origin) - (-1.0)).abs() < 1e-9);
    }

    #[test]
    fn dial_to_raw_roundtrip() {
        let origin = 400;
        for raw in [0, 100, 400, 800, 1200, 4400] {
            let dial = raw_to_dial(raw, origin);
            let back = dial_to_raw(dial, origin);
            assert_eq!(back, raw, "roundtrip failed for raw={raw}");
        }
    }

    #[test]
    fn dial_to_raw_at_origin() {
        // dial=0 => raw = 0*400 + 0.5 + 400 = 400 (truncated to i32)
        assert_eq!(dial_to_raw(0.0, 400), 400);
    }

    // -- Slit geometry tests --

    #[test]
    fn width_from_blades_symmetric() {
        assert!((width_from_blades(2.5, 2.5) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn h_center_from_blades_centered() {
        assert!((h_center_from_blades(2.5, 2.5) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn h_center_from_blades_offset() {
        // left=1, right=3 => center = (3-1)/2 = 1.0
        assert!((h_center_from_blades(1.0, 3.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn blades_from_width_center_roundtrip() {
        let left = 1.5;
        let right = 3.5;
        let w = width_from_blades(left, right);
        let c = h_center_from_blades(left, right);
        let (l2, r2) = blades_from_width_center(w, c);
        assert!((l2 - left).abs() < 1e-9);
        assert!((r2 - right).abs() < 1e-9);
    }

    #[test]
    fn height_and_v_center_roundtrip() {
        let top = 4.0;
        let bottom = 2.0;
        let h = height_from_blades(top, bottom);
        let c = v_center_from_blades(top, bottom);
        let (t2, b2) = blades_from_height_center(h, c);
        assert!((t2 - top).abs() < 1e-9);
        assert!((b2 - bottom).abs() < 1e-9);
    }

    #[test]
    fn geometry_from_blades_all() {
        let bp = BladePositions {
            left: 1.0,
            right: 3.0,
            top: 4.0,
            bottom: 2.0,
        };
        let g = geometry_from_blades(&bp);
        assert!((g.width - 4.0).abs() < 1e-9);
        assert!((g.height - 6.0).abs() < 1e-9);
        assert!((g.h_center - 1.0).abs() < 1e-9);
        assert!((g.v_center - 1.0).abs() < 1e-9);
    }

    // -- Axis limits tests --

    #[test]
    fn axis_limits_default() {
        let lim = AxisLimits::from_hsc_params(DEFAULT_ORIGIN, DEFAULT_OUTER_LIMIT);
        let (lo, hi) = compute_axis_limits(DEFAULT_ORIGIN, DEFAULT_OUTER_LIMIT);
        assert!((lim.blade_a_lo - lo).abs() < 1e-9);
        assert!((lim.blade_a_hi - hi).abs() < 1e-9);
        // lo = (0-400)/400 = -1.0, hi = (4400-400)/400 = 10.0
        assert!((lo - (-1.0)).abs() < 1e-9);
        assert!((hi - 10.0).abs() < 1e-9);
    }

    #[test]
    fn limit_test_passes() {
        assert!(limit_test(0.0, 5.0, 10.0));
        assert!(limit_test(0.0, 0.0, 10.0));
        assert!(limit_test(0.0, 10.0, 10.0));
    }

    #[test]
    fn limit_test_fails() {
        assert!(!limit_test(0.0, -0.1, 10.0));
        assert!(!limit_test(0.0, 10.1, 10.0));
    }

    // -- Command formatting tests --

    #[test]
    fn command_kill_all() {
        assert_eq!(HscCommand::KillAll.to_serial(), "!ALL K");
    }

    #[test]
    fn command_position_inquiry() {
        assert_eq!(
            HscCommand::PositionInquiry("H-1234".into()).to_serial(),
            "!H-1234 P"
        );
    }

    #[test]
    fn command_move() {
        assert_eq!(
            HscCommand::Move("V-5678".into(), 100, 200).to_serial(),
            "!V-5678 M 100 200"
        );
    }

    #[test]
    fn command_read_register() {
        assert_eq!(
            HscCommand::ReadRegister("H-1".into(), 1).to_serial(),
            "!H-1 R 1"
        );
    }

    #[test]
    fn command_write_register() {
        assert_eq!(
            HscCommand::WriteRegister("H-1".into(), 7, 255).to_serial(),
            "!H-1 W 7 255"
        );
    }

    #[test]
    fn command_calibrate() {
        assert_eq!(HscCommand::CalibrateImmediate.to_serial(), "!ALL 0 I");
    }

    #[test]
    fn command_to_bytes_has_cr() {
        let bytes = HscCommand::KillAll.to_bytes();
        assert_eq!(bytes, b"!ALL K\r");
    }

    // -- Response parsing tests --

    #[test]
    fn parse_ok() {
        assert_eq!(
            parse_response("%H-1234 OK;"),
            HscResponse::Ok("H-1234".into())
        );
    }

    #[test]
    fn parse_busy() {
        assert_eq!(
            parse_response("%V-5678 BUSY;"),
            HscResponse::Busy("V-5678".into())
        );
    }

    #[test]
    fn parse_position_done() {
        assert_eq!(
            parse_response("%H-1234 500 600 DONE;"),
            HscResponse::Position {
                id: "H-1234".into(),
                pos_a: 500,
                pos_b: 600,
            }
        );
    }

    #[test]
    fn parse_position_ok() {
        assert_eq!(
            parse_response("%H-1234 OK 500 600 DONE;"),
            HscResponse::PositionOk {
                id: "H-1234".into(),
                pos_a: 500,
                pos_b: 600,
            }
        );
    }

    #[test]
    fn parse_error_no_code() {
        assert_eq!(
            parse_response("%H-1234 ERROR;"),
            HscResponse::Error {
                id: "H-1234".into(),
                code: None,
            }
        );
    }

    #[test]
    fn parse_error_with_code() {
        assert_eq!(
            parse_response("%H-1234 ERROR; 5"),
            HscResponse::Error {
                id: "H-1234".into(),
                code: Some(5),
            }
        );
    }

    #[test]
    fn parse_register_value() {
        assert_eq!(
            parse_response("%H-1234 R 4400"),
            HscResponse::RegisterValue {
                id: "H-1234".into(),
                value: 4400,
            }
        );
    }

    #[test]
    fn parse_empty() {
        assert_eq!(parse_response(""), HscResponse::Unknown(String::new()));
    }

    #[test]
    fn parse_no_prefix() {
        match parse_response("no prefix here") {
            HscResponse::Unknown(_) => {}
            other => panic!("Expected Unknown, got {:?}", other),
        }
    }

    #[test]
    fn parse_whitespace_trimmed() {
        assert_eq!(
            parse_response("  %H-1 OK;  \n"),
            HscResponse::Ok("H-1".into())
        );
    }

    // -- Response validation tests --

    #[test]
    fn validate_response_matching() {
        assert!(validate_response("!H-1234 P", "%H-1234 500 600 DONE;"));
    }

    #[test]
    fn validate_response_mismatch() {
        assert!(!validate_response("!H-1234 P", "%V-5678 500 600 DONE;"));
    }

    #[test]
    fn validate_response_empty() {
        assert!(!validate_response("!H-1234 P", ""));
    }

    // -- HSC ID validation tests --

    #[test]
    fn valid_hsc_ids() {
        assert!(validate_hsc_id("XIAHSC-H-1234"));
        assert!(validate_hsc_id("H-1234"));
        assert!(validate_hsc_id("V-5678"));
        assert!(validate_hsc_id("A1234"));
    }

    #[test]
    fn invalid_hsc_ids() {
        assert!(!validate_hsc_id(""));
        assert!(!validate_hsc_id("1234"));
        assert!(!validate_hsc_id("-1234"));
        assert!(!validate_hsc_id("H-"));
    }

    // -- Control/Status Word tests --

    #[test]
    fn csw_decode_zero() {
        let csw = ControlStatusWord::from_raw(0);
        assert_eq!(csw.power_level, 0);
        assert!(!csw.limits);
        assert!(!csw.banner);
        assert!(!csw.echo);
        assert!(!csw.lock);
        assert!(!csw.alias);
        assert!(!csw.text);
    }

    #[test]
    fn csw_decode_all_set() {
        let csw = ControlStatusWord::from_raw(0xFF);
        assert_eq!(csw.power_level, 3);
        assert!(csw.limits);
        assert!(csw.banner);
        assert!(csw.echo);
        assert!(csw.lock);
        assert!(csw.alias);
        assert!(csw.text);
    }

    #[test]
    fn csw_roundtrip() {
        for raw in [0, 1, 2, 3, 0x04, 0x44, 0x7F, 0xFF] {
            let csw = ControlStatusWord::from_raw(raw);
            assert_eq!(csw.to_raw(), raw & 0xFF);
        }
    }

    // -- Controller tests --

    #[test]
    fn controller_set_h_target_within_limits() {
        let mut ctrl = HscController::default();
        ctrl.h_id = "H-1".to_string();
        ctrl.v_id = "V-1".to_string();
        ctrl.update_h_limits();
        // default limits: blade lo=-1.0, hi=10.0
        assert!(ctrl.set_h_target_blades(1.0, 2.0));
        assert_eq!(ctrl.h_target, (1.0, 2.0));
        assert!(ctrl.h_move_pending);
    }

    #[test]
    fn controller_set_h_target_exceeds_limits() {
        let mut ctrl = HscController::default();
        ctrl.h_id = "H-1".to_string();
        ctrl.v_id = "V-1".to_string();
        ctrl.update_h_limits();
        assert!(!ctrl.set_h_target_blades(-2.0, 2.0)); // -2.0 < -1.0
        assert_eq!(ctrl.error, ERROR_SOFT_LIMITS);
    }

    #[test]
    fn controller_process_h_position_left_right() {
        let mut ctrl = HscController::default();
        ctrl.h_id = "H-1".to_string();
        ctrl.h_orient = HOrient::LeftRight;
        ctrl.h_origin = 400;
        // raw A=800 => dial 1.0, raw B=1200 => dial 2.0
        ctrl.process_h_position(800, 1200);
        assert!((ctrl.h_readback.blade_a - 1.0).abs() < 1e-9);
        assert!((ctrl.h_readback.blade_b - 2.0).abs() < 1e-9);
        assert!((ctrl.h_readback.gap - 3.0).abs() < 1e-9);
        assert!((ctrl.h_readback.center - 0.5).abs() < 1e-9);
    }

    #[test]
    fn controller_process_h_position_right_left() {
        let mut ctrl = HscController::default();
        ctrl.h_id = "H-1".to_string();
        ctrl.h_orient = HOrient::RightLeft;
        ctrl.h_origin = 400;
        // raw A=800 => dial 1.0 (right), raw B=1200 => dial 2.0 (left)
        ctrl.process_h_position(800, 1200);
        assert!((ctrl.h_readback.blade_a - 2.0).abs() < 1e-9); // left
        assert!((ctrl.h_readback.blade_b - 1.0).abs() < 1e-9); // right
    }

    #[test]
    fn controller_process_response_busy() {
        let mut ctrl = HscController::default();
        ctrl.h_id = "H-1".to_string();
        ctrl.v_id = "V-1".to_string();
        ctrl.process_response(&HscResponse::Busy("H-1".into()));
        assert!(ctrl.h_busy);
        assert!(!ctrl.v_busy);
    }

    #[test]
    fn controller_process_response_error() {
        let mut ctrl = HscController::default();
        ctrl.h_id = "H-1".to_string();
        ctrl.v_id = "V-1".to_string();
        ctrl.h_busy = true;
        ctrl.process_response(&HscResponse::Error {
            id: "H-1".into(),
            code: Some(6),
        });
        assert!(!ctrl.h_busy);
        assert_eq!(ctrl.error, 6);
        assert_eq!(ctrl.error_msg, "Value Out of Range");
    }

    #[test]
    fn controller_h_raw_positions_left_right() {
        let mut ctrl = HscController::default();
        ctrl.h_orient = HOrient::LeftRight;
        ctrl.h_origin = 400;
        ctrl.h_target = (1.0, 2.0);
        let (a, b) = ctrl.h_raw_positions();
        assert_eq!(a, dial_to_raw(1.0, 400));
        assert_eq!(b, dial_to_raw(2.0, 400));
    }

    #[test]
    fn controller_h_raw_positions_right_left() {
        let mut ctrl = HscController::default();
        ctrl.h_orient = HOrient::RightLeft;
        ctrl.h_origin = 400;
        ctrl.h_target = (1.0, 2.0); // (left, right)
        let (a, b) = ctrl.h_raw_positions();
        // In RightLeft, motor A = right, motor B = left
        assert_eq!(a, dial_to_raw(2.0, 400));
        assert_eq!(b, dial_to_raw(1.0, 400));
    }
}
