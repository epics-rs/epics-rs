//! Oxford Quad X-ray Beam Position Monitor — native Rust port of `sncqxbpm.st`.
//!
//! Implements the serial command protocol and readout state machine for the
//! Oxford 4-channel X-ray BPM (quad diode). The device measures four diode
//! currents (A, B, C, D) and computes beam position and total current.
//!
//! # Serial Protocol
//!
//! Commands use SCPI-like syntax with a device address prefix:
//! - Reset: `*RST<addr>`
//! - Set range: `:CONF<addr>:CURR:RANG <gain>`
//! - Read all currents: `:READ<addr>:CURRALL?`
//! - Set single mode: `:CONF<addr>:SINGLE`
//! - Set average mode: `:CONF<addr>:AVGCURR <buflen>`
//! - Set window mode: `:CONF<addr>:WDWCURR <buflen>`
//!
//! Terminators: output `\n`, input `\n`.
//!
//! # Position Calculation
//!
//! From the original code comments (empirically determined):
//! - X position = GX * (B - D) / (B + D)
//! - Y position = GY * (A - C) / (A + C)
//!
//! where GX, GY are geometric scaling factors (typically 4.5).
//!
//! # Diode Current
//!
//! Raw counts are converted to current via per-channel gain trim and offset:
//! - `current = gainTrim[gain][channel] * (raw - offset[gain][channel])`

use std::fmt;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Number of gain ranges.
pub const NUM_GAINS: usize = 6;

/// Number of diode channels.
pub const NUM_CHANNELS: usize = 4;

/// Delay after a gain change before reading (seconds).
pub const NEW_GAIN_DELAY: f64 = 3.0;

/// Absolute fastest sample time (measured ~50ms).
pub const BASE_SAMPLE_TIME: f64 = 0.05;

/// Default geometric scaling factor for X position.
pub const DEFAULT_GX: f64 = 4.5;

/// Default geometric scaling factor for Y position.
pub const DEFAULT_GY: f64 = 4.5;

/// Default read period (seconds).
pub const DEFAULT_PERIOD: f64 = 0.1;

/// Default low-current raw threshold.
pub const DEFAULT_LOW_CURRENT_RAW: u32 = 1000;

/// Default settling time for offset calibration (seconds).
pub const DEFAULT_SETTLING: f64 = 2.5;

/// Default buffer length for averaging/window modes.
pub const DEFAULT_BUFLEN: i32 = 30;

/// Serial response timeout.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_millis(250);

/// Reconnect interval after communication error.
pub const ERROR_RECONNECT_INTERVAL: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Error codes
// ---------------------------------------------------------------------------

pub const NO_ERROR: i32 = 0;
pub const ERROR_UNKNOWN: i32 = 1;
pub const ERROR_COMM_ERROR: i32 = 2;

// ---------------------------------------------------------------------------
// Signal (sampling) modes
// ---------------------------------------------------------------------------

/// Signal acquisition mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalMode {
    /// Single-shot readout.
    Single = 0,
    /// Averaged readout over a buffer.
    Average = 1,
    /// Window (running average) readout.
    Window = 2,
}

impl SignalMode {
    /// Parse from an integer.
    pub fn from_i32(v: i32) -> Option<Self> {
        match v {
            0 => Some(SignalMode::Single),
            1 => Some(SignalMode::Average),
            2 => Some(SignalMode::Window),
            _ => None,
        }
    }
}

impl fmt::Display for SignalMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignalMode::Single => write!(f, "SINGLE"),
            SignalMode::Average => write!(f, "AVERAGE"),
            SignalMode::Window => write!(f, "WINDOW"),
        }
    }
}

// ---------------------------------------------------------------------------
// Serial Protocol — Commands
// ---------------------------------------------------------------------------

/// QXBPM serial commands.
#[derive(Debug, Clone, PartialEq)]
pub enum QxbpmCommand {
    /// Reset the device: `*RST<addr>`
    Reset(i32),
    /// Set current range (gain): `:CONF<addr>:CURR:RANG <range>`
    SetRange(i32, i32),
    /// Read current range: `:CONF<addr>:CURR:RANG?`
    ReadRange(i32),
    /// Set single acquisition mode: `:CONF<addr>:SINGLE`
    SetSingle(i32),
    /// Set average mode with buffer length: `:CONF<addr>:AVGCURR <buflen>`
    SetAverage(i32, i32),
    /// Set window mode with buffer length: `:CONF<addr>:WDWCURR <buflen>`
    SetWindow(i32, i32),
    /// Read all four currents: `:READ<addr>:CURRALL?`
    ReadAllCurrents(i32),
    /// Read a single channel current: `:READ<addr>:CURR<chan>?`
    ReadCurrent(i32, i32),
    /// Read position: `:READ<addr>:POS<axis>?`
    ReadPosition(i32, String),
    /// Set a configuration variable: `:CONF<addr>:<var> <value>`
    SetVariable(i32, String, String),
    /// Read a configuration variable: `:CONF<addr>:<var>?`
    ReadVariable(i32, String),
    /// Read user input status: `:SENS<addr>:STAT<n>?`
    ReadUserInput(i32, i32),
    /// Read user output status: `:SOUR<addr>:STAT<n>?`
    ReadUserOutput(i32, i32),
    /// Set user output: `:SOUR<addr>:STAT<n> <value>`
    SetUserOutput(i32, i32, i32),
}

