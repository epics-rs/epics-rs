//! XIA Slit Controller with sscan support — native Rust port of `xia_slit.st`.
//!
//! Builds on the same HSC-1 hardware as [`super::xiahsc`] but adds:
//! - FIFO-based response processing for multi-axis concurrent reads
//! - Busy record integration for sscan compatibility
//! - Event-based position updates to avoid write-back loops
//! - Control/status word readout and display
//!
//! # Protocol
//!
//! Same serial protocol as xiahsc: `!<ID> <CMD>\r` / `%<ID> <DATA>\r\n`.
//! Input EOS = `\r\n`, output EOS = `\r`.
//!
//! # Coordinate Conventions
//!
//! Positions are in mm, converted from raw motor steps via:
//! - `dial = (raw - origin) / STEPS_PER_MM`
//!
//! Gap/center and individual blade coordinates are interconvertible:
//! - `width  = left + right`
//! - `h0     = (right - left) / 2`
//! - `height = top + bottom`
//! - `v0     = (top - bottom) / 2`

use std::collections::VecDeque;
use std::fmt;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;
use tracing::{debug, error, info};

// Re-use the HSC protocol layer from xiahsc.
use super::xiahsc::{
    self, AxisLimits, ControlStatusWord, HOrient, HscCommand, VOrient, dial_to_raw,
    h_center_from_blades, height_from_blades, raw_to_dial, v_center_from_blades, validate_hsc_id,
    width_from_blades,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// FIFO size for buffering serial responses.
pub const FIFO_SIZE: usize = 40;

/// Moving poll interval (seconds).
pub const MOVING_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Idle poll interval (seconds).
pub const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Move timeout.
pub const MOVE_TIMEOUT: Duration = Duration::from_secs(300);

/// Error reconnect interval.
pub const ERROR_RECONNECT_INTERVAL: Duration = Duration::from_secs(600);

/// Response timeout for serial I/O.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_millis(500);

/// Serial timeout (ticks of ~20ms each, 30 ticks = 600ms).
pub const TIMEOUT_TICKS: u32 = 30;

/// Small epsilon for floating-point comparison.
pub const SMALL: f64 = 1.0e-9;

/// Error codes.
pub const NO_ERROR: i32 = 0;
pub const ERROR_SOFT_LIMITS: i32 = 15;
pub const ERROR_UNKNOWN: i32 = 16;
pub const ERROR_BAD_ID: i32 = 17;
pub const ERROR_COMM_ERROR: i32 = 18;

// ---------------------------------------------------------------------------
// Response FIFO
// ---------------------------------------------------------------------------

/// A bounded FIFO for serial response strings, modeled on the circular buffer
/// in the original SNL program.
#[derive(Debug, Clone)]
pub struct ResponseFifo {
    buf: VecDeque<String>,
    capacity: usize,
}

impl ResponseFifo {
    /// Create a new FIFO with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Push a response into the FIFO. If full, the oldest entry is dropped.
    pub fn push(&mut self, s: String) {
        if self.buf.len() >= self.capacity {
            self.buf.pop_front();
        }
        self.buf.push_back(s);
    }

    /// Pop the oldest response from the FIFO.
    pub fn pop(&mut self) -> Option<String> {
        self.buf.pop_front()
    }

    /// Number of buffered entries.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the FIFO is empty.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Clear all entries.
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

// ---------------------------------------------------------------------------
// Parsed FIFO response
// ---------------------------------------------------------------------------

/// A parsed response from the FIFO with the extracted ID and word decomposition.
#[derive(Debug, Clone)]
pub struct ParsedFifoResponse {
    /// The raw line.
    pub raw: String,
    /// The module ID (from `%<ID>`), if present.
    pub id: String,
    /// Space-delimited words.
    pub words: Vec<String>,
}

/// Parse a raw FIFO line into words and extract the HSC ID.
pub fn parse_fifo_line(line: &str) -> ParsedFifoResponse {
    let line = line.trim();
    let words: Vec<String> = line.split_whitespace().map(|s| s.to_string()).collect();
    let id = words
        .first()
        .and_then(|w| w.strip_prefix('%'))
        .unwrap_or("")
        .to_string();
    ParsedFifoResponse {
        raw: line.to_string(),
        id,
        words,
    }
}

/// Classify a parsed FIFO response.
#[derive(Debug, Clone, PartialEq)]
pub enum FifoResponseKind {
    /// Empty or timeout (numWords == -1 in original).
    Empty,
    /// OK acknowledgment: `%<ID> OK;`
    Ok,
    /// Error: `%<ID> ERROR; [<code>]`
    Error { code: Option<i32> },
    /// Busy: `%<ID> BUSY;`
    Busy,
    /// Position DONE: `%<ID> <posA> <posB> DONE;` (4 words)
    PositionDone { pos_a: i32, pos_b: i32 },
    /// Position OK: `%<ID> OK <posA> <posB>` (5 words, word[1]=="OK")
    PositionOk { pos_a: i32, pos_b: i32 },
    /// Unrecognized.
    Unknown,
}

/// Classify a parsed FIFO response into a kind.
pub fn classify_fifo_response(parsed: &ParsedFifoResponse) -> FifoResponseKind {
    let n = parsed.words.len();
    if n == 0 {
        return FifoResponseKind::Empty;
    }
    if n == 2 && parsed.words[1] == "OK;" {
        return FifoResponseKind::Ok;
    }
    if n >= 2 && parsed.words[1] == "ERROR;" {
        let code = if n >= 3 {
            parsed.words[2].parse::<i32>().ok()
        } else {
            None
        };
        return FifoResponseKind::Error { code };
    }
    if n == 2 && parsed.words[1] == "BUSY;" {
        return FifoResponseKind::Busy;
    }
    if n == 4
        && parsed.words[3] == "DONE;"
        && let (Ok(a), Ok(b)) = (
            parsed.words[1].parse::<i32>(),
            parsed.words[2].parse::<i32>(),
        )
    {
        return FifoResponseKind::PositionDone { pos_a: a, pos_b: b };
    }
    if n == 5
        && parsed.words[1] == "OK"
        && let (Ok(a), Ok(b)) = (
            parsed.words[2].parse::<i32>(),
            parsed.words[3].parse::<i32>(),
        )
    {
        return FifoResponseKind::PositionOk { pos_a: a, pos_b: b };
    }
    FifoResponseKind::Unknown
}

// ---------------------------------------------------------------------------
// Slit coordinate helpers (pure functions)
// ---------------------------------------------------------------------------

/// Check whether two floating-point values differ by more than SMALL.
pub fn different(a: f64, b: f64) -> bool {
    (a - b).abs() > SMALL
}

/// Blade-pair target, with both individual blade and gap/center representations.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BladePairTarget {
    pub blade_a: f64,
    pub blade_b: f64,
    pub gap: f64,
    pub center: f64,
}

impl BladePairTarget {
    /// Create from individual blade positions, computing gap and center.
    /// For horizontal: blade_a=left, blade_b=right.
    /// For vertical: blade_a=top, blade_b=bottom.
    pub fn from_blades(a: f64, b: f64, is_vertical: bool) -> Self {
        if is_vertical {
            Self {
                blade_a: a,
                blade_b: b,
                gap: height_from_blades(a, b),
                center: v_center_from_blades(a, b),
            }
        } else {
            Self {
                blade_a: a,
                blade_b: b,
                gap: width_from_blades(a, b),
                center: h_center_from_blades(a, b),
            }
        }
    }

    /// Create from gap and center, computing individual blades.
    /// For horizontal: blade_a=left, blade_b=right.
    /// For vertical: blade_a=top, blade_b=bottom.
    pub fn from_gap_center(gap: f64, center: f64, is_vertical: bool) -> Self {
        if is_vertical {
            let top = gap / 2.0 + center;
            let bottom = gap / 2.0 - center;
            Self {
                blade_a: top,
                blade_b: bottom,
                gap,
                center,
            }
        } else {
            let left = gap / 2.0 - center;
            let right = gap / 2.0 + center;
            Self {
                blade_a: left,
                blade_b: right,
                gap,
                center,
            }
        }
    }
}

/// Compute the readback gap and center for a horizontal axis.
pub fn h_readback_gap_center(l_rb: f64, r_rb: f64) -> (f64, f64) {
    (l_rb + r_rb, (r_rb - l_rb) / 2.0)
}

/// Compute the readback gap and center for a vertical axis.
pub fn v_readback_gap_center(t_rb: f64, b_rb: f64) -> (f64, f64) {
    (t_rb + b_rb, (t_rb - b_rb) / 2.0)
}

// ---------------------------------------------------------------------------
// Limit checking for the premove state
// ---------------------------------------------------------------------------

/// Validate a horizontal move, returning Ok((left, right, width, h0))
/// or Err with the error message.
pub fn validate_h_move(
    left: f64,
    right: f64,
    width: f64,
    h0: f64,
    limits: &AxisLimits,
) -> Result<(f64, f64, f64, f64), &'static str> {
    let mut err = false;
    let l = left;
    let r = right;
    let w = width;
    let c = h0;

    if !xiahsc::limit_test(limits.blade_a_lo, l, limits.blade_a_hi) {
        err = true;
    }
    if !xiahsc::limit_test(limits.blade_b_lo, r, limits.blade_b_hi) {
        err = true;
    }
    if !xiahsc::limit_test(limits.center_lo, c, limits.center_hi) {
        err = true;
    }
    if !xiahsc::limit_test(limits.gap_lo, w, limits.gap_hi) {
        err = true;
    }

    if err {
        Err("H soft limits exceeded")
    } else {
        Ok((l, r, w, c))
    }
}

