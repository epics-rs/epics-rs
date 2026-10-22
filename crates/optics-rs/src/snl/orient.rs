//! 4-circle diffractometer orientation matrix state machine.
//!
//! Pure Rust port of `orient_st.st` — drives TTH, TH, CHI, PHI motors
//! based on HKL requests using orientation-matrix calculations from
//! [`crate::math::orient`].

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::snl::{PutQueue, SeqPv};
use epics_base_rs::types::EpicsValue;
use epics_libcom_rs::cfmt::CFloat;

use crate::db_access::{DbChannel, DbMultiMonitor, alloc_origin};
use crate::math::matrix3::{IDENTITY, Mat3, Vec3, invert};
use crate::math::orient::{
    Constraint, angles_to_hkl, calc_a0, calc_omtx, check_omtx, hkl_to_angles,
};
use epics_base_rs::runtime::task::sleep;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Energy-wavelength conversion constant: E(keV) = HC / lambda(Angstroms).
const HC: f64 = 12.3984244;

/// Threshold below which a value is treated as "effectively zero".
const SMALL: f64 = 1.0e-9;

/// Map the `Mode` mbbo wire value to a diffractometer [`Constraint`].
///
/// Mirrors the C `orient.h:27-29` enum / `orient.db` `Mode` mbbo menu:
/// `OMEGA_ZERO=0` ("TH=TTH/2"), `MIN_CHI_PHIm90=1` ("CHI~90; PHI~0"),
/// `PHI_CONST=2` ("PHI_CONST"). Any out-of-range value falls back to
/// `OMEGA_ZERO`, matching the C default branch.
fn constraint_from_mode(m: i32) -> Constraint {
    match m {
        1 => Constraint::MinChiPhiMinus90,
        2 => Constraint::PhiConst,
        _ => Constraint::OmegaZero,
    }
}

/// Configuration for the orient state machine (PV names from macros).
#[derive(Debug, Clone)]
pub struct OrientConfig {
    pub prefix: String,
    pub motor_prefix: String,
    pub motor_tth: String,
    pub motor_th: String,
    pub motor_chi: String,
    pub motor_phi: String,
}

impl OrientConfig {
    pub fn new(p: &str, pm: &str, m_tth: &str, m_th: &str, m_chi: &str, m_phi: &str) -> Self {
        Self {
            prefix: p.to_string(),
            motor_prefix: pm.to_string(),
            motor_tth: format!("{pm}{m_tth}"),
            motor_th: format!("{pm}{m_th}"),
            motor_chi: format!("{pm}{m_chi}"),
            motor_phi: format!("{pm}{m_phi}"),
        }
    }

    pub fn motor_tth_rbv(&self) -> String {
        let base = self.motor_tth.split('.').next().unwrap_or(&self.motor_tth);
        format!("{base}.RBV")
    }

    pub fn motor_th_rbv(&self) -> String {
        let base = self.motor_th.split('.').next().unwrap_or(&self.motor_th);
        format!("{base}.RBV")
    }

    pub fn motor_chi_rbv(&self) -> String {
        let base = self.motor_chi.split('.').next().unwrap_or(&self.motor_chi);
        format!("{base}.RBV")
    }

    pub fn motor_phi_rbv(&self) -> String {
        let base = self.motor_phi.split('.').next().unwrap_or(&self.motor_phi);
        format!("{base}.RBV")
    }
}

/// Async entry point — runs the orient state machine against live PVs.
///
/// ss orient, ss connections, ss readback and ss stop each run as a task.
pub async fn run(
    config: OrientConfig,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let motors = [
        &config.motor_tth,
        &config.motor_th,
        &config.motor_chi,
        &config.motor_phi,
    ]
    .map(|m| m.split('.').next().unwrap_or(m).to_string());
    let stop = stop_all(db.clone(), config.prefix.clone(), motors.clone());
    let conn = Arc::new(Connections::default());
    let (matrices, matrices_rx) = watch::channel(Matrices::default());
    let readback = readback(
        db.clone(),
        config.prefix.clone(),
        [
            config.motor_tth_rbv(),
            config.motor_th_rbv(),
            config.motor_chi_rbv(),
            config.motor_phi_rbv(),
        ],
        matrices_rx,
    );
    let check = check_connections(
        db.clone(),
        config.prefix.clone(),
        [
            config.motor_tth.clone(),
            config.motor_th.clone(),
            config.motor_chi.clone(),
            config.motor_phi.clone(),
        ],
        motors,
        conn.clone(),
    );
    tokio::select! {
        r = orient_ss(config, db, conn, matrices) => r,
        () = stop => Ok(()),
        () = check => Ok(()),
        () = readback => Ok(()),
    }
}

/// The ss orient variables ss readback reads: whether A0_state and
/// OMTX_state are both CALC_SUCCEEDED, and A0_i and OMTX_i.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Matrices {
    good: bool,
    a0_inv: Mat3,
    omtx_inv: Mat3,
}

impl Default for Matrices {
    fn default() -> Self {
        Self {
            good: false,
            a0_inv: IDENTITY,
            omtx_inv: IDENTITY,
        }
    }
}

