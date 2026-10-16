//! Kohzu soft-motor monochromator control -- native Rust port of `kohzuCtl_soft.st`.
//!
//! Identical physics to `kohzu_ctl`, but uses a separate MONO prefix for the
//! soft PV names, allowing multiple monochromator instances on one IOC.
//!
//! PV naming: `{P}{MONO}E`, `{P}{MONO}Lambda`, `{P}{MONO}Theta`, etc.
//! Motor PVs still use `{P}{M_THETA}`, `{P}{M_Y}`, `{P}{M_Z}`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use epics_base_rs::server::database::PvDatabase;
use tracing::info;

use crate::db_access::{DbMultiMonitor, alloc_origin};
use epics_base_rs::server::snl::{PutQueue, StateSet};

// Re-use physics from kohzu_ctl.
use crate::snl::kohzu_ctl::{
    CrystalMode, Geometry, LimitSwitches, calc_2d_spacing, calc_y_position, calc_z_position,
    clamp_theta, compute_energy_lambda_limits, compute_theta_limits, coordinate_speeds,
    echo_motor_fields, energy_to_lambda, lambda_to_energy, lambda_to_theta, theta_to_lambda,
    update_set,
};

// ---------------------------------------------------------------------------
// State enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KohzuSoftState {
    Init,
    InitSequence,
    WaitForCommandEnter,
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
    let mut echoes = Vec::new();
    for (m, axis) in [
        (&config.m_theta, "Theta"),
        (&config.m_y, "Y"),
        (&config.m_z, "Z"),
    ] {
        if axis != "Theta" {
            echoes.push((
                config.motor_pv(m, ".RBV"),
                config.mono_pv(&format!("{axis}Rdbk")),
            ));
        }
        echoes.push((
            config.motor_pv(m, ".VELO"),
            config.mono_pv(&format!("{axis}Vel")),
        ));
        echoes.push((
            config.motor_pv(m, ""),
            config.mono_pv(&format!("{axis}Cmd")),
        ));
        echoes.push((
            config.motor_pv(m, ".DMOV"),
            config.mono_pv(&format!("{axis}Dmov")),
        ));
    }
    // updatePsuedo_soft (kohzuCtl_soft.st:1168-1235).
    let queue = PutQueue::new(&db, "kohzuCtl_soft");
    let echoes = echo_motor_fields(db.clone(), queue.state_set("updatePsuedo_soft"), echoes);
    let use_set_pv = config.mono_pv("UseSet");
    let update_set_ss = queue.state_set("updateSet");
    let use_set_mode = Arc::new(AtomicBool::new(
        update_set_ss.pv(&use_set_pv).get_i16().await != 0,
    ));
    // updateSet (kohzuCtl_soft.st:1243-1286).
    let update_set = update_set(
        db.clone(),
        update_set_ss,
        use_set_pv,
        [&config.m_theta, &config.m_y, &config.m_z].map(|m| config.motor_pv(m, ".SET")),
        use_set_mode.clone(),
    );
    tokio::select! {
        r = kohzu_ctl_soft(config, db, queue.state_set("kohzuCtl_soft"), use_set_mode) => r,
        () = echoes => Ok(()),
        () = update_set => Ok(()),
    }
}

