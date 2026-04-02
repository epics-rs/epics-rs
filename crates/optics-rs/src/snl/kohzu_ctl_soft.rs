//! Kohzu soft-motor monochromator control -- native Rust port of `kohzuCtl_soft.st`.
//!
//! Identical physics to `kohzu_ctl`, but uses a separate MONO prefix for the
//! soft PV names, allowing multiple monochromator instances on one IOC.
//!
//! PV naming: `{P}{MONO}E`, `{P}{MONO}Lambda`, `{P}{MONO}Theta`, etc.
//! Motor PVs still use `{P}{M_THETA}`, `{P}{M_Y}`, `{P}{M_Z}`.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use tracing::info;

use crate::db_access::{DbChannel, DbMultiMonitor, alloc_origin};

// Re-use physics from kohzu_ctl.
use crate::snl::kohzu_ctl::{
    CrystalMode, Geometry, calc_2d_spacing, calc_y_position, calc_z_position, clamp_theta,
    compute_energy_lambda_limits, compute_theta_limits, coordinate_speeds, energy_to_lambda,
    lambda_to_energy, lambda_to_theta, theta_to_lambda,
};

// ---------------------------------------------------------------------------
// State enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KohzuSoftState {
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
// Configuration
// ---------------------------------------------------------------------------

/// PV name configuration for the soft-motor Kohzu variant.
pub struct KohzuSoftConfig {
    /// IOC prefix, e.g. "xxx:".
    pub prefix: String,
    /// Mono prefix, e.g. "Kohzu1:".
    pub mono: String,
    /// Motor record names.
    pub m_theta: String,
    pub m_y: String,
    pub m_z: String,
    /// Geometry type.
    pub geom: Geometry,
}

impl KohzuSoftConfig {
    pub fn new(prefix: &str, mono: &str, m_theta: &str, m_y: &str, m_z: &str, geom: i32) -> Self {
        Self {
            prefix: prefix.to_string(),
            mono: mono.to_string(),
            m_theta: m_theta.to_string(),
            m_y: m_y.to_string(),
            m_z: m_z.to_string(),
            geom: Geometry::from_i32(geom),
        }
    }

    /// Build a mono-prefixed PV name: {P}{MONO}suffix
    fn mono_pv(&self, suffix: &str) -> String {
        format!("{}{}{}", self.prefix, self.mono, suffix)
    }

    /// Build a motor PV name: {P}{motor}field
    fn motor_pv(&self, motor: &str, field: &str) -> String {
        format!("{}{}{}", self.prefix, motor, field)
    }
}

// ---------------------------------------------------------------------------
// Async runner
// ---------------------------------------------------------------------------