/// ss readback (orient_st.st:1180-1212): its own state set, so H/K/L_RBV
/// follow the four motor readbacks, or read 0 while a matrix is not good,
/// whatever state ss orient is in.
async fn readback(
    db: PvDatabase,
    p: String,
    rbvs: [String; 4],
    mut matrices: watch::Receiver<Matrices>,
) {
    let origin = alloc_origin();
    let ch_rbvs = rbvs
        .each_ref()
        .map(|pv| DbChannel::with_origin(&db, pv, origin));
    let ch_hkl_rbv =
        ["H", "K", "L"].map(|f| DbChannel::with_origin(&db, &format!("{p}{f}_RBV"), origin));
    // motRBV_mon, synced to all four readbacks and raised by their connect
    // monitors.
    let mut monitor = DbMultiMonitor::new(&db, &rbvs).await;
    let mut raised = true;
    // H/K/L_RBV: assigned, not monitored, so only this ss writes them.
    let mut hkl_rbv = [0.0; 3];
    loop {
        // rbv_monitor
        let m = *matrices.borrow_and_update();
        if !m.good {
            for (v, ch) in hkl_rbv.iter_mut().zip(&ch_hkl_rbv) {
                if *v != 0.0 {
                    *v = 0.0;
                    let _ = ch.put_f64_process(0.0).await;
                }
            }
            // rbv_monitor_wait
            tokio::select! {
                _ = matrices.wait_for(|m| m.good) => {}
                () = sleep(Duration::from_secs(5)) => {}
            }
            continue;
        }
        if !raised {
            tokio::select! {
                _ = monitor.wait_changes() => {}
                _ = matrices.changed() => continue,
            }
        }
        raised = false;
        for pv in &rbvs {
            monitor.clear(pv);
        }
        let mut angles = [0.0; 4];
        for (a, ch) in angles.iter_mut().zip(&ch_rbvs) {
            *a = ch.get_f64().await;
        }
        // angles_to_HKL leaves zeros when it fails.
        hkl_rbv = angles_to_hkl(&angles, &m.omtx_inv, &m.a0_inv).unwrap_or([0.0; 3]);
        for (ch, v) in ch_hkl_rbv.iter().zip(hkl_rbv) {
            let _ = ch.put_f64_process(v).await;
        }
        // rbv_monitor_throttle
        sleep(Duration::from_millis(100)).await;
    }
}

/// motorsConnected and motorStopsConnected, set by ss connections and read
/// by ss orient.
#[derive(Default)]
struct Connections {
    motors: AtomicBool,
    stops: AtomicBool,
}

/// ss connections (orient_st.st:1148-1177). A local channel is connected
/// when the db holds its name.
async fn check_connections(
    db: PvDatabase,
    p: String,
    motors: [String; 4],
    bases: [String; 4],
    conn: Arc<Connections>,
) {
    let origin = alloc_origin();
    let ch_connected = ["TTH", "TH", "CHI", "PHI"]
        .map(|a| DbChannel::with_origin(&db, &format!("{p}mot{a}_Connected"), origin));
    let mut connected = [false; 4];
    loop {
        sleep(Duration::from_secs(1)).await;
        for i in 0..4 {
            let c = db.has_name(&motors[i]).await;
            if c != connected[i] {
                connected[i] = c;
                let _ = ch_connected[i].put_i16_process(i16::from(c)).await;
            }
        }
        conn.motors
            .store(connected.iter().all(|&c| c), Ordering::Relaxed);
        let mut stops = true;
        for b in &bases {
            stops &= db.has_name(format!("{b}.STOP")).await;
        }
        conn.stops.store(stops, Ordering::Relaxed);
    }
}

/// ss stop (orient_st.st:1215-1226): its own state set, so a stop is sent
/// whatever state ss orient is in.
async fn stop_all(db: PvDatabase, p: String, motors: [String; 4]) {
    let origin = alloc_origin();
    let pv_stop_motors = format!("{p}stopMotors");
    let ch_stop_motors = DbChannel::with_origin(&db, &pv_stop_motors, origin);
    let ch_msg = DbChannel::with_origin(&db, &format!("{p}Msg"), origin);
    let ch_debug = DbChannel::with_origin(&db, &format!("{p}Debug"), origin);
    let ch_stops = motors.map(|m| DbChannel::with_origin(&db, &format!("{m}.STOP"), origin));
    let mut monitor = DbMultiMonitor::new(&db, &[pv_stop_motors]).await;
    loop {
        if ch_stop_motors.get_f64().await != 0.0 {
            // PUTMSG
            let _ = ch_msg.put_string_process("stopping motors").await;
            if ch_debug.get_f64().await != 0.0 {
                println!("orient: stopping motors");
            }
            for ch in &ch_stops {
                let _ = ch.put_i16_process(1).await;
            }
            let _ = ch_stop_motors.put_i16_process(0).await;
        }
        monitor.wait_changes().await;
    }
}

/// A0_state and OMTX_state values (orient_st.st:333-335).
const NEEDED: i32 = 0;
const SUCCEEDED: i32 = 1;
const FAILED: i32 = 2;

/// Msg is a stringout (orient_st.st:85).
const MSG_SIZE: usize = 39;

/// SYNC_WAIT_SECS (orient_st.st:103).
const SYNC_WAIT: Duration = Duration::from_millis(20);

const BAD_OMTX: &str = "Bad OMTX calc; motPut_Auto set to Manual";

const HKL: [&str; 3] = ["H", "K", "L"];
const AXES: [&str; 4] = ["TTH", "TH", "CHI", "PHI"];
const FLAG_BUSY: [&str; 7] = [
    "H_busy", "K_busy", "L_busy", "TTH_busy", "TH_busy", "CHI_busy", "PHI_busy",
];

/// ss orient's sync'd event flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Flag {
    Lambda,
    Energy,
    XtalParm,
    A0,
    Omtx,
    Hkl(usize),
    Angle(usize),
    Mot(usize),
    Mode,
}

/// ss orient's states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum St {
    InitWait,
    Init,
    RefShow,
    EnterMonitorAll,
    MonitorAllDelay,
    MonitorAll,
    NewA0,
    NewOmtx,
    NewHkl,
    NewAngles,
    CheckDone,
    NewMotors,
    PutAll,
}

