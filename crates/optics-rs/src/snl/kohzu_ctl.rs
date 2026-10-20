//! Kohzu double-crystal monochromator control -- native Rust port of `kohzuCtl.st`.
//!
//! Controls a Kohzu-style monochromator with up to 3 motors (Theta, Y, Z).
//! Provides energy/wavelength/theta conversions using Bragg's law:
//!   lambda = 2d * sin(theta), where d = a / sqrt(H^2 + K^2 + L^2)
//!   `E [keV] = 12.3984244 / lambda [Angstrom]`
//!
//! Supports Normal, Channel-Cut, Freeze-Z, and Freeze-Y operating modes.
//! Supports Auto and Manual modes, plus Set/Use calibration mode.

use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_libcom_rs::cfmt::CFloat;

use crate::db_access::alloc_origin;
use crate::snl::vars::{
    DOUBLE, DOUBLE_MON, DOUBLE_SYNC, Decl, Delays, EventFlag, SHORT, SHORT_MON, SHORT_SYNC, Vars,
    ctl_debug,
};
use epics_base_rs::server::snl::{PutQueue, SeqPv, StateSet};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Planck constant * speed of light in keV*Angstrom.
pub const HC: f64 = 12.3984244;

/// Degrees per radian, rounded as C's radConv (kohzuCtl.st:525).
pub const RAD_CONV: f64 = 57.2958;

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
// Pure physics functions
// ---------------------------------------------------------------------------

/// kohzuCtl_calc2dSpacing (kohzuCtl.st:1439-1460): the 2d-spacing from
/// lattice constant `a` and Miller indices (h,k,l), with opAlert and seqMsg1.
/// All-zero HKL divides by zero as C does.
pub fn calc_2d_spacing(a: f64, h: f64, k: f64, l: f64) -> (f64, bool, &'static str) {
    let two_d = (2.0 * a) / (h * h + k * k + l * l).sqrt();
    let forbidden = is_forbidden_reflection(h, k, l);
    let msg = if forbidden {
        "(H,K,L) is 'forbidden' reflection"
    } else {
        "New d spacing"
    };
    (two_d, forbidden, msg)
}

/// C's opAlert test: exact `fmod` parity, then `float avg` and `int nint`.
pub fn is_forbidden_reflection(h: f64, k: f64, l: f64) -> bool {
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
    alert
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

    fn program(self) -> Program {
        Program {
            name: "kohzuCtl",
            soft: false,
            p: self.prefix.clone(),
            prefix: self.prefix,
            motors: [self.m_theta, self.m_y, self.m_z],
            geom: self.geom,
        }
    }
}

// ---------------------------------------------------------------------------
// Async runner
// ---------------------------------------------------------------------------

/// kohzuCtl.st, or kohzuCtl_soft.st: the same program but for the
/// `{P}{MONO}` names, no `kMbusy`/busyChanged, no tweak, and the limit
/// rollback nested inside moveKohzu's move arm (kohzuCtl_soft.st:891-938).
pub(crate) struct Program {
    pub(crate) name: &'static str,
    pub(crate) soft: bool,
    /// `{P}`, or `{P}{MONO}` for the soft names.
    pub(crate) p: String,
    /// `{P}`, which the motor records and their names take.
    pub(crate) prefix: String,
    pub(crate) motors: [String; 3],
    pub(crate) geom: Geometry,
}

impl Program {
    fn pv(&self, hard: &str, soft: &str) -> String {
        format!("{}{}", self.p, if self.soft { soft } else { hard })
    }

    fn motor(&self, axis: usize, field: &str) -> String {
        format!("{}{}{}", self.prefix, self.motors[axis], field)
    }
}

// Indices into `Vars`, one per assigned SNL variable that is not a string.
const OP_ALERT: usize = 0;
const OP_ACK: usize = 1;
const PUT_VALS: usize = 2;
const AUTO_MODE: usize = 3;
const CC_MODE: usize = 4;
const MOVING: usize = 5;
const H: usize = 6;
const K: usize = 7;
const L: usize = 8;
const A: usize = 9;
const D: usize = 10;
const E: usize = 11;
const E_HI: usize = 12;
const E_LO: usize = 13;
const E_RDBK: usize = 14;
const LAMBDA: usize = 15;
const LAMBDA_HI: usize = 16;
const LAMBDA_LO: usize = 17;
const LAMBDA_RDBK: usize = 18;
const THETA: usize = 19;
const THETA_HI: usize = 20;
const THETA_LO: usize = 21;
const THETA_RDBK: usize = 22;
const CMD_ECHO: [usize; 3] = [23, 24, 25];
const RDBK_ECHO: [usize; 3] = [26, 27, 28];
const VEL_ECHO: [usize; 3] = [29, 30, 31];
const DMOV_ECHO: [usize; 3] = [32, 33, 34];
const STOP: [usize; 3] = [35, 36, 37];
const DMOV: [usize; 3] = [38, 39, 40];
const HLS: [usize; 3] = [41, 42, 43];
const LLS: [usize; 3] = [44, 45, 46];
const DESIRED: [usize; 3] = [47, 48, 49];
const Y_DESIRED_HI: usize = 50;
const Y_DESIRED_LO: usize = 51;
const HLM: [usize; 3] = [52, 53, 54];
const LLM: [usize; 3] = [55, 56, 57];
const CMD: [usize; 3] = [58, 59, 60];
const CMD_MON: [usize; 3] = [61, 62, 63];
const SPEED: [usize; 3] = [64, 65, 66];
const SPEED_CTRL: usize = 67;
const RBV: [usize; 3] = [68, 69, 70];
const USE_SET: usize = 71;
const Y_OFFSET: usize = 72;
const Y_OFFSET_HI: usize = 73;
const Y_OFFSET_LO: usize = 74;
const DEBUG: usize = 75;
/// The variables both programs declare; kohzuCtl.st adds the tweak ones.
const N_SOFT: usize = 76;
const E_TWEAK: usize = 76;
const E_INC: usize = 77;
const E_DEC: usize = 78;
const L_TWEAK: usize = 79;
const L_INC: usize = 80;
const L_DEC: usize = 81;
const TH_TWEAK: usize = 82;
const TH_INC: usize = 83;
const TH_DEC: usize = 84;
const N_VARS: usize = 85;

