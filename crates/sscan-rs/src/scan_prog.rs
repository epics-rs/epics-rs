//! The scan-progress sequence — port of `scanProg.st`.
//!
//! Two state sets, as in C: `scanProgress` turns the BUSY and CPT of up to
//! four nested sscan records into a progress bar, an estimated end time and
//! a hang detector; `scanPauseUpdate` keeps `{P}paused` true only while every
//! running scan is actually stopped. They share `scanDim` and `paused`, the
//! two C globals one writes and the other reads.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use chrono::{Local, TimeZone};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::db_access::DbSubscription;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::snl::{PutQueue, SeqPv, StateSet};
use epics_base_rs::types::EpicsValue;

const PROG_BAR_VERSION: f64 = 2.3;
/// Seconds between progress updates.
const UPDATE_DELAY: f64 = 2.0;
/// The pause state set re-checks this often regardless.
const UPDATE_DELAY_LONG: f64 = 20.0;

/// `scanProgress`'s macros: `P` prefixes the progress PVs, `S` the sscan
/// records `{S}scan1`..`{S}scan4`, and `name` is the `name` macro, printed
/// in the debug header.
pub struct ScanProgressConfig {
    pub p: String,
    pub s: String,
    pub name: Option<String>,
}

impl ScanProgressConfig {
    pub fn new(p: &str, s: &str) -> Self {
        Self {
            p: p.into(),
            s: s.into(),
            name: None,
        }
    }

    fn pv(&self, name: &str) -> String {
        format!("{}{name}", self.p)
    }

    fn scan(&self, n: usize, field: &str) -> String {
        format!("{}scan{}.{field}", self.s, n + 1)
    }
}

/// `clocks2str`: seconds as `hh:mm:ss`, `infinite` past a year and nine days.
pub fn clocks2str(isec: i64) -> String {
    if isec > 32_000_000 {
        return "infinite".into();
    }
    if isec < -32_000_000 {
        return "-infinite".into();
    }
    let (sign, isec) = if isec < 0 { (-1, -isec) } else { (1, isec) };
    let ss = isec % 60;
    let isec = isec / 60;
    let mm = isec % 60;
    let hh = sign * isec / 60;
    format!("{hh:02}:{mm:02}:{ss:02}")
}

/// `calc_cpt`: points done across the `scan_dim` nested scans, each pass of
/// an outer scan worth every point of the scans inside it.
pub fn calc_cpt(scan_dim: i32, cpt: [i32; 4], npts: [i32; 4]) -> i64 {
    let mut lcpt = i64::from(cpt[0]);
    let mut below = 1i64;
    for d in 1..scan_dim.clamp(1, 4) as usize {
        below = below.wrapping_mul(i64::from(npts[d - 1]));
        lcpt = lcpt.wrapping_add(i64::from(cpt[d]).wrapping_mul(below));
    }
    lcpt
}

/// `allStopped`: every scan up to `scan_dim` is either paused or not busy.
pub fn all_stopped(scan_dim: i32, pause: [i16; 4], busy: [i16; 4]) -> bool {
    (0..4).all(|d| d as i32 >= scan_dim.max(1) || pause[d] != 0 || busy[d] == 0)
}

/// `formatTimeStr`: `final` (epoch seconds) as local time, named Today,
/// Tomorrow or Yesterday against `now` unless `full`.
pub fn format_time_str(final_t: i64, full: bool, now: i64) -> String {
    let Some(at) = Local.timestamp_opt(final_t, 0).earliest() else {
        return String::new();
    };
    let fmt = match Local.timestamp_opt(now, 0).earliest() {
        Some(today) if !full => {
            let (day, today) = (at.date_naive(), today.date_naive());
            if day == today {
                "%H:%M:%S Today (%A)"
            } else if today.succ_opt() == Some(day) {
                "%H:%M:%S Tomorrow (%A)"
            } else if today.pred_opt() == Some(day) {
                "%H:%M:%S Yesterday (%A)"
            } else {
                "%H:%M:%S  (%a) %b %d, %Y"
            }
        }
        _ => "%H:%M:%S  (%a) %b %d, %Y",
    };
    // strftime into the 40-byte SNL string.
    let mut s = at.format(fmt).to_string();
    s.truncate(39);
    s
}

/// `time(NULL)`.
fn time_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// C's double to int conversion as x86 `cvttsd2si` performs it: out of range
/// and NaN give `INT_MIN`.
fn c_int(v: f64) -> i32 {
    if v.is_nan() || v >= 2_147_483_648.0 || v < -2_147_483_648.0 {
        i32::MIN
    } else {
        v as i32
    }
}