/// ss orient's variables. `vals` holds each by PV name: a monitored one
/// as its last monitor or ss orient's last put left it, an unmonitored one
/// as ss orient last put it. Monitors are taken only before a clause is
/// tested, so those an action's own puts post arrive after its efClears,
/// as they reach C's sequencer.
struct Orient {
    db: PvDatabase,
    origin: u64,
    p: String,
    mots: [String; 4],
    /// The motors' channels, with seq's put-callback slot each.
    queue: PutQueue,
    mot_pvs: [SeqPv; 4],
    monitor: DbMultiMonitor,
    flag_of: HashMap<String, Flag>,
    vals: HashMap<String, f64>,
    flags: HashSet<Flag>,
    conn: Arc<Connections>,
    matrices: watch::Sender<Matrices>,
    a0: Mat3,
    a0_i: Mat3,
    omtx: Mat3,
    omtx_i: Mat3,
    v2_hkl: Vec3,
    v2_angles: [f64; 4],
    startup: bool,
}

impl Orient {
    async fn new(
        config: OrientConfig,
        db: PvDatabase,
        conn: Arc<Connections>,
        matrices: watch::Sender<Matrices>,
    ) -> Self {
        let p = config.prefix;
        let pv = |f: &str| format!("{p}{f}");
        let mots = [
            config.motor_tth,
            config.motor_th,
            config.motor_chi,
            config.motor_phi,
        ];
        let origin = alloc_origin();
        let queue = PutQueue::new(&db, "orient");
        let mot_pvs: [SeqPv; 4] = std::array::from_fn(|i| {
            queue.pv_with_origin(&format!("mot{}", AXES[i]), &mots[i], origin)
        });
        let mut flag_of = HashMap::new();
        flag_of.insert(pv("lambda_event"), Flag::Lambda);
        flag_of.insert(pv("energy_event"), Flag::Energy);
        for f in ["a", "b", "c", "alpha", "beta", "gamma"] {
            flag_of.insert(pv(f), Flag::XtalParm);
        }
        for i in 1..=3 {
            for j in 1..=3 {
                flag_of.insert(pv(&format!("A0_{i}{j}")), Flag::A0);
                flag_of.insert(pv(&format!("OMTX_{i}{j}")), Flag::Omtx);
            }
        }
        for (i, f) in HKL.iter().enumerate() {
            flag_of.insert(pv(&format!("{f}_event")), Flag::Hkl(i));
        }
        for (i, a) in AXES.iter().enumerate() {
            flag_of.insert(pv(&format!("{a}_event")), Flag::Angle(i));
        }
        for (i, m) in mots.iter().enumerate() {
            flag_of.insert(m.clone(), Flag::Mot(i));
        }
        flag_of.insert(pv("Mode"), Flag::Mode);

        // Every variable ss orient monitors (orient_st.st:107-331).
        let mut monitored: Vec<String> = flag_of.keys().cloned().collect();
        for f in [
            "Debug",
            "OperAck",
            "motPut",
            "motPut_Auto",
            "motGet",
            "motGet_Auto",
            "OMTX_Calc",
            "Busy",
            "lambda",
            "energy",
            "errAngleThresh",
            "A0_state",
            "OMTX_state",
            "refGet1",
            "refPut1",
            "refGet2",
            "refPut2",
        ] {
            monitored.push(pv(f));
        }
        for f in HKL.iter().chain(&AXES).chain(&FLAG_BUSY) {
            monitored.push(pv(f));
        }
        for n in 1..=2 {
            for f in HKL.iter().chain(&AXES) {
                monitored.push(pv(&format!("{f}{n}")));
            }
        }
        for a in AXES {
            monitored.push(pv(&format!("waiting4mot{a}")));
            monitored.push(pv(&format!("weWrote_mot{a}")));
        }
        let monitor = DbMultiMonitor::new(&db, &monitored).await;
        let vals = monitored
            .iter()
            .map(|name| {
                let v = db.get_pv(name).ok().and_then(|v| v.to_f64()).unwrap_or(0.0);
                (name.clone(), v)
            })
            .collect();
        // Each sync'd flag is raised by its channel's first monitor.
        let mut flags = HashSet::new();
        for (name, &flag) in &flag_of {
            if db.has_name(name).await {
                flags.insert(flag);
            }
        }
        Self {
            db,
            origin,
            p,
            mots,
            queue,
            mot_pvs,
            monitor,
            flag_of,
            vals,
            flags,
            conn,
            matrices,
            a0: IDENTITY,
            a0_i: IDENTITY,
            omtx: IDENTITY,
            omtx_i: IDENTITY,
            v2_hkl: [0.0; 3],
            v2_angles: [0.0; 4],
            startup: false,
        }
    }

    fn pv(&self, f: &str) -> String {
        format!("{}{f}", self.p)
    }

    fn get(&self, f: &str) -> f64 {
        self.vals.get(&self.pv(f)).copied().unwrap_or(0.0)
    }

    fn is_set(&self, f: &str) -> bool {
        self.get(f) != 0.0
    }

    /// orientDebug.
    fn debug(&self) -> bool {
        self.is_set("Debug")
    }

    fn state(&self, f: &str) -> i32 {
        self.get(f) as i32
    }

    /// Assign a variable without putting it.
    fn set(&mut self, f: &str, v: f64) {
        let name = self.pv(f);
        self.vals.insert(name, v);
    }

    fn mot(&self, i: usize) -> f64 {
        self.vals.get(&self.mots[i]).copied().unwrap_or(0.0)
    }

    fn hkl(&self) -> Vec3 {
        HKL.map(|f| self.get(f))
    }

    fn angles(&self) -> [f64; 4] {
        AXES.map(|a| self.get(a))
    }

    fn set_hkl(&mut self, hkl: Vec3) {
        for (f, v) in HKL.iter().zip(hkl) {
            self.set(f, v);
        }
    }