const TH: usize = 0;
const Y: usize = 1;
const Z: usize = 2;

/// Each variable's name, PV and declaration, by index (kohzuCtl.st:47-485).
fn table(prog: &Program) -> Vec<(&'static str, String, Decl)> {
    let pv = |hard: &str, soft: &str| prog.pv(hard, soft);
    let axes = |vars: [&'static str; 3], field: &str, decl: Decl| {
        [TH, Y, Z].map(|i| (vars[i], prog.motor(i, field), decl))
    };
    let echoes = |vars: [&'static str; 3], hard: &str, soft: &str, decl: Decl| {
        [(TH, "Theta"), (Y, "Y"), (Z, "Z")].map(|(i, axis)| {
            (
                vars[i],
                pv(&format!("Kohzu{axis}{hard}"), &format!("{axis}{soft}")),
                decl,
            )
        })
    };
    let mut t = vec![
        ("opAlert", pv("KohzuAlertBO", "Alert"), SHORT),
        ("opAck", pv("KohzuOperAckBO", "OperAck"), SHORT_MON),
        ("putVals", pv("KohzuPutBO", "Put"), SHORT_MON),
        ("autoMode", pv("KohzuModeBO", "Mode"), SHORT_MON),
        ("ccMode", pv("KohzuMode2MO", "Mode2"), SHORT_MON),
        // `sync kohzuMoving kMbusy` is kohzuCtl.st's alone (:90-91).
        (
            "kohzuMoving",
            pv("KohzuMoving", "Moving"),
            if prog.soft { SHORT_MON } else { SHORT_SYNC },
        ),
        ("H", pv("BraggHAO", "H"), DOUBLE_SYNC),
        ("K", pv("BraggKAO", "K"), DOUBLE_SYNC),
        ("L", pv("BraggLAO", "L"), DOUBLE_SYNC),
        ("A", pv("BraggAAO", "A"), DOUBLE_SYNC),
        ("D", pv("Bragg2dSpacingAO", "2dSpacing"), DOUBLE),
        ("E", pv("BraggEAO", "E"), DOUBLE_SYNC),
        ("EHi", pv("BraggEAO.DRVH", "E.HLM"), DOUBLE),
        ("ELo", pv("BraggEAO.DRVL", "E.LLM"), DOUBLE),
        ("ERdbk", pv("BraggERdbkAO", "ERdbk"), DOUBLE),
        ("lambda", pv("BraggLambdaAO", "Lambda"), DOUBLE_SYNC),
        ("lambdaHi", pv("BraggLambdaAO.DRVH", "Lambda.HLM"), DOUBLE),
        ("lambdaLo", pv("BraggLambdaAO.DRVL", "Lambda.LLM"), DOUBLE),
        ("lambdaRdbk", pv("BraggLambdaRdbkAO", "LambdaRdbk"), DOUBLE),
        ("theta", pv("BraggThetaAO", "Theta"), DOUBLE_SYNC),
        ("thetaHi", pv("BraggThetaAO.DRVH", "Theta.HLM"), DOUBLE),
        ("thetaLo", pv("BraggThetaAO.DRVL", "Theta.LLM"), DOUBLE),
        ("thetaRdbk", pv("BraggThetaRdbkAO", "ThetaRdbk"), DOUBLE),
    ];
    t.extend(echoes(
        ["thetaMotCmdEcho", "yMotCmdEcho", "zMotCmdEcho"],
        "CmdAO",
        "Cmd",
        DOUBLE,
    ));
    t.push((
        "thetaMotRdbkEcho",
        pv("KohzuThetaRdbkAI", "ThetaMotRdbk"),
        DOUBLE,
    ));
    t.push(("yMotRdbkEcho", pv("KohzuYRdbkAI", "YRdbk"), DOUBLE));
    t.push(("zMotRdbkEcho", pv("KohzuZRdbkAI", "ZRdbk"), DOUBLE));
    t.extend(echoes(
        ["thetaMotVelEcho", "yMotVelEcho", "zMotVelEcho"],
        "VelAI",
        "Vel",
        DOUBLE,
    ));
    t.extend(echoes(
        ["thetaDmovEcho", "yDmovEcho", "zDmovEcho"],
        "DmovBI",
        "Dmov",
        SHORT,
    ));
    t.extend(axes(["thetaMotStop", "yStop", "zStop"], ".STOP", SHORT));
    t.extend(axes(["thetaDmov", "yDmov", "zDmov"], ".DMOV", SHORT_SYNC));
    t.extend(axes(["thetaHls", "yHls", "zHls"], ".HLS", SHORT_MON));
    t.extend(axes(["thetaLls", "yLls", "zLls"], ".LLS", SHORT_MON));
    t.extend([
        (
            "thetaMotDesired",
            pv("KohzuThetaSetAO", "ThetaSet"),
            DOUBLE_MON,
        ),
        ("yMotDesired", pv("KohzuYSetAO", "YSet"), DOUBLE_MON),
        ("zMotDesired", pv("KohzuZSetAO", "ZSet"), DOUBLE_MON),
        ("yMotDesiredHi", pv("KohzuYSetAO.DRVH", "YSet.DRVH"), DOUBLE),
        ("yMotDesiredLo", pv("KohzuYSetAO.DRVL", "YSet.DRVL"), DOUBLE),
    ]);
    t.push(("thetaMotHiLim", prog.motor(TH, ".HLM"), DOUBLE_SYNC));
    t.extend(
        [(Y, "yMotHiLim"), (Z, "zMotHiLim")]
            .map(|(i, var)| (var, prog.motor(i, ".HLM"), DOUBLE_MON)),
    );
    t.push(("thetaMotLoLim", prog.motor(TH, ".LLM"), DOUBLE_SYNC));
    t.extend(
        [(Y, "yMotLoLim"), (Z, "zMotLoLim")]
            .map(|(i, var)| (var, prog.motor(i, ".LLM"), DOUBLE_MON)),
    );
    t.extend(axes(["thetaMotCmd", "yMotCmd", "zMotCmd"], "", DOUBLE));
    t.extend(axes(
        ["thetaMotCmdMon", "yMotCmdMon", "zMotCmdMon"],
        "",
        DOUBLE_SYNC,
    ));
    t.extend(axes(["thSpeed", "ySpeed", "zSpeed"], ".VELO", DOUBLE_SYNC));
    t.push(("speedControl", pv("KohzuSpeedCtrl", "SpeedCtrl"), SHORT_MON));
    t.extend(axes(
        ["thetaMotRdbk", "yMotRdbk", "zMotRdbk"],
        ".RBV",
        DOUBLE_SYNC,
    ));
    t.extend([
        ("useSetMode", pv("KohzuUseSetBO", "UseSet"), SHORT_MON),
        ("yOffset", pv("Kohzu_yOffsetAO", "yOffset"), DOUBLE_SYNC),
        (
            "yOffsetHi",
            pv("Kohzu_yOffsetAO.DRVH", "yOffset.DRVH"),
            DOUBLE,
        ),
        (
            "yOffsetLo",
            pv("Kohzu_yOffsetAO.DRVL", "yOffset.DRVL"),
            DOUBLE,
        ),
        ("kohzuCtlDebug", pv("KohzuCtlDebug", "CtlDebug"), SHORT_MON),
    ]);
    debug_assert_eq!(t.len(), N_SOFT);
    if !prog.soft {
        for (vars, (step, inc, dec)) in [
            (
                ["ETweakVal", "ETweakInc", "ETweakDec"],
                ("BraggETweakAI", "BraggEIncBO", "BraggEDecBO"),
            ),
            (
                ["LTweakVal", "LTweakInc", "LTweakDec"],
                ("BraggLambdaTweakAI", "BraggLambdaIncBO", "BraggLambdaDecBO"),
            ),
            (
                ["thTweakVal", "thTweakInc", "thTweakDec"],
                ("BraggThetaTweakAI", "BraggThetaIncBO", "BraggThetaDecBO"),
            ),
        ] {
            t.extend([
                (vars[0], pv(step, ""), DOUBLE_MON),
                (vars[1], pv(inc, ""), SHORT_MON),
                (vars[2], pv(dec, ""), SHORT_MON),
            ]);
        }
        debug_assert_eq!(t.len(), N_VARS);
    }
    t
}

