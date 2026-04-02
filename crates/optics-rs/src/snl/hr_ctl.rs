//! High-resolution analyzer crystal control -- native Rust port of `hrCtl.st`.
//!
//! Controls a 4-bounce high-resolution monochromator with two analyzer crystals,
//! each with independent (H,K,L,A) parameters. Supports nested and symmetric
//! geometries, single-crystal and two-crystal operating modes, and world/local
//! offset tracking for beam wander correction.
//!
//! Crystal angles are in *micro-radian* motor units internally; user-facing
//! angles (theta, phi) are in degrees.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use tracing::info;

use crate::db_access::{DbChannel, DbMultiMonitor, alloc_origin};
use crate::snl::kohzu_ctl::HC;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const PI: f64 = std::f64::consts::PI;
/// Radians to degrees.
const R2D: f64 = 180.0 / PI;
/// Degrees to radians.
const D2R: f64 = PI / 180.0;
/// Micro-radians to degrees.
const UR2D: f64 = R2D / 1_000_000.0;
/// Degrees to micro-radians.
const D2UR: f64 = 1_000_000.0 * PI / 180.0;

// ---------------------------------------------------------------------------
// Operating modes
// ---------------------------------------------------------------------------

/// HR operating mode (opMode / Mode2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HrOpMode {
    /// Single crystal -- only phi1 moves.
    Single = 0,
    /// Two crystals locked -- phi1 and phi2 move together.
    TwoLocked = 1,
    /// Two crystals independent -- user adjusts theta2 for fine energy.
    TwoIndependent = 2,
}

impl HrOpMode {
    pub fn from_i16(v: i16) -> Self {
        match v {
            1 => Self::TwoLocked,
            2 => Self::TwoIndependent,
            _ => Self::Single,
        }
    }
}

/// HR geometry (nested vs symmetric).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HrGeometry {
    /// Nested: surfaces 1,4 use (H1,K1,L1); surfaces 2,3 use (H2,K2,L2).
    Nested = 0,
    /// Symmetric: surfaces 1,2 use (H1,K1,L1); surfaces 3,4 use (H2,K2,L2).
    Symmetric = 1,
}

impl HrGeometry {
    pub fn from_i16(v: i16) -> Self {
        if v == 0 {
            Self::Nested
        } else {
            Self::Symmetric
        }
    }
}

// ---------------------------------------------------------------------------
// State enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HrState {
    Init,
    InitSequence,
    WaitForCommand,
    DInputChanged,
    PhiLimits,
    EChanged,
    LambdaChanged,
    ThetaChanged,
    CalcMovements,
    MoveHr,
    UpdateReadback,
    CheckDone,
    MotorsStopped,
    CheckMotorLimits,
    StopHr,
    Tweak,
}

// ---------------------------------------------------------------------------
// Pure physics functions
// ---------------------------------------------------------------------------

/// Calculate 2d-spacing for a crystal from lattice constant and Miller indices.
/// Returns (two_d, is_forbidden, message).
pub fn calc_2d_spacing(a: f64, h: f64, k: f64, l: f64) -> (f64, bool, String) {
    let hkl_sq = h * h + k * k + l * l;
    if hkl_sq <= 0.0 {
        return (0.0, true, "Invalid HKL: all zero".to_string());
    }
    let two_d = (2.0 * a) / hkl_sq.sqrt();
    let forbidden = crate::snl::kohzu_ctl::is_forbidden_reflection(h, k, l);
    let msg = if forbidden {
        "Invalid HKL combination".to_string()
    } else {
        "New d spacing".to_string()
    };
    (two_d, forbidden, msg)
}

/// Compute phi1 from theta1 (they are equal in all modes).
pub fn theta1_to_phi1(theta1: f64) -> f64 {
    theta1
}

/// Compute phi2 from phi1, theta1, theta2 for nested geometry.
pub fn calc_phi2_nested(phi1: f64, theta1: f64, theta2: f64) -> f64 {
    phi1 + theta1 + theta2
}

/// Compute phi2 for symmetric geometry.
pub fn calc_phi2_symmetric(theta2: f64) -> f64 {
    theta2
}

/// Compute phi2 for two-independent mode with nested geometry.
/// Uses the perturbation formula from hrCtl.st.
pub fn calc_phi2_independent_nested(
    phi1: f64,
    theta1: f64,
    theta2_nom: f64,
    lambda: f64,
    lambda_nom: f64,
    d1: f64,
    d2: f64,
) -> f64 {
    let correction = R2D
        * (lambda - lambda_nom)
        * (1.0 / (d1 * (theta1 * D2R).cos()) + 1.0 / (d2 * (theta2_nom * D2R).cos()));
    (phi1 + theta1 + theta2_nom) + correction
}

/// Compute phi2 for two-independent mode with symmetric geometry.
pub fn calc_phi2_independent_symmetric(
    theta2_nom: f64,
    lambda: f64,
    lambda_nom: f64,
    d1: f64,
    d2: f64,
    theta1: f64,
) -> f64 {
    let correction = R2D
        * (lambda - lambda_nom)
        * (1.0 / (d1 * (theta1 * D2R).cos()) + 1.0 / (d2 * (theta2_nom * D2R).cos()));
    theta2_nom + correction
}

/// Compute phi motor command from phi angle, offset, and world offset.
/// `geom_sign` is -1 for symmetric geometry phi2, +1 otherwise.
pub fn phi_to_motor(phi: f64, phi_off: f64, world_off_deg: f64) -> f64 {
    (phi - phi_off - world_off_deg) * D2UR
}