impl QxbpmCommand {
    /// Format the command as a serial string (without trailing `\n`).
    pub fn to_serial(&self) -> String {
        match self {
            QxbpmCommand::Reset(addr) => format!("*RST{addr}"),
            QxbpmCommand::SetRange(addr, range) => {
                format!(":CONF{addr}:CURR:RANG {range}")
            }
            QxbpmCommand::ReadRange(addr) => format!(":CONF{addr}:CURR:RANG?"),
            QxbpmCommand::SetSingle(addr) => format!(":CONF{addr}:SINGLE"),
            QxbpmCommand::SetAverage(addr, buflen) => {
                format!(":CONF{addr}:AVGCURR {buflen}")
            }
            QxbpmCommand::SetWindow(addr, buflen) => {
                format!(":CONF{addr}:WDWCURR {buflen}")
            }
            QxbpmCommand::ReadAllCurrents(addr) => format!(":READ{addr}:CURRALL?"),
            QxbpmCommand::ReadCurrent(addr, chan) => format!(":READ{addr}:CURR{chan}?"),
            QxbpmCommand::ReadPosition(addr, axis) => format!(":READ{addr}:POS{axis}?"),
            QxbpmCommand::SetVariable(addr, var, val) => {
                format!(":CONF{addr}:{var} {val}")
            }
            QxbpmCommand::ReadVariable(addr, var) => format!(":CONF{addr}:{var}?"),
            QxbpmCommand::ReadUserInput(addr, n) => format!(":SENS{addr}:STAT{n}?"),
            QxbpmCommand::ReadUserOutput(addr, n) => format!(":SOUR{addr}:STAT{n}?"),
            QxbpmCommand::SetUserOutput(addr, n, val) => {
                format!(":SOUR{addr}:STAT{n} {val}")
            }
        }
    }

    /// Format as bytes with trailing `\n` (output EOS).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut s = self.to_serial();
        s.push('\n');
        s.into_bytes()
    }
}

impl fmt::Display for QxbpmCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_serial())
    }
}

// ---------------------------------------------------------------------------
// Raw diode data
// ---------------------------------------------------------------------------

/// Raw unsigned counts from the four diode channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RawDiodeData {
    pub a: u32,
    pub b: u32,
    pub c: u32,
    pub d: u32,
}

/// Parse the response from `:READ<addr>:CURRALL?`.
///
/// The device returns four unsigned integers separated by whitespace.
/// The response line may have a leading character that should be skipped
/// (the original code does `strcpy(bpm_response, s_ainp+1)`).
pub fn parse_currall_response(line: &str) -> Option<RawDiodeData> {
    let line = line.trim();
    // Skip a leading non-digit character if present (response prefix)
    let data = if !line.is_empty() && !line.as_bytes()[0].is_ascii_digit() {
        &line[1..]
    } else {
        line
    };
    let parts: Vec<&str> = data.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }
    Some(RawDiodeData {
        a: parts[0].parse().ok()?,
        b: parts[1].parse().ok()?,
        c: parts[2].parse().ok()?,
        d: parts[3].parse().ok()?,
    })
}

// ---------------------------------------------------------------------------
// Gain trim and offset calibration
// ---------------------------------------------------------------------------

/// Per-gain, per-channel calibration data.
///
/// Indexed as `[gain_index * NUM_CHANNELS + channel_index]` where channel
/// index is 0=A, 1=B, 2=C, 3=D.
#[derive(Debug, Clone)]
pub struct CalibrationData {
    /// Gain trim factors: `current = gain_trim * (raw - offset)`.
    pub gain_trim: Vec<f64>,
    /// Dark current offsets (raw counts).
    pub offset: Vec<i32>,
}

impl CalibrationData {
    /// Index into the flat arrays.
    fn idx(gain: usize, channel: usize) -> usize {
        gain * NUM_CHANNELS + channel
    }

    /// Get the gain trim for a specific gain and channel.
    pub fn get_trim(&self, gain: usize, channel: usize) -> f64 {
        let i = Self::idx(gain, channel);
        if i < self.gain_trim.len() {
            self.gain_trim[i]
        } else {
            1.0
        }
    }

    /// Get the offset for a specific gain and channel.
    pub fn get_offset(&self, gain: usize, channel: usize) -> i32 {
        let i = Self::idx(gain, channel);
        if i < self.offset.len() {
            self.offset[i]
        } else {
            0
        }
    }

    /// Set the offset for a specific gain and channel.
    pub fn set_offset(&mut self, gain: usize, channel: usize, value: i32) {
        let i = Self::idx(gain, channel);
        if i < self.offset.len() {
            self.offset[i] = value;
        }
    }

    /// Set the gain trim for a specific gain and channel.
    pub fn set_trim(&mut self, gain: usize, channel: usize, value: f64) {
        let i = Self::idx(gain, channel);
        if i < self.gain_trim.len() {
            self.gain_trim[i] = value;
        }
    }
}

impl Default for CalibrationData {
    fn default() -> Self {
        Self {
            gain_trim: vec![1.0; NUM_GAINS * NUM_CHANNELS],
            offset: vec![0; NUM_GAINS * NUM_CHANNELS],
        }
    }
}

/// Compute the default gain trim factors (from the original set_defaults).
///
/// These are based on the full-scale current for each range divided by
/// 10 VDC full-scale and 100 kHz/VDC.
pub fn default_gain_trims() -> Vec<f64> {
    let full_scale = [350e-9, 700e-9, 1400e-9, 7e-6, 70e-6, 700e-6];
    let mut trims = vec![0.0; NUM_GAINS * NUM_CHANNELS];
    for gain in 0..NUM_GAINS {
        let factor = full_scale[gain] / 10.0 / 1e5;
        for ch in 0..NUM_CHANNELS {
            trims[gain * NUM_CHANNELS + ch] = factor;
        }
    }
    trims
}

/// Create a CalibrationData with default trims and zero offsets.
pub fn default_calibration() -> CalibrationData {
    CalibrationData {
        gain_trim: default_gain_trims(),
        offset: vec![0; NUM_GAINS * NUM_CHANNELS],
    }
}

// ---------------------------------------------------------------------------
// Diode current calculation (pure functions)
// ---------------------------------------------------------------------------