/// Run the kohzuCtl, updatePsuedo and updateSet state sets.
pub async fn run(
    config: KohzuConfig,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_program(config.program(), db).await;
    Ok(())
}

pub(crate) async fn run_program(prog: Program, db: PvDatabase) {
    let queue = PutQueue::new(&db, prog.name);
    let pseudo = Arc::new(EventFlag::default());
    let main = Kohzu::new(&prog, &db, &queue.state_set(prog.name), pseudo.clone()).await;
    let pseudo_ss = queue.state_set(if prog.soft {
        "updatePsuedo_soft"
    } else {
        "updatePsuedo"
    });
    let update_set = update_set(
        db.clone(),
        queue.state_set("updateSet"),
        prog.pv("KohzuUseSetBO", "UseSet"),
        ["thetaMotSet", "yMotSet", "zMotSet"],
        [0, 1, 2].map(|i| prog.motor(i, ".SET")),
    );
    tokio::select! {
        () = main.run() => {}
        () = update_pseudo(&prog, &db, &pseudo_ss, &pseudo) => {}
        () = update_set => {}
    }
}

/// ss updatePsuedo (kohzuCtl.st:1312-1380). `pseudo` is
/// `thetaMotRdbkPseudo_mon`, which ss kohzuCtl sets.
async fn update_pseudo(prog: &Program, db: &PvDatabase, ss: &StateSet, pseudo: &EventFlag) {
    let watch = RBV.into_iter().chain(SPEED).chain(CMD_MON).chain(DMOV);
    let mut v = Vars::new(db, ss, &table(prog), alloc_origin(), watch).await;
    let echoes = [
        (RBV[Y], RDBK_ECHO[Y]),
        (RBV[Z], RDBK_ECHO[Z]),
        (SPEED[TH], VEL_ECHO[TH]),
        (SPEED[Y], VEL_ECHO[Y]),
        (SPEED[Z], VEL_ECHO[Z]),
        (CMD_MON[TH], CMD_ECHO[TH]),
        (CMD_MON[Y], CMD_ECHO[Y]),
        (CMD_MON[Z], CMD_ECHO[Z]),
        (DMOV[TH], DMOV_ECHO[TH]),
        (DMOV[Y], DMOV_ECHO[Y]),
        (DMOV[Z], DMOV_ECHO[Z]),
    ];
    v.flush().await;
    loop {
        let fired = if pseudo.test_and_clear() {
            Some((RBV[TH], RDBK_ECHO[TH]))
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
                () = pseudo.raised() => {}
            },
        }
    }
}