/// Validate a vertical move, returning Ok((top, bottom, height, v0))
/// or Err with the error message.
pub fn validate_v_move(
    top: f64,
    bottom: f64,
    height: f64,
    v0: f64,
    limits: &AxisLimits,
) -> Result<(f64, f64, f64, f64), &'static str> {
    let mut err = false;

    if !xiahsc::limit_test(limits.blade_a_lo, top, limits.blade_a_hi) {
        err = true;
    }
    if !xiahsc::limit_test(limits.blade_b_lo, bottom, limits.blade_b_hi) {
        err = true;
    }
    if !xiahsc::limit_test(limits.center_lo, v0, limits.center_hi) {
        err = true;
    }
    if !xiahsc::limit_test(limits.gap_lo, height, limits.gap_hi) {
        err = true;
    }

    if err {
        Err("V soft limits exceeded")
    } else {
        Ok((top, bottom, height, v0))
    }
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

/// States of the XIA slit controller state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XiaSlitState {
    Startup,
    Disable,
    CommError,
    Init,
    InitLimits,
    Idle,
    PreMove,
    ProcessResponse,
    UpdatePositions,
}

impl fmt::Display for XiaSlitState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            XiaSlitState::Startup => write!(f, "startup"),
            XiaSlitState::Disable => write!(f, "disable"),
            XiaSlitState::CommError => write!(f, "comm_error"),
            XiaSlitState::Init => write!(f, "init"),
            XiaSlitState::InitLimits => write!(f, "init_limits"),
            XiaSlitState::Idle => write!(f, "idle"),
            XiaSlitState::PreMove => write!(f, "premove"),
            XiaSlitState::ProcessResponse => write!(f, "process_response"),
            XiaSlitState::UpdatePositions => write!(f, "update_positions"),
        }
    }
}

/// Per-axis readback data for the XIA slit.
#[derive(Debug, Clone, Copy, Default)]
pub struct SlitAxisReadback {
    /// Left or top blade readback (mm).
    pub blade_a_rb: f64,
    /// Right or bottom blade readback (mm).
    pub blade_b_rb: f64,
    /// Gap (width or height) readback (mm).
    pub gap_rb: f64,
    /// Center (h0 or v0) readback (mm).
    pub center_rb: f64,
}

/// Per-axis target data.
#[derive(Debug, Clone, Copy, Default)]
pub struct SlitAxisTarget {
    pub blade_a: f64,
    pub blade_b: f64,
    pub gap: f64,
    pub center: f64,
    // Previous (old) values for change detection.
    pub blade_a_old: f64,
    pub blade_b_old: f64,
    pub gap_old: f64,
    pub center_old: f64,
}

/// Per-axis HSC configuration read from the controller.
#[derive(Debug, Clone, Copy, Default)]
pub struct AxisConfig {
    pub origin: i32,
    pub outer_limit: i32,
    pub step_delay: i32,
    pub gear_backlash: i32,
    pub csw: i32,
}

/// Full XIA slit controller state.
pub struct XiaSlitController {
    pub state: XiaSlitState,
    pub h_id: String,
    pub v_id: String,
    pub h_orient: HOrient,
    pub v_orient: VOrient,
    pub h_config: AxisConfig,
    pub v_config: AxisConfig,
    pub h_limits: AxisLimits,
    pub v_limits: AxisLimits,
    pub h_target: SlitAxisTarget,
    pub v_target: SlitAxisTarget,
    pub h_readback: SlitAxisReadback,
    pub v_readback: SlitAxisReadback,
    pub h_busy: bool,
    pub v_busy: bool,
    pub h_is_moving: bool,
    pub v_is_moving: bool,
    pub error: i32,
    pub error_msg: String,
    pub enabled: bool,
    pub init_requested: bool,
    pub calibrate_requested: bool,
    pub locate_requested: bool,
    pub stop_requested: bool,
    pub h_move_pending: bool,
    pub v_move_pending: bool,
    pub fifo: ResponseFifo,
    /// Status word decoded for horizontal axis.
    pub h_csw: Option<ControlStatusWord>,
    /// Status word decoded for vertical axis.
    pub v_csw: Option<ControlStatusWord>,
}