/// printf's `%g`: six significant digits, trailing zeros dropped, exponent
/// form below 1e-4 or from 1e6.
fn fmt_g(v: f64) -> String {
    if v.is_nan() {
        return if v.is_sign_negative() { "-nan" } else { "nan" }.into();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-inf" } else { "inf" }.into();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let sci = format!("{v:.5e}");
    let (mantissa, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let trim = |s: &str| {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s.to_string()
        }
    };
    if !(-4..6).contains(&exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", trim(mantissa), exp.abs())
    } else {
        trim(&format!("{v:.*}", (5 - exp) as usize))
    }
}

/// The variables assigned with `EvFlag`: monitored, each `sync`ed to its
/// own flag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mon {
    Debug,
    HangWaitSec,
    Pause(usize),
    Busy(usize),
}

impl Mon {
    const ALL: [Mon; 10] = [
        Mon::Debug,
        Mon::HangWaitSec,
        Mon::Pause(0),
        Mon::Pause(1),
        Mon::Pause(2),
        Mon::Pause(3),
        Mon::Busy(0),
        Mon::Busy(1),
        Mon::Busy(2),
        Mon::Busy(3),
    ];

    fn index(self) -> usize {
        match self {
            Mon::Debug => 0,
            Mon::HangWaitSec => 1,
            Mon::Pause(n) => 2 + n,
            Mon::Busy(n) => 6 + n,
        }
    }

    fn pv(self, cfg: &ScanProgressConfig) -> String {
        match self {
            Mon::Debug => cfg.pv("debug"),
            Mon::HangWaitSec => cfg.pv("hangWaitSec"),
            Mon::Pause(n) => cfg.scan(n, "PAUS"),
            Mon::Busy(n) => cfg.scan(n, "BUSY"),
        }
    }
}

/// One state set's copies of the monitored variables and their event flags,
/// kept current from its own subscriptions. Both state sets see every post,
/// as both read the one C variable; each owns the flags it tests.
struct Monitored {
    debug: i16,
    hang_wait_sec: i32,
    pause: [i16; 4],
    busy: [i16; 4],
    flags: [bool; 10],
    subs: Vec<(Mon, DbSubscription)>,
    /// `SNLtaskName`: the `name` macro, which seq does not default, so
    /// glibc prints a missing one as `(null)`.
    name: Arc<str>,
}

impl Monitored {
    /// Subscribe to every monitored PV with seq's `DBE_VALUE | DBE_ALARM`.
    /// Under the default `+c` seq starts no state set before every channel
    /// has connected and posted once (`seq_task.c:61`), and that first post
    /// raises each synced flag.
    async fn connect(db: &PvDatabase, cfg: &ScanProgressConfig) -> Self {
        let mut m = Self {
            debug: 0,
            hang_wait_sec: 0,
            pause: [0; 4],
            busy: [0; 4],
            flags: [false; 10],
            subs: Vec::new(),
            name: cfg.name.as_deref().unwrap_or("(null)").into(),
        };
        let mask = (EventMask::VALUE | EventMask::ALARM).bits();
        for k in Mon::ALL {
            let pv = k.pv(cfg);
            let sub = loop {
                if let Ok(sub) = DbSubscription::subscribe_with_mask(db, &pv, 0, mask).await {
                    break sub;
                }
                tracing::warn!("scanProgress: waiting for {pv} to connect");
                epics_base_rs::runtime::task::sleep(Duration::from_secs(10)).await;
            };
            if let Ok(v) = db.get_pv(&pv) {
                m.apply(k, &v);
            }
            m.subs.push((k, sub));
        }
        m
    }

    fn apply(&mut self, k: Mon, v: &EpicsValue) {
        let x = v.to_f64().unwrap_or(0.0);
        match k {
            Mon::Debug => self.debug = x as i16,
            Mon::HangWaitSec => self.hang_wait_sec = c_int(x),
            Mon::Pause(n) => self.pause[n] = x as i16,
            Mon::Busy(n) => self.busy[n] = x as i16,
        }
        self.flags[k.index()] = true;
    }

    /// Take every queued post; true when one reached a variable `wakes`
    /// names, which is what wakes seq's state set (`ss_wakeup`'s mask).
    fn poll(&mut self, wakes: impl Fn(Mon) -> bool) -> bool {
        let mut woke = false;
        let mut posts = Vec::new();
        for (k, sub) in &mut self.subs {
            while let Ok(event) = sub.try_recv_event() {
                if let Some(v) = event.value().as_value() {
                    posts.push((*k, v.clone()));
                }
            }
        }
        for (k, v) in posts {
            self.apply(k, &v);
            woke |= wakes(k);
        }
        woke
    }

    /// Drain the subscriptions, registering the waker; ready once a post
    /// reached a variable `wakes` names.
    fn poll_ready(&mut self, cx: &mut Context<'_>, wakes: impl Fn(Mon) -> bool) -> Poll<()> {
        let mut posts = Vec::new();
        for (k, sub) in &mut self.subs {
            while let Poll::Ready(Some(event)) = sub.poll_recv_event(cx) {
                if let Some(v) = event.value().as_value() {
                    posts.push((*k, v.clone()));
                }
            }
        }
        let mut woke = false;
        for (k, v) in posts {
            self.apply(k, &v);
            woke |= wakes(k);
        }
        if woke { Poll::Ready(()) } else { Poll::Pending }
    }