/// ss updateSet (kohzuCtl.st:1387-1430): the UseSet PV (variable
/// useSetMode) and the three motor .SET fields follow each other, starting
/// with the connect-time monitor of each.
pub(crate) async fn update_set(
    db: PvDatabase,
    ss: StateSet,
    use_set_pv: String,
    motor_set_vars: [&str; 3],
    motor_set_pvs: [String; 3],
) {
    const USE: usize = 0;
    let mut table = vec![("useSetMode", use_set_pv, SHORT_SYNC)];
    table.extend(
        motor_set_vars
            .into_iter()
            .zip(motor_set_pvs)
            .map(|(var, pv)| (var, pv, SHORT_SYNC)),
    );
    let mut v = Vars::new(&db, &ss, &table, alloc_origin(), 0..4).await;
    v.flush().await;
    loop {
        if v.test_and_clear(USE) {
            for i in 1..4 {
                let use_set = v.get(USE);
                if v.get(i) != use_set {
                    v.set(i, use_set).await;
                }
            }
            v.flush().await;
        } else if let Some(i) = (1..4).find(|&i| v.test_and_clear(i)) {
            let mot_set = v.get(i);
            if v.get(USE) != mot_set {
                v.set(USE, mot_set).await;
            }
            v.flush().await;
        } else {
            v.wait(None).await;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Init,
    InitSequence,
    WaitForCmndEnter,
    WaitForCmnd,
    Tweak,
    DInputChanged,
    ThetaLimits,
    BusyChanged,
    EChanged,
    LChanged,
    ThChanged,
    CalcMovements,
    MoveKohzu,
    CheckDone,
    UpdateRdbk,
    ThetaMotStopped,
    ChkMotorLimits,
    ChkMotorLimitsExit,
    StopKohzu,
    StoppedKohzuWait,
}

/// ss kohzuCtl and its program variables. `riskAverse` is C's constant 0.
struct Kohzu {
    soft: bool,
    geom: Geometry,
    v: Vars,
    seq_msg1: SeqPv,
    seq_msg2: SeqPv,
    mot_names: [(SeqPv, String); 3],
    pseudo: Arc<EventFlag>,
    kohzu_done: bool,
    caused_move: bool,
    init_seq: u8,
    init_seq_done: bool,
    will_violate_limit: bool,
    prev_e: f64,
    prev_theta: f64,
    prev_lambda: f64,
    prev_desired: [f64; 3],
    old_speed: [f64; 3],
    new_speed: [f64; 3],
}

impl Kohzu {
    async fn new(prog: &Program, db: &PvDatabase, ss: &StateSet, pseudo: Arc<EventFlag>) -> Self {
        let origin = alloc_origin();
        let ch = |var: &str, hard: &str, soft: &str| {
            ss.pv_with_origin(var, &prog.pv(hard, soft), origin)
        };
        let name = |var: &str, hard: &str, soft: &str, axis: usize| {
            (ch(var, hard, soft), prog.motor(axis, ""))
        };
        let table = table(prog);
        let n = table.len();
        Self {
            soft: prog.soft,
            geom: prog.geom,
            v: Vars::new(db, ss, &table, origin, 0..n).await,
            seq_msg1: ch("seqMsg1", "KohzuSeqMsg1SI", "SeqMsg1"),
            seq_msg2: ch("seqMsg2", "KohzuSeqMsg2SI", "SeqMsg2"),
            mot_names: [
                name("thetaMotName", "KohzuThetaPvSI", "ThetaPv", TH),
                name("yMotName", "KohzuYPvSI", "YPv", Y),
                name("zMotName", "KohzuZPvSI", "ZPv", Z),
            ],
            pseudo,
            kohzu_done: false,
            caused_move: false,
            init_seq: 0,
            init_seq_done: false,
            will_violate_limit: false,
            prev_e: 0.0,
            prev_theta: 0.0,
            prev_lambda: 0.0,
            prev_desired: [0.0; 3],
            old_speed: [0.0; 3],
            new_speed: [0.0; 3],
        }
    }

    async fn msg1(&self, s: &str) {
        let _ = self.seq_msg1.put_string_process(s).await;
    }

    async fn msg2(&self, s: &str) {
        let _ = self.seq_msg2.put_string_process(s).await;
    }

    /// `D*sin(theta/radConv)`.
    fn lambda_of(&mut self, theta: f64) -> f64 {
        theta_to_lambda(theta, self.v.get(D))
    }

    async fn run(mut self) {
        let mut state = State::Init;
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
            State::Init => {
                self.init_seq = 0;
                Some(State::InitSequence)
            }
            State::InitSequence => {
                if self.init_seq == 0 {
                    ctl_debug!(v, "kohzuCtl:initSequence:initSeq=0");
                    for i in STOP {
                        v.assign(i, 1.0);
                    }
                    v.set(PUT_VALS, 0.0).await;
                    v.set(AUTO_MODE, 0.0).await;
                    v.set(OP_ACK, 0.0).await;
                    if self.geom == Geometry::Standard {
                        v.set(Y_OFFSET_HI, 17.5 + 0.000001).await;
                        v.set(Y_OFFSET_LO, 17.5 - 0.000001).await;
                        v.set(Y_OFFSET, 17.5).await;
                        v.set(Y_DESIRED_HI, 0.0).await;
                        v.set(Y_DESIRED_LO, -35.0).await;
                    } else {
                        v.set(Y_DESIRED_HI, 60.0).await;
                        v.set(Y_DESIRED_LO, 0.0).await;
                    }
                    for (ch, name) in &self.mot_names {
                        let _ = ch.put_string_process(name).await;
                    }
                    self.init_seq = 1;
                    return Some(State::DInputChanged);
                }
                if self.init_seq == 1 {
                    ctl_debug!(v, "kohzuCtl:initSequence:initSeq=1");
                    self.init_seq = 2;
                    return Some(State::ChkMotorLimits);
                }
                if self.init_seq == 2 {
                    ctl_debug!(v, "kohzuCtl:initSequence:initSeq=2");
                    self.init_seq = 3;
                    return Some(State::UpdateRdbk);
                }
                if self.init_seq == 3 && d.delay(1.0) {
                    ctl_debug!(v, "kohzuCtl:initSequence:initSeq=3");
                    self.msg1("Kohzu Control Ready").await;
                    self.msg2(" ").await;
                    self.init_seq_done = true;
                    let v = &mut self.v;
                    let x = v.get(RBV[TH]);
                    v.set(THETA, x).await;
                    return Some(State::ThChanged);
                }
                None
            }
            State::WaitForCmndEnter => {
                if self.init_seq_done && d.delay(0.1) {
                    ctl_debug!(v, "kohzuCtl:waitForCmndEnter:initSeqDone");
                    for i in [H, K, L, A, E] {
                        v.clear(i);
                    }
                    if !self.soft {
                        v.clear(MOVING);
                    }
                    v.clear(LAMBDA);
                    v.clear(THETA);
                    for (to, from) in [
                        (RDBK_ECHO, RBV),
                        (VEL_ECHO, SPEED),
                        (CMD_ECHO, CMD_MON),
                        (DMOV_ECHO, DMOV),
                    ] {
                        for axis in [TH, Y, Z] {
                            let x = v.get(from[axis]);
                            v.set(to[axis], x).await;
                        }
                    }
                    if v.on(AUTO_MODE) || v.on(USE_SET) {
                        self.put_rdbk().await;
                    }
                    let v = &mut self.v;
                    v.set(OP_ACK, 0.0).await;
                    ctl_debug!(
                        v,
                        "kohzuCtl:waitForCmndEnter: kohzuMoving={}; kohzuDone={}",
                        v.get(MOVING) as i32,
                        u8::from(self.kohzu_done)
                    );
                    if v.on(MOVING) && self.kohzu_done {
                        ctl_debug!(v, "kohzuCtl:waitForCmndEnter: asserting 'done'.");
                        self.kohzu_done = false;
                        v.set(MOVING, 0.0).await;
                    }
                    self.prev_theta = v.get(THETA);
                    self.prev_e = v.get(E);
                    self.prev_lambda = v.get(LAMBDA);
                    self.prev_desired = DESIRED.map(|i| v.get(i));
                    self.will_violate_limit = false;
                    return Some(State::WaitForCmnd);
                }
                if !self.init_seq_done {
                    return Some(State::InitSequence);
                }
                None
            }
            State::WaitForCmnd => {
                if v.test(H) || v.test(K) || v.test(L) || v.test(A) {
                    ctl_debug!(v, "kohzuCtl:waitForCmnd:HKLA_mon");
                    self.msg1("Lattice Spacing Changed").await;
                    self.msg2(" ").await;
                    for i in [H, K, L, A] {
                        self.v.clear(i);
                    }
                    return Some(State::DInputChanged);
                }
                if v.test(HLM[TH]) || v.test(LLM[TH]) {
                    ctl_debug!(v, "kohzuCtl:waitForCmnd:thetaMotXxLim_mon");
                    self.msg1("Theta Motor Soft Limits Changed").await;
                    self.msg2(" ").await;
                    self.v.clear(HLM[TH]);
                    self.v.clear(LLM[TH]);
                    return Some(State::ThetaLimits);
                }
                if v.test_and_clear(RBV[TH]) {
                    ctl_debug!(v, "kohzuCtl:waitForCmnd:thetaMotRdbk_mon");
                    self.pseudo.set();
                    return Some(State::UpdateRdbk);
                }
                if v.test_and_clear(E) {
                    ctl_debug!(v, "kohzuCtl:waitForCmnd:E_mon");
                    return Some(State::EChanged);
                }
                if v.test_and_clear(LAMBDA) {
                    ctl_debug!(v, "kohzuCtl:waitForCmnd:lambda_mon");
                    return Some(State::LChanged);
                }
                if v.test_and_clear(THETA) {
                    ctl_debug!(v, "kohzuCtl:waitForCmnd:theta_mon");
                    return Some(State::ThChanged);
                }
                if !self.soft && v.test_and_clear(MOVING) {
                    ctl_debug!(v, "kohzuCtl:waitForCmnd:kMbusy");
                    return Some(State::BusyChanged);
                }
                if v.on(PUT_VALS) && d.delay(0.1) {
                    ctl_debug!(v, "kohzuCtl:waitForCmnd:putVals");
                    return Some(State::MoveKohzu);
                }
                if !self.soft
                    && [E_INC, E_DEC, L_INC, L_DEC, TH_INC, TH_DEC]
                        .into_iter()
                        .any(|i| v.on(i))
                {
                    ctl_debug!(v, "kohzuCtl:waitForCmnd:Tweak");
                    return Some(State::Tweak);
                }
                if v.test_and_clear(Y_OFFSET) {
                    ctl_debug!(v, "kohzuCtl:waitForCmnd:yOffset_mon");
                    v.set(AUTO_MODE, 0.0).await;
                    let msg = format!("y offset changed to {}", CFloat(v.get(Y_OFFSET)));
                    self.msg1(&msg).await;
                    self.msg2("Set to Manual Mode").await;
                    return Some(State::CalcMovements);
                }
                if v.on(OP_ACK) {
                    ctl_debug!(v, "kohzuCtl:waitForCmnd:opAck");
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
                        "kohzuCtl:tweak:E+",
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
                        "kohzuCtl:tweak:L",
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
                        "kohzuCtl:tweak:th",
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
                ctl_debug!(v, "kohzuCtl:dInputChanged");
                v.set(AUTO_MODE, 0.0).await;
                self.msg2("Set to Manual Mode").await;
                let v = &mut self.v;
                let (two_d, forbidden, msg) =
                    calc_2d_spacing(v.get(A), v.get(H), v.get(K), v.get(L));
                v.assign(OP_ALERT, f64::from(u8::from(forbidden)));
                v.set(D, two_d).await;
                self.msg1(msg).await;
                self.put_e_lambda_limits().await;
                self.v.put(OP_ALERT).await;
                Some(State::ThChanged)
            }
            State::ThetaLimits => {
                ctl_debug!(v, "kohzuCtl:thetaLimits");
                let x = v.get(HLM[TH]);
                v.assign(THETA_HI, x);
                let x = v.get(LLM[TH]);
                v.assign(THETA_LO, x);
                if v.get(THETA_HI) > 89.0 {
                    v.assign(THETA_HI, 89.0);
                    self.msg2("ThetaHiLim > 89 deg. (Using 89 deg.)").await;
                }
                if self.v.get(THETA_LO) < 1.0 {
                    self.v.assign(THETA_LO, 1.0);
                    self.msg2("ThetaLoLim < 1 deg. (Using 1 deg.)").await;
                }
                self.v.put(THETA_HI).await;
                self.v.put(THETA_LO).await;
                self.put_e_lambda_limits().await;
                Some(State::CheckDone)
            }
            // A setpoint put that writes the value already there posts no
            // monitor, but its FLNK still asserts the busy record
            // (kohzuCtl.st:861-864).
            State::BusyChanged => {
                let moving = v.get(MOVING) > 0.0;
                for (flag, line, next) in [
                    (E, "E changed", State::EChanged),
                    (LAMBDA, "lambda changed", State::LChanged),
                    (THETA, "theta changed", State::ThChanged),
                ] {
                    if moving && v.test(flag) {
                        // C's lines put the newline mid-string (kohzuCtl.st:867-882).
                        if v.on(DEBUG) {
                            print!("kohzuCtl:busyChanged\n to Moving and {line}");
                        }
                        v.clear(flag);
                        return Some(next);
                    }
                }
                if moving {
                    if v.on(DEBUG) {
                        print!("kohzuCtl:busyChanged\n to Moving but unclear why");
                    }
                    return Some(State::EChanged);
                }
                if v.get(MOVING) == 0.0 {
                    if v.on(DEBUG) {
                        print!("kohzuCtl:busyChanged\n to Done");
                    }
                    return Some(State::WaitForCmndEnter);
                }
                None
            }
            State::EChanged => {
                ctl_debug!(v, "kohzuCtl:eChanged");
                let x = HC / v.get(E);
                v.set(LAMBDA, x).await;
                Some(State::LChanged)
            }
            State::LChanged => {
                if v.get(LAMBDA) > v.get(D) {
                    ctl_debug!(v, "kohzuCtl:lChanged: lambda>D");
                    self.msg1("Wavelength > 2d spacing.").await;
                    self.v.set(OP_ALERT, 1.0).await;
                } else {
                    ctl_debug!(v, "kohzuCtl:lChanged");
                    let theta = RAD_CONV * (v.get(LAMBDA) / v.get(D)).asin();
                    v.set(THETA, theta).await;
                }
                Some(State::ThChanged)
            }
            State::ThChanged => {
                ctl_debug!(v, "kohzuCtl:thChanged");
                if (v.get(THETA) <= v.get(THETA_LO)) || (v.get(THETA) >= v.get(THETA_HI)) {
                    self.msg1("Theta constrained to LIMIT").await;
                    let v = &mut self.v;
                    v.set(OP_ALERT, 1.0).await;
                    if v.get(THETA) < v.get(THETA_LO) {
                        let x = v.get(THETA_LO);
                        v.assign(THETA, x);
                    }
                    if v.get(THETA) > v.get(THETA_HI) {
                        let x = v.get(THETA_HI);
                        v.assign(THETA, x);
                    }
                    self.will_violate_limit = true;
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
                ctl_debug!(v, "kohzuCtl:calcMovements");
                let theta = v.get(THETA);
                let y_off = v.get(Y_OFFSET);
                v.assign(DESIRED[TH], theta);
                v.assign(DESIRED[Y], calc_y_position(self.geom, theta, y_off));
                v.assign(DESIRED[Z], calc_z_position(self.geom, theta, y_off));
                for (axis, other_frozen, msg) in [
                    (Y, 3.0, "Y will exceed soft limits"),
                    (Z, 2.0, "Z will exceed soft limits"),
                ] {
                    let v = &mut self.v;
                    let cc = v.get(CC_MODE);
                    let x = v.get(DESIRED[axis]);
                    if cc != 1.0
                        && cc != other_frozen
                        && (x < v.get(LLM[axis]) || x > v.get(HLM[axis]))
                    {
                        self.msg1(msg).await;
                        self.v.set(OP_ALERT, 1.0).await;
                        self.will_violate_limit = true;
                    }
                }
                let v = &mut self.v;
                for i in DESIRED {
                    v.put(i).await;
                }
                self.old_speed = SPEED.map(|i| v.get(i));
                let dist = [TH, Y, Z].map(|axis| (v.get(DESIRED[axis]) - v.get(RBV[axis])).abs());
                let mut time = [TH, Y, Z].map(|axis| dist[axis] / v.get(SPEED[axis]));
                match v.get(CC_MODE) {
                    1.0 => {
                        time[Y] = 0.0;
                        time[Z] = 0.0;
                    }
                    2.0 => time[Z] = 0.0,
                    3.0 => time[Y] = 0.0,
                    _ => {}
                }
                // The longest move keeps its speed; the others take its time.
                let longest = if time[TH] >= time[Y] && time[TH] >= time[Z] {
                    TH
                } else if time[Y] >= time[TH] && time[Y] >= time[Z] {
                    Y
                } else {
                    Z
                };
                for axis in [TH, Y, Z] {
                    self.new_speed[axis] = if axis == longest {
                        v.get(SPEED[axis])
                    } else {
                        dist[axis] / time[longest]
                    };
                }
                Some(State::MoveKohzu)
            }
            State::MoveKohzu => {
                if !self.soft && self.will_violate_limit {
                    self.roll_back().await;
                    self.kohzu_done = true;
                    return Some(State::WaitForCmndEnter);
                }
                let v = &mut self.v;
                if v.on(AUTO_MODE) || v.on(PUT_VALS) || v.on(USE_SET) {
                    ctl_debug!(v, "kohzuCtl:moveKohzu:auto|put");
                    if self.will_violate_limit {
                        self.roll_back().await;
                    } else {
                        self.move_motors().await;
                    }
                    return Some(State::UpdateRdbk);
                }
                ctl_debug!(v, "kohzuCtl:moveKohzu");
                Some(State::WaitForCmndEnter)
            }
            State::CheckDone => {
                if DMOV.into_iter().all(|i| v.on(i)) {
                    ctl_debug!(v, "kohzuCtl:checkDone: done");
                    self.kohzu_done = true;
                    return Some(State::WaitForCmndEnter);
                }
                ctl_debug!(v, "kohzuCtl:checkDone: not done");
                Some(State::UpdateRdbk)
            }
            State::UpdateRdbk => {
                if self.will_violate_limit {
                    return Some(State::WaitForCmndEnter);
                }
                if v.get(RBV[TH]) == 0.0 {
                    ctl_debug!(v, "kohzuCtl:updateRdbk:thetaMotRdbk==0");
                    self.msg1("Theta Motor Readback is 0 !").await;
                    self.msg2("Monochromator stopped").await;
                    self.v.set(OP_ALERT, 1.0).await;
                    return Some(State::WaitForCmndEnter);
                }
                // A delay, as some readback monitors arrive after DMOV
                // (kohzuCtl.st:1114).
                let all_done = DMOV.into_iter().all(|i| v.on(i));
                if all_done && d.delay(0.01) {
                    ctl_debug!(
                        v,
                        "kohzuCtl:updateRdbk:*Dmov (done={},moving={})",
                        u8::from(self.kohzu_done),
                        v.get(MOVING) as i32
                    );
                    self.put_rdbk().await;
                    self.kohzu_done = true;
                    self.pseudo.set();
                    self.v.clear(RBV[TH]);
                    ctl_debug!(self.v, "kohzuCtl:updateRdbk: done");
                    return Some(State::ThetaMotStopped);
                }
                let v = &mut self.v;
                let cc = v.get(CC_MODE);
                for (axis, watched, name, msg) in [
                    (TH, true, "Th", "Theta Motor hit a limit switch!"),
                    (
                        Y,
                        cc != 1.0 && cc != 3.0,
                        "Y",
                        "Y Motor hit a limit switch!",
                    ),
                    (
                        Z,
                        cc != 1.0 && cc != 2.0,
                        "Z",
                        "Z Motor hit a limit switch!",
                    ),
                ] {
                    let v = &mut self.v;
                    if watched && (v.on(HLS[axis]) || v.on(LLS[axis])) {
                        ctl_debug!(v, "kohzuCtl:updateRdbk: {name} limit switch");
                        self.msg1(msg).await;
                        self.v.set(OP_ALERT, 1.0).await;
                        self.v.set(AUTO_MODE, 0.0).await;
                        self.msg2("Setting to Manual Mode").await;
                        return Some(State::StopKohzu);
                    }
                }
                if !all_done && d.delay(0.1) {
                    if self.v.get(DEBUG) > 1.0 {
                        println!("kohzuCtl:updateRdbk:moving");
                    }
                    self.put_rdbk().await;
                    return Some(State::UpdateRdbk);
                }
                None
            }
            State::ThetaMotStopped => {
                if self.caused_move {
                    ctl_debug!(
                        v,
                        "kohzuCtl:thetaMotStopped:causedMove (done={},moving={})",
                        u8::from(self.kohzu_done),
                        v.get(MOVING) as i32
                    );
                    if v.on(SPEED_CTRL) {
                        for axis in [TH, Y, Z] {
                            v.set(SPEED[axis], self.old_speed[axis]).await;
                        }
                    }
                    ctl_debug!(
                        v,
                        "kohzuCtl:thetaMotStopped: ySpeed = {:.6}",
                        CFloat(v.get(SPEED[Y]))
                    );
                    self.caused_move = false;
                    return Some(State::WaitForCmndEnter);
                }
                // Someone moved the theta motor directly.
                ctl_debug!(
                    v,
                    "kohzuCtl:thetaMotStopped:!causedMove (done={},moving={})",
                    u8::from(self.kohzu_done),
                    v.get(MOVING) as i32
                );
                if self.init_seq_done {
                    v.set(OP_ALERT, 1.0).await;
                    self.msg1("Only Theta Motor Moved").await;
                    self.msg2("Hit MOVE to Adjust Y & Z").await;
                    self.v.set(AUTO_MODE, 0.0).await;
                }
                let theta = self.v.get(RBV[TH]);
                self.v.set(THETA, theta).await;
                let lambda = self.lambda_of(theta);
                self.v.set(LAMBDA, lambda).await;
                self.v.set(E, HC / lambda).await;
                Some(State::ThChanged)
            }
            State::ChkMotorLimits => {
                ctl_debug!(v, "kohzuCtl:chkMotorLimits");
                v.assign(OP_ALERT, 0.0);
                self.put_rdbk().await;
                let cc = self.v.get(CC_MODE);
                for (axis, checked, name) in [
                    (Y, cc != 1.0 && cc != 3.0, "Y"),
                    (Z, cc != 1.0 && cc != 2.0, "Z"),
                    (TH, true, "Theta"),
                ] {
                    if !checked {
                        continue;
                    }
                    let v = &mut self.v;
                    let rbv = v.get(RBV[axis]);
                    if rbv == 0.0 {
                        self.msg1(&format!("{name} Readback of 0 not reasonable"))
                            .await;
                        self.v.assign(OP_ALERT, 1.0);
                    }
                    let v = &mut self.v;
                    if (rbv < v.get(LLM[axis])) || (rbv > v.get(HLM[axis])) {
                        self.msg1(&format!("{name} Readback outside Motor Limits"))
                            .await;
                        self.v.assign(OP_ALERT, 1.0);
                    }
                }
                if self.v.on(OP_ALERT) {
                    self.v.put(OP_ALERT).await;
                    self.v.set(AUTO_MODE, 0.0).await;
                    self.msg2("Set to Manual Mode").await;
                }
                Some(State::ChkMotorLimitsExit)
            }
            State::ChkMotorLimitsExit => {
                if !v.on(OP_ALERT) {
                    ctl_debug!(v, "kohzuCtl:chkMotorLimitsExit: OK");
                    v.put(OP_ALERT).await;
                    self.msg1("All motors within limits").await;
                    return Some(State::WaitForCmndEnter);
                }
                if d.delay(2.0) {
                    ctl_debug!(v, "kohzuCtl:chkMotorLimitsExit: opAlert");
                    v.put(OP_ALERT).await;
                    self.msg2("Modify Motor Positions/Limits").await;
                    return Some(State::ChkMotorLimits);
                }
                None
            }
            State::StopKohzu => {
                ctl_debug!(v, "kohzuCtl:stopKohzu");
                for i in STOP {
                    v.put(i).await;
                }
                Some(State::StoppedKohzuWait)
            }
            State::StoppedKohzuWait => {
                if d.delay(1.0) {
                    ctl_debug!(v, "kohzuCtl:stoppedKohzuWait");
                    return Some(State::UpdateRdbk);
                }
                None
            }
        }
    }

    /// moveKohzu's `willViolateLimit` arm: the command's E, lambda, theta
    /// and motor setpoints back to what waitForCmndEnter saved.
    async fn roll_back(&mut self) {
        let v = &mut self.v;
        v.set(E, self.prev_e).await;
        v.set(THETA, self.prev_theta).await;
        v.set(LAMBDA, self.prev_lambda).await;
        for axis in [TH, Y, Z] {
            v.set(DESIRED[axis], self.prev_desired[axis]).await;
        }
        self.msg2("Command ignored").await;
    }

    /// moveKohzu's move arm: a frozen motor gets neither speed nor move.
    async fn move_motors(&mut self) {
        let v = &mut self.v;
        let cc = v.get(CC_MODE);
        if v.on(SPEED_CTRL) {
            for axis in [TH, Y, Z] {
                v.assign(SPEED[axis], self.new_speed[axis]);
            }
            v.put(SPEED[TH]).await;
            if cc == 0.0 || cc == 2.0 {
                v.put(SPEED[Y]).await;
            }
            if cc == 0.0 || cc == 3.0 {
                v.put(SPEED[Z]).await;
            }
        }
        for axis in [TH, Y, Z] {
            let x = v.get(DESIRED[axis]);
            v.assign(CMD[axis], x);
        }
        v.put(CMD[TH]).await;
        if cc == 0.0 || cc == 2.0 {
            v.put(CMD[Y]).await;
        }
        if cc == 0.0 || cc == 3.0 {
            v.put(CMD[Z]).await;
        }
        v.set(PUT_VALS, 0.0).await;
        self.caused_move = true;
    }

    /// `thetaRdbk = thetaMotRdbk; lambdaRdbk = ...; ERdbk = ...` and their puts.
    async fn put_rdbk(&mut self) {
        let theta = self.v.get(RBV[TH]);
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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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

    /// kohzuCtl_calc2dSpacing (kohzuCtl.st:1439-1460): exact fmod parity,
    /// so -1 and 1 differ and 1.005 is not odd, and all-zero HKL is 2A/0
    /// with no alert.
    #[test]
    fn calc_2d_spacing_follows_c_arithmetic() {
        assert!(is_forbidden_reflection(-1.0, 1.0, 1.0));
        assert!(is_forbidden_reflection(1.005, 1.0, 1.0));
        assert!(is_forbidden_reflection(0.5, 0.5, 0.5));
        assert!(!is_forbidden_reflection(-2.0, -2.0, 0.0));
        assert_eq!(
            calc_2d_spacing(5.4309, 0.0, 0.0, 0.0),
            (f64::INFINITY, false, "New d spacing")
        );
        let (two_d, alert, _) = calc_2d_spacing(0.0, 0.0, 0.0, 0.0);
        assert!(two_d.is_nan() && !alert);
    }

    /// C's radConv is 57.2958 (kohzuCtl.st:525), not 180/pi.
    #[test]
    fn theta_converts_with_c_rad_conv() {
        assert_eq!(theta_to_lambda(30.0, 6.0), 6.0 * (30.0f64 / 57.2958).sin());
        let y = calc_y_position(Geometry::Standard, 30.0, 17.5);
        assert_eq!(y, -17.5 / (30.0f64 / 57.2958).cos());
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
}