impl Default for XiaSlitController {
    fn default() -> Self {
        let h_config = AxisConfig {
            origin: xiahsc::DEFAULT_ORIGIN,
            outer_limit: xiahsc::DEFAULT_OUTER_LIMIT,
            ..Default::default()
        };
        let v_config = AxisConfig {
            origin: xiahsc::DEFAULT_ORIGIN,
            outer_limit: xiahsc::DEFAULT_OUTER_LIMIT,
            ..Default::default()
        };
        Self {
            state: XiaSlitState::Startup,
            h_id: String::new(),
            v_id: String::new(),
            h_orient: HOrient::default(),
            v_orient: VOrient::default(),
            h_config,
            v_config,
            h_limits: AxisLimits::from_hsc_params(h_config.origin, h_config.outer_limit),
            v_limits: AxisLimits::from_hsc_params(v_config.origin, v_config.outer_limit),
            h_target: SlitAxisTarget::default(),
            v_target: SlitAxisTarget::default(),
            h_readback: SlitAxisReadback::default(),
            v_readback: SlitAxisReadback::default(),
            h_busy: false,
            v_busy: false,
            h_is_moving: false,
            v_is_moving: false,
            error: NO_ERROR,
            error_msg: "no error".to_string(),
            enabled: true,
            init_requested: true,
            calibrate_requested: false,
            locate_requested: false,
            stop_requested: false,
            h_move_pending: false,
            v_move_pending: false,
            fifo: ResponseFifo::new(FIFO_SIZE),
            h_csw: None,
            v_csw: None,
        }
    }
}

impl XiaSlitController {
    /// Create a new controller with the given module IDs.
    pub fn new(h_id: String, v_id: String) -> Self {
        Self {
            h_id,
            v_id,
            ..Default::default()
        }
    }

    /// Update limits for the horizontal axis.
    pub fn update_h_limits(&mut self) {
        self.h_limits =
            AxisLimits::from_hsc_params(self.h_config.origin, self.h_config.outer_limit);
    }

    /// Update limits for the vertical axis.
    pub fn update_v_limits(&mut self) {
        self.v_limits =
            AxisLimits::from_hsc_params(self.v_config.origin, self.v_config.outer_limit);
    }

    /// Process a DONE or OK position response for a given axis.
    pub fn update_axis_position(&mut self, id: &str, pos_a: i32, pos_b: i32) {
        if id == self.h_id {
            let a_dial = raw_to_dial(pos_a, self.h_config.origin);
            let b_dial = raw_to_dial(pos_b, self.h_config.origin);
            let (left, right) = match self.h_orient {
                HOrient::LeftRight => (a_dial, b_dial),
                HOrient::RightLeft => (b_dial, a_dial),
            };
            let (gap, center) = h_readback_gap_center(left, right);
            self.h_readback = SlitAxisReadback {
                blade_a_rb: left,
                blade_b_rb: right,
                gap_rb: gap,
                center_rb: center,
            };
            self.h_busy = false;
            self.h_target.blade_a_old = left;
            self.h_target.blade_b_old = right;
            self.h_target.gap_old = gap;
            self.h_target.center_old = center;
        } else if id == self.v_id {
            let a_dial = raw_to_dial(pos_a, self.v_config.origin);
            let b_dial = raw_to_dial(pos_b, self.v_config.origin);
            let (top, bottom) = match self.v_orient {
                VOrient::TopBottom => (a_dial, b_dial),
                VOrient::BottomTop => (b_dial, a_dial),
            };
            let (gap, center) = v_readback_gap_center(top, bottom);
            self.v_readback = SlitAxisReadback {
                blade_a_rb: top,
                blade_b_rb: bottom,
                gap_rb: gap,
                center_rb: center,
            };
            self.v_busy = false;
            self.v_target.blade_a_old = top;
            self.v_target.blade_b_old = bottom;
            self.v_target.gap_old = gap;
            self.v_target.center_old = center;
        }
    }

    /// Compute the raw motor positions for a horizontal move.
    pub fn h_raw_positions(&self) -> (i32, i32) {
        let (left, right) = (self.h_target.blade_a, self.h_target.blade_b);
        match self.h_orient {
            HOrient::LeftRight => (
                dial_to_raw(left, self.h_config.origin),
                dial_to_raw(right, self.h_config.origin),
            ),
            HOrient::RightLeft => (
                dial_to_raw(right, self.h_config.origin),
                dial_to_raw(left, self.h_config.origin),
            ),
        }
    }

    /// Compute the raw motor positions for a vertical move.
    pub fn v_raw_positions(&self) -> (i32, i32) {
        let (top, bottom) = (self.v_target.blade_a, self.v_target.blade_b);
        match self.v_orient {
            VOrient::TopBottom => (
                dial_to_raw(top, self.v_config.origin),
                dial_to_raw(bottom, self.v_config.origin),
            ),
            VOrient::BottomTop => (
                dial_to_raw(bottom, self.v_config.origin),
                dial_to_raw(top, self.v_config.origin),
            ),
        }
    }

    /// Set horizontal target by blade positions. Returns true if valid.
    pub fn set_h_blades(&mut self, left: f64, right: f64) -> bool {
        let width = width_from_blades(left, right);
        let h0 = h_center_from_blades(left, right);
        match validate_h_move(left, right, width, h0, &self.h_limits) {
            Ok((l, r, w, c)) => {
                self.h_target.blade_a = l;
                self.h_target.blade_b = r;
                self.h_target.gap = w;
                self.h_target.center = c;
                self.h_move_pending = true;
                self.error = NO_ERROR;
                self.error_msg = "no error".to_string();
                true
            }
            Err(msg) => {
                self.error = ERROR_SOFT_LIMITS;
                self.error_msg = msg.to_string();
                false
            }
        }
    }

    /// Set horizontal target by gap and center. Returns true if valid.
    pub fn set_h_gap_center(&mut self, width: f64, h0: f64) -> bool {
        let left = width / 2.0 - h0;
        let right = width / 2.0 + h0;
        match validate_h_move(left, right, width, h0, &self.h_limits) {
            Ok((l, r, w, c)) => {
                self.h_target.blade_a = l;
                self.h_target.blade_b = r;
                self.h_target.gap = w;
                self.h_target.center = c;
                self.h_move_pending = true;
                self.error = NO_ERROR;
                self.error_msg = "no error".to_string();
                true
            }
            Err(msg) => {
                self.error = ERROR_SOFT_LIMITS;
                self.error_msg = msg.to_string();
                false
            }
        }
    }