/// Compute phi motor command for phi2 in symmetric geometry (world sign is reversed).
pub fn phi2_to_motor_symmetric(phi2: f64, phi2_off: f64, world_off_deg: f64) -> f64 {
    (phi2 - phi2_off + world_off_deg) * D2UR
}

/// Convert motor readback (micro-radians) to phi angle (degrees) with offset.
pub fn motor_to_phi(motor_ur: f64, phi_off: f64, world_off_deg: f64) -> f64 {
    motor_ur * UR2D + phi_off + world_off_deg
}

/// For phi2 in symmetric geometry, world offset sign is reversed.
pub fn motor_to_phi2_symmetric(motor_ur: f64, phi2_off: f64, world_off_deg: f64) -> f64 {
    motor_ur * UR2D + phi2_off - world_off_deg
}

/// Calculate readback values from motor positions.
pub struct HrReadback {
    pub phi1_rdbk: f64,
    pub theta1_rdbk: f64,
    pub phi2_rdbk: f64,
    pub theta2_rdbk: f64,
    pub lambda_rdbk: f64,
    pub e_rdbk: f64,
}

#[allow(clippy::too_many_arguments)]
pub fn calc_readback(
    phi1_mot_rdbk: f64,
    phi2_mot_rdbk: f64,
    phi1_off: f64,
    phi2_off: f64,
    world_off: f64, // in degrees already (worldOff * uR2D applied by caller)
    d1: f64,
    d2: f64,
    op_mode: HrOpMode,
    geom: HrGeometry,
) -> HrReadback {
    let world_off_deg = world_off * UR2D;

    let phi1_rdbk = motor_to_phi(phi1_mot_rdbk, phi1_off, world_off_deg);
    let theta1_rdbk_initial = phi1_rdbk;

    let (phi2_rdbk, lambda_rdbk) = match op_mode {
        HrOpMode::Single => {
            let lambda = d1 * (theta1_rdbk_initial * D2R).sin();
            (0.0, lambda)
        }
        HrOpMode::TwoLocked => {
            let phi2 = match geom {
                HrGeometry::Nested => motor_to_phi(phi2_mot_rdbk, phi2_off, world_off_deg),
                HrGeometry::Symmetric => {
                    motor_to_phi2_symmetric(phi2_mot_rdbk, phi2_off, world_off_deg)
                }
            };
            let lambda = d1 * (theta1_rdbk_initial * D2R).sin();
            (phi2, lambda)
        }
        HrOpMode::TwoIndependent => {
            let phi2 = match geom {
                HrGeometry::Nested => motor_to_phi(phi2_mot_rdbk, phi2_off, world_off_deg),
                HrGeometry::Symmetric => {
                    motor_to_phi2_symmetric(phi2_mot_rdbk, phi2_off, world_off_deg)
                }
            };
            let theta2_nom = (d1 * (theta1_rdbk_initial * D2R).sin() / d2).asin() * R2D;
            let lambda = match geom {
                HrGeometry::Nested => {
                    d1 * (theta1_rdbk_initial * D2R).sin()
                        + D2R * (phi2 - phi1_rdbk - theta1_rdbk_initial - theta2_nom)
                            / (1.0 / (d1 * (theta1_rdbk_initial * D2R).cos())
                                + 1.0 / (d2 * (theta2_nom * D2R).cos()))
                }
                HrGeometry::Symmetric => {
                    d1 * (theta1_rdbk_initial * D2R).sin()
                        + D2R * (phi2 - theta2_nom)
                            / (1.0 / (d1 * (theta1_rdbk_initial * D2R).cos())
                                + 1.0 / (d2 * (theta2_nom * D2R).cos()))
                }
            };
            (phi2, lambda)
        }
    };

    // Recompute theta from lambda
    let theta1_rdbk = if d1 > 0.0 && lambda_rdbk.abs() <= d1 {
        (lambda_rdbk / d1).asin() * R2D
    } else {
        theta1_rdbk_initial
    };
    let theta2_rdbk = if d2 > 0.0 && lambda_rdbk.abs() <= d2 && op_mode != HrOpMode::Single {
        (lambda_rdbk / d2).asin() * R2D
    } else {
        0.0
    };
    let e_rdbk = if lambda_rdbk > 0.0 {
        HC / lambda_rdbk
    } else {
        0.0
    };

    HrReadback {
        phi1_rdbk,
        theta1_rdbk,
        phi2_rdbk,
        theta2_rdbk,
        lambda_rdbk,
        e_rdbk,
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

pub struct HrConfig {
    pub prefix: String,
    pub n: String,
    pub m_phi1: String,
    pub m_phi2: String,
}

impl HrConfig {
    pub fn new(prefix: &str, n: &str, m_phi1: &str, m_phi2: &str) -> Self {
        Self {
            prefix: prefix.to_string(),
            n: n.to_string(),
            m_phi1: m_phi1.to_string(),
            m_phi2: m_phi2.to_string(),
        }
    }

    fn pv(&self, suffix: &str) -> String {
        format!("{}HR{}_{}", self.prefix, self.n, suffix)
    }

    fn pv_no_underscore(&self, suffix: &str) -> String {
        format!("{}HR{}{}", self.prefix, self.n, suffix)
    }

    fn motor_pv(&self, motor: &str, field: &str) -> String {
        format!("{}{}{}", self.prefix, motor, field)
    }
}

// ---------------------------------------------------------------------------
// Async runner
// ---------------------------------------------------------------------------

/// Run the HR analyzer crystal control state machine.
pub async fn run(
    config: HrConfig,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tokio::time::sleep(Duration::from_secs(3)).await;
    println!("hrCtl: starting for prefix={}HR{}", config.prefix, config.n);

    let my_origin = alloc_origin();

    // -- Create channels --
    let _ch_debug = DbChannel::new(&db, &config.pv_no_underscore("CtlDebug"));
    let ch_msg1 = DbChannel::new(&db, &config.pv("SeqMsg1SI"));
    let ch_msg2 = DbChannel::new(&db, &config.pv("SeqMsg2SI"));
    let ch_alert = DbChannel::new(&db, &config.pv("AlertBO"));
    let ch_oper_ack = DbChannel::new(&db, &config.pv("OperAckBO"));
    let ch_put_vals = DbChannel::new(&db, &config.pv("PutBO"));
    let ch_auto_mode = DbChannel::new(&db, &config.pv("ModeBO"));
    let ch_op_mode = DbChannel::new(&db, &config.pv("Mode2MO"));
    let ch_geom = DbChannel::new(&db, &config.pv_no_underscore("_GeomMO"));
    let ch_moving = DbChannel::new(&db, &config.pv("Moving"));

    // Crystal 1
    let ch_h1 = DbChannel::new(&db, &config.pv("H1AO"));
    let ch_k1 = DbChannel::new(&db, &config.pv("K1AO"));
    let ch_l1 = DbChannel::new(&db, &config.pv("L1AO"));
    let ch_a1 = DbChannel::new(&db, &config.pv("A1AO"));
    let ch_d1 = DbChannel::new(&db, &config.pv("2d1AO"));

    // Crystal 2
    let ch_h2 = DbChannel::new(&db, &config.pv("H2AO"));
    let ch_k2 = DbChannel::new(&db, &config.pv("K2AO"));
    let ch_l2 = DbChannel::new(&db, &config.pv("L2AO"));
    let ch_a2 = DbChannel::new(&db, &config.pv("A2AO"));
    let ch_d2 = DbChannel::new(&db, &config.pv("2d2AO"));

    // Energy / lambda
    let ch_e = DbChannel::with_origin(&db, &config.pv("EAO"), my_origin);
    let _ch_e_hi = DbChannel::new(&db, &config.pv("EAO.DRVH"));
    let _ch_e_lo = DbChannel::new(&db, &config.pv("EAO.DRVL"));
    let ch_e_rdbk = DbChannel::with_origin(&db, &config.pv("ERdbkAO"), my_origin);

    let ch_lambda = DbChannel::with_origin(&db, &config.pv("LambdaAO"), my_origin);
    let _ch_lambda_hi = DbChannel::new(&db, &config.pv("LambdaAO.DRVH"));
    let _ch_lambda_lo = DbChannel::new(&db, &config.pv("LambdaAO.DRVL"));
    let ch_lambda_rdbk = DbChannel::with_origin(&db, &config.pv("LambdaRdbkAO"), my_origin);

    // Theta 1/2
    let ch_theta1 = DbChannel::with_origin(&db, &config.pv("Theta1AO"), my_origin);
    let _ch_theta1_hi = DbChannel::new(&db, &config.pv("Theta1AO.DRVH"));
    let _ch_theta1_lo = DbChannel::new(&db, &config.pv("Theta1AO.DRVL"));
    let ch_theta1_rdbk = DbChannel::with_origin(&db, &config.pv("Theta1RdbkAO"), my_origin);

    let ch_theta2 = DbChannel::with_origin(&db, &config.pv("Theta2AO"), my_origin);
    let _ch_theta2_hi = DbChannel::new(&db, &config.pv("Theta2AO.DRVH"));
    let _ch_theta2_lo = DbChannel::new(&db, &config.pv("Theta2AO.DRVL"));
    let ch_theta2_rdbk = DbChannel::with_origin(&db, &config.pv("Theta2RdbkAO"), my_origin);

    // Phi 1/2
    let ch_phi1 = DbChannel::with_origin(&db, &config.pv("phi1AO"), my_origin);
    let ch_phi1_off = DbChannel::new(&db, &config.pv("phi1OffAO"));
    let _ch_phi1_hi = DbChannel::new(&db, &config.pv("phi1AO.DRVH"));
    let _ch_phi1_lo = DbChannel::new(&db, &config.pv("phi1AO.DRVL"));
    let ch_phi1_rdbk = DbChannel::with_origin(&db, &config.pv("phi1RdbkAO"), my_origin);

    let ch_phi2 = DbChannel::with_origin(&db, &config.pv("phi2AO"), my_origin);
    let ch_phi2_off = DbChannel::new(&db, &config.pv("phi2OffAO"));
    let _ch_phi2_hi = DbChannel::new(&db, &config.pv("phi2AO.DRVH"));
    let _ch_phi2_lo = DbChannel::new(&db, &config.pv("phi2AO.DRVL"));
    let ch_phi2_rdbk = DbChannel::with_origin(&db, &config.pv("phi2RdbkAO"), my_origin);

    // Echo PVs
    let ch_phi1_mot_name = DbChannel::new(&db, &config.pv("phi1PvSI"));
    let ch_phi2_mot_name = DbChannel::new(&db, &config.pv("phi2PvSI"));
    let _ch_phi1_cmd_echo = DbChannel::new(&db, &config.pv("phi1CmdAO"));
    let _ch_phi2_cmd_echo = DbChannel::new(&db, &config.pv("phi2CmdAO"));
    let ch_phi1_rdbk_echo = DbChannel::new(&db, &config.pv("phi1RdbkAI"));
    let ch_phi2_rdbk_echo = DbChannel::new(&db, &config.pv("phi2RdbkAI"));
    let ch_phi1_dmov_echo = DbChannel::new(&db, &config.pv("phi1DmovBI"));
    let ch_phi2_dmov_echo = DbChannel::new(&db, &config.pv("phi2DmovBI"));

    // Motor records
    let ch_phi1_mot_stop = DbChannel::new(&db, &config.motor_pv(&config.m_phi1, ".STOP"));
    let ch_phi2_mot_stop = DbChannel::new(&db, &config.motor_pv(&config.m_phi2, ".STOP"));
    let ch_phi1_dmov = DbChannel::new(&db, &config.motor_pv(&config.m_phi1, ".DMOV"));
    let ch_phi2_dmov = DbChannel::new(&db, &config.motor_pv(&config.m_phi2, ".DMOV"));
    let ch_phi1_hls = DbChannel::new(&db, &config.motor_pv(&config.m_phi1, ".HLS"));
    let ch_phi1_lls = DbChannel::new(&db, &config.motor_pv(&config.m_phi1, ".LLS"));
    let ch_phi2_hls = DbChannel::new(&db, &config.motor_pv(&config.m_phi2, ".HLS"));
    let ch_phi2_lls = DbChannel::new(&db, &config.motor_pv(&config.m_phi2, ".LLS"));

    let ch_phi1_set_ao = DbChannel::new(&db, &config.pv("phi1SetAO"));
    let ch_phi2_set_ao = DbChannel::new(&db, &config.pv("phi2SetAO"));

    let _ch_phi1_mot_hilim = DbChannel::new(&db, &config.motor_pv(&config.m_phi1, ".HLM"));
    let _ch_phi1_mot_lolim = DbChannel::new(&db, &config.motor_pv(&config.m_phi1, ".LLM"));
    let _ch_phi2_mot_hilim = DbChannel::new(&db, &config.motor_pv(&config.m_phi2, ".HLM"));
    let _ch_phi2_mot_lolim = DbChannel::new(&db, &config.motor_pv(&config.m_phi2, ".LLM"));

    let ch_phi1_mot_cmd = DbChannel::new(&db, &config.motor_pv(&config.m_phi1, ""));
    let ch_phi2_mot_cmd = DbChannel::new(&db, &config.motor_pv(&config.m_phi2, ""));
    let ch_phi1_mot_rbv = DbChannel::new(&db, &config.motor_pv(&config.m_phi1, ".RBV"));
    let ch_phi2_mot_rbv = DbChannel::new(&db, &config.motor_pv(&config.m_phi2, ".RBV"));

    let ch_world_off = DbChannel::new(&db, &config.pv("worldOffAO"));
    let _ch_use_set = DbChannel::new(&db, &config.pv("UseSetBO"));

    let _ch_theta_min = DbChannel::new(&db, &config.pv_no_underscore("_thetaMin"));
    let _ch_theta_max = DbChannel::new(&db, &config.pv_no_underscore("_thetaMax"));

    // Build multi-monitor for all event-driving PVs
    let monitored_pvs: Vec<String> = vec![
        config.pv("EAO"),
        config.pv("LambdaAO"),
        config.pv("Theta1AO"),
        config.pv("Theta2AO"),
        config.pv("H1AO"),
        config.pv("K1AO"),
        config.pv("L1AO"),
        config.pv("A1AO"),
        config.pv("H2AO"),
        config.pv("K2AO"),
        config.pv("L2AO"),
        config.pv("A2AO"),
        config.pv("PutBO"),
        config.pv("ModeBO"),
        config.pv("OperAckBO"),
        config.motor_pv(&config.m_phi1, ".RBV"),
        config.motor_pv(&config.m_phi2, ".RBV"),
        config.pv("worldOffAO"),
        config.pv("phi1OffAO"),
        config.pv("phi2OffAO"),
        config.pv("UseSetBO"),
        config.pv("Mode2MO"),
        config.pv_no_underscore("_GeomMO"),
        config.pv("Moving"),
    ];
    let mut monitor = DbMultiMonitor::new_filtered(&db, &monitored_pvs, my_origin).await;
    println!(
        "hrCtl: subscribed to {} PVs, {} active",
        monitored_pvs.len(),
        monitor.sub_count()
    );

    // -- Initialize --
    let _ = ch_put_vals.put_i16(0).await;
    let _ = ch_auto_mode.put_i16(0).await;
    let _ = ch_oper_ack.put_i16(0).await;

    let phi1_name = format!("{}{}", config.prefix, config.m_phi1);
    let phi2_name = format!("{}{}", config.prefix, config.m_phi2);
    let _ = ch_phi1_mot_name.put_string(&phi1_name).await;
    let _ = ch_phi2_mot_name.put_string(&phi2_name).await;

    // Read crystal parameters
    let mut h1 = ch_h1.get_f64().await;
    let mut k1 = ch_k1.get_f64().await;
    let mut l1 = ch_l1.get_f64().await;
    let mut a1 = ch_a1.get_f64().await;
    let (mut d1, _, _) = calc_2d_spacing(a1, h1, k1, l1);
    let _ = ch_d1.put_f64(d1).await;

    let mut h2 = ch_h2.get_f64().await;
    let mut k2 = ch_k2.get_f64().await;
    let mut l2 = ch_l2.get_f64().await;
    let mut a2 = ch_a2.get_f64().await;
    let (mut d2, _, _) = calc_2d_spacing(a2, h2, k2, l2);
    let _ = ch_d2.put_f64(d2).await;

    let mut op_mode = HrOpMode::from_i16(ch_op_mode.get_i16().await);
    let mut geom = HrGeometry::from_i16(ch_geom.get_i16().await);
    let mut phi1_off = ch_phi1_off.get_f64().await;
    let mut phi2_off = ch_phi2_off.get_f64().await;
    let mut world_off = ch_world_off.get_f64().await;
    let mut auto_mode = false;
    let mut use_set_mode = false;
    let mut _caused_move = false;

    // Read motor readbacks and compute initial angles
    let phi1_mot_rdbk = ch_phi1_mot_rbv.get_f64().await;
    let phi2_mot_rdbk = ch_phi2_mot_rbv.get_f64().await;
    let world_off_deg = world_off * UR2D;

    let mut phi1_val = motor_to_phi(phi1_mot_rdbk, phi1_off, world_off_deg);
    let mut theta1_val = phi1_val;
    let _ = ch_phi1.put_f64(phi1_val).await;
    let _ = ch_theta1.put_f64(theta1_val).await;

    let mut phi2_val = match geom {
        HrGeometry::Nested => motor_to_phi(phi2_mot_rdbk, phi2_off, world_off_deg),
        HrGeometry::Symmetric => motor_to_phi2_symmetric(phi2_mot_rdbk, phi2_off, world_off_deg),
    };
    let mut theta2_val = match geom {
        HrGeometry::Nested => phi2_val - phi1_val - theta1_val,
        HrGeometry::Symmetric => phi2_val,
    };
    let _ = ch_phi2.put_f64(phi2_val).await;
    let _ = ch_theta2.put_f64(theta2_val).await;

    // Compute initial lambda/E from theta1
    let mut lambda_val = d1 * (theta1_val * D2R).sin();
    let _ = ch_lambda.put_f64(lambda_val).await;
    let mut e_val = if lambda_val > 0.0 {
        HC / lambda_val
    } else {
        0.0
    };
    let _ = ch_e.put_f64(e_val).await;

    // Initial readback
    let rdbk = calc_readback(
        phi1_mot_rdbk,
        phi2_mot_rdbk,
        phi1_off,
        phi2_off,
        world_off,
        d1,
        d2,
        op_mode,
        geom,
    );
    let _ = ch_phi1_rdbk.put_f64_post(rdbk.phi1_rdbk).await;
    let _ = ch_theta1_rdbk.put_f64_post(rdbk.theta1_rdbk).await;
    let _ = ch_phi2_rdbk.put_f64_post(rdbk.phi2_rdbk).await;
    let _ = ch_theta2_rdbk.put_f64_post(rdbk.theta2_rdbk).await;
    let _ = ch_lambda_rdbk.put_f64_post(rdbk.lambda_rdbk).await;
    let _ = ch_e_rdbk.put_f64_post(rdbk.e_rdbk).await;

    let _ = ch_msg1.put_string("HR Control Ready").await;
    let _ = ch_msg2.put_string(" ").await;

    info!(
        "HR controller initialized for {}HR{}",
        config.prefix, config.n
    );

    // PV name constants for dispatch
    let pv_e = config.pv("EAO");
    let pv_lambda = config.pv("LambdaAO");
    let pv_theta1 = config.pv("Theta1AO");
    let pv_theta2 = config.pv("Theta2AO");
    let pv_h1 = config.pv("H1AO");
    let pv_k1 = config.pv("K1AO");
    let pv_l1 = config.pv("L1AO");
    let pv_a1 = config.pv("A1AO");
    let pv_h2 = config.pv("H2AO");
    let pv_k2 = config.pv("K2AO");
    let pv_l2 = config.pv("L2AO");
    let pv_a2 = config.pv("A2AO");
    let pv_put_vals = config.pv("PutBO");
    let pv_auto_mode = config.pv("ModeBO");
    let pv_oper_ack = config.pv("OperAckBO");
    let pv_phi1_mot_rbv = config.motor_pv(&config.m_phi1, ".RBV");
    let pv_phi2_mot_rbv = config.motor_pv(&config.m_phi2, ".RBV");
    let pv_world_off = config.pv("worldOffAO");
    let pv_phi1_off = config.pv("phi1OffAO");
    let pv_phi2_off = config.pv("phi2OffAO");
    let pv_use_set = config.pv("UseSetBO");
    let pv_op_mode = config.pv("Mode2MO");
    let pv_geom = config.pv_no_underscore("_GeomMO");

    println!("hrCtl: ready");

    // -- Main loop --
    let mut deferred_events: HashMap<String, f64> = HashMap::new();
    loop {
        let mut proceed_to_theta_changed = false;

        let (changed_pv, new_val) = if let Some(key) = deferred_events.keys().next().cloned() {
            let val = deferred_events.remove(&key).unwrap();
            (key, val)
        } else {
            monitor.wait_change().await
        };

        if changed_pv == pv_e {
            let new_e = new_val;
            if (new_e - e_val).abs() > 1e-15 {
                e_val = new_e;
                lambda_val = HC / e_val;
                let _ = ch_lambda.put_f64(lambda_val).await;
                proceed_to_theta_changed = true;
            }
        } else if changed_pv == pv_lambda {
            let new_l = new_val;
            if (new_l - lambda_val).abs() > 1e-15 {
                lambda_val = new_l;
                proceed_to_theta_changed = true;
            }
        } else if changed_pv == pv_theta1 {
            theta1_val = new_val;
            proceed_to_theta_changed = true;
        } else if changed_pv == pv_theta2 {
            theta2_val = new_val;
            proceed_to_theta_changed = true;
        } else if changed_pv == pv_h1
            || changed_pv == pv_k1
            || changed_pv == pv_l1
            || changed_pv == pv_a1
        {
            h1 = ch_h1.get_f64().await;
            k1 = ch_k1.get_f64().await;
            l1 = ch_l1.get_f64().await;
            a1 = ch_a1.get_f64().await;
            let (d, _, _) = calc_2d_spacing(a1, h1, k1, l1);
            d1 = d;
            let _ = ch_d1.put_f64(d1).await;
            auto_mode = false;
            let _ = ch_auto_mode.put_i16(0).await;
        } else if changed_pv == pv_h2
            || changed_pv == pv_k2
            || changed_pv == pv_l2
            || changed_pv == pv_a2
        {
            h2 = ch_h2.get_f64().await;
            k2 = ch_k2.get_f64().await;
            l2 = ch_l2.get_f64().await;
            a2 = ch_a2.get_f64().await;
            let (d, _, _) = calc_2d_spacing(a2, h2, k2, l2);
            d2 = d;
            let _ = ch_d2.put_f64(d2).await;
            auto_mode = false;
            let _ = ch_auto_mode.put_i16(0).await;
        } else if changed_pv == pv_put_vals {
            if new_val as i16 != 0 {
                proceed_to_theta_changed = true;
            }
        } else if changed_pv == pv_auto_mode {
            auto_mode = new_val as i16 != 0;
        } else if changed_pv == pv_oper_ack {
            if new_val as i16 != 0 {
                let _ = ch_alert.put_i16(0).await;
                let _ = ch_msg1.put_string(" ").await;
                let _ = ch_msg2.put_string(" ").await;
            }
        } else if changed_pv == pv_phi1_mot_rbv || changed_pv == pv_phi2_mot_rbv {
            let rdbk = calc_readback(
                ch_phi1_mot_rbv.get_f64().await,
                ch_phi2_mot_rbv.get_f64().await,
                phi1_off,
                phi2_off,
                world_off,
                d1,
                d2,
                op_mode,
                geom,
            );
            let _ = ch_phi1_rdbk.put_f64_post(rdbk.phi1_rdbk).await;
            let _ = ch_theta1_rdbk.put_f64_post(rdbk.theta1_rdbk).await;
            let _ = ch_lambda_rdbk.put_f64_post(rdbk.lambda_rdbk).await;
            let _ = ch_e_rdbk.put_f64_post(rdbk.e_rdbk).await;
            if op_mode != HrOpMode::Single {
                let _ = ch_phi2_rdbk.put_f64_post(rdbk.phi2_rdbk).await;
                let _ = ch_theta2_rdbk.put_f64_post(rdbk.theta2_rdbk).await;
            }
        } else if changed_pv == pv_world_off {
            world_off = ch_world_off.get_f64().await;
            proceed_to_theta_changed = true;
        } else if changed_pv == pv_phi1_off {
            phi1_off = ch_phi1_off.get_f64().await;
        } else if changed_pv == pv_phi2_off {
            phi2_off = ch_phi2_off.get_f64().await;
        } else if changed_pv == pv_use_set {
            use_set_mode = new_val as i16 != 0;
        } else if changed_pv == pv_op_mode {
            op_mode = HrOpMode::from_i16(new_val as i16);
            auto_mode = false;
            let _ = ch_auto_mode.put_i16(0).await;
            let _ = ch_msg2.put_string("Set to Manual Mode").await;
        } else if changed_pv == pv_geom {
            geom = HrGeometry::from_i16(new_val as i16);
            auto_mode = false;
            let _ = ch_auto_mode.put_i16(0).await;
            let _ = ch_msg1
                .put_string("New geometry. Switch Phi 2 motor dir.")
                .await;
            let _ = ch_alert.put_i16(1).await;
        }

        if !proceed_to_theta_changed {
            continue;
        }

        // -- Lambda changed processing --
        if d1 > 0.0 && lambda_val > d1 {
            let _ = ch_msg1
                .put_string("Wavelength > 2d spacing of crystal 1.")
                .await;
            let _ = ch_alert.put_i16(1).await;
        } else if d2 > 0.0 && lambda_val > d2 && op_mode != HrOpMode::Single {
            let _ = ch_msg1
                .put_string("Wavelength > 2d spacing of crystal 2.")
                .await;
            let _ = ch_alert.put_i16(1).await;
        } else {
            // Compute theta from lambda
            match op_mode {
                HrOpMode::Single => {
                    if d1 > 0.0 {
                        theta1_val = (lambda_val / d1).asin() * R2D;
                    }
                    let _ = ch_theta1.put_f64(theta1_val).await;
                }
                HrOpMode::TwoLocked => {
                    if d1 > 0.0 {
                        theta1_val = (lambda_val / d1).asin() * R2D;
                    }
                    if d2 > 0.0 {
                        theta2_val = (lambda_val / d2).asin() * R2D;
                    }
                    let _ = ch_theta1.put_f64(theta1_val).await;
                    let _ = ch_theta2.put_f64(theta2_val).await;
                }
                HrOpMode::TwoIndependent => {
                    if d2 > 0.0 {
                        theta2_val = (lambda_val / d2).asin() * R2D;
                    }
                    let _ = ch_theta2.put_f64(theta2_val).await;
                }
            }
        }

        // -- Theta changed -> calc phi, lambda --
        let world_off_deg = world_off * UR2D;
        match op_mode {
            HrOpMode::Single | HrOpMode::TwoLocked => {
                lambda_val = d1 * (theta1_val * D2R).sin();
                phi1_val = theta1_to_phi1(theta1_val);
                if op_mode == HrOpMode::TwoLocked {
                    phi2_val = match geom {
                        HrGeometry::Nested => calc_phi2_nested(phi1_val, theta1_val, theta2_val),
                        HrGeometry::Symmetric => calc_phi2_symmetric(theta2_val),
                    };
                }
            }
            HrOpMode::TwoIndependent => {
                let lambda_nom = d1 * (theta1_val * D2R).sin();
                let theta2_nom = if d2 > 0.0 {
                    (lambda_nom / d2).asin() * R2D
                } else {
                    0.0
                };
                lambda_val = d2 * (theta2_val * D2R).sin();
                phi1_val = theta1_to_phi1(theta1_val);
                phi2_val = match geom {
                    HrGeometry::Nested => calc_phi2_independent_nested(
                        phi1_val, theta1_val, theta2_nom, lambda_val, lambda_nom, d1, d2,
                    ),
                    HrGeometry::Symmetric => calc_phi2_independent_symmetric(
                        theta2_nom, lambda_val, lambda_nom, d1, d2, theta1_val,
                    ),
                };
            }
        }
        let _ = ch_phi1.put_f64(phi1_val).await;
        if op_mode != HrOpMode::Single {
            let _ = ch_phi2.put_f64(phi2_val).await;
        }
        let _ = ch_lambda.put_f64(lambda_val).await;
        e_val = if lambda_val > 0.0 {
            HC / lambda_val
        } else {
            0.0
        };
        let _ = ch_e.put_f64(e_val).await;

        // Update readbacks
        let rdbk = calc_readback(
            ch_phi1_mot_rbv.get_f64().await,
            ch_phi2_mot_rbv.get_f64().await,
            phi1_off,
            phi2_off,
            world_off,
            d1,
            d2,
            op_mode,
            geom,
        );
        let _ = ch_phi1_rdbk.put_f64_post(rdbk.phi1_rdbk).await;
        let _ = ch_theta1_rdbk.put_f64_post(rdbk.theta1_rdbk).await;
        let _ = ch_lambda_rdbk.put_f64_post(rdbk.lambda_rdbk).await;
        let _ = ch_e_rdbk.put_f64_post(rdbk.e_rdbk).await;
        if op_mode != HrOpMode::Single {
            let _ = ch_phi2_rdbk.put_f64_post(rdbk.phi2_rdbk).await;
            let _ = ch_theta2_rdbk.put_f64_post(rdbk.theta2_rdbk).await;
        }

        // -- Calc motor movements --
        if use_set_mode {
            let phi1_mot_cur = ch_phi1_mot_rbv.get_f64().await;
            phi1_off = phi1_val - phi1_mot_cur / D2UR;
            let _ = ch_phi1_off.put_f64(phi1_off).await;
            if op_mode != HrOpMode::Single {
                let phi2_mot_cur = ch_phi2_mot_rbv.get_f64().await;
                phi2_off = phi2_val - phi2_mot_cur / D2UR;
                let _ = ch_phi2_off.put_f64(phi2_off).await;
            }
            let _ = ch_put_vals.put_i16(0).await;
        }

        let phi1_mot_desired = phi_to_motor(phi1_val, phi1_off, world_off_deg);
        let _ = ch_phi1_set_ao.put_f64(phi1_mot_desired).await;

        let phi2_mot_desired = if op_mode != HrOpMode::Single {
            let v = match geom {
                HrGeometry::Nested => phi_to_motor(phi2_val, phi2_off, world_off_deg),
                HrGeometry::Symmetric => phi2_to_motor_symmetric(phi2_val, phi2_off, world_off_deg),
            };
            let _ = ch_phi2_set_ao.put_f64(v).await;
            v
        } else {
            0.0
        };

        // -- Move motors --
        let put_requested = ch_put_vals.get_i16().await != 0;
        if (auto_mode || put_requested) && !use_set_mode {
            let _ = ch_phi1_mot_cmd.put_f64_process(phi1_mot_desired).await;
            if op_mode != HrOpMode::Single {
                let _ = ch_phi2_mot_cmd.put_f64_process(phi2_mot_desired).await;
            }
            let _ = ch_put_vals.put_i16(0).await;
            _caused_move = true;

            // Wait for motors
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;

                // Check limit switches
                if ch_phi1_hls.get_i16().await != 0 || ch_phi1_lls.get_i16().await != 0 {
                    let _ = ch_msg1
                        .put_string("Theta 1 motor hit a limit switch!")
                        .await;
                    let _ = ch_alert.put_i16(1).await;
                    auto_mode = false;
                    let _ = ch_auto_mode.put_i16(0).await;
                    let _ = ch_phi1_mot_stop.put_i16(1).await;
                    if op_mode != HrOpMode::Single {
                        let _ = ch_phi2_mot_stop.put_i16(1).await;
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    break;
                }
                if op_mode != HrOpMode::Single
                    && (ch_phi2_hls.get_i16().await != 0 || ch_phi2_lls.get_i16().await != 0)
                {
                    let _ = ch_msg1
                        .put_string("Theta 2 motor hit a limit switch!")
                        .await;
                    let _ = ch_alert.put_i16(1).await;
                    auto_mode = false;
                    let _ = ch_auto_mode.put_i16(0).await;
                    let _ = ch_phi1_mot_stop.put_i16(1).await;
                    let _ = ch_phi2_mot_stop.put_i16(1).await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    break;
                }

                // Update readbacks
                let rdbk = calc_readback(
                    ch_phi1_mot_rbv.get_f64().await,
                    ch_phi2_mot_rbv.get_f64().await,
                    phi1_off,
                    phi2_off,
                    world_off,
                    d1,
                    d2,
                    op_mode,
                    geom,
                );
                let _ = ch_phi1_rdbk.put_f64_post(rdbk.phi1_rdbk).await;
                let _ = ch_theta1_rdbk.put_f64_post(rdbk.theta1_rdbk).await;
                let _ = ch_lambda_rdbk.put_f64_post(rdbk.lambda_rdbk).await;
                let _ = ch_e_rdbk.put_f64_post(rdbk.e_rdbk).await;
                if op_mode != HrOpMode::Single {
                    let _ = ch_phi2_rdbk.put_f64_post(rdbk.phi2_rdbk).await;
                    let _ = ch_theta2_rdbk.put_f64_post(rdbk.theta2_rdbk).await;
                }

                let d1_done = ch_phi1_dmov.get_i16().await != 0;
                let d2_done = if op_mode != HrOpMode::Single {
                    ch_phi2_dmov.get_i16().await != 0
                } else {
                    true
                };
                if d1_done && d2_done {
                    break;
                }
            }
            _caused_move = false;
            let _ = ch_moving.put_i16(0).await;
        }

        // Echo updates
        let _ = ch_phi1_rdbk_echo
            .put_f64(ch_phi1_mot_rbv.get_f64().await)
            .await;
        let _ = ch_phi2_rdbk_echo
            .put_f64(ch_phi2_mot_rbv.get_f64().await)
            .await;
        let _ = ch_phi1_dmov_echo
            .put_i16(ch_phi1_dmov.get_i16().await)
            .await;
        let _ = ch_phi2_dmov_echo
            .put_i16(ch_phi2_dmov.get_i16().await)
            .await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calc_2d_spacing() {
        let (two_d, forb, _) = calc_2d_spacing(5.4309, 1.0, 1.0, 1.0);
        assert!((two_d - 6.2712).abs() < 0.01);
        assert!(!forb);
    }

    #[test]
    fn test_theta1_to_phi1() {
        assert_eq!(theta1_to_phi1(14.0), 14.0);
    }

    #[test]
    fn test_phi2_nested() {
        // phi2 = phi1 + theta1 + theta2
        let phi2 = calc_phi2_nested(14.0, 14.0, 15.0);
        assert!((phi2 - 43.0).abs() < 1e-10);
    }

    #[test]
    fn test_phi2_symmetric() {
        assert_eq!(calc_phi2_symmetric(15.0), 15.0);
    }

    #[test]
    fn test_motor_phi_roundtrip() {
        let phi = 14.5;
        let phi_off = 0.1;
        let world = 0.05;
        let motor = phi_to_motor(phi, phi_off, world);
        let phi2 = motor_to_phi(motor, phi_off, world);
        assert!(
            (phi - phi2).abs() < 1e-8,
            "phi roundtrip: {} vs {}",
            phi,
            phi2
        );
    }

    #[test]
    fn test_motor_phi2_symmetric_roundtrip() {
        let phi = 14.5;
        let phi_off = 0.1;
        let world = 0.05;
        let motor = phi2_to_motor_symmetric(phi, phi_off, world);
        let phi2 = motor_to_phi2_symmetric(motor, phi_off, world);
        assert!((phi - phi2).abs() < 1e-8);
    }

    #[test]
    fn test_calc_readback_single() {
        let d1 = 6.2712;
        let d2 = 6.0;
        let phi1_off = 0.0;
        let phi2_off = 0.0;
        let world_off = 0.0;
        // Motor value in micro-radians for theta1=14 deg
        let theta1_deg = 14.0;
        let phi1_mot = theta1_deg * D2UR;
        let rdbk = calc_readback(
            phi1_mot,
            0.0,
            phi1_off,
            phi2_off,
            world_off,
            d1,
            d2,
            HrOpMode::Single,
            HrGeometry::Nested,
        );
        // lambda = d1 * sin(theta1)
        let expected_lambda = d1 * (theta1_deg * D2R).sin();
        assert!(
            (rdbk.lambda_rdbk - expected_lambda).abs() < 1e-6,
            "lambda: {} vs {}",
            rdbk.lambda_rdbk,
            expected_lambda
        );
        assert!(
            (rdbk.theta1_rdbk - theta1_deg).abs() < 0.01,
            "theta1: {} vs {}",
            rdbk.theta1_rdbk,
            theta1_deg
        );
        assert!(rdbk.e_rdbk > 0.0);
    }

    #[test]
    fn test_calc_readback_two_locked_nested() {
        let d1 = 6.2712;
        let d2 = 6.0;
        let theta1 = 14.0;
        let theta2 = (d1 * (theta1 * D2R).sin() / d2).asin() * R2D;
        let phi1 = theta1;
        let phi2 = phi1 + theta1 + theta2;
        let phi1_mot = phi1 * D2UR;
        let phi2_mot = phi2 * D2UR;
        let rdbk = calc_readback(
            phi1_mot,
            phi2_mot,
            0.0,
            0.0,
            0.0,
            d1,
            d2,
            HrOpMode::TwoLocked,
            HrGeometry::Nested,
        );
        let expected_lambda = d1 * (theta1 * D2R).sin();
        assert!((rdbk.lambda_rdbk - expected_lambda).abs() < 1e-4);
    }

    #[test]
    fn test_op_mode_from_i16() {
        assert_eq!(HrOpMode::from_i16(0), HrOpMode::Single);
        assert_eq!(HrOpMode::from_i16(1), HrOpMode::TwoLocked);
        assert_eq!(HrOpMode::from_i16(2), HrOpMode::TwoIndependent);
        assert_eq!(HrOpMode::from_i16(99), HrOpMode::Single);
    }

    #[test]
    fn test_hr_geometry() {
        assert_eq!(HrGeometry::from_i16(0), HrGeometry::Nested);
        assert_eq!(HrGeometry::from_i16(1), HrGeometry::Symmetric);
    }

    #[test]
    fn test_config_pv() {
        let cfg = HrConfig::new("xxx:", "1", "m9", "m10");
        assert_eq!(cfg.pv("EAO"), "xxx:HR1_EAO");
        assert_eq!(cfg.pv_no_underscore("CtlDebug"), "xxx:HR1CtlDebug");
        assert_eq!(cfg.motor_pv("m9", ".RBV"), "xxx:m9.RBV");
    }
}
