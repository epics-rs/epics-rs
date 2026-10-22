//! High-resolution analyzer crystal control -- native Rust port of `hrCtl.st`.
//!
//! Controls a 4-bounce high-resolution monochromator with two analyzer crystals,
//! each with independent (H,K,L,A) parameters. Supports nested and symmetric
//! geometries, single-crystal and two-crystal operating modes, and world/local
//! offset tracking for beam wander correction.
//!
//! Crystal angles are in *micro-radian* motor units internally; user-facing
//! angles (theta, phi) are in degrees.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::snl::{PutQueue, SeqPv, StateSet};
use epics_libcom_rs::cfmt::CFloat;

use crate::db_access::alloc_origin;
use crate::snl::vars::HC;
use crate::snl::vars::{
    DOUBLE, DOUBLE_MON, DOUBLE_SYNC, Decl, Delays, SHORT, SHORT_MON, SHORT_SYNC, Vars, ctl_debug,
};

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
// State enum
// ---------------------------------------------------------------------------

/// The states of ss hr_Ctl (hrCtl.st:552-1477).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HrState {
    Init,
    InitSequence,
    WaitForCmndEnter,
    WaitForCmnd,
    CheckAutoMode,
    PhiLimits,
    CheckDone,
    DInputChanged,
    Tweak,
    EChanged,
    LChanged,
    ThChanged,
    CalcMovements,
    MoveHr,
    UpdateRdbk,
    UpdateRdbkAfterDelay,
    MotorsStopped,
    ChkMotorLimits,
    ChkMotorLimitsExit,
    StopHr,
    StoppedHrWait,
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
// Program variables
// ---------------------------------------------------------------------------

// Indices into `Vars`, one per assigned variable that is not a string.
const DEBUG: usize = 0;
const OP_ALERT: usize = 1;
const OP_ACK: usize = 2;
const PUT_VALS: usize = 3;
const AUTO_MODE: usize = 4;
const OP_MODE: usize = 5;
const GEOM: usize = 6;
const MOVING: usize = 7;
const H1: usize = 8;
const K1: usize = 9;
const L1: usize = 10;
const A1: usize = 11;
const D1: usize = 12;
const H2: usize = 13;
const K2: usize = 14;
const L2: usize = 15;
const A2: usize = 16;
const D2: usize = 17;
const E: usize = 18;
const E_HI: usize = 19;
const E_LO: usize = 20;
const E_RDBK: usize = 21;
const LAMBDA: usize = 22;
const LAMBDA_HI: usize = 23;
const LAMBDA_LO: usize = 24;
const LAMBDA_RDBK: usize = 25;
const THETA1: usize = 26;
const THETA1_HI: usize = 27;
const THETA1_LO: usize = 28;
const THETA1_RDBK: usize = 29;
const PHI1: usize = 30;
const PHI1_OFF: usize = 31;
const PHI1_HI: usize = 32;
const PHI1_LO: usize = 33;
const PHI1_RDBK: usize = 34;
const THETA2: usize = 35;
const THETA2_HI: usize = 36;
const THETA2_LO: usize = 37;
const THETA2_RDBK: usize = 38;
const PHI2: usize = 39;
const PHI2_OFF: usize = 40;
const PHI2_HI: usize = 41;
const PHI2_LO: usize = 42;
const PHI2_RDBK: usize = 43;
const CMD_ECHO: [usize; 2] = [44, 45];
const RDBK_ECHO: [usize; 2] = [46, 47];
const DMOV_ECHO: [usize; 2] = [48, 49];
const STOP: [usize; 2] = [50, 51];
const DMOV: [usize; 2] = [52, 53];
const HLS: [usize; 2] = [54, 55];
const LLS: [usize; 2] = [56, 57];
const DESIRED: [usize; 2] = [58, 59];
const HLM: [usize; 2] = [60, 61];
const LLM: [usize; 2] = [62, 63];
const CMD: [usize; 2] = [64, 65];
const CMD_MON: [usize; 2] = [66, 67];
const RBV: [usize; 2] = [68, 69];
const WORLD_OFF: usize = 70;
const WORLD_TWEAK: usize = 71;
const WORLD_INC: usize = 72;
const WORLD_DEC: usize = 73;
const USE_SET: usize = 74;
const E_TWEAK: usize = 75;
const E_INC: usize = 76;
const E_DEC: usize = 77;
const L_TWEAK: usize = 78;
const L_INC: usize = 79;
const L_DEC: usize = 80;
const TH1_TWEAK: usize = 81;
const TH1_INC: usize = 82;
const TH1_DEC: usize = 83;
const TH2_TWEAK: usize = 84;
const TH2_INC: usize = 85;
const TH2_DEC: usize = 86;
const THETA_MIN: usize = 87;
const THETA_MAX: usize = 88;
const N_VARS: usize = 89;

/// The limits phiLimits zeroes in set mode, in C's put order.
const LIMITS: [usize; 12] = [
    E_HI, E_LO, LAMBDA_HI, LAMBDA_LO, THETA1_HI, THETA1_LO, PHI1_HI, PHI1_LO, THETA2_HI, THETA2_LO,
    PHI2_HI, PHI2_LO,
];