    /// Set vertical target by blade positions. Returns true if valid.
    pub fn set_v_blades(&mut self, top: f64, bottom: f64) -> bool {
        let height = height_from_blades(top, bottom);
        let v0 = v_center_from_blades(top, bottom);
        match validate_v_move(top, bottom, height, v0, &self.v_limits) {
            Ok((t, b, h, c)) => {
                self.v_target.blade_a = t;
                self.v_target.blade_b = b;
                self.v_target.gap = h;
                self.v_target.center = c;
                self.v_move_pending = true;
                self.error = NO_ERROR;
                self.error_msg = "no error".to_string();
                true
            }
            Err(msg) => {
                self.error = ERROR_SOFT_LIMITS;
                self.error_msg = msg.to_string();
                false
            }
        }
    }

    /// Set vertical target by gap and center. Returns true if valid.
    pub fn set_v_gap_center(&mut self, height: f64, v0: f64) -> bool {
        let top = height / 2.0 + v0;
        let bottom = height / 2.0 - v0;
        match validate_v_move(top, bottom, height, v0, &self.v_limits) {
            Ok((t, b, h, c)) => {
                self.v_target.blade_a = t;
                self.v_target.blade_b = b;
                self.v_target.gap = h;
                self.v_target.center = c;
                self.v_move_pending = true;
                self.error = NO_ERROR;
                self.error_msg = "no error".to_string();
                true
            }
            Err(msg) => {
                self.error = ERROR_SOFT_LIMITS;
                self.error_msg = msg.to_string();
                false
            }
        }
    }

    /// Check if a done position matches the commanded raw position for H axis.
    /// Returns true if the axes have reached their target.
    pub fn h_at_target(&self, pos_a: i32, pos_b: i32) -> bool {
        let l_raw = dial_to_raw(self.h_target.blade_a, self.h_config.origin);
        let r_raw = dial_to_raw(self.h_target.blade_b, self.h_config.origin);
        match self.h_orient {
            HOrient::LeftRight => pos_a == l_raw && pos_b == r_raw,
            HOrient::RightLeft => pos_a == r_raw && pos_b == l_raw,
        }
    }

    /// Check if a done position matches the commanded raw position for V axis.
    pub fn v_at_target(&self, pos_a: i32, pos_b: i32) -> bool {
        let t_raw = dial_to_raw(self.v_target.blade_a, self.v_config.origin);
        let b_raw = dial_to_raw(self.v_target.blade_b, self.v_config.origin);
        match self.v_orient {
            VOrient::TopBottom => pos_a == t_raw && pos_b == b_raw,
            VOrient::BottomTop => pos_a == b_raw && pos_b == t_raw,
        }
    }
}

// ---------------------------------------------------------------------------
// Actor types
// ---------------------------------------------------------------------------

/// Configuration for the XIA slit actor.
pub struct XiaSlitActorConfig {
    pub h_id: String,
    pub v_id: String,
    pub h_orient: HOrient,
    pub v_orient: VOrient,
}

/// Commands that can be sent to the XIA slit actor.
#[derive(Debug, Clone)]
pub enum XiaSlitActorCommand {
    Init,
    SetEnabled(bool),
    Stop,
    SetHBlades(f64, f64),
    SetHGapCenter(f64, f64),
    SetVBlades(f64, f64),
    SetVGapCenter(f64, f64),
    Locate,
    Calibrate,
    Shutdown,
}

/// Status published by the XIA slit actor.
#[derive(Debug, Clone)]
pub struct XiaSlitActorStatus {
    pub state: XiaSlitState,
    pub h_readback: SlitAxisReadback,
    pub v_readback: SlitAxisReadback,
    pub h_busy: bool,
    pub v_busy: bool,
    pub error: i32,
    pub error_msg: String,
    pub enabled: bool,
    pub h_csw: Option<ControlStatusWord>,
    pub v_csw: Option<ControlStatusWord>,
}

// ---------------------------------------------------------------------------
// Async actor
// ---------------------------------------------------------------------------

