//! Multi-layer monochromator control -- native Rust port of `ml_monoCtl.st`.
//!
//! Controls a multilayer monochromator with variable d-spacing (not crystal):
//! the effective 2d-spacing is `2*D/Order`. Motors: Theta, Theta2, Z (beam
//! tracking, `z = yOffset/tan(2*theta)`); the Y readback is the y offset.
//!
//! The three state sets run as C's: `ml_monoCtl` through its states, with
//! every `monitor` variable written as its monitor arrives, every `sync`ed
//! event flag raised by it (the connect-time monitor and the program's own
//! puts included), and a state left only when one of its `when` clauses is
//! true; `updatePsuedo` echoing the motor fields; `updateSet`.

use std::sync::Arc;

use epics_base_rs::calc::engine::cvt::fmt_f;
use epics_base_rs::server::database::PvDatabase;

use crate::db_access::alloc_origin;
use crate::snl::kohzu_ctl::{HC, update_set};
use crate::snl::vars::{
    DOUBLE, DOUBLE_MON, DOUBLE_SYNC, Decl, Delays, EventFlag, SHORT, SHORT_MON, SHORT_SYNC, Vars,
    ctl_debug,
};
use epics_base_rs::server::snl::{PutQueue, SeqPv, StateSet};

/// `radConv`, as C sets it in state `init` (ml_monoCtl.st:537).
const RAD_CONV: f64 = 57.2958;

