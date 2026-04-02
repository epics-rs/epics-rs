//! Multi-layer monochromator control -- native Rust port of `ml_monoCtl.st`.
//!
//! Controls a multilayer monochromator with variable d-spacing (not crystal).
//! Key difference from Kohzu: uses `D` (layer spacing) and `Order` instead of
//! Miller indices (H,K,L) and lattice constant. The effective 2d-spacing is
//! `2d = 2*D/Order`.
//!
//! Motors: Theta, Theta2 (parallel crystals), Z (optional, for beam tracking).
//! The Y motor provides the y-offset readback but is not commanded.
//!
//! Modes: Normal (Z moves) and Freeze-Z (channel-cut, Z frozen).
//! Z position: z = y_offset / tan(2*theta).

use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use tracing::info;

use crate::db_access::{DbChannel, DbMultiMonitor, alloc_origin};
use crate::snl::kohzu_ctl::HC;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const RAD_CONV: f64 = 57.29577951308232;

// ---------------------------------------------------------------------------
// Operating modes
// ---------------------------------------------------------------------------

/// Multi-layer mono mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MlMonoMode {
    /// Normal -- Z motor moves.
    Normal = 0,
    /// Freeze Z -- channel-cut, Z frozen.
    FreezeZ = 1,
}

impl MlMonoMode {
    pub fn from_i16(v: i16) -> Self {
        if v == 1 { Self::FreezeZ } else { Self::Normal }
    }

    pub fn z_frozen(self) -> bool {
        self == Self::FreezeZ
    }
}

// ---------------------------------------------------------------------------
// State enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MlMonoState {
    Init,
    InitSequence,
    WaitForCommand,
    DInputChanged,
    ThetaLimits,
    EChanged,
    LambdaChanged,
    ThetaChanged,
    CalcMovements,
    MoveMlMono,
    UpdateReadback,
    CheckDone,
    ThetaMotorStopped,
    CheckMotorLimits,
    StopMlMono,
}

// ---------------------------------------------------------------------------
// Pure physics functions
// ---------------------------------------------------------------------------

/// Calculate effective 2d-spacing from layer spacing D and diffraction order.
/// Returns (two_d, is_error, message).
pub fn calc_2d_spacing(d: f64, order: f64) -> (f64, bool, &'static str) {
    if order < 1.0 {
        return (0.0, true, "Order must be >= 1");
    }
    let two_d = (2.0 * d) / order;
    (two_d, false, "New effective d spacing")
}

/// Convert energy (keV) to wavelength (Angstrom).
pub fn energy_to_lambda(e: f64) -> f64 {
    if e <= 0.0 {
        return f64::INFINITY;
    }
    HC / e
}

/// Convert wavelength to energy.
pub fn lambda_to_energy(lambda: f64) -> f64 {
    if lambda <= 0.0 {
        return f64::INFINITY;
    }
    HC / lambda
}

/// Convert wavelength to theta (degrees) given 2d spacing.
/// Returns None if impossible.
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

/// Convert theta (degrees) to wavelength given 2d spacing.
pub fn theta_to_lambda(theta_deg: f64, two_d: f64) -> f64 {
    two_d * (theta_deg / RAD_CONV).sin()
}

/// Calculate Z motor position for the multilayer geometry.
/// z = y_offset / tan(2*theta)
pub fn calc_z_position(theta_deg: f64, y_offset: f64) -> f64 {
    let two_theta_rad = 2.0 * theta_deg / RAD_CONV;
    let tan_val = two_theta_rad.tan();
    if tan_val.abs() < 1e-15 {
        return f64::MAX;
    }
    y_offset / tan_val
}