    fn set_angles(&mut self, angles: [f64; 4]) {
        for (a, v) in AXES.iter().zip(angles) {
            self.set(a, v);
        }
    }

    fn reflection(&self, n: u8) -> (Vec3, [f64; 4]) {
        (
            HKL.map(|f| self.get(&format!("{f}{n}"))),
            AXES.map(|a| self.get(&format!("{a}{n}"))),
        )
    }

    fn test(&self, flag: Flag) -> bool {
        self.flags.contains(&flag)
    }

    fn clear(&mut self, flag: Flag) {
        self.flags.remove(&flag);
    }

    fn channel(&self, name: &str) -> DbChannel {
        DbChannel::with_origin(&self.db, name, self.origin)
    }

    async fn put_d(&mut self, f: &str, v: f64) {
        let name = self.pv(f);
        let _ = self.channel(&name).put_f64_process(v).await;
        self.vals.insert(name, v);
    }

    async fn put_s(&mut self, f: &str, v: i16) {
        let name = self.pv(f);
        let _ = self.channel(&name).put_i16_process(v).await;
        self.vals.insert(name, f64::from(v));
    }

    async fn put_l(&mut self, f: &str, v: i32) {
        let name = self.pv(f);
        let _ = self.channel(&name).put_i32_process(v).await;
        self.vals.insert(name, f64::from(v));
    }

    async fn put_str(&mut self, f: &str, v: &str) {
        let _ = self.channel(&self.pv(f)).put_string_process(v).await;
    }

    /// `motX = v; pvPut(motX, ASYNC)`, which seq refuses while the last
    /// one's callback is pending (seq_if.c:128-139).
    fn put_mot(&mut self, i: usize, v: f64) {
        self.vals.insert(self.mots[i].clone(), v);
        let _ = self.mot_pvs[i].put_async(EpicsValue::Double(v));
    }

    fn put_complete(&self, i: usize) -> bool {
        self.mot_pvs[i].put_complete()
    }

    /// A motor's put callback raises the flag motX is synced to
    /// (seq_ca.c:331-333).
    fn complete(&mut self, name: &str) {
        if let Some(i) = self.mots.iter().position(|m| m == name) {
            self.flags.insert(Flag::Mot(i));
        }
    }

    /// The matrix in the element variables `{name}_11`..`{name}_33`.
    fn matrix(&self, name: &str) -> Mat3 {
        std::array::from_fn(|i| {
            std::array::from_fn(|j| self.get(&format!("{name}_{}{}", i + 1, j + 1)))
        })
    }

    async fn put_matrix(&mut self, name: &str, m: Mat3) {
        for (i, row) in m.iter().enumerate() {
            for (j, &v) in row.iter().enumerate() {
                self.put_d(&format!("{name}_{}{}", i + 1, j + 1), v).await;
            }
        }
    }

    /// PUTMSG (orient_st.st:88).
    async fn putmsg(&mut self, text: &str) {
        let text = &text[..text.len().min(MSG_SIZE)];
        self.put_str("Msg", text).await;
        if self.debug() {
            println!("orient: {text}");
        }
    }

    /// PUTALERT (orient_st.st:89).
    async fn putalert(&mut self, text: &str) {
        let text = &text[..text.len().min(MSG_SIZE)];
        self.put_str("Msg", text).await;
        self.put_s("Alert", 1).await;
        println!("orient: {text}");
    }

    fn deliver(&mut self, name: String, v: f64) {
        if let Some(&flag) = self.flag_of.get(&name) {
            self.flags.insert(flag);
        }
        self.vals.insert(name, v);
    }

    /// Take the monitors that arrived, as they reach C's variables and
    /// flags before a clause is tested.
    fn poll(&mut self) {
        for (name, v) in self.monitor.try_changes() {
            self.deliver(name, v);
        }
        while let Some(name) = self.queue.try_completion() {
            self.complete(&name);
        }
        let now = Matrices {
            good: self.state("A0_state") == SUCCEEDED && self.state("OMTX_state") == SUCCEEDED,
            a0_inv: self.a0_i,
            omtx_inv: self.omtx_i,
        };
        self.matrices
            .send_if_modified(|m| std::mem::replace(m, now) != now);
    }

    /// Sleep until a monitor or a put callback arrives, or `deadline`
    /// passes.
    async fn wait(&mut self, deadline: Option<Instant>) {
        let until = async {
            match deadline {
                Some(d) => sleep(d.saturating_duration_since(Instant::now())).await,
                None => std::future::pending().await,
            }
        };
        let (changes, done) = tokio::select! {
            c = self.monitor.wait_changes() => (c, None),
            n = self.queue.completion() => (Vec::new(), Some(n)),
            () = until => (Vec::new(), None),
        };
        for (name, v) in changes {
            self.deliver(name, v);
        }
        if let Some(name) = done {
            self.complete(&name);
        }
        self.poll();
    }

    /// A state whose only clause is `when (delay(d))`.
    async fn delay(&mut self, d: Duration) {
        sleep(d).await;
        self.poll();
    }

    /// calc_A0, which leaves identity in A0 and A0_i when it fails.
    fn a0_calc(&mut self) -> bool {
        let [a, b, c, alpha, beta, gamma, lambda] =
            ["a", "b", "c", "alpha", "beta", "gamma", "lambda"].map(|f| self.get(f));
        let r = calc_a0(a, b, c, alpha, beta, gamma, lambda);
        (self.a0, self.a0_i) = r.unwrap_or((IDENTITY, IDENTITY));
        r.is_some()
    }