/// Each variable's name, PV and declaration, by index (hrCtl.st:43-489).
fn table(c: &HrConfig) -> Vec<(&'static str, String, Decl)> {
    let pv = |s: &str| c.pv(s);
    let motors = |vars: [&'static str; 2], field: &str, decl: Decl| {
        [(vars[0], &c.m_phi1), (vars[1], &c.m_phi2)]
            .map(|(var, m)| (var, c.motor_pv(m, field), decl))
    };
    let echoes = |vars: [&'static str; 2], suffix: &str, decl: Decl| {
        [(vars[0], "phi1"), (vars[1], "phi2")]
            .map(|(var, phi)| (var, pv(&format!("{phi}{suffix}")), decl))
    };
    let mut t = vec![
        ("hrCtlDebug", c.pv_no_underscore("CtlDebug"), SHORT_MON),
        ("opAlert", pv("AlertBO"), SHORT),
        ("opAck", pv("OperAckBO"), SHORT_MON),
        ("putVals", pv("PutBO"), SHORT_MON),
        ("autoMode", pv("ModeBO"), SHORT_MON),
        ("opMode", pv("Mode2MO"), SHORT_SYNC),
        ("Geom", pv("GeomMO"), SHORT_SYNC),
        ("hr_Moving", pv("Moving"), SHORT_SYNC),
        ("H1", pv("H1AO"), DOUBLE_SYNC),
        ("K1", pv("K1AO"), DOUBLE_SYNC),
        ("L1", pv("L1AO"), DOUBLE_SYNC),
        ("A1", pv("A1AO"), DOUBLE_SYNC),
        ("D1", pv("2d1AO"), DOUBLE),
        ("H2", pv("H2AO"), DOUBLE_SYNC),
        ("K2", pv("K2AO"), DOUBLE_SYNC),
        ("L2", pv("L2AO"), DOUBLE_SYNC),
        ("A2", pv("A2AO"), DOUBLE_SYNC),
        ("D2", pv("2d2AO"), DOUBLE),
        ("E", pv("EAO"), DOUBLE_SYNC),
        ("EHi", pv("EAO.DRVH"), DOUBLE),
        ("ELo", pv("EAO.DRVL"), DOUBLE),
        ("ERdbk", pv("ERdbkAO"), DOUBLE),
        ("lambda", pv("LambdaAO"), DOUBLE_SYNC),
        ("lambdaHi", pv("LambdaAO.DRVH"), DOUBLE),
        ("lambdaLo", pv("LambdaAO.DRVL"), DOUBLE),
        ("lambdaRdbk", pv("LambdaRdbkAO"), DOUBLE),
    ];
    for (n, vars) in [
        (
            "1",
            [
                "theta1",
                "theta1Hi",
                "theta1Lo",
                "theta1Rdbk",
                "phi1",
                "phi1Off",
                "phi1Hi",
                "phi1Lo",
                "phi1Rdbk",
            ],
        ),
        (
            "2",
            [
                "theta2",
                "theta2Hi",
                "theta2Lo",
                "theta2Rdbk",
                "phi2",
                "phi2Off",
                "phi2Hi",
                "phi2Lo",
                "phi2Rdbk",
            ],
        ),
    ] {
        t.extend([
            (vars[0], pv(&format!("Theta{n}AO")), DOUBLE_SYNC),
            (vars[1], pv(&format!("Theta{n}AO.DRVH")), DOUBLE),
            (vars[2], pv(&format!("Theta{n}AO.DRVL")), DOUBLE),
            (vars[3], pv(&format!("Theta{n}RdbkAO")), DOUBLE),
            (vars[4], pv(&format!("phi{n}AO")), DOUBLE_SYNC),
            (vars[5], pv(&format!("phi{n}OffAO")), DOUBLE_SYNC),
            (vars[6], pv(&format!("phi{n}AO.DRVH")), DOUBLE),
            (vars[7], pv(&format!("phi{n}AO.DRVL")), DOUBLE),
            (vars[8], pv(&format!("phi{n}RdbkAO")), DOUBLE),
        ]);
    }
    t.extend(echoes(
        ["phi1MotCmdEcho", "phi2MotCmdEcho"],
        "CmdAO",
        DOUBLE,
    ));
    t.extend(echoes(
        ["phi1MotRdbkEcho", "phi2MotRdbkEcho"],
        "RdbkAI",
        DOUBLE,
    ));
    t.extend(echoes(["phi1DmovEcho", "phi2DmovEcho"], "DmovBI", SHORT));
    t.extend(motors(["phi1MotStop", "phi2MotStop"], ".STOP", SHORT));
    t.extend(motors(["phi1Dmov", "phi2Dmov"], ".DMOV", SHORT_SYNC));
    t.extend(motors(["phi1Hls", "phi2Hls"], ".HLS", SHORT_MON));
    t.extend(motors(["phi1Lls", "phi2Lls"], ".LLS", SHORT_MON));
    t.extend(echoes(
        ["phi1MotDesired", "phi2MotDesired"],
        "SetAO",
        DOUBLE_MON,
    ));
    t.extend(motors(
        ["phi1MotHiLim", "phi2MotHiLim"],
        ".HLM",
        DOUBLE_SYNC,
    ));
    t.extend(motors(
        ["phi1MotLoLim", "phi2MotLoLim"],
        ".LLM",
        DOUBLE_SYNC,
    ));
    t.extend(motors(["phi1MotCmd", "phi2MotCmd"], "", DOUBLE));
    t.extend(motors(["phi1MotCmdMon", "phi2MotCmdMon"], "", DOUBLE_SYNC));
    t.extend(motors(["phi1MotRdbk", "phi2MotRdbk"], ".RBV", DOUBLE_SYNC));
    t.extend([
        ("worldOff", pv("worldOffAO"), DOUBLE_SYNC),
        ("worldTweakVal", pv("worldTweakAI"), DOUBLE_MON),
        ("worldTweakInc", pv("worldIncBO"), SHORT_MON),
        ("worldTweakDec", pv("worldDecBO"), SHORT_MON),
        ("useSetMode", pv("UseSetBO"), SHORT_SYNC),
    ]);
    for (vars, (step, inc, dec)) in [
        (
            ["ETweakVal", "ETweakInc", "ETweakDec"],
            ("ETweakAI", "EIncBO", "EDecBO"),
        ),
        (
            ["LTweakVal", "LTweakInc", "LTweakDec"],
            ("LambdaTweakAI", "LambdaIncBO", "LambdaDecBO"),
        ),
        (
            ["th1TweakVal", "th1TweakInc", "th1TweakDec"],
            ("Theta1TweakAI", "Theta1IncBO", "Theta1DecBO"),
        ),
        (
            ["th2TweakVal", "th2TweakInc", "th2TweakDec"],
            ("Theta2TweakAI", "Theta2IncBO", "Theta2DecBO"),
        ),
    ] {
        t.extend([
            (vars[0], pv(step), DOUBLE_MON),
            (vars[1], pv(inc), SHORT_MON),
            (vars[2], pv(dec), SHORT_MON),
        ]);
    }
    t.push(("thetaMin", pv("thetaMin"), DOUBLE_MON));
    t.push(("thetaMax", pv("thetaMax"), DOUBLE_MON));
    debug_assert_eq!(t.len(), N_VARS);
    t
}

