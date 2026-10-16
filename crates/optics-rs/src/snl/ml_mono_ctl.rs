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

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use epics_base_rs::calc::engine::cvt::fmt_f;
use epics_base_rs::server::database::PvDatabase;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::db_access::{DbChannel, DbMultiMonitor, alloc_origin};
use crate::snl::kohzu_ctl::{HC, update_set};

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
const N_VARS: usize = 76;

/// How C declares a variable: `short` or `double`, `monitor`ed, `sync`ed.
#[derive(Clone, Copy)]
struct Decl {
    short: bool,
    monitor: bool,
    sync: bool,
}

const fn decl(short: bool, monitor: bool, sync: bool) -> Decl {
    Decl {
        short,
        monitor,
        sync,
    }
}

/// Each variable's PV and declaration, by index (ml_monoCtl.st:26-512).
fn table(cfg: &MlMonoConfig) -> Vec<(String, Decl)> {
    let m = |motor: &str, field: &str| cfg.motor_pv(motor, field);
    let (th, th2, z, y) = (&cfg.m_theta, &cfg.m_theta2, &cfg.m_z, &cfg.m_y);
    let short = decl(true, false, false);
    let short_mon = decl(true, true, false);
    let short_sync = decl(true, true, true);
    let dbl = decl(false, false, false);
    let dbl_mon = decl(false, true, false);
    let dbl_sync = decl(false, true, true);
    let t = vec![
        (cfg.pv("Alert"), short),
        (cfg.pv("OperAck"), short_mon),
        (cfg.pv("Put"), short_mon),
        (cfg.pv("Mode"), short_mon),
        (cfg.pv("Mode2"), short_mon),
        (cfg.pv("Moving"), short_mon),
        (cfg.pv("Order"), dbl_sync),
        (cfg.pv("D"), dbl_sync),
        (cfg.pv("E"), dbl_sync),
        (cfg.pv("E.DRVH"), dbl),
        (cfg.pv("E.DRVL"), dbl),
        (cfg.pv("ERdbk"), dbl),
        (cfg.pv("Lambda"), dbl_sync),
        (cfg.pv("Lambda.DRVH"), dbl),
        (cfg.pv("Lambda.DRVL"), dbl),
        (cfg.pv("LambdaRdbk"), dbl),
        (cfg.pv("Theta"), dbl_sync),
        (cfg.pv("Theta.DRVH"), dbl),
        (cfg.pv("Theta.DRVL"), dbl),
        (cfg.pv("ThetaRdbk"), dbl),
        (cfg.pv("ThetaCmd"), dbl),
        (cfg.pv("Theta2Cmd"), dbl),
        (cfg.pv("ZCmd"), dbl),
        (cfg.pv("ThetaRdbkEcho"), dbl),
        (cfg.pv("Theta2RdbkEcho"), dbl),
        (cfg.pv("ZRdbk"), dbl),
        (cfg.pv("ThetaVel"), dbl),
        (cfg.pv("Theta2Vel"), dbl),
        (cfg.pv("ZVel"), dbl),
        (cfg.pv("ThetaDmov"), short),
        (cfg.pv("Theta2Dmov"), short),
        (cfg.pv("ZDmov"), short),
        (m(th, ".STOP"), short),
        (m(th2, ".STOP"), short),
        (m(z, ".STOP"), short),
        (m(th, ".DMOV"), short_sync),
        (m(th2, ".DMOV"), short_sync),
        (m(z, ".DMOV"), short_sync),
        (m(th, ".HLS"), short_mon),
        (m(th, ".LLS"), short_mon),
        (m(th2, ".HLS"), short_mon),
        (m(th2, ".LLS"), short_mon),
        (m(z, ".HLS"), short_mon),
        (m(z, ".LLS"), short_mon),
        (cfg.pv("ThetaSet"), dbl_mon),
        (cfg.pv("Theta2Set"), dbl_mon),
        (cfg.pv("ZSet"), dbl_mon),
        (m(th, ".HLM"), dbl_sync),
        (m(th, ".LLM"), dbl_sync),
        (m(th2, ".HLM"), dbl_sync),
        (m(th2, ".LLM"), dbl_sync),
        (m(z, ".HLM"), dbl_mon),
        (m(z, ".LLM"), dbl_mon),
        (m(th, ""), dbl),
        (m(th2, ""), dbl),
        (m(z, ""), dbl),
        (m(th, ""), dbl_sync),
        (m(th2, ""), dbl_sync),
        (m(z, ""), dbl_sync),
        (m(th, ".VELO"), dbl_sync),
        (m(th2, ".VELO"), dbl_sync),
        (m(z, ".VELO"), dbl_sync),
        (m(th, ".RBV"), dbl_sync),
        (m(th2, ".RBV"), dbl_sync),
        (m(z, ".RBV"), dbl_sync),
        (m(y, ".RBV"), dbl_sync),
        (cfg.pv("ETweak"), dbl_mon),
        (cfg.pv("EInc"), short_mon),
        (cfg.pv("EDec"), short_mon),
        (cfg.pv("LambdaTweak"), dbl_mon),
        (cfg.pv("LambdaInc"), short_mon),
        (cfg.pv("LambdaDec"), short_mon),
        (cfg.pv("ThetaTweak"), dbl_mon),
        (cfg.pv("ThetaInc"), short_mon),
        (cfg.pv("ThetaDec"), short_mon),
        (cfg.pv("_yOffset"), dbl_sync),
    ];
    debug_assert_eq!(t.len(), N_VARS);
    t
}