/// Run the XIA slit actor.
///
/// `R` and `W` are the async read/write halves of the serial connection.
pub async fn run<R, W>(
    config: XiaSlitActorConfig,
    reader: R,
    writer: W,
    mut cmd_rx: tokio::sync::mpsc::Receiver<XiaSlitActorCommand>,
    status_tx: watch::Sender<XiaSlitActorStatus>,
) where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    let mut ctrl = XiaSlitController::new(config.h_id.clone(), config.v_id.clone());
    ctrl.h_orient = config.h_orient;
    ctrl.v_orient = config.v_orient;

    let mut buf_reader = BufReader::new(reader);
    let mut writer = writer;
    let mut line_buf = String::new();

    async fn send_cmd<W2: tokio::io::AsyncWrite + Unpin>(
        writer: &mut W2,
        cmd: &HscCommand,
    ) -> Result<(), std::io::Error> {
        let bytes = cmd.to_bytes();
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn read_line_timeout<R2: tokio::io::AsyncBufRead + Unpin>(
        reader: &mut R2,
        buf: &mut String,
        timeout: Duration,
    ) -> Result<String, std::io::Error> {
        buf.clear();
        match tokio::time::timeout(timeout, reader.read_line(buf)).await {
            Ok(Ok(0)) => Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF",
            )),
            Ok(Ok(_)) => Ok(buf.clone()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout")),
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
    ) -> Result<String, std::io::Error> {
        send_cmd(writer, cmd).await?;
        read_line_timeout(reader, buf, RESPONSE_TIMEOUT).await
    }

    fn publish_status(ctrl: &XiaSlitController, tx: &watch::Sender<XiaSlitActorStatus>) {
        let _ = tx.send(XiaSlitActorStatus {
            state: ctrl.state,
            h_readback: ctrl.h_readback,
            v_readback: ctrl.v_readback,
            h_busy: ctrl.h_busy,
            v_busy: ctrl.v_busy,
            error: ctrl.error,
            error_msg: ctrl.error_msg.clone(),
            enabled: ctrl.enabled,
            h_csw: ctrl.h_csw,
            v_csw: ctrl.v_csw,
        });
    }

    /// Read register value from a response line. Expects `%<ID> R <value>`.
    fn parse_register_value(line: &str) -> Option<i32> {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() >= 3 {
            words[2].parse::<i32>().ok()
        } else {
            None
        }
    }

    info!(
        "XIA slit actor starting: h_id={}, v_id={}",
        ctrl.h_id, ctrl.v_id
    );
    ctrl.state = XiaSlitState::Init;
    publish_status(&ctrl, &status_tx);

    loop {
        match ctrl.state {
            XiaSlitState::Startup | XiaSlitState::Init => {
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
                ctrl.h_is_moving = false;
                ctrl.v_is_moving = false;
                ctrl.error = NO_ERROR;
                ctrl.error_msg = "no error".to_string();
                ctrl.fifo.clear();

                // Kill all movement
                if let Err(e) = send_cmd(&mut writer, &HscCommand::KillAll).await {
                    error!("Failed to send kill: {e}");
                    ctrl.state = XiaSlitState::CommError;
                    publish_status(&ctrl, &status_tx);
                    continue;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;

                // Request all positions (responses go into FIFO later)
                let _ =
                    send_cmd(&mut writer, &HscCommand::PositionInquiry("ALL".to_string())).await;
                tokio::time::sleep(Duration::from_millis(100)).await;

                // Drain any responses into the FIFO
                while let Ok(line) =
                    read_line_timeout(&mut buf_reader, &mut line_buf, Duration::from_millis(100))
                        .await
                {
                    ctrl.fifo.push(line);
                }

                ctrl.state = XiaSlitState::InitLimits;
                publish_status(&ctrl, &status_tx);
            }

            XiaSlitState::InitLimits => {
                let mut read_error = false;

                // Read H step delay
                match send_and_read(
                    &mut writer,
                    &mut buf_reader,
                    &mut line_buf,
                    &HscCommand::ReadRegister(
                        ctrl.h_id.clone(),
                        xiahsc::register::MOTOR_STEP_DELAY,
                    ),
                )
                .await
                {
                    Ok(ref line) => {
                        if let Some(v) = parse_register_value(line) {
                            ctrl.h_config.step_delay = v;
                        }
                    }
                    Err(e) => {
                        debug!("Error reading H step delay: {e}");
                        read_error = true;
                    }
                }

                // Read H gear backlash
                match send_and_read(
                    &mut writer,
                    &mut buf_reader,
                    &mut line_buf,
                    &HscCommand::ReadRegister(ctrl.h_id.clone(), xiahsc::register::GEAR_BACKLASH),
                )
                .await
                {
                    Ok(ref line) => {
                        if let Some(v) = parse_register_value(line) {
                            ctrl.h_config.gear_backlash = v;
                        }
                    }
                    Err(e) => {
                        debug!("Error reading H backlash: {e}");
                        read_error = true;
                    }
                }

                // Read H control word
                match send_and_read(
                    &mut writer,
                    &mut buf_reader,
                    &mut line_buf,
                    &HscCommand::ReadRegister(ctrl.h_id.clone(), xiahsc::register::CONTROL_WORD),
                )
                .await
                {
                    Ok(ref line) => {
                        if let Some(v) = parse_register_value(line) {
                            ctrl.h_config.csw = v;
                            ctrl.h_csw = Some(ControlStatusWord::from_raw(v));
                        }
                    }
                    Err(e) => {
                        debug!("Error reading H CSW: {e}");
                        read_error = true;
                    }
                }

                // Read H outer limit
                match send_and_read(
                    &mut writer,
                    &mut buf_reader,
                    &mut line_buf,
                    &HscCommand::ReadRegister(
                        ctrl.h_id.clone(),
                        xiahsc::register::OUTER_MOTION_LIMIT,
                    ),
                )
                .await
                {
                    Ok(ref line) => {
                        if let Some(v) = parse_register_value(line) {
                            ctrl.h_config.outer_limit = v;
                        }
                    }
                    Err(e) => {
                        debug!("Error reading H outer limit: {e}");
                        read_error = true;
                    }
                }

                // Read H origin
                match send_and_read(
                    &mut writer,
                    &mut buf_reader,
                    &mut line_buf,
                    &HscCommand::ReadRegister(ctrl.h_id.clone(), xiahsc::register::ORIGIN_POSITION),
                )
                .await
                {
                    Ok(ref line) => {
                        if let Some(v) = parse_register_value(line) {
                            ctrl.h_config.origin = v;
                        }
                    }
                    Err(e) => {
                        debug!("Error reading H origin: {e}");
                        read_error = true;
                    }
                }

                if !read_error {
                    ctrl.update_h_limits();
                }

                // Read V parameters
                read_error = false;

                match send_and_read(
                    &mut writer,
                    &mut buf_reader,
                    &mut line_buf,
                    &HscCommand::ReadRegister(ctrl.v_id.clone(), xiahsc::register::CONTROL_WORD),
                )
                .await
                {
                    Ok(ref line) => {
                        if let Some(v) = parse_register_value(line) {
                            ctrl.v_config.csw = v;
                            ctrl.v_csw = Some(ControlStatusWord::from_raw(v));
                        }
                    }
                    Err(e) => {
                        debug!("Error reading V CSW: {e}");
                        read_error = true;
                    }
                }

                match send_and_read(
                    &mut writer,
                    &mut buf_reader,
                    &mut line_buf,
                    &HscCommand::ReadRegister(ctrl.v_id.clone(), xiahsc::register::GEAR_BACKLASH),
                )
                .await
                {
                    Ok(ref line) => {
                        if let Some(v) = parse_register_value(line) {
                            ctrl.v_config.gear_backlash = v;
                        }
                    }
                    Err(e) => {
                        debug!("Error reading V backlash: {e}");
                        read_error = true;
                    }
                }

                match send_and_read(
                    &mut writer,
                    &mut buf_reader,
                    &mut line_buf,
                    &HscCommand::ReadRegister(
                        ctrl.v_id.clone(),
                        xiahsc::register::MOTOR_STEP_DELAY,
                    ),
                )
                .await
                {
                    Ok(ref line) => {
                        if let Some(v) = parse_register_value(line) {
                            ctrl.v_config.step_delay = v;
                        }
                    }
                    Err(e) => {
                        debug!("Error reading V step delay: {e}");
                        read_error = true;
                    }
                }

                match send_and_read(
                    &mut writer,
                    &mut buf_reader,
                    &mut line_buf,
                    &HscCommand::ReadRegister(
                        ctrl.v_id.clone(),
                        xiahsc::register::OUTER_MOTION_LIMIT,
                    ),
                )
                .await
                {
                    Ok(ref line) => {
                        if let Some(v) = parse_register_value(line) {
                            ctrl.v_config.outer_limit = v;
                        }
                    }
                    Err(e) => {
                        debug!("Error reading V outer limit: {e}");
                        read_error = true;
                    }
                }

                match send_and_read(
                    &mut writer,
                    &mut buf_reader,
                    &mut line_buf,
                    &HscCommand::ReadRegister(ctrl.v_id.clone(), xiahsc::register::ORIGIN_POSITION),
                )
                .await
                {
                    Ok(ref line) => {
                        if let Some(v) = parse_register_value(line) {
                            ctrl.v_config.origin = v;
                        }
                    }
                    Err(e) => {
                        debug!("Error reading V origin: {e}");
                        read_error = true;
                    }
                }

                if !read_error {
                    ctrl.update_v_limits();
                }

                ctrl.locate_requested = true;
                ctrl.state = XiaSlitState::Idle;
                publish_status(&ctrl, &status_tx);
            }

            XiaSlitState::Disable => {
                publish_status(&ctrl, &status_tx);
                loop {
                    match cmd_rx.recv().await {
                        Some(XiaSlitActorCommand::SetEnabled(true)) => {
                            ctrl.enabled = true;
                            ctrl.init_requested = true;
                            ctrl.state = XiaSlitState::Init;
                            break;
                        }
                        Some(XiaSlitActorCommand::Shutdown) | None => {
                            info!("XIA slit actor shutting down");
                            return;
                        }
                        _ => {}
                    }
                }
                publish_status(&ctrl, &status_tx);
            }

            XiaSlitState::CommError => {
                ctrl.error = ERROR_COMM_ERROR;
                ctrl.error_msg = "communications error".to_string();
                publish_status(&ctrl, &status_tx);

                tokio::select! {
                    _ = tokio::time::sleep(ERROR_RECONNECT_INTERVAL) => {
                        ctrl.state = XiaSlitState::Init;
                    }
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(XiaSlitActorCommand::Init) => {
                                ctrl.state = XiaSlitState::Init;
                            }
                            Some(XiaSlitActorCommand::Shutdown) | None => {
                                info!("XIA slit actor shutting down");
                                return;
                            }
                            _ => {}
                        }
                    }
                }
                publish_status(&ctrl, &status_tx);
            }

            XiaSlitState::Idle => {
                if !ctrl.enabled {
                    ctrl.state = XiaSlitState::Disable;
                    publish_status(&ctrl, &status_tx);
                    continue;
                }

                if ctrl.init_requested {
                    ctrl.init_requested = false;
                    ctrl.state = XiaSlitState::Init;
                    publish_status(&ctrl, &status_tx);
                    continue;
                }

                if ctrl.stop_requested {
                    ctrl.stop_requested = false;
                    let _ = send_cmd(&mut writer, &HscCommand::KillAll).await;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }

                if ctrl.calibrate_requested {
                    ctrl.calibrate_requested = false;
                    let _ = send_cmd(&mut writer, &HscCommand::CalibrateImmediate).await;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    ctrl.locate_requested = true;
                }

                // Process pending H move
                if ctrl.h_move_pending {
                    ctrl.h_move_pending = false;
                    if ctrl.h_busy {
                        let _ = send_cmd(&mut writer, &HscCommand::Kill(ctrl.h_id.clone())).await;
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    } else {
                        ctrl.h_busy = true;
                    }
                    ctrl.h_is_moving = true;
                    let (pos_a, pos_b) = ctrl.h_raw_positions();
                    let cmd = HscCommand::Move(ctrl.h_id.clone(), pos_a, pos_b);
                    if let Err(e) = send_cmd(&mut writer, &cmd).await {
                        error!("Failed to send H move: {e}");
                        ctrl.state = XiaSlitState::CommError;
                        publish_status(&ctrl, &status_tx);
                        continue;
                    }
                    publish_status(&ctrl, &status_tx);
                }

                // Process pending V move
                if ctrl.v_move_pending {
                    ctrl.v_move_pending = false;
                    if ctrl.v_busy {
                        let _ = send_cmd(&mut writer, &HscCommand::Kill(ctrl.v_id.clone())).await;
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    } else {
                        ctrl.v_busy = true;
                    }
                    ctrl.v_is_moving = true;
                    let (pos_a, pos_b) = ctrl.v_raw_positions();
                    let cmd = HscCommand::Move(ctrl.v_id.clone(), pos_a, pos_b);
                    if let Err(e) = send_cmd(&mut writer, &cmd).await {
                        error!("Failed to send V move: {e}");
                        ctrl.state = XiaSlitState::CommError;
                        publish_status(&ctrl, &status_tx);
                        continue;
                    }
                    publish_status(&ctrl, &status_tx);
                }

                // Locate: send position inquiry, drain into FIFO
                if ctrl.locate_requested {
                    ctrl.locate_requested = false;
                    let _ = send_cmd(&mut writer, &HscCommand::PositionInquiry("ALL".to_string()))
                        .await;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    while let Ok(line) = read_line_timeout(
                        &mut buf_reader,
                        &mut line_buf,
                        Duration::from_millis(100),
                    )
                    .await
                    {
                        ctrl.fifo.push(line);
                    }
                }

                // Process FIFO entries
                while let Some(line) = ctrl.fifo.pop() {
                    let parsed = parse_fifo_line(&line);
                    let kind = classify_fifo_response(&parsed);
                    match kind {
                        FifoResponseKind::Empty | FifoResponseKind::Ok => {}
                        FifoResponseKind::Error { code } => {
                            let code_val = code.unwrap_or(0);
                            if (0..14).contains(&code_val) {
                                ctrl.error = code_val;
                                ctrl.error_msg =
                                    xiahsc::HSC_ERROR_MESSAGES[code_val as usize].to_string();
                            } else {
                                ctrl.error = ERROR_UNKNOWN;
                                ctrl.error_msg = format!("{}: unknown error", parsed.id);
                            }
                            if parsed.id == ctrl.h_id {
                                ctrl.h_is_moving = false;
                                ctrl.h_busy = false;
                            } else if parsed.id == ctrl.v_id {
                                ctrl.v_is_moving = false;
                                ctrl.v_busy = false;
                            }
                        }
                        FifoResponseKind::Busy => {
                            if parsed.id == ctrl.h_id {
                                ctrl.h_busy = true;
                            } else if parsed.id == ctrl.v_id {
                                ctrl.v_busy = true;
                            }
                            if ctrl.error != NO_ERROR {
                                ctrl.error = NO_ERROR;
                                ctrl.error_msg = "no error".to_string();
                            }
                        }
                        FifoResponseKind::PositionDone { pos_a, pos_b } => {
                            if ctrl.error != NO_ERROR {
                                ctrl.error = NO_ERROR;
                                ctrl.error_msg = "no error".to_string();
                            }
                            if parsed.id == ctrl.h_id {
                                ctrl.h_is_moving = false;
                            } else if parsed.id == ctrl.v_id {
                                ctrl.v_is_moving = false;
                            }
                            ctrl.update_axis_position(&parsed.id, pos_a, pos_b);
                        }
                        FifoResponseKind::PositionOk { pos_a, pos_b } => {
                            if ctrl.error != NO_ERROR {
                                ctrl.error = NO_ERROR;
                                ctrl.error_msg = "no error".to_string();
                            }
                            ctrl.update_axis_position(&parsed.id, pos_a, pos_b);
                        }
                        FifoResponseKind::Unknown => {
                            debug!("Unrecognized response: {}", line.trim());
                        }
                    }
                }

                // Update targets from readback when not busy
                if !ctrl.h_busy {
                    if different(ctrl.h_target.blade_a, ctrl.h_readback.blade_a_rb) {
                        ctrl.h_target.blade_a = ctrl.h_readback.blade_a_rb;
                    }
                    if different(ctrl.h_target.blade_b, ctrl.h_readback.blade_b_rb) {
                        ctrl.h_target.blade_b = ctrl.h_readback.blade_b_rb;
                    }
                    if different(ctrl.h_target.gap, ctrl.h_readback.gap_rb) {
                        ctrl.h_target.gap = ctrl.h_readback.gap_rb;
                    }
                    if different(ctrl.h_target.center, ctrl.h_readback.center_rb) {
                        ctrl.h_target.center = ctrl.h_readback.center_rb;
                    }
                }
                if !ctrl.v_busy {
                    if different(ctrl.v_target.blade_a, ctrl.v_readback.blade_a_rb) {
                        ctrl.v_target.blade_a = ctrl.v_readback.blade_a_rb;
                    }
                    if different(ctrl.v_target.blade_b, ctrl.v_readback.blade_b_rb) {
                        ctrl.v_target.blade_b = ctrl.v_readback.blade_b_rb;
                    }
                    if different(ctrl.v_target.gap, ctrl.v_readback.gap_rb) {
                        ctrl.v_target.gap = ctrl.v_readback.gap_rb;
                    }
                    if different(ctrl.v_target.center, ctrl.v_readback.center_rb) {
                        ctrl.v_target.center = ctrl.v_readback.center_rb;
                    }
                }

                publish_status(&ctrl, &status_tx);

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
                            Some(XiaSlitActorCommand::Init) => {
                                ctrl.init_requested = true;
                            }
                            Some(XiaSlitActorCommand::SetEnabled(en)) => {
                                ctrl.enabled = en;
                            }
                            Some(XiaSlitActorCommand::Stop) => {
                                ctrl.stop_requested = true;
                            }
                            Some(XiaSlitActorCommand::SetHBlades(l, r)) => {
                                ctrl.set_h_blades(l, r);
                            }
                            Some(XiaSlitActorCommand::SetHGapCenter(w, c)) => {
                                ctrl.set_h_gap_center(w, c);
                            }
                            Some(XiaSlitActorCommand::SetVBlades(t, b)) => {
                                ctrl.set_v_blades(t, b);
                            }
                            Some(XiaSlitActorCommand::SetVGapCenter(h, c)) => {
                                ctrl.set_v_gap_center(h, c);
                            }
                            Some(XiaSlitActorCommand::Locate) => {
                                ctrl.locate_requested = true;
                            }
                            Some(XiaSlitActorCommand::Calibrate) => {
                                ctrl.calibrate_requested = true;
                            }
                            Some(XiaSlitActorCommand::Shutdown) => {
                                info!("XIA slit actor shutting down");
                                return;
                            }
                            None => {
                                info!("XIA slit command channel closed");
                                return;
                            }
                        }
                    }
                }
            }

            XiaSlitState::PreMove
            | XiaSlitState::ProcessResponse
            | XiaSlitState::UpdatePositions => {
                // These states are handled inline in Idle for the async actor.
                ctrl.state = XiaSlitState::Idle;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- ResponseFifo tests --

    #[test]
    fn fifo_push_pop() {
        let mut fifo = ResponseFifo::new(4);
        fifo.push("one".into());
        fifo.push("two".into());
        assert_eq!(fifo.len(), 2);
        assert_eq!(fifo.pop(), Some("one".into()));
        assert_eq!(fifo.pop(), Some("two".into()));
        assert!(fifo.is_empty());
    }

    #[test]
    fn fifo_overflow_drops_oldest() {
        let mut fifo = ResponseFifo::new(2);
        fifo.push("a".into());
        fifo.push("b".into());
        fifo.push("c".into());
        assert_eq!(fifo.len(), 2);
        assert_eq!(fifo.pop(), Some("b".into()));
        assert_eq!(fifo.pop(), Some("c".into()));
    }

    #[test]
    fn fifo_clear() {
        let mut fifo = ResponseFifo::new(10);
        fifo.push("x".into());
        fifo.push("y".into());
        fifo.clear();
        assert!(fifo.is_empty());
    }

    // -- parse_fifo_line tests --

    #[test]
    fn parse_fifo_line_ok() {
        let p = parse_fifo_line("%H-1234 OK;");
        assert_eq!(p.id, "H-1234");
        assert_eq!(p.words.len(), 2);
    }

    #[test]
    fn parse_fifo_line_position_done() {
        let p = parse_fifo_line("%V-5678 500 600 DONE;");
        assert_eq!(p.id, "V-5678");
        assert_eq!(p.words.len(), 4);
    }

    #[test]
    fn parse_fifo_line_no_prefix() {
        let p = parse_fifo_line("garbage data");
        assert_eq!(p.id, "");
    }

    // -- classify_fifo_response tests --

    #[test]
    fn classify_ok() {
        let p = parse_fifo_line("%H-1 OK;");
        assert_eq!(classify_fifo_response(&p), FifoResponseKind::Ok);
    }

    #[test]
    fn classify_busy() {
        let p = parse_fifo_line("%V-1 BUSY;");
        assert_eq!(classify_fifo_response(&p), FifoResponseKind::Busy);
    }

    #[test]
    fn classify_error_with_code() {
        let p = parse_fifo_line("%H-1 ERROR; 5");
        assert_eq!(
            classify_fifo_response(&p),
            FifoResponseKind::Error { code: Some(5) }
        );
    }

    #[test]
    fn classify_error_no_code() {
        let p = parse_fifo_line("%H-1 ERROR;");
        assert_eq!(
            classify_fifo_response(&p),
            FifoResponseKind::Error { code: None }
        );
    }

    #[test]
    fn classify_position_done() {
        let p = parse_fifo_line("%H-1 800 1200 DONE;");
        assert_eq!(
            classify_fifo_response(&p),
            FifoResponseKind::PositionDone {
                pos_a: 800,
                pos_b: 1200,
            }
        );
    }

    #[test]
    fn classify_position_ok() {
        let p = parse_fifo_line("%H-1 OK 800 1200 DONE;");
        assert_eq!(
            classify_fifo_response(&p),
            FifoResponseKind::PositionOk {
                pos_a: 800,
                pos_b: 1200,
            }
        );
    }

    #[test]
    fn classify_empty() {
        let p = parse_fifo_line("");
        assert_eq!(classify_fifo_response(&p), FifoResponseKind::Empty);
    }

    #[test]
    fn classify_unknown() {
        let p = parse_fifo_line("%H-1 SOMETHING WEIRD");
        assert_eq!(classify_fifo_response(&p), FifoResponseKind::Unknown);
    }

    // -- different() tests --

    #[test]
    fn different_yes() {
        assert!(different(1.0, 2.0));
    }

    #[test]
    fn different_no() {
        assert!(!different(1.0, 1.0));
        assert!(!different(1.0, 1.0 + 1e-15));
    }

    // -- BladePairTarget tests --

    #[test]
    fn blade_pair_from_blades_horizontal() {
        let bp = BladePairTarget::from_blades(1.0, 3.0, false);
        assert!((bp.gap - 4.0).abs() < 1e-9);
        assert!((bp.center - 1.0).abs() < 1e-9);
    }

    #[test]
    fn blade_pair_from_blades_vertical() {
        let bp = BladePairTarget::from_blades(4.0, 2.0, true);
        assert!((bp.gap - 6.0).abs() < 1e-9);
        assert!((bp.center - 1.0).abs() < 1e-9);
    }

    #[test]
    fn blade_pair_from_gap_center_horizontal_roundtrip() {
        let original = BladePairTarget::from_blades(1.5, 3.5, false);
        let reconstructed = BladePairTarget::from_gap_center(original.gap, original.center, false);
        assert!((reconstructed.blade_a - 1.5).abs() < 1e-9);
        assert!((reconstructed.blade_b - 3.5).abs() < 1e-9);
    }

    #[test]
    fn blade_pair_from_gap_center_vertical_roundtrip() {
        let original = BladePairTarget::from_blades(4.0, 2.0, true);
        let reconstructed = BladePairTarget::from_gap_center(original.gap, original.center, true);
        assert!((reconstructed.blade_a - 4.0).abs() < 1e-9);
        assert!((reconstructed.blade_b - 2.0).abs() < 1e-9);
    }

    // -- readback gap/center tests --

    #[test]
    fn h_readback_gap_center_symmetric() {
        let (gap, center) = h_readback_gap_center(2.5, 2.5);
        assert!((gap - 5.0).abs() < 1e-9);
        assert!((center - 0.0).abs() < 1e-9);
    }

    #[test]
    fn v_readback_gap_center_offset() {
        let (gap, center) = v_readback_gap_center(4.0, 2.0);
        assert!((gap - 6.0).abs() < 1e-9);
        assert!((center - 1.0).abs() < 1e-9);
    }

    // -- validate_h_move / validate_v_move tests --

    #[test]
    fn validate_h_move_ok() {
        let limits = AxisLimits::from_hsc_params(400, 4400);
        let result = validate_h_move(1.0, 2.0, 3.0, 0.5, &limits);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_h_move_exceeds() {
        let limits = AxisLimits::from_hsc_params(400, 4400);
        // blade_lo = -1.0, so -2.0 exceeds
        let result = validate_h_move(-2.0, 2.0, 0.0, -2.0, &limits);
        assert!(result.is_err());
    }

    #[test]
    fn validate_v_move_ok() {
        let limits = AxisLimits::from_hsc_params(400, 4400);
        let result = validate_v_move(3.0, 1.0, 4.0, 1.0, &limits);
        assert!(result.is_ok());
    }

    // -- Controller tests --

    #[test]
    fn controller_update_axis_position_h() {
        let mut ctrl = XiaSlitController::new("H-1".into(), "V-1".into());
        ctrl.h_orient = HOrient::LeftRight;
        ctrl.h_config.origin = 400;
        // raw 800 => dial 1.0, raw 1200 => dial 2.0
        ctrl.update_axis_position("H-1", 800, 1200);
        assert!((ctrl.h_readback.blade_a_rb - 1.0).abs() < 1e-9);
        assert!((ctrl.h_readback.blade_b_rb - 2.0).abs() < 1e-9);
        assert!((ctrl.h_readback.gap_rb - 3.0).abs() < 1e-9);
        assert!((ctrl.h_readback.center_rb - 0.5).abs() < 1e-9);
    }

    #[test]
    fn controller_update_axis_position_v() {
        let mut ctrl = XiaSlitController::new("H-1".into(), "V-1".into());
        ctrl.v_orient = VOrient::TopBottom;
        ctrl.v_config.origin = 400;
        ctrl.update_axis_position("V-1", 1600, 800);
        // top = (1600-400)/400 = 3.0, bottom = (800-400)/400 = 1.0
        assert!((ctrl.v_readback.blade_a_rb - 3.0).abs() < 1e-9);
        assert!((ctrl.v_readback.blade_b_rb - 1.0).abs() < 1e-9);
        assert!((ctrl.v_readback.gap_rb - 4.0).abs() < 1e-9);
        assert!((ctrl.v_readback.center_rb - 1.0).abs() < 1e-9);
    }

    #[test]
    fn controller_set_h_blades_valid() {
        let mut ctrl = XiaSlitController::new("H-1".into(), "V-1".into());
        ctrl.update_h_limits();
        assert!(ctrl.set_h_blades(1.0, 2.0));
        assert!(ctrl.h_move_pending);
        assert_eq!(ctrl.error, NO_ERROR);
    }

    #[test]
    fn controller_set_h_blades_invalid() {
        let mut ctrl = XiaSlitController::new("H-1".into(), "V-1".into());
        ctrl.update_h_limits();
        assert!(!ctrl.set_h_blades(-2.0, 2.0));
        assert_eq!(ctrl.error, ERROR_SOFT_LIMITS);
    }

    #[test]
    fn controller_set_h_gap_center() {
        let mut ctrl = XiaSlitController::new("H-1".into(), "V-1".into());
        ctrl.update_h_limits();
        assert!(ctrl.set_h_gap_center(4.0, 0.5));
        // l = 4/2 - 0.5 = 1.5, r = 4/2 + 0.5 = 2.5
        assert!((ctrl.h_target.blade_a - 1.5).abs() < 1e-9);
        assert!((ctrl.h_target.blade_b - 2.5).abs() < 1e-9);
    }

    #[test]
    fn controller_h_at_target() {
        let mut ctrl = XiaSlitController::new("H-1".into(), "V-1".into());
        ctrl.h_orient = HOrient::LeftRight;
        ctrl.h_config.origin = 400;
        ctrl.h_target.blade_a = 1.0;
        ctrl.h_target.blade_b = 2.0;
        let a = dial_to_raw(1.0, 400);
        let b = dial_to_raw(2.0, 400);
        assert!(ctrl.h_at_target(a, b));
        assert!(!ctrl.h_at_target(a + 1, b));
    }
}