// ---------------------------------------------------------------------------
// Async runner
// ---------------------------------------------------------------------------

/// Run the hr_Ctl and updatePsuedo state sets.
pub async fn run(
    config: HrConfig,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let queue = PutQueue::new(&db, "hr_Ctl");
    let main = Hr::new(&config, &db, &queue.state_set("hr_Ctl")).await;
    let pseudo = queue.state_set("updatePsuedo");
    tokio::select! {
        () = main.run() => {}
        () = update_pseudo(&config, &db, &pseudo) => {}
    }
    Ok(())
}

/// ss updatePsuedo (hrCtl.st:1487-1536). Its phi readback clauses never
/// fire: nothing sets phi1MotRdbkPseudo_mon or phi2MotRdbkPseudo_mon.
async fn update_pseudo(config: &HrConfig, db: &PvDatabase, ss: &StateSet) {
    let watch = CMD_MON.into_iter().chain(DMOV).chain([DEBUG]);
    let mut v = Vars::new(db, ss, &table(config), alloc_origin(), watch).await;
    ctl_debug!(v, "hrCtl:updatePsuedo:init: entry");
    ctl_debug!(v, "hrCtl:updatePsuedo:update: entry");
    let echoes = [
        (CMD_MON[0], CMD_ECHO[0]),
        (CMD_MON[1], CMD_ECHO[1]),
        (DMOV[0], DMOV_ECHO[0]),
        (DMOV[1], DMOV_ECHO[1]),
    ];
    v.flush().await;
    loop {
        match echoes.into_iter().find(|&(from, _)| v.test(from)) {
            Some((from, to)) => {
                let x = v.get(from);
                v.set(to, x).await;
                v.clear(from);
                v.flush().await;
            }
            None => v.wait(None).await,
        }
    }
}

/// ss hr_Ctl and its program variables.
struct Hr {
    v: Vars,
    seq_msg1: SeqPv,
    seq_msg2: SeqPv,
    mot_names: [(SeqPv, String); 2],
    /// seqMsg1 as last written: dInputChanged puts it after calc2dSpacing,
    /// which writes it only for an invalid HKL.
    msg1: String,
    put_complete: [bool; 2],
    hr_done: bool,
    caused_move: bool,
    init_seq: u8,
    init_seq_done: bool,
}

impl Hr {
    async fn new(c: &HrConfig, db: &PvDatabase, ss: &StateSet) -> Self {
        let origin = alloc_origin();
        let ch = |var: &str, s: &str| ss.pv_with_origin(var, &c.pv(s), origin);
        Self {
            v: Vars::new(db, ss, &table(c), origin, 0..N_VARS).await,
            seq_msg1: ch("seqMsg1", "SeqMsg1SI"),
            seq_msg2: ch("seqMsg2", "SeqMsg2SI"),
            mot_names: [
                (
                    ch("phi1MotName", "phi1PvSI"),
                    format!("{}{}", c.prefix, c.m_phi1),
                ),
                (
                    ch("phi2MotName", "phi2PvSI"),
                    format!("{}{}", c.prefix, c.m_phi2),
                ),
            ],
            msg1: String::new(),
            put_complete: [false; 2],
            hr_done: false,
            caused_move: false,
            init_seq: 0,
            init_seq_done: false,
        }
    }

    async fn msg1(&mut self, s: &str) {
        self.msg1 = s.to_string();
        let _ = self.seq_msg1.put_string_process(s).await;
    }

    async fn msg2(&self, s: &str) {
        let _ = self.seq_msg2.put_string_process(s).await;
    }

    async fn run(mut self) {
        let mut state = HrState::Init;
        let mut prev = None;
        loop {
            if prev != Some(state) {
                self.entry(state);
            }
            self.v.flush().await;
            let mut delays = Delays::new();
            let next = loop {
                if let Some(next) = self.step(state, &mut delays).await {
                    break next;
                }
                let deadline = delays.next.take();
                self.v.wait(deadline).await;
            };
            prev = Some(state);
            state = next;
        }
    }

    /// The entry blocks.
    fn entry(&mut self, state: HrState) {
        let name = match state {
            HrState::Init => "init:",
            HrState::InitSequence => "initSequence:",
            HrState::WaitForCmndEnter => "waitForCmndEnter:",
            HrState::WaitForCmnd => "waitForCmnd:",
            HrState::CheckAutoMode => "checkAutoMode:",
            HrState::PhiLimits => "phiLimits:",
            HrState::CheckDone => "checkDone:",
            HrState::DInputChanged => "dInputChanged:",
            HrState::Tweak => "tweak:",
            HrState::EChanged => "eChanged:",
            HrState::LChanged => "lChanged:",
            HrState::ThChanged => "thChanged:",
            HrState::CalcMovements => "calcMovements:",
            HrState::MoveHr => "moveHR",
            HrState::UpdateRdbk => "updateRdbk:",
            HrState::MotorsStopped => "motorsStopped:",
            HrState::ChkMotorLimits => "chkMotorLimits:",
            HrState::ChkMotorLimitsExit => "chkMotorLimitsExit:",
            HrState::StopHr => "stopHR:",
            HrState::StoppedHrWait => "stoppedHRWait:",
            HrState::UpdateRdbkAfterDelay => "",
        };
        if !name.is_empty() {
            ctl_debug!(self.v, "hrCtl:{name} entry");
        }
        if matches!(state, HrState::CheckDone | HrState::UpdateRdbk) {
            for (done, cmd) in self.put_complete.iter_mut().zip(CMD) {
                if !*done {
                    *done = self.v.put_complete(cmd);
                }
            }
        }
    }

    fn op_mode(&mut self) -> f64 {
        self.v.get(OP_MODE)
    }

    fn nested(&mut self) -> bool {
        self.v.get(GEOM) == 0.0
    }