/// One state set's view of the SNL variables: a `monitor`ed variable is
/// written as its monitor arrives and raises the flag `sync`ed to it; the
/// others hold what the program last assigned (0 until then).
struct Vars {
    chans: Vec<DbChannel>,
    decls: Vec<Decl>,
    val: [f64; N_VARS],
    flag: [bool; N_VARS],
    by_pv: HashMap<String, usize>,
    monitor: DbMultiMonitor,
}

impl Vars {
    /// Subscribe the monitored variables among `watch`. Their connect-time
    /// monitors raise their flags, so each found starts raised.
    async fn new(db: &PvDatabase, cfg: &MlMonoConfig, origin: u64, watch: &[usize]) -> Self {
        let table = table(cfg);
        let mut val = [0.0; N_VARS];
        let mut flag = [false; N_VARS];
        let mut by_pv = HashMap::new();
        let mut names = Vec::new();
        for &i in watch {
            let (pv, d) = &table[i];
            if !d.monitor {
                continue;
            }
            // A PV that never connects delivers no monitor and raises nothing.
            let Ok(seed) = db.get_pv(pv) else { continue };
            val[i] = seed.to_f64().unwrap_or(0.0);
            flag[i] = d.sync;
            by_pv.insert(pv.clone(), i);
            names.push(pv.clone());
        }
        Self {
            chans: table
                .iter()
                .map(|(pv, _)| DbChannel::with_origin(db, pv, origin))
                .collect(),
            decls: table.iter().map(|(_, d)| *d).collect(),
            val,
            flag,
            by_pv,
            monitor: DbMultiMonitor::new(db, &names).await,
        }
    }

    fn take(&mut self, pv: &str, v: f64) {
        if let Some(&i) = self.by_pv.get(pv) {
            self.val[i] = v;
            self.flag[i] |= self.decls[i].sync;
        }
    }

    /// Take every monitor already queued.
    async fn poll(&mut self) {
        while let Ok((pv, v)) =
            tokio::time::timeout(std::time::Duration::ZERO, self.monitor.wait_change()).await
        {
            self.take(&pv, v);
        }
    }

    /// Wait for a monitor or `deadline`, whichever is first.
    async fn wait(&mut self, deadline: Option<Instant>) {
        let (pv, v) = match deadline {
            Some(at) => tokio::select! {
                r = self.monitor.wait_change() => r,
                _ = tokio::time::sleep_until(at) => return,
            },
            None => self.monitor.wait_change().await,
        };
        self.take(&pv, v);
    }

    fn on(&self, i: usize) -> bool {
        self.val[i] != 0.0
    }

    /// `pvPut(var)`.
    async fn put(&self, i: usize) {
        let v = self.val[i];
        let _ = if self.decls[i].short {
            self.chans[i].put_i16_process(v as i16).await
        } else {
            self.chans[i].put_f64_process(v).await
        };
    }