    /// calc_OMTX from the reflections, which become checkOMTX's v2_hkl and
    /// v2_angles; identity in OMTX and OMTX_i when it fails.
    fn omtx_calc(&mut self) -> bool {
        let (v1_hkl, v1_angles) = self.reflection(1);
        (self.v2_hkl, self.v2_angles) = self.reflection(2);
        let r = calc_omtx(
            &v1_hkl,
            &v1_angles,
            &self.v2_hkl,
            &self.v2_angles,
            &self.a0,
            &self.a0_i,
        );
        (self.omtx, self.omtx_i) = r.unwrap_or((IDENTITY, IDENTITY));
        r.is_some()
    }

    fn omtx_check(&self) -> f64 {
        check_omtx(
            &self.v2_hkl,
            &self.v2_angles,
            &self.a0,
            &self.a0_i,
            &self.omtx_i,
        )
    }

    /// Keep energy and wavelength synchronized (orient_st.st:462-470,
    /// :659-667).
    async fn sync_energy_lambda(&mut self) {
        if self.test(Flag::Lambda) {
            let energy = HC / self.get("lambda");
            self.set("energy", energy);
            self.put_d("load_energy", energy).await;
        } else if self.test(Flag::Energy) {
            let lambda = HC / self.get("energy");
            self.set("lambda", lambda);
            self.put_d("load_lambda", lambda).await;
        }
    }

    fn motors_done(&self) -> bool {
        self.is_set("Busy")
            && self.is_set("waiting4Mot")
            && AXES
                .iter()
                .all(|a| !self.is_set(&format!("waiting4mot{a}")))
    }

    /// The action of the all-motors-done clauses (orient_st.st:866-874,
    /// :1034-1042).
    async fn finish_motors(&mut self) {
        self.put_s("waiting4Mot", 0).await;
        self.put_s("Busy", 0).await;
        self.clear_flag_busy().await;
        if self.debug() {
            println!("motors done");
        }
    }

    async fn clear_flag_busy(&mut self) {
        for f in FLAG_BUSY {
            if self.is_set(f) {
                self.put_s(f, 0).await;
            }
        }
    }

    /// state init_wait (orient_st.st:364-370).
    async fn init_wait(&mut self) -> St {
        // :365
        self.delay(Duration::from_secs(1)).await;
        if self.state("A0_state") == FAILED {
            self.put_l("A0_state", NEEDED).await;
        }
        if self.state("OMTX_state") == FAILED {
            self.put_l("OMTX_state", NEEDED).await;
        }
        St::Init
    }

    /// state init (orient_st.st:377-571).
    async fn init(&mut self) -> St {
        loop {
            self.poll();
            // :378. Missing macros are refused before the program starts.
            if !self.startup {
                for (a, name) in AXES.iter().zip(self.mots.clone()) {
                    self.put_str(&format!("mot{a}_name"), &name).await;
                }
                self.startup = true;
                continue;
            }
            // :461
            if self.test(Flag::Lambda) || self.test(Flag::Energy) {
                self.sync_energy_lambda().await;
                self.clear(Flag::Lambda);
                self.clear(Flag::Energy);
                self.put_l("A0_state", NEEDED).await;
                continue;
            }
            // :475
            if self.state("A0_state") == NEEDED {
                let (lambda, energy) = (self.get("lambda"), self.get("energy"));
                if lambda > SMALL {
                    self.set("energy", HC / lambda);
                    self.put_d("load_energy", HC / lambda).await;
                    if self.debug() {
                        self.putmsg("calculated energy").await;
                    }
                } else if energy > SMALL {
                    self.set("lambda", HC / energy);
                    self.put_d("load_lambda", HC / energy).await;
                }
                let lattice =
                    ["a", "b", "c", "alpha", "beta", "gamma", "lambda"].map(|f| self.get(f));
                if lattice.iter().all(|&x| x != 0.0) {
                    self.clear(Flag::XtalParm);
                    self.clear(Flag::Lambda);
                    self.clear(Flag::Energy);
                    if self.debug() {
                        self.putmsg("calling calc_A0").await;
                    }
                    let ok = self.a0_calc();
                    self.put_matrix("A0", self.a0).await;
                    self.put_l("A0_state", if ok { SUCCEEDED } else { FAILED })
                        .await;
                    if ok {
                        self.putmsg("Successful A0 calc").await;
                    }
                } else {
                    self.put_l("A0_state", FAILED).await;
                }
                continue;
            }
            // :510
            if self.state("A0_state") == FAILED {
                return St::InitWait;
            }
            // :514
            if self.state("A0_state") == SUCCEEDED && self.state("OMTX_state") == NEEDED {
                let (hkl1, _) = self.reflection(1);
                let (hkl2, _) = self.reflection(2);
                let ok = if hkl1 != [0.0; 3] && hkl2 != [0.0; 3] {
                    if self.debug() {
                        self.putmsg("calling calc_OMTX").await;
                    }
                    let ok = self.omtx_calc();
                    self.put_matrix("OMTX", self.omtx).await;
                    if self.is_set("OMTX_Calc") {
                        self.put_s("OMTX_Calc", 0).await;
                    }
                    let err = self.omtx_check();
                    self.put_d("errAngle", err).await;
                    ok
                } else {
                    false
                };
                if ok && self.get("errAngle").abs() < self.get("errAngleThresh") {
                    self.put_l("OMTX_state", SUCCEEDED).await;
                    self.putmsg("Successful OMTX calc").await;
                } else {
                    self.put_l("OMTX_state", FAILED).await;
                    if self.is_set("motPut_Auto") {
                        self.put_s("motPut_Auto", 0).await;
                        self.putalert(BAD_OMTX).await;
                    }
                }
                continue;
            }
            // :564
            if self.state("OMTX_state") == FAILED {
                return St::InitWait;
            }
            // :568
            if self.state("A0_state") == SUCCEEDED && self.state("OMTX_state") == SUCCEEDED {
                return St::EnterMonitorAll;
            }
            self.wait(None).await;
        }
    }