    /// Evaluate `state`'s `when` clauses in order; run the first true one's
    /// action and return its next state, or `None` when no clause is true.
    async fn step(&mut self, state: HrState, d: &mut Delays) -> Option<HrState> {
        let v = &mut self.v;
        match state {
            HrState::Init => {
                self.init_seq = 0;
                Some(HrState::InitSequence)
            }
            HrState::InitSequence => match self.init_seq {
                0 => {
                    for i in STOP {
                        v.assign(i, 1.0);
                    }
                    self.put_complete = [true; 2];
                    v.set(PUT_VALS, 0.0).await;
                    v.set(AUTO_MODE, 0.0).await;
                    v.set(OP_ACK, 0.0).await;
                    for (ch, name) in &self.mot_names {
                        let _ = ch.put_string_process(name).await;
                    }
                    self.init_seq = 1;
                    Some(HrState::DInputChanged)
                }
                1 => {
                    self.init_seq = 2;
                    Some(HrState::ChkMotorLimits)
                }
                2 => {
                    self.init_seq = 3;
                    Some(HrState::UpdateRdbk)
                }
                3 => {
                    self.msg1("HR Control Ready").await;
                    self.msg2(" ").await;
                    self.init_seq_done = true;
                    let nested = self.nested();
                    let v = &mut self.v;
                    let world = v.get(WORLD_OFF) * UR2D;
                    let phi1 = v.get(RBV[0]) * UR2D + (v.get(PHI1_OFF) + world);
                    v.set(PHI1, phi1).await;
                    v.set(THETA1, phi1).await;
                    let (phi2, theta2) = if nested {
                        let phi2 = v.get(RBV[1]) * UR2D + v.get(PHI2_OFF) + world;
                        (phi2, phi2 - phi1 - phi1)
                    } else {
                        let phi2 = v.get(RBV[1]) * UR2D + v.get(PHI2_OFF) - world;
                        (phi2, phi2)
                    };
                    ctl_debug!(
                        v,
                        "hrCtl:initSequence: opMode={}, Geom={}, phi2={:.6}",
                        v.get(OP_MODE) as i32,
                        v.get(GEOM) as i32,
                        CFloat(phi2)
                    );
                    v.set(PHI2, phi2).await;
                    v.set(THETA2, theta2).await;
                    Some(HrState::ThChanged)
                }
                _ => None,
            },
            HrState::WaitForCmndEnter => {
                if self.init_seq_done && d.delay(0.1) {
                    ctl_debug!(v, "hrCtl:waitForCmdEnter: initSeqDone");
                    for i in [
                        H1, K1, L1, A1, H2, K2, L2, A2, E, LAMBDA, THETA1, THETA2, PHI1_OFF,
                        PHI2_OFF, WORLD_OFF, USE_SET, OP_MODE, GEOM,
                    ] {
                        v.clear(i);
                    }
                    for (to, from) in [
                        (RDBK_ECHO[0], RBV[0]),
                        (RDBK_ECHO[1], RBV[1]),
                        (CMD_ECHO[0], CMD_MON[0]),
                        (DMOV_ECHO[0], DMOV[0]),
                        (CMD_ECHO[1], CMD_MON[1]),
                        (DMOV_ECHO[1], DMOV[1]),
                    ] {
                        let x = v.get(from);
                        v.set(to, x).await;
                    }
                    v.set(OP_ACK, 0.0).await;
                    ctl_debug!(
                        v,
                        "hrCtl:waitForCmndEnter: Moving={}; Done={}",
                        v.get(MOVING) as i32,
                        u8::from(self.hr_done)
                    );
                    if v.on(MOVING) && self.hr_done {
                        ctl_debug!(v, "hrCtl:waitForCmndEnter: asserting 'done'.");
                        self.hr_done = false;
                        v.set(MOVING, 0.0).await;
                    }
                    return Some(HrState::WaitForCmnd);
                }
                if !self.init_seq_done {
                    return Some(HrState::InitSequence);
                }
                None
            }
            HrState::WaitForCmnd => {
                if v.test_and_clear(HLM[0])
                    || v.test_and_clear(LLM[0])
                    || v.test_and_clear(HLM[1])
                    || v.test_and_clear(LLM[1])
                {
                    self.msg1("Motor Soft Limit Changed").await;
                    self.msg2(" ").await;
                    return Some(HrState::PhiLimits);
                }
                if [H1, K1, L1, A1, H2, K2, L2, A2]
                    .into_iter()
                    .any(|i| v.test_and_clear(i))
                {
                    self.msg1("Lattice Spacing Changed").await;
                    self.msg2(" ").await;
                    return Some(HrState::DInputChanged);
                }
                if v.test(E) {
                    ctl_debug!(v, "hrCtl:waitForCmd: E_mon");
                    v.clear(E);
                    return Some(HrState::EChanged);
                }
                if v.test(LAMBDA) {
                    ctl_debug!(v, "hrCtl:waitForCmd: lambda_mon");
                    v.clear(LAMBDA);
                    return Some(HrState::LChanged);
                }
                if v.test(THETA1) || v.test(THETA2) {
                    ctl_debug!(v, "hrCtl:waitForCmd: theta*_mon");
                    v.clear(THETA1);
                    v.clear(THETA2);
                    return Some(HrState::ThChanged);
                }
                if v.test(WORLD_OFF) {
                    ctl_debug!(v, "hrCtl:waitForCmd: worldOff_mon");
                    v.clear(WORLD_OFF);
                    return Some(HrState::CalcMovements);
                }
                if v.test(PHI1_OFF) || v.test(PHI2_OFF) {
                    ctl_debug!(v, "hrCtl:waitForCmd: phi*Off_mon");
                    v.clear(PHI1_OFF);
                    v.clear(PHI2_OFF);
                    self.calc_readback();
                    self.put_rdbk(false).await;
                    return Some(HrState::PhiLimits);
                }
                if v.on(PUT_VALS) && d.delay(0.1) {
                    ctl_debug!(v, "hrCtl:waitForCmnd:putVals");
                    return Some(HrState::CalcMovements);
                }
                if v.test(USE_SET) && v.on(USE_SET) {
                    v.clear(USE_SET);
                    for i in LIMITS {
                        v.set(i, 0.0).await;
                    }
                    return Some(HrState::EChanged);
                }
                if v.test(USE_SET) && !v.on(USE_SET) {
                    v.clear(USE_SET);
                    return Some(HrState::PhiLimits);
                }
                if v.test_and_clear(OP_MODE) {
                    ctl_debug!(v, "hrCtl:waitForCmnd: opMode event.");
                    v.set(AUTO_MODE, 0.0).await;
                    self.msg2("Set to Manual Mode").await;
                    return Some(HrState::CalcMovements);
                }
                if v.test_and_clear(GEOM) {
                    ctl_debug!(v, "hrCtl:waitForCmnd: Geom event.");
                    v.set(AUTO_MODE, 0.0).await;
                    self.msg1("New geometry.  Switch Phi 2 motor dir.").await;
                    self.msg2("and recalibrate monochromator.").await;
                    self.v.set(OP_ALERT, 1.0).await;
                    return Some(HrState::CalcMovements);
                }
                if [
                    E_INC, E_DEC, L_INC, L_DEC, TH1_INC, TH1_DEC, TH2_INC, TH2_DEC, WORLD_INC,
                    WORLD_DEC,
                ]
                .into_iter()
                .any(|i| v.on(i))
                {
                    return Some(HrState::Tweak);
                }
                if v.on(OP_ACK) {
                    v.set(OP_ALERT, 0.0).await;
                    self.msg1(" ").await;
                    self.msg2(" ").await;
                    return Some(HrState::WaitForCmndEnter);
                }
                if v.test(MOVING) {
                    v.clear(MOVING);
                    return Some(HrState::CheckAutoMode);
                }
                if (!v.on(DMOV[0]) || !v.on(DMOV[1])) && d.delay(0.1) {
                    return Some(HrState::UpdateRdbk);
                }
                None
            }
            HrState::CheckAutoMode => Some(if v.on(AUTO_MODE) {
                HrState::CheckDone
            } else {
                HrState::WaitForCmnd
            }),
            HrState::PhiLimits => {
                ctl_debug!(v, "hrCtl:phiLimits");
                if !v.on(USE_SET) {
                    self.phi_limits().await;
                } else {
                    for i in LIMITS {
                        v.set(i, 0.0).await;
                    }
                }
                Some(HrState::CheckDone)
            }
            HrState::CheckDone => {
                if self.put_complete == [true; 2] {
                    ctl_debug!(v, "hrCtl:checkDone: done");
                    self.hr_done = true;
                    return Some(HrState::WaitForCmndEnter);
                }
                ctl_debug!(v, "hrCtl:checkDone: not done");
                Some(HrState::UpdateRdbk)
            }
            HrState::DInputChanged => {
                v.set(AUTO_MODE, 0.0).await;
                self.msg2("Set to Manual Mode").await;
                for (xtal, [a, h, k, l, d]) in
                    [(1, [A1, H1, K1, L1, D1]), (2, [A2, H2, K2, L2, D2])]
                {
                    let x = self.calc_2d_spacing(xtal, a, h, k, l);
                    self.v.set(d, x).await;
                    let _ = self.seq_msg1.put_string_process(&self.msg1).await;
                }
                let v = &mut self.v;
                let (lambda_hi, lambda_lo) = if v.on(USE_SET) {
                    let d1 = v.get(D1);
                    (
                        d1 * (v.get(THETA1_HI) * D2R).sin(),
                        d1 * (v.get(THETA1_LO) * D2R).sin(),
                    )
                } else {
                    (0.0, 0.0)
                };
                let (e_hi, e_lo) = if v.on(USE_SET) {
                    (HC / lambda_lo, HC / lambda_hi)
                } else {
                    (0.0, 0.0)
                };
                v.set(LAMBDA_LO, lambda_lo).await;
                v.set(LAMBDA_HI, lambda_hi).await;
                v.set(E_HI, e_hi).await;
                v.set(E_LO, e_lo).await;
                v.put(OP_ALERT).await;
                Some(HrState::ThChanged)
            }
            HrState::Tweak => {
                let use_set = v.on(USE_SET);
                for (inc, dec, step, scale, var, lo, hi, msg, next) in [
                    (
                        E_INC,
                        E_DEC,
                        E_TWEAK,
                        1_000_000.0,
                        E,
                        E_LO,
                        E_HI,
                        "Tweaked E",
                        HrState::EChanged,
                    ),
                    (
                        L_INC,
                        L_DEC,
                        L_TWEAK,
                        100_000.0,
                        LAMBDA,
                        LAMBDA_LO,
                        LAMBDA_HI,
                        "Tweaked Lambda",
                        HrState::LChanged,
                    ),
                    (
                        TH1_INC,
                        TH1_DEC,
                        TH1_TWEAK,
                        1.0,
                        THETA1,
                        THETA1_LO,
                        THETA1_HI,
                        "Tweaked Theta 1",
                        HrState::ThChanged,
                    ),
                    (
                        TH2_INC,
                        TH2_DEC,
                        TH2_TWEAK,
                        1.0,
                        THETA2,
                        THETA2_LO,
                        THETA2_HI,
                        "Tweaked Theta 2",
                        HrState::ThChanged,
                    ),
                ] {
                    let v = &mut self.v;
                    if !(v.on(inc) || v.on(dec)) {
                        continue;
                    }
                    let up = v.on(inc);
                    let temp = if up {
                        v.get(var) + v.get(step) / scale
                    } else {
                        v.get(var) - v.get(step) / scale
                    };
                    if use_set || (temp >= v.get(lo) && temp <= v.get(hi)) {
                        v.set(var, temp).await;
                        self.msg1(msg).await;
                    } else {
                        self.msg1("Exceeds Limit").await;
                        self.v.set(OP_ALERT, 1.0).await;
                    }
                    self.v.set(if up { inc } else { dec }, 0.0).await;
                    return Some(next);
                }
                let v = &mut self.v;
                if v.on(WORLD_INC) || v.on(WORLD_DEC) {
                    let up = v.on(WORLD_INC);
                    let world = if up {
                        v.get(WORLD_OFF) + v.get(WORLD_TWEAK)
                    } else {
                        v.get(WORLD_OFF) - v.get(WORLD_TWEAK)
                    };
                    v.set(WORLD_OFF, world).await;
                    self.msg1("Tweaked world").await;
                    let i = if up { WORLD_INC } else { WORLD_DEC };
                    self.v.set(i, 0.0).await;
                    return Some(HrState::CalcMovements);
                }
                None
            }
            HrState::EChanged => {
                ctl_debug!(v, "hrCtl:eChanged");
                let lambda = HC / v.get(E);
                v.set(LAMBDA, lambda).await;
                Some(HrState::LChanged)
            }
            HrState::LChanged => {
                let op_mode = self.op_mode();
                let v = &mut self.v;
                let (lambda, d1, d2) = (v.get(LAMBDA), v.get(D1), v.get(D2));
                if lambda > d1 {
                    self.msg1("Wavelength > 2d spacing of crystal 1.").await;
                    self.v.set(OP_ALERT, 1.0).await;
                    return Some(HrState::ThChanged);
                }
                if lambda > d2 && op_mode != 0.0 {
                    self.msg1("Wavelength > 2d spacing of crystal 2.").await;
                    self.v.set(OP_ALERT, 1.0).await;
                    return Some(HrState::ThChanged);
                }
                // C prints "opMode 1" for opMode 2 too (hrCtl.st:1136).
                if op_mode == 0.0 {
                    ctl_debug!(v, "hrCtl:lChanged, opMode 0");
                } else if op_mode == 1.0 || op_mode == 2.0 {
                    ctl_debug!(v, "hrCtl:lChanged, opMode 1");
                }
                if op_mode == 0.0 || op_mode == 1.0 {
                    v.set(THETA1, (lambda / d1).asin() * R2D).await;
                }
                if op_mode == 1.0 || op_mode == 2.0 {
                    v.set(THETA2, (lambda / d2).asin() * R2D).await;
                }
                (op_mode == 0.0 || op_mode == 1.0 || op_mode == 2.0).then_some(HrState::ThChanged)
            }
            HrState::ThChanged => {
                self.th_changed().await;
                Some(HrState::CalcMovements)
            }
            HrState::CalcMovements => {
                let use_set = v.get(USE_SET);
                if use_set == 0.0 {
                    ctl_debug!(v, "hrCtl:calcMovements, USE");
                    self.put_desired().await;
                    return Some(HrState::MoveHr);
                }
                if use_set == 1.0 {
                    ctl_debug!(v, "hrCtl:calcMovements, SET");
                    let op_mode = self.op_mode();
                    let v = &mut self.v;
                    let off = v.get(PHI1) - v.get(RBV[0]) / D2UR;
                    v.set(PHI1_OFF, off).await;
                    if op_mode != 0.0 {
                        let off = v.get(PHI2) - v.get(RBV[1]) / D2UR;
                        v.set(PHI2_OFF, off).await;
                    }
                    self.put_desired().await;
                    self.v.set(PUT_VALS, 0.0).await;
                    self.calc_readback();
                    self.put_rdbk(false).await;
                    return Some(HrState::WaitForCmndEnter);
                }
                None
            }
            HrState::MoveHr => {
                if v.on(AUTO_MODE) || v.on(PUT_VALS) {
                    ctl_debug!(v, "hrCtl:moveHR: putting to motors");
                    let op_mode = self.op_mode();
                    let v = &mut self.v;
                    let x = v.get(DESIRED[0]);
                    v.assign(CMD[0], x);
                    if op_mode == 0.0 {
                        self.put_complete[0] = false;
                        v.put_async(CMD[0]);
                    } else if op_mode == 1.0 || op_mode == 2.0 {
                        let x = v.get(DESIRED[1]);
                        v.assign(CMD[1], x);
                        self.put_complete = [false; 2];
                        v.put_async(CMD[1]);
                        v.put_async(CMD[0]);
                    }
                    v.set(PUT_VALS, 0.0).await;
                    self.caused_move = true;
                    return Some(HrState::UpdateRdbk);
                }
                ctl_debug!(v, "hrCtl:moveHR: NOT putting to motors");
                Some(HrState::WaitForCmndEnter)
            }
            HrState::UpdateRdbk => {
                let (hls1, hls2) = (v.on(HLS[0]) || v.on(LLS[0]), v.on(HLS[1]) || v.on(LLS[1]));
                if hls1 || hls2 {
                    if hls1 {
                        self.msg1("Theta 1 motor hit a limit switch !").await;
                    }
                    if self.op_mode() != 0.0 && hls2 {
                        self.msg1("Theta 2 motor hit a limit switch !").await;
                    }
                    self.v.set(OP_ALERT, 1.0).await;
                    self.v.set(AUTO_MODE, 0.0).await;
                    self.msg2("Setting to Manual Mode").await;
                    return Some(HrState::StopHr);
                }
                if !v.on(DMOV[0]) || !v.on(DMOV[1]) {
                    ctl_debug!(v, "hrCtl:updateRdbk: a motor is moving");
                    v.set(MOVING, 1.0).await;
                    self.calc_readback();
                    self.put_rdbk(false).await;
                    return Some(HrState::UpdateRdbkAfterDelay);
                }
                if self.put_complete == [true; 2] {
                    ctl_debug!(self.v, "hrCtl:updateRdbk: motors stopped");
                    self.calc_readback();
                    self.put_rdbk(false).await;
                    self.hr_done = true;
                    return Some(HrState::MotorsStopped);
                }
                None
            }
            HrState::UpdateRdbkAfterDelay => d.delay(0.1).then_some(HrState::UpdateRdbk),
            HrState::MotorsStopped => {
                if self.caused_move {
                    ctl_debug!(v, "hrCtl:motorsStopped (we started them)");
                    self.caused_move = false;
                    return Some(HrState::WaitForCmndEnter);
                }
                ctl_debug!(v, "hrCtl:motorsStopped (we didn't start them)");
                if self.init_seq_done {
                    v.set(OP_ALERT, 1.0).await;
                    self.msg1("Somebody moved one of my motors.").await;
                    self.v.set(AUTO_MODE, 0.0).await;
                }
                self.calc_readback();
                self.put_rdbk(false).await;
                Some(HrState::WaitForCmndEnter)
            }
            HrState::ChkMotorLimits => {
                v.assign(OP_ALERT, 0.0);
                self.calc_readback();
                self.put_rdbk(true).await;
                let v = &mut self.v;
                let rbv = v.get(RBV[0]);
                if rbv < v.get(LLM[0]) || rbv > v.get(HLM[0]) {
                    self.msg1("Theta 1 motor is outside its limits").await;
                    self.v.assign(OP_ALERT, 1.0);
                }
                let op_mode = self.op_mode();
                let v = &mut self.v;
                let rbv = v.get(RBV[1]);
                if op_mode != 0.0 && (rbv < v.get(LLM[1]) || rbv > v.get(HLM[1])) {
                    self.msg1("Theta 2 motor is outside its limits").await;
                    self.v.assign(OP_ALERT, 1.0);
                }
                if self.v.on(OP_ALERT) {
                    self.v.put(OP_ALERT).await;
                    self.v.set(AUTO_MODE, 0.0).await;
                    self.msg2("Set to manual mode").await;
                }
                Some(HrState::ChkMotorLimitsExit)
            }
            HrState::ChkMotorLimitsExit => {
                if !v.on(OP_ALERT) {
                    v.put(OP_ALERT).await;
                    self.msg1("All motors within limits").await;
                    return Some(HrState::WaitForCmndEnter);
                }
                if d.delay(2.0) {
                    v.put(OP_ALERT).await;
                    self.msg2("Modify motor positions and/or limits").await;
                    return Some(HrState::ChkMotorLimits);
                }
                None
            }
            HrState::StopHr => {
                v.put(STOP[0]).await;
                if self.op_mode() != 0.0 {
                    self.v.put(STOP[1]).await;
                }
                Some(HrState::StoppedHrWait)
            }
            HrState::StoppedHrWait => d.delay(1.0).then_some(HrState::UpdateRdbk),
        }
    }