/// Compute theta limits, clamped to [0.1, 89.0] degrees.
pub fn compute_theta_limits(motor_hi: f64, motor_lo: f64) -> (f64, f64) {
    let hi = motor_hi.min(89.0);
    let lo = motor_lo.max(0.1);
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

/// Coordinate speeds for theta and Z motors.
/// Returns (new_th_speed, new_z_speed).
pub fn coordinate_speeds(
    theta_delta: f64,
    z_delta: f64,
    th_speed: f64,
    z_speed: f64,
    cc_mode: MlMonoMode,
) -> (f64, f64) {
    let th_time = if th_speed > 0.0 {
        theta_delta.abs() / th_speed
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

    let max_time = th_time.max(z_time);
    if max_time <= 0.0 {
        return (th_speed, z_speed);
    }

    let new_th = if theta_delta.abs() > 0.0 {
        theta_delta.abs() / max_time
    } else {
        th_speed
    };
    let new_z = if z_delta.abs() > 0.0 {
        z_delta.abs() / max_time
    } else {
        z_speed
    };
    (new_th, new_z)
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

pub struct MlMonoConfig {
    pub prefix: String,
    pub m_theta: String,
    pub m_theta2: String,
    pub m_y: String,
    pub m_z: String,
    pub y_offset: f64,
    pub geom: i32,
}

impl MlMonoConfig {
    pub fn new(
        prefix: &str,
        m_theta: &str,
        m_theta2: &str,
        m_y: &str,
        m_z: &str,
        y_offset: f64,
        geom: i32,
    ) -> Self {
        let y_off = if !(1.0..=60.0).contains(&y_offset) {
            35.0
        } else {
            y_offset
        };
        Self {
            prefix: prefix.to_string(),
            m_theta: m_theta.to_string(),
            m_theta2: m_theta2.to_string(),
            m_y: m_y.to_string(),
            m_z: m_z.to_string(),
            y_offset: y_off,
            geom,
        }
    }

    fn pv(&self, suffix: &str) -> String {
        format!("{}ml_mono{}", self.prefix, suffix)
    }

    fn motor_pv(&self, motor: &str, field: &str) -> String {
        format!("{}{}{}", self.prefix, motor, field)
    }
}

// ---------------------------------------------------------------------------
// Async runner
// ---------------------------------------------------------------------------

/// Run the multi-layer monochromator control state machine.
pub async fn run(
    config: MlMonoConfig,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tokio::time::sleep(Duration::from_secs(3)).await;
    println!("ml_monoCtl: starting for prefix={}", config.prefix);

    let my_origin = alloc_origin();

    // -- Create channels --
    let _ch_debug = DbChannel::new(&db, &config.pv("CtlDebug"));
    let ch_msg1 = DbChannel::new(&db, &config.pv("SeqMsg1"));
    let ch_msg2 = DbChannel::new(&db, &config.pv("SeqMsg2"));
    let ch_alert = DbChannel::new(&db, &config.pv("Alert"));
    let ch_oper_ack = DbChannel::new(&db, &config.pv("OperAck"));
    let ch_put_vals = DbChannel::new(&db, &config.pv("Put"));
    let ch_auto_mode = DbChannel::new(&db, &config.pv("Mode"));
    let ch_cc_mode = DbChannel::new(&db, &config.pv("Mode2"));
    let ch_moving = DbChannel::new(&db, &config.pv("Moving"));

    // D-spacing parameters
    let ch_order = DbChannel::new(&db, &config.pv("Order"));
    let ch_d = DbChannel::new(&db, &config.pv("D"));

    // Energy / lambda / theta
    let ch_e = DbChannel::new(&db, &config.pv("E"));
    let ch_e_hi = DbChannel::new(&db, &config.pv("E.DRVH"));
    let ch_e_lo = DbChannel::new(&db, &config.pv("E.DRVL"));
    let ch_e_rdbk = DbChannel::new(&db, &config.pv("ERdbk"));

    let ch_lambda = DbChannel::new(&db, &config.pv("Lambda"));
    let ch_lambda_hi = DbChannel::new(&db, &config.pv("Lambda.DRVH"));
    let ch_lambda_lo = DbChannel::new(&db, &config.pv("Lambda.DRVL"));
    let ch_lambda_rdbk = DbChannel::new(&db, &config.pv("LambdaRdbk"));

    let ch_theta = DbChannel::new(&db, &config.pv("Theta"));
    let ch_theta_hi = DbChannel::new(&db, &config.pv("Theta.DRVH"));
    let ch_theta_lo = DbChannel::new(&db, &config.pv("Theta.DRVL"));
    let ch_theta_rdbk = DbChannel::new(&db, &config.pv("ThetaRdbk"));

    // Echo PVs
    let ch_theta_mot_name = DbChannel::new(&db, &config.pv("ThetaPv"));
    let ch_theta2_mot_name = DbChannel::new(&db, &config.pv("Theta2Pv"));
    let ch_z_mot_name = DbChannel::new(&db, &config.pv("ZPv"));
    let ch_y_mot_name = DbChannel::new(&db, &config.pv("YPv"));

    let _ch_theta_cmd_echo = DbChannel::new(&db, &config.pv("ThetaCmd"));
    let _ch_theta2_cmd_echo = DbChannel::new(&db, &config.pv("Theta2Cmd"));
    let _ch_z_cmd_echo = DbChannel::new(&db, &config.pv("ZCmd"));
    let ch_theta_rdbk_echo = DbChannel::new(&db, &config.pv("ThetaRdbkEcho"));
    let ch_theta2_rdbk_echo = DbChannel::new(&db, &config.pv("Theta2RdbkEcho"));
    let ch_z_rdbk_echo = DbChannel::new(&db, &config.pv("ZRdbk"));
    let ch_theta_vel_echo = DbChannel::new(&db, &config.pv("ThetaVel"));
    let ch_theta2_vel_echo = DbChannel::new(&db, &config.pv("Theta2Vel"));
    let ch_z_vel_echo = DbChannel::new(&db, &config.pv("ZVel"));
    let ch_theta_dmov_echo = DbChannel::new(&db, &config.pv("ThetaDmov"));
    let ch_theta2_dmov_echo = DbChannel::new(&db, &config.pv("Theta2Dmov"));
    let ch_z_dmov_echo = DbChannel::new(&db, &config.pv("ZDmov"));

    // Motor records
    let ch_theta_mot_stop = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".STOP"));
    let ch_theta2_mot_stop = DbChannel::new(&db, &config.motor_pv(&config.m_theta2, ".STOP"));
    let ch_z_stop = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".STOP"));

    let ch_theta_dmov = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".DMOV"));
    let ch_theta2_dmov = DbChannel::new(&db, &config.motor_pv(&config.m_theta2, ".DMOV"));
    let ch_z_dmov = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".DMOV"));

    let ch_theta_hls = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".HLS"));
    let ch_theta_lls = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".LLS"));
    let ch_theta2_hls = DbChannel::new(&db, &config.motor_pv(&config.m_theta2, ".HLS"));
    let ch_theta2_lls = DbChannel::new(&db, &config.motor_pv(&config.m_theta2, ".LLS"));
    let ch_z_hls = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".HLS"));
    let ch_z_lls = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".LLS"));

    let ch_theta_set = DbChannel::new(&db, &config.pv("ThetaSet"));
    let ch_theta2_set = DbChannel::new(&db, &config.pv("Theta2Set"));
    let ch_z_set = DbChannel::new(&db, &config.pv("ZSet"));

    let ch_theta_mot_hilim = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".HLM"));
    let ch_theta_mot_lolim = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".LLM"));
    let ch_z_mot_hilim = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".HLM"));
    let ch_z_mot_lolim = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".LLM"));

    let ch_theta_mot_cmd = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ""));
    let ch_theta2_mot_cmd = DbChannel::new(&db, &config.motor_pv(&config.m_theta2, ""));
    let ch_z_mot_cmd = DbChannel::new(&db, &config.motor_pv(&config.m_z, ""));

    let ch_theta_mot_velo = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".VELO"));
    let ch_theta2_mot_velo = DbChannel::new(&db, &config.motor_pv(&config.m_theta2, ".VELO"));
    let ch_z_mot_velo = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".VELO"));

    let ch_theta_mot_rbv = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".RBV"));
    let ch_theta2_mot_rbv = DbChannel::new(&db, &config.motor_pv(&config.m_theta2, ".RBV"));
    let ch_z_mot_rbv = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".RBV"));
    let ch_y_mot_rbv = DbChannel::new(&db, &config.motor_pv(&config.m_y, ".RBV"));

    let _ch_use_set = DbChannel::new(&db, &config.pv("UseSet"));
    let ch_theta_mot_set_flag = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".SET"));
    let ch_theta2_mot_set_flag = DbChannel::new(&db, &config.motor_pv(&config.m_theta2, ".SET"));
    let ch_z_mot_set_flag = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".SET"));

    let ch_y_offset = DbChannel::new(&db, &config.pv("_yOffset"));

    // Wait for key channels

    // Build multi-monitor
    let monitored_pvs: Vec<String> = vec![
        config.pv("E"),
        config.pv("Lambda"),
        config.pv("Theta"),
        config.pv("Order"),
        config.pv("D"),
        config.pv("Put"),
        config.pv("Mode"),
        config.pv("Mode2"),
        config.pv("OperAck"),
        config.motor_pv(&config.m_theta, ".RBV"),
        config.motor_pv(&config.m_theta2, ".RBV"),
        config.motor_pv(&config.m_theta, ".HLM"),
        config.motor_pv(&config.m_theta, ".LLM"),
        config.pv("_yOffset"),
        config.pv("UseSet"),
    ];
    let mut monitor = DbMultiMonitor::new_filtered(&db, &monitored_pvs, my_origin).await;
    println!(
        "ml_monoCtl: subscribed to {} PVs, {} active",
        monitored_pvs.len(),
        monitor.sub_count()
    );

    // -- Initialize --
    let theta_name = format!("{}{}", config.prefix, config.m_theta);
    let theta2_name = format!("{}{}", config.prefix, config.m_theta2);
    let z_name = format!("{}{}", config.prefix, config.m_z);
    let y_name = format!("{}{}", config.prefix, config.m_y);
    let _ = ch_theta_mot_name.put_string(&theta_name).await;
    let _ = ch_theta2_mot_name.put_string(&theta2_name).await;
    let _ = ch_z_mot_name.put_string(&z_name).await;
    let _ = ch_y_mot_name.put_string(&y_name).await;

    let _ = ch_y_offset.put_f64(config.y_offset).await;
    let _ = ch_put_vals.put_i16(0).await;
    let _ = ch_auto_mode.put_i16(0).await;
    let _ = ch_oper_ack.put_i16(0).await;

    // D-spacing
    let mut order_val = ch_order.get_f64().await;
    let mut d_val = ch_d.get_f64().await;
    let (mut two_d, err, msg) = calc_2d_spacing(d_val, order_val);
    let _ = ch_msg1.put_string(msg).await;
    if err {
        let _ = ch_alert.put_i16(1).await;
    }

    // Theta limits
    let mut theta_mot_hi = ch_theta_mot_hilim.get_f64().await;
    let mut theta_mot_lo = ch_theta_mot_lolim.get_f64().await;
    let (mut theta_hi, mut theta_lo) = compute_theta_limits(theta_mot_hi, theta_mot_lo);
    let _ = ch_theta_hi.put_f64(theta_hi).await;
    let _ = ch_theta_lo.put_f64(theta_lo).await;
    let (e_hi, e_lo, l_hi, l_lo) = compute_energy_lambda_limits(two_d, theta_hi, theta_lo);
    let _ = ch_e_hi.put_f64(e_hi).await;
    let _ = ch_e_lo.put_f64(e_lo).await;
    let _ = ch_lambda_hi.put_f64(l_hi).await;
    let _ = ch_lambda_lo.put_f64(l_lo).await;

    // Initial readback
    let theta_mot_rdbk = ch_theta_mot_rbv.get_f64().await;
    let mut theta_rdbk_val = theta_mot_rdbk;
    let mut lambda_rdbk_val = theta_to_lambda(theta_rdbk_val, two_d);
    let mut e_rdbk_val = lambda_to_energy(lambda_rdbk_val);
    let _ = ch_theta_rdbk.put_f64(theta_rdbk_val).await;
    let _ = ch_lambda_rdbk.put_f64(lambda_rdbk_val).await;
    let _ = ch_e_rdbk.put_f64(e_rdbk_val).await;

    let mut theta_val = theta_mot_rdbk;
    let _ = ch_theta.put_f64(theta_val).await;
    let mut lambda_val = theta_to_lambda(theta_val, two_d);
    let _ = ch_lambda.put_f64(lambda_val).await;
    let mut e_val = lambda_to_energy(lambda_val);
    let _ = ch_e.put_f64(e_val).await;

    let mut auto_mode = false;
    let mut use_set_mode = false;
    let mut cc_mode = MlMonoMode::from_i16(ch_cc_mode.get_i16().await);
    let mut y_offset_val = config.y_offset;
    let mut _caused_move = false;

    let _ = ch_msg1.put_string("ml_mono Control Ready").await;
    let _ = ch_msg2.put_string(" ").await;

    info!("ML mono controller initialized for {}", config.prefix);

    // PV name constants for dispatch
    let pv_e = config.pv("E");
    let pv_lambda = config.pv("Lambda");
    let pv_theta = config.pv("Theta");
    let pv_order = config.pv("Order");
    let pv_d = config.pv("D");
    let pv_put_vals = config.pv("Put");
    let pv_auto_mode = config.pv("Mode");
    let pv_cc_mode = config.pv("Mode2");
    let pv_oper_ack = config.pv("OperAck");
    let pv_theta_mot_rbv = config.motor_pv(&config.m_theta, ".RBV");
    let pv_theta2_mot_rbv = config.motor_pv(&config.m_theta2, ".RBV");
    let pv_theta_hilim = config.motor_pv(&config.m_theta, ".HLM");
    let pv_theta_lolim = config.motor_pv(&config.m_theta, ".LLM");
    let pv_y_offset = config.pv("_yOffset");
    let pv_use_set = config.pv("UseSet");

    println!(
        "ml_monoCtl: ready (two_d={:.4}, theta=[{:.1}..{:.1}])",
        two_d, theta_lo, theta_hi
    );

    let mut deferred_events: std::collections::HashMap<String, f64> =
        std::collections::HashMap::new();
    // -- Main loop --
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
            if (new_e - e_val).abs() > 1e-12 {
                e_val = new_e;
                lambda_val = energy_to_lambda(e_val);
                let _ = ch_lambda.put_f64(lambda_val).await;
                if lambda_val > two_d {
                    let _ = ch_msg1.put_string("Wavelength > 2d spacing.").await;
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
                    let _ = ch_msg1.put_string("Wavelength > 2d spacing.").await;
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
        } else if changed_pv == pv_order {
            order_val = ch_order.get_f64().await;
            let (td, err, msg) = calc_2d_spacing(d_val, order_val);
            two_d = td;
            let _ = ch_msg1.put_string(msg).await;
            if err {
                let _ = ch_alert.put_i16(1).await;
            }
            auto_mode = false;
            let _ = ch_auto_mode.put_i16(0).await;
            let _ = ch_msg2.put_string("Set to Manual Mode").await;
            let (eh, el, lh, ll) = compute_energy_lambda_limits(two_d, theta_hi, theta_lo);
            let _ = ch_e_hi.put_f64(eh).await;
            let _ = ch_e_lo.put_f64(el).await;
            let _ = ch_lambda_hi.put_f64(lh).await;
            let _ = ch_lambda_lo.put_f64(ll).await;
        } else if changed_pv == pv_d {
            d_val = ch_d.get_f64().await;
            let (td, err, msg) = calc_2d_spacing(d_val, order_val);
            two_d = td;
            let _ = ch_msg1.put_string(msg).await;
            if err {
                let _ = ch_alert.put_i16(1).await;
            }
            auto_mode = false;
            let _ = ch_auto_mode.put_i16(0).await;
            let _ = ch_msg2.put_string("Set to Manual Mode").await;
            let (eh, el, lh, ll) = compute_energy_lambda_limits(two_d, theta_hi, theta_lo);
            let _ = ch_e_hi.put_f64(eh).await;
            let _ = ch_e_lo.put_f64(el).await;
            let _ = ch_lambda_hi.put_f64(lh).await;
            let _ = ch_lambda_lo.put_f64(ll).await;
        } else if changed_pv == pv_put_vals {
            if new_val as i16 != 0 {
                proceed_to_theta_changed = true;
            }
        } else if changed_pv == pv_auto_mode {
            auto_mode = new_val as i16 != 0;
        } else if changed_pv == pv_cc_mode {
            cc_mode = MlMonoMode::from_i16(new_val as i16);
        } else if changed_pv == pv_oper_ack {
            if new_val as i16 != 0 {
                let _ = ch_alert.put_i16(0).await;
                let _ = ch_msg1.put_string(" ").await;
                let _ = ch_msg2.put_string(" ").await;
                let _ = ch_oper_ack.put_i16(0).await;
            }
        } else if changed_pv == pv_theta_mot_rbv {
            let rbv = new_val;
            let _ = ch_theta_rdbk_echo.put_f64(rbv).await;
            theta_rdbk_val = rbv;
            lambda_rdbk_val = theta_to_lambda(theta_rdbk_val, two_d);
            e_rdbk_val = lambda_to_energy(lambda_rdbk_val);
            let _ = ch_theta_rdbk.put_f64_post(theta_rdbk_val).await;
            let _ = ch_lambda_rdbk.put_f64_post(lambda_rdbk_val).await;
            let _ = ch_e_rdbk.put_f64_post(e_rdbk_val).await;
        } else if changed_pv == pv_theta2_mot_rbv {
            let _ = ch_theta2_rdbk_echo.put_f64(new_val).await;
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
            let _ = ch_msg1
                .put_string(&format!("y offset changed to {:.4}", y_offset_val))
                .await;
            let _ = ch_msg2.put_string("Set to Manual Mode").await;
            proceed_to_theta_changed = true;
        } else if changed_pv == pv_use_set {
            use_set_mode = new_val as i16 != 0;
            let sv = if use_set_mode { 1i16 } else { 0 };
            let _ = ch_theta_mot_set_flag.put_i16(sv).await;
            let _ = ch_theta2_mot_set_flag.put_i16(sv).await;
            let _ = ch_z_mot_set_flag.put_i16(sv).await;
        }

        if !proceed_to_theta_changed {
            continue;
        }

        // -- Theta changed --
        if theta_val <= theta_lo || theta_val >= theta_hi {
            let _ = ch_msg1.put_string("Theta constrained to LIMIT").await;
            let _ = ch_alert.put_i16(1).await;
            auto_mode = false;
            let _ = ch_auto_mode.put_i16(0).await;
            let _ = ch_msg2.put_string("Set to Manual Mode").await;
        }

        lambda_val = theta_to_lambda(theta_val, two_d);
        let _ = ch_lambda.put_f64(lambda_val).await;
        e_val = lambda_to_energy(lambda_val);
        let _ = ch_e.put_f64(e_val).await;

        let current_rbv = ch_theta_mot_rbv.get_f64().await;
        theta_rdbk_val = current_rbv;
        lambda_rdbk_val = theta_to_lambda(theta_rdbk_val, two_d);
        e_rdbk_val = lambda_to_energy(lambda_rdbk_val);
        let _ = ch_theta_rdbk.put_f64_post(theta_rdbk_val).await;
        let _ = ch_lambda_rdbk.put_f64_post(lambda_rdbk_val).await;
        let _ = ch_e_rdbk.put_f64_post(e_rdbk_val).await;

        // -- Calc movements --
        let theta_mot_desired = theta_val;
        let theta2_mot_desired = theta_val;
        let z_mot_desired = calc_z_position(theta_val, y_offset_val);

        let _ = ch_theta_set.put_f64(theta_mot_desired).await;
        let _ = ch_theta2_set.put_f64(theta2_mot_desired).await;
        let _ = ch_z_set.put_f64(z_mot_desired).await;

        // Check Z limits
        if !cc_mode.z_frozen() {
            let z_hi = ch_z_mot_hilim.get_f64().await;
            let z_lo = ch_z_mot_lolim.get_f64().await;
            if z_mot_desired < z_lo || z_mot_desired > z_hi {
                let _ = ch_msg1.put_string("Z will exceed soft limits").await;
                let _ = ch_msg2.put_string("Setting to Manual Mode").await;
                let _ = ch_alert.put_i16(1).await;
                auto_mode = false;
                let _ = ch_auto_mode.put_i16(0).await;
            }
        }

        // -- Move --
        let put_requested = ch_put_vals.get_i16().await != 0;
        if auto_mode || put_requested || use_set_mode {
            let th_speed = ch_theta_mot_velo.get_f64().await;
            let z_speed = ch_z_mot_velo.get_f64().await;
            let (new_th, new_z) = coordinate_speeds(
                theta_val - current_rbv,
                z_mot_desired - ch_z_mot_rbv.get_f64().await,
                th_speed,
                z_speed,
                cc_mode,
            );
            let _ = ch_theta_mot_velo.put_f64(new_th).await;
            let _ = ch_theta2_mot_velo.put_f64(new_th).await;
            if !cc_mode.z_frozen() {
                let _ = ch_z_mot_velo.put_f64(new_z).await;
            }

            let _ = ch_theta_mot_cmd.put_f64_process(theta_mot_desired).await;
            let _ = ch_theta2_mot_cmd.put_f64_process(theta2_mot_desired).await;
            if !cc_mode.z_frozen() {
                let _ = ch_z_mot_cmd.put_f64_process(z_mot_desired).await;
            }
            let _ = ch_put_vals.put_i16(0).await;
            _caused_move = true;

            // Wait for motors
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let th_dmov = ch_theta_dmov.get_i16().await;
                let th2_dmov = ch_theta2_dmov.get_i16().await;
                let z_dmov = ch_z_dmov.get_i16().await;

                if ch_theta_hls.get_i16().await != 0
                    || ch_theta_lls.get_i16().await != 0
                    || ch_theta2_hls.get_i16().await != 0
                    || ch_theta2_lls.get_i16().await != 0
                {
                    let _ = ch_msg1.put_string("Theta Motor hit a limit switch!").await;
                    let _ = ch_alert.put_i16(1).await;
                    auto_mode = false;
                    let _ = ch_auto_mode.put_i16(0).await;
                    let _ = ch_theta_mot_stop.put_i16(1).await;
                    let _ = ch_theta2_mot_stop.put_i16(1).await;
                    let _ = ch_z_stop.put_i16(1).await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    break;
                }
                if !cc_mode.z_frozen()
                    && (ch_z_hls.get_i16().await != 0 || ch_z_lls.get_i16().await != 0)
                {
                    let _ = ch_msg1.put_string("Z Motor hit a limit switch!").await;
                    let _ = ch_alert.put_i16(1).await;
                    auto_mode = false;
                    let _ = ch_auto_mode.put_i16(0).await;
                    let _ = ch_theta_mot_stop.put_i16(1).await;
                    let _ = ch_theta2_mot_stop.put_i16(1).await;
                    let _ = ch_z_stop.put_i16(1).await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    break;
                }

                let rbv = ch_theta_mot_rbv.get_f64().await;
                theta_rdbk_val = rbv;
                lambda_rdbk_val = theta_to_lambda(theta_rdbk_val, two_d);
                e_rdbk_val = lambda_to_energy(lambda_rdbk_val);
                let _ = ch_theta_rdbk.put_f64_post(theta_rdbk_val).await;
                let _ = ch_lambda_rdbk.put_f64_post(lambda_rdbk_val).await;
                let _ = ch_e_rdbk.put_f64_post(e_rdbk_val).await;
                let _ = ch_theta_rdbk_echo.put_f64(rbv).await;
                let _ = ch_theta2_rdbk_echo
                    .put_f64(ch_theta2_mot_rbv.get_f64().await)
                    .await;

                if th_dmov != 0 && th2_dmov != 0 && z_dmov != 0 {
                    break;
                }
            }

            if _caused_move {
                _caused_move = false;
            }

            // Final readback
            let rbv = ch_theta_mot_rbv.get_f64().await;
            theta_rdbk_val = rbv;
            lambda_rdbk_val = theta_to_lambda(theta_rdbk_val, two_d);
            e_rdbk_val = lambda_to_energy(lambda_rdbk_val);
            let _ = ch_theta_rdbk.put_f64_post(theta_rdbk_val).await;
            let _ = ch_lambda_rdbk.put_f64_post(lambda_rdbk_val).await;
            let _ = ch_e_rdbk.put_f64_post(e_rdbk_val).await;
            let _ = ch_moving.put_i16(0).await;
        }

        // Update echoes
        let _ = ch_theta_rdbk_echo
            .put_f64(ch_theta_mot_rbv.get_f64().await)
            .await;
        let _ = ch_theta2_rdbk_echo
            .put_f64(ch_theta2_mot_rbv.get_f64().await)
            .await;
        let _ = ch_z_rdbk_echo.put_f64(ch_z_mot_rbv.get_f64().await).await;
        let _ = ch_theta_vel_echo
            .put_f64(ch_theta_mot_velo.get_f64().await)
            .await;
        let _ = ch_theta2_vel_echo
            .put_f64(ch_theta2_mot_velo.get_f64().await)
            .await;
        let _ = ch_z_vel_echo.put_f64(ch_z_mot_velo.get_f64().await).await;
        let _ = ch_theta_dmov_echo
            .put_i16(ch_theta_dmov.get_i16().await)
            .await;
        let _ = ch_theta2_dmov_echo
            .put_i16(ch_theta2_dmov.get_i16().await)
            .await;
        let _ = ch_z_dmov_echo.put_i16(ch_z_dmov.get_i16().await).await;

        // Update y offset from readback
        let y_rbv = ch_y_mot_rbv.get_f64().await;
        y_offset_val = y_rbv;
        let _ = ch_y_offset.put_f64(y_offset_val).await;
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
        let (two_d, err, _) = calc_2d_spacing(25.0, 1.0);
        assert!(!err);
        assert!((two_d - 50.0).abs() < 1e-10);
    }

    #[test]
    fn test_calc_2d_spacing_order2() {
        let (two_d, err, _) = calc_2d_spacing(25.0, 2.0);
        assert!(!err);
        assert!((two_d - 25.0).abs() < 1e-10);
    }

    #[test]
    fn test_calc_2d_spacing_invalid_order() {
        let (_, err, _) = calc_2d_spacing(25.0, 0.5);
        assert!(err);
    }

    #[test]
    fn test_energy_lambda_roundtrip() {
        for e in [5.0, 10.0, 20.0, 50.0] {
            let l = energy_to_lambda(e);
            let e2 = lambda_to_energy(l);
            assert!((e - e2).abs() < 1e-10);
        }
    }

    #[test]
    fn test_lambda_to_theta() {
        let two_d = 50.0;
        let lambda = 2.0;
        let theta = lambda_to_theta(lambda, two_d).unwrap();
        // sin(theta) = 2/50 = 0.04, theta = 2.29 deg
        assert!((theta - 2.292).abs() < 0.01, "theta={}", theta);
    }

    #[test]
    fn test_theta_to_lambda_roundtrip() {
        let two_d = 50.0;
        let theta = 5.0;
        let lambda = theta_to_lambda(theta, two_d);
        let theta2 = lambda_to_theta(lambda, two_d).unwrap();
        assert!((theta - theta2).abs() < 1e-10);
    }

    #[test]
    fn test_calc_z_position() {
        // z = y_offset / tan(2*theta)
        let z = calc_z_position(30.0, 35.0);
        // tan(60 deg) = 1.7321
        let expected = 35.0 / (60.0_f64 / RAD_CONV).tan();
        assert!((z - expected).abs() < 0.01, "z={} expected={}", z, expected);
    }

    #[test]
    fn test_compute_theta_limits() {
        assert_eq!(compute_theta_limits(100.0, -5.0), (89.0, 0.1));
        assert_eq!(compute_theta_limits(45.0, 5.0), (45.0, 5.0));
    }

    #[test]
    fn test_coordinate_speeds() {
        let (th, z) = coordinate_speeds(10.0, 20.0, 1.0, 1.0, MlMonoMode::Normal);
        // Z takes 20s, so theta should slow to 0.5
        assert!((th - 0.5).abs() < 1e-6);
        assert!((z - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_coordinate_speeds_frozen() {
        let (th, z) = coordinate_speeds(10.0, 20.0, 1.0, 1.0, MlMonoMode::FreezeZ);
        // Z frozen, only theta matters
        assert!((th - 1.0).abs() < 1e-6);
        // z_delta != 0 but z_time = 0, so z_speed adjusted to theta_time
        assert!((z - 2.0).abs() < 1e-6);
    }

    #[test]
    fn test_ml_mono_mode() {
        assert!(!MlMonoMode::Normal.z_frozen());
        assert!(MlMonoMode::FreezeZ.z_frozen());
    }

    #[test]
    fn test_config_pv_names() {
        let cfg = MlMonoConfig::new("xxx:", "m9", "m12", "m10", "m11", 35.0, 1);
        assert_eq!(cfg.pv("E"), "xxx:ml_monoE");
        assert_eq!(cfg.motor_pv("m9", ".RBV"), "xxx:m9.RBV");
        assert_eq!(cfg.y_offset, 35.0);
    }

    #[test]
    fn test_config_y_offset_clamp() {
        let cfg = MlMonoConfig::new("xxx:", "m9", "m12", "m10", "m11", 0.5, 1);
        assert_eq!(cfg.y_offset, 35.0); // Clamped to default
        let cfg2 = MlMonoConfig::new("xxx:", "m9", "m12", "m10", "m11", 100.0, 1);
        assert_eq!(cfg2.y_offset, 35.0); // Clamped to default
    }
}