    /// state ref_show (orient_st.st:578-590).
    async fn ref_show(&mut self) -> St {
        // :587
        self.delay(Duration::from_millis(500)).await;
        for f in ["refGet1", "refGet2", "refPut1", "refPut2"] {
            self.put_s(f, 0).await;
        }
        St::MonitorAll
    }

    /// state enter_monitor_all (orient_st.st:593-601).
    async fn enter_monitor_all(&mut self) -> St {
        // :594
        self.delay(Duration::from_secs(10)).await;
        for flag in [Flag::A0, Flag::Omtx, Flag::Mode]
            .into_iter()
            .chain((0..3).map(Flag::Hkl))
            .chain((0..4).map(Flag::Angle))
        {
            self.clear(flag);
        }
        self.putmsg("Initialization complete; going live").await;
        St::MonitorAll
    }

    /// state monitor_all_delay (orient_st.st:604-608).
    async fn monitor_all_delay(&mut self) -> St {
        // :606
        self.delay(Duration::from_secs(1)).await;
        St::MonitorAll
    }

    /// state monitor_all (orient_st.st:611-933).
    async fn monitor_all(&mut self) -> St {
        loop {
            self.poll();
            // :617
            if !self.conn.motors.load(Ordering::Relaxed) {
                self.putmsg("Waiting for motor connection(s)").await;
                return St::MonitorAllDelay;
            }
            // :621
            if !self.conn.stops.load(Ordering::Relaxed) {
                self.putmsg("Waiting for motor-stop connection(s)").await;
                return St::MonitorAllDelay;
            }
            // :626
            if self.state("A0_state") == SUCCEEDED && self.is_set("OMTX_Calc") {
                if self.debug() {
                    self.putmsg("Recalculating OMTX").await;
                }
                let ok = self.omtx_calc();
                self.put_matrix("OMTX", self.omtx).await;
                self.put_s("OMTX_Calc", 0).await;
                let err = self.omtx_check();
                self.put_d("errAngle", err).await;
                if ok && err.abs() < self.get("errAngleThresh") {
                    self.put_l("OMTX_state", SUCCEEDED).await;
                } else {
                    self.put_l("OMTX_state", FAILED).await;
                    self.put_s("motPut_Auto", 0).await;
                    self.putalert(BAD_OMTX).await;
                }
                return St::NewOmtx;
            }
            // :657
            if self.test(Flag::XtalParm) || self.test(Flag::Lambda) || self.test(Flag::Energy) {
                self.sync_energy_lambda().await;
                self.clear(Flag::XtalParm);
                self.clear(Flag::Lambda);
                self.clear(Flag::Energy);
                if self.debug() {
                    self.putmsg("new XTAL parameters").await;
                }
                let ok = self.a0_calc();
                self.put_matrix("A0", self.a0).await;
                self.put_l("A0_state", if ok { SUCCEEDED } else { FAILED })
                    .await;
                if ok {
                    self.putmsg("Successful A0 calc").await;
                } else {
                    self.putalert("A0 calc failed").await;
                }
                return St::NewHkl;
            }
            // :686. invertArray leaves A0_i as it was when it fails.
            if self.test(Flag::A0) {
                self.clear(Flag::A0);
                if self.debug() {
                    self.putmsg("new A0 precursor(s)").await;
                }
                self.a0 = self.matrix("A0");
                match invert(&self.a0) {
                    Some(a0_i) => self.a0_i = a0_i,
                    None => {
                        self.putmsg("Could not invert A0 array").await;
                        self.put_l("A0_state", FAILED).await;
                    }
                }
                return St::NewA0;
            }
            // :701
            if self.test(Flag::Omtx) {
                self.clear(Flag::Omtx);
                if self.debug() {
                    self.putmsg("new OMTX element(s)").await;
                }
                self.omtx = self.matrix("OMTX");
                if let Some(omtx_i) = invert(&self.omtx) {
                    self.omtx_i = omtx_i;
                    if self.state("A0_state") != SUCCEEDED {
                        self.putmsg("Can't check or use OMTX array (No A0)").await;
                        self.put_l("OMTX_state", FAILED).await;
                    } else {
                        let err = self.omtx_check();
                        self.put_d("errAngle", err).await;
                        if err.abs() < self.get("errAngleThresh") {
                            self.put_l("OMTX_state", SUCCEEDED).await;
                            if self.debug() {
                                self.putmsg("User's OMTX passes test").await;
                            }
                        } else {
                            self.put_l("OMTX_state", FAILED).await;
                            if self.debug() {
                                self.putmsg("User's OMTX fails test").await;
                            }
                        }
                    }
                } else {
                    self.putmsg("Could not invert OMTX array").await;
                    self.put_l("OMTX_state", FAILED).await;
                }
                return St::NewOmtx;
            }
            // :730, :736, :742
            for (i, f) in HKL.iter().enumerate() {
                if self.test(Flag::Hkl(i)) {
                    if self.debug() {
                        println!("\norient: client wrote {f}");
                    }
                    if !self.is_set("Busy") {
                        self.put_s("Busy", 1).await;
                    }
                    self.clear(Flag::Hkl(i));
                    return St::NewHkl;
                }
            }
            // :750, :756, :762, :768
            for (i, f) in AXES.iter().enumerate() {
                if self.test(Flag::Angle(i)) {
                    if self.debug() {
                        println!("\norient: client wrote {f}");
                    }
                    if !self.is_set("Busy") {
                        self.put_s("Busy", 1).await;
                    }
                    self.clear(Flag::Angle(i));
                    return St::NewAngles;
                }
            }
            // :801-861, three clauses per motor.
            for (i, a) in AXES.iter().enumerate() {
                let (waiting4, we_wrote) = (format!("waiting4mot{a}"), format!("weWrote_mot{a}"));
                if self.test(Flag::Mot(i)) && self.is_set(&waiting4) && self.put_complete(i) {
                    if self.debug() {
                        println!("orient: mot{a} done moving");
                    }
                    self.clear(Flag::Mot(i));
                    self.put_s(&waiting4, 0).await;
                    if self.is_set(&we_wrote) {
                        self.put_l(&we_wrote, 0).await;
                    }
                    return St::CheckDone;
                }
                if self.test(Flag::Mot(i)) && self.is_set(&we_wrote) {
                    if self.debug() {
                        println!("orient: We wrote mot{a}");
                    }
                    self.clear(Flag::Mot(i));
                    self.put_l(&we_wrote, 0).await;
                    return St::MonitorAll;
                }
                if self.test(Flag::Mot(i)) {
                    if self.debug() {
                        println!("orient: client wrote mot{a}");
                    }
                    self.clear(Flag::Mot(i));
                    return St::NewMotors;
                }
            }
            // :864
            if self.motors_done() {
                self.finish_motors().await;
                return St::MonitorAll;
            }
            // :879
            if self.test(Flag::Mode) {
                self.clear(Flag::Mode);
                if self.debug() {
                    self.putmsg("new HKL-to-Angles Mode").await;
                    println!("...Mode={}", self.get("Mode") as i16);
                }
                return St::NewHkl;
            }
            // :888
            if self.is_set("motPut") {
                if self.debug() {
                    println!("orient: motPut=={}", self.get("motPut") as i16);
                }
                for (i, a) in AXES.iter().enumerate() {
                    self.put_l(&format!("weWrote_mot{a}"), 1).await;
                    let angle = self.get(a);
                    self.put_s(&format!("waiting4mot{a}"), 1).await;
                    self.put_mot(i, angle);
                }
                self.set("motPut", 0.0);
                self.put_s("load_motPut", 0).await;
                self.put_s("waiting4Mot", 1).await;
                return St::MonitorAll;
            }
            // :903
            if self.is_set("motGet") {
                return St::NewMotors;
            }
            // :907, :912
            for n in [1, 2] {
                if self.is_set(&format!("refGet{n}")) {
                    for f in HKL.iter().chain(&AXES) {
                        self.put_d(&format!("{f}{n}"), self.get(f)).await;
                    }
                    return St::RefShow;
                }
            }
            // :918, :923
            for n in [1, 2] {
                if self.is_set(&format!("refPut{n}")) {
                    for f in HKL.iter().chain(&AXES) {
                        self.put_d(f, self.get(&format!("{f}{n}"))).await;
                    }
                    return St::RefShow;
                }
            }
            // :929
            if self.is_set("OperAck") {
                self.putmsg("").await;
                self.put_s("OperAck", 0).await;
                self.put_s("Alert", 0).await;
                return St::MonitorAll;
            }
            self.wait(None).await;
        }
    }