    /// hrCtl_calc2dSpacing (hrCtl.st:1546-1566), which leaves opAlert set
    /// for an invalid HKL and writes seqMsg1 without putting it.
    fn calc_2d_spacing(&mut self, xtal: u8, a: usize, h: usize, k: usize, l: usize) -> f64 {
        let v = &mut self.v;
        let (a, h, k, l) = (v.get(a), v.get(h), v.get(k), v.get(l));
        let result = (2.0 * a) / (h * h + k * k + l * l).sqrt();
        let mut alert = h % 2.0 != k % 2.0 || h % 2.0 != l % 2.0;
        let avg = ((h + k + l) / 2.0) as f32;
        let nint = if avg > 0.0 {
            (f64::from(avg) + 0.5) as i32
        } else {
            (f64::from(avg) - 0.5) as i32
        };
        if (avg - nint as f32).abs() <= 0.25 && nint % 2 != 0 {
            alert = true;
        }
        v.assign(OP_ALERT, f64::from(u8::from(alert)));
        if alert {
            self.msg1 = format!("Invalid HKL combination for crystal {xtal}");
        }
        result
    }

    /// hrCtl_calcReadback (hrCtl.st:1568-1616).
    fn calc_readback(&mut self) {
        let op_mode = self.op_mode();
        let nested = self.nested();
        let v = &mut self.v;
        let world = v.get(WORLD_OFF) * UR2D;
        let phi1 = v.get(RBV[0]) * UR2D + (v.get(PHI1_OFF) + world);
        v.assign(PHI1_RDBK, phi1);
        let theta1 = phi1;
        let (d1, d2) = (v.get(D1), v.get(D2));
        let phi2 = if nested {
            v.get(RBV[1]) * UR2D + (v.get(PHI2_OFF) + world)
        } else {
            v.get(RBV[1]) * UR2D + (v.get(PHI2_OFF) - world)
        };
        let lambda = if op_mode == 0.0 {
            Some(d1 * (theta1 * D2R).sin())
        } else if op_mode == 1.0 {
            v.assign(PHI2_RDBK, phi2);
            Some(d1 * (theta1 * D2R).sin())
        } else if op_mode == 2.0 {
            v.assign(PHI2_RDBK, phi2);
            let nom = (d1 * (theta1 * D2R).sin() / d2).asin() * R2D;
            let dev = if nested {
                phi2 - phi1 - theta1 - nom
            } else {
                phi2 - nom
            };
            Some(
                d1 * (theta1 * D2R).sin()
                    + D2R * dev
                        / (1.0 / (d1 * (theta1 * D2R).cos()) + 1.0 / (d2 * (nom * D2R).cos())),
            )
        } else {
            None
        };
        if let Some(lambda) = lambda {
            v.assign(LAMBDA_RDBK, lambda);
        }
        let lambda = v.get(LAMBDA_RDBK);
        v.assign(THETA1_RDBK, (lambda / d1).asin() * R2D);
        if op_mode != 0.0 {
            v.assign(THETA2_RDBK, (lambda / d2).asin() * R2D);
        }
        v.assign(E_RDBK, HC / lambda);
    }