/// Compute the calibrated current for one channel.
pub fn diode_current(raw: u32, gain: usize, channel: usize, cal: &CalibrationData) -> f64 {
    let trim = cal.get_trim(gain, channel);
    let offset = cal.get_offset(gain, channel);
    trim * (raw as f64 - offset as f64)
}

/// Calibrated currents for all four channels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DiodeCurrents {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
}

/// Compute calibrated currents from raw data.
pub fn compute_currents(raw: &RawDiodeData, gain: usize, cal: &CalibrationData) -> DiodeCurrents {
    DiodeCurrents {
        a: diode_current(raw.a, gain, 0, cal),
        b: diode_current(raw.b, gain, 1, cal),
        c: diode_current(raw.c, gain, 2, cal),
        d: diode_current(raw.d, gain, 3, cal),
    }
}

/// Total beam intensity (sum of all four channels).
pub fn total_current(currents: &DiodeCurrents) -> f64 {
    currents.a + currents.b + currents.c + currents.d
}

// ---------------------------------------------------------------------------
// Beam position calculation (pure functions)
// ---------------------------------------------------------------------------

/// Beam position from four diode currents.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BeamPosition {
    pub x: f64,
    pub y: f64,
}

/// Compute beam position from diode currents and geometric factors.
///
/// From the original code (empirically discovered mapping):
/// - X = GX * (B - D) / (B + D)
/// - Y = GY * (A - C) / (A + C)
///
/// Returns (0, 0) if the denominator is zero to avoid division by zero.
pub fn compute_position(currents: &DiodeCurrents, gx: f64, gy: f64) -> BeamPosition {
    let bd_sum = currents.b + currents.d;
    let ac_sum = currents.a + currents.c;

    let x = if bd_sum.abs() > f64::EPSILON {
        gx * (currents.b - currents.d) / bd_sum
    } else {
        0.0
    };

    let y = if ac_sum.abs() > f64::EPSILON {
        gy * (currents.a - currents.c) / ac_sum
    } else {
        0.0
    };

    BeamPosition { x, y }
}

/// Check if all raw diode readings are below the low-current threshold.
pub fn current_is_low(raw: &RawDiodeData, threshold: u32) -> bool {
    raw.a < threshold && raw.b < threshold && raw.c < threshold && raw.d < threshold
}

/// Check if all raw diode readings are at or above the low-current threshold.
pub fn current_is_ok(raw: &RawDiodeData, threshold: u32) -> bool {
    raw.a >= threshold && raw.b >= threshold && raw.c >= threshold && raw.d >= threshold
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

/// States of the QXBPM controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QxbpmState {
    Startup,
    Init,
    Disable,
    CommError,
    Idle,
}

impl fmt::Display for QxbpmState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QxbpmState::Startup => write!(f, "startup"),
            QxbpmState::Init => write!(f, "init"),
            QxbpmState::Disable => write!(f, "disable"),
            QxbpmState::CommError => write!(f, "comm_error"),
            QxbpmState::Idle => write!(f, "idle"),
        }
    }
}

/// Full QXBPM controller state.
pub struct QxbpmController {
    pub state: QxbpmState,
    pub address: i32,
    pub enabled: bool,
    pub init_requested: bool,
    pub gain: usize,
    pub signal_mode: SignalMode,
    pub buflen: i32,
    pub buflen_lo: i32,
    pub buflen_hi: i32,
    pub period: f64,
    pub period_lo: f64,
    pub period_hi: f64,
    pub gx: f64,
    pub gy: f64,
    pub settling: f64,
    pub low_current_threshold: u32,
    pub calibration: CalibrationData,
    pub raw: RawDiodeData,
    pub currents: DiodeCurrents,
    pub position: BeamPosition,
    pub total: f64,
    pub current_low: bool,
    pub current_ok: bool,
    pub error: i32,
    pub error_msg: String,
    // Change flags
    pub gain_changed: bool,
    pub mode_changed: bool,
    pub period_changed: bool,
    pub set_offsets_requested: bool,
    pub set_defaults_requested: bool,
}

impl Default for QxbpmController {
    fn default() -> Self {
        Self {
            state: QxbpmState::Startup,
            address: 1,
            enabled: true,
            init_requested: true,
            gain: 0,
            signal_mode: SignalMode::Window,
            buflen: DEFAULT_BUFLEN,
            buflen_lo: 1,
            buflen_hi: 100,
            period: DEFAULT_PERIOD,
            period_lo: BASE_SAMPLE_TIME,
            period_hi: 60.0,
            gx: DEFAULT_GX,
            gy: DEFAULT_GY,
            settling: DEFAULT_SETTLING,
            low_current_threshold: DEFAULT_LOW_CURRENT_RAW,
            calibration: default_calibration(),
            raw: RawDiodeData::default(),
            currents: DiodeCurrents::default(),
            position: BeamPosition::default(),
            total: 0.0,
            current_low: true,
            current_ok: false,
            error: NO_ERROR,
            error_msg: String::new(),
            gain_changed: false,
            mode_changed: false,
            period_changed: false,
            set_offsets_requested: false,
            set_defaults_requested: false,
        }
    }
}

impl QxbpmController {
    /// Create a new controller with the given device address.
    pub fn new(address: i32) -> Self {
        Self {
            address,
            ..Default::default()
        }
    }

    /// Clamp the period to valid range.
    pub fn clamp_period(&mut self) {
        if self.period < self.period_lo {
            self.period = self.period_lo;
        }
        if self.period > self.period_hi {
            self.period = self.period_hi;
        }
    }

    /// Clamp the buffer length to valid range.
    pub fn clamp_buflen(&mut self) {
        if self.buflen < self.buflen_lo {
            self.buflen = self.buflen_lo;
        }
        if self.buflen > self.buflen_hi {
            self.buflen = self.buflen_hi;
        }
    }

