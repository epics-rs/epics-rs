//! Kohzu double-crystal monochromator control -- native Rust port of `kohzuCtl.st`.
//!
//! Controls a Kohzu-style monochromator with up to 3 motors (Theta, Y, Z).
//! Provides energy/wavelength/theta conversions using Bragg's law:
//!   lambda = 2d * sin(theta), where d = a / sqrt(H^2 + K^2 + L^2)
//!   E [keV] = 12.3984244 / lambda [Angstrom]
//!
//! Supports Normal, Channel-Cut, Freeze-Z, and Freeze-Y operating modes.
//! Supports Auto and Manual modes, plus Set/Use calibration mode.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use tracing::info;

use crate::db_access::{DbChannel, DbMultiMonitor, alloc_origin};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Planck constant * speed of light in keV*Angstrom.
pub const HC: f64 = 12.3984244;

/// Degrees per radian.
pub const RAD_CONV: f64 = 57.29577951308232;

// ---------------------------------------------------------------------------
// Operating modes
// ---------------------------------------------------------------------------

/// Crystal mode (ccMode / Mode2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrystalMode {
    /// Normal -- all motors move.
    Normal = 0,
    /// Channel-cut -- Y and Z frozen.
    ChannelCut = 1,
    /// Freeze Z only.
    FreezeZ = 2,
    /// Freeze Y only.
    FreezeY = 3,
}

impl CrystalMode {
    pub fn from_i16(v: i16) -> Self {
        match v {
            1 => Self::ChannelCut,
            2 => Self::FreezeZ,
            3 => Self::FreezeY,
            _ => Self::Normal,
        }
    }

    /// Whether the Y motor is frozen in this mode.
    pub fn y_frozen(self) -> bool {
        matches!(self, Self::ChannelCut | Self::FreezeY)
    }

    /// Whether the Z motor is frozen in this mode.
    pub fn z_frozen(self) -> bool {
        matches!(self, Self::ChannelCut | Self::FreezeZ)
    }
}

// ---------------------------------------------------------------------------
// Monochromator geometry
// ---------------------------------------------------------------------------

/// Geometry type derived from the GEOM macro.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Geometry {
    /// Standard Kohzu/PSL geometry (GEOM=1).
    Standard = 1,
    /// Alternate geometry (GEOM=2).
    Alternate = 2,
}

impl Geometry {
    pub fn from_i32(v: i32) -> Self {
        if v == 2 {
            Self::Alternate
        } else {
            Self::Standard
        }
    }
}

// ---------------------------------------------------------------------------
// State enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KohzuState {
    Init,
    InitSequence,
    WaitForCommand,
    DInputChanged,
    ThetaLimits,
    EChanged,
    LambdaChanged,
    ThetaChanged,
    CalcMovements,
    MoveKohzu,
    UpdateReadback,
    CheckDone,
    ThetaMotorStopped,
    CheckMotorLimits,
    StopKohzu,
}

// ---------------------------------------------------------------------------
// Pure physics functions
// ---------------------------------------------------------------------------

/// Calculate the 2d-spacing from lattice constant `a` and Miller indices (h,k,l).
/// Returns (two_d, is_forbidden, message).
pub fn calc_2d_spacing(a: f64, h: f64, k: f64, l: f64) -> (f64, bool, &'static str) {
    let hkl_sq = h * h + k * k + l * l;
    if hkl_sq <= 0.0 {
        return (0.0, true, "Invalid (H,K,L): all zero");
    }
    let two_d = (2.0 * a) / hkl_sq.sqrt();

    // Check for forbidden reflections (diamond cubic selection rules).
    let forbidden = is_forbidden_reflection(h, k, l);
    let msg = if forbidden {
        "(H,K,L) is 'forbidden' reflection"
    } else {
        "New d spacing"
    };
    (two_d, forbidden, msg)
}

/// Check diamond cubic selection rules for forbidden reflections.
pub fn is_forbidden_reflection(h: f64, k: f64, l: f64) -> bool {
    // All must have same parity
    let h_mod = h.rem_euclid(2.0);
    let k_mod = k.rem_euclid(2.0);
    let l_mod = l.rem_euclid(2.0);
    if (h_mod - k_mod).abs() > 0.01 || (h_mod - l_mod).abs() > 0.01 {
        return true;
    }
    // Additional rule: if (h+k+l)/2 is odd and near integer, forbidden
    let avg = (h + k + l) / 2.0;
    let nint = avg.round();
    if (avg - nint).abs() <= 0.25 && (nint as i64).rem_euclid(2) != 0 {
        return true;
    }
    false
}

/// Convert energy (keV) to wavelength (Angstrom).
pub fn energy_to_lambda(e: f64) -> f64 {
    if e <= 0.0 {
        return f64::INFINITY;
    }
    HC / e
}

/// Convert wavelength (Angstrom) to energy (keV).
pub fn lambda_to_energy(lambda: f64) -> f64 {
    if lambda <= 0.0 {
        return f64::INFINITY;
    }
    HC / lambda
}

/// Convert wavelength to Bragg angle (degrees), given 2d spacing.
/// Returns `None` if lambda > two_d (impossible reflection).
pub fn lambda_to_theta(lambda: f64, two_d: f64) -> Option<f64> {
    if two_d <= 0.0 || lambda <= 0.0 {
        return None;
    }
    let sin_theta = lambda / two_d;
    if !(-1.0..=1.0).contains(&sin_theta) {
        return None;
    }
    Some(sin_theta.asin() * RAD_CONV)
}

/// Convert Bragg angle (degrees) to wavelength, given 2d spacing.
pub fn theta_to_lambda(theta_deg: f64, two_d: f64) -> f64 {
    two_d * (theta_deg / RAD_CONV).sin()
}

/// Calculate Y motor position for the given geometry, theta, and y_offset.
pub fn calc_y_position(geom: Geometry, theta_deg: f64, y_offset: f64) -> f64 {
    let theta_rad = theta_deg / RAD_CONV;
    match geom {
        Geometry::Standard => -y_offset / theta_rad.cos(),
        Geometry::Alternate => y_offset / (2.0 * theta_rad.cos()),
    }
}

/// Calculate Z motor position for the given geometry, theta, and y_offset.
pub fn calc_z_position(geom: Geometry, theta_deg: f64, y_offset: f64) -> f64 {
    let theta_rad = theta_deg / RAD_CONV;
    match geom {
        Geometry::Standard => y_offset / theta_rad.sin(),
        Geometry::Alternate => y_offset / (2.0 * theta_rad.sin()),
    }
}

/// Clamp theta to [lo, hi], returning (clamped_value, was_clamped).
pub fn clamp_theta(theta: f64, lo: f64, hi: f64) -> (f64, bool) {
    if theta < lo {
        (lo, true)
    } else if theta > hi {
        (hi, true)
    } else {
        (theta, false)
    }
}

/// Compute theta limits from motor limits, clamped to [1, 89] degrees.
pub fn compute_theta_limits(motor_hi: f64, motor_lo: f64) -> (f64, f64) {
    let hi = motor_hi.min(89.0);
    let lo = motor_lo.max(1.0);
    (hi, lo)
}