    /// Sleep until a post reaches a variable `wakes` names or `deadline`
    /// passes.
    async fn wait(&mut self, deadline: Option<Instant>, wakes: impl Fn(Mon) -> bool + Copy) {
        let ready = std::future::poll_fn(|cx| self.poll_ready(cx, wakes));
        match deadline {
            Some(at) => {
                let left = at.saturating_duration_since(Instant::now());
                let _ = epics_base_rs::runtime::task::timeout(left, ready).await;
            }
            None => ready.await,
        }
    }

    fn ef_test(&self, k: Mon) -> bool {
        self.flags[k.index()]
    }

    fn ef_clear(&mut self, k: Mon) {
        self.flags[k.index()] = false;
    }

    /// `DEBUG_PRINT` (`seqPVmacros.h:228-238`); `__FILE__` is the path the
    /// build hands cpp.
    async fn debug(&self, level: i16, line: u32, msg: impl FnOnce() -> String) {
        if self.debug >= level {
            println!("<../scanProg.st,{line},{},{level}> {}", self.name, msg());
            epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// `delay()` against the time the state was entered (`seq_task.c:316-320`:
/// a self-transition re-enters, so its delays restart).
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
        let at = self.entered + Duration::from_secs_f64(secs);
        if Instant::now() >= at {
            return true;
        }
        self.next = Some(self.next.map_or(at, |n| n.min(at)));
        false
    }
}

/// The two C globals the state sets share.
#[derive(Default)]
struct Shared {
    scan_dim: AtomicI32,
    paused: AtomicBool,
}

/// `pvPut(var, SYNC)` of `PVPUT`; a failed put is seq's error return, which
/// the program never tests.
async fn put_sync(pv: &SeqPv, v: EpicsValue) {
    let _ = pv.put_sync(v).await;
}

/// `PVPUTSTR`: a `SYNC` string put, then `epicsThreadSleep(0.01)`.
async fn put_str(pv: &SeqPv, s: &str) {
    let _ = pv.put_sync(EpicsValue::String(s.into())).await;
    epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
}

/// A plain `pvPut(var)`: no completion is waited for.
fn put(pv: &SeqPv, v: EpicsValue) {
    let _ = pv.put(v);
}

/// The `scanProgress` state set's channels.
struct ProgressPvs {
    ss: StateSet,
    fraction_done: SeqPv,
    percent_done: SeqPv,
    starting: SeqPv,
    ending: SeqPv,
    remaining: SeqPv,
    pause_time: SeqPv,
    total_elapsed: SeqPv,
    total_active: SeqPv,
    pause_sec: SeqPv,
    remaining_sec: SeqPv,
    npts: SeqPv,
    cpt: SeqPv,
    running: SeqPv,
    npauses: SeqPv,
    hung_sec: SeqPv,
    hung_time: SeqPv,
    version: SeqPv,
    npts_n: [SeqPv; 4],
    cpt_n: [SeqPv; 4],
    debug: SeqPv,
    hang_wait_sec: SeqPv,
    busy_n: [SeqPv; 4],
}

impl ProgressPvs {
    fn new(ss: StateSet, cfg: &ScanProgressConfig) -> Self {
        let p = |var: &str, n: &str| ss.pv(var, &cfg.pv(n));
        let scan = |vars: [&str; 4], field: &str| {
            std::array::from_fn(|n| ss.pv(vars[n], &cfg.scan(n, field)))
        };
        Self {
            fraction_done: p("fractionDone", "fractionDone"),
            percent_done: p("percentDone", "percentDone"),
            starting: p("startingTimeStr", "startingTimeStr"),
            ending: p("endingTimeStr", "endingTimeStr"),
            remaining: p("remainingTimeStr", "remainingTimeStr"),
            pause_time: p("pauseTimeStr", "pauseTimeStr"),
            total_elapsed: p("totalElapsedTimeStr", "totalElapsedTimeStr"),
            total_active: p("totalActiveTimeStr", "totalActiveTimeStr"),
            pause_sec: p("pauseSec", "pauseSec"),
            remaining_sec: p("remainingSec", "remainingSec"),
            npts: p("npts", "Ntotal"),
            cpt: p("cpt", "Nfinished"),
            running: p("running", "running"),
            npauses: p("Npauses", "Npauses"),
            hung_sec: p("hungSec", "hungSec"),
            hung_time: p("hungTimeStr", "hungTimeStr"),
            version: p("version", "version"),
            npts_n: scan(["npts1", "npts2", "npts3", "npts4"], "NPTS"),
            cpt_n: scan(["cpt1", "cpt2", "cpt3", "cpt4"], "CPT"),
            debug: p("scanProgressDebug", "debug"),
            hang_wait_sec: p("hangWaitSec", "hangWaitSec"),
            busy_n: scan(["busy1", "busy2", "busy3", "busy4"], "BUSY"),
            ss,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ProgressState {
    Init,
    Idle,
    DuringScan,
    DuringScanPause,
    ScanFinish,
}

/// The `scanProgress` state set's own variables.
#[derive(Default)]
struct Progress {
    fraction_done: f64,
    npts: i32,
    cpt: i32,
    npts_n: [i32; 4],
    cpt_n: [i32; 4],
    pause_sec: i32,
    remaining_sec: i32,
    npauses: i32,
    hung_sec: i32,
    start_time: i64,
    pause_start: i64,
    predict_end: i64,
    cpt_at_last_ok: i64,
    time_of_last_ok: i64,
    begin_pause: bool,
}

impl Progress {
    fn busy(m: &Monitored) -> bool {
        m.busy.iter().any(|&b| b != 0)
    }

    /// The `pvGet(cptN)` cascade over the active dimensions, then `calc_cpt`.
    async fn read_cpt(&mut self, pvs: &ProgressPvs, scan_dim: i32) -> i32 {
        for d in 0..scan_dim.clamp(1, 4) as usize {
            self.cpt_n[d] = pvs.cpt_n[d].get_i32().await;
        }
        calc_cpt(scan_dim, self.cpt_n, self.npts_n) as i32
    }
}

/// `ss scanProgress` (`scanProg.st:70-317`).
async fn scan_progress(
    cfg: &ScanProgressConfig,
    db: &PvDatabase,
    pvs: ProgressPvs,
    shared: &Shared,
) {
    let mut m = Monitored::connect(db, cfg).await;
    let mut v = Progress::default();
    let mut state = ProgressState::Init;
    loop {
        pvs.ss.flush().await;
        let mut delays = Delays::new();
        // The variables this state's `when` clauses name: seq wakes the
        // state set on their posts alone.
        let wakes = move |k: Mon| match state {
            ProgressState::Idle => matches!(k, Mon::Debug | Mon::HangWaitSec | Mon::Busy(_)),
            ProgressState::DuringScan | ProgressState::DuringScanPause => matches!(k, Mon::Busy(_)),
            ProgressState::Init | ProgressState::ScanFinish => false,
        };
        let next = loop {
            m.poll(wakes);
            delays.next = None;
            if let Some(next) =
                progress_when(&pvs, &mut m, &mut v, shared, state, &mut delays).await
            {
                break next;
            }
            m.wait(delays.next, wakes).await;
        };
        state = next;
    }
}

/// One evaluation of `state`'s `when` clauses, in order, running the first
/// true clause's action; `Some` is the state it names.
async fn progress_when(
    pvs: &ProgressPvs,
    m: &mut Monitored,
    v: &mut Progress,
    shared: &Shared,
    state: ProgressState,
    delays: &mut Delays,
) -> Option<ProgressState> {
    let scan_dim = shared.scan_dim.load(Ordering::SeqCst);
    let paused = shared.paused.load(Ordering::SeqCst);
    match state {
        ProgressState::Init => {
            shared.scan_dim.store(1, Ordering::SeqCst);
            m.debug = pvs.debug.get_i16().await;
            m.flags[Mon::Debug.index()] = true;
            put_sync(&pvs.version, EpicsValue::Double(PROG_BAR_VERSION)).await;
            v.fraction_done = 0.0;
            put_sync(&pvs.fraction_done, EpicsValue::Double(0.0)).await;
            put_sync(&pvs.percent_done, EpicsValue::Double(0.0)).await;
            for pv in [
                &pvs.ending,
                &pvs.remaining,
                &pvs.pause_time,
                &pvs.starting,
                &pvs.total_elapsed,
                &pvs.total_active,
                &pvs.hung_time,
            ] {
                put_str(pv, "").await;
            }
            v.hung_sec = 0;
            put_sync(&pvs.hung_sec, EpicsValue::Long(0)).await;
            v.pause_sec = 0;
            put_sync(&pvs.pause_sec, EpicsValue::Long(0)).await;
            v.remaining_sec = 0;
            put_sync(&pvs.remaining_sec, EpicsValue::Long(0)).await;
            v.npts = 0;
            put_sync(&pvs.npts, EpicsValue::Long(0)).await;
            v.cpt = 0;
            put_sync(&pvs.cpt, EpicsValue::Long(0)).await;
            v.npauses = 0;
            put_sync(&pvs.npauses, EpicsValue::Long(0)).await;
            put_sync(&pvs.running, EpicsValue::Short(0)).await;
            m.hang_wait_sec = pvs.hang_wait_sec.get_i32().await;
            m.flags[Mon::HangWaitSec.index()] = true;
            for n in 0..4 {
                m.busy[n] = pvs.busy_n[n].get_i16().await;
                m.flags[Mon::Busy(n).index()] = true;
            }
            v.begin_pause = false;
            v.cpt_at_last_ok = 0;
            m.ef_clear(Mon::Debug);
            m.ef_clear(Mon::HangWaitSec);
            m.debug(1, 154, || "init complete in state init".into())
                .await;
            Some(ProgressState::Idle)
        }
        ProgressState::Idle => {
            if m.ef_test(Mon::Debug) {
                m.debug(1, 162, || format!("changed debug flag to {}", m.debug))
                    .await;
                m.ef_clear(Mon::Debug);
                return Some(ProgressState::Idle);
            }
            if m.ef_test(Mon::HangWaitSec) {
                let msg = format!(
                    "changed requred time for a hang to {} (sec)",
                    m.hang_wait_sec
                );
                m.debug(1, 168, || msg).await;
                m.ef_clear(Mon::HangWaitSec);
                return Some(ProgressState::Idle);
            }
            if !Progress::busy(m) {
                return None;
            }
            m.debug(1, 173, || "scan started".into()).await;
            let dim = if m.busy[3] != 0 {
                4
            } else if m.busy[2] != 0 {
                3
            } else if m.busy[1] != 0 {
                2
            } else {
                1
            };
            shared.scan_dim.store(dim, Ordering::SeqCst);
            m.debug(2, 179, || format!("got scanDim = {dim}")).await;
            v.start_time = time_now();
            put(
                &pvs.starting,
                EpicsValue::String(format_time_str(v.start_time, true, v.start_time).into()),
            );
            v.npauses = 0;
            put_sync(&pvs.npauses, EpicsValue::Long(0)).await;
            v.pause_sec = 0;
            put_sync(&pvs.pause_sec, EpicsValue::Long(0)).await;
            v.fraction_done = 0.0;
            put_sync(&pvs.fraction_done, EpicsValue::Double(0.0)).await;
            put_sync(&pvs.percent_done, EpicsValue::Double(0.0)).await;
            put_sync(&pvs.running, EpicsValue::Short(1)).await;
            put_str(&pvs.total_elapsed, "00:00:00").await;
            put_str(&pvs.total_active, "00:00:00").await;
            put_str(&pvs.pause_time, "00:00:00").await;
            v.npts_n[0] = pvs.npts_n[0].get_i32().await;
            v.npts = v.npts_n[0];
            for d in 1..dim as usize {
                v.npts_n[d] = pvs.npts_n[d].get_i32().await;
                v.npts = v.npts.wrapping_mul(v.npts_n[d]);
            }
            put(&pvs.npts, EpicsValue::Long(v.npts));
            v.cpt_at_last_ok = 0;
            v.time_of_last_ok = v.start_time;
            v.hung_sec = 0;
            put_sync(&pvs.hung_sec, EpicsValue::Long(0)).await;
            put_str(&pvs.hung_time, "").await;
            Some(ProgressState::DuringScan)
        }
        ProgressState::DuringScan => {
            if !Progress::busy(m) {
                m.debug(2, 217, || {
                    "in duringScan, scan is done, goto scanFinish".into()
                })
                .await;
                return Some(ProgressState::ScanFinish);
            }
            if paused {
                v.begin_pause = true;
                m.debug(2, 222, || {
                    "in duringScan, scan is paused, goto duringScanPause".into()
                })
                .await;
                return Some(ProgressState::DuringScanPause);
            }
            if !delays.delay(UPDATE_DELAY) {
                return None;
            }
            m.debug(3, 226, || {
                "in duringScan, do a regular update to progress".into()
            })
            .await;
            v.cpt = v.read_cpt(pvs, scan_dim).await;
            put(&pvs.cpt, EpicsValue::Long(v.cpt));
            v.fraction_done = f64::from(v.cpt) / f64::from(v.npts);
            let msg = format!(
                "scanDim = {scan_dim},  completed = {{{}/{},  {}/{},  {}/{},  {}/{}}}, fractionDone = {}",
                v.cpt_n[0],
                v.npts_n[0],
                v.cpt_n[1],
                v.npts_n[1],
                v.cpt_n[2],
                v.npts_n[2],
                v.cpt_n[3],
                v.npts_n[3],
                fmt_g(v.fraction_done)
            );
            m.debug(4, 241, || msg).await;
            // NaN passes through as it does C's if/else chain.
            v.fraction_done = v.fraction_done.clamp(0.0, 1.0);
            let now = time_now();
            if v.fraction_done > 0.0 {
                v.remaining_sec = c_int(
                    (1.0 - v.fraction_done) / v.fraction_done
                        * (now - v.start_time - i64::from(v.pause_sec)) as f64,
                );
                put(&pvs.remaining_sec, EpicsValue::Long(v.remaining_sec));
                v.predict_end = now + i64::from(v.remaining_sec);
                put_str(&pvs.remaining, &clocks2str(i64::from(v.remaining_sec))).await;
                put_str(&pvs.total_elapsed, &clocks2str(now - v.start_time)).await;
                put_str(
                    &pvs.total_active,
                    &clocks2str(now - v.start_time - i64::from(v.pause_sec)),
                )
                .await;
                let ending = format_time_str(v.predict_end, false, now);
                put(&pvs.ending, EpicsValue::String(ending.as_str().into()));
                let msg = format!(
                    "   now = {now} sec,   remaining = {},  predictEnd = {}",
                    v.remaining_sec, v.predict_end
                );
                m.debug(5, 258, || msg).await;
                let remaining = clocks2str(i64::from(v.remaining_sec));
                m.debug(5, 260, || {
                    format!("   remainingTimeStr = '{remaining}',   endingTimeStr = '{ending}'")
                })
                .await;
            } else {
                put_str(&pvs.remaining, "").await;
                put_str(&pvs.total_active, "").await;
                put_str(&pvs.ending, "").await;
                put_str(&pvs.total_elapsed, "").await;
                v.remaining_sec = 0;
                put_sync(&pvs.remaining_sec, EpicsValue::Long(0)).await;
            }
            put(&pvs.fraction_done, EpicsValue::Double(v.fraction_done));
            put_sync(
                &pvs.percent_done,
                EpicsValue::Double(100.0 * v.fraction_done),
            )
            .await;

            if i64::from(v.cpt) != v.cpt_at_last_ok {
                v.cpt_at_last_ok = i64::from(v.cpt);
                v.time_of_last_ok = now;
                v.hung_sec = 0;
                put_sync(&pvs.hung_sec, EpicsValue::Long(0)).await;
                put_str(&pvs.hung_time, "").await;
            } else if now - v.time_of_last_ok > i64::from(m.hang_wait_sec) {
                v.hung_sec = (now - v.time_of_last_ok) as i32;
                put(&pvs.hung_sec, EpicsValue::Long(v.hung_sec));
                put_str(&pvs.hung_time, &clocks2str(i64::from(v.hung_sec))).await;
            }
            Some(ProgressState::DuringScan)
        }
        ProgressState::DuringScanPause => {
            if !Progress::busy(m) {
                m.debug(2, 291, || {
                    "in duringScanPause, scan is done, goto scanFinish".into()
                })
                .await;
                return Some(ProgressState::ScanFinish);
            }
            if v.begin_pause {
                v.begin_pause = false;
                v.pause_start = time_now();
                v.npauses = v.npauses.wrapping_add(1);
                put_sync(&pvs.npauses, EpicsValue::Long(v.npauses)).await;
                m.debug(2, 298, || "in duringScanPause, started a scan pause".into())
                    .await;
                v.time_of_last_ok = v.pause_start;
                v.hung_sec = 0;
                put_sync(&pvs.hung_sec, EpicsValue::Long(0)).await;
                put_str(&pvs.hung_time, "").await;
                return Some(ProgressState::DuringScanPause);
            }
            if !paused {
                let now = time_now();
                v.pause_sec = v.pause_sec.wrapping_add((now - v.pause_start) as i32);
                put(&pvs.pause_sec, EpicsValue::Long(v.pause_sec));
                put_str(&pvs.pause_time, &clocks2str(i64::from(v.pause_sec))).await;
                m.debug(2, 310, || "in duringScanPause, scan pause rescinded".into())
                    .await;
                v.time_of_last_ok = now;
                return Some(ProgressState::DuringScan);
            }
            if !delays.delay(UPDATE_DELAY) {
                return None;
            }
            let now = time_now();
            let current = clocks2str(now - v.pause_start);
            let total = clocks2str(now - v.pause_start + i64::from(v.pause_sec));
            let mut pause_time = format!("current {current},  total {total}");
            pause_time.truncate(39);
            put(
                &pvs.pause_time,
                EpicsValue::String(pause_time.as_str().into()),
            );
            put_str(&pvs.total_elapsed, &clocks2str(now - v.start_time)).await;
            if v.fraction_done > 0.0 {
                v.remaining_sec = c_int(
                    (1.0 - v.fraction_done) / v.fraction_done
                        * (now - v.start_time - i64::from(v.pause_sec)) as f64,
                );
                put(&pvs.remaining_sec, EpicsValue::Long(v.remaining_sec));
                v.predict_end = now + i64::from(v.remaining_sec);
                let ending = format_time_str(v.predict_end, false, now);
                put(&pvs.ending, EpicsValue::String(ending.as_str().into()));
            }
            m.debug(5, 330, || {
                format!("   periodic check in duringScanPause, time paused; '{pause_time}', ")
            })
            .await;
            Some(ProgressState::DuringScanPause)
        }
        ProgressState::ScanFinish => {
            let now = time_now();
            put_str(&pvs.remaining, "00:00:00").await;
            put_str(
                &pvs.total_active,
                &clocks2str(now - v.start_time - i64::from(v.pause_sec)),
            )
            .await;
            put_str(&pvs.total_elapsed, &clocks2str(now - v.start_time)).await;
            put(
                &pvs.ending,
                EpicsValue::String(format_time_str(now, true, now).into()),
            );
            put_sync(&pvs.running, EpicsValue::Short(0)).await;
            v.remaining_sec = 0;
            put_sync(&pvs.remaining_sec, EpicsValue::Long(0)).await;
            v.cpt = v.read_cpt(pvs, scan_dim).await;
            put(&pvs.cpt, EpicsValue::Long(v.cpt));
            v.fraction_done = f64::from(v.cpt) / f64::from(v.npts);
            if v.fraction_done > 1.0 {
                v.fraction_done = 1.0;
            }
            put(&pvs.fraction_done, EpicsValue::Double(v.fraction_done));
            put_sync(
                &pvs.percent_done,
                EpicsValue::Double(100.0 * v.fraction_done),
            )
            .await;
            v.hung_sec = 0;
            put_sync(&pvs.hung_sec, EpicsValue::Long(0)).await;
            put_str(&pvs.hung_time, "").await;
            m.debug(2, 367, || {
                "no scan is busy in duringScan, back to idle".into()
            })
            .await;
            Some(ProgressState::Idle)
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PauseState {
    Init,
    Idle,
    CheckAndSet,
}

/// `ss scanPauseUpdate` (`scanProg.st:348-408`).
async fn scan_pause_update(
    cfg: &ScanProgressConfig,
    db: &PvDatabase,
    ss: StateSet,
    shared: &Shared,
) {
    let paused_pv = ss.pv("paused", &cfg.pv("paused"));
    let mut m = Monitored::connect(db, cfg).await;
    let mut almost_paused = false;
    let mut state = PauseState::Init;
    loop {
        ss.flush().await;
        let mut delays = Delays::new();
        let wakes =
            move |k: Mon| state == PauseState::Idle && matches!(k, Mon::Pause(_) | Mon::Busy(_));
        state = loop {
            m.poll(wakes);
            delays.next = None;
            let next = match state {
                PauseState::Init => {
                    shared.paused.store(false, Ordering::SeqCst);
                    put_sync(&paused_pv, EpicsValue::Short(0)).await;
                    almost_paused = false;
                    // Give the main state set a second to run its init.
                    epics_base_rs::runtime::task::sleep(Duration::from_secs(1)).await;
                    m.flags[Mon::Pause(0).index()] = true;
                    m.debug(1, 385, || "completed state initPause".into()).await;
                    Some(PauseState::Idle)
                }
                PauseState::Idle => {
                    if (0..4).any(|n| m.ef_test(Mon::Pause(n))) {
                        m.debug(7, 393, || {
                            // `%p` of the four flag ids, which snc numbers
                            // from 1 in declaration order.
                            "at top of state idlePause, monitors are : 0x3,  0x4,  0x5,  0x6".into()
                        })
                        .await;
                        for n in 0..4 {
                            m.ef_clear(Mon::Pause(n));
                        }
                        Some(PauseState::CheckAndSet)
                    } else if almost_paused && (0..4).any(|n| m.ef_test(Mon::Busy(n))) {
                        let b = m.busy;
                        m.debug(6, 404, || {
                            format!(
                                "paused but waiting for busy, busy flags are:  busy1={}, busy2={}, busy3={}, busy4={}",
                                b[0], b[1], b[2], b[3]
                            )
                        })
                        .await;
                        for n in 0..4 {
                            m.ef_clear(Mon::Busy(n));
                        }
                        Some(PauseState::CheckAndSet)
                    } else if delays.delay(UPDATE_DELAY_LONG) {
                        m.debug(7, 412, || {
                            "delay(UPDATE_DELAY_LONG) in scanPauseUpdate".into()
                        })
                        .await;
                        Some(PauseState::CheckAndSet)
                    } else {
                        None
                    }
                }
                PauseState::CheckAndSet => {
                    let p = m.pause;
                    m.debug(8, 420, || {
                        format!(
                            "paused flags are pause1={}, pause2={}, pause3={}, pause4={}",
                            p[0], p[1], p[2], p[3]
                        )
                    })
                    .await;
                    let scan_dim = shared.scan_dim.load(Ordering::SeqCst);
                    let a_pause = (0..4).any(|d| (d as i32) < scan_dim.max(1) && m.pause[d] != 0);
                    let stopped = all_stopped(scan_dim, m.pause, m.busy);
                    almost_paused = a_pause && !stopped;
                    let paused = a_pause && stopped;
                    shared.paused.store(paused, Ordering::SeqCst);
                    put(&paused_pv, EpicsValue::Short(i16::from(paused)));
                    Some(PauseState::Idle)
                }
            };
            if let Some(next) = next {
                break next;
            }
            m.wait(delays.next, wakes).await;
        };
    }
}

/// Run `scanProgress` against `db`; it never returns.
pub async fn run(
    config: ScanProgressConfig,
    db: PvDatabase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let queue = PutQueue::new(&db, "scanProgress");
    let progress = ProgressPvs::new(queue.state_set("scanProgress"), &config);
    let pause = queue.state_set("scanPauseUpdate");
    let shared = Shared::default();
    // seq runs each state set on its own thread; neither returns, so
    // polling both in one task is the whole join.
    let mut ss1 = std::pin::pin!(scan_progress(&config, &db, progress, &shared));
    let mut ss2 = std::pin::pin!(scan_pause_update(&config, &db, pause, &shared));
    std::future::poll_fn(|cx| {
        let _ = ss1.as_mut().poll(cx);
        let _ = ss2.as_mut().poll(cx);
        Poll::<()>::Pending
    })
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clocks2str_is_hh_mm_ss_with_the_sign_on_the_hours() {
        assert_eq!(clocks2str(0), "00:00:00");
        assert_eq!(clocks2str(3725), "01:02:05");
        assert_eq!(clocks2str(-3725), "-1:02:05");
        // `hh = sign * isec / 60` is negative zero for under an hour.
        assert_eq!(clocks2str(-59), "00:00:59");
        assert_eq!(clocks2str(360_000), "100:00:00");
        assert_eq!(clocks2str(32_000_000), "8888:53:20");
        assert_eq!(clocks2str(32_000_001), "infinite");
        assert_eq!(clocks2str(-32_000_001), "-infinite");
    }

    #[test]
    fn calc_cpt_weights_each_outer_point_by_the_points_inside_it() {
        let npts = [10, 5, 3, 2];
        let cpt = [4, 2, 1, 1];
        assert_eq!(calc_cpt(1, cpt, npts), 4);
        assert_eq!(calc_cpt(2, cpt, npts), 4 + 2 * 10);
        assert_eq!(calc_cpt(3, cpt, npts), 4 + 2 * 10 + 50);
        assert_eq!(calc_cpt(4, cpt, npts), 4 + 2 * 10 + 50 + 150);
    }

    #[test]
    fn all_stopped_ignores_dimensions_past_the_scan() {
        let none = [0; 4];
        assert!(all_stopped(1, none, none));
        assert!(!all_stopped(1, none, [1, 0, 0, 0]));
        assert!(all_stopped(1, [1, 0, 0, 0], [1, 1, 1, 1]));
        assert!(!all_stopped(2, [1, 0, 0, 0], [1, 1, 0, 0]));
        assert!(all_stopped(2, [1, 1, 0, 0], [1, 1, 1, 1]));
        assert!(all_stopped(4, [0, 0, 0, 1], [0, 0, 0, 1]));
    }

    #[test]
    fn format_time_str_names_the_day_against_now() {
        let now = Local
            .with_ymd_and_hms(2026, 3, 11, 12, 0, 0)
            .earliest()
            .unwrap()
            .timestamp();
        let at = |h: i64| now + h * 3600;
        assert_eq!(
            format_time_str(at(2), false, now),
            "14:00:00 Today (Wednesday)"
        );
        assert_eq!(
            format_time_str(at(24), false, now),
            "12:00:00 Tomorrow (Thursday)"
        );
        assert_eq!(
            format_time_str(at(-24), false, now),
            "12:00:00 Yesterday (Tuesday)"
        );
        assert_eq!(
            format_time_str(at(48), false, now),
            "12:00:00  (Fri) Mar 13, 2026"
        );
        assert_eq!(
            format_time_str(at(2), true, now),
            "14:00:00  (Wed) Mar 11, 2026"
        );
    }

    #[test]
    fn fmt_g_is_printf_g() {
        assert_eq!(fmt_g(0.0), "0");
        assert_eq!(fmt_g(0.5), "0.5");
        assert_eq!(fmt_g(1.0 / 3.0), "0.333333");
        assert_eq!(fmt_g(2.0 / 3.0), "0.666667");
        assert_eq!(fmt_g(1.0), "1");
        assert_eq!(fmt_g(0.0001), "0.0001");
        assert_eq!(fmt_g(0.00001234), "1.234e-05");
        assert_eq!(fmt_g(123456.0), "123456");
        assert_eq!(fmt_g(1234567.0), "1.23457e+06");
        assert_eq!(fmt_g(999999.5), "1e+06");
        assert_eq!(fmt_g(f64::INFINITY), "inf");
        assert_eq!(fmt_g(f64::NAN), "nan");
    }

    #[test]
    fn c_int_truncates_and_gives_int_min_out_of_range() {
        assert_eq!(c_int(2.9), 2);
        assert_eq!(c_int(-2.9), -2);
        assert_eq!(c_int(f64::NAN), i32::MIN);
        assert_eq!(c_int(1e10), i32::MIN);
    }
}