    /// Process raw diode data: compute currents, position, and status.
    pub fn process_raw(&mut self) {
        self.currents = compute_currents(&self.raw, self.gain, &self.calibration);
        self.total = total_current(&self.currents);
        self.position = compute_position(&self.currents, self.gx, self.gy);
        self.current_low = current_is_low(&self.raw, self.low_current_threshold);
        self.current_ok = current_is_ok(&self.raw, self.low_current_threshold);
    }

    /// Set all offsets from current raw readings for the current gain.
    pub fn set_offsets_for_gain(&mut self, gain: usize) {
        self.calibration.set_offset(gain, 0, self.raw.a as i32);
        self.calibration.set_offset(gain, 1, self.raw.b as i32);
        self.calibration.set_offset(gain, 2, self.raw.c as i32);
        self.calibration.set_offset(gain, 3, self.raw.d as i32);
    }

    /// Restore default calibration values.
    pub fn set_defaults(&mut self) {
        self.signal_mode = SignalMode::Window;
        self.buflen = DEFAULT_BUFLEN;
        self.low_current_threshold = DEFAULT_LOW_CURRENT_RAW;
        self.period = DEFAULT_PERIOD;
        self.gx = DEFAULT_GX;
        self.gy = DEFAULT_GY;
        self.settling = DEFAULT_SETTLING;
        self.calibration = default_calibration();
    }

    /// Compute the command for the current signal mode.
    pub fn signal_mode_command(&self) -> QxbpmCommand {
        match self.signal_mode {
            SignalMode::Single => QxbpmCommand::SetSingle(self.address),
            SignalMode::Average => QxbpmCommand::SetAverage(self.address, self.buflen),
            SignalMode::Window => QxbpmCommand::SetWindow(self.address, self.buflen),
        }
    }
}

// ---------------------------------------------------------------------------
// Actor types
// ---------------------------------------------------------------------------

/// Configuration for the QXBPM actor.
pub struct QxbpmActorConfig {
    /// Device address (typically 1).
    pub address: i32,
    /// Initial geometric scaling for X.
    pub gx: f64,
    /// Initial geometric scaling for Y.
    pub gy: f64,
}

/// Commands that can be sent to the QXBPM actor.
#[derive(Debug, Clone)]
pub enum QxbpmActorCommand {
    Init,
    SetEnabled(bool),
    SetGain(usize),
    SetSignalMode(SignalMode),
    SetBufLen(i32),
    SetPeriod(f64),
    SetGx(f64),
    SetGy(f64),
    SetLowCurrentThreshold(u32),
    SetOffsets,
    SetDefaults,
    Shutdown,
}

/// Status published by the QXBPM actor.
#[derive(Debug, Clone)]
pub struct QxbpmActorStatus {
    pub state: QxbpmState,
    pub raw: RawDiodeData,
    pub currents: DiodeCurrents,
    pub position: BeamPosition,
    pub total: f64,
    pub current_low: bool,
    pub current_ok: bool,
    pub gain: usize,
    pub signal_mode: SignalMode,
    pub period: f64,
    pub error: i32,
    pub error_msg: String,
    pub enabled: bool,
}

// ---------------------------------------------------------------------------
// Async actor
// ---------------------------------------------------------------------------