/// `ccMode` menu (`MODE_NORMAL`, `MODE_FREEZE_Z`).
const MODE_NORMAL: f64 = 0.0;
const MODE_FREEZE_Z: f64 = 1.0;

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
        // initSequence (ml_monoCtl.st:588-590).
        let y_offset = if (y_offset < 1.0) || (y_offset > 60.0) {
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
            y_offset,
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

/// C `ml_monoCtl_calc2dSpacing`: `(2D/Order, opAlert, seqMsg1)`.
pub fn calc_2d_spacing(d: f64, order: f64) -> (f64, bool, String) {
    let result = (2.0 * d) / order;
    let op_alert = order < 1.0;
    let msg = if op_alert {
        format!("Order == {}?", fmt_f(order, 6))
    } else {
        "New effective d spacing".to_string()
    };
    (result, op_alert, msg)
}

// ---------------------------------------------------------------------------
// SNL variables
// ---------------------------------------------------------------------------

// Indices into `Vars`, one per assigned SNL variable that is not a string.
const OP_ALERT: usize = 0;
const OP_ACK: usize = 1;
const PUT_VALS: usize = 2;
const AUTO_MODE: usize = 3;
const CC_MODE: usize = 4;
const MOVING: usize = 5;
const ORDER: usize = 6;
const D: usize = 7;
const E: usize = 8;
const E_HI: usize = 9;
const E_LO: usize = 10;
const E_RDBK: usize = 11;
const LAMBDA: usize = 12;
const LAMBDA_HI: usize = 13;
const LAMBDA_LO: usize = 14;
const LAMBDA_RDBK: usize = 15;
const THETA: usize = 16;
const THETA_HI: usize = 17;
const THETA_LO: usize = 18;
const THETA_RDBK: usize = 19;
const TH_CMD_ECHO: usize = 20;
const TH2_CMD_ECHO: usize = 21;
const Z_CMD_ECHO: usize = 22;
const TH_RDBK_ECHO: usize = 23;
const TH2_RDBK_ECHO: usize = 24;
const Z_RDBK_ECHO: usize = 25;
const TH_VEL_ECHO: usize = 26;
const TH2_VEL_ECHO: usize = 27;
const Z_VEL_ECHO: usize = 28;
const TH_DMOV_ECHO: usize = 29;
const TH2_DMOV_ECHO: usize = 30;
const Z_DMOV_ECHO: usize = 31;
const TH_STOP: usize = 32;
const TH2_STOP: usize = 33;
const Z_STOP: usize = 34;
const TH_DMOV: usize = 35;
const TH2_DMOV: usize = 36;
const Z_DMOV: usize = 37;
const TH_HLS: usize = 38;
const TH_LLS: usize = 39;
const TH2_HLS: usize = 40;
const TH2_LLS: usize = 41;
const Z_HLS: usize = 42;
const Z_LLS: usize = 43;
const TH_DESIRED: usize = 44;
const TH2_DESIRED: usize = 45;
const Z_DESIRED: usize = 46;
const TH_HLM: usize = 47;
const TH_LLM: usize = 48;
const TH2_HLM: usize = 49;
const TH2_LLM: usize = 50;
const Z_HLM: usize = 51;
const Z_LLM: usize = 52;
const TH_CMD: usize = 53;
const TH2_CMD: usize = 54;
const Z_CMD: usize = 55;
const TH_CMD_MON: usize = 56;
const TH2_CMD_MON: usize = 57;
const Z_CMD_MON: usize = 58;
const TH_SPEED: usize = 59;
const TH2_SPEED: usize = 60;
const Z_SPEED: usize = 61;
const TH_RBV: usize = 62;
const TH2_RBV: usize = 63;
const Z_RBV: usize = 64;
const Y_RBV: usize = 65;
const E_TWEAK: usize = 66;
const E_INC: usize = 67;
const E_DEC: usize = 68;
const L_TWEAK: usize = 69;
const L_INC: usize = 70;
const L_DEC: usize = 71;
const TH_TWEAK: usize = 72;
const TH_INC: usize = 73;
const TH_DEC: usize = 74;
const Y_OFFSET: usize = 75;
const USE_SET: usize = 76;
const DEBUG: usize = 77;
const N_VARS: usize = 78;

/// Each variable's name, PV and declaration, by index (ml_monoCtl.st:26-512).
fn table(cfg: &MlMonoConfig) -> Vec<(&'static str, String, Decl)> {
    let m = |motor: &str, field: &str| cfg.motor_pv(motor, field);
    let (th, th2, z, y) = (&cfg.m_theta, &cfg.m_theta2, &cfg.m_z, &cfg.m_y);
    let (short, short_mon, short_sync) = (SHORT, SHORT_MON, SHORT_SYNC);
    let (dbl, dbl_mon, dbl_sync) = (DOUBLE, DOUBLE_MON, DOUBLE_SYNC);
    let t = vec![
        ("opAlert", cfg.pv("Alert"), short),
        ("opAck", cfg.pv("OperAck"), short_mon),
        ("putVals", cfg.pv("Put"), short_mon),
        ("autoMode", cfg.pv("Mode"), short_mon),
        ("ccMode", cfg.pv("Mode2"), short_mon),
        ("ml_monoMoving", cfg.pv("Moving"), short_mon),
        ("Order", cfg.pv("Order"), dbl_sync),
        ("D", cfg.pv("D"), dbl_sync),
        ("E", cfg.pv("E"), dbl_sync),
        ("EHi", cfg.pv("E.DRVH"), dbl),
        ("ELo", cfg.pv("E.DRVL"), dbl),
        ("ERdbk", cfg.pv("ERdbk"), dbl),
        ("lambda", cfg.pv("Lambda"), dbl_sync),
        ("lambdaHi", cfg.pv("Lambda.DRVH"), dbl),
        ("lambdaLo", cfg.pv("Lambda.DRVL"), dbl),
        ("lambdaRdbk", cfg.pv("LambdaRdbk"), dbl),
        ("theta", cfg.pv("Theta"), dbl_sync),
        ("thetaHi", cfg.pv("Theta.DRVH"), dbl),
        ("thetaLo", cfg.pv("Theta.DRVL"), dbl),
        ("thetaRdbk", cfg.pv("ThetaRdbk"), dbl),
        ("thetaMotCmdEcho", cfg.pv("ThetaCmd"), dbl),
        ("theta2MotCmdEcho", cfg.pv("Theta2Cmd"), dbl),
        ("zMotCmdEcho", cfg.pv("ZCmd"), dbl),
        ("thetaMotRdbkEcho", cfg.pv("ThetaRdbkEcho"), dbl),
        ("theta2MotRdbkEcho", cfg.pv("Theta2RdbkEcho"), dbl),
        ("zMotRdbkEcho", cfg.pv("ZRdbk"), dbl),
        ("thetaMotVelEcho", cfg.pv("ThetaVel"), dbl),
        ("theta2MotVelEcho", cfg.pv("Theta2Vel"), dbl),
        ("zMotVelEcho", cfg.pv("ZVel"), dbl),
        ("thetaDmovEcho", cfg.pv("ThetaDmov"), short),
        ("theta2DmovEcho", cfg.pv("Theta2Dmov"), short),
        ("zDmovEcho", cfg.pv("ZDmov"), short),
        ("thetaMotStop", m(th, ".STOP"), short),
        ("theta2MotStop", m(th2, ".STOP"), short),
        ("zStop", m(z, ".STOP"), short),
        ("thetaDmov", m(th, ".DMOV"), short_sync),
        ("theta2Dmov", m(th2, ".DMOV"), short_sync),
        ("zDmov", m(z, ".DMOV"), short_sync),
        ("thetaHls", m(th, ".HLS"), short_mon),
        ("thetaLls", m(th, ".LLS"), short_mon),
        ("theta2Hls", m(th2, ".HLS"), short_mon),
        ("theta2Lls", m(th2, ".LLS"), short_mon),
        ("zHls", m(z, ".HLS"), short_mon),
        ("zLls", m(z, ".LLS"), short_mon),
        ("thetaMotDesired", cfg.pv("ThetaSet"), dbl_mon),
        ("theta2MotDesired", cfg.pv("Theta2Set"), dbl_mon),
        ("zMotDesired", cfg.pv("ZSet"), dbl_mon),
        ("thetaMotHiLim", m(th, ".HLM"), dbl_sync),
        ("thetaMotLoLim", m(th, ".LLM"), dbl_sync),
        ("theta2MotHiLim", m(th2, ".HLM"), dbl_sync),
        ("theta2MotLoLim", m(th2, ".LLM"), dbl_sync),
        ("zMotHiLim", m(z, ".HLM"), dbl_mon),
        ("zMotLoLim", m(z, ".LLM"), dbl_mon),
        ("thetaMotCmd", m(th, ""), dbl),
        ("theta2MotCmd", m(th2, ""), dbl),
        ("zMotCmd", m(z, ""), dbl),
        ("thetaMotCmdMon", m(th, ""), dbl_sync),
        ("theta2MotCmdMon", m(th2, ""), dbl_sync),
        ("zMotCmdMon", m(z, ""), dbl_sync),
        ("thSpeed", m(th, ".VELO"), dbl_sync),
        ("th2Speed", m(th2, ".VELO"), dbl_sync),
        ("zSpeed", m(z, ".VELO"), dbl_sync),
        ("thetaMotRdbk", m(th, ".RBV"), dbl_sync),
        ("theta2MotRdbk", m(th2, ".RBV"), dbl_sync),
        ("zMotRdbk", m(z, ".RBV"), dbl_sync),
        ("yMotRdbk", m(y, ".RBV"), dbl_sync),
        ("ETweakVal", cfg.pv("ETweak"), dbl_mon),
        ("ETweakInc", cfg.pv("EInc"), short_mon),
        ("ETweakDec", cfg.pv("EDec"), short_mon),
        ("LTweakVal", cfg.pv("LambdaTweak"), dbl_mon),
        ("LTweakInc", cfg.pv("LambdaInc"), short_mon),
        ("LTweakDec", cfg.pv("LambdaDec"), short_mon),
        ("thTweakVal", cfg.pv("ThetaTweak"), dbl_mon),
        ("thTweakInc", cfg.pv("ThetaInc"), short_mon),
        ("thTweakDec", cfg.pv("ThetaDec"), short_mon),
        ("yOffset", cfg.pv("_yOffset"), dbl_sync),
        ("useSetMode", cfg.pv("UseSet"), short_mon),
        ("ml_monoCtlDebug", cfg.pv("CtlDebug"), short_mon),
    ];
    debug_assert_eq!(t.len(), N_VARS);
    t
}

// ---------------------------------------------------------------------------
// Async runner
// ---------------------------------------------------------------------------

/// Run the ml_monoCtl, updatePsuedo and updateSet state sets.
pub async fn run(
    config: MlMonoConfig,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let queue = PutQueue::new(&db, "ml_monoCtl");
    let use_set_pv = config.pv("UseSet");
    let update_set_ss = queue.state_set("updateSet");
    // updateSet (ml_monoCtl.st:1262-1305).
    let update_set = update_set(
        db.clone(),
        update_set_ss,
        use_set_pv,
        ["thetaMotSet", "theta2MotSet", "zMotSet"],
        [&config.m_theta, &config.m_theta2, &config.m_z].map(|m| config.motor_pv(m, ".SET")),
    );
    let pseudo = Arc::new(Pseudo::default());
    let main = MlMono::new(&config, &db, &queue.state_set("ml_monoCtl"), pseudo.clone()).await;
    let update_pseudo_ss = queue.state_set("updatePsuedo");
    tokio::select! {
        () = main.run() => Ok(()),
        () = update_pseudo(&config, &db, &update_pseudo_ss, &pseudo) => Ok(()),
        () = update_set => Ok(()),
    }
}

/// `thetaMotRdbkPseudo_mon` and `theta2MotRdbkPseudo_mon`: set by
/// ml_monoCtl, taken by updatePsuedo.
#[derive(Default)]
struct Pseudo {
    theta: EventFlag,
    theta2: EventFlag,
}

/// ss updatePsuedo (ml_monoCtl.st:1191-1255).
async fn update_pseudo(cfg: &MlMonoConfig, db: &PvDatabase, ss: &StateSet, pseudo: &Pseudo) {
    let watch = [
        TH_RBV,
        TH2_RBV,
        Z_RBV,
        Y_RBV,
        TH_SPEED,
        TH2_SPEED,
        Z_SPEED,
        TH_CMD_MON,
        TH2_CMD_MON,
        Z_CMD_MON,
        TH_DMOV,
        TH2_DMOV,
        Z_DMOV,
    ];
    let mut v = Vars::new(db, ss, &table(cfg), alloc_origin(), watch).await;
    let echoes = [
        (Z_RBV, Z_RDBK_ECHO),
        (Y_RBV, Y_OFFSET),
        (TH_SPEED, TH_VEL_ECHO),
        (TH2_SPEED, TH2_VEL_ECHO),
        (Z_SPEED, Z_VEL_ECHO),
        (TH_CMD_MON, TH_CMD_ECHO),
        (TH2_CMD_MON, TH2_CMD_ECHO),
        (Z_CMD_MON, Z_CMD_ECHO),
        (TH_DMOV, TH_DMOV_ECHO),
        (TH2_DMOV, TH2_DMOV_ECHO),
        (Z_DMOV, Z_DMOV_ECHO),
    ];
    v.flush().await;
    loop {
        let fired = if pseudo.theta.test_and_clear() {
            Some((TH_RBV, TH_RDBK_ECHO))
        } else if pseudo.theta2.test_and_clear() {
            Some((TH2_RBV, TH2_RDBK_ECHO))
        } else {
            echoes.into_iter().find(|&(from, _)| v.test_and_clear(from))
        };
        match fired {
            Some((from, to)) => {
                let x = v.get(from);
                v.set(to, x).await;
                v.flush().await;
            }
            None => tokio::select! {
                () = v.wait(None) => {}
                () = pseudo.theta.raised() => {}
                () = pseudo.theta2.raised() => {}
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    InitSequence,
    WaitForCmndEnter,
    WaitForCmnd,
    Tweak,
    DInputChanged,
    ThetaLimits,
    EChanged,
    LChanged,
    ThChanged,
    CalcMovements,
    MoveMlMono,
    CheckDone,
    UpdateRdbk,
    ThetaMotStopped,
    ChkMotorLimits,
    ChkMotorLimitsExit,
    StopMlMono,
    StoppedMlMonoWait,
}

/// ss ml_monoCtl and its program variables.
struct MlMono {
    v: Vars,
    seq_msg1: SeqPv,
    seq_msg2: SeqPv,
    mot_names: [(SeqPv, String); 4],
    y_off: f64,
    pseudo: Arc<Pseudo>,
    ml_mono_done: bool,
    two_d: f64,
    caused_move: bool,
    init_seq: u8,
    init_seq_done: bool,
    old_th_speed: f64,
    old_th2_speed: f64,
    old_z_speed: f64,
    new_th_speed: f64,
    new_z_speed: f64,
}

impl MlMono {
    async fn new(cfg: &MlMonoConfig, db: &PvDatabase, ss: &StateSet, pseudo: Arc<Pseudo>) -> Self {
        let origin = alloc_origin();
        let ch = |var: &str, suffix: &str| ss.pv_with_origin(var, &cfg.pv(suffix), origin);
        let name = |var: &str, suffix: &str, motor: &str| {
            (ch(var, suffix), format!("{}{}", cfg.prefix, motor))
        };
        Self {
            v: Vars::new(db, ss, &table(cfg), origin, 0..N_VARS).await,
            seq_msg1: ch("seqMsg1", "SeqMsg1"),
            seq_msg2: ch("seqMsg2", "SeqMsg2"),
            mot_names: [
                name("thetaMotName", "ThetaPv", &cfg.m_theta),
                name("theta2MotName", "Theta2Pv", &cfg.m_theta2),
                name("zMotName", "ZPv", &cfg.m_z),
                name("yMotName", "YPv", &cfg.m_y),
            ],
            y_off: cfg.y_offset,
            pseudo,
            ml_mono_done: false,
            two_d: 0.0,
            caused_move: false,
            init_seq: 0,
            init_seq_done: false,
            old_th_speed: 0.0,
            old_th2_speed: 0.0,
            old_z_speed: 0.0,
            new_th_speed: 0.0,
            new_z_speed: 0.0,
        }
    }

    async fn msg1(&self, s: &str) {
        let _ = self.seq_msg1.put_string_process(s).await;
    }

    async fn msg2(&self, s: &str) {
        let _ = self.seq_msg2.put_string_process(s).await;
    }

    /// `x = TwoD*sin(theta/radConv)`.
    fn lambda_of(&self, theta: f64) -> f64 {
        self.two_d * (theta / RAD_CONV).sin()
    }

    async fn run(mut self) {
        let mut state = State::InitSequence;
        loop {
            self.v.flush().await;
            let mut delays = Delays::new();
            state = loop {
                if let Some(next) = self.step(state, &mut delays).await {
                    break next;
                }
                let deadline = delays.next.take();
                self.v.wait(deadline).await;
            };
        }
    }

    /// Evaluate `state`'s `when` clauses in order; run the first true one's
    /// action and return its next state, or `None` when no clause is true.
    async fn step(&mut self, state: State, d: &mut Delays) -> Option<State> {
        let v = &mut self.v;
        match state {
            State::InitSequence => {
                if self.init_seq == 0 {
                    ctl_debug!(v, "ml_monoCtl:initSequence:initSeq=0");
                    v.assign(TH_STOP, 1.0);
                    v.assign(TH2_STOP, 1.0);
                    v.assign(Z_STOP, 1.0);
                    v.set(PUT_VALS, 0.0).await;
                    v.set(AUTO_MODE, 0.0).await;
                    v.set(OP_ACK, 0.0).await;
                    v.set(Y_OFFSET, self.y_off).await;
                    for (ch, name) in &self.mot_names {
                        let _ = ch.put_string_process(name).await;
                    }
                    self.init_seq = 1;
                    return Some(State::DInputChanged);
                }
                if self.init_seq == 1 {
                    ctl_debug!(v, "ml_monoCtl:initSequence:initSeq=1");
                    self.init_seq = 2;
                    return Some(State::ChkMotorLimits);
                }
                if self.init_seq == 2 {
                    ctl_debug!(v, "ml_monoCtl:initSequence:initSeq=2");
                    self.init_seq = 3;
                    return Some(State::UpdateRdbk);
                }
                if self.init_seq == 3 && d.delay(1.0) {
                    ctl_debug!(v, "ml_monoCtl:initSequence:initSeq=3");
                    self.msg1("ml_mono Control Ready").await;
                    self.msg2(" ").await;
                    self.init_seq_done = true;
                    let v = &mut self.v;
                    let x = v.get(TH_RBV);
                    v.set(THETA, x).await;
                    return Some(State::ThChanged);
                }
                None
            }
            State::WaitForCmndEnter => {
                if self.init_seq_done && d.delay(0.1) {
                    ctl_debug!(v, "ml_monoCtl:waitForCmndEnter:initSeqDone");
                    for i in [ORDER, D, E, LAMBDA, THETA] {
                        v.clear(i);
                    }
                    for (to, from) in [
                        (TH_RDBK_ECHO, TH_RBV),
                        (TH2_RDBK_ECHO, TH2_RBV),
                        (Z_RDBK_ECHO, Z_RBV),
                        (Y_OFFSET, Y_RBV),
                        (TH_VEL_ECHO, TH_SPEED),
                        (TH2_VEL_ECHO, TH2_SPEED),
                        (Z_VEL_ECHO, Z_SPEED),
                        (TH_CMD_ECHO, TH_CMD_MON),
                        (TH2_CMD_ECHO, TH2_CMD_MON),
                        (Z_CMD_ECHO, Z_CMD_MON),
                        (TH_DMOV_ECHO, TH_DMOV),
                        (TH2_DMOV_ECHO, TH2_DMOV),
                        (Z_DMOV_ECHO, Z_DMOV),
                    ] {
                        let x = v.get(from);
                        v.set(to, x).await;
                    }
                    if v.on(AUTO_MODE) || v.on(USE_SET) {
                        self.put_rdbk().await;
                    }
                    let v = &mut self.v;
                    v.set(OP_ACK, 0.0).await;
                    ctl_debug!(
                        v,
                        "ml_monoCtl:waitForCmndEnter: ml_monoMoving={}; ml_monoDone={}",
                        v.get(MOVING) as i32,
                        u8::from(self.ml_mono_done)
                    );
                    if v.on(MOVING) && self.ml_mono_done {
                        ctl_debug!(v, "ml_monoCtl:waitForCmndEnter: asserting 'done'.");
                        self.ml_mono_done = false;
                        v.set(MOVING, 0.0).await;
                    }
                    return Some(State::WaitForCmnd);
                }
                if !self.init_seq_done {
                    return Some(State::InitSequence);
                }
                None
            }
            State::WaitForCmnd => {
                if v.test(ORDER) || v.test(D) {
                    ctl_debug!(v, "ml_monoCtl:waitForCmnd:Order/D_mon");
                    v.clear(ORDER);
                    v.clear(D);
                    self.msg1("Layer Spacing Changed").await;
                    self.msg2(" ").await;
                    return Some(State::DInputChanged);
                }
                for (hi, lo, line, msg) in [
                    (
                        TH_HLM,
                        TH_LLM,
                        "ml_monoCtl:waitForCmnd:thetaMotXxLim_mon",
                        "Theta Motor Soft Limits Changed",
                    ),
                    (
                        TH2_HLM,
                        TH2_LLM,
                        "ml_monoCtl:waitForCmnd:theta2MotXxLim_mon",
                        "Theta2 Motor Soft Limits Changed",
                    ),
                ] {
                    let v = &mut self.v;
                    if v.test(hi) || v.test(lo) {
                        ctl_debug!(v, "{line}");
                        v.clear(hi);
                        v.clear(lo);
                        self.msg1(msg).await;
                        self.msg2(" ").await;
                        return Some(State::ThetaLimits);
                    }
                }
                let v = &mut self.v;
                for (rbv, echo, line) in [
                    (
                        TH_RBV,
                        TH_RDBK_ECHO,
                        "ml_monoCtl:waitForCmnd:thetaMotRdbk_mon",
                    ),
                    (
                        TH2_RBV,
                        TH2_RDBK_ECHO,
                        "ml_monoCtl:waitForCmnd:theta2MotRdbk_mon",
                    ),
                ] {
                    if v.test_and_clear(rbv) {
                        ctl_debug!(v, "{line}");
                        let x = v.get(rbv);
                        v.set(echo, x).await;
                        return Some(State::UpdateRdbk);
                    }
                }
                if v.test_and_clear(E) {
                    ctl_debug!(v, "ml_monoCtl:waitForCmnd:E_mon");
                    return Some(State::EChanged);
                }
                if v.test_and_clear(LAMBDA) {
                    ctl_debug!(v, "ml_monoCtl:waitForCmnd:lambda_mon");
                    return Some(State::LChanged);
                }
                if v.test_and_clear(THETA) {
                    ctl_debug!(v, "ml_monoCtl:waitForCmnd:theta_mon");
                    return Some(State::ThChanged);
                }
                if v.on(PUT_VALS) && d.delay(0.1) {
                    ctl_debug!(v, "ml_monoCtl:waitForCmnd:putVals");
                    return Some(State::MoveMlMono);
                }
                if [E_INC, E_DEC, L_INC, L_DEC, TH_INC, TH_DEC]
                    .into_iter()
                    .any(|i| v.on(i))
                {
                    ctl_debug!(v, "ml_monoCtl:waitForCmnd:Tweak");
                    return Some(State::Tweak);
                }
                if v.test_and_clear(Y_OFFSET) && self.init_seq_done {
                    ctl_debug!(v, "ml_monoCtl:waitForCmnd:yOffset_mon");
                    v.set(AUTO_MODE, 0.0).await;
                    let msg = format!("y offset changed to {}", fmt_f(v.get(Y_OFFSET), 6));
                    self.msg1(&msg).await;
                    self.msg2("Set to Manual Mode").await;
                    return Some(State::CalcMovements);
                }
                if v.on(OP_ACK) {
                    ctl_debug!(v, "ml_monoCtl:waitForCmnd:opAck");
                    v.set(OP_ALERT, 0.0).await;
                    self.msg1(" ").await;
                    self.msg2(" ").await;
                    return Some(State::WaitForCmndEnter);
                }
                None
            }
            State::Tweak => {
                for (line, inc, dec, val, step, lo, hi, msg, next) in [
                    (
                        "ml_monoCtl:tweak:E+",
                        E_INC,
                        E_DEC,
                        E,
                        E_TWEAK,
                        E_LO,
                        E_HI,
                        "E tweak exceeds limit",
                        State::EChanged,
                    ),
                    (
                        "ml_monoCtl:tweak:L",
                        L_INC,
                        L_DEC,
                        LAMBDA,
                        L_TWEAK,
                        LAMBDA_LO,
                        LAMBDA_HI,
                        "Lambda tweak exceeds limit",
                        State::LChanged,
                    ),
                    (
                        "ml_monoCtl:tweak:th",
                        TH_INC,
                        TH_DEC,
                        THETA,
                        TH_TWEAK,
                        THETA_LO,
                        THETA_HI,
                        "Theta tweak exceeds limit",
                        State::ThChanged,
                    ),
                ] {
                    let v = &mut self.v;
                    if !(v.on(inc) || v.on(dec)) {
                        continue;
                    }
                    ctl_debug!(v, "{line}");
                    let sign = if v.on(inc) { 1.0 } else { -1.0 };
                    let temp = v.get(val) + v.get(step) * sign;
                    if (temp >= v.get(lo)) && (temp <= v.get(hi)) {
                        v.set(val, temp).await;
                    } else {
                        self.msg1(msg).await;
                        self.v.set(OP_ALERT, 1.0).await;
                    }
                    let v = &mut self.v;
                    let pressed = if v.on(inc) { inc } else { dec };
                    v.set(pressed, 0.0).await;
                    return Some(next);
                }
                None
            }
            State::DInputChanged => {
                ctl_debug!(v, "ml_monoCtl:dInputChanged");
                v.set(AUTO_MODE, 0.0).await;
                self.msg2("Set to Manual Mode").await;
                let (two_d, alert, msg) = calc_2d_spacing(self.v.get(D), self.v.get(ORDER));
                self.two_d = two_d;
                self.v.assign(OP_ALERT, f64::from(u8::from(alert)));
                self.msg1(&msg).await;
                self.put_e_lambda_limits().await;
                self.v.put(OP_ALERT).await;
                Some(State::ThChanged)
            }
            State::ThetaLimits => {
                ctl_debug!(v, "ml_monoCtl:thetaLimits");
                let x = v.get(TH_HLM);
                v.assign(THETA_HI, x);
                let x = v.get(TH_LLM);
                v.assign(THETA_LO, x);
                if v.get(THETA_HI) > 89.0 {
                    v.assign(THETA_HI, 89.0);
                    self.msg2("ThetaHiLim > 89 deg. (Using 89 deg.)").await;
                }
                if self.v.get(THETA_LO) < 0.1 {
                    self.v.assign(THETA_LO, 0.1);
                    self.msg2("ThetaLoLim < 0.1 deg. (Using 0.1 deg.)").await;
                }
                self.v.put(THETA_HI).await;
                self.v.put(THETA_LO).await;
                self.put_e_lambda_limits().await;
                Some(State::CheckDone)
            }
            State::EChanged => {
                ctl_debug!(v, "ml_monoCtl:eChanged");
                let x = HC / v.get(E);
                v.set(LAMBDA, x).await;
                Some(State::LChanged)
            }
            State::LChanged => {
                if v.get(LAMBDA) > self.two_d {
                    ctl_debug!(v, "ml_monoCtl:lChanged: lambda>2*D");
                    self.msg1("Wavelength > 2d spacing.").await;
                    self.v.set(OP_ALERT, 1.0).await;
                } else {
                    ctl_debug!(v, "ml_monoCtl:lChanged");
                    let theta = RAD_CONV * (v.get(LAMBDA) / self.two_d).asin();
                    v.set(THETA, theta).await;
                }
                Some(State::ThChanged)
            }
            State::ThChanged => {
                ctl_debug!(v, "ml_monoCtl:thChanged");
                if (v.get(THETA) <= v.get(THETA_LO)) || (v.get(THETA) >= v.get(THETA_HI)) {
                    self.msg1("Theta constrained to LIMIT").await;
                    self.v.set(OP_ALERT, 1.0).await;
                    self.v.set(AUTO_MODE, 0.0).await;
                    self.msg2("Set to Manual Mode").await;
                }
                let theta = self.v.get(THETA);
                let lambda = self.lambda_of(theta);
                let v = &mut self.v;
                v.set(LAMBDA, lambda).await;
                v.set(E, HC / lambda).await;
                self.put_rdbk().await;
                Some(State::CalcMovements)
            }
            State::CalcMovements => {
                ctl_debug!(v, "ml_monoCtl:calcMovements");
                let theta = v.get(THETA);
                v.assign(TH_DESIRED, theta);
                v.assign(TH2_DESIRED, theta);
                // geom is 1 whatever GEOM says (ml_monoCtl.st:593).
                let x = v.get(Y_OFFSET) / (2.0 * theta / RAD_CONV).tan();
                v.assign(Z_DESIRED, x);
                let z = v.get(Z_DESIRED);
                if v.get(CC_MODE) != MODE_FREEZE_Z && ((z < v.get(Z_LLM)) || (z > v.get(Z_HLM))) {
                    self.msg1("Z will exceed soft limits").await;
                    self.msg2("Setting to Manual Mode").await;
                    self.v.set(OP_ALERT, 1.0).await;
                    self.v.set(AUTO_MODE, 0.0).await;
                }
                let v = &mut self.v;
                for i in [TH_DESIRED, TH2_DESIRED, Z_DESIRED] {
                    v.put(i).await;
                }
                self.old_th_speed = v.get(TH_SPEED);
                self.old_th2_speed = v.get(TH2_SPEED);
                self.old_z_speed = v.get(Z_SPEED);
                let z_dist = (z - v.get(Z_RBV)).abs();
                let th_dist = (theta - v.get(TH_RBV)).abs();
                let mut z_time = z_dist / v.get(Z_SPEED);
                let th_time = th_dist / v.get(TH_SPEED);
                if v.get(CC_MODE) == MODE_FREEZE_Z {
                    z_time = 0.0;
                }
                if th_time >= z_time {
                    self.new_th_speed = v.get(TH_SPEED);
                    self.new_z_speed = z_dist / th_time;
                } else {
                    self.new_th_speed = th_dist / z_time;
                    self.new_z_speed = v.get(Z_SPEED);
                }
                Some(State::MoveMlMono)
            }
            State::MoveMlMono => {
                if v.on(AUTO_MODE) || v.on(PUT_VALS) || v.on(USE_SET) {
                    ctl_debug!(v, "ml_monoCtl:move_ml_mono:auto|put");
                    let normal = v.get(CC_MODE) == MODE_NORMAL;
                    v.assign(TH_SPEED, self.new_th_speed);
                    v.assign(TH2_SPEED, self.new_th_speed);
                    v.assign(Z_SPEED, self.new_z_speed);
                    v.put(TH_SPEED).await;
                    v.put(TH2_SPEED).await;
                    if normal {
                        v.put(Z_SPEED).await;
                    }
                    let x = v.get(TH_DESIRED);
                    v.assign(TH_CMD, x);
                    let x = v.get(TH2_DESIRED);
                    v.assign(TH2_CMD, x);
                    let x = v.get(Z_DESIRED);
                    v.assign(Z_CMD, x);
                    v.put(TH_CMD).await;
                    v.put(TH2_CMD).await;
                    if normal {
                        v.put(Z_CMD).await;
                    }
                    v.set(PUT_VALS, 0.0).await;
                    self.caused_move = true;
                    return Some(State::UpdateRdbk);
                }
                ctl_debug!(v, "ml_monoCtl:move_ml_mono");
                Some(State::WaitForCmndEnter)
            }
            State::CheckDone => {
                if v.on(TH_DMOV) && v.on(TH2_DMOV) && v.on(Z_DMOV) {
                    ctl_debug!(v, "ml_monoCtl:checkDone: done");
                    self.ml_mono_done = true;
                    return Some(State::WaitForCmndEnter);
                }
                ctl_debug!(v, "ml_monoCtl:checkDone: not done");
                Some(State::UpdateRdbk)
            }
            State::UpdateRdbk => {
                let all_done = v.on(TH_DMOV) && v.on(TH2_DMOV) && v.on(Z_DMOV);
                if v.get(TH_RBV) == 0.0 || v.get(TH2_RBV) == 0.0 {
                    ctl_debug!(v, "ml_monoCtl:updateRdbk:thetaMotRdbk==0");
                    self.msg1("Theta Motor Readback is 0 !").await;
                    self.msg2(" ").await;
                    self.v.set(OP_ALERT, 1.0).await;
                    return Some(State::WaitForCmndEnter);
                }
                if all_done {
                    ctl_debug!(
                        v,
                        "ml_monoCtl:updateRdbk:*Dmov (done={},moving={})",
                        u8::from(self.ml_mono_done),
                        v.get(MOVING) as i32
                    );
                    self.put_rdbk().await;
                    self.ml_mono_done = true;
                    self.pseudo.theta.set();
                    self.v.clear(TH_RBV);
                    self.pseudo.theta2.set();
                    self.v.clear(TH2_RBV);
                    ctl_debug!(self.v, "ml_monoCtl:updateRdbk: done");
                    return Some(State::ThetaMotStopped);
                }
                let v = &mut self.v;
                if v.on(TH_HLS) || v.on(TH_LLS) || v.on(TH2_HLS) || v.on(TH2_LLS) {
                    ctl_debug!(v, "ml_monoCtl:updateRdbk: Th limit switch");
                    self.limit_switch("Theta Motor hit a limit switch!").await;
                    return Some(State::StopMlMono);
                }
                if (v.get(CC_MODE) != MODE_FREEZE_Z) && (v.on(Z_HLS) || v.on(Z_LLS)) {
                    ctl_debug!(v, "ml_monoCtl:updateRdbk: Z limit switch");
                    self.limit_switch("Z Motor hit a limit switch!").await;
                    return Some(State::StopMlMono);
                }
                if d.delay(0.1) {
                    if v.get(DEBUG) > 1.0 {
                        println!("ml_monoCtl:updateRdbk:moving");
                    }
                    self.put_rdbk().await;
                    let v = &mut self.v;
                    let x = v.get(TH_RBV);
                    v.set(TH_RDBK_ECHO, x).await;
                    let x = v.get(TH2_RBV);
                    v.set(TH2_RDBK_ECHO, x).await;
                    return Some(State::UpdateRdbk);
                }
                None
            }
            State::ThetaMotStopped => {
                if self.caused_move {
                    ctl_debug!(
                        v,
                        "ml_monoCtl:thetaMotStopped:causedMove (done={},moving={})",
                        u8::from(self.ml_mono_done),
                        v.get(MOVING) as i32
                    );
                    v.assign(TH_SPEED, self.old_th_speed);
                    v.assign(TH2_SPEED, self.old_th2_speed);
                    v.assign(Z_SPEED, self.old_z_speed);
                    for i in [TH_SPEED, TH2_SPEED, Z_SPEED] {
                        v.put(i).await;
                    }
                    self.caused_move = false;
                    return Some(State::WaitForCmndEnter);
                }
                ctl_debug!(
                    v,
                    "ml_monoCtl:thetaMotStopped:!causedMove (done={},moving={})",
                    u8::from(self.ml_mono_done),
                    v.get(MOVING) as i32
                );
                if self.init_seq_done {
                    v.set(OP_ALERT, 1.0).await;
                    self.msg1("Only Theta Motor Moved").await;
                    self.msg2("Hit MOVE to Adjust Z").await;
                    self.v.set(AUTO_MODE, 0.0).await;
                }
                let theta = self.v.get(TH_RBV);
                self.v.set(THETA, theta).await;
                let lambda = self.lambda_of(theta);
                self.v.set(LAMBDA, lambda).await;
                self.v.set(E, HC / lambda).await;
                Some(State::ThChanged)
            }
            State::ChkMotorLimits => {
                ctl_debug!(v, "ml_monoCtl:chkMotorLimits");
                v.assign(OP_ALERT, 0.0);
                self.put_rdbk().await;
                let v = &mut self.v;
                let mut msgs = Vec::new();
                if v.get(CC_MODE) != MODE_FREEZE_Z {
                    if v.get(Z_RBV) == 0.0 {
                        msgs.push("Z Readback of 0 not reasonable");
                    }
                    if (v.get(Z_RBV) < v.get(Z_LLM)) || (v.get(Z_RBV) > v.get(Z_HLM)) {
                        msgs.push("Z Readback outside Motor Limits");
                    }
                }
                if v.get(Y_RBV) == 0.0 {
                    msgs.push("Y Readback of 0 not reasonable");
                }
                if v.get(TH_RBV) == 0.0 {
                    msgs.push("Theta Readback of 0 not reasonable");
                }
                if (v.get(TH_RBV) < v.get(TH_LLM)) || (v.get(TH_RBV) > v.get(TH_HLM)) {
                    msgs.push("Theta Readback outside Motor Limits");
                }
                if v.get(TH2_RBV) == 0.0 {
                    msgs.push("Theta2 Readback of 0 not reasonable");
                }
                if (v.get(TH2_RBV) < v.get(TH2_LLM)) || (v.get(TH2_RBV) > v.get(TH2_HLM)) {
                    msgs.push("Theta2 Readback outside Motor Limits");
                }
                for msg in &msgs {
                    self.msg1(msg).await;
                }
                if !msgs.is_empty() {
                    self.v.assign(OP_ALERT, 1.0);
                    self.v.put(OP_ALERT).await;
                    self.v.set(AUTO_MODE, 0.0).await;
                    self.msg2("Set to Manual Mode").await;
                }
                Some(State::ChkMotorLimitsExit)
            }
            State::ChkMotorLimitsExit => {
                if !v.on(OP_ALERT) {
                    ctl_debug!(v, "ml_monoCtl:chkMotorLimitsExit: OK");
                    v.put(OP_ALERT).await;
                    self.msg1("All motors within limits").await;
                    return Some(State::WaitForCmndEnter);
                }
                if d.delay(2.0) {
                    ctl_debug!(v, "ml_monoCtl:chkMotorLimitsExit: opAlert");
                    v.put(OP_ALERT).await;
                    self.msg2("Modify Motor Positions/Limits").await;
                    return Some(State::ChkMotorLimits);
                }
                None
            }
            State::StopMlMono => {
                ctl_debug!(v, "ml_monoCtl:stop_ml_mono");
                for i in [TH_STOP, TH2_STOP, Z_STOP] {
                    v.put(i).await;
                }
                Some(State::StoppedMlMonoWait)
            }
            State::StoppedMlMonoWait => {
                if d.delay(1.0) {
                    ctl_debug!(v, "ml_monoCtl:stopped_ml_monoWait");
                    return Some(State::UpdateRdbk);
                }
                None
            }
        }
    }

    /// `thetaRdbk = thetaMotRdbk; lambdaRdbk = ...; ERdbk = ...` and their puts.
    async fn put_rdbk(&mut self) {
        let theta = self.v.get(TH_RBV);
        let lambda = self.lambda_of(theta);
        let v = &mut self.v;
        v.assign(THETA_RDBK, theta);
        v.assign(LAMBDA_RDBK, lambda);
        v.assign(E_RDBK, HC / lambda);
        for i in [THETA_RDBK, LAMBDA_RDBK, E_RDBK] {
            v.put(i).await;
        }
    }

    /// lambdaHi/Lo from thetaHi/Lo, then EHi/Lo, each put.
    async fn put_e_lambda_limits(&mut self) {
        let (hi, lo) = (self.v.get(THETA_HI), self.v.get(THETA_LO));
        let (hi, lo) = (self.lambda_of(hi), self.lambda_of(lo));
        let v = &mut self.v;
        v.set(LAMBDA_HI, hi).await;
        v.set(LAMBDA_LO, lo).await;
        v.set(E_HI, HC / lo).await;
        v.set(E_LO, HC / hi).await;
    }

    async fn limit_switch(&mut self, msg: &str) {
        self.msg1(msg).await;
        self.v.set(OP_ALERT, 1.0).await;
        self.v.set(AUTO_MODE, 0.0).await;
        self.msg2("Setting to Manual Mode").await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calc_2d_spacing_is_c_calc2d_spacing() {
        let (two_d, alert, msg) = calc_2d_spacing(25.0, 1.0);
        assert_eq!(
            (two_d, alert, msg.as_str()),
            (50.0, false, "New effective d spacing")
        );
        let (two_d, alert, msg) = calc_2d_spacing(25.0, 2.0);
        assert_eq!(
            (two_d, alert, msg.as_str()),
            (25.0, false, "New effective d spacing")
        );
        // C divides unconditionally and words the alert with %f.
        let (two_d, alert, msg) = calc_2d_spacing(25.0, 0.0);
        assert_eq!(
            (two_d, alert, msg.as_str()),
            (f64::INFINITY, true, "Order == 0.000000?")
        );
        let (_, alert, msg) = calc_2d_spacing(25.0, 0.5);
        assert_eq!((alert, msg.as_str()), (true, "Order == 0.500000?"));
    }

    #[test]
    fn config_pv_names() {
        let cfg = MlMonoConfig::new("xxx:", "m9", "m12", "m10", "m11", 35.0, 1);
        assert_eq!(cfg.pv("E"), "xxx:ml_monoE");
        assert_eq!(cfg.motor_pv("m9", ".RBV"), "xxx:m9.RBV");
        assert_eq!(table(&cfg).len(), N_VARS);
    }

    #[test]
    fn y_offset_outside_1_to_60_is_35() {
        for (y, want) in [(0.5, 35.0), (1.0, 1.0), (60.0, 60.0), (100.0, 35.0)] {
            let cfg = MlMonoConfig::new("xxx:", "m9", "m12", "m10", "m11", y, 1);
            assert_eq!(cfg.y_offset, want, "Y_OFF {y}");
        }
    }
}