    /// The readback puts; chkMotorLimits puts phi2's and theta2's in every
    /// mode, the other states only when `opMode != 0`.
    async fn put_rdbk(&mut self, both: bool) {
        let two = both || self.op_mode() != 0.0;
        let v = &mut self.v;
        v.put(PHI1_RDBK).await;
        v.put(THETA1_RDBK).await;
        if two {
            v.put(PHI2_RDBK).await;
            v.put(THETA2_RDBK).await;
        }
        v.put(LAMBDA_RDBK).await;
        v.put(E_RDBK).await;
    }

    /// phiLimits with `!useSetMode` (hrCtl.st:843-904).
    async fn phi_limits(&mut self) {
        let nested = self.nested();
        let v = &mut self.v;
        let world = v.get(WORLD_OFF) * UR2D;
        let (theta_min, theta_max) = (v.get(THETA_MIN), v.get(THETA_MAX));
        let phi1_off = v.get(PHI1_OFF);
        let phi1_hi = v.get(HLM[0]) * UR2D + (phi1_off + world);
        let phi1_lo = v.get(LLM[0]) * UR2D + (phi1_off + world);
        v.assign(PHI1_HI, phi1_hi);
        v.assign(PHI1_LO, phi1_lo);
        let (mut theta1_hi, mut theta1_lo) = (phi1_hi, phi1_lo);
        if theta1_hi > theta_max {
            theta1_hi = theta_max;
            self.msg2(&format!("Theta1HiLim clipped at {:.3}", CFloat(theta_max)))
                .await;
        }
        if theta1_lo < theta_min {
            theta1_lo = theta_min;
            self.msg2(&format!("Theta1LoLim clipped at {:.3}", CFloat(theta_min)))
                .await;
        }
        let v = &mut self.v;
        v.set(THETA1_HI, theta1_hi).await;
        v.set(THETA1_LO, theta1_lo).await;

        let phi2_off = v.get(PHI2_OFF);
        let (phi2_hi, phi2_lo) = if nested {
            (
                v.get(HLM[1]) * UR2D + phi2_off + world,
                v.get(LLM[1]) * UR2D + phi2_off + world,
            )
        } else {
            (
                v.get(HLM[1]) * UR2D + phi2_off - world,
                v.get(LLM[1]) * UR2D + phi2_off - world,
            )
        };
        v.assign(PHI2_HI, phi2_hi);
        v.assign(PHI2_LO, phi2_lo);
        let (mut theta2_hi, mut theta2_lo) = if nested {
            (phi2_hi - phi1_lo - theta1_lo, phi2_lo - phi1_hi - theta1_hi)
        } else {
            (phi2_hi, phi2_lo)
        };
        if theta2_hi > theta_max {
            theta2_hi = theta_max;
            self.msg2(&format!("Theta2HiLim clipped at {:.3}", CFloat(theta_max)))
                .await;
        }
        if theta2_lo < theta_min {
            theta2_lo = theta_min;
            self.msg2(&format!("Theta2LoLim clipped at {:.3}", CFloat(theta_min)))
                .await;
        }
        let v = &mut self.v;
        v.set(THETA2_HI, theta2_hi).await;
        v.set(THETA2_LO, theta2_lo).await;

        let (d1, d2) = (v.get(D1), v.get(D2));
        let lambda_hi = (d1 * (theta1_hi * D2R).sin()).min(d2 * (theta2_hi * D2R).sin());
        v.set(LAMBDA_HI, lambda_hi).await;
        let lambda_lo = (d1 * (theta1_lo * D2R).sin()).max(d2 * (theta2_lo * D2R).sin());
        v.set(LAMBDA_LO, lambda_lo).await;
        v.set(E_HI, HC / lambda_lo).await;
        v.set(E_LO, HC / lambda_hi).await;
    }