    /// state newA0 (orient_st.st:943-948).
    async fn new_a0(&mut self) -> St {
        self.poll();
        // :944
        if self.state("A0_state") != SUCCEEDED {
            return St::MonitorAll;
        }
        // :946
        St::NewHkl
    }

    /// state newOMTX (orient_st.st:954-961).
    async fn new_omtx(&mut self) -> St {
        self.poll();
        // :955
        if self.state("OMTX_state") == SUCCEEDED {
            return St::NewHkl;
        }
        // :957
        self.put_s("motPut_Auto", 0).await;
        self.putalert(BAD_OMTX).await;
        St::MonitorAll
    }

    /// The first clause of newHKL and of newAngles (orient_st.st:971-977,
    /// :1006-1012).
    async fn no_good_matrix(&mut self) -> bool {
        if self.state("A0_state") != SUCCEEDED {
            self.putalert("We don't have a good A0 matrix").await;
        } else if self.state("OMTX_state") != SUCCEEDED {
            self.putalert("We don't have a good OMTX matrix").await;
        } else {
            return false;
        }
        true
    }

    /// state newHKL (orient_st.st:970-1001).
    async fn new_hkl(&mut self) -> St {
        let deadline = Instant::now() + SYNC_WAIT;
        loop {
            self.poll();
            // :971
            if self.no_good_matrix().await {
                return St::MonitorAll;
            }
            // :978
            if Instant::now() >= deadline {
                for (i, f) in HKL.iter().enumerate() {
                    if self.test(Flag::Hkl(i)) {
                        self.clear(Flag::Hkl(i));
                        if self.debug() {
                            println!("orient:newHKL: client also wrote {f}");
                        }
                    }
                }
                let mut angles = self.angles();
                let mode = constraint_from_mode(self.get("Mode") as i32);
                if hkl_to_angles(&self.hkl(), &self.a0, &self.omtx, &mut angles, mode).is_some() {
                    self.set_angles(angles);
                    self.put_s("newAngles", 1).await;
                    if self.is_set("motPut_Auto") {
                        self.put_s("newMotors", 1).await;
                    }
                } else {
                    self.putalert("Bad HKL2angles calc.").await;
                }
                return St::PutAll;
            }
            self.wait(Some(deadline)).await;
        }
    }

    /// state newAngles (orient_st.st:1005-1028).
    async fn new_angles(&mut self) -> St {
        let deadline = Instant::now() + SYNC_WAIT;
        loop {
            self.poll();
            // :1006
            if self.no_good_matrix().await {
                return St::MonitorAll;
            }
            // :1013
            if Instant::now() >= deadline {
                for (i, a) in AXES.iter().enumerate() {
                    if self.test(Flag::Angle(i)) {
                        self.clear(Flag::Angle(i));
                        if self.debug() {
                            println!("orient:newAngles: client also wrote {a}");
                        }
                    }
                }
                // angles_to_HKL leaves zeros when it fails.
                let hkl =
                    angles_to_hkl(&self.angles(), &self.omtx_i, &self.a0_i).unwrap_or([0.0; 3]);
                self.set_hkl(hkl);
                self.put_s("newHKL", 1).await;
                if self.is_set("motPut_Auto") {
                    self.put_s("newMotors", 1).await;
                }
                return St::PutAll;
            }
            self.wait(Some(deadline)).await;
        }
    }