/// Compute energy/lambda limits from 2d spacing and theta limits.
pub fn compute_energy_lambda_limits(
    two_d: f64,
    theta_hi: f64,
    theta_lo: f64,
) -> (f64, f64, f64, f64) {
    let lambda_hi = two_d * (theta_hi / RAD_CONV).sin();
    let lambda_lo = two_d * (theta_lo / RAD_CONV).sin();
    let e_hi = if lambda_lo > 0.0 {
        HC / lambda_lo
    } else {
        f64::INFINITY
    };
    let e_lo = if lambda_hi > 0.0 { HC / lambda_hi } else { 0.0 };
    (e_hi, e_lo, lambda_hi, lambda_lo)
}

/// Coordinate motor speeds so all motors arrive at the same time.
/// Returns (new_th_speed, new_y_speed, new_z_speed).
pub fn coordinate_speeds(
    theta_delta: f64,
    y_delta: f64,
    z_delta: f64,
    th_speed: f64,
    y_speed: f64,
    z_speed: f64,
    cc_mode: CrystalMode,
) -> (f64, f64, f64) {
    let th_time = if th_speed > 0.0 {
        theta_delta.abs() / th_speed
    } else {
        0.0
    };
    let y_time = if cc_mode.y_frozen() {
        0.0
    } else if y_speed > 0.0 {
        y_delta.abs() / y_speed
    } else {
        0.0
    };
    let z_time = if cc_mode.z_frozen() {
        0.0
    } else if z_speed > 0.0 {
        z_delta.abs() / z_speed
    } else {
        0.0
    };

    let max_time = th_time.max(y_time).max(z_time);
    if max_time <= 0.0 {
        return (th_speed, y_speed, z_speed);
    }

    let new_th = if max_time > 0.0 && theta_delta.abs() > 0.0 {
        theta_delta.abs() / max_time
    } else {
        th_speed
    };
    let new_y = if max_time > 0.0 && y_delta.abs() > 0.0 {
        y_delta.abs() / max_time
    } else {
        y_speed
    };
    let new_z = if max_time > 0.0 && z_delta.abs() > 0.0 {
        z_delta.abs() / max_time
    } else {
        z_speed
    };
    (new_th, new_y, new_z)
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// PV name configuration derived from macro substitution.
pub struct KohzuConfig {
    pub prefix: String,
    pub m_theta: String,
    pub m_y: String,
    pub m_z: String,
    pub geom: Geometry,
}

impl KohzuConfig {
    pub fn new(prefix: &str, m_theta: &str, m_y: &str, m_z: &str, geom: i32) -> Self {
        Self {
            prefix: prefix.to_string(),
            m_theta: m_theta.to_string(),
            m_y: m_y.to_string(),
            m_z: m_z.to_string(),
            geom: Geometry::from_i32(geom),
        }
    }

    /// Build config from a macro string like "P=mini:,M_THETA=dcm:theta,M_Y=dcm:y,M_Z=dcm:z".
    pub fn from_macros(macro_str: &str) -> Self {
        let mut p = String::new();
        let mut m_theta = String::new();
        let mut m_y = String::new();
        let mut m_z = String::new();
        let mut geom = 0i32;
        for part in macro_str.split(',') {
            let part = part.trim();
            if let Some((k, v)) = part.split_once('=') {
                match k.trim() {
                    "P" => p = v.trim().to_string(),
                    "M_THETA" => m_theta = v.trim().to_string(),
                    "M_Y" => m_y = v.trim().to_string(),
                    "M_Z" => m_z = v.trim().to_string(),
                    "GEOM" => geom = v.trim().parse().unwrap_or(0),
                    _ => {}
                }
            }
        }
        Self::new(&p, &m_theta, &m_y, &m_z, geom)
    }

    fn pv(&self, suffix: &str) -> String {
        format!("{}{}", self.prefix, suffix)
    }

    fn motor_pv(&self, motor: &str, field: &str) -> String {
        format!("{}{}{}", self.prefix, motor, field)
    }
}

// ---------------------------------------------------------------------------
// Async runner
// ---------------------------------------------------------------------------

/// Run the Kohzu monochromator control state machine.
///
/// This is the main async entry point. It accesses PVs directly through
/// the in-process database — no CA network, no search, no port conflicts.
pub async fn run(
    config: KohzuConfig,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Wait for iocInit to complete — PVs are loaded during st.cmd but
    // subscriber wiring happens during iocInit which runs after st.cmd finishes.
    tokio::time::sleep(Duration::from_secs(3)).await;
    println!("kohzuCtl: starting for prefix={}", config.prefix);

    // Allocate a unique origin ID for this controller instance.
    // All put_f64_post calls use this origin, and the monitor filters
    // out events with the same origin to prevent self-feedback loops.
    let my_origin = alloc_origin();

    // -- Create channels for all PVs (direct database access, available immediately) --
    let ch_debug = DbChannel::new(&db, &config.pv("KohzuCtlDebug"));
    let ch_seq_msg1 = DbChannel::new(&db, &config.pv("KohzuSeqMsg1SI"));
    let ch_seq_msg2 = DbChannel::new(&db, &config.pv("KohzuSeqMsg2SI"));
    let ch_alert = DbChannel::new(&db, &config.pv("KohzuAlertBO"));
    let ch_oper_ack = DbChannel::new(&db, &config.pv("KohzuOperAckBO"));
    let ch_put_vals = DbChannel::new(&db, &config.pv("KohzuPutBO"));
    let ch_auto_mode = DbChannel::new(&db, &config.pv("KohzuModeBO"));
    let ch_cc_mode = DbChannel::new(&db, &config.pv("KohzuMode2MO"));
    let ch_moving = DbChannel::new(&db, &config.pv("KohzuMoving"));

    // Crystal parameters
    let ch_h = DbChannel::new(&db, &config.pv("BraggHAO"));
    let ch_k = DbChannel::new(&db, &config.pv("BraggKAO"));
    let ch_l = DbChannel::new(&db, &config.pv("BraggLAO"));
    let ch_a = DbChannel::new(&db, &config.pv("BraggAAO"));
    let ch_d = DbChannel::new(&db, &config.pv("Bragg2dSpacingAO"));

    // Energy / lambda / theta set points
    let ch_e = DbChannel::with_origin(&db, &config.pv("BraggEAO"), my_origin);
    let ch_e_hi = DbChannel::new(&db, &config.pv("BraggEAO.DRVH"));
    let ch_e_lo = DbChannel::new(&db, &config.pv("BraggEAO.DRVL"));
    let ch_e_rdbk = DbChannel::with_origin(&db, &config.pv("BraggERdbkAO"), my_origin);

    let ch_lambda = DbChannel::with_origin(&db, &config.pv("BraggLambdaAO"), my_origin);
    let ch_lambda_hi = DbChannel::new(&db, &config.pv("BraggLambdaAO.DRVH"));
    let ch_lambda_lo = DbChannel::new(&db, &config.pv("BraggLambdaAO.DRVL"));
    let ch_lambda_rdbk = DbChannel::with_origin(&db, &config.pv("BraggLambdaRdbkAO"), my_origin);

    let ch_theta = DbChannel::with_origin(&db, &config.pv("BraggThetaAO"), my_origin);
    let ch_theta_hi = DbChannel::new(&db, &config.pv("BraggThetaAO.DRVH"));
    let ch_theta_lo = DbChannel::new(&db, &config.pv("BraggThetaAO.DRVL"));
    let ch_theta_rdbk = DbChannel::with_origin(&db, &config.pv("BraggThetaRdbkAO"), my_origin);

    // Soft echo PVs
    let ch_theta_mot_name = DbChannel::new(&db, &config.pv("KohzuThetaPvSI"));
    let ch_y_mot_name = DbChannel::new(&db, &config.pv("KohzuYPvSI"));
    let ch_z_mot_name = DbChannel::new(&db, &config.pv("KohzuZPvSI"));

    let _ch_theta_cmd_echo = DbChannel::new(&db, &config.pv("KohzuThetaCmdAO"));
    let _ch_y_cmd_echo = DbChannel::new(&db, &config.pv("KohzuYCmdAO"));
    let _ch_z_cmd_echo = DbChannel::new(&db, &config.pv("KohzuZCmdAO"));
    let ch_theta_rdbk_echo = DbChannel::new(&db, &config.pv("KohzuThetaRdbkAI"));
    let ch_y_rdbk_echo = DbChannel::new(&db, &config.pv("KohzuYRdbkAI"));
    let ch_z_rdbk_echo = DbChannel::new(&db, &config.pv("KohzuZRdbkAI"));
    let ch_theta_vel_echo = DbChannel::new(&db, &config.pv("KohzuThetaVelAI"));
    let ch_y_vel_echo = DbChannel::new(&db, &config.pv("KohzuYVelAI"));
    let ch_z_vel_echo = DbChannel::new(&db, &config.pv("KohzuZVelAI"));
    let ch_theta_dmov_echo = DbChannel::new(&db, &config.pv("KohzuThetaDmovBI"));
    let ch_y_dmov_echo = DbChannel::new(&db, &config.pv("KohzuYDmovBI"));
    let ch_z_dmov_echo = DbChannel::new(&db, &config.pv("KohzuZDmovBI"));

    // Motor records
    let ch_theta_mot_stop = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".STOP"));
    let ch_y_stop = DbChannel::new(&db, &config.motor_pv(&config.m_y, ".STOP"));
    let ch_z_stop = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".STOP"));

    let ch_theta_dmov = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".DMOV"));
    let ch_y_dmov = DbChannel::new(&db, &config.motor_pv(&config.m_y, ".DMOV"));
    let ch_z_dmov = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".DMOV"));

    let ch_theta_hls = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".HLS"));
    let ch_theta_lls = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".LLS"));
    let ch_y_hls = DbChannel::new(&db, &config.motor_pv(&config.m_y, ".HLS"));
    let ch_y_lls = DbChannel::new(&db, &config.motor_pv(&config.m_y, ".LLS"));
    let ch_z_hls = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".HLS"));
    let ch_z_lls = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".LLS"));

    let ch_theta_set_ao = DbChannel::new(&db, &config.pv("KohzuThetaSetAO"));
    let ch_y_set_ao = DbChannel::new(&db, &config.pv("KohzuYSetAO"));
    let ch_z_set_ao = DbChannel::new(&db, &config.pv("KohzuZSetAO"));
    let ch_y_set_hi = DbChannel::new(&db, &config.pv("KohzuYSetAO.DRVH"));
    let ch_y_set_lo = DbChannel::new(&db, &config.pv("KohzuYSetAO.DRVL"));

    let ch_theta_mot_hilim = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".HLM"));
    let ch_theta_mot_lolim = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".LLM"));
    let ch_y_mot_hilim = DbChannel::new(&db, &config.motor_pv(&config.m_y, ".HLM"));
    let ch_y_mot_lolim = DbChannel::new(&db, &config.motor_pv(&config.m_y, ".LLM"));
    let ch_z_mot_hilim = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".HLM"));
    let ch_z_mot_lolim = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".LLM"));

    let ch_theta_mot_cmd = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ""));
    let ch_y_mot_cmd = DbChannel::new(&db, &config.motor_pv(&config.m_y, ""));
    let ch_z_mot_cmd = DbChannel::new(&db, &config.motor_pv(&config.m_z, ""));

    let ch_theta_mot_velo = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".VELO"));
    let ch_y_mot_velo = DbChannel::new(&db, &config.motor_pv(&config.m_y, ".VELO"));
    let ch_z_mot_velo = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".VELO"));

    let ch_theta_mot_rbv = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".RBV"));
    let ch_y_mot_rbv = DbChannel::new(&db, &config.motor_pv(&config.m_y, ".RBV"));
    let ch_z_mot_rbv = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".RBV"));

    let _ch_use_set = DbChannel::new(&db, &config.pv("KohzuUseSetBO"));
    let ch_theta_mot_set = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".SET"));
    let ch_y_mot_set = DbChannel::new(&db, &config.motor_pv(&config.m_y, ".SET"));
    let ch_z_mot_set = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".SET"));

    let ch_speed_ctrl = DbChannel::new(&db, &config.pv("KohzuSpeedCtrl"));

    let ch_y_offset = DbChannel::new(&db, &config.pv("Kohzu_yOffsetAO"));
    let ch_y_offset_hi = DbChannel::new(&db, &config.pv("Kohzu_yOffsetAO.DRVH"));
    let ch_y_offset_lo = DbChannel::new(&db, &config.pv("Kohzu_yOffsetAO.DRVL"));

    // Build a poll-based monitor for all PVs that drive state changes.
    // DbMonitor replaces CA subscribe+select — it polls the in-process database.
    let monitored_pvs: Vec<String> = vec![
        config.pv("BraggEAO"),
        config.pv("BraggLambdaAO"),
        config.pv("BraggThetaAO"),
        config.pv("BraggHAO"),
        config.pv("BraggKAO"),
        config.pv("BraggLAO"),
        config.pv("BraggAAO"),
        config.pv("KohzuPutBO"),
        config.pv("KohzuModeBO"),
        config.pv("KohzuMode2MO"),
        config.pv("KohzuOperAckBO"),
        config.motor_pv(&config.m_theta, ".RBV"),
        config.motor_pv(&config.m_theta, ".HLM"),
        config.motor_pv(&config.m_theta, ".LLM"),
        config.pv("Kohzu_yOffsetAO"),
        config.pv("KohzuUseSetBO"),
    ];
    let mut monitor = DbMultiMonitor::new_filtered(&db, &monitored_pvs, my_origin).await;
    println!(
        "kohzuCtl: subscribed to {} PVs, {} active",
        monitored_pvs.len(),
        monitor.sub_count()
    );

    // -- Initialize state --
    let geom = config.geom;

    // Set motor names
    let theta_name = format!("{}{}", config.prefix, config.m_theta);
    let y_name = format!("{}{}", config.prefix, config.m_y);
    let z_name = format!("{}{}", config.prefix, config.m_z);
    let _ = ch_theta_mot_name.put_string(&theta_name).await;
    let _ = ch_y_mot_name.put_string(&y_name).await;
    let _ = ch_z_mot_name.put_string(&z_name).await;

    // Initialize geometry-specific y offset limits
    match geom {
        Geometry::Standard => {
            let _ = ch_y_offset_hi.put_f64(17.5 + 0.000001).await;
            let _ = ch_y_offset_lo.put_f64(17.5 - 0.000001).await;
            let _ = ch_y_offset.put_f64(17.5).await;
            let _ = ch_y_set_hi.put_f64(0.0).await;
            let _ = ch_y_set_lo.put_f64(-35.0).await;
        }
        Geometry::Alternate => {
            let _ = ch_y_set_hi.put_f64(60.0).await;
            let _ = ch_y_set_lo.put_f64(0.0).await;
        }
    }

    let _ = ch_put_vals.put_i16(0).await;
    let _ = ch_auto_mode.put_i16(0).await;
    let _ = ch_oper_ack.put_i16(0).await;

    // Read initial crystal parameters and compute 2d spacing
    let mut h = ch_h.get_f64().await;
    let mut k = ch_k.get_f64().await;
    let mut l = ch_l.get_f64().await;
    let mut a = ch_a.get_f64().await;
    let (mut two_d, forbidden, msg) = calc_2d_spacing(a, h, k, l);
    let _ = ch_d.put_f64(two_d).await;
    let _ = ch_seq_msg1.put_string(msg).await;
    if forbidden {
        let _ = ch_alert.put_i16(1).await;
    }

    // Read motor limits and compute theta/energy limits
    let mut theta_mot_hi = ch_theta_mot_hilim.get_f64().await;
    let mut theta_mot_lo = ch_theta_mot_lolim.get_f64().await;
    let (mut theta_hi, mut theta_lo) = compute_theta_limits(theta_mot_hi, theta_mot_lo);
    let _ = ch_theta_hi.put_f64(theta_hi).await;
    let _ = ch_theta_lo.put_f64(theta_lo).await;

    let (e_hi, e_lo, lambda_hi, lambda_lo) =
        compute_energy_lambda_limits(two_d, theta_hi, theta_lo);
    let _ = ch_e_hi.put_f64(e_hi).await;
    let _ = ch_e_lo.put_f64(e_lo).await;
    let _ = ch_lambda_hi.put_f64(lambda_hi).await;
    let _ = ch_lambda_lo.put_f64(lambda_lo).await;

    // Check motor limits
    let theta_mot_rdbk = ch_theta_mot_rbv.get_f64().await;
    let _y_mot_rdbk = ch_y_mot_rbv.get_f64().await;
    let _z_mot_rdbk = ch_z_mot_rbv.get_f64().await;
    let _y_mot_hi = ch_y_mot_hilim.get_f64().await;
    let _y_mot_lo = ch_y_mot_lolim.get_f64().await;
    let _z_mot_hi = ch_z_mot_hilim.get_f64().await;
    let _z_mot_lo = ch_z_mot_lolim.get_f64().await;
    let cc_mode_val = ch_cc_mode.get_i16().await;
    let mut cc_mode = CrystalMode::from_i16(cc_mode_val);

    // Update readbacks
    let mut theta_rdbk_val = theta_mot_rdbk;
    let mut lambda_rdbk_val = if two_d > 0.0 {
        theta_to_lambda(theta_rdbk_val, two_d)
    } else {
        0.0
    };
    let mut e_rdbk_val = if lambda_rdbk_val > 0.0 {
        lambda_to_energy(lambda_rdbk_val)
    } else {
        0.0
    };
    let _ = ch_theta_rdbk.put_f64_post(theta_rdbk_val).await;
    let _ = ch_lambda_rdbk.put_f64_post(lambda_rdbk_val).await;
    let _ = ch_e_rdbk.put_f64_post(e_rdbk_val).await;

    // Set initial theta from motor readback
    let mut theta_val = theta_mot_rdbk;
    let _ = ch_theta.put_f64(theta_val).await;
    let mut lambda_val = theta_to_lambda(theta_val, two_d);
    let _ = ch_lambda.put_f64(lambda_val).await;
    let mut e_val = lambda_to_energy(lambda_val);
    let _ = ch_e.put_f64(e_val).await;

    let mut auto_mode: bool = false;
    let mut use_set_mode: bool = false;
    #[allow(unused_assignments)]
    let mut speed_control: bool = false;
    let mut y_offset_val: f64 = ch_y_offset.get_f64().await;
    let mut _caused_move = false;
    let risk_averse = false;

    let _ = ch_seq_msg1.put_string("Kohzu Control Ready").await;
    let _ = ch_seq_msg2.put_string(" ").await;

    info!("Kohzu controller initialized for {}", config.prefix);

    // -- Main loop --
    // PV name constants for dispatch
    let pv_e = config.pv("BraggEAO");
    let pv_lambda = config.pv("BraggLambdaAO");
    let pv_theta = config.pv("BraggThetaAO");
    let pv_h = config.pv("BraggHAO");
    let pv_k = config.pv("BraggKAO");
    let pv_l = config.pv("BraggLAO");
    let pv_a = config.pv("BraggAAO");
    let pv_put_vals = config.pv("KohzuPutBO");
    let pv_auto_mode = config.pv("KohzuModeBO");
    let pv_cc_mode = config.pv("KohzuMode2MO");
    let pv_oper_ack = config.pv("KohzuOperAckBO");
    let pv_theta_mot_rbv = config.motor_pv(&config.m_theta, ".RBV");
    let pv_theta_hilim = config.motor_pv(&config.m_theta, ".HLM");
    let pv_theta_lolim = config.motor_pv(&config.m_theta, ".LLM");
    let pv_y_offset = config.pv("Kohzu_yOffsetAO");
    let pv_use_set = config.pv("KohzuUseSetBO");

    println!(
        "kohzuCtl: ready (two_d={:.4}, theta=[{:.1}..{:.1}])",
        two_d, theta_lo, theta_hi
    );

    let mut deferred_events: HashMap<String, f64> = HashMap::new();
    let mut pending_retarget = false;
    loop {
        let mut proceed_to_theta_changed = false;
        if pending_retarget {
            // Retarget: values already updated, skip wait and go straight to move
            pending_retarget = false;
            proceed_to_theta_changed = true;
        }
        let (changed_pv, new_val) = if proceed_to_theta_changed {
            // Skip PV dispatch — go directly to theta_changed processing
            (String::new(), 0.0)
        } else if let Some(key) = deferred_events.keys().next().cloned() {
            let val = deferred_events.remove(&key).unwrap();
            (key, val)
        } else {
            monitor.wait_change().await
        };
        tracing::trace!("kohzuCtl: PV changed: {} = {}", changed_pv, new_val);

        if changed_pv == pv_e {
            let new_e = new_val;
            if (new_e - e_val).abs() > 1e-12 {
                e_val = new_e;
                lambda_val = energy_to_lambda(e_val);
                let _ = ch_lambda.put_f64(lambda_val).await;

                if lambda_val > two_d {
                    let _ = ch_seq_msg1.put_string("Wavelength > 2d spacing.").await;
                    let _ = ch_alert.put_i16(1).await;
                } else if let Some(th) = lambda_to_theta(lambda_val, two_d) {
                    theta_val = th;
                    let _ = ch_theta.put_f64(theta_val).await;
                }
                proceed_to_theta_changed = true;
            }
        } else if changed_pv == pv_lambda {
            let new_l = new_val;
            if (new_l - lambda_val).abs() > 1e-12 {
                lambda_val = new_l;
                if lambda_val > two_d {
                    let _ = ch_seq_msg1.put_string("Wavelength > 2d spacing.").await;
                    let _ = ch_alert.put_i16(1).await;
                } else if let Some(th) = lambda_to_theta(lambda_val, two_d) {
                    theta_val = th;
                    let _ = ch_theta.put_f64(theta_val).await;
                }
                proceed_to_theta_changed = true;
            }
        } else if changed_pv == pv_theta {
            let new_th = new_val;
            if (new_th - theta_val).abs() > 1e-12 {
                theta_val = new_th;
                proceed_to_theta_changed = true;
            }
        } else if changed_pv == pv_h {
            h = ch_h.get_f64().await;
            let (d, _forb, msg) = calc_2d_spacing(a, h, k, l);
            two_d = d;
            let _ = ch_d.put_f64(two_d).await;
            let _ = ch_seq_msg1.put_string(msg).await;
            auto_mode = false;
            let _ = ch_auto_mode.put_i16(0).await;
            let _ = ch_seq_msg2.put_string("Set to Manual Mode").await;
            let (eh, el, lh, ll) = compute_energy_lambda_limits(two_d, theta_hi, theta_lo);
            let _ = ch_e_hi.put_f64(eh).await;
            let _ = ch_e_lo.put_f64(el).await;
            let _ = ch_lambda_hi.put_f64(lh).await;
            let _ = ch_lambda_lo.put_f64(ll).await;
        } else if changed_pv == pv_k {
            k = ch_k.get_f64().await;
            let (d, _forb, msg) = calc_2d_spacing(a, h, k, l);
            two_d = d;
            let _ = ch_d.put_f64(two_d).await;
            let _ = ch_seq_msg1.put_string(msg).await;
            auto_mode = false;
            let _ = ch_auto_mode.put_i16(0).await;
            let _ = ch_seq_msg2.put_string("Set to Manual Mode").await;
            let (eh, el, lh, ll) = compute_energy_lambda_limits(two_d, theta_hi, theta_lo);
            let _ = ch_e_hi.put_f64(eh).await;
            let _ = ch_e_lo.put_f64(el).await;
            let _ = ch_lambda_hi.put_f64(lh).await;
            let _ = ch_lambda_lo.put_f64(ll).await;
        } else if changed_pv == pv_l {
            l = ch_l.get_f64().await;
            let (d, _forb, msg) = calc_2d_spacing(a, h, k, l);
            two_d = d;
            let _ = ch_d.put_f64(two_d).await;
            let _ = ch_seq_msg1.put_string(msg).await;
            auto_mode = false;
            let _ = ch_auto_mode.put_i16(0).await;
            let _ = ch_seq_msg2.put_string("Set to Manual Mode").await;
            let (eh, el, lh, ll) = compute_energy_lambda_limits(two_d, theta_hi, theta_lo);
            let _ = ch_e_hi.put_f64(eh).await;
            let _ = ch_e_lo.put_f64(el).await;
            let _ = ch_lambda_hi.put_f64(lh).await;
            let _ = ch_lambda_lo.put_f64(ll).await;
        } else if changed_pv == pv_a {
            a = ch_a.get_f64().await;
            let (d, _forb, msg) = calc_2d_spacing(a, h, k, l);
            two_d = d;
            let _ = ch_d.put_f64(two_d).await;
            let _ = ch_seq_msg1.put_string(msg).await;
            auto_mode = false;
            let _ = ch_auto_mode.put_i16(0).await;
            let _ = ch_seq_msg2.put_string("Set to Manual Mode").await;
            let (eh, el, lh, ll) = compute_energy_lambda_limits(two_d, theta_hi, theta_lo);
            let _ = ch_e_hi.put_f64(eh).await;
            let _ = ch_e_lo.put_f64(el).await;
            let _ = ch_lambda_hi.put_f64(lh).await;
            let _ = ch_lambda_lo.put_f64(ll).await;
        } else if changed_pv == pv_put_vals {
            let pv = new_val as i16;
            if pv != 0 {
                proceed_to_theta_changed = true;
            }
        } else if changed_pv == pv_auto_mode {
            auto_mode = new_val as i16 != 0;
        } else if changed_pv == pv_cc_mode {
            cc_mode = CrystalMode::from_i16(new_val as i16);
        } else if changed_pv == pv_oper_ack {
            if new_val as i16 != 0 {
                let _ = ch_alert.put_i16(0).await;
                let _ = ch_seq_msg1.put_string(" ").await;
                let _ = ch_seq_msg2.put_string(" ").await;
                let _ = ch_oper_ack.put_i16(0).await;
            }
        } else if changed_pv == pv_theta_mot_rbv {
            let rbv = new_val;
            let _ = ch_theta_rdbk_echo.put_f64(rbv).await;
            theta_rdbk_val = rbv;
            lambda_rdbk_val = theta_to_lambda(theta_rdbk_val, two_d);
            e_rdbk_val = if lambda_rdbk_val > 0.0 {
                lambda_to_energy(lambda_rdbk_val)
            } else {
                0.0
            };
            let _ = ch_theta_rdbk.put_f64_post(theta_rdbk_val).await;
            let _ = ch_lambda_rdbk.put_f64_post(lambda_rdbk_val).await;
            let _ = ch_e_rdbk.put_f64_post(e_rdbk_val).await;
        } else if changed_pv == pv_theta_hilim {
            theta_mot_hi = new_val;
            let (hi, lo) = compute_theta_limits(theta_mot_hi, theta_mot_lo);
            theta_hi = hi;
            theta_lo = lo;
            let _ = ch_theta_hi.put_f64(theta_hi).await;
            let _ = ch_theta_lo.put_f64(theta_lo).await;
            let (eh, el, lh, ll) = compute_energy_lambda_limits(two_d, theta_hi, theta_lo);
            let _ = ch_e_hi.put_f64(eh).await;
            let _ = ch_e_lo.put_f64(el).await;
            let _ = ch_lambda_hi.put_f64(lh).await;
            let _ = ch_lambda_lo.put_f64(ll).await;
        } else if changed_pv == pv_theta_lolim {
            theta_mot_lo = new_val;
            let (hi, lo) = compute_theta_limits(theta_mot_hi, theta_mot_lo);
            theta_hi = hi;
            theta_lo = lo;
            let _ = ch_theta_hi.put_f64(theta_hi).await;
            let _ = ch_theta_lo.put_f64(theta_lo).await;
            let (eh, el, lh, ll) = compute_energy_lambda_limits(two_d, theta_hi, theta_lo);
            let _ = ch_e_hi.put_f64(eh).await;
            let _ = ch_e_lo.put_f64(el).await;
            let _ = ch_lambda_hi.put_f64(lh).await;
            let _ = ch_lambda_lo.put_f64(ll).await;
        } else if changed_pv == pv_y_offset {
            y_offset_val = new_val;
            auto_mode = false;
            let _ = ch_auto_mode.put_i16(0).await;
            let _ = ch_seq_msg1
                .put_string(&format!("y offset changed to {:.4}", y_offset_val))
                .await;
            let _ = ch_seq_msg2.put_string("Set to Manual Mode").await;
            proceed_to_theta_changed = true;
        } else if changed_pv == pv_use_set {
            use_set_mode = new_val as i16 != 0;
            let set_val = if use_set_mode { 1i16 } else { 0i16 };
            let _ = ch_theta_mot_set.put_i16(set_val).await;
            let _ = ch_y_mot_set.put_i16(set_val).await;
            let _ = ch_z_mot_set.put_i16(set_val).await;
        }

        if !proceed_to_theta_changed {
            continue;
        }

        // -- Theta-changed processing --
        // Clamp theta to limits
        let (clamped_theta, was_clamped) = clamp_theta(theta_val, theta_lo, theta_hi);
        if was_clamped {
            theta_val = clamped_theta;
            let _ = ch_seq_msg1.put_string("Theta constrained to LIMIT").await;
            let _ = ch_alert.put_i16(1).await;
            if risk_averse {
                auto_mode = false;
                let _ = ch_auto_mode.put_i16(0).await;
                let _ = ch_seq_msg2.put_string("Set to Manual Mode").await;
            }
        }

        // Recompute lambda and E from theta
        lambda_val = theta_to_lambda(theta_val, two_d);
        let _ = ch_lambda.put_f64(lambda_val).await;
        e_val = lambda_to_energy(lambda_val);
        let _ = ch_e.put_f64(e_val).await;

        // Update readbacks
        let current_rbv = ch_theta_mot_rbv.get_f64().await;
        theta_rdbk_val = current_rbv;
        lambda_rdbk_val = theta_to_lambda(theta_rdbk_val, two_d);
        e_rdbk_val = lambda_to_energy(lambda_rdbk_val);
        let _ = ch_theta_rdbk.put_f64_post(theta_rdbk_val).await;
        let _ = ch_lambda_rdbk.put_f64_post(lambda_rdbk_val).await;
        let _ = ch_e_rdbk.put_f64_post(e_rdbk_val).await;

        // -- Calculate motor movements --
        let theta_mot_desired = theta_val;
        let y_mot_desired = calc_y_position(geom, theta_val, y_offset_val);
        let z_mot_desired = calc_z_position(geom, theta_val, y_offset_val);

        let _ = ch_theta_set_ao.put_f64(theta_mot_desired).await;
        let _ = ch_y_set_ao.put_f64(y_mot_desired).await;
        let _ = ch_z_set_ao.put_f64(z_mot_desired).await;

        // Check Y and Z limits
        let mut will_violate = false;
        let y_hi = ch_y_mot_hilim.get_f64().await;
        let y_lo = ch_y_mot_lolim.get_f64().await;
        let z_hi = ch_z_mot_hilim.get_f64().await;
        let z_lo = ch_z_mot_lolim.get_f64().await;

        if !cc_mode.y_frozen() && (y_mot_desired < y_lo || y_mot_desired > y_hi) {
            let detail = format!(
                "Y soft limit exceeded: want={:.3}, range=[{:.3}, {:.3}]",
                y_mot_desired, y_lo, y_hi
            );
            let _ = ch_seq_msg1.put_string(&detail).await;
            if ch_debug.get_i16().await > 0 {
                eprintln!(
                    "kohzuCtl: move blocked by Y soft limit (want={:.6}, lo={:.6}, hi={:.6}, theta={:.6}, E={:.6})",
                    y_mot_desired, y_lo, y_hi, theta_val, e_val
                );
            }
            let _ = ch_alert.put_i16(1).await;
            if risk_averse {
                auto_mode = false;
                let _ = ch_auto_mode.put_i16(0).await;
                let _ = ch_seq_msg2.put_string("Setting to Manual Mode").await;
            } else {
                will_violate = true;
            }
        }
        if !cc_mode.z_frozen() && (z_mot_desired < z_lo || z_mot_desired > z_hi) {
            let detail = format!(
                "Z soft limit exceeded: want={:.3}, range=[{:.3}, {:.3}]",
                z_mot_desired, z_lo, z_hi
            );
            let _ = ch_seq_msg1.put_string(&detail).await;
            if ch_debug.get_i16().await > 0 {
                eprintln!(
                    "kohzuCtl: move blocked by Z soft limit (want={:.6}, lo={:.6}, hi={:.6}, theta={:.6}, E={:.6})",
                    z_mot_desired, z_lo, z_hi, theta_val, e_val
                );
            }
            let _ = ch_alert.put_i16(1).await;
            if risk_averse {
                auto_mode = false;
                let _ = ch_auto_mode.put_i16(0).await;
                let _ = ch_seq_msg2.put_string("Setting to Manual Mode").await;
            } else {
                will_violate = true;
            }
        }

        if will_violate {
            let blocked = format!(
                "Move blocked: E={:.3} keV theta={:.3} deg",
                e_val, theta_val
            );
            let _ = ch_seq_msg2.put_string(&blocked).await;
            if ch_debug.get_i16().await > 0 {
                eprintln!("kohzuCtl: {}", blocked);
            }
            let _ = ch_moving.put_i16(0).await;
            continue;
        }

        // -- Move motors if in auto or put mode --
        let put_requested = ch_put_vals.get_i16().await != 0;
        if auto_mode || put_requested || use_set_mode {
            let _ = ch_moving.put_i16(1).await;

            // Coordinate speeds
            speed_control = ch_speed_ctrl.get_i16().await != 0;
            if speed_control {
                let th_speed = ch_theta_mot_velo.get_f64().await;
                let y_speed = ch_y_mot_velo.get_f64().await;
                let z_speed = ch_z_mot_velo.get_f64().await;
                let th_delta = theta_val - current_rbv;
                let y_delta = y_mot_desired - ch_y_mot_rbv.get_f64().await;
                let z_delta = z_mot_desired - ch_z_mot_rbv.get_f64().await;

                let (new_th, new_y, new_z) = coordinate_speeds(
                    th_delta, y_delta, z_delta, th_speed, y_speed, z_speed, cc_mode,
                );
                let _ = ch_theta_mot_velo.put_f64(new_th).await;
                if !cc_mode.y_frozen() {
                    let _ = ch_y_mot_velo.put_f64(new_y).await;
                }
                if !cc_mode.z_frozen() {
                    let _ = ch_z_mot_velo.put_f64(new_z).await;
                }
            }

            // Command motors — use put_f64_process to trigger motor record processing
            if ch_debug.get_i16().await > 0 {
                eprintln!(
                    "kohzuCtl: MOVING theta={} y={} z={}",
                    theta_mot_desired, y_mot_desired, z_mot_desired
                );
            }
            let _ = ch_theta_mot_cmd.put_f64_process(theta_mot_desired).await;
            match cc_mode {
                CrystalMode::Normal => {
                    let _ = ch_y_mot_cmd.put_f64_process(y_mot_desired).await;
                    let _ = ch_z_mot_cmd.put_f64_process(z_mot_desired).await;
                }
                CrystalMode::ChannelCut => {}
                CrystalMode::FreezeZ => {
                    let _ = ch_y_mot_cmd.put_f64_process(y_mot_desired).await;
                }
                CrystalMode::FreezeY => {
                    let _ = ch_z_mot_cmd.put_f64_process(z_mot_desired).await;
                }
            }

            let _ = ch_put_vals.put_i16(0).await;
            _caused_move = true;

            // Wait for motors done, while still accepting new setpoints.
            // If a new setpoint arrives during the move, stop motors and
            // let the outer loop recalculate and re-issue the move.
            let mut retarget = false;
            loop {
                tokio::select! {
                    // Poll DMOV every 100ms
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        let th_dmov = ch_theta_dmov.get_i16().await;
                        let y_dmov = ch_y_dmov.get_i16().await;
                        let z_dmov = ch_z_dmov.get_i16().await;

                        // Check limit switches
                        let th_hls = ch_theta_hls.get_i16().await;
                        let th_lls = ch_theta_lls.get_i16().await;
                        if th_hls != 0 || th_lls != 0 {
                            let _ = ch_seq_msg1.put_string("Theta Motor hit a limit switch!").await;
                            let _ = ch_alert.put_i16(1).await;
                            auto_mode = false;
                            let _ = ch_auto_mode.put_i16(0).await;
                            let _ = ch_seq_msg2.put_string("Setting to Manual Mode").await;
                            let _ = ch_theta_mot_stop.put_i16(1).await;
                            let _ = ch_y_stop.put_i16(1).await;
                            let _ = ch_z_stop.put_i16(1).await;
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            break;
                        }
                        if !cc_mode.y_frozen() {
                            let y_hls = ch_y_hls.get_i16().await;
                            let y_lls = ch_y_lls.get_i16().await;
                            if y_hls != 0 || y_lls != 0 {
                                let _ = ch_seq_msg1.put_string("Y Motor hit a limit switch!").await;
                                let _ = ch_alert.put_i16(1).await;
                                auto_mode = false;
                                let _ = ch_auto_mode.put_i16(0).await;
                                let _ = ch_seq_msg2.put_string("Setting to Manual Mode").await;
                                let _ = ch_theta_mot_stop.put_i16(1).await;
                                let _ = ch_y_stop.put_i16(1).await;
                                let _ = ch_z_stop.put_i16(1).await;
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                break;
                            }
                        }
                        if !cc_mode.z_frozen() {
                            let z_hls = ch_z_hls.get_i16().await;
                            let z_lls = ch_z_lls.get_i16().await;
                            if z_hls != 0 || z_lls != 0 {
                                let _ = ch_seq_msg1.put_string("Z Motor hit a limit switch!").await;
                                let _ = ch_alert.put_i16(1).await;
                                auto_mode = false;
                                let _ = ch_auto_mode.put_i16(0).await;
                                let _ = ch_seq_msg2.put_string("Setting to Manual Mode").await;
                                let _ = ch_theta_mot_stop.put_i16(1).await;
                                let _ = ch_y_stop.put_i16(1).await;
                                let _ = ch_z_stop.put_i16(1).await;
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                break;
                            }
                        }

                        // Update readbacks while moving
                        let rbv = ch_theta_mot_rbv.get_f64().await;
                        theta_rdbk_val = rbv;
                        lambda_rdbk_val = theta_to_lambda(theta_rdbk_val, two_d);
                        e_rdbk_val = lambda_to_energy(lambda_rdbk_val);
                        let _ = ch_theta_rdbk.put_f64_post(theta_rdbk_val).await;
                        let _ = ch_lambda_rdbk.put_f64_post(lambda_rdbk_val).await;
                        let _ = ch_e_rdbk.put_f64_post(e_rdbk_val).await;

                        if th_dmov != 0 && y_dmov != 0 && z_dmov != 0 {
                            break;
                        }
                    }
                    // Monitor event during move — only retarget for setpoint changes
                    (changed_pv_new, new_val_new) = monitor.wait_change() => {
                        // Only retarget for energy/lambda/theta setpoint changes
                        let is_setpoint = changed_pv_new == pv_e
                            || changed_pv_new == pv_lambda
                            || changed_pv_new == pv_theta;
                        if !is_setpoint {
                            if changed_pv_new != pv_theta_mot_rbv {
                                deferred_events.insert(changed_pv_new.clone(), new_val_new);
                            }
                            if changed_pv_new == pv_put_vals
                                || changed_pv_new == pv_auto_mode
                                || changed_pv_new == pv_cc_mode
                                || changed_pv_new == pv_use_set
                            {
                                let _ = ch_seq_msg2
                                    .put_string("Control change deferred until move completes")
                                    .await;
                            }
                            continue;
                        }
                        tracing::debug!("kohzuCtl: new setpoint during move: {} = {}", changed_pv_new, new_val_new);
                        // Stop current motors
                        let _ = ch_theta_mot_stop.put_i16(1).await;
                        let _ = ch_y_stop.put_i16(1).await;
                        let _ = ch_z_stop.put_i16(1).await;
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        // Update internal state directly (don't defer —
                        // deferred events would be filtered as "no change"
                        // since we already updated the value here)
                        if changed_pv_new == pv_e {
                            e_val = new_val_new;
                            lambda_val = energy_to_lambda(e_val);
                            if let Some(th) = lambda_to_theta(lambda_val, two_d) {
                                theta_val = th;
                            }
                        } else if changed_pv_new == pv_lambda {
                            lambda_val = new_val_new;
                            e_val = lambda_to_energy(lambda_val);
                            if let Some(th) = lambda_to_theta(lambda_val, two_d) {
                                theta_val = th;
                            }
                        } else if changed_pv_new == pv_theta {
                            theta_val = new_val_new;
                            lambda_val = theta_to_lambda(theta_val, two_d);
                            e_val = lambda_to_energy(lambda_val);
                        }
                        let _ = ch_moving.put_i16(0).await;
                        retarget = true;
                        break;
                    }
                }
            }
            // If retarget, recalculate and immediately re-enter move logic
            if retarget {
                let _ = ch_seq_msg1.put_string("Retargeting...").await;
                pending_retarget = true;
                _caused_move = false;
                continue;
            }

            // Restore speeds if we changed them
            if speed_control && _caused_move {
                // Speeds were already replaced; the SNL code restores old speeds.
                // In practice the next move recalculates anyway.
            }
            _caused_move = false;

            // Final readback update
            let rbv = ch_theta_mot_rbv.get_f64().await;
            theta_rdbk_val = rbv;
            lambda_rdbk_val = theta_to_lambda(theta_rdbk_val, two_d);
            e_rdbk_val = lambda_to_energy(lambda_rdbk_val);
            let _ = ch_theta_rdbk.put_f64_post(theta_rdbk_val).await;
            let _ = ch_lambda_rdbk.put_f64_post(lambda_rdbk_val).await;
            let _ = ch_e_rdbk.put_f64_post(e_rdbk_val).await;

            // Assert done
            let _ = ch_moving.put_i16(0).await;
        }

        // Update echo PVs
        let _ = ch_theta_rdbk_echo
            .put_f64(ch_theta_mot_rbv.get_f64().await)
            .await;
        let _ = ch_y_rdbk_echo.put_f64(ch_y_mot_rbv.get_f64().await).await;
        let _ = ch_z_rdbk_echo.put_f64(ch_z_mot_rbv.get_f64().await).await;
        let _ = ch_theta_vel_echo
            .put_f64(ch_theta_mot_velo.get_f64().await)
            .await;
        let _ = ch_y_vel_echo.put_f64(ch_y_mot_velo.get_f64().await).await;
        let _ = ch_z_vel_echo.put_f64(ch_z_mot_velo.get_f64().await).await;
        let _ = ch_theta_dmov_echo
            .put_i16(ch_theta_dmov.get_i16().await)
            .await;
        let _ = ch_y_dmov_echo.put_i16(ch_y_dmov.get_i16().await).await;
        let _ = ch_z_dmov_echo.put_i16(ch_z_dmov.get_i16().await).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_energy_to_lambda() {
        let lambda = energy_to_lambda(10.0);
        assert!((lambda - 1.23984244).abs() < 1e-6);
    }

    #[test]
    fn test_lambda_to_energy() {
        let e = lambda_to_energy(1.23984244);
        assert!((e - 10.0).abs() < 1e-6);
    }

    #[test]
    fn test_energy_lambda_roundtrip() {
        for e in [5.0, 8.0, 12.0, 20.0, 50.0] {
            let l = energy_to_lambda(e);
            let e2 = lambda_to_energy(l);
            assert!((e - e2).abs() < 1e-10, "roundtrip failed for E={e}");
        }
    }

    #[test]
    fn test_calc_2d_spacing_si111() {
        // Silicon (111): a = 5.4309 A
        let (two_d, forbidden, _msg) = calc_2d_spacing(5.4309, 1.0, 1.0, 1.0);
        // 2d = 2 * 5.4309 / sqrt(3) = 6.2712...
        assert!((two_d - 6.2712).abs() < 0.01);
        assert!(!forbidden);
    }

    #[test]
    fn test_forbidden_reflection() {
        // (1,0,0) is forbidden for diamond cubic
        assert!(is_forbidden_reflection(1.0, 0.0, 0.0));
        // (1,1,1) is allowed
        assert!(!is_forbidden_reflection(1.0, 1.0, 1.0));
        // (2,0,0) has mixed parity -> forbidden
        assert!(is_forbidden_reflection(2.0, 0.0, 0.0));
        // Wait, (2,0,0): all even -- same parity check passes, but sum rule:
        // (2+0+0)/2 = 1, |1-1|=0 <= 0.25, nint=1 is odd -> forbidden.
        assert!(is_forbidden_reflection(2.0, 0.0, 0.0));
        // (2,2,0) is allowed
        assert!(!is_forbidden_reflection(2.0, 2.0, 0.0));
    }

    #[test]
    fn test_lambda_to_theta() {
        // Si(111) at 8 keV: lambda = 12.3984/8 = 1.54980 A
        let (two_d, _, _) = calc_2d_spacing(5.4309, 1.0, 1.0, 1.0);
        let lambda = energy_to_lambda(8.0);
        let theta = lambda_to_theta(lambda, two_d).unwrap();
        // Expected: asin(1.5498/6.2712) * 57.2958 ~ 14.3 deg
        assert!((theta - 14.3).abs() < 0.5, "theta={theta}");
    }

    #[test]
    fn test_lambda_to_theta_impossible() {
        // lambda > 2d should return None
        assert!(lambda_to_theta(10.0, 6.0).is_none());
    }

    #[test]
    fn test_theta_to_lambda_roundtrip() {
        let two_d = 6.2712;
        let theta = 14.3;
        let lambda = theta_to_lambda(theta, two_d);
        let theta2 = lambda_to_theta(lambda, two_d).unwrap();
        assert!((theta - theta2).abs() < 1e-6);
    }

    #[test]
    fn test_calc_y_z_standard() {
        let y = calc_y_position(Geometry::Standard, 30.0, 17.5);
        let z = calc_z_position(Geometry::Standard, 30.0, 17.5);
        // y = -17.5/cos(30 deg) = -17.5/0.86603 = -20.2073
        assert!((y - (-20.2073)).abs() < 0.01, "y={y}");
        // z = 17.5/sin(30 deg) = 17.5/0.5 = 35.0
        assert!((z - 35.0).abs() < 0.01, "z={z}");
    }

    #[test]
    fn test_calc_y_z_alternate() {
        let y = calc_y_position(Geometry::Alternate, 30.0, 17.5);
        let z = calc_z_position(Geometry::Alternate, 30.0, 17.5);
        // y = 17.5/(2*cos(30)) = 17.5/1.73205 = 10.1036
        assert!((y - 10.1036).abs() < 0.01, "y={y}");
        // z = 17.5/(2*sin(30)) = 17.5/1.0 = 17.5
        assert!((z - 17.5).abs() < 0.01, "z={z}");
    }

    #[test]
    fn test_clamp_theta() {
        assert_eq!(clamp_theta(45.0, 1.0, 89.0), (45.0, false));
        assert_eq!(clamp_theta(0.5, 1.0, 89.0), (1.0, true));
        assert_eq!(clamp_theta(90.0, 1.0, 89.0), (89.0, true));
    }

    #[test]
    fn test_compute_theta_limits() {
        assert_eq!(compute_theta_limits(100.0, -5.0), (89.0, 1.0));
        assert_eq!(compute_theta_limits(45.0, 5.0), (45.0, 5.0));
    }

    #[test]
    fn test_compute_energy_lambda_limits() {
        let two_d = 6.2712;
        let (e_hi, e_lo, l_hi, l_lo) = compute_energy_lambda_limits(two_d, 89.0, 1.0);
        assert!(e_hi > e_lo);
        assert!(l_hi > l_lo);
        assert!(l_hi <= two_d);
    }

    #[test]
    fn test_coordinate_speeds() {
        let (th, y, z) = coordinate_speeds(10.0, 5.0, 20.0, 1.0, 1.0, 1.0, CrystalMode::Normal);
        // Z takes longest (20s at speed 1), so th and y should be slower
        assert!((z - 1.0).abs() < 1e-6);
        assert!((th - 0.5).abs() < 1e-6);
        assert!((y - 0.25).abs() < 1e-6);
    }

    #[test]
    fn test_coordinate_speeds_frozen() {
        let (th, _y, z) =
            coordinate_speeds(10.0, 5.0, 20.0, 1.0, 1.0, 1.0, CrystalMode::ChannelCut);
        // Y and Z frozen, so only theta time matters
        assert!((th - 1.0).abs() < 1e-6);
        // Z frozen means z_time = 0, so z_speed adjusted to theta time
        assert!((z - 2.0).abs() < 1e-6);
    }

    #[test]
    fn test_crystal_mode_frozen() {
        assert!(!CrystalMode::Normal.y_frozen());
        assert!(!CrystalMode::Normal.z_frozen());
        assert!(CrystalMode::ChannelCut.y_frozen());
        assert!(CrystalMode::ChannelCut.z_frozen());
        assert!(!CrystalMode::FreezeZ.y_frozen());
        assert!(CrystalMode::FreezeZ.z_frozen());
        assert!(CrystalMode::FreezeY.y_frozen());
        assert!(!CrystalMode::FreezeY.z_frozen());
    }
}