    /// thChanged's action (hrCtl.st:1147-1216).
    async fn th_changed(&mut self) {
        let op_mode = self.op_mode();
        let nested = self.nested();
        let v = &mut self.v;
        ctl_debug!(v, "hrCtl:thChanged");
        let use_set = v.on(USE_SET);
        let theta1 = v.get(THETA1);
        if !use_set && (theta1 <= v.get(THETA1_LO) || theta1 >= v.get(THETA1_HI)) {
            self.constrained("Theta 1 constrained to LIMIT").await;
        }
        let v = &mut self.v;
        let theta2 = v.get(THETA2);
        if !use_set && op_mode != 0.0 && (theta2 <= v.get(THETA2_LO) || theta2 >= v.get(THETA2_HI))
        {
            self.constrained("Theta 2 constrained to LIMIT").await;
        }
        let v = &mut self.v;
        let (theta1, theta2) = (v.get(THETA1), v.get(THETA2));
        let (d1, d2) = (v.get(D1), v.get(D2));
        if op_mode == 0.0 || op_mode == 1.0 {
            v.assign(LAMBDA, d1 * (theta1 * D2R).sin());
            v.assign(PHI1, theta1);
            if op_mode == 1.0 {
                v.assign(
                    PHI2,
                    if nested {
                        theta1 + theta1 + theta2
                    } else {
                        theta2
                    },
                );
                ctl_debug!(
                    v,
                    "hrCtl:thChanged: opMode={}, Geom={}, theta2={:.6}",
                    op_mode as i32,
                    v.get(GEOM) as i32,
                    CFloat(theta2)
                );
            }
        } else {
            let lambda_nom = d1 * (theta1 * D2R).sin();
            let nom = (lambda_nom / d2).asin() * R2D;
            let lambda = d2 * (theta2 * D2R).sin();
            v.assign(LAMBDA, lambda);
            v.assign(PHI1, theta1);
            let dev = R2D
                * (lambda - lambda_nom)
                * (1.0 / (d1 * (theta1 * D2R).cos()) + 1.0 / (d2 * (nom * D2R).cos()));
            v.assign(
                PHI2,
                if nested {
                    (theta1 + theta1 + nom) + dev
                } else {
                    nom + dev
                },
            );
        }
        v.put(PHI1).await;
        if op_mode != 0.0 {
            ctl_debug!(
                v,
                "hrCtl:thChanged: opMode={}, Geom={}, phi2={:.6}",
                op_mode as i32,
                v.get(GEOM) as i32,
                CFloat(v.get(PHI2))
            );
            v.put(PHI2).await;
        }
        v.put(LAMBDA).await;
        let e = HC / v.get(LAMBDA);
        v.set(E, e).await;
        self.calc_readback();
        self.put_rdbk(false).await;
    }