/// Run the soft-motor Kohzu monochromator control state machine.
pub async fn run(
    config: KohzuSoftConfig,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tokio::time::sleep(Duration::from_secs(3)).await;
    println!(
        "kohzuCtl_soft: starting for prefix={}{}",
        config.prefix, config.mono
    );

    let my_origin = alloc_origin();

    // -- Create channels --
    let _ch_debug = DbChannel::new(&db, &config.mono_pv("CtlDebug"));
    let ch_seq_msg1 = DbChannel::new(&db, &config.mono_pv("SeqMsg1"));
    let ch_seq_msg2 = DbChannel::new(&db, &config.mono_pv("SeqMsg2"));
    let ch_alert = DbChannel::new(&db, &config.mono_pv("Alert"));
    let ch_oper_ack = DbChannel::new(&db, &config.mono_pv("OperAck"));
    let ch_put_vals = DbChannel::new(&db, &config.mono_pv("Put"));
    let ch_auto_mode = DbChannel::new(&db, &config.mono_pv("Mode"));
    let ch_cc_mode = DbChannel::new(&db, &config.mono_pv("Mode2"));
    let ch_moving = DbChannel::new(&db, &config.mono_pv("Moving"));

    // Crystal parameters
    let ch_h = DbChannel::new(&db, &config.mono_pv("H"));
    let ch_k = DbChannel::new(&db, &config.mono_pv("K"));
    let ch_l = DbChannel::new(&db, &config.mono_pv("L"));
    let ch_a = DbChannel::new(&db, &config.mono_pv("A"));
    let ch_d = DbChannel::new(&db, &config.mono_pv("2dSpacing"));

    // Energy / lambda / theta
    let ch_e = DbChannel::new(&db, &config.mono_pv("E"));
    let ch_e_hi = DbChannel::new(&db, &config.mono_pv("E.HLM"));
    let ch_e_lo = DbChannel::new(&db, &config.mono_pv("E.LLM"));
    let ch_e_rdbk = DbChannel::new(&db, &config.mono_pv("ERdbk"));

    let ch_lambda = DbChannel::new(&db, &config.mono_pv("Lambda"));
    let ch_lambda_hi = DbChannel::new(&db, &config.mono_pv("Lambda.HLM"));
    let ch_lambda_lo = DbChannel::new(&db, &config.mono_pv("Lambda.LLM"));
    let ch_lambda_rdbk = DbChannel::new(&db, &config.mono_pv("LambdaRdbk"));

    let ch_theta = DbChannel::new(&db, &config.mono_pv("Theta"));
    let ch_theta_hi = DbChannel::new(&db, &config.mono_pv("Theta.HLM"));
    let ch_theta_lo = DbChannel::new(&db, &config.mono_pv("Theta.LLM"));
    let ch_theta_rdbk = DbChannel::new(&db, &config.mono_pv("ThetaRdbk"));

    // Soft echo PVs
    let ch_theta_mot_name = DbChannel::new(&db, &config.mono_pv("ThetaPv"));
    let ch_y_mot_name = DbChannel::new(&db, &config.mono_pv("YPv"));
    let ch_z_mot_name = DbChannel::new(&db, &config.mono_pv("ZPv"));

    let _ch_theta_cmd_echo = DbChannel::new(&db, &config.mono_pv("ThetaCmd"));
    let _ch_y_cmd_echo = DbChannel::new(&db, &config.mono_pv("YCmd"));
    let _ch_z_cmd_echo = DbChannel::new(&db, &config.mono_pv("ZCmd"));
    let ch_theta_rdbk_echo = DbChannel::new(&db, &config.mono_pv("ThetaMotRdbk"));
    let ch_y_rdbk_echo = DbChannel::new(&db, &config.mono_pv("YRdbk"));
    let ch_z_rdbk_echo = DbChannel::new(&db, &config.mono_pv("ZRdbk"));
    let ch_theta_vel_echo = DbChannel::new(&db, &config.mono_pv("ThetaVel"));
    let ch_y_vel_echo = DbChannel::new(&db, &config.mono_pv("YVel"));
    let ch_z_vel_echo = DbChannel::new(&db, &config.mono_pv("ZVel"));
    let ch_theta_dmov_echo = DbChannel::new(&db, &config.mono_pv("ThetaDmov"));
    let ch_y_dmov_echo = DbChannel::new(&db, &config.mono_pv("YDmov"));
    let ch_z_dmov_echo = DbChannel::new(&db, &config.mono_pv("ZDmov"));

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

    let ch_theta_set_ao = DbChannel::new(&db, &config.mono_pv("ThetaSet"));
    let ch_y_set_ao = DbChannel::new(&db, &config.mono_pv("YSet"));
    let ch_z_set_ao = DbChannel::new(&db, &config.mono_pv("ZSet"));
    let ch_y_set_hi = DbChannel::new(&db, &config.mono_pv("YSet.DRVH"));
    let ch_y_set_lo = DbChannel::new(&db, &config.mono_pv("YSet.DRVL"));

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

    let _ch_use_set = DbChannel::new(&db, &config.mono_pv("UseSet"));
    let ch_theta_mot_set = DbChannel::new(&db, &config.motor_pv(&config.m_theta, ".SET"));
    let ch_y_mot_set = DbChannel::new(&db, &config.motor_pv(&config.m_y, ".SET"));
    let ch_z_mot_set = DbChannel::new(&db, &config.motor_pv(&config.m_z, ".SET"));

    let ch_speed_ctrl = DbChannel::new(&db, &config.mono_pv("SpeedCtrl"));
    let ch_y_offset = DbChannel::new(&db, &config.mono_pv("yOffset"));
    let ch_y_offset_hi = DbChannel::new(&db, &config.mono_pv("yOffset.DRVH"));
    let ch_y_offset_lo = DbChannel::new(&db, &config.mono_pv("yOffset.DRVL"));

    // Wait for key channels

    // Build multi-monitor for all event-driving PVs
    let monitored_pvs: Vec<String> = vec![
        config.mono_pv("E"),
        config.mono_pv("Lambda"),
        config.mono_pv("Theta"),
        config.mono_pv("H"),
        config.mono_pv("K"),
        config.mono_pv("L"),
        config.mono_pv("A"),
        config.mono_pv("Put"),
        config.mono_pv("Mode"),
        config.mono_pv("Mode2"),
        config.mono_pv("OperAck"),
        config.motor_pv(&config.m_theta, ".RBV"),
        config.motor_pv(&config.m_theta, ".HLM"),
        config.motor_pv(&config.m_theta, ".LLM"),
        config.mono_pv("yOffset"),
        config.mono_pv("UseSet"),
    ];
    let mut monitor = DbMultiMonitor::new_filtered(&db, &monitored_pvs, my_origin).await;
    println!(
        "kohzuCtl_soft: subscribed to {} PVs, {} active",
        monitored_pvs.len(),
        monitor.sub_count()
    );

    let geom = config.geom;

    // Motor names
    let theta_name = format!("{}{}", config.prefix, config.m_theta);
    let y_name = format!("{}{}", config.prefix, config.m_y);
    let z_name = format!("{}{}", config.prefix, config.m_z);
    let _ = ch_theta_mot_name.put_string(&theta_name).await;
    let _ = ch_y_mot_name.put_string(&y_name).await;
    let _ = ch_z_mot_name.put_string(&z_name).await;

    // Geometry init
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

    // Crystal parameters
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

    // Theta/energy limits
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

    // Initial readbacks
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
    let mut cc_mode = CrystalMode::from_i16(ch_cc_mode.get_i16().await);
    let mut y_offset_val = ch_y_offset.get_f64().await;
    let mut _caused_move = false;
    let risk_averse = false;

    let _ = ch_seq_msg1.put_string("Kohzu Control Ready").await;
    let _ = ch_seq_msg2.put_string(" ").await;

    info!(
        "Kohzu soft controller initialized for {}{}",
        config.prefix, config.mono
    );

    // PV name constants for dispatch
    let pv_e = config.mono_pv("E");
    let pv_lambda = config.mono_pv("Lambda");
    let pv_theta = config.mono_pv("Theta");
    let pv_h = config.mono_pv("H");
    let pv_k = config.mono_pv("K");
    let pv_l = config.mono_pv("L");
    let pv_a = config.mono_pv("A");
    let pv_put_vals = config.mono_pv("Put");
    let pv_auto_mode = config.mono_pv("Mode");
    let pv_cc_mode = config.mono_pv("Mode2");
    let pv_oper_ack = config.mono_pv("OperAck");
    let pv_theta_mot_rbv = config.motor_pv(&config.m_theta, ".RBV");
    let pv_theta_hilim = config.motor_pv(&config.m_theta, ".HLM");
    let pv_theta_lolim = config.motor_pv(&config.m_theta, ".LLM");
    let pv_y_offset = config.mono_pv("yOffset");
    let pv_use_set = config.mono_pv("UseSet");

    println!(
        "kohzuCtl_soft: ready (two_d={:.4}, theta=[{:.1}..{:.1}])",
        two_d, theta_lo, theta_hi
    );

    let mut deferred_events: HashMap<String, f64> = HashMap::new();
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
        } else if changed_pv == pv_h
            || changed_pv == pv_k
            || changed_pv == pv_l
            || changed_pv == pv_a
        {
            h = ch_h.get_f64().await;
            k = ch_k.get_f64().await;
            l = ch_l.get_f64().await;
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
            if new_val as i16 != 0 {
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
            e_rdbk_val = lambda_to_energy(lambda_rdbk_val);
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
            let sv = if use_set_mode { 1i16 } else { 0 };
            let _ = ch_theta_mot_set.put_i16(sv).await;
            let _ = ch_y_mot_set.put_i16(sv).await;
            let _ = ch_z_mot_set.put_i16(sv).await;
        }

        if !proceed_to_theta_changed {
            continue;
        }

        // -- Theta-changed processing --
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
        let y_mot_desired = calc_y_position(geom, theta_val, y_offset_val);
        let z_mot_desired = calc_z_position(geom, theta_val, y_offset_val);
        let _ = ch_theta_set_ao.put_f64(theta_mot_desired).await;
        let _ = ch_y_set_ao.put_f64(y_mot_desired).await;
        let _ = ch_z_set_ao.put_f64(z_mot_desired).await;

        // Check limits
        let mut will_violate = false;
        let y_hi = ch_y_mot_hilim.get_f64().await;
        let y_lo = ch_y_mot_lolim.get_f64().await;
        let z_hi = ch_z_mot_hilim.get_f64().await;
        let z_lo = ch_z_mot_lolim.get_f64().await;

        if !cc_mode.y_frozen() && (y_mot_desired < y_lo || y_mot_desired > y_hi) {
            let _ = ch_seq_msg1.put_string("Y will exceed soft limits").await;
            let _ = ch_alert.put_i16(1).await;
            will_violate = true;
        }
        if !cc_mode.z_frozen() && (z_mot_desired < z_lo || z_mot_desired > z_hi) {
            let _ = ch_seq_msg1.put_string("Z will exceed soft limits").await;
            let _ = ch_alert.put_i16(1).await;
            will_violate = true;
        }

        if will_violate {
            let _ = ch_seq_msg2.put_string("Command ignored").await;
            let _ = ch_moving.put_i16(0).await;
            continue;
        }

        // -- Move if appropriate --
        let put_requested = ch_put_vals.get_i16().await != 0;
        if auto_mode || put_requested || use_set_mode {
            let speed_control = ch_speed_ctrl.get_i16().await != 0;
            if speed_control {
                let th_speed = ch_theta_mot_velo.get_f64().await;
                let y_speed = ch_y_mot_velo.get_f64().await;
                let z_speed = ch_z_mot_velo.get_f64().await;
                let (new_th, new_y, new_z) = coordinate_speeds(
                    theta_val - current_rbv,
                    y_mot_desired - ch_y_mot_rbv.get_f64().await,
                    z_mot_desired - ch_z_mot_rbv.get_f64().await,
                    th_speed,
                    y_speed,
                    z_speed,
                    cc_mode,
                );
                let _ = ch_theta_mot_velo.put_f64(new_th).await;
                if !cc_mode.y_frozen() {
                    let _ = ch_y_mot_velo.put_f64(new_y).await;
                }
                if !cc_mode.z_frozen() {
                    let _ = ch_z_mot_velo.put_f64(new_z).await;
                }
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

            // Wait for motors
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let th_dmov = ch_theta_dmov.get_i16().await;
                let y_dmov = ch_y_dmov.get_i16().await;
                let z_dmov = ch_z_dmov.get_i16().await;

                if ch_theta_hls.get_i16().await != 0 || ch_theta_lls.get_i16().await != 0 {
                    let _ = ch_seq_msg1
                        .put_string("Theta Motor hit a limit switch!")
                        .await;
                    let _ = ch_alert.put_i16(1).await;
                    auto_mode = false;
                    let _ = ch_auto_mode.put_i16(0).await;
                    let _ = ch_theta_mot_stop.put_i16(1).await;
                    let _ = ch_y_stop.put_i16(1).await;
                    let _ = ch_z_stop.put_i16(1).await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    break;
                }
                if !cc_mode.y_frozen()
                    && (ch_y_hls.get_i16().await != 0 || ch_y_lls.get_i16().await != 0)
                {
                    let _ = ch_seq_msg1.put_string("Y Motor hit a limit switch!").await;
                    let _ = ch_alert.put_i16(1).await;
                    auto_mode = false;
                    let _ = ch_auto_mode.put_i16(0).await;
                    let _ = ch_theta_mot_stop.put_i16(1).await;
                    let _ = ch_y_stop.put_i16(1).await;
                    let _ = ch_z_stop.put_i16(1).await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    break;
                }
                if !cc_mode.z_frozen()
                    && (ch_z_hls.get_i16().await != 0 || ch_z_lls.get_i16().await != 0)
                {
                    let _ = ch_seq_msg1.put_string("Z Motor hit a limit switch!").await;
                    let _ = ch_alert.put_i16(1).await;
                    auto_mode = false;
                    let _ = ch_auto_mode.put_i16(0).await;
                    let _ = ch_theta_mot_stop.put_i16(1).await;
                    let _ = ch_y_stop.put_i16(1).await;
                    let _ = ch_z_stop.put_i16(1).await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    break;
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

            _caused_move = false;

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
// Tests -- physics is tested in kohzu_ctl; here we verify config building.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_pv_names() {
        let cfg = KohzuSoftConfig::new("xxx:", "Kohzu1:", "m9", "m10", "m11", 1);
        assert_eq!(cfg.mono_pv("E"), "xxx:Kohzu1:E");
        assert_eq!(cfg.motor_pv("m9", ".RBV"), "xxx:m9.RBV");
        assert_eq!(cfg.mono_pv("Lambda"), "xxx:Kohzu1:Lambda");
    }

    #[test]
    fn test_geometry_from_i32() {
        assert_eq!(Geometry::from_i32(1), Geometry::Standard);
        assert_eq!(Geometry::from_i32(2), Geometry::Alternate);
        assert_eq!(Geometry::from_i32(99), Geometry::Standard);
    }
}