    /// `var = v; pvPut(var);`
    async fn set(&mut self, i: usize, v: f64) {
        self.val[i] = v;
        self.put(i).await;
    }

    /// `efTestAndClear`.
    fn test_and_clear(&mut self, i: usize) -> bool {
        std::mem::take(&mut self.flag[i])
    }
}

/// `delay()` in a state's `when` clauses: counted from state entry.
struct Delays {
    entered: Instant,
    next: Option<Instant>,
}

impl Delays {
    fn new() -> Self {
        Self {
            entered: Instant::now(),
            next: None,
        }
    }

    fn delay(&mut self, secs: f64) -> bool {
        let at = self.entered + epics_base_rs::runtime::time::duration_from_secs(secs);
        if Instant::now() >= at {
            return true;
        }
        self.next = Some(self.next.map_or(at, |n| n.min(at)));
        false
    }
}

// ---------------------------------------------------------------------------
// Async runner
// ---------------------------------------------------------------------------

/// Run the ml_monoCtl, updatePsuedo and updateSet state sets.
pub async fn run(
    config: MlMonoConfig,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let use_set_pv = config.pv("UseSet");
    let use_set_mode = Arc::new(AtomicBool::new(
        DbChannel::new(&db, &use_set_pv).get_i16().await != 0,
    ));
    // updateSet (ml_monoCtl.st:1262-1305).
    let update_set = update_set(
        db.clone(),
        use_set_pv,
        [&config.m_theta, &config.m_theta2, &config.m_z].map(|m| config.motor_pv(m, ".SET")),
        use_set_mode.clone(),
    );
    let pseudo = Arc::new(Pseudo::default());
    let main = MlMono::new(&config, &db, use_set_mode, pseudo.clone()).await;
    tokio::select! {
        () = main.run() => Ok(()),
        () = update_pseudo(&config, &db, &pseudo) => Ok(()),
        () = update_set => Ok(()),
    }
}

/// `thetaMotRdbkPseudo_mon` and `theta2MotRdbkPseudo_mon`: set by
/// ml_monoCtl, taken by updatePsuedo.
#[derive(Default)]
struct Pseudo {
    theta: AtomicBool,
    theta2: AtomicBool,
    wake: Notify,
}

