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
    let _ch_debug = DbChannel::with_origin(&db, &config.mono_pv("CtlDebug"), my_origin);
    let ch_seq_msg1 = DbChannel::with_origin(&db, &config.mono_pv("SeqMsg1"), my_origin);
    let ch_seq_msg2 = DbChannel::with_origin(&db, &config.mono_pv("SeqMsg2"), my_origin);
    let ch_alert = DbChannel::with_origin(&db, &config.mono_pv("Alert"), my_origin);
    let ch_oper_ack = DbChannel::with_origin(&db, &config.mono_pv("OperAck"), my_origin);
    let ch_put_vals = DbChannel::with_origin(&db, &config.mono_pv("Put"), my_origin);
    let ch_auto_mode = DbChannel::with_origin(&db, &config.mono_pv("Mode"), my_origin);
    let ch_cc_mode = DbChannel::with_origin(&db, &config.mono_pv("Mode2"), my_origin);
    let ch_moving = DbChannel::with_origin(&db, &config.mono_pv("Moving"), my_origin);

    // Crystal parameters
    let ch_h = DbChannel::with_origin(&db, &config.mono_pv("H"), my_origin);
    let ch_k = DbChannel::with_origin(&db, &config.mono_pv("K"), my_origin);
    let ch_l = DbChannel::with_origin(&db, &config.mono_pv("L"), my_origin);
    let ch_a = DbChannel::with_origin(&db, &config.mono_pv("A"), my_origin);
    let ch_d = DbChannel::with_origin(&db, &config.mono_pv("2dSpacing"), my_origin);

    // Energy / lambda / theta
    let ch_e = DbChannel::with_origin(&db, &config.mono_pv("E"), my_origin);
    let ch_e_hi = DbChannel::with_origin(&db, &config.mono_pv("E.HLM"), my_origin);
    let ch_e_lo = DbChannel::with_origin(&db, &config.mono_pv("E.LLM"), my_origin);
    let ch_e_rdbk = DbChannel::with_origin(&db, &config.mono_pv("ERdbk"), my_origin);

    let ch_lambda = DbChannel::with_origin(&db, &config.mono_pv("Lambda"), my_origin);
    let ch_lambda_hi = DbChannel::with_origin(&db, &config.mono_pv("Lambda.HLM"), my_origin);
    let ch_lambda_lo = DbChannel::with_origin(&db, &config.mono_pv("Lambda.LLM"), my_origin);
    let ch_lambda_rdbk = DbChannel::with_origin(&db, &config.mono_pv("LambdaRdbk"), my_origin);

    let ch_theta = DbChannel::with_origin(&db, &config.mono_pv("Theta"), my_origin);
    let ch_theta_hi = DbChannel::with_origin(&db, &config.mono_pv("Theta.HLM"), my_origin);
    let ch_theta_lo = DbChannel::with_origin(&db, &config.mono_pv("Theta.LLM"), my_origin);
    let ch_theta_rdbk = DbChannel::with_origin(&db, &config.mono_pv("ThetaRdbk"), my_origin);

    // Soft echo PVs
    let ch_theta_mot_name = DbChannel::with_origin(&db, &config.mono_pv("ThetaPv"), my_origin);
    let ch_y_mot_name = DbChannel::with_origin(&db, &config.mono_pv("YPv"), my_origin);
    let ch_z_mot_name = DbChannel::with_origin(&db, &config.mono_pv("ZPv"), my_origin);

    let _ch_theta_cmd_echo = DbChannel::with_origin(&db, &config.mono_pv("ThetaCmd"), my_origin);
    let _ch_y_cmd_echo = DbChannel::with_origin(&db, &config.mono_pv("YCmd"), my_origin);
    let _ch_z_cmd_echo = DbChannel::with_origin(&db, &config.mono_pv("ZCmd"), my_origin);
    let ch_theta_rdbk_echo =
        DbChannel::with_origin(&db, &config.mono_pv("ThetaMotRdbk"), my_origin);
    let ch_y_rdbk_echo = DbChannel::with_origin(&db, &config.mono_pv("YRdbk"), my_origin);
    let ch_z_rdbk_echo = DbChannel::with_origin(&db, &config.mono_pv("ZRdbk"), my_origin);
    let ch_theta_vel_echo = DbChannel::with_origin(&db, &config.mono_pv("ThetaVel"), my_origin);
    let ch_y_vel_echo = DbChannel::with_origin(&db, &config.mono_pv("YVel"), my_origin);
    let ch_z_vel_echo = DbChannel::with_origin(&db, &config.mono_pv("ZVel"), my_origin);
    let ch_theta_dmov_echo = DbChannel::with_origin(&db, &config.mono_pv("ThetaDmov"), my_origin);
    let ch_y_dmov_echo = DbChannel::with_origin(&db, &config.mono_pv("YDmov"), my_origin);
    let ch_z_dmov_echo = DbChannel::with_origin(&db, &config.mono_pv("ZDmov"), my_origin);

    // Motor records
    let ch_theta_mot_stop =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_theta, ".STOP"), my_origin);
    let ch_y_stop = DbChannel::with_origin(&db, &config.motor_pv(&config.m_y, ".STOP"), my_origin);
    let ch_z_stop = DbChannel::with_origin(&db, &config.motor_pv(&config.m_z, ".STOP"), my_origin);

    let ch_theta_dmov =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_theta, ".DMOV"), my_origin);
    let ch_y_dmov = DbChannel::with_origin(&db, &config.motor_pv(&config.m_y, ".DMOV"), my_origin);
    let ch_z_dmov = DbChannel::with_origin(&db, &config.motor_pv(&config.m_z, ".DMOV"), my_origin);

    let ch_theta_hls =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_theta, ".HLS"), my_origin);
    let ch_theta_lls =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_theta, ".LLS"), my_origin);
    let ch_y_hls = DbChannel::with_origin(&db, &config.motor_pv(&config.m_y, ".HLS"), my_origin);
    let ch_y_lls = DbChannel::with_origin(&db, &config.motor_pv(&config.m_y, ".LLS"), my_origin);
    let ch_z_hls = DbChannel::with_origin(&db, &config.motor_pv(&config.m_z, ".HLS"), my_origin);
    let ch_z_lls = DbChannel::with_origin(&db, &config.motor_pv(&config.m_z, ".LLS"), my_origin);

    let ch_theta_set_ao = DbChannel::with_origin(&db, &config.mono_pv("ThetaSet"), my_origin);
    let ch_y_set_ao = DbChannel::with_origin(&db, &config.mono_pv("YSet"), my_origin);
    let ch_z_set_ao = DbChannel::with_origin(&db, &config.mono_pv("ZSet"), my_origin);
    let ch_y_set_hi = DbChannel::with_origin(&db, &config.mono_pv("YSet.DRVH"), my_origin);
    let ch_y_set_lo = DbChannel::with_origin(&db, &config.mono_pv("YSet.DRVL"), my_origin);

    let ch_theta_mot_hilim =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_theta, ".HLM"), my_origin);
    let ch_theta_mot_lolim =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_theta, ".LLM"), my_origin);
    let ch_y_mot_hilim =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_y, ".HLM"), my_origin);
    let ch_y_mot_lolim =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_y, ".LLM"), my_origin);
    let ch_z_mot_hilim =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_z, ".HLM"), my_origin);
    let ch_z_mot_lolim =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_z, ".LLM"), my_origin);

    let ch_theta_mot_cmd =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_theta, ""), my_origin);
    let ch_y_mot_cmd = DbChannel::with_origin(&db, &config.motor_pv(&config.m_y, ""), my_origin);
    let ch_z_mot_cmd = DbChannel::with_origin(&db, &config.motor_pv(&config.m_z, ""), my_origin);

    let ch_theta_mot_velo =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_theta, ".VELO"), my_origin);
    let ch_y_mot_velo =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_y, ".VELO"), my_origin);
    let ch_z_mot_velo =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_z, ".VELO"), my_origin);

    let ch_theta_mot_rbv =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_theta, ".RBV"), my_origin);
    let ch_y_mot_rbv =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_y, ".RBV"), my_origin);
    let ch_z_mot_rbv =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_z, ".RBV"), my_origin);

    let _ch_use_set = DbChannel::with_origin(&db, &config.mono_pv("UseSet"), my_origin);
    let ch_theta_mot_set =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_theta, ".SET"), my_origin);
    let ch_y_mot_set =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_y, ".SET"), my_origin);
    let ch_z_mot_set =
        DbChannel::with_origin(&db, &config.motor_pv(&config.m_z, ".SET"), my_origin);

    let ch_speed_ctrl = DbChannel::with_origin(&db, &config.mono_pv("SpeedCtrl"), my_origin);
    let ch_y_offset = DbChannel::with_origin(&db, &config.mono_pv("yOffset"), my_origin);
    let ch_y_offset_hi = DbChannel::with_origin(&db, &config.mono_pv("yOffset.DRVH"), my_origin);
    let ch_y_offset_lo = DbChannel::with_origin(&db, &config.mono_pv("yOffset.DRVL"), my_origin);

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
    let _ = ch_theta_mot_name.put_string_process(&theta_name).await;
    let _ = ch_y_mot_name.put_string_process(&y_name).await;
    let _ = ch_z_mot_name.put_string_process(&z_name).await;

    // Geometry init
    match geom {
        Geometry::Standard => {
            let _ = ch_y_offset_hi.put_f64_process(17.5 + 0.000001).await;
            let _ = ch_y_offset_lo.put_f64_process(17.5 - 0.000001).await;
            let _ = ch_y_offset.put_f64_process(17.5).await;
            let _ = ch_y_set_hi.put_f64_process(0.0).await;
            let _ = ch_y_set_lo.put_f64_process(-35.0).await;
        }
        Geometry::Alternate => {
            let _ = ch_y_set_hi.put_f64_process(60.0).await;
            let _ = ch_y_set_lo.put_f64_process(0.0).await;
        }
    }

    let _ = ch_put_vals.put_i16_process(0).await;
    let _ = ch_auto_mode.put_i16_process(0).await;
    let _ = ch_oper_ack.put_i16_process(0).await;

    // Crystal parameters
    let mut h = ch_h.get_f64().await;
    let mut k = ch_k.get_f64().await;
    let mut l = ch_l.get_f64().await;
    let mut a = ch_a.get_f64().await;
    let (mut two_d, forbidden, msg) = calc_2d_spacing(a, h, k, l);
    let _ = ch_d.put_f64_process(two_d).await;
    let _ = ch_seq_msg1.put_string_process(msg).await;
    // C calc2dSpacing sets opAlert from the forbidden-reflection check (1/0).
    let _ = ch_alert.put_i16_process(forbidden as i16).await;

    // Theta/energy limits
    let mut theta_mot_hi = ch_theta_mot_hilim.get_f64().await;
    let mut theta_mot_lo = ch_theta_mot_lolim.get_f64().await;
    let (mut theta_hi, mut theta_lo) = compute_theta_limits(theta_mot_hi, theta_mot_lo);
    let _ = ch_theta_hi.put_f64_process(theta_hi).await;
    let _ = ch_theta_lo.put_f64_process(theta_lo).await;
    let (e_hi, e_lo, l_hi, l_lo) = compute_energy_lambda_limits(two_d, theta_hi, theta_lo);
    let _ = ch_e_hi.put_f64_process(e_hi).await;
    let _ = ch_e_lo.put_f64_process(e_lo).await;
    let _ = ch_lambda_hi.put_f64_process(l_hi).await;
    let _ = ch_lambda_lo.put_f64_process(l_lo).await;

    // Initial readbacks
    let theta_mot_rdbk = ch_theta_mot_rbv.get_f64().await;
    let mut theta_rdbk_val = theta_mot_rdbk;
    let mut lambda_rdbk_val = theta_to_lambda(theta_rdbk_val, two_d);
    let mut e_rdbk_val = lambda_to_energy(lambda_rdbk_val);
    let _ = ch_theta_rdbk.put_f64_process(theta_rdbk_val).await;
    let _ = ch_lambda_rdbk.put_f64_process(lambda_rdbk_val).await;
    let _ = ch_e_rdbk.put_f64_process(e_rdbk_val).await;

    let mut theta_val = theta_mot_rdbk;
    let _ = ch_theta.put_f64_process(theta_val).await;
    let mut lambda_val = theta_to_lambda(theta_val, two_d);
    let _ = ch_lambda.put_f64_process(lambda_val).await;
    let mut e_val = lambda_to_energy(lambda_val);
    let _ = ch_e.put_f64_process(e_val).await;

    let mut auto_mode = false;
    let mut use_set_mode = false;
    let mut cc_mode = CrystalMode::from_i16(ch_cc_mode.get_i16().await);
    let mut y_offset_val = ch_y_offset.get_f64().await;
    let mut _caused_move = false;
    let risk_averse = false;

    // Last *accepted* motor setpoints, restored when a command would violate a
    // limit. C kohzuCtl_soft snapshots prev_thetaMotDesired/prev_yMotDesired/
    // prev_zMotDesired at waitForCmnd entry (kohzuCtl_soft.st:618-620) and
    // restores them in the `when (willViolateLimit)` block (kohzuCtl_soft.st:899-903).
    let mut last_theta_set = theta_val;
    let mut last_y_set = calc_y_position(geom, theta_val, y_offset_val);
    let mut last_z_set = calc_z_position(geom, theta_val, y_offset_val);

    let _ = ch_seq_msg1.put_string_process("Kohzu Control Ready").await;
    let _ = ch_seq_msg2.put_string_process(" ").await;

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
        // Snapshot the last-accepted bragg state for limit-violation rollback
        // (C kohzuCtl_soft.st:615-620, snapshotted at waitForCmnd entry).
        let prev_e = e_val;
        let prev_theta = theta_val;
        let prev_lambda = lambda_val;

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
                let _ = ch_lambda.put_f64_process(lambda_val).await;
                if lambda_val > two_d {
                    let _ = ch_seq_msg1
                        .put_string_process("Wavelength > 2d spacing.")
                        .await;
                    let _ = ch_alert.put_i16_process(1).await;
                } else if let Some(th) = lambda_to_theta(lambda_val, two_d) {
                    theta_val = th;
                    let _ = ch_theta.put_f64_process(theta_val).await;
                }
                proceed_to_theta_changed = true;
            }
        } else if changed_pv == pv_lambda {
            let new_l = new_val;
            if (new_l - lambda_val).abs() > 1e-12 {
                lambda_val = new_l;
                if lambda_val > two_d {
                    let _ = ch_seq_msg1
                        .put_string_process("Wavelength > 2d spacing.")
                        .await;
                    let _ = ch_alert.put_i16_process(1).await;
                } else if let Some(th) = lambda_to_theta(lambda_val, two_d) {
                    theta_val = th;
                    let _ = ch_theta.put_f64_process(theta_val).await;
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
            let (d, forbidden, msg) = calc_2d_spacing(a, h, k, l);
            two_d = d;
            let _ = ch_d.put_f64_process(two_d).await;
            let _ = ch_seq_msg1.put_string_process(msg).await;
            // C kohzuCtl_soft_calc2dSpacing sets opAlert from the (H,K,L) parity
            // check and pvPuts it (kohzuCtl_soft.st:700,711): forbidden -> 1,
            // valid -> 0 (cleared on return to a valid reflection).
            let _ = ch_alert.put_i16_process(forbidden as i16).await;
            auto_mode = false;
            let _ = ch_auto_mode.put_i16_process(0).await;
            let _ = ch_seq_msg2.put_string_process("Set to Manual Mode").await;
            let (eh, el, lh, ll) = compute_energy_lambda_limits(two_d, theta_hi, theta_lo);
            let _ = ch_e_hi.put_f64_process(eh).await;
            let _ = ch_e_lo.put_f64_process(el).await;
            let _ = ch_lambda_hi.put_f64_process(lh).await;
            let _ = ch_lambda_lo.put_f64_process(ll).await;
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
                let _ = ch_alert.put_i16_process(0).await;
                let _ = ch_seq_msg1.put_string_process(" ").await;
                let _ = ch_seq_msg2.put_string_process(" ").await;
                let _ = ch_oper_ack.put_i16_process(0).await;
            }
        } else if changed_pv == pv_theta_mot_rbv {
            let rbv = new_val;
            let _ = ch_theta_rdbk_echo.put_f64_process(rbv).await;
            theta_rdbk_val = rbv;
            lambda_rdbk_val = theta_to_lambda(theta_rdbk_val, two_d);
            e_rdbk_val = lambda_to_energy(lambda_rdbk_val);
            let _ = ch_theta_rdbk.put_f64_process(theta_rdbk_val).await;
            let _ = ch_lambda_rdbk.put_f64_process(lambda_rdbk_val).await;
            let _ = ch_e_rdbk.put_f64_process(e_rdbk_val).await;
        } else if changed_pv == pv_theta_hilim {
            theta_mot_hi = new_val;
            let (hi, lo) = compute_theta_limits(theta_mot_hi, theta_mot_lo);
            theta_hi = hi;
            theta_lo = lo;
            let _ = ch_theta_hi.put_f64_process(theta_hi).await;
            let _ = ch_theta_lo.put_f64_process(theta_lo).await;
            let (eh, el, lh, ll) = compute_energy_lambda_limits(two_d, theta_hi, theta_lo);
            let _ = ch_e_hi.put_f64_process(eh).await;
            let _ = ch_e_lo.put_f64_process(el).await;
            let _ = ch_lambda_hi.put_f64_process(lh).await;
            let _ = ch_lambda_lo.put_f64_process(ll).await;
        } else if changed_pv == pv_theta_lolim {
            theta_mot_lo = new_val;
            let (hi, lo) = compute_theta_limits(theta_mot_hi, theta_mot_lo);
            theta_hi = hi;
            theta_lo = lo;
            let _ = ch_theta_hi.put_f64_process(theta_hi).await;
            let _ = ch_theta_lo.put_f64_process(theta_lo).await;
            let (eh, el, lh, ll) = compute_energy_lambda_limits(two_d, theta_hi, theta_lo);
            let _ = ch_e_hi.put_f64_process(eh).await;
            let _ = ch_e_lo.put_f64_process(el).await;
            let _ = ch_lambda_hi.put_f64_process(lh).await;
            let _ = ch_lambda_lo.put_f64_process(ll).await;
        } else if changed_pv == pv_y_offset {
            y_offset_val = new_val;
            auto_mode = false;
            let _ = ch_auto_mode.put_i16_process(0).await;
            let _ = ch_seq_msg1
                .put_string_process(&format!("y offset changed to {:.4}", y_offset_val))
                .await;
            let _ = ch_seq_msg2.put_string_process("Set to Manual Mode").await;
            proceed_to_theta_changed = true;
        } else if changed_pv == pv_use_set {
            use_set_mode = new_val as i16 != 0;
            let sv = if use_set_mode { 1i16 } else { 0 };
            let _ = ch_theta_mot_set.put_i16_process(sv).await;
            let _ = ch_y_mot_set.put_i16_process(sv).await;
            let _ = ch_z_mot_set.put_i16_process(sv).await;
        }

        if !proceed_to_theta_changed {
            continue;
        }

        // -- Theta-changed processing --
        let mut will_violate = false;
        let (clamped_theta, was_clamped) = clamp_theta(theta_val, theta_lo, theta_hi);
        if was_clamped {
            theta_val = clamped_theta;
            let _ = ch_seq_msg1
                .put_string_process("Theta constrained to LIMIT")
                .await;
            let _ = ch_alert.put_i16_process(1).await;
            if risk_averse {
                auto_mode = false;
                let _ = ch_auto_mode.put_i16_process(0).await;
                let _ = ch_seq_msg2.put_string_process("Set to Manual Mode").await;
            } else {
                // C kohzuCtl_soft.st:783-784 — outside risk-averse mode a
                // theta-limit hit sets willViolateLimit, rolling back the command.
                will_violate = true;
            }
        }

        lambda_val = theta_to_lambda(theta_val, two_d);
        let _ = ch_lambda.put_f64_process(lambda_val).await;
        e_val = lambda_to_energy(lambda_val);
        let _ = ch_e.put_f64_process(e_val).await;

        let current_rbv = ch_theta_mot_rbv.get_f64().await;
        theta_rdbk_val = current_rbv;
        lambda_rdbk_val = theta_to_lambda(theta_rdbk_val, two_d);
        e_rdbk_val = lambda_to_energy(lambda_rdbk_val);
        let _ = ch_theta_rdbk.put_f64_process(theta_rdbk_val).await;
        let _ = ch_lambda_rdbk.put_f64_process(lambda_rdbk_val).await;
        let _ = ch_e_rdbk.put_f64_process(e_rdbk_val).await;

        // -- Calc movements --
        let theta_mot_desired = theta_val;
        let y_mot_desired = calc_y_position(geom, theta_val, y_offset_val);
        let z_mot_desired = calc_z_position(geom, theta_val, y_offset_val);
        let _ = ch_theta_set_ao.put_f64_process(theta_mot_desired).await;
        let _ = ch_y_set_ao.put_f64_process(y_mot_desired).await;
        let _ = ch_z_set_ao.put_f64_process(z_mot_desired).await;

        // Check limits
        let y_hi = ch_y_mot_hilim.get_f64().await;
        let y_lo = ch_y_mot_lolim.get_f64().await;
        let z_hi = ch_z_mot_hilim.get_f64().await;
        let z_lo = ch_z_mot_lolim.get_f64().await;

        if !cc_mode.y_frozen() && (y_mot_desired < y_lo || y_mot_desired > y_hi) {
            let _ = ch_seq_msg1
                .put_string_process("Y will exceed soft limits")
                .await;
            let _ = ch_alert.put_i16_process(1).await;
            will_violate = true;
        }
        if !cc_mode.z_frozen() && (z_mot_desired < z_lo || z_mot_desired > z_hi) {
            let _ = ch_seq_msg1
                .put_string_process("Z will exceed soft limits")
                .await;
            let _ = ch_alert.put_i16_process(1).await;
            will_violate = true;
        }

        if will_violate {
            // C kohzuCtl_soft.st:892-905 (`when (willViolateLimit)`): restore
            // E/theta/lambda and the three motor setpoints to their pre-command
            // values, pvPut each, message "Command ignored". Without this the
            // rejected out-of-range setpoints stay on the PVs.
            e_val = prev_e;
            theta_val = prev_theta;
            lambda_val = prev_lambda;
            let _ = ch_e.put_f64_process(e_val).await;
            let _ = ch_theta.put_f64_process(theta_val).await;
            let _ = ch_lambda.put_f64_process(lambda_val).await;
            let _ = ch_theta_set_ao.put_f64_process(last_theta_set).await;
            let _ = ch_y_set_ao.put_f64_process(last_y_set).await;
            let _ = ch_z_set_ao.put_f64_process(last_z_set).await;
            let _ = ch_seq_msg2.put_string_process("Command ignored").await;
            let _ = ch_moving.put_i16_process(0).await;
            continue;
        }

        // Command accepted — record the setpoints to revert to next time
        // (C snapshots them as prev_*MotDesired at the next waitForCmnd entry).
        last_theta_set = theta_mot_desired;
        last_y_set = y_mot_desired;
        last_z_set = z_mot_desired;

        // -- Move if appropriate --
        let put_requested = ch_put_vals.get_i16().await != 0;
        if auto_mode || put_requested || use_set_mode {
            let speed_control = ch_speed_ctrl.get_i16().await != 0;
            // Save the pre-coordination motor speeds for restoration after the
            // move. Without it each coordinated move leaves a reduced `.VELO`
            // that the next move reads back as its baseline, decaying the
            // speeds geometrically toward zero. C `kohzuCtl_soft.st` saves
            // oldThSpeed/oldYSpeed/oldZSpeed (kohzuCtl_soft.st:854-856) and
            // restores them once the motors stop (kohzuCtl_soft.st:1040-1044).
            let mut old_th_speed = 0.0;
            let mut old_y_speed = 0.0;
            let mut old_z_speed = 0.0;
            let mut speeds_coordinated = false;
            if speed_control {
                let th_speed = ch_theta_mot_velo.get_f64().await;
                let y_speed = ch_y_mot_velo.get_f64().await;
                let z_speed = ch_z_mot_velo.get_f64().await;
                old_th_speed = th_speed;
                old_y_speed = y_speed;
                old_z_speed = z_speed;
                speeds_coordinated = true;
                let (new_th, new_y, new_z) = coordinate_speeds(
                    theta_val - current_rbv,
                    y_mot_desired - ch_y_mot_rbv.get_f64().await,
                    z_mot_desired - ch_z_mot_rbv.get_f64().await,
                    th_speed,
                    y_speed,
                    z_speed,
                    cc_mode,
                );
                let _ = ch_theta_mot_velo.put_f64_process(new_th).await;
                if !cc_mode.y_frozen() {
                    let _ = ch_y_mot_velo.put_f64_process(new_y).await;
                }
                if !cc_mode.z_frozen() {
                    let _ = ch_z_mot_velo.put_f64_process(new_z).await;
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

            let _ = ch_put_vals.put_i16_process(0).await;
            _caused_move = true;

            // Wait for motors
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let th_dmov = ch_theta_dmov.get_i16().await;
                let y_dmov = ch_y_dmov.get_i16().await;
                let z_dmov = ch_z_dmov.get_i16().await;

                if ch_theta_hls.get_i16().await != 0 || ch_theta_lls.get_i16().await != 0 {
                    let _ = ch_seq_msg1
                        .put_string_process("Theta Motor hit a limit switch!")
                        .await;
                    let _ = ch_alert.put_i16_process(1).await;
                    auto_mode = false;
                    let _ = ch_auto_mode.put_i16_process(0).await;
                    let _ = ch_theta_mot_stop.put_i16_process(1).await;
                    let _ = ch_y_stop.put_i16_process(1).await;
                    let _ = ch_z_stop.put_i16_process(1).await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    break;
                }
                if !cc_mode.y_frozen()
                    && (ch_y_hls.get_i16().await != 0 || ch_y_lls.get_i16().await != 0)
                {
                    let _ = ch_seq_msg1
                        .put_string_process("Y Motor hit a limit switch!")
                        .await;
                    let _ = ch_alert.put_i16_process(1).await;
                    auto_mode = false;
                    let _ = ch_auto_mode.put_i16_process(0).await;
                    let _ = ch_theta_mot_stop.put_i16_process(1).await;
                    let _ = ch_y_stop.put_i16_process(1).await;
                    let _ = ch_z_stop.put_i16_process(1).await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    break;
                }
                if !cc_mode.z_frozen()
                    && (ch_z_hls.get_i16().await != 0 || ch_z_lls.get_i16().await != 0)
                {
                    let _ = ch_seq_msg1
                        .put_string_process("Z Motor hit a limit switch!")
                        .await;
                    let _ = ch_alert.put_i16_process(1).await;
                    auto_mode = false;
                    let _ = ch_auto_mode.put_i16_process(0).await;
                    let _ = ch_theta_mot_stop.put_i16_process(1).await;
                    let _ = ch_y_stop.put_i16_process(1).await;
                    let _ = ch_z_stop.put_i16_process(1).await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    break;
                }

                // Update readbacks while moving
                let rbv = ch_theta_mot_rbv.get_f64().await;
                theta_rdbk_val = rbv;
                lambda_rdbk_val = theta_to_lambda(theta_rdbk_val, two_d);
                e_rdbk_val = lambda_to_energy(lambda_rdbk_val);
                let _ = ch_theta_rdbk.put_f64_process(theta_rdbk_val).await;
                let _ = ch_lambda_rdbk.put_f64_process(lambda_rdbk_val).await;
                let _ = ch_e_rdbk.put_f64_process(e_rdbk_val).await;

                if th_dmov != 0 && y_dmov != 0 && z_dmov != 0 {
                    break;
                }
            }

            // Restore the pre-coordination motor speeds now the move has
            // stopped (C `kohzuCtl_soft.st` state thetaMotStopped).
            if speeds_coordinated {
                let _ = ch_theta_mot_velo.put_f64_process(old_th_speed).await;
                if !cc_mode.y_frozen() {
                    let _ = ch_y_mot_velo.put_f64_process(old_y_speed).await;
                }
                if !cc_mode.z_frozen() {
                    let _ = ch_z_mot_velo.put_f64_process(old_z_speed).await;
                }
            }

            _caused_move = false;

            // Final readback
            let rbv = ch_theta_mot_rbv.get_f64().await;
            theta_rdbk_val = rbv;
            lambda_rdbk_val = theta_to_lambda(theta_rdbk_val, two_d);
            e_rdbk_val = lambda_to_energy(lambda_rdbk_val);
            let _ = ch_theta_rdbk.put_f64_process(theta_rdbk_val).await;
            let _ = ch_lambda_rdbk.put_f64_process(lambda_rdbk_val).await;
            let _ = ch_e_rdbk.put_f64_process(e_rdbk_val).await;
            let _ = ch_moving.put_i16_process(0).await;
        }

        // Update echoes
        let _ = ch_theta_rdbk_echo
            .put_f64_process(ch_theta_mot_rbv.get_f64().await)
            .await;
        let _ = ch_y_rdbk_echo
            .put_f64_process(ch_y_mot_rbv.get_f64().await)
            .await;
        let _ = ch_z_rdbk_echo
            .put_f64_process(ch_z_mot_rbv.get_f64().await)
            .await;
        let _ = ch_theta_vel_echo
            .put_f64_process(ch_theta_mot_velo.get_f64().await)
            .await;
        let _ = ch_y_vel_echo
            .put_f64_process(ch_y_mot_velo.get_f64().await)
            .await;
        let _ = ch_z_vel_echo
            .put_f64_process(ch_z_mot_velo.get_f64().await)
            .await;
        let _ = ch_theta_dmov_echo
            .put_i16_process(ch_theta_dmov.get_i16().await)
            .await;
        let _ = ch_y_dmov_echo
            .put_i16_process(ch_y_dmov.get_i16().await)
            .await;
        let _ = ch_z_dmov_echo
            .put_i16_process(ch_z_dmov.get_i16().await)
            .await;
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