    /// state checkDone (orient_st.st:1030-1047).
    async fn check_done(&mut self) -> St {
        self.poll();
        // :1032
        if self.motors_done() {
            self.finish_motors().await;
            return St::NewMotors;
        }
        // :1045
        St::MonitorAll
    }

    /// state newMotors (orient_st.st:1050-1067).
    async fn new_motors(&mut self) -> St {
        self.poll();
        // :1051
        if self.debug() {
            println!("orient:newMotors: motGet={}", self.get("motGet") as i16);
        }
        if self.is_set("motGet") || (self.is_set("motGet_Auto") && !self.is_set("waiting4Mot")) {
            if self.debug() {
                println!("orient:newMotors: getting values");
            }
            let angles = [0, 1, 2, 3].map(|i| self.mot(i));
            self.set_angles(angles);
            self.put_s("newAngles", 1).await;
            let hkl = angles_to_hkl(&angles, &self.omtx_i, &self.a0_i).unwrap_or([0.0; 3]);
            self.set_hkl(hkl);
            self.put_s("newHKL", 1).await;
            if self.is_set("motGet") {
                self.put_s("motGet", 0).await;
            }
        }
        St::PutAll
    }

    /// state putAll (orient_st.st:1078-1144).
    async fn put_all(&mut self) -> St {
        self.poll();
        // :1079
        if self.is_set("newHKL") {
            if self.debug() {
                println!("orient:putAll: writing HKL");
            }
            for f in HKL {
                self.put_d(&format!("load{f}"), self.get(f)).await;
            }
            self.put_s("newHKL", 0).await;
        }
        if self.is_set("newAngles") {
            if self.debug() {
                println!("orient:putAll: writing Angles");
            }
            for a in AXES {
                self.put_d(&format!("load{a}"), self.get(a)).await;
            }
            self.put_s("newAngles", 0).await;
        }
        if self.is_set("newMotors") {
            if self.debug() {
                println!("orient:putAll: comparing new/old Motor values");
            }
            if self.conn.motors.load(Ordering::Relaxed) {
                if self.debug() {
                    let [h, k, l] = self.hkl();
                    println!(
                        "putAll:newMotors: H={:8.4}, K={:8.4}, L={:8.4}",
                        CFloat(h),
                        CFloat(k),
                        CFloat(l)
                    );
                }
                for (i, a) in AXES.iter().enumerate() {
                    let angle = self.get(a);
                    if (self.mot(i) - angle).abs() > SMALL {
                        if self.debug() {
                            let mot = format!("mot{a}");
                            println!(
                                "putAll:newMotors: {a:>3}={:8.4}, {mot:>6}={:8.4}",
                                CFloat(angle),
                                CFloat(self.mot(i))
                            );
                        }
                        self.put_l(&format!("weWrote_mot{a}"), 1).await;
                        self.put_s(&format!("waiting4mot{a}"), 1).await;
                        self.put_mot(i, angle);
                    }
                }
                if AXES.iter().any(|a| self.is_set(&format!("waiting4mot{a}"))) {
                    self.put_s("waiting4Mot", 1).await;
                    if !self.is_set("Busy") {
                        self.put_s("Busy", 1).await;
                    }
                }
            } else {
                self.putalert("Motors not connected").await;
            }
            self.put_s("newMotors", 0).await;
        }
        if !self.is_set("waiting4Mot") {
            if self.is_set("Busy") {
                self.put_s("Busy", 0).await;
            }
            self.clear_flag_busy().await;
            if self.debug() {
                println!("orient:putAll: all complete (no motors)");
            }
        }
        St::MonitorAll
    }
}

/// ss orient (orient_st.st:362-1146): each state a method, each `when`
/// clause in C's order under its C line.
async fn orient_ss(
    config: OrientConfig,
    db: PvDatabase,
    conn: Arc<Connections>,
    matrices: watch::Sender<Matrices>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut ss = Orient::new(config, db, conn, matrices).await;
    let mut st = St::InitWait;
    loop {
        st = match st {
            St::InitWait => ss.init_wait().await,
            St::Init => ss.init().await,
            St::RefShow => ss.ref_show().await,
            St::EnterMonitorAll => ss.enter_monitor_all().await,
            St::MonitorAllDelay => ss.monitor_all_delay().await,
            St::MonitorAll => ss.monitor_all().await,
            St::NewA0 => ss.new_a0().await,
            St::NewOmtx => ss.new_omtx().await,
            St::NewHkl => ss.new_hkl().await,
            St::NewAngles => ss.new_angles().await,
            St::CheckDone => ss.check_done().await,
            St::NewMotors => ss.new_motors().await,
            St::PutAll => ss.put_all().await,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constraint_from_mode() {
        // C orient.h:27-29 / orient.db Mode mbbo menu ordering.
        assert_eq!(constraint_from_mode(0), Constraint::OmegaZero);
        assert_eq!(constraint_from_mode(1), Constraint::MinChiPhiMinus90);
        assert_eq!(constraint_from_mode(2), Constraint::PhiConst);
        // Out-of-range falls back to the C default branch (OMEGA_ZERO).
        assert_eq!(constraint_from_mode(3), Constraint::OmegaZero);
        assert_eq!(constraint_from_mode(-1), Constraint::OmegaZero);
    }
}