/// ss updatePsuedo (ml_monoCtl.st:1191-1255).
async fn update_pseudo(cfg: &MlMonoConfig, db: &PvDatabase, pseudo: &Pseudo) {
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
    let mut v = Vars::new(db, cfg, alloc_origin(), &watch).await;
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
    loop {
        v.poll().await;
        let fired = if pseudo.theta.swap(false, Ordering::AcqRel) {
            Some((TH_RBV, TH_RDBK_ECHO))
        } else if pseudo.theta2.swap(false, Ordering::AcqRel) {
            Some((TH2_RBV, TH2_RDBK_ECHO))
        } else {
            echoes.into_iter().find(|&(from, _)| v.test_and_clear(from))
        };
        match fired {
            Some((from, to)) => v.set(to, v.val[from]).await,
            None => tokio::select! {
                () = v.wait(None) => {}
                () = pseudo.wake.notified() => {}
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
    seq_msg1: DbChannel,
    seq_msg2: DbChannel,
    mot_names: [(DbChannel, String); 4],
    y_off: f64,
    use_set_mode: Arc<AtomicBool>,
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
    async fn new(
        cfg: &MlMonoConfig,
        db: &PvDatabase,
        use_set_mode: Arc<AtomicBool>,
        pseudo: Arc<Pseudo>,
    ) -> Self {
        let origin = alloc_origin();
        let all: Vec<usize> = (0..N_VARS).collect();
        let ch = |suffix: &str| DbChannel::with_origin(db, &cfg.pv(suffix), origin);
        let name = |suffix: &str, motor: &str| (ch(suffix), format!("{}{}", cfg.prefix, motor));
        Self {
            v: Vars::new(db, cfg, origin, &all).await,
            seq_msg1: ch("SeqMsg1"),
            seq_msg2: ch("SeqMsg2"),
            mot_names: [
                name("ThetaPv", &cfg.m_theta),
                name("Theta2Pv", &cfg.m_theta2),
                name("ZPv", &cfg.m_z),
                name("YPv", &cfg.m_y),
            ],
            y_off: cfg.y_offset,
            use_set_mode,
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
            let mut delays = Delays::new();
            state = loop {
                self.v.poll().await;
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
                    v.val[TH_STOP] = 1.0;
                    v.val[TH2_STOP] = 1.0;
                    v.val[Z_STOP] = 1.0;
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
                    self.init_seq = 2;
                    return Some(State::ChkMotorLimits);
                }
                if self.init_seq == 2 {
                    self.init_seq = 3;
                    return Some(State::UpdateRdbk);
                }
                if self.init_seq == 3 && d.delay(1.0) {
                    self.msg1("ml_mono Control Ready").await;
                    self.msg2(" ").await;
                    self.init_seq_done = true;
                    let v = &mut self.v;
                    v.set(THETA, v.val[TH_RBV]).await;
                    return Some(State::ThChanged);
                }
                None
            }
            State::WaitForCmndEnter => {
                if self.init_seq_done && d.delay(0.1) {
                    for i in [ORDER, D, E, LAMBDA, THETA] {
                        v.flag[i] = false;
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
                        v.set(to, v.val[from]).await;
                    }
                    if v.on(AUTO_MODE) || self.use_set_mode.load(Ordering::Acquire) {
                        self.put_rdbk().await;
                    }
                    let v = &mut self.v;
                    v.set(OP_ACK, 0.0).await;
                    if v.on(MOVING) && self.ml_mono_done {
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
                if v.flag[ORDER] || v.flag[D] {
                    v.flag[ORDER] = false;
                    v.flag[D] = false;
                    self.msg1("Layer Spacing Changed").await;
                    self.msg2(" ").await;
                    return Some(State::DInputChanged);
                }
                for (hi, lo, msg) in [
                    (TH_HLM, TH_LLM, "Theta Motor Soft Limits Changed"),
                    (TH2_HLM, TH2_LLM, "Theta2 Motor Soft Limits Changed"),
                ] {
                    let v = &mut self.v;
                    if v.flag[hi] || v.flag[lo] {
                        v.flag[hi] = false;
                        v.flag[lo] = false;
                        self.msg1(msg).await;
                        self.msg2(" ").await;
                        return Some(State::ThetaLimits);
                    }
                }
                let v = &mut self.v;
                for (rbv, echo) in [(TH_RBV, TH_RDBK_ECHO), (TH2_RBV, TH2_RDBK_ECHO)] {
                    if v.test_and_clear(rbv) {
                        v.set(echo, v.val[rbv]).await;
                        return Some(State::UpdateRdbk);
                    }
                }
                if v.test_and_clear(E) {
                    return Some(State::EChanged);
                }
                if v.test_and_clear(LAMBDA) {
                    return Some(State::LChanged);
                }
                if v.test_and_clear(THETA) {
                    return Some(State::ThChanged);
                }
                if v.on(PUT_VALS) && d.delay(0.1) {
                    return Some(State::MoveMlMono);
                }
                if [E_INC, E_DEC, L_INC, L_DEC, TH_INC, TH_DEC]
                    .into_iter()
                    .any(|i| v.on(i))
                {
                    return Some(State::Tweak);
                }
                if v.test_and_clear(Y_OFFSET) && self.init_seq_done {
                    v.set(AUTO_MODE, 0.0).await;
                    let msg = format!("y offset changed to {}", fmt_f(v.val[Y_OFFSET], 6));
                    self.msg1(&msg).await;
                    self.msg2("Set to Manual Mode").await;
                    return Some(State::CalcMovements);
                }
                if v.on(OP_ACK) {
                    v.set(OP_ALERT, 0.0).await;
                    self.msg1(" ").await;
                    self.msg2(" ").await;
                    return Some(State::WaitForCmndEnter);
                }
                None
            }
            State::Tweak => {
                for (inc, dec, val, step, lo, hi, msg, next) in [
                    (
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
                    let sign = if v.on(inc) { 1.0 } else { -1.0 };
                    let temp = v.val[val] + v.val[step] * sign;
                    if (temp >= v.val[lo]) && (temp <= v.val[hi]) {
                        v.set(val, temp).await;
                    } else {
                        self.msg1(msg).await;
                        self.v.set(OP_ALERT, 1.0).await;
                    }
                    let v = &mut self.v;
                    v.set(if v.on(inc) { inc } else { dec }, 0.0).await;
                    return Some(next);
                }
                None
            }
            State::DInputChanged => {
                v.set(AUTO_MODE, 0.0).await;
                self.msg2("Set to Manual Mode").await;
                let (two_d, alert, msg) = calc_2d_spacing(self.v.val[D], self.v.val[ORDER]);
                self.two_d = two_d;
                self.v.val[OP_ALERT] = f64::from(u8::from(alert));
                self.msg1(&msg).await;
                self.put_e_lambda_limits().await;
                self.v.put(OP_ALERT).await;
                Some(State::ThChanged)
            }
            State::ThetaLimits => {
                v.val[THETA_HI] = v.val[TH_HLM];
                v.val[THETA_LO] = v.val[TH_LLM];
                if v.val[THETA_HI] > 89.0 {
                    v.val[THETA_HI] = 89.0;
                    self.msg2("ThetaHiLim > 89 deg. (Using 89 deg.)").await;
                }
                if self.v.val[THETA_LO] < 0.1 {
                    self.v.val[THETA_LO] = 0.1;
                    self.msg2("ThetaLoLim < 0.1 deg. (Using 0.1 deg.)").await;
                }
                self.v.put(THETA_HI).await;
                self.v.put(THETA_LO).await;
                self.put_e_lambda_limits().await;
                Some(State::CheckDone)
            }
            State::EChanged => {
                v.set(LAMBDA, HC / v.val[E]).await;
                Some(State::LChanged)
            }
            State::LChanged => {
                if v.val[LAMBDA] > self.two_d {
                    self.msg1("Wavelength > 2d spacing.").await;
                    self.v.set(OP_ALERT, 1.0).await;
                } else {
                    let theta = RAD_CONV * (v.val[LAMBDA] / self.two_d).asin();
                    v.set(THETA, theta).await;
                }
                Some(State::ThChanged)
            }
            State::ThChanged => {
                if (v.val[THETA] <= v.val[THETA_LO]) || (v.val[THETA] >= v.val[THETA_HI]) {
                    self.msg1("Theta constrained to LIMIT").await;
                    self.v.set(OP_ALERT, 1.0).await;
                    self.v.set(AUTO_MODE, 0.0).await;
                    self.msg2("Set to Manual Mode").await;
                }
                let lambda = self.lambda_of(self.v.val[THETA]);
                let v = &mut self.v;
                v.set(LAMBDA, lambda).await;
                v.set(E, HC / lambda).await;
                self.put_rdbk().await;
                Some(State::CalcMovements)
            }
            State::CalcMovements => {
                let theta = v.val[THETA];
                v.val[TH_DESIRED] = theta;
                v.val[TH2_DESIRED] = theta;
                // geom is 1 whatever GEOM says (ml_monoCtl.st:593).
                v.val[Z_DESIRED] = v.val[Y_OFFSET] / (2.0 * theta / RAD_CONV).tan();
                let z = v.val[Z_DESIRED];
                if v.val[CC_MODE] != MODE_FREEZE_Z && ((z < v.val[Z_LLM]) || (z > v.val[Z_HLM])) {
                    self.msg1("Z will exceed soft limits").await;
                    self.msg2("Setting to Manual Mode").await;
                    self.v.set(OP_ALERT, 1.0).await;
                    self.v.set(AUTO_MODE, 0.0).await;
                }
                let v = &mut self.v;
                for i in [TH_DESIRED, TH2_DESIRED, Z_DESIRED] {
                    v.put(i).await;
                }
                self.old_th_speed = v.val[TH_SPEED];
                self.old_th2_speed = v.val[TH2_SPEED];
                self.old_z_speed = v.val[Z_SPEED];
                let z_dist = (z - v.val[Z_RBV]).abs();
                let th_dist = (theta - v.val[TH_RBV]).abs();
                let mut z_time = z_dist / v.val[Z_SPEED];
                let th_time = th_dist / v.val[TH_SPEED];
                if v.val[CC_MODE] == MODE_FREEZE_Z {
                    z_time = 0.0;
                }
                if th_time >= z_time {
                    self.new_th_speed = v.val[TH_SPEED];
                    self.new_z_speed = z_dist / th_time;
                } else {
                    self.new_th_speed = th_dist / z_time;
                    self.new_z_speed = v.val[Z_SPEED];
                }
                Some(State::MoveMlMono)
            }
            State::MoveMlMono => {
                if v.on(AUTO_MODE) || v.on(PUT_VALS) || self.use_set_mode.load(Ordering::Acquire) {
                    let normal = v.val[CC_MODE] == MODE_NORMAL;
                    v.val[TH_SPEED] = self.new_th_speed;
                    v.val[TH2_SPEED] = self.new_th_speed;
                    v.val[Z_SPEED] = self.new_z_speed;
                    v.put(TH_SPEED).await;
                    v.put(TH2_SPEED).await;
                    if normal {
                        v.put(Z_SPEED).await;
                    }
                    v.val[TH_CMD] = v.val[TH_DESIRED];
                    v.val[TH2_CMD] = v.val[TH2_DESIRED];
                    v.val[Z_CMD] = v.val[Z_DESIRED];
                    v.put(TH_CMD).await;
                    v.put(TH2_CMD).await;
                    if normal {
                        v.put(Z_CMD).await;
                    }
                    v.set(PUT_VALS, 0.0).await;
                    self.caused_move = true;
                    return Some(State::UpdateRdbk);
                }
                Some(State::WaitForCmndEnter)
            }
            State::CheckDone => {
                if v.on(TH_DMOV) && v.on(TH2_DMOV) && v.on(Z_DMOV) {
                    self.ml_mono_done = true;
                    return Some(State::WaitForCmndEnter);
                }
                Some(State::UpdateRdbk)
            }
            State::UpdateRdbk => {
                let all_done = v.on(TH_DMOV) && v.on(TH2_DMOV) && v.on(Z_DMOV);
                if v.val[TH_RBV] == 0.0 || v.val[TH2_RBV] == 0.0 {
                    self.msg1("Theta Motor Readback is 0 !").await;
                    self.msg2(" ").await;
                    self.v.set(OP_ALERT, 1.0).await;
                    return Some(State::WaitForCmndEnter);
                }
                if all_done {
                    self.put_rdbk().await;
                    self.ml_mono_done = true;
                    self.pseudo.theta.store(true, Ordering::Release);
                    self.v.flag[TH_RBV] = false;
                    self.pseudo.theta2.store(true, Ordering::Release);
                    self.v.flag[TH2_RBV] = false;
                    self.pseudo.wake.notify_one();
                    return Some(State::ThetaMotStopped);
                }
                let v = &mut self.v;
                if v.on(TH_HLS) || v.on(TH_LLS) || v.on(TH2_HLS) || v.on(TH2_LLS) {
                    self.limit_switch("Theta Motor hit a limit switch!").await;
                    return Some(State::StopMlMono);
                }
                if (v.val[CC_MODE] != MODE_FREEZE_Z) && (v.on(Z_HLS) || v.on(Z_LLS)) {
                    self.limit_switch("Z Motor hit a limit switch!").await;
                    return Some(State::StopMlMono);
                }
                if d.delay(0.1) {
                    self.put_rdbk().await;
                    let v = &mut self.v;
                    v.set(TH_RDBK_ECHO, v.val[TH_RBV]).await;
                    v.set(TH2_RDBK_ECHO, v.val[TH2_RBV]).await;
                    return Some(State::UpdateRdbk);
                }
                None
            }
            State::ThetaMotStopped => {
                if self.caused_move {
                    v.val[TH_SPEED] = self.old_th_speed;
                    v.val[TH2_SPEED] = self.old_th2_speed;
                    v.val[Z_SPEED] = self.old_z_speed;
                    for i in [TH_SPEED, TH2_SPEED, Z_SPEED] {
                        v.put(i).await;
                    }
                    self.caused_move = false;
                    return Some(State::WaitForCmndEnter);
                }
                if self.init_seq_done {
                    v.set(OP_ALERT, 1.0).await;
                    self.msg1("Only Theta Motor Moved").await;
                    self.msg2("Hit MOVE to Adjust Z").await;
                    self.v.set(AUTO_MODE, 0.0).await;
                }
                let theta = self.v.val[TH_RBV];
                self.v.set(THETA, theta).await;
                let lambda = self.lambda_of(theta);
                self.v.set(LAMBDA, lambda).await;
                self.v.set(E, HC / lambda).await;
                Some(State::ThChanged)
            }
            State::ChkMotorLimits => {
                v.val[OP_ALERT] = 0.0;
                self.put_rdbk().await;
                let v = &self.v;
                let rbv = |i| v.val[i];
                let mut msgs = Vec::new();
                if v.val[CC_MODE] != MODE_FREEZE_Z {
                    if rbv(Z_RBV) == 0.0 {
                        msgs.push("Z Readback of 0 not reasonable");
                    }
                    if (rbv(Z_RBV) < v.val[Z_LLM]) || (rbv(Z_RBV) > v.val[Z_HLM]) {
                        msgs.push("Z Readback outside Motor Limits");
                    }
                }
                if rbv(Y_RBV) == 0.0 {
                    msgs.push("Y Readback of 0 not reasonable");
                }
                if rbv(TH_RBV) == 0.0 {
                    msgs.push("Theta Readback of 0 not reasonable");
                }
                if (rbv(TH_RBV) < v.val[TH_LLM]) || (rbv(TH_RBV) > v.val[TH_HLM]) {
                    msgs.push("Theta Readback outside Motor Limits");
                }
                if rbv(TH2_RBV) == 0.0 {
                    msgs.push("Theta2 Readback of 0 not reasonable");
                }
                if (rbv(TH2_RBV) < v.val[TH2_LLM]) || (rbv(TH2_RBV) > v.val[TH2_HLM]) {
                    msgs.push("Theta2 Readback outside Motor Limits");
                }
                for msg in &msgs {
                    self.msg1(msg).await;
                }
                if !msgs.is_empty() {
                    self.v.val[OP_ALERT] = 1.0;
                    self.v.put(OP_ALERT).await;
                    self.v.set(AUTO_MODE, 0.0).await;
                    self.msg2("Set to Manual Mode").await;
                }
                Some(State::ChkMotorLimitsExit)
            }
            State::ChkMotorLimitsExit => {
                if !v.on(OP_ALERT) {
                    v.put(OP_ALERT).await;
                    self.msg1("All motors within limits").await;
                    return Some(State::WaitForCmndEnter);
                }
                if d.delay(2.0) {
                    v.put(OP_ALERT).await;
                    self.msg2("Modify Motor Positions/Limits").await;
                    return Some(State::ChkMotorLimits);
                }
                None
            }
            State::StopMlMono => {
                for i in [TH_STOP, TH2_STOP, Z_STOP] {
                    v.put(i).await;
                }
                Some(State::StoppedMlMonoWait)
            }
            State::StoppedMlMonoWait => d.delay(1.0).then_some(State::UpdateRdbk),
        }
    }

    /// `thetaRdbk = thetaMotRdbk; lambdaRdbk = ...; ERdbk = ...` and their puts.
    async fn put_rdbk(&mut self) {
        let theta = self.v.val[TH_RBV];
        let lambda = self.lambda_of(theta);
        let v = &mut self.v;
        v.val[THETA_RDBK] = theta;
        v.val[LAMBDA_RDBK] = lambda;
        v.val[E_RDBK] = HC / lambda;
        for i in [THETA_RDBK, LAMBDA_RDBK, E_RDBK] {
            v.put(i).await;
        }
    }

    /// lambdaHi/Lo from thetaHi/Lo, then EHi/Lo, each put.
    async fn put_e_lambda_limits(&mut self) {
        let hi = self.lambda_of(self.v.val[THETA_HI]);
        let lo = self.lambda_of(self.v.val[THETA_LO]);
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