    /// thChanged's limit arm.
    async fn constrained(&mut self, msg: &str) {
        self.msg1(msg).await;
        self.v.set(OP_ALERT, 1.0).await;
        self.v.set(AUTO_MODE, 0.0).await;
        self.msg2("Set to Manual Mode").await;
    }

    /// Both calcMovements arms' phi*MotDesired puts.
    async fn put_desired(&mut self) {
        let op_mode = self.op_mode();
        let nested = self.nested();
        let v = &mut self.v;
        let world = v.get(WORLD_OFF) * UR2D;
        let x = (v.get(PHI1) - v.get(PHI1_OFF) - world) * D2UR;
        v.set(DESIRED[0], x).await;
        if op_mode != 0.0 {
            let x = if nested {
                (v.get(PHI2) - v.get(PHI2_OFF) - world) * D2UR
            } else {
                (v.get(PHI2) - v.get(PHI2_OFF) + world) * D2UR
            };
            v.set(DESIRED[1], x).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_pv() {
        let cfg = HrConfig::new("xxx:", "1", "m9", "m10");
        assert_eq!(cfg.pv("EAO"), "xxx:HR1_EAO");
        assert_eq!(cfg.pv_no_underscore("CtlDebug"), "xxx:HR1CtlDebug");
        assert_eq!(cfg.motor_pv("m9", ".RBV"), "xxx:m9.RBV");
    }
}