/// Run the QXBPM actor.
///
/// `R` and `W` are the async read/write halves of the serial connection.
pub async fn run<R, W>(
    config: QxbpmActorConfig,
    reader: R,
    writer: W,
    mut cmd_rx: tokio::sync::mpsc::Receiver<QxbpmActorCommand>,
    status_tx: watch::Sender<QxbpmActorStatus>,
) where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    let mut ctrl = QxbpmController::new(config.address);
    ctrl.gx = config.gx;
    ctrl.gy = config.gy;

    let mut buf_reader = BufReader::new(reader);
    let mut writer = writer;
    let mut line_buf = String::new();

    async fn send_cmd<W2: tokio::io::AsyncWrite + Unpin>(
        writer: &mut W2,
        cmd: &QxbpmCommand,
    ) -> Result<(), std::io::Error> {
        let bytes = cmd.to_bytes();
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn send_and_read<
        R2: tokio::io::AsyncBufRead + Unpin,
        W2: tokio::io::AsyncWrite + Unpin,
    >(
        writer: &mut W2,
        reader: &mut R2,
        buf: &mut String,
        cmd: &QxbpmCommand,
    ) -> Result<String, std::io::Error> {
        send_cmd(writer, cmd).await?;
        buf.clear();
        match tokio::time::timeout(RESPONSE_TIMEOUT, reader.read_line(buf)).await {
            Ok(Ok(0)) => Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF",
            )),
            Ok(Ok(_)) => Ok(buf.clone()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout")),
        }
    }

    fn publish_status(ctrl: &QxbpmController, tx: &watch::Sender<QxbpmActorStatus>) {
        let _ = tx.send(QxbpmActorStatus {
            state: ctrl.state,
            raw: ctrl.raw,
            currents: ctrl.currents,
            position: ctrl.position,
            total: ctrl.total,
            current_low: ctrl.current_low,
            current_ok: ctrl.current_ok,
            gain: ctrl.gain,
            signal_mode: ctrl.signal_mode,
            period: ctrl.period,
            error: ctrl.error,
            error_msg: ctrl.error_msg.clone(),
            enabled: ctrl.enabled,
        });
    }

    info!("QXBPM actor starting: address={}", ctrl.address);
    ctrl.state = QxbpmState::Init;
    publish_status(&ctrl, &status_tx);

    // Extra delay after gain change
    let mut update_delay: f64 = NEW_GAIN_DELAY;

    loop {
        match ctrl.state {
            QxbpmState::Startup | QxbpmState::Init => {
                ctrl.current_low = true;
                ctrl.current_ok = false;
                ctrl.error = NO_ERROR;
                ctrl.error_msg.clear();
                ctrl.period_lo = BASE_SAMPLE_TIME;

                // Reset the device
                if let Err(e) = send_cmd(&mut writer, &QxbpmCommand::Reset(ctrl.address)).await {
                    error!("Failed to send reset: {e}");
                    ctrl.state = QxbpmState::CommError;
                    publish_status(&ctrl, &status_tx);
                    continue;
                }
                // Wait for device reset
                tokio::time::sleep(Duration::from_millis(100)).await;

                // Apply current gain
                if let Err(e) = send_cmd(
                    &mut writer,
                    &QxbpmCommand::SetRange(ctrl.address, ctrl.gain as i32 + 1),
                )
                .await
                {
                    error!("Failed to set range: {e}");
                    ctrl.state = QxbpmState::CommError;
                    publish_status(&ctrl, &status_tx);
                    continue;
                }

                // Apply current signal mode
                let mode_cmd = ctrl.signal_mode_command();
                if let Err(e) = send_cmd(&mut writer, &mode_cmd).await {
                    error!("Failed to set signal mode: {e}");
                    ctrl.state = QxbpmState::CommError;
                    publish_status(&ctrl, &status_tx);
                    continue;
                }

                update_delay = NEW_GAIN_DELAY;
                ctrl.init_requested = false;
                ctrl.state = QxbpmState::Idle;
                publish_status(&ctrl, &status_tx);
            }

            QxbpmState::Disable => {
                publish_status(&ctrl, &status_tx);
                loop {
                    match cmd_rx.recv().await {
                        Some(QxbpmActorCommand::SetEnabled(true)) => {
                            ctrl.enabled = true;
                            ctrl.init_requested = true;
                            ctrl.state = QxbpmState::Init;
                            break;
                        }
                        Some(QxbpmActorCommand::Shutdown) | None => {
                            info!("QXBPM actor shutting down");
                            return;
                        }
                        _ => {}
                    }
                }
                publish_status(&ctrl, &status_tx);
            }

            QxbpmState::CommError => {
                ctrl.error = ERROR_COMM_ERROR;
                ctrl.error_msg = "communications error".to_string();
                publish_status(&ctrl, &status_tx);

                tokio::select! {
                    _ = tokio::time::sleep(ERROR_RECONNECT_INTERVAL) => {
                        ctrl.state = QxbpmState::Init;
                    }
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(QxbpmActorCommand::Init) => {
                                ctrl.state = QxbpmState::Init;
                            }
                            Some(QxbpmActorCommand::Shutdown) | None => {
                                info!("QXBPM actor shutting down");
                                return;
                            }
                            _ => {}
                        }
                    }
                }
                publish_status(&ctrl, &status_tx);
            }

            QxbpmState::Idle => {
                if !ctrl.enabled {
                    ctrl.state = QxbpmState::Disable;
                    publish_status(&ctrl, &status_tx);
                    continue;
                }

                if ctrl.init_requested {
                    ctrl.init_requested = false;
                    ctrl.state = QxbpmState::Init;
                    publish_status(&ctrl, &status_tx);
                    continue;
                }

                // Process gain change
                if ctrl.gain_changed {
                    ctrl.gain_changed = false;
                    if let Err(e) = send_cmd(
                        &mut writer,
                        &QxbpmCommand::SetRange(ctrl.address, ctrl.gain as i32 + 1),
                    )
                    .await
                    {
                        error!("Failed to set range: {e}");
                        ctrl.state = QxbpmState::CommError;
                        publish_status(&ctrl, &status_tx);
                        continue;
                    }
                    update_delay = NEW_GAIN_DELAY;
                }

                // Process mode change
                if ctrl.mode_changed {
                    ctrl.mode_changed = false;
                    ctrl.clamp_buflen();
                    let mode_cmd = ctrl.signal_mode_command();
                    if let Err(e) = send_cmd(&mut writer, &mode_cmd).await {
                        error!("Failed to set signal mode: {e}");
                        ctrl.state = QxbpmState::CommError;
                        publish_status(&ctrl, &status_tx);
                        continue;
                    }
                }

                // Process period change
                if ctrl.period_changed {
                    ctrl.period_changed = false;
                    ctrl.clamp_period();
                }

                // Process set_defaults
                if ctrl.set_defaults_requested {
                    ctrl.set_defaults_requested = false;
                    ctrl.set_defaults();
                    // Re-apply mode after defaults
                    let mode_cmd = ctrl.signal_mode_command();
                    let _ = send_cmd(&mut writer, &mode_cmd).await;
                    publish_status(&ctrl, &status_tx);
                }

                // Process set_offsets (dark current calibration)
                if ctrl.set_offsets_requested {
                    ctrl.set_offsets_requested = false;
                    let old_gain = ctrl.gain;
                    for g in 0..NUM_GAINS {
                        // Change gain
                        ctrl.gain = g;
                        let _ = send_cmd(
                            &mut writer,
                            &QxbpmCommand::SetRange(ctrl.address, g as i32 + 1),
                        )
                        .await;
                        // Wait for settling
                        tokio::time::sleep(Duration::from_secs_f64(ctrl.settling)).await;
                        // Read current values
                        match send_and_read(
                            &mut writer,
                            &mut buf_reader,
                            &mut line_buf,
                            &QxbpmCommand::ReadAllCurrents(ctrl.address),
                        )
                        .await
                        {
                            Ok(ref line) => {
                                if let Some(raw) = parse_currall_response(line) {
                                    ctrl.raw = raw;
                                    ctrl.set_offsets_for_gain(g);
                                }
                            }
                            Err(e) => {
                                warn!("Error reading currents for offset cal: {e}");
                            }
                        }
                    }
                    // Restore original gain
                    ctrl.gain = old_gain;
                    let _ = send_cmd(
                        &mut writer,
                        &QxbpmCommand::SetRange(ctrl.address, old_gain as i32 + 1),
                    )
                    .await;
                    publish_status(&ctrl, &status_tx);
                }

                // Wait for the appropriate period, then read currents
                let read_delay = (ctrl.period - BASE_SAMPLE_TIME + update_delay).max(0.01);
                update_delay = 0.0; // Only delay extra once after gain change

                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs_f64(read_delay)) => {
                        // Read all currents
                        match send_and_read(
                            &mut writer,
                            &mut buf_reader,
                            &mut line_buf,
                            &QxbpmCommand::ReadAllCurrents(ctrl.address),
                        ).await {
                            Ok(ref line) => {
                                if let Some(raw) = parse_currall_response(line) {
                                    ctrl.raw = raw;
                                    ctrl.process_raw();
                                }
                            }
                            Err(e) => {
                                warn!("Error reading currents: {e}");
                                // Don't go to comm_error for timeout, only for real errors
                                if e.kind() != std::io::ErrorKind::TimedOut {
                                    ctrl.state = QxbpmState::CommError;
                                }
                            }
                        }
                    }
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(QxbpmActorCommand::Init) => {
                                ctrl.init_requested = true;
                            }
                            Some(QxbpmActorCommand::SetEnabled(en)) => {
                                ctrl.enabled = en;
                            }
                            Some(QxbpmActorCommand::SetGain(g)) => {
                                if g < NUM_GAINS {
                                    ctrl.gain = g;
                                    ctrl.gain_changed = true;
                                }
                            }
                            Some(QxbpmActorCommand::SetSignalMode(mode)) => {
                                ctrl.signal_mode = mode;
                                ctrl.mode_changed = true;
                            }
                            Some(QxbpmActorCommand::SetBufLen(bl)) => {
                                ctrl.buflen = bl;
                                ctrl.mode_changed = true;
                            }
                            Some(QxbpmActorCommand::SetPeriod(p)) => {
                                ctrl.period = p;
                                ctrl.period_changed = true;
                            }
                            Some(QxbpmActorCommand::SetGx(v)) => {
                                ctrl.gx = v;
                            }
                            Some(QxbpmActorCommand::SetGy(v)) => {
                                ctrl.gy = v;
                            }
                            Some(QxbpmActorCommand::SetLowCurrentThreshold(t)) => {
                                ctrl.low_current_threshold = t;
                            }
                            Some(QxbpmActorCommand::SetOffsets) => {
                                ctrl.set_offsets_requested = true;
                            }
                            Some(QxbpmActorCommand::SetDefaults) => {
                                ctrl.set_defaults_requested = true;
                            }
                            Some(QxbpmActorCommand::Shutdown) => {
                                info!("QXBPM actor shutting down");
                                return;
                            }
                            None => {
                                info!("QXBPM command channel closed");
                                return;
                            }
                        }
                    }
                }
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

    // -- Command formatting tests --

    #[test]
    fn cmd_reset() {
        assert_eq!(QxbpmCommand::Reset(1).to_serial(), "*RST1");
    }

    #[test]
    fn cmd_set_range() {
        assert_eq!(
            QxbpmCommand::SetRange(1, 3).to_serial(),
            ":CONF1:CURR:RANG 3"
        );
    }

    #[test]
    fn cmd_read_range() {
        assert_eq!(QxbpmCommand::ReadRange(1).to_serial(), ":CONF1:CURR:RANG?");
    }

    #[test]
    fn cmd_set_single() {
        assert_eq!(QxbpmCommand::SetSingle(1).to_serial(), ":CONF1:SINGLE");
    }

    #[test]
    fn cmd_set_average() {
        assert_eq!(
            QxbpmCommand::SetAverage(1, 30).to_serial(),
            ":CONF1:AVGCURR 30"
        );
    }

    #[test]
    fn cmd_set_window() {
        assert_eq!(
            QxbpmCommand::SetWindow(1, 30).to_serial(),
            ":CONF1:WDWCURR 30"
        );
    }

    #[test]
    fn cmd_read_all_currents() {
        assert_eq!(
            QxbpmCommand::ReadAllCurrents(1).to_serial(),
            ":READ1:CURRALL?"
        );
    }

    #[test]
    fn cmd_read_current() {
        assert_eq!(QxbpmCommand::ReadCurrent(1, 2).to_serial(), ":READ1:CURR2?");
    }

    #[test]
    fn cmd_set_variable() {
        assert_eq!(
            QxbpmCommand::SetVariable(1, "GX".into(), "4.5".into()).to_serial(),
            ":CONF1:GX 4.5"
        );
    }

    #[test]
    fn cmd_read_variable() {
        assert_eq!(
            QxbpmCommand::ReadVariable(1, "GX".into()).to_serial(),
            ":CONF1:GX?"
        );
    }

    #[test]
    fn cmd_to_bytes_has_newline() {
        let bytes = QxbpmCommand::Reset(1).to_bytes();
        assert_eq!(bytes, b"*RST1\n");
    }

    #[test]
    fn cmd_user_io() {
        assert_eq!(
            QxbpmCommand::ReadUserInput(1, 2).to_serial(),
            ":SENS1:STAT2?"
        );
        assert_eq!(
            QxbpmCommand::SetUserOutput(1, 3, 1).to_serial(),
            ":SOUR1:STAT3 1"
        );
    }

    // -- Response parsing tests --

    #[test]
    fn parse_currall_four_values() {
        let raw = parse_currall_response(" 1000 2000 3000 4000").unwrap();
        assert_eq!(raw.a, 1000);
        assert_eq!(raw.b, 2000);
        assert_eq!(raw.c, 3000);
        assert_eq!(raw.d, 4000);
    }

    #[test]
    fn parse_currall_with_prefix_char() {
        // Original code does s_ainp+1 to skip a leading char
        let raw = parse_currall_response(">100 200 300 400").unwrap();
        assert_eq!(raw.a, 100);
        assert_eq!(raw.b, 200);
        assert_eq!(raw.c, 300);
        assert_eq!(raw.d, 400);
    }

    #[test]
    fn parse_currall_plain() {
        let raw = parse_currall_response("1000 2000 3000 4000").unwrap();
        assert_eq!(raw.a, 1000);
        assert_eq!(raw.b, 2000);
    }

    #[test]
    fn parse_currall_too_few() {
        assert!(parse_currall_response("1000 2000 3000").is_none());
    }

    #[test]
    fn parse_currall_empty() {
        assert!(parse_currall_response("").is_none());
    }

    // -- Calibration tests --

    #[test]
    fn calibration_default_trims_nonzero() {
        let cal = default_calibration();
        for g in 0..NUM_GAINS {
            for ch in 0..NUM_CHANNELS {
                let trim = cal.get_trim(g, ch);
                assert!(trim > 0.0, "trim[{g}][{ch}] should be > 0, got {trim}");
            }
        }
    }

    #[test]
    fn calibration_offsets_default_zero() {
        let cal = default_calibration();
        for g in 0..NUM_GAINS {
            for ch in 0..NUM_CHANNELS {
                assert_eq!(cal.get_offset(g, ch), 0);
            }
        }
    }

    #[test]
    fn calibration_set_and_get() {
        let mut cal = CalibrationData::default();
        cal.set_trim(2, 1, 0.42);
        cal.set_offset(2, 1, 500);
        assert!((cal.get_trim(2, 1) - 0.42).abs() < 1e-15);
        assert_eq!(cal.get_offset(2, 1), 500);
    }

    #[test]
    fn calibration_out_of_range() {
        let cal = CalibrationData::default();
        // Should return defaults for out-of-range
        assert!((cal.get_trim(99, 0) - 1.0).abs() < 1e-15);
        assert_eq!(cal.get_offset(99, 0), 0);
    }

    // -- Diode current calculation tests --

    #[test]
    fn diode_current_no_offset() {
        let cal = CalibrationData {
            gain_trim: vec![2.0; NUM_GAINS * NUM_CHANNELS],
            offset: vec![0; NUM_GAINS * NUM_CHANNELS],
        };
        // current = 2.0 * (1000 - 0) = 2000.0
        assert!((diode_current(1000, 0, 0, &cal) - 2000.0).abs() < 1e-9);
    }

    #[test]
    fn diode_current_with_offset() {
        let mut cal = CalibrationData {
            gain_trim: vec![1.0; NUM_GAINS * NUM_CHANNELS],
            offset: vec![0; NUM_GAINS * NUM_CHANNELS],
        };
        cal.set_offset(0, 0, 100);
        // current = 1.0 * (500 - 100) = 400.0
        assert!((diode_current(500, 0, 0, &cal) - 400.0).abs() < 1e-9);
    }

    #[test]
    fn compute_currents_all() {
        let cal = CalibrationData {
            gain_trim: vec![1.0; NUM_GAINS * NUM_CHANNELS],
            offset: vec![0; NUM_GAINS * NUM_CHANNELS],
        };
        let raw = RawDiodeData {
            a: 100,
            b: 200,
            c: 300,
            d: 400,
        };
        let c = compute_currents(&raw, 0, &cal);
        assert!((c.a - 100.0).abs() < 1e-9);
        assert!((c.b - 200.0).abs() < 1e-9);
        assert!((c.c - 300.0).abs() < 1e-9);
        assert!((c.d - 400.0).abs() < 1e-9);
    }

    #[test]
    fn total_current_sum() {
        let c = DiodeCurrents {
            a: 1.0,
            b: 2.0,
            c: 3.0,
            d: 4.0,
        };
        assert!((total_current(&c) - 10.0).abs() < 1e-9);
    }

    // -- Beam position tests --

    #[test]
    fn position_centered() {
        let c = DiodeCurrents {
            a: 1.0,
            b: 1.0,
            c: 1.0,
            d: 1.0,
        };
        let pos = compute_position(&c, 4.5, 4.5);
        assert!((pos.x - 0.0).abs() < 1e-9);
        assert!((pos.y - 0.0).abs() < 1e-9);
    }

    #[test]
    fn position_x_offset() {
        // X = GX * (B-D)/(B+D)
        let c = DiodeCurrents {
            a: 1.0,
            b: 3.0,
            c: 1.0,
            d: 1.0,
        };
        let pos = compute_position(&c, 4.5, 4.5);
        // x = 4.5 * (3-1)/(3+1) = 4.5 * 0.5 = 2.25
        assert!((pos.x - 2.25).abs() < 1e-9);
        assert!((pos.y - 0.0).abs() < 1e-9);
    }

    #[test]
    fn position_y_offset() {
        // Y = GY * (A-C)/(A+C)
        let c = DiodeCurrents {
            a: 4.0,
            b: 1.0,
            c: 2.0,
            d: 1.0,
        };
        let pos = compute_position(&c, 4.5, 4.5);
        // y = 4.5 * (4-2)/(4+2) = 4.5 * 1/3 = 1.5
        assert!((pos.y - 1.5).abs() < 1e-9);
    }

    #[test]
    fn position_zero_denominator() {
        let c = DiodeCurrents {
            a: 0.0,
            b: 0.0,
            c: 0.0,
            d: 0.0,
        };
        let pos = compute_position(&c, 4.5, 4.5);
        assert!((pos.x - 0.0).abs() < 1e-9);
        assert!((pos.y - 0.0).abs() < 1e-9);
    }

    #[test]
    fn position_negative_x() {
        // B < D => x < 0
        let c = DiodeCurrents {
            a: 1.0,
            b: 1.0,
            c: 1.0,
            d: 3.0,
        };
        let pos = compute_position(&c, 4.5, 4.5);
        // x = 4.5 * (1-3)/(1+3) = 4.5 * (-0.5) = -2.25
        assert!((pos.x - (-2.25)).abs() < 1e-9);
    }

    // -- Current threshold tests --

    #[test]
    fn current_low_all_below() {
        let raw = RawDiodeData {
            a: 100,
            b: 200,
            c: 300,
            d: 400,
        };
        assert!(current_is_low(&raw, 1000));
        assert!(!current_is_ok(&raw, 1000));
    }

    #[test]
    fn current_ok_all_above() {
        let raw = RawDiodeData {
            a: 1000,
            b: 2000,
            c: 3000,
            d: 4000,
        };
        assert!(!current_is_low(&raw, 1000));
        assert!(current_is_ok(&raw, 1000));
    }

    #[test]
    fn current_mixed() {
        let raw = RawDiodeData {
            a: 500,
            b: 2000,
            c: 3000,
            d: 4000,
        };
        // a < 1000 but others >= 1000: neither all low nor all ok
        assert!(!current_is_low(&raw, 1000));
        assert!(!current_is_ok(&raw, 1000));
    }

    // -- Signal mode tests --

    #[test]
    fn signal_mode_from_i32() {
        assert_eq!(SignalMode::from_i32(0), Some(SignalMode::Single));
        assert_eq!(SignalMode::from_i32(1), Some(SignalMode::Average));
        assert_eq!(SignalMode::from_i32(2), Some(SignalMode::Window));
        assert_eq!(SignalMode::from_i32(3), None);
    }

    #[test]
    fn signal_mode_display() {
        assert_eq!(format!("{}", SignalMode::Single), "SINGLE");
        assert_eq!(format!("{}", SignalMode::Average), "AVERAGE");
        assert_eq!(format!("{}", SignalMode::Window), "WINDOW");
    }

    // -- Controller tests --

    #[test]
    fn controller_clamp_period() {
        let mut ctrl = QxbpmController::default();
        ctrl.period = 0.01;
        ctrl.clamp_period();
        assert!((ctrl.period - BASE_SAMPLE_TIME).abs() < 1e-9);

        ctrl.period = 999.0;
        ctrl.clamp_period();
        assert!((ctrl.period - ctrl.period_hi).abs() < 1e-9);
    }

    #[test]
    fn controller_clamp_buflen() {
        let mut ctrl = QxbpmController::default();
        ctrl.buflen = -5;
        ctrl.clamp_buflen();
        assert_eq!(ctrl.buflen, ctrl.buflen_lo);

        ctrl.buflen = 999;
        ctrl.clamp_buflen();
        assert_eq!(ctrl.buflen, ctrl.buflen_hi);
    }

    #[test]
    fn controller_process_raw() {
        let mut ctrl = QxbpmController::default();
        ctrl.raw = RawDiodeData {
            a: 1000,
            b: 2000,
            c: 3000,
            d: 4000,
        };
        ctrl.process_raw();
        assert!(ctrl.total > 0.0);
    }

    #[test]
    fn controller_set_offsets_for_gain() {
        let mut ctrl = QxbpmController::default();
        ctrl.raw = RawDiodeData {
            a: 10,
            b: 20,
            c: 30,
            d: 40,
        };
        ctrl.set_offsets_for_gain(0);
        assert_eq!(ctrl.calibration.get_offset(0, 0), 10);
        assert_eq!(ctrl.calibration.get_offset(0, 1), 20);
        assert_eq!(ctrl.calibration.get_offset(0, 2), 30);
        assert_eq!(ctrl.calibration.get_offset(0, 3), 40);
    }

    #[test]
    fn controller_set_defaults() {
        let mut ctrl = QxbpmController::default();
        ctrl.gain = 5;
        ctrl.period = 99.0;
        ctrl.set_defaults();
        assert_eq!(ctrl.signal_mode, SignalMode::Window);
        assert_eq!(ctrl.buflen, DEFAULT_BUFLEN);
        assert!((ctrl.period - DEFAULT_PERIOD).abs() < 1e-9);
        assert!((ctrl.gx - DEFAULT_GX).abs() < 1e-9);
    }

    #[test]
    fn controller_signal_mode_command() {
        let mut ctrl = QxbpmController::new(1);

        ctrl.signal_mode = SignalMode::Single;
        assert_eq!(ctrl.signal_mode_command().to_serial(), ":CONF1:SINGLE");

        ctrl.signal_mode = SignalMode::Average;
        ctrl.buflen = 20;
        assert_eq!(ctrl.signal_mode_command().to_serial(), ":CONF1:AVGCURR 20");

        ctrl.signal_mode = SignalMode::Window;
        ctrl.buflen = 30;
        assert_eq!(ctrl.signal_mode_command().to_serial(), ":CONF1:WDWCURR 30");
    }

    // -- Default gain trim values test --

    #[test]
    fn default_gain_trims_values() {
        let trims = default_gain_trims();
        assert_eq!(trims.len(), NUM_GAINS * NUM_CHANNELS);
        // Gain 0: 350e-9 / 10 / 1e5 = 3.5e-13
        let expected_g0 = 350e-9 / 10.0 / 1e5;
        assert!((trims[0] - expected_g0).abs() < 1e-20);
        // Gain 5: 700e-6 / 10 / 1e5 = 7e-10
        let expected_g5 = 700e-6 / 10.0 / 1e5;
        assert!((trims[5 * NUM_CHANNELS] - expected_g5).abs() < 1e-20);
    }
}