/// The kohzuCtl_soft state set.
async fn kohzu_ctl_soft(
    config: KohzuSoftConfig,
    db: PvDatabase,
    ss: StateSet,
    use_set_mode: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!(
        "kohzuCtl_soft: starting for prefix={}{}",
        config.prefix, config.mono
    );

    let my_origin = alloc_origin();

    // -- Create channels --
    let _ch_debug = ss.pv_with_origin(&config.mono_pv("CtlDebug"), my_origin);
    let ch_seq_msg1 = ss.pv_with_origin(&config.mono_pv("SeqMsg1"), my_origin);
    let ch_seq_msg2 = ss.pv_with_origin(&config.mono_pv("SeqMsg2"), my_origin);
    let ch_alert = ss.pv_with_origin(&config.mono_pv("Alert"), my_origin);
    let ch_oper_ack = ss.pv_with_origin(&config.mono_pv("OperAck"), my_origin);
    let ch_put_vals = ss.pv_with_origin(&config.mono_pv("Put"), my_origin);
    let ch_auto_mode = ss.pv_with_origin(&config.mono_pv("Mode"), my_origin);
    let ch_moving = ss.pv_with_origin(&config.mono_pv("Moving"), my_origin);

    // Crystal parameters
    let ch_h = ss.pv_with_origin(&config.mono_pv("H"), my_origin);
    let ch_k = ss.pv_with_origin(&config.mono_pv("K"), my_origin);
    let ch_l = ss.pv_with_origin(&config.mono_pv("L"), my_origin);
    let ch_a = ss.pv_with_origin(&config.mono_pv("A"), my_origin);
    let ch_d = ss.pv_with_origin(&config.mono_pv("2dSpacing"), my_origin);

    // Energy / lambda / theta
    let ch_e = ss.pv_with_origin(&config.mono_pv("E"), my_origin);
    let ch_e_hi = ss.pv_with_origin(&config.mono_pv("E.HLM"), my_origin);
    let ch_e_lo = ss.pv_with_origin(&config.mono_pv("E.LLM"), my_origin);
    let ch_e_rdbk = ss.pv_with_origin(&config.mono_pv("ERdbk"), my_origin);

    let ch_lambda = ss.pv_with_origin(&config.mono_pv("Lambda"), my_origin);
    let ch_lambda_hi = ss.pv_with_origin(&config.mono_pv("Lambda.HLM"), my_origin);
    let ch_lambda_lo = ss.pv_with_origin(&config.mono_pv("Lambda.LLM"), my_origin);
    let ch_lambda_rdbk = ss.pv_with_origin(&config.mono_pv("LambdaRdbk"), my_origin);

    let ch_theta = ss.pv_with_origin(&config.mono_pv("Theta"), my_origin);
    let ch_theta_hi = ss.pv_with_origin(&config.mono_pv("Theta.HLM"), my_origin);
    let ch_theta_lo = ss.pv_with_origin(&config.mono_pv("Theta.LLM"), my_origin);
    let ch_theta_rdbk = ss.pv_with_origin(&config.mono_pv("ThetaRdbk"), my_origin);

    // Soft echo PVs
    let ch_theta_mot_name = ss.pv_with_origin(&config.mono_pv("ThetaPv"), my_origin);
    let ch_y_mot_name = ss.pv_with_origin(&config.mono_pv("YPv"), my_origin);
    let ch_z_mot_name = ss.pv_with_origin(&config.mono_pv("ZPv"), my_origin);

    let ch_theta_cmd_echo = ss.pv_with_origin(&config.mono_pv("ThetaCmd"), my_origin);
    let ch_y_cmd_echo = ss.pv_with_origin(&config.mono_pv("YCmd"), my_origin);
    let ch_z_cmd_echo = ss.pv_with_origin(&config.mono_pv("ZCmd"), my_origin);
    let ch_theta_rdbk_echo = ss.pv_with_origin(&config.mono_pv("ThetaMotRdbk"), my_origin);
    let ch_y_rdbk_echo = ss.pv_with_origin(&config.mono_pv("YRdbk"), my_origin);
    let ch_z_rdbk_echo = ss.pv_with_origin(&config.mono_pv("ZRdbk"), my_origin);
    let ch_theta_vel_echo = ss.pv_with_origin(&config.mono_pv("ThetaVel"), my_origin);
    let ch_y_vel_echo = ss.pv_with_origin(&config.mono_pv("YVel"), my_origin);
    let ch_z_vel_echo = ss.pv_with_origin(&config.mono_pv("ZVel"), my_origin);
    let ch_theta_dmov_echo = ss.pv_with_origin(&config.mono_pv("ThetaDmov"), my_origin);
    let ch_y_dmov_echo = ss.pv_with_origin(&config.mono_pv("YDmov"), my_origin);
    let ch_z_dmov_echo = ss.pv_with_origin(&config.mono_pv("ZDmov"), my_origin);

    // Motor records
    let ch_theta_mot_stop =
        ss.pv_with_origin(&config.motor_pv(&config.m_theta, ".STOP"), my_origin);
    let ch_y_stop = ss.pv_with_origin(&config.motor_pv(&config.m_y, ".STOP"), my_origin);
    let ch_z_stop = ss.pv_with_origin(&config.motor_pv(&config.m_z, ".STOP"), my_origin);

    let ch_theta_dmov = ss.pv_with_origin(&config.motor_pv(&config.m_theta, ".DMOV"), my_origin);
    let ch_y_dmov = ss.pv_with_origin(&config.motor_pv(&config.m_y, ".DMOV"), my_origin);
    let ch_z_dmov = ss.pv_with_origin(&config.motor_pv(&config.m_z, ".DMOV"), my_origin);

    let ch_theta_set_ao = ss.pv_with_origin(&config.mono_pv("ThetaSet"), my_origin);
    let ch_y_set_ao = ss.pv_with_origin(&config.mono_pv("YSet"), my_origin);
    let ch_z_set_ao = ss.pv_with_origin(&config.mono_pv("ZSet"), my_origin);
    let ch_y_set_hi = ss.pv_with_origin(&config.mono_pv("YSet.DRVH"), my_origin);
    let ch_y_set_lo = ss.pv_with_origin(&config.mono_pv("YSet.DRVL"), my_origin);

    let ch_theta_mot_hilim =
        ss.pv_with_origin(&config.motor_pv(&config.m_theta, ".HLM"), my_origin);
    let ch_theta_mot_lolim =
        ss.pv_with_origin(&config.motor_pv(&config.m_theta, ".LLM"), my_origin);
    let ch_y_mot_hilim = ss.pv_with_origin(&config.motor_pv(&config.m_y, ".HLM"), my_origin);
    let ch_y_mot_lolim = ss.pv_with_origin(&config.motor_pv(&config.m_y, ".LLM"), my_origin);
    let ch_z_mot_hilim = ss.pv_with_origin(&config.motor_pv(&config.m_z, ".HLM"), my_origin);
    let ch_z_mot_lolim = ss.pv_with_origin(&config.motor_pv(&config.m_z, ".LLM"), my_origin);

    let ch_theta_mot_cmd = ss.pv_with_origin(&config.motor_pv(&config.m_theta, ""), my_origin);
    let ch_y_mot_cmd = ss.pv_with_origin(&config.motor_pv(&config.m_y, ""), my_origin);
    let ch_z_mot_cmd = ss.pv_with_origin(&config.motor_pv(&config.m_z, ""), my_origin);

    let ch_theta_mot_velo =
        ss.pv_with_origin(&config.motor_pv(&config.m_theta, ".VELO"), my_origin);
    let ch_y_mot_velo = ss.pv_with_origin(&config.motor_pv(&config.m_y, ".VELO"), my_origin);
    let ch_z_mot_velo = ss.pv_with_origin(&config.motor_pv(&config.m_z, ".VELO"), my_origin);

    let ch_theta_mot_rbv = ss.pv_with_origin(&config.motor_pv(&config.m_theta, ".RBV"), my_origin);
    let ch_y_mot_rbv = ss.pv_with_origin(&config.motor_pv(&config.m_y, ".RBV"), my_origin);
    let ch_z_mot_rbv = ss.pv_with_origin(&config.motor_pv(&config.m_z, ".RBV"), my_origin);

    let ch_speed_ctrl = ss.pv_with_origin(&config.mono_pv("SpeedCtrl"), my_origin);
    let ch_y_offset = ss.pv_with_origin(&config.mono_pv("yOffset"), my_origin);
    let ch_y_offset_hi = ss.pv_with_origin(&config.mono_pv("yOffset.DRVH"), my_origin);
    let ch_y_offset_lo = ss.pv_with_origin(&config.mono_pv("yOffset.DRVL"), my_origin);

    let limit_switches = LimitSwitches::new(&ss, my_origin, |m, f| {
        config.motor_pv([&config.m_theta, &config.m_y, &config.m_z][m], f)
    });

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
        config.mono_pv("OperAck"),
        config.motor_pv(&config.m_theta, ".RBV"),
        config.motor_pv(&config.m_theta, ".HLM"),
        config.motor_pv(&config.m_theta, ".LLM"),
        config.mono_pv("yOffset"),
    ];
    let mut monitor = DbMultiMonitor::new(&db, &monitored_pvs).await;
    // C `monitor autoMode;` and `monitor ccMode;` sync no event flag and no
    // waitForCmnd `when` tests them (kohzuCtl_soft.st:74-81, :628-692): they
    // are written on arrival and only read. So is `monitor kohzuMoving;`
    // (:83-85), read only by waitForCmndEnter (:609).
    monitor
        .add_vars(
            &db,
            &[
                config.mono_pv("Mode"),
                config.mono_pv("Mode2"),
                config.mono_pv("Moving"),
            ],
        )
        .await;
    println!(
        "kohzuCtl_soft: subscribed to {} PVs, {} active",
        monitored_pvs.len(),
        monitor.sub_count()
    );

    let geom = config.geom;

    // C initSeq=0 (kohzuCtl_soft.st:478-539).
    let _ = ch_put_vals.put_i16_process(0).await;
    let _ = ch_auto_mode.put_i16_process(0).await;
    let _ = ch_oper_ack.put_i16_process(0).await;
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
    let theta_name = format!("{}{}", config.prefix, config.m_theta);
    let y_name = format!("{}{}", config.prefix, config.m_y);
    let z_name = format!("{}{}", config.prefix, config.m_z);
    let _ = ch_theta_mot_name.put_string_process(&theta_name).await;
    let _ = ch_y_mot_name.put_string_process(&y_name).await;
    let _ = ch_z_mot_name.put_string_process(&z_name).await;
    let mut init_seq = 1;

    let mut two_d = 0.0;
    // C's thetaHi/thetaLo are assigned, not monitored: zero until
    // thetaLimits, which first runs on the connect-time thetaMotHiLim monitor.
    let mut theta_hi = 0.0;
    let mut theta_lo = 0.0;
    // C's E/lambda/theta and the three setpoints are monitored globals;
    // prev_* are snapshotted only by waitForCmndEnter once initSeqDone.
    let mut theta_val = ch_theta.get_f64().await;
    let mut lambda_val = ch_lambda.get_f64().await;
    let mut e_val = ch_e.get_f64().await;
    let (mut prev_e, mut prev_theta, mut prev_lambda) = (0.0, 0.0, 0.0);
    let mut desired = [0.0; 3];
    let mut prev_desired = [0.0; 3];
    let mut old_speeds = [0.0; 3];
    let mut new_speeds = [0.0; 3];

    let mut y_offset_val = ch_y_offset.get_f64().await;
    let mut caused_move = false;
    let mut will_violate = false;
    let mut kohzu_done = false;
    let mut init_seq_done = false;
    let risk_averse = false;
    // The connect-time monitor events waitForCmnd takes before any live one.
    let mut connect_events: VecDeque<(String, f64)> = VecDeque::new();

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
    let pv_moving = config.mono_pv("Moving");
    let pv_cc_mode = config.mono_pv("Mode2");
    let cc_mode_of = |m: &mut DbMultiMonitor| CrystalMode::from_i16(m.var(&pv_cc_mode) as i16);
    let pv_oper_ack = config.mono_pv("OperAck");
    let pv_theta_mot_rbv = config.motor_pv(&config.m_theta, ".RBV");
    let pv_theta_hilim = config.motor_pv(&config.m_theta, ".HLM");
    let pv_theta_lolim = config.motor_pv(&config.m_theta, ".LLM");
    let pv_y_offset = config.mono_pv("yOffset");

    // C's thetaRdbk/lambdaRdbk/ERdbk, recomputed from a theta readback.
    let put_readbacks = async |rbv: f64, two_d: f64| {
        let lambda = theta_to_lambda(rbv, two_d);
        let _ = ch_theta_rdbk.put_f64_process(rbv).await;
        let _ = ch_lambda_rdbk.put_f64_process(lambda).await;
        let _ = ch_e_rdbk.put_f64_process(lambda_to_energy(lambda)).await;
    };
    // C's lambdaHi, lambdaLo, EHi, ELo puts, in its order.
    let put_limits = async |theta_hi: f64, theta_lo: f64, two_d: f64| {
        let (e_hi, e_lo, lambda_hi, lambda_lo) =
            compute_energy_lambda_limits(two_d, theta_hi, theta_lo);
        let _ = ch_lambda_hi.put_f64_process(lambda_hi).await;
        let _ = ch_lambda_lo.put_f64_process(lambda_lo).await;
        let _ = ch_e_hi.put_f64_process(e_hi).await;
        let _ = ch_e_lo.put_f64_process(e_lo).await;
    };
    let all_done = async || {
        ch_theta_dmov.get_i16().await != 0
            && ch_y_dmov.get_i16().await != 0
            && ch_z_dmov.get_i16().await != 0
    };

    let mut state = KohzuSoftState::DInputChanged;
    loop {
        state = match state {
            KohzuSoftState::Init | KohzuSoftState::InitSequence => match init_seq {
                // kohzuCtl_soft.st:541-560
                1 => {
                    init_seq = 2;
                    KohzuSoftState::CheckMotorLimits
                }
                2 => {
                    init_seq = 3;
                    KohzuSoftState::UpdateReadback
                }
                _ => {
                    epics_base_rs::runtime::task::sleep(Duration::from_secs(1)).await;
                    let _ = ch_seq_msg1.put_string_process("Kohzu Control Ready").await;
                    let _ = ch_seq_msg2.put_string_process(" ").await;
                    init_seq_done = true;
                    theta_val = ch_theta_mot_rbv.get_f64().await;
                    let _ = ch_theta.put_f64_process(theta_val).await;
                    // The monitors waitForCmndEnter does not clear, in
                    // waitForCmnd's order (kohzuCtl_soft.st:641-680).
                    for ch in [&ch_theta_mot_hilim, &ch_theta_mot_rbv, &ch_y_offset] {
                        connect_events.push_back((ch.name().to_string(), ch.get_f64().await));
                    }
                    KohzuSoftState::ThetaChanged
                }
            },
            KohzuSoftState::WaitForCommandEnter if !init_seq_done => KohzuSoftState::InitSequence,
            KohzuSoftState::WaitForCommandEnter => {
                // kohzuCtl_soft.st:563-622
                epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
                for pv in [&pv_h, &pv_k, &pv_l, &pv_a, &pv_e, &pv_lambda, &pv_theta] {
                    monitor.clear(pv);
                }
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
                let _ = ch_theta_cmd_echo
                    .put_f64_process(ch_theta_mot_cmd.get_f64().await)
                    .await;
                let _ = ch_y_cmd_echo
                    .put_f64_process(ch_y_mot_cmd.get_f64().await)
                    .await;
                let _ = ch_z_cmd_echo
                    .put_f64_process(ch_z_mot_cmd.get_f64().await)
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
                if monitor.var(&pv_auto_mode) != 0.0 || use_set_mode.load(Ordering::Relaxed) {
                    put_readbacks(ch_theta_mot_rbv.get_f64().await, two_d).await;
                }
                let _ = ch_oper_ack.put_i16_process(0).await;
                if kohzu_done && monitor.var(&pv_moving) != 0.0 {
                    kohzu_done = false;
                    let _ = ch_moving.put_i16_process(0).await;
                }
                prev_theta = theta_val;
                prev_e = e_val;
                prev_lambda = lambda_val;
                prev_desired = [
                    ch_theta_set_ao.get_f64().await,
                    ch_y_set_ao.get_f64().await,
                    ch_z_set_ao.get_f64().await,
                ];
                will_violate = false;
                KohzuSoftState::WaitForCommand
            }
            KohzuSoftState::WaitForCommand => {
                // kohzuCtl_soft.st:628-691
                let (pv, val) = match connect_events.pop_front() {
                    Some((pv, val)) => {
                        // One C flag with the live monitors queued behind it.
                        monitor.clear(&pv);
                        (pv, val)
                    }
                    None => monitor.wait_change().await,
                };
                if pv == pv_h || pv == pv_k || pv == pv_l || pv == pv_a {
                    let _ = ch_seq_msg1
                        .put_string_process("Lattice Spacing Changed")
                        .await;
                    for pv in [&pv_h, &pv_k, &pv_l, &pv_a] {
                        monitor.clear(pv);
                    }
                    let _ = ch_seq_msg2.put_string_process(" ").await;
                    KohzuSoftState::DInputChanged
                } else if pv == pv_theta_hilim || pv == pv_theta_lolim {
                    let _ = ch_seq_msg1
                        .put_string_process("Theta Motor Soft Limits Changed")
                        .await;
                    let _ = ch_seq_msg2.put_string_process(" ").await;
                    monitor.clear(&pv_theta_hilim);
                    monitor.clear(&pv_theta_lolim);
                    KohzuSoftState::ThetaLimits
                } else if pv == pv_theta_mot_rbv {
                    // updatePsuedo_soft's thetaMotRdbkPseudo_mon echo.
                    let _ = ch_theta_rdbk_echo.put_f64_process(val).await;
                    KohzuSoftState::UpdateReadback
                } else if pv == pv_e {
                    e_val = val;
                    KohzuSoftState::EChanged
                } else if pv == pv_lambda {
                    lambda_val = val;
                    KohzuSoftState::LambdaChanged
                } else if pv == pv_theta {
                    theta_val = val;
                    KohzuSoftState::ThetaChanged
                } else if pv == pv_put_vals && val as i16 != 0 {
                    desired = [
                        ch_theta_set_ao.get_f64().await,
                        ch_y_set_ao.get_f64().await,
                        ch_z_set_ao.get_f64().await,
                    ];
                    KohzuSoftState::MoveKohzu
                } else if pv == pv_y_offset {
                    y_offset_val = val;
                    let _ = ch_auto_mode.put_i16_process(0).await;
                    let _ = ch_seq_msg1
                        .put_string_process(&format!("y offset changed to {y_offset_val:.6}"))
                        .await;
                    let _ = ch_seq_msg2.put_string_process("Set to Manual Mode").await;
                    KohzuSoftState::CalcMovements
                } else if pv == pv_oper_ack && val as i16 != 0 {
                    let _ = ch_alert.put_i16_process(0).await;
                    let _ = ch_seq_msg1.put_string_process(" ").await;
                    let _ = ch_seq_msg2.put_string_process(" ").await;
                    KohzuSoftState::WaitForCommandEnter
                } else {
                    KohzuSoftState::WaitForCommand
                }
            }
            KohzuSoftState::DInputChanged => {
                // kohzuCtl_soft.st:693-713
                let _ = ch_auto_mode.put_i16_process(0).await;
                let _ = ch_seq_msg2.put_string_process("Set to Manual Mode").await;
                let (d, forbidden, msg) = calc_2d_spacing(
                    ch_a.get_f64().await,
                    ch_h.get_f64().await,
                    ch_k.get_f64().await,
                    ch_l.get_f64().await,
                );
                two_d = d;
                let _ = ch_d.put_f64_process(two_d).await;
                let _ = ch_seq_msg1.put_string_process(msg).await;
                put_limits(theta_hi, theta_lo, two_d).await;
                let _ = ch_alert.put_i16_process(forbidden as i16).await;
                KohzuSoftState::ThetaChanged
            }
            KohzuSoftState::ThetaLimits => {
                // kohzuCtl_soft.st:715-741
                let theta_mot_hi = ch_theta_mot_hilim.get_f64().await;
                let theta_mot_lo = ch_theta_mot_lolim.get_f64().await;
                (theta_hi, theta_lo) = compute_theta_limits(theta_mot_hi, theta_mot_lo);
                if theta_mot_hi > theta_hi {
                    let _ = ch_seq_msg2
                        .put_string_process("ThetaHiLim > 89 deg. (Using 89 deg.)")
                        .await;
                }
                if theta_mot_lo < theta_lo {
                    let _ = ch_seq_msg2
                        .put_string_process("ThetaLoLim < 1 deg. (Using 1 deg.)")
                        .await;
                }
                let _ = ch_theta_hi.put_f64_process(theta_hi).await;
                let _ = ch_theta_lo.put_f64_process(theta_lo).await;
                put_limits(theta_hi, theta_lo, two_d).await;
                KohzuSoftState::CheckDone
            }
            KohzuSoftState::CheckDone => {
                // kohzuCtl_soft.st:946-956
                if all_done().await {
                    kohzu_done = true;
                    KohzuSoftState::WaitForCommandEnter
                } else {
                    KohzuSoftState::UpdateReadback
                }
            }
            KohzuSoftState::EChanged => {
                lambda_val = energy_to_lambda(e_val);
                let _ = ch_lambda.put_f64_process(lambda_val).await;
                KohzuSoftState::LambdaChanged
            }
            KohzuSoftState::LambdaChanged => {
                // kohzuCtl_soft.st:751-766
                if lambda_val > two_d {
                    let _ = ch_seq_msg1
                        .put_string_process("Wavelength > 2d spacing.")
                        .await;
                    let _ = ch_alert.put_i16_process(1).await;
                } else if let Some(th) = lambda_to_theta(lambda_val, two_d) {
                    theta_val = th;
                    let _ = ch_theta.put_f64_process(theta_val).await;
                }
                KohzuSoftState::ThetaChanged
            }
            KohzuSoftState::ThetaChanged => {
                // kohzuCtl_soft.st:768-800
                let (clamped_theta, was_clamped) = clamp_theta(theta_val, theta_lo, theta_hi);
                if was_clamped {
                    let _ = ch_seq_msg1
                        .put_string_process("Theta constrained to LIMIT")
                        .await;
                    let _ = ch_alert.put_i16_process(1).await;
                    theta_val = clamped_theta;
                    if risk_averse {
                        let _ = ch_auto_mode.put_i16_process(0).await;
                        let _ = ch_seq_msg2.put_string_process("Set to Manual Mode").await;
                    } else {
                        will_violate = true;
                    }
                }
                lambda_val = theta_to_lambda(theta_val, two_d);
                let _ = ch_lambda.put_f64_process(lambda_val).await;
                e_val = lambda_to_energy(lambda_val);
                let _ = ch_e.put_f64_process(e_val).await;
                put_readbacks(ch_theta_mot_rbv.get_f64().await, two_d).await;
                KohzuSoftState::CalcMovements
            }
            KohzuSoftState::CalcMovements => {
                // kohzuCtl_soft.st:802-888
                desired = [
                    theta_val,
                    calc_y_position(geom, theta_val, y_offset_val),
                    calc_z_position(geom, theta_val, y_offset_val),
                ];
                let [_, y_mot_desired, z_mot_desired] = desired;
                let y_out = y_mot_desired < ch_y_mot_lolim.get_f64().await
                    || y_mot_desired > ch_y_mot_hilim.get_f64().await;
                let z_out = z_mot_desired < ch_z_mot_lolim.get_f64().await
                    || z_mot_desired > ch_z_mot_hilim.get_f64().await;
                for (out, msg) in [
                    (
                        !cc_mode_of(&mut monitor).y_frozen() && y_out,
                        "Y will exceed soft limits",
                    ),
                    (
                        !cc_mode_of(&mut monitor).z_frozen() && z_out,
                        "Z will exceed soft limits",
                    ),
                ] {
                    if !out {
                        continue;
                    }
                    let _ = ch_seq_msg1.put_string_process(msg).await;
                    let _ = ch_alert.put_i16_process(1).await;
                    if risk_averse {
                        let _ = ch_seq_msg2
                            .put_string_process("Setting to Manual Mode")
                            .await;
                        let _ = ch_alert.put_i16_process(1).await;
                        let _ = ch_auto_mode.put_i16_process(0).await;
                    } else {
                        will_violate = true;
                    }
                }
                let _ = ch_theta_set_ao.put_f64_process(desired[0]).await;
                let _ = ch_y_set_ao.put_f64_process(y_mot_desired).await;
                let _ = ch_z_set_ao.put_f64_process(z_mot_desired).await;
                old_speeds = [
                    ch_theta_mot_velo.get_f64().await,
                    ch_y_mot_velo.get_f64().await,
                    ch_z_mot_velo.get_f64().await,
                ];
                let (th, y, z) = coordinate_speeds(
                    theta_val - ch_theta_mot_rbv.get_f64().await,
                    y_mot_desired - ch_y_mot_rbv.get_f64().await,
                    z_mot_desired - ch_z_mot_rbv.get_f64().await,
                    old_speeds[0],
                    old_speeds[1],
                    old_speeds[2],
                    cc_mode_of(&mut monitor),
                );
                new_speeds = [th, y, z];
                KohzuSoftState::MoveKohzu
            }
            KohzuSoftState::MoveKohzu => {
                // kohzuCtl_soft.st:890-944: unlike kohzuCtl, a limit
                // violation is rolled back only when a move was asked for.
                if !(monitor.var(&pv_auto_mode) != 0.0
                    || ch_put_vals.get_i16().await != 0
                    || use_set_mode.load(Ordering::Relaxed))
                {
                    KohzuSoftState::WaitForCommandEnter
                } else if will_violate {
                    e_val = prev_e;
                    let _ = ch_e.put_f64_process(e_val).await;
                    theta_val = prev_theta;
                    let _ = ch_theta.put_f64_process(theta_val).await;
                    lambda_val = prev_lambda;
                    let _ = ch_lambda.put_f64_process(lambda_val).await;
                    desired = prev_desired;
                    let _ = ch_theta_set_ao.put_f64_process(desired[0]).await;
                    let _ = ch_y_set_ao.put_f64_process(desired[1]).await;
                    let _ = ch_z_set_ao.put_f64_process(desired[2]).await;
                    let _ = ch_seq_msg2.put_string_process("Command ignored").await;
                    KohzuSoftState::UpdateReadback
                } else {
                    if ch_speed_ctrl.get_i16().await != 0 {
                        let _ = ch_theta_mot_velo.put_f64_process(new_speeds[0]).await;
                        if !cc_mode_of(&mut monitor).y_frozen() {
                            let _ = ch_y_mot_velo.put_f64_process(new_speeds[1]).await;
                        }
                        if !cc_mode_of(&mut monitor).z_frozen() {
                            let _ = ch_z_mot_velo.put_f64_process(new_speeds[2]).await;
                        }
                    }
                    let _ = ch_theta_mot_cmd.put_f64_process(desired[0]).await;
                    if !cc_mode_of(&mut monitor).y_frozen() {
                        let _ = ch_y_mot_cmd.put_f64_process(desired[1]).await;
                    }
                    if !cc_mode_of(&mut monitor).z_frozen() {
                        let _ = ch_z_mot_cmd.put_f64_process(desired[2]).await;
                    }
                    let _ = ch_put_vals.put_i16_process(0).await;
                    caused_move = true;
                    KohzuSoftState::UpdateReadback
                }
            }
            KohzuSoftState::UpdateReadback => {
                // kohzuCtl_soft.st:958-1035
                if will_violate {
                    KohzuSoftState::WaitForCommandEnter
                } else if ch_theta_mot_rbv.get_f64().await == 0.0 {
                    let _ = ch_seq_msg1
                        .put_string_process("Theta Motor Readback is 0 !")
                        .await;
                    let _ = ch_seq_msg2
                        .put_string_process("Monochromator stopped")
                        .await;
                    let _ = ch_alert.put_i16_process(1).await;
                    KohzuSoftState::WaitForCommandEnter
                } else if all_done().await {
                    epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
                    let rbv = ch_theta_mot_rbv.get_f64().await;
                    put_readbacks(rbv, two_d).await;
                    kohzu_done = true;
                    let _ = ch_theta_rdbk_echo.put_f64_process(rbv).await;
                    monitor.clear(&pv_theta_mot_rbv);
                    KohzuSoftState::ThetaMotorStopped
                } else if let Some(msg) = limit_switches.hit(cc_mode_of(&mut monitor)).await {
                    let _ = ch_seq_msg1.put_string_process(msg).await;
                    let _ = ch_alert.put_i16_process(1).await;
                    let _ = ch_auto_mode.put_i16_process(0).await;
                    let _ = ch_seq_msg2
                        .put_string_process("Setting to Manual Mode")
                        .await;
                    KohzuSoftState::StopKohzu
                } else {
                    epics_base_rs::runtime::task::sleep(Duration::from_millis(100)).await;
                    put_readbacks(ch_theta_mot_rbv.get_f64().await, two_d).await;
                    KohzuSoftState::UpdateReadback
                }
            }
            KohzuSoftState::ThetaMotorStopped if caused_move => {
                // kohzuCtl_soft.st:1038-1050
                if ch_speed_ctrl.get_i16().await != 0 {
                    let _ = ch_theta_mot_velo.put_f64_process(old_speeds[0]).await;
                    let _ = ch_y_mot_velo.put_f64_process(old_speeds[1]).await;
                    let _ = ch_z_mot_velo.put_f64_process(old_speeds[2]).await;
                }
                caused_move = false;
                KohzuSoftState::WaitForCommandEnter
            }
            KohzuSoftState::ThetaMotorStopped => {
                // Someone moved the theta motor directly (kohzuCtl_soft.st:1052-1071).
                if init_seq_done {
                    let _ = ch_alert.put_i16_process(1).await;
                    let _ = ch_seq_msg1
                        .put_string_process("Only Theta Motor Moved")
                        .await;
                    let _ = ch_seq_msg2
                        .put_string_process("Hit MOVE to Adjust Y & Z")
                        .await;
                    let _ = ch_auto_mode.put_i16_process(0).await;
                }
                theta_val = ch_theta_mot_rbv.get_f64().await;
                let _ = ch_theta.put_f64_process(theta_val).await;
                lambda_val = theta_to_lambda(theta_val, two_d);
                let _ = ch_lambda.put_f64_process(lambda_val).await;
                e_val = lambda_to_energy(lambda_val);
                let _ = ch_e.put_f64_process(e_val).await;
                KohzuSoftState::ThetaChanged
            }
            KohzuSoftState::CheckMotorLimits => {
                // chkMotorLimits / chkMotorLimitsExit (kohzuCtl_soft.st:1074-1143).
                // C re-checks on its delay(2.), so init holds here at that
                // period until every readback is nonzero and inside its limits.
                let rbv = ch_theta_mot_rbv.get_f64().await;
                put_readbacks(rbv, two_d).await;
                let cc_mode = cc_mode_of(&mut monitor);
                let axes = [
                    (
                        !cc_mode.y_frozen(),
                        ch_y_mot_rbv.get_f64().await,
                        ch_y_mot_lolim.get_f64().await,
                        ch_y_mot_hilim.get_f64().await,
                        "Y",
                    ),
                    (
                        !cc_mode.z_frozen(),
                        ch_z_mot_rbv.get_f64().await,
                        ch_z_mot_lolim.get_f64().await,
                        ch_z_mot_hilim.get_f64().await,
                        "Z",
                    ),
                    (
                        true,
                        rbv,
                        ch_theta_mot_lolim.get_f64().await,
                        ch_theta_mot_hilim.get_f64().await,
                        "Theta",
                    ),
                ];
                let mut alert = false;
                for (checked, rdbk, lo, hi, axis) in axes {
                    if !checked {
                        continue;
                    }
                    if rdbk == 0.0 {
                        let _ = ch_seq_msg1
                            .put_string_process(&format!("{axis} Readback of 0 not reasonable"))
                            .await;
                        alert = true;
                    }
                    if rdbk < lo || rdbk > hi {
                        let _ = ch_seq_msg1
                            .put_string_process(&format!("{axis} Readback outside Motor Limits"))
                            .await;
                        alert = true;
                    }
                }
                if alert {
                    let _ = ch_alert.put_i16_process(1).await;
                    let _ = ch_auto_mode.put_i16_process(0).await;
                    let _ = ch_seq_msg2.put_string_process("Set to Manual Mode").await;
                    epics_base_rs::runtime::task::sleep(Duration::from_secs(2)).await;
                    let _ = ch_alert.put_i16_process(1).await;
                    let _ = ch_seq_msg2
                        .put_string_process("Modify Motor Positions/Limits")
                        .await;
                    KohzuSoftState::CheckMotorLimits
                } else {
                    let _ = ch_alert.put_i16_process(0).await;
                    let _ = ch_seq_msg1
                        .put_string_process("All motors within limits")
                        .await;
                    KohzuSoftState::WaitForCommandEnter
                }
            }
            KohzuSoftState::StopKohzu => {
                // stopKohzu / stoppedKohzuWait (kohzuCtl_soft.st:1145-1158).
                let _ = ch_theta_mot_stop.put_i16_process(1).await;
                let _ = ch_y_stop.put_i16_process(1).await;
                let _ = ch_z_stop.put_i16_process(1).await;
                epics_base_rs::runtime::task::sleep(Duration::from_secs(1)).await;
                KohzuSoftState::UpdateReadback
            }
        };
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
