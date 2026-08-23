use std::any::Any;
use std::sync::LazyLock;
use std::time::Instant;

use super::dbd_generated;
use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::record::{
    FieldDesc, FieldMetadataOverride, ProcessAction, ProcessOutcome, Record,
};
use epics_base_rs::types::{EpicsValue, PvString};

/// Maximum number of scaler channels.
pub const MAX_SCALER_CHANNELS: usize = 64;

const VERSION: f32 = 3.19;

// Scaler hardware state
const SCALER_STATE_IDLE: i16 = 0;
const SCALER_STATE_WAITING: i16 = 1;
const SCALER_STATE_COUNTING: i16 = 2;

// User request state
const USER_STATE_IDLE: i16 = 0;
const USER_STATE_WAITING: i16 = 1;
const USER_STATE_REQSTART: i16 = 2;
const USER_STATE_COUNTING: i16 = 3;

/// Hold time (seconds) before auto-count resumes after a user count or a
/// dbPutNotify operation. C `scalerRecord.c:135` — `volatile int
/// scaler_wait_time = 10;` (used via `MAX(dly1, scaler_wait_time)` at
/// `scalerRecord.c:489-490`).
const SCALER_WAIT_TIME: f64 = 10.0;

/// Device command names used in ProcessAction::DeviceCommand.
pub const CMD_RESET: &str = "scaler_reset";
pub const CMD_ARM: &str = "scaler_arm";
pub const CMD_WRITE_PRESET: &str = "scaler_write_preset";
/// Count-start command — runs the full REQSTART driver sequence
/// (write per-channel presets, reconcile PR1/TP/FREQ with what the
/// driver actually programmed, then arm). C `scalerRecord.c:405-432`.
/// This sequence cannot be split into independent `DeviceCommand`s
/// because the reconciliation must observe the driver's adjustment
/// *between* the preset writes and the arm — exactly as the C
/// `write_preset` calls run synchronously inside `process()`.
pub const CMD_START_COUNT: &str = "scaler_start_count";
/// Auto-count start command — the auto-count analogue of
/// `CMD_START_COUNT`. C `scalerRecord.c:508-535`: writes the auto
/// presets, applies the `save_pr1 != pr1` driver-adjustment re-write
/// (`:514-522`), then **restores the user's PR1** (`:532` —
/// "Don't let autocount disturb user's channel-1 preset") and arms.
/// Unlike REQSTART it does not recompute `TP`; it only adopts a
/// driver-changed `FREQ`.
pub const CMD_AUTOCOUNT: &str = "scaler_autocount";

/// Scaler record — up to 64-channel 32-bit counter with preset and auto-count.
///
/// Ported from EPICS scaler module `scalerRecord.c`.
///
/// Each channel has:
/// - S{n}: current count value (read-only)
/// - PR{n}: preset count value
/// - G{n}: gate/preset enable (N/Y)
/// - D{n}: count direction (Up/Dn)
/// - NM{n}: channel name
///
/// Channel 1 (S1/PR1) is the time-base channel: T = S1 / FREQ.
///
/// **Driver integration**: The record does NOT hold a direct driver reference.
/// - `check_done()` and `read_counts()` are performed by device support's
///   `read()` BEFORE process(), writing results into the record's fields.
/// - `reset`, `write_preset`, `arm` commands are sent as
///   `ProcessAction::DeviceCommand` and executed by the framework via
///   `DeviceSupport::handle_command()` AFTER process().
///
/// **DLY/DLY1:** Implemented via `ProcessAction::ReprocessAfter`.
///
/// An armed user-count start delay: C's `pdelayCallback` watchdog as data.
#[derive(Clone, Copy, Debug)]
struct CountDelay {
    start: Instant,
    /// The delay the timer was armed with, in seconds.
    secs: f64,
}

impl CountDelay {
    fn expired(&self) -> bool {
        self.start.elapsed().as_secs_f64() >= self.secs
    }
}

/// Which C callback the record's one pending re-entry stands for.
///
/// C arms three independent `callbackRequestDelayed` timers and only two of
/// them process the record: `delayCallbackFunc` (scalerRecord.c:216-231) and
/// `autoCallbackFunc` (:233-239) end in `scanOnce`, while
/// `updateCallbackFunc` (:203-214) calls `updateCounts` and returns — it
/// cannot reach `recGblFwdLink` at :480. The framework has one re-entry shape,
/// `ProcessAction::ReprocessAfter`, and it is always a `process()`, so which
/// callback a timer models has to be recorded when it is armed rather than
/// inferred from record state when it fires.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReentryKind {
    /// `updateCallbackFunc` — the periodic display refresh.
    DisplayRefresh,
    /// `delayCallbackFunc` / `autoCallbackFunc` — a real process cycle.
    Process,
}

pub struct ScalerRecord {
    // --- Control/Status ---
    pub val: f64,
    pub freq: f64,
    pub cnt: i16,
    pub pcnt: i16,
    pub ss: i16,
    pub us: i16,
    pub cont: i16,
    pub rate: f32,
    pub rat1: f32,
    pub dly: f32,
    pub dly1: f32,
    pub nch: i16,
    pub tp: f64,
    pub tp1: f64,
    pub t: f64,
    pub vers: f32,
    pub prec: i16,
    pub egu: PvString,
    pub out: String,
    pub cout: String,
    pub coutp: String,

    // --- Per-channel arrays (64 channels) ---
    pub d: [i16; MAX_SCALER_CHANNELS],
    pub g: [i16; MAX_SCALER_CHANNELS],
    pub pr: [u32; MAX_SCALER_CHANNELS],
    pub s: [u32; MAX_SCALER_CHANNELS],
    pub nm: [PvString; MAX_SCALER_CHANNELS],

    // --- Delay tracking ---
    //
    // C runs TWO independent delay callbacks with two timers, and the port
    // gives each its own cell so neither can retime or cancel the other:
    //
    //   * `pdelayCallback` / `delayCallbackFunc` (scalerRecord.c:216-231) —
    //     the USER count start delay (DLY), armed by `special(CNT)`; tracked
    //     by `count_delay`.
    //   * `pauto_callback` / `autoCallbackFunc` (:233-240) — the AUTOCOUNT
    //     restart hold (DLY1), armed by `process()`; tracked by
    //     `delay_start` + `autocount_delay`.
    /// Start instant of the current SCALER_STATE_WAITING autocount hold.
    delay_start: Option<Instant>,
    /// The autocount hold time (seconds) the current SCALER_STATE_WAITING
    /// period was scheduled with. C scalerRecord.c computes `dly_sec`
    /// once (`MAX(dly1, scaler_wait_time)` after a user count) and the
    /// `autoCallbackFunc` fires after exactly that interval; the port
    /// must compare elapsed time against the scheduled value, not the
    /// live `dly1` (which the user may change mid-wait).
    autocount_delay: f64,
    /// The armed user-count start delay: C's `pdelayCallback` watchdog.
    /// `Some` exactly while `us == USER_STATE_WAITING` with a timer pending;
    /// `special(CNT)` arms it (`callbackRequestDelayed(pdelayCallback,
    /// pscal->dly)`, scalerRecord.c:658-660), cancels it on an abort
    /// (`epicsTimerCancel`, :645), and `process()` consumes it when the
    /// scheduled interval has elapsed. `secs` is the delay the timer was armed
    /// WITH — as in C, a DLY written mid-wait cannot retime the armed wait.
    count_delay: Option<CountDelay>,
    /// The kind of the one re-entry this record currently has armed, set by
    /// `arm_reentry` — the sole emitter of `ProcessAction::ReprocessAfter`
    /// here. Consumed by `process()` on the cycle the framework flags as a
    /// continuation. Only one can be outstanding: arming mints a fresh async
    /// token, which supersedes any pending one.
    pending_reentry: Option<ReentryKind>,
    /// Set by the framework immediately before `process()` when this cycle is
    /// the record's own scheduled re-entry rather than a put, scan or forward
    /// link. Read once, with `pending_reentry`, at the top of `process()`.
    continuation: bool,

    // --- Done flag (set by device support read, consumed by process) ---
    /// Set by device support's read() when counting has completed.
    /// process() checks and clears this flag.
    pub(crate) done_flag: bool,

    /// Channel-1 preset captured by `process()` at the start of a
    /// REQSTART count, BEFORE the `pr[0] = NINT(tp*freq)`
    /// self-consistency guard runs. C `scalerRecord.c:406` captures
    /// `old_pr1 = pscal->pr1` *before* the `:409-410` guard; the
    /// `:424` `old_pr1 != pr1` test therefore fires when the guard
    /// alone changed PR1 (e.g. the user wrote TP and `frac(tp*freq)
    /// >= 0.5`). `process()` writes this; `run_start_count` reads it
    /// as the true pre-guard baseline for `count_start_finalize_tp`.
    pub(crate) reqstart_old_pr1: u32,

    /// Carries C's `special()` COUTP put — and only that one.
    ///
    /// C `scalerRecord.c:623-624` calls `dbPutLink(&pscal->coutp, ...)` inside
    /// `special()` itself, so the link is written — and its target processed —
    /// while the scaler is still IDLE, before `:637` sets REQSTART and before
    /// the CNT-triggered process cycle arms the count. The framework drains this
    /// through [`Record::take_special_actions`] at the end of the put and
    /// executes it there, which is that point.
    ///
    /// It is NOT the record's "should COUTP fire" state: C's other COUTP put
    /// (`:463`, on the finish edge) is independent, is emitted by `process()` on
    /// its own, and both fire on a user stop. Never merge the two.
    special_actions: Vec<ProcessAction>,

    /// The `db_post_events` calls C's `special()` made on the put now in flight,
    /// handed to the framework by
    /// [`Record::monitor_side_effect_fields`](crate::records::scaler::ScalerRecord).
    ///
    /// C `special()` posts these fields itself; none of them is `pp(TRUE)`, so no
    /// process cycle would otherwise post them. `special()` is the single writer:
    /// its `after == false` pre-pass — which the framework runs on EVERY put
    /// (`field_io.rs:937`) — retires the previous put's list, and each `after`
    /// arm records exactly what C's matching case posts.
    side_effect_posts: &'static [&'static str],

    /// The forward-link decision C makes *inside* `process()`.
    ///
    /// C `scalerRecord.c:470-481` calls `recGblFwdLink(pscal)` while
    /// still in the middle of process — after `updateCounts()`, guarded
    /// by `ss==IDLE && pcnt==0 && us==IDLE`, and **before** the
    /// auto-count block (`:484-541`) re-arms and drives `ss` to WAITING
    /// or COUNTING. The framework calls `should_fire_forward_link()`
    /// only *after* `process()` returns, so re-reading `ss`/`us`/`pcnt`
    /// there answers a different question than C asked. `process()` is
    /// the single owner: it clears this flag on entry and sets it at
    /// exactly C's `recGblFwdLink` line; the hook only reports it.
    fire_fwd_link: bool,
}

impl ScalerRecord {
    /// Mark counting as complete.
    ///
    /// C parity: this is the equivalent of device support's `done()`
    /// dset entry returning true (`scalerRecord.c:367`). Device support
    /// (`ScalerAsynDeviceSupport::read`) calls this before `process()`
    /// when the driver reports acquisition has finished; `process()`
    /// then consumes and clears the flag.
    pub fn set_done(&mut self) {
        self.done_flag = true;
    }
}

impl Default for ScalerRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            freq: 1.0e7,
            cnt: 0,
            pcnt: 0,
            ss: SCALER_STATE_IDLE,
            us: USER_STATE_IDLE,
            cont: 0,
            // RATE: scalerRecord.dbd `initial("10")`.
            rate: 10.0,
            rat1: 0.0,
            dly: 0.0,
            dly1: 0.0,
            nch: 0,
            // TP has no `initial` in scalerRecord.dbd: raw default is 0.
            // init_record (pass 1) applies the both-zero -> 1.0 rule,
            // matching scalerRecord.c:320-323.
            tp: 0.0,
            // TP1: scalerRecord.dbd `initial("1")`.
            tp1: 1.0,
            t: 0.0,
            vers: VERSION,
            prec: 0,
            egu: PvString::new(),
            out: String::new(),
            cout: String::new(),
            coutp: String::new(),
            d: {
                // D1: scalerRecord.dbd `initial("1")` (Dn). D2..D64 default 0.
                let mut d = [0i16; MAX_SCALER_CHANNELS];
                d[0] = 1;
                d
            },
            g: {
                // G1: scalerRecord.dbd `initial("1")` (Y). G2..G64 default 0.
                let mut g = [0i16; MAX_SCALER_CHANNELS];
                g[0] = 1;
                g
            },
            pr: [0; MAX_SCALER_CHANNELS],
            s: [0; MAX_SCALER_CHANNELS],
            nm: std::array::from_fn(|_| PvString::new()),
            delay_start: None,
            autocount_delay: 0.0,
            count_delay: None,
            pending_reentry: None,
            continuation: false,
            done_flag: false,
            reqstart_old_pr1: 0,
            special_actions: Vec::new(),
            side_effect_posts: &[],
            fire_fwd_link: false,
        }
    }
}

fn parse_indexed_field(name: &str, prefix: &str) -> Option<usize> {
    name.strip_prefix(prefix)
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&i| (1..=MAX_SCALER_CHANNELS).contains(&i))
        .map(|i| i - 1)
}

impl ScalerRecord {
    pub fn update_time(&mut self) {
        if self.freq > 0.0 {
            self.t = self.s[0] as f64 / self.freq;
        }
    }

    /// Arm this record's one pending re-entry, recording which C callback it
    /// stands for. The sole emitter of `ProcessAction::ReprocessAfter` here, so
    /// a timer cannot be armed without declaring its kind.
    fn arm_reentry(&mut self, kind: ReentryKind, delay: std::time::Duration) -> ProcessAction {
        self.pending_reentry = Some(kind);
        ProcessAction::ReprocessAfter(delay)
    }

    /// Port of C `updateCounts()` (scalerRecord.c:549-601).
    ///
    /// The framework's device support `read()` already filled `s[]` with
    /// the hardware counts before `process()` ran. This method:
    /// - C:571-575 — while `us == USER_STATE_WAITING` the displayed
    ///   counts are forced to 0 (the count has not started yet).
    /// - C:586-588 — recompute elapsed time `T = S1 / FREQ`.
    /// - C:590-596 — while `ss == SCALER_STATE_COUNTING`, return the
    ///   interval after which the record should reprocess to refresh the
    ///   display: `1 / rate`, where `rate` is the user `RATE` while the
    ///   user is counting and the auto `RAT1` otherwise. The callback is
    ///   only scheduled when that rate is `> 0.1` Hz.
    fn update_counts(&mut self) -> Option<std::time::Duration> {
        if self.us == USER_STATE_WAITING {
            self.s = [0; MAX_SCALER_CHANNELS];
        }
        self.update_time();

        if self.ss == SCALER_STATE_COUNTING {
            let rate = if self.us == USER_STATE_COUNTING {
                self.rate
            } else {
                self.rat1
            };
            if rate > 0.1 {
                return Some(epics_base_rs::runtime::time::duration_from_secs(
                    1.0 / rate as f64,
                ));
            }
        }
        None
    }

    /// Number of active channels, clamped to the physical array bound.
    ///
    /// `nch` is a public record field; device support sets it from a custom
    /// driver's `num_channels()` with no inherent bound, and a negative
    /// `i16` would wrap to a huge `usize`. Every `0..nch` loop that indexes
    /// the fixed 64-element `g`/`pr`/`d`/`s` arrays must go through this so a
    /// bad `nch` cannot cause an out-of-bounds panic.
    pub(crate) fn active_channels(&self) -> usize {
        if self.nch < 0 {
            0
        } else {
            (self.nch as usize).min(MAX_SCALER_CHANNELS)
        }
    }

    /// C's gate → direction copy, the single owner of `d[]`'s count-start value.
    ///
    /// C `scalerRecord.c:413-414` (REQSTART) and `:525-526` (auto-count re-arm)
    /// both run `for (i=0; i<pscal->nch; i++) pdir[i] = pgate[i];` — the bound is
    /// `nch`, not the physical array size, so `D` fields above the configured
    /// channel count keep whatever the user last put there.
    fn copy_gates_to_directions(&mut self) {
        for i in 0..self.active_channels() {
            self.d[i] = self.g[i];
        }
    }

    /// Saturating `f64 -> u32` cast for clock-tick counts so a large
    /// `tp * freq` cannot wrap. `ticks` already has rounding/truncation
    /// applied by the caller.
    fn ticks_to_u32(ticks: f64) -> u32 {
        if ticks <= 0.0 {
            0
        } else if ticks >= u32::MAX as f64 {
            u32::MAX
        } else {
            ticks as u32
        }
    }

    /// `NINT` — round-to-nearest cast used by the count-start path.
    /// C `scalerRecord.c:139`: `NINT(f) (unsigned long)((f)>0 ? (f)+0.5
    /// : (f)-0.5)`. Used at `scalerRecord.c:409-410` (REQSTART preset).
    pub(crate) fn pr1_nint(tp: f64, freq: f64) -> u32 {
        let f = tp * freq;
        Self::ticks_to_u32(if f > 0.0 { f + 0.5 } else { f - 0.5 })
    }

    /// Truncating `tp -> pr1` conversion used by `init_record` and the
    /// `special()` TP handler. C `scalerRecord.c:328` and `:672`:
    /// `pscal->pr1 = (epicsUInt32)(pscal->tp * pscal->freq);` — a plain
    /// cast, i.e. truncation toward zero, NOT `NINT`.
    pub(crate) fn pr1_trunc(tp: f64, freq: f64) -> u32 {
        Self::ticks_to_u32(tp * freq)
    }

    /// TP -> PR1 for the `special()` TP handler. C `scalerRecord.c:670-677`:
    /// truncating conversion, then unconditionally `d1 = g1 = 1`.
    fn tp_to_pr1(&mut self) {
        self.pr[0] = Self::pr1_trunc(self.tp, self.freq);
        self.d[0] = 1;
        self.g[0] = 1;
    }

    fn pr1_to_tp(&mut self) {
        if self.freq > 0.0 {
            self.tp = self.pr[0] as f64 / self.freq;
        }
    }

    /// First half of the REQSTART preset reconciliation — C
    /// `scalerRecord.c:420-423`.
    ///
    /// After the per-channel `write_preset` loop has stored the
    /// driver-returned channel-0 preset into `pr[0]`, this checks the
    /// C `save_pr1 != pscal->pr1` condition: if the driver adjusted
    /// the preset, recompute `pr[0] = NINT(tp*freq)` (the driver may
    /// also have changed `freq`, adopted by the caller before this
    /// call) and return that value as the count the caller must
    /// re-write to driver channel 0 (`scalerRecord.c:422`).
    /// `None` means the driver left the preset alone — no re-write.
    pub(crate) fn count_start_rewrite_preset(&mut self, save_pr1: u32) -> Option<u32> {
        if save_pr1 != self.pr[0] {
            self.pr[0] = Self::pr1_nint(self.tp, self.freq);
            Some(self.pr[0])
        } else {
            None
        }
    }

    /// Second half of the REQSTART preset reconciliation — C
    /// `scalerRecord.c:424-428`.
    ///
    /// Called after the optional `scalerRecord.c:422` re-write, with
    /// `pr[0]` holding the *final* driver-programmed channel-0 preset.
    /// If that differs from `old_pr1` (the value before the count
    /// start), recompute `tp` from the effective `pr[0]`/`freq`.
    /// `db_post_events` is the monitor layer's job once the field
    /// changed; this only mutates `tp`.
    pub(crate) fn count_start_finalize_tp(&mut self, old_pr1: u32) {
        if old_pr1 != self.pr[0] && self.freq > 0.0 {
            self.tp = self.pr[0] as f64 / self.freq;
        }
    }

    /// Whether counting has completed.
    ///
    /// C parity: `scalerRecord.c:367` `process()` calls `(*pdset->done)`
    /// unconditionally; the *record* never inspects presets itself —
    /// preset/done detection lives entirely in device support (e.g.
    /// `drvScalerSoft.c::checkAcquireDone`). `done_flag` is the
    /// equivalent of the dset `done()` return; the framework's device
    /// support `read()` sets it before `process()` runs.
    fn check_done(&self) -> bool {
        self.done_flag
    }

    /// Build DeviceCommand actions for a count start sequence.
    ///
    /// C parity — `scalerRecord.c:392-432`: the REQSTART block runs
    /// `reset()`, then the per-channel `write_preset` loop, then the
    /// `save_pr1 != pr1` / `old_pr1 != pr1` / `old_freq != freq`
    /// reconciliation, then `arm(1)` — all synchronously inside
    /// `process()`. In the Rust port `process()` returns before any
    /// `DeviceCommand` is dispatched, so the reconciliation cannot be
    /// expressed as separate post-process `write_preset` actions: the
    /// record would never see the driver's adjustment. The whole
    /// write-presets + reconcile + arm sequence is therefore a single
    /// `CMD_START_COUNT` executed by device support, which holds both
    /// the driver and a `&mut ScalerRecord` and reproduces
    /// `scalerRecord.c:408-432` in `handle_command`.
    fn build_start_actions(&self) -> Vec<ProcessAction> {
        vec![
            ProcessAction::DeviceCommand {
                command: CMD_RESET,
                args: vec![],
            },
            ProcessAction::DeviceCommand {
                command: CMD_START_COUNT,
                args: vec![],
            },
        ]
    }

    /// Build DeviceCommand action to disarm.
    fn build_disarm_action() -> ProcessAction {
        ProcessAction::DeviceCommand {
            command: CMD_ARM,
            args: vec![EpicsValue::Long(0)],
        }
    }

    /// Build actions for auto-count start.
    ///
    /// C parity — `scalerRecord.c:508-535`: like REQSTART the
    /// auto-count `reset()` + `write_preset` + reconcile + `arm(1)`
    /// sequence runs synchronously inside `process()`, so it cannot be
    /// split into post-process `write_preset` actions without losing
    /// the `save_pr1 != pr1` driver-adjustment re-write
    /// (`scalerRecord.c:514-522`). It is dispatched as a single
    /// `CMD_AUTOCOUNT` whose `handle_command` reproduces
    /// `scalerRecord.c:510-535`.
    ///
    /// The `D` fields are the record's, not the driver's: C sets them here in
    /// `process()` (`:525`), exactly as it does at REQSTART (`:413`), so the
    /// copy stays with the record and only the preset writes are dispatched.
    fn build_autocount_actions(&mut self) -> Vec<ProcessAction> {
        // C `scalerRecord.c:524-528` — below a millisecond, auto-count falls
        // back on the *user's* per-channel presets, so the gates decide which
        // channels count and the directions follow them:
        //     for (i=0; i<pscal->nch; i++) {
        //         pdir[i] = pgate[i];
        //         if (pgate[i]) (*pdset->write_preset)(pscal, i, ppreset[i]);
        //     }
        // The copy is unconditional; only the preset write is gated. The
        // `tp1 >= 1ms` branch (`:512-523`) programs channel 0 from `tp1*freq`
        // "regardless of any presets the user may have set" (`:506-507`) and
        // never touches `pdir`.
        if self.tp1 < 1.0e-3 {
            self.copy_gates_to_directions();
        }
        let mut actions = vec![
            ProcessAction::DeviceCommand {
                command: CMD_RESET,
                args: vec![],
            },
            ProcessAction::DeviceCommand {
                command: CMD_AUTOCOUNT,
                args: vec![],
            },
        ];
        // C scalerRecord.c:537-538 — once autocount is armed and
        // `ss = SCALER_STATE_COUNTING`, the record schedules the first
        // periodic display update via `callbackRequestDelayed(pupdateCallback,
        // 1.0/rat1)` when `rat1 > .1`. `update_counts()` has already run
        // earlier this process cycle (scalerRecord.c:453) and saw `ss !=
        // COUNTING`, so it could not queue this — emit it here directly.
        if self.rat1 > 0.1 {
            let refresh = self.arm_reentry(
                ReentryKind::DisplayRefresh,
                epics_base_rs::runtime::time::duration_from_secs(1.0 / self.rat1 as f64),
            );
            actions.push(refresh);
        }
        actions
    }
}

/// `S1..S{MAX_SCALER_CHANNELS}` field names, in channel order, for
/// [`ScalerRecord::log_swept_fields`]. C `scalerRecord.c:770-787`
/// `monitor()` sweeps each active channel `S1..Snch` with a literal
/// `DBE_LOG` on every idle process; this static is sliced to `nch` so
/// the record can return the exact set without per-call allocation.
static SN_FIELD_NAMES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    (1..=MAX_SCALER_CHANNELS)
        .map(|i| Box::leak(format!("S{}", i).into_boxed_str()) as &'static str)
        .collect()
});

/// `[D{n}, G{n}]` per channel (0-based), the pair C's `special()` posts when a
/// `PR{n}` write forces the channel on (`scalerRecord.c:702-706`).
static DG_PAIR_BY_CHANNEL: LazyLock<Vec<&'static [&'static str]>> = LazyLock::new(|| {
    (1..=MAX_SCALER_CHANNELS)
        .map(|i| {
            let d: &'static str = Box::leak(format!("D{}", i).into_boxed_str());
            let g: &'static str = Box::leak(format!("G{}", i).into_boxed_str());
            Box::leak(vec![d, g].into_boxed_slice()) as &'static [&'static str]
        })
        .collect()
});

/// `[PR{n}]` per channel (0-based), the preset C's `special()` posts when a
/// `G{n}` write defaults it to 1000 (`scalerRecord.c:715-718`).
static PR_BY_CHANNEL: LazyLock<Vec<&'static [&'static str]>> = LazyLock::new(|| {
    (1..=MAX_SCALER_CHANNELS)
        .map(|i| {
            let pr: &'static str = Box::leak(format!("PR{}", i).into_boxed_str());
            Box::leak(vec![pr].into_boxed_slice()) as &'static [&'static str]
        })
        .collect()
});

/// Every scaler post outside the idle `monitor()` sweep carries a literal
/// `DBE_VALUE` in C: the process/updateCounts posts
/// (scalerRecord.c:316/322/329/334/372/425/427/430/478/530/582/588) and the
/// `special()` posts (`:673-676` PR1/D1/G1, `:682-687` TP/D1/G1, `:692` TP,
/// `:703-705` Dn/Gn, `:716` PRn). `DBE_LOG` appears ONLY in the `monitor()`
/// sweep of `S1..Snch` (line 771, [`SN_FIELD_NAMES`]).
///
/// Returning a field here makes the framework strip the LOG bit from its
/// change post and from its `special()` side-effect post, so a `DBE_LOG`-only
/// subscriber sees `Sn` on the idle sweep alone — and never sees `PRn`/`Gn`/`Dn`
/// at all, which is what C does with them.
///
/// Indexed by `nch`: entry `n` holds the five always-`DBE_VALUE` scalars, `PR1`
/// (posted by process at `:425`/`:427` whatever `nch` is), and the per-channel
/// `Sn`/`PRn`/`Gn`/`Dn` names for the `n` active channels. A table rather than a
/// re-sliced flat static: the four per-channel groups cannot be contiguous for
/// every `nch` at once.
static VALUE_ONLY_BY_NCH: LazyLock<Vec<Vec<&'static str>>> = LazyLock::new(|| {
    (0..=MAX_SCALER_CHANNELS)
        .map(|nch| {
            let mut v: Vec<&'static str> = vec!["CNT", "T", "VAL", "PR1", "TP", "FREQ"];
            v.extend(SN_FIELD_NAMES.iter().take(nch).copied());
            for ch in 0..nch {
                // PR1 is already in the fixed head.
                if ch > 0 {
                    v.push(PR_BY_CHANNEL[ch][0]);
                }
                v.extend_from_slice(DG_PAIR_BY_CHANNEL[ch]);
            }
            v
        })
        .collect()
});

/// Every `db_post_events` C's scaler makes from a PROCESS cycle, enumerated —
/// [`ScalerRecord::process_posted_fields`]. `process()`: `CNT` (:372), `PR1`
/// (:425, only when the count-start recompute moved it), `TP` (:427), `FREQ`
/// (:430, :530), `VAL` (:478), `S1..Snch` (:582), `T` (:588); `monitor()`:
/// `S1..Snch` (:771). That is the whole list — C writes `D1..Dnch` in process
/// (:413-414, :525-526) and posts none of them, and it posts `Gn`/`PRn`(n>1)
/// only from `special()`.
///
/// Indexed by `nch`: the fixed head plus the `Sn` of the active channels, so
/// the `Sn` of a channel the record does not have stays out of the set (C's
/// post loops are bounded by `nch` too).
static PROCESS_POSTED_BY_NCH: LazyLock<Vec<Vec<&'static str>>> = LazyLock::new(|| {
    (0..=MAX_SCALER_CHANNELS)
        .map(|nch| {
            let mut v: Vec<&'static str> = vec!["VAL", "T", "CNT", "PR1", "TP", "FREQ"];
            v.extend(SN_FIELD_NAMES.iter().take(nch).copied());
            v
        })
        .collect()
});

/// C `scalerRecord.c:735` writes the literal `*precision = 2` for `VERS`, the
/// record-support version number.
const SCALER_VERS_PRECISION: i16 = 2;

impl Record for ScalerRecord {
    fn record_type(&self) -> &'static str {
        "scaler"
    }

    /// `scalerRecord.c:728-742` seeds `pscal->prec`, then answers `VERS` with a
    /// literal 2 and every field at or after `VAL` with `pscal->prec` again —
    /// and its `recGblGetPrec` arm only reaches dbCommon, which has no
    /// DBF_DOUBLE field, so the seed survives everywhere else too. That leaves
    /// `VERS` as the sole per-field departure, and it is the one the
    /// record-level PREC cache would otherwise answer wrongly.
    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        field
            .eq_ignore_ascii_case("VERS")
            .then(|| FieldMetadataOverride {
                precision: Some(SCALER_VERS_PRECISION),
                ..Default::default()
            })
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let prev_scaler_state = self.ss;
        // C fires the forward link (or does not) exactly once per
        // process() cycle; every path that leaves process() without
        // reaching `scalerRecord.c:480` leaves the link unfired.
        self.fire_fwd_link = false;

        // C `updateCallbackFunc` (scalerRecord.c:203-214) calls `updateCounts`
        // and nothing else, so a periodic display refresh never reaches
        // `recGblFwdLink` (:480). `updateCounts` in turn refuses the call
        // outright when it did not come from `process()` and the count is over
        // (:562-568):
        //
        //     called_by_process = (pscal->pact == TRUE);
        //     if (!called_by_process) {
        //         if (pscal->ss != SCALER_STATE_IDLE) pscal->pact = TRUE;
        //         else return;
        //     }
        //
        // The port's refresh is a `ReprocessAfter` re-entry — a whole
        // `process()` — so without that gate a refresh armed while counting
        // lands after the count has stopped, reaches the "done counting?" block
        // with `ss`/`pcnt`/`us` all clear, and fires FLNK a second time. The
        // refresh still runs the full cycle while `ss != IDLE`: unlike C, which
        // is driven to `process()` by `deviceCallbackFunc` on completion, this
        // port polls `done()` from the process cycle, and the refresh IS that
        // poll while a count is live.
        let reentry = if std::mem::take(&mut self.continuation) {
            self.pending_reentry.take()
        } else {
            None
        };
        if reentry == Some(ReentryKind::DisplayRefresh) && self.ss == SCALER_STATE_IDLE {
            return Ok(ProcessOutcome::complete_with(Vec::new()));
        }

        let mut just_finished_user_count = false;
        let mut just_started_user_count = false;
        let mut actions = Vec::new();

        // C scalerRecord.c:346 — dbPutNotify completions also force the
        // long autocount hold time. The port has no putNotify plumbing
        // yet, so this stays false; kept named for C-structure parity.
        let put_notify_operation = false;

        // C scalerRecord.c:367 — the record asks device support whether
        // counting has finished (`(*pdset->done)(pscal)`) UNCONDITIONALLY,
        // every process cycle. `done_flag` is the dset `done()` return,
        // set by device support's read() before process() runs.
        if self.check_done() {
            self.done_flag = false;
            self.ss = SCALER_STATE_IDLE;
            // C:370 — an auto-count cycle is NOT allowed to reset CNT.
            if self.us == USER_STATE_COUNTING {
                self.cnt = 0;
                self.us = USER_STATE_IDLE;
                just_finished_user_count = true;
            }
        }

        // C `delayCallbackFunc` (scalerRecord.c:216-231), the body of the
        // watchdog `special(CNT)` armed:
        //
        //     if (pscal->us == USER_STATE_WAITING && pscal->cnt) {
        //         pscal->us = USER_STATE_REQSTART;
        //         (void)scanOnce((void *)pscal);
        //     }
        //
        // The timer that brings us here is armed in `special()` as a
        // `ProcessAction::ReprocessAfter` — the single owner of the delayed
        // start, exactly as `pdelayCallback` is in C. So this runs the
        // callback's state transition and nothing else: a process cycle that
        // lands MID-wait (a periodic scan, a device-done interrupt) falls
        // straight through, as it does in C, where such a cycle finds
        // `us == WAITING` outside `(REQSTART || WAITING) && cnt`'s start arm
        // (:381-382) and just runs `updateCounts` (:453). It must NOT
        // re-schedule the wait: a second timer would double-process at expiry.
        if self.us == USER_STATE_WAITING && self.cnt != 0 {
            if let Some(delay) = self.count_delay {
                if delay.expired() {
                    self.us = USER_STATE_REQSTART;
                    self.count_delay = None;
                }
            }
        }

        // Handle CNT state change
        if self.cnt != self.pcnt {
            let mut handled = false;
            if self.cnt != 0 && (self.us == USER_STATE_REQSTART || self.us == USER_STATE_WAITING) {
                // Stop any existing auto-count via DeviceCommand
                if self.ss == SCALER_STATE_COUNTING {
                    actions.push(Self::build_disarm_action());
                    self.ss = SCALER_STATE_IDLE;
                }

                if self.us == USER_STATE_REQSTART {
                    // C scalerRecord.c:406 — capture old_pr1 BEFORE the
                    // :409-410 self-consistency guard. The guard itself
                    // may change pr[0] (user wrote TP and frac(tp*freq)
                    // >= 0.5); C's :424 `old_pr1 != pr1` TP-recompute
                    // must see that pre-guard baseline, so the count-start
                    // reconciliation in run_start_count reads this field
                    // rather than re-capturing pr[0] post-guard.
                    self.reqstart_old_pr1 = self.pr[0];

                    // Ensure channel-1 preset count agrees with time
                    // preset and freq. C scalerRecord.c:409-410 uses
                    // NINT (round-to-nearest), unlike init_record/special.
                    let expected_pr1 = Self::pr1_nint(self.tp, self.freq);
                    if self.pr[0] != expected_pr1 {
                        self.pr[0] = expected_pr1;
                    }

                    // Set directions from gates (C `scalerRecord.c:413-414`)
                    self.copy_gates_to_directions();

                    // Queue reset → write_presets → arm via DeviceCommands
                    actions.extend(self.build_start_actions());
                    self.ss = SCALER_STATE_COUNTING;
                    self.us = USER_STATE_COUNTING;
                    just_started_user_count = true;
                    handled = true;
                }
            } else if self.cnt == 0 {
                if self.ss != SCALER_STATE_IDLE {
                    actions.push(Self::build_disarm_action());
                }
                self.ss = SCALER_STATE_IDLE;
                self.us = USER_STATE_IDLE;
                just_finished_user_count = true;
                handled = true;
            }
            if handled {
                self.pcnt = self.cnt;
            }
        }

        // C scalerRecord.c:453 — read and display scalers. updateCounts()
        // zeroes the display while us==WAITING, recomputes T from S1/FREQ,
        // and (while counting) schedules the next periodic update.
        if let Some(reprocess) = self.update_counts() {
            let refresh = self.arm_reentry(ReentryKind::DisplayRefresh, reprocess);
            actions.push(refresh);
        }

        // C scalerRecord.c:455-468 — COUT on either edge, then a SECOND COUTP put
        // on the finish edge, `dbPutLink(&pscal->coutp, ...)` at :463. C does not
        // coalesce it with special()'s put at :624: a user stop (CNT 1->0) runs
        // special() — which puts 0 to COUTP — and then process(), which puts 0 to
        // COUTP again, so the link is written TWICE and a record wired to it is
        // processed twice. A user start reaches only special()'s put, because
        // :463 is guarded by justFinishedUserCount.
        if just_started_user_count || just_finished_user_count {
            actions.push(ProcessAction::WriteDbLink {
                link_field: "COUT",
                value: EpicsValue::Short(self.cnt),
            });
            if just_finished_user_count {
                actions.push(ProcessAction::WriteDbLink {
                    link_field: "COUTP",
                    value: EpicsValue::Short(self.cnt),
                });
            }
        }

        // C scalerRecord.c:470-481 — "done counting?": while ss==IDLE,
        // VAL takes T if we just left COUNTING, and `recGblFwdLink()`
        // fires. Both are decided HERE, before the auto-count block
        // below re-arms `ss`.
        if self.ss == SCALER_STATE_IDLE && self.pcnt == 0 && self.us == USER_STATE_IDLE {
            if prev_scaler_state == SCALER_STATE_COUNTING {
                self.val = self.t;
            }
            self.fire_fwd_link = true;
        }

        // AutoCount — C scalerRecord.c:484-541.
        if self.us == USER_STATE_IDLE && self.cont != 0 && self.ss != SCALER_STATE_COUNTING {
            // C:487-490 — `dly_sec = dly1`, but after a user count or a
            // dbPutNotify operation the hold time is `MAX(dly1,
            // scaler_wait_time)` so the scalers are not wiped immediately.
            let mut dly_sec = self.dly1.max(0.0) as f64;
            if just_finished_user_count || put_notify_operation {
                dly_sec = dly_sec.max(SCALER_WAIT_TIME);
            }
            // C:492 — `if (dly_sec > 0 && ss != WAITING)`: schedule the
            // restart. Otherwise (delay elapsed, or no delay) start now.
            if dly_sec > 0.0 && self.ss != SCALER_STATE_WAITING {
                self.ss = SCALER_STATE_WAITING;
                self.delay_start = Some(Instant::now());
                self.autocount_delay = dly_sec;
                let auto = self.arm_reentry(
                    ReentryKind::Process,
                    epics_base_rs::runtime::time::duration_from_secs(dly_sec),
                );
                actions.push(auto);
                return Ok(ProcessOutcome::complete_with(actions));
            } else if self.ss == SCALER_STATE_WAITING {
                // Already WAITING: only start once the scheduled delay
                // has actually elapsed (guards a premature reprocess).
                let elapsed = self
                    .delay_start
                    .map(|s| s.elapsed().as_secs_f64())
                    .unwrap_or(f64::MAX);
                if elapsed >= self.autocount_delay {
                    self.delay_start = None;
                    actions.extend(self.build_autocount_actions());
                    self.ss = SCALER_STATE_COUNTING;
                } else {
                    let remaining = self.autocount_delay - elapsed;
                    let auto = self.arm_reentry(
                        ReentryKind::Process,
                        epics_base_rs::runtime::time::duration_from_secs(remaining),
                    );
                    actions.push(auto);
                    return Ok(ProcessOutcome::complete_with(actions));
                }
            } else {
                // dly_sec <= 0 and not WAITING: start immediately.
                actions.extend(self.build_autocount_actions());
                self.ss = SCALER_STATE_COUNTING;
            }
        }

        Ok(ProcessOutcome::complete_with(actions))
    }

    /// C's `special()` COUTP put (`scalerRecord.c:623-624`), handed to the
    /// framework to execute where C executes it: inside the put, before the
    /// CNT-triggered process cycle. The queue is filled by `special()` and
    /// emptied here — the framework drains it on every put, so a put that queues
    /// nothing hands back nothing.
    fn take_special_actions(&mut self) -> Vec<ProcessAction> {
        std::mem::take(&mut self.special_actions)
    }

    /// C hands each delayed callback to a function of its own; the framework
    /// has one re-entry shape, so it reports whether this cycle IS that
    /// re-entry and `process()` pairs the answer with `pending_reentry` to
    /// recover which callback fired.
    fn set_process_continuation(&mut self, continuation: bool) {
        self.continuation = continuation;
    }

    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after {
            // The framework runs this pre-pass on every put (field_io.rs:937),
            // so it is the one point that retires the previous put's post list.
            // Each `after` arm below then records the db_post_events C's matching
            // case makes; an arm that records nothing leaves the list empty,
            // which is C's "this case posts nothing".
            self.side_effect_posts = &[];
            return Ok(());
        }
        match field {
            // C scalerRecord.c:620-662 — CNT (SPC_MOD).
            "CNT" => {
                // C:622 — ignore redundant Count requests while a count
                // is already in progress.
                if self.cnt != 0 && self.us != USER_STATE_IDLE {
                    return Ok(());
                }
                // C:623-624 — fire the COUTP link on every CNT write that
                // passes the redundant-command guard. C makes this put from
                // `special()` itself, i.e. inside `dbPut`: the target is written
                // and processed with `us` still IDLE and the count not yet
                // armed. The framework drains `take_special_actions()` at the
                // end of the put and executes it there.
                self.special_actions.push(ProcessAction::WriteDbLink {
                    link_field: "COUTP",
                    value: EpicsValue::Short(self.cnt),
                });
                // C:633-634 — `dly = pscal->dly; if (dly<0.0) dly = 0.0;`
                let dly = self.dly.max(0.0);
                // C:635 — `if (dly == 0.0 || pscal->cnt == 0)`: handle now.
                if dly == 0.0 || self.cnt == 0 {
                    if self.cnt != 0 {
                        // C:637-639 — start counting.
                        self.us = USER_STATE_REQSTART;
                    } else {
                        // C:641-653 — abort any counting / start request.
                        match self.us {
                            USER_STATE_WAITING => {
                                // C:643-647 — `if (pdelayCallback->timer)
                                // epicsTimerCancel(...)`, the first statement of
                                // the arm. C's own callback is a two-line guarded
                                // transition, so a raced one costs nothing; the
                                // port's is a whole `process()`, which reaches the
                                // "done counting?" block and fires FLNK a second
                                // time DLY seconds after the user stopped the
                                // count. Cancelling advances the re-entry
                                // generation, so the armed timer is gone rather
                                // than merely harmless.
                                self.special_actions.push(ProcessAction::CancelReprocess);
                                self.count_delay = None;
                                self.us = USER_STATE_IDLE;
                            }
                            USER_STATE_REQSTART => {
                                self.us = USER_STATE_IDLE;
                            }
                            _ => {}
                        }
                    }
                    // C:655 — `if (pscal->scan) scanOnce((void *)pscal);`,
                    // the last statement of the handle-it-now arm. The state
                    // change above (REQSTART / abort) is acted on by
                    // `process()`, and a non-Passive scaler gets no process
                    // from this put — without the scan-once it would sit on
                    // the new state until the next periodic scan. The
                    // `if (pscal->scan)` half of the guard belongs to the
                    // framework (see `ProcessAction::ScanOnce`), which drops
                    // the action for a Passive record whose put processes it
                    // anyway. Not emitted in the delayed arm below: C does not
                    // call `scanOnce` there either — `delayCallbackFunc` does,
                    // when the delay expires.
                    self.special_actions.push(ProcessAction::ScanOnce);
                } else {
                    // C:657-661 — schedule the delayed start callback:
                    // `pscal->us = USER_STATE_WAITING;
                    //  callbackRequestDelayed(pdelayCallback, pscal->dly);`
                    //
                    // The timer is the ONLY thing that starts the count. C's
                    // `delayCallbackFunc` (:216-231) calls `scanOnce`
                    // UNCONDITIONALLY on expiry — no `if (pscal->scan)` guard,
                    // unlike the handle-it-now arm above — so a Passive scaler
                    // AND a periodically-scanned one both start counting
                    // exactly DLY seconds after the CNT write. Without this
                    // action the port armed nothing: a non-Passive scaler got
                    // no process from the put and sat in WAITING until its next
                    // periodic scan (SCAN=I/O Intr / Event: forever).
                    self.us = USER_STATE_WAITING;
                    let secs = dly as f64;
                    self.count_delay = Some(CountDelay {
                        start: Instant::now(),
                        secs,
                    });
                    let start = self.arm_reentry(
                        ReentryKind::Process,
                        epics_base_rs::runtime::time::duration_from_secs(secs),
                    );
                    self.special_actions.push(start);
                }
            }
            // C scalerRecord.c:664-668 — CONT. The write changes auto-count
            // mode, which only `process()` acts on, so C rescans the record:
            // `if (pscal->scan) scanOnce((void *)pscal);` (:667). A Passive
            // scaler is processed by the put itself (CONT is `pp(TRUE)`) and the
            // framework drops the action for it — same gate C writes inline.
            "CONT" => {
                self.special_actions.push(ProcessAction::ScanOnce);
            }
            // C scalerRecord.c:670-677 — TP (truncating tp->pr1, d1=g1=1).
            "TP" => {
                self.tp_to_pr1();
                // C:673-676 — PR1, D1 and G1 are posted unconditionally.
                self.side_effect_posts = &["PR1", "D1", "G1"];
            }
            // C has NO special() case for TP1 or RAT1 — leave unchanged.
            "TP1" => {}
            // C scalerRecord.c:690-693 — RATE clamped to [0, 60].
            "RATE" => {
                self.rate = self.rate.clamp(0.0, 60.0);
                // DEVIATION from C, deliberate — CBUG-B18. C's case is
                // `pscal->rate = MIN(60.,MAX(0.,pscal->rate));
                //  db_post_events(pscal,&(pscal->tp),DBE_VALUE);` — the clamp
                // writes RATE, the post passes `&pscal->tp`, a field this write
                // never touched. A slip, not a convention: it is a copy-paste of
                // the TP case's post two cases up, and every `special()` post in
                // this file exists to announce the OTHER fields a case changed
                // (`:672-676` TP→PR1/D1/G1, `:681-686`, `:703-706`, `:717-719`).
                // The clamp changes RATE and nothing else, and the written field
                // is already posted by `dbPut` itself — `db_post_events(precord,
                // pfieldsave, DBE_VALUE|DBE_LOG)` at dbAccess.c:1455-1459, which
                // runs AFTER `dbPutSpecial(paddr, 1)` and so carries the CLAMPED
                // value. So the correct side-effect list is empty, and C's post
                // is pure noise: a no-change TP event on every RATE write.
                self.side_effect_posts = &[];
            }
            _ => {
                if field == "PR1" {
                    self.pr1_to_tp();
                    if self.tp > 0.0 {
                        self.d[0] = 1;
                        self.g[0] = 1;
                        // C:682-687 — TP always, D1/G1 only when TP came out > 0.
                        self.side_effect_posts = &["TP", "D1", "G1"];
                    } else {
                        self.side_effect_posts = &["TP"];
                    }
                } else if let Some(i) = parse_indexed_field(field, "PR") {
                    if self.pr[i] > 0 {
                        self.d[i] = 1;
                        self.g[i] = 1;
                        // C:703-705 — the forced-on channel's Dn and Gn.
                        self.side_effect_posts = DG_PAIR_BY_CHANNEL[i];
                    }
                } else if let Some(i) = parse_indexed_field(field, "G") {
                    if self.g[i] != 0 && self.pr[i] == 0 {
                        self.pr[i] = 1000;
                        // C:716-717 — the preset this write just defaulted.
                        self.side_effect_posts = PR_BY_CHANNEL[i];
                    }
                }
            }
        }
        Ok(())
    }

    fn should_fire_forward_link(&self) -> bool {
        // Report the decision `process()` captured at C's
        // `recGblFwdLink` line; do NOT re-evaluate `ss`/`us`/`pcnt`
        // here — under CONT=AutoCount the auto-count block has already
        // moved `ss` to WAITING/COUNTING by the time the framework asks.
        self.fire_fwd_link
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => return Some(EpicsValue::Double(self.val)),
            "FREQ" => return Some(EpicsValue::Double(self.freq)),
            "CNT" => return Some(EpicsValue::Short(self.cnt)),
            "PCNT" => return Some(EpicsValue::Short(self.pcnt)),
            "SS" => return Some(EpicsValue::Short(self.ss)),
            "US" => return Some(EpicsValue::Short(self.us)),
            "CONT" => return Some(EpicsValue::Short(self.cont)),
            "RATE" => return Some(EpicsValue::Float(self.rate)),
            "RAT1" => return Some(EpicsValue::Float(self.rat1)),
            "DLY" => return Some(EpicsValue::Float(self.dly)),
            "DLY1" => return Some(EpicsValue::Float(self.dly1)),
            "NCH" => return Some(EpicsValue::Short(self.nch)),
            "TP" => return Some(EpicsValue::Double(self.tp)),
            "TP1" => return Some(EpicsValue::Double(self.tp1)),
            "T" => return Some(EpicsValue::Double(self.t)),
            "VERS" => return Some(EpicsValue::Float(self.vers)),
            "PREC" => return Some(EpicsValue::Short(self.prec)),
            "EGU" => return Some(EpicsValue::String(self.egu.clone())),
            "OUT" => return Some(EpicsValue::String(self.out.clone().into())),
            "COUT" => return Some(EpicsValue::String(self.cout.clone().into())),
            "COUTP" => return Some(EpicsValue::String(self.coutp.clone().into())),
            _ => {}
        }
        if let Some(i) = parse_indexed_field(name, "NM") {
            return Some(EpicsValue::String(self.nm[i].clone()));
        }
        if let Some(i) = parse_indexed_field(name, "PR") {
            // PR1..PR64 are DBF_ULONG (scalerRecord.dbd:945-1323).
            return Some(EpicsValue::ULong(self.pr[i]));
        }
        if let Some(i) = parse_indexed_field(name, "S") {
            // S1..S64 are DBF_ULONG (scalerRecord.dbd:1334-1649).
            return Some(EpicsValue::ULong(self.s[i]));
        }
        if let Some(i) = parse_indexed_field(name, "G") {
            return Some(EpicsValue::Short(self.g[i]));
        }
        if let Some(i) = parse_indexed_field(name, "D") {
            return Some(EpicsValue::Short(self.d[i]));
        }
        None
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::Double(v) => {
                    self.val = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "FREQ" => match value {
                EpicsValue::Double(v) => {
                    self.freq = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "CNT" => match value {
                EpicsValue::Short(v) => {
                    self.cnt = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "CONT" => match value {
                EpicsValue::Short(v) => {
                    self.cont = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "RATE" => match value {
                EpicsValue::Float(v) => {
                    self.rate = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "RAT1" => match value {
                EpicsValue::Float(v) => {
                    self.rat1 = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DLY" => match value {
                EpicsValue::Float(v) => {
                    self.dly = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "DLY1" => match value {
                EpicsValue::Float(v) => {
                    self.dly1 = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "TP" => match value {
                EpicsValue::Double(v) => {
                    self.tp = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "TP1" => match value {
                EpicsValue::Double(v) => {
                    self.tp1 = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "PREC" => match value {
                EpicsValue::Short(v) => {
                    self.prec = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "EGU" => match value {
                EpicsValue::String(v) => {
                    self.egu = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OUT" => match value {
                EpicsValue::String(v) => {
                    self.out = v.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "COUT" => match value {
                EpicsValue::String(v) => {
                    self.cout = v.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "COUTP" => match value {
                EpicsValue::String(v) => {
                    self.coutp = v.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "PCNT" | "SS" | "US" | "NCH" | "T" | "VERS" => Err(CaError::ReadOnlyField(name.into())),
            _ => {
                if let Some(i) = parse_indexed_field(name, "NM") {
                    match value {
                        EpicsValue::String(v) => {
                            self.nm[i] = v;
                            Ok(())
                        }
                        _ => Err(CaError::TypeMismatch(name.into())),
                    }
                } else if let Some(i) = parse_indexed_field(name, "PR") {
                    // PR1..PR64 are DBF_ULONG (scalerRecord.dbd:945-1323):
                    // accept the native ULong and tolerate the legacy signed
                    // Long carrier (the reinterpret preserves the bit pattern).
                    match value {
                        EpicsValue::ULong(v) => {
                            self.pr[i] = v;
                            Ok(())
                        }
                        EpicsValue::Long(v) => {
                            self.pr[i] = v as u32;
                            Ok(())
                        }
                        _ => Err(CaError::TypeMismatch(name.into())),
                    }
                } else if let Some(i) = parse_indexed_field(name, "G") {
                    match value {
                        EpicsValue::Short(v) => {
                            self.g[i] = v;
                            Ok(())
                        }
                        _ => Err(CaError::TypeMismatch(name.into())),
                    }
                } else if let Some(i) = parse_indexed_field(name, "D") {
                    match value {
                        EpicsValue::Short(v) => {
                            self.d[i] = v;
                            Ok(())
                        }
                        _ => Err(CaError::TypeMismatch(name.into())),
                    }
                } else if parse_indexed_field(name, "S").is_some() {
                    Err(CaError::ReadOnlyField(name.into()))
                } else {
                    Err(CaError::FieldNotFound(name.into()))
                }
            }
        }
    }

    fn put_field_internal(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        // Allow framework to write S1-S64 (read-only) from device support read.
        // S1..S64 are DBF_ULONG (scalerRecord.dbd:1334-1649): accept the
        // native ULong and tolerate the legacy signed Long carrier.
        if let Some(i) = parse_indexed_field(name, "S") {
            match value {
                EpicsValue::ULong(v) => {
                    self.s[i] = v;
                    Ok(())
                }
                EpicsValue::Long(v) => {
                    self.s[i] = v as u32;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            }
        } else {
            self.put_field(name, value)
        }
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        dbd_generated::SCALER_FIELDS
    }

    fn declared_noaccess_fields(&self) -> &'static [&'static str] {
        dbd_generated::SCALER_NOACCESS
    }

    /// C `scalerRecord.c:471,770-787`: `process()` calls `monitor()` only
    /// while `ss == SCALER_STATE_IDLE`, and `monitor()` re-posts every
    /// active channel `S1..Snch` with a literal `DBE_LOG` regardless of
    /// change — so an archiver `camonitor SCALER:Sn` receives an event on
    /// every idle scan, even when the count is unchanged. A counting or
    /// WAITING cycle does not call `monitor()`, so the sweep is empty
    /// there.
    ///
    /// The sweep is INDEPENDENT of the change post, and the
    /// count-completion cycle is where that matters: the done-interrupt
    /// sets `ss = IDLE` (`:367`), `updateCounts()` posts each changed `Sn`
    /// with `DBE_VALUE` (`:582`), and `monitor()` then posts the SAME `Sn`
    /// with `DBE_LOG` (`:771`). Both events fire on that one cycle, so the
    /// framework emits the sweep post in addition to the change post — the
    /// final counts are the only `Sn` value a `DBE_LOG`-only archiver ever
    /// cares about.
    ///
    /// The framework posts this sweep with `DBE_LOG | <alarm-transition bits>`.
    /// DEVIATION from C, deliberate — CBUG-B19: C's `monitor()` computes
    /// `monitor_mask = recGblResetAlarms(pscal)` (`:764`), ORs `DBE_VALUE|DBE_LOG`
    /// into it (`:766`), and then posts with a literal `DBE_LOG` (`:771`) —
    /// `monitor_mask` is never read, so the alarm bit is dropped and a client
    /// subscribed to `Sn` with `DBE_ALARM` receives nothing on a severity
    /// transition. Slicing the static `SN_FIELD_NAMES` to `nch`
    /// avoids a per-call allocation; the unsafe `'static` re-view matches
    /// `field_list` above (the `LazyLock` lives for the program and
    /// `active_channels() <= SN_FIELD_NAMES.len()`).
    fn log_swept_fields(&self) -> &'static [&'static str] {
        if self.ss == SCALER_STATE_IDLE {
            let names: &Vec<&'static str> = &SN_FIELD_NAMES;
            unsafe { std::slice::from_raw_parts(names.as_ptr(), self.active_channels()) }
        } else {
            &[]
        }
    }

    /// Every C scaler post outside the idle `monitor()` sweep is a literal
    /// `DBE_VALUE` — see `VALUE_ONLY_BY_NCH`, which lists them. The framework
    /// strips the LOG bit from the change post (and from `VAL`'s deadband post,
    /// and from the `special()` side-effect posts in
    /// [`Self::monitor_side_effect_fields`]) of every field returned here, so a
    /// `DBE_LOG`-only subscriber sees `Sn` on the idle sweep alone and never
    /// sees `PRn`/`Gn`/`Dn`, matching C.
    fn value_only_change_fields(&self) -> &'static [&'static str] {
        &VALUE_ONLY_BY_NCH[self.active_channels()]
    }

    /// C's scaler posts a FIXED list from a process cycle (see
    /// `PROCESS_POSTED_BY_NCH`) and leaves every other field it wrote silent.
    /// Declaring that list closes the spurious-event family the framework's
    /// generic "post whatever changed" rule opened:
    ///
    /// * `D1..Dnch` — `process()` copies the gates into them on every count
    ///   start (`scalerRecord.c:413-414`, `:525-526`) and posts NOTHING; C's
    ///   only `Dn` posts are in `special()` (`:675`, `:685`, `:704`). The port
    ///   change-detected the copy and fired a `Dn` monitor C never sends.
    ///
    /// `G1..Gnch` and `PR2..PRnch` stay outside the set for the same C reason —
    /// `process()` posts no `Gn` at all, and `PRn` only for n = 1 — NOT to
    /// compensate for a framework defect: the put-time double post (R11-C10,
    /// `last_posted` not advanced by the put's own post) is fixed at the
    /// framework, in `RecordInstance::notify_field_with_origin`.
    ///
    /// `PR1` stays in the set: C's `process()` does post it (`:425`), when the
    /// count-start preset recompute moved it.
    fn process_posted_fields(&self) -> Option<&'static [&'static str]> {
        Some(&PROCESS_POSTED_BY_NCH[self.active_channels()])
    }

    /// The `db_post_events` calls C's `special()` makes inline, for the put that
    /// just ran. `special()` is their single owner: it knows which posts each
    /// case made and records them; this hook only hands the list to the
    /// framework, which posts each one. The list cannot be re-derived from record
    /// state here — C posts `PRn` on a `Gn` write only when that write is what
    /// defaulted it to 1000, which the post-put state no longer distinguishes.
    ///
    /// These lists carry the OTHER fields a case changed — the written field is
    /// posted by the put itself (C `dbPut`, dbAccess.c:1455-1459). So `RATE`,
    /// whose clamp changes only `RATE`, contributes nothing; C posts an untouched
    /// `TP` there, which is CBUG-B18 (see the deviation note at that case).
    fn monitor_side_effect_fields(&self, _put_field: &str) -> &'static [&'static str] {
        self.side_effect_posts
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            self.vers = VERSION;
            return Ok(());
        }
        if self.freq == 0.0 {
            self.freq = 1.0e7;
        }
        // C scalerRecord.c:320-336: default count time when both TP and
        // PR1 are zero, then convert between time and clock ticks. The
        // cast is truncating (`(epicsUInt32)`), not NINT.
        if self.tp == 0.0 && self.pr[0] == 0 {
            self.tp = 1.0;
        }
        if self.tp != 0.0 {
            self.pr[0] = Self::pr1_trunc(self.tp, self.freq);
        } else if self.pr[0] > 0 && self.freq > 0.0 {
            self.tp = self.pr[0] as f64 / self.freq;
        }
        Ok(())
    }
}

#[cfg(test)]
mod menu_choice_tests {
    use super::ScalerRecord;
    use epics_base_rs::server::record::{Record, RecordInstance};
    use epics_base_rs::types::EpicsValue;

    // CONT is menu(scalerCONT) served as Short; the base snapshot path
    // promotes it to DBR_ENUM and attaches the wire-visible labels.
    #[test]
    fn scaler_cont_snapshot_is_enum_with_labels() {
        let mut rec = ScalerRecord::default();
        rec.put_field("CONT", EpicsValue::Short(1)).unwrap();
        let inst = RecordInstance::new("SC:CONT".into(), rec);

        let snap = inst.snapshot_for_field("CONT").unwrap();
        assert_eq!(snap.value, EpicsValue::Enum(1));
        assert_eq!(
            snap.enums.as_ref().unwrap().strings,
            vec!["OneShot", "AutoCount"]
        );
    }

    /// The choices a client sees are the DECLARATION's — `scalerRecord.dbd`'s
    /// `menu()` on each field. This used to assert them through
    /// `Record::menu_field_choices`, a hand-written table that declared the
    /// same menus a second time.
    #[test]
    fn scaler_menu_choices_come_from_the_declaration() {
        use epics_base_rs::server::record::FieldDeclaration;
        let rec = ScalerRecord::default();
        let menu = |name: &str| {
            rec.field_list()
                .iter()
                .find(|f| f.name == name)
                .unwrap_or_else(|| panic!("{name} is declared"))
                .menu
        };
        assert_eq!(menu("CNT"), Some(&["Done", "Count"][..]));
        assert_eq!(menu("PCNT"), Some(&["Done", "Count"][..]));
        assert_eq!(menu("VAL"), None);
    }

    /// `D1..D64` are `menu(scalerD1)` (Up/Dn) and `G1..G64` are `menu(scalerG1)`
    /// (N/Y) — declared field by field in the `.dbd`, so the whole indexed range
    /// carries its menu and the `D`-prefixed non-menu fields (`DLY`, `DLY1`)
    /// cannot falsely match: they are separate declarations, not a prefix rule.
    /// The hand-written table resolved these by parsing the field NAME, which is
    /// why it needed a guard against `DLY`/`DLY1` at all.
    #[test]
    fn scaler_indexed_menu_choices_come_from_the_declaration() {
        use epics_base_rs::server::record::FieldDeclaration;
        let rec = ScalerRecord::default();
        let menu = |name: &str| {
            rec.field_list()
                .iter()
                .find(|f| f.name == name)
                .map(|f| f.menu)
        };
        for i in 1..=super::MAX_SCALER_CHANNELS {
            assert_eq!(
                menu(&format!("D{i}")),
                Some(Some(&["Up", "Dn"][..])),
                "D{i} must serve menu(scalerD1)"
            );
            assert_eq!(
                menu(&format!("G{i}")),
                Some(Some(&["N", "Y"][..])),
                "G{i} must serve menu(scalerG1)"
            );
        }
        // 'D'-prefixed non-indexed fields are declared, and carry no menu.
        assert_eq!(menu("DLY"), Some(None));
        assert_eq!(menu("DLY1"), Some(None));
        // Out-of-range indices are not fields at all.
        assert_eq!(menu("D0"), None);
        assert_eq!(menu("D65"), None);
        assert_eq!(menu("G65"), None);
    }

    // A G{n} field served as Short is promoted to DBR_ENUM by the base
    // snapshot path, carrying the wire-visible N/Y labels.
    #[test]
    fn scaler_gate_snapshot_is_enum_with_labels() {
        let mut rec = ScalerRecord::default();
        rec.put_field("G3", EpicsValue::Short(1)).unwrap();
        let inst = RecordInstance::new("SC:G3".into(), rec);
        let snap = inst.snapshot_for_field("G3").unwrap();
        assert_eq!(snap.value, EpicsValue::Enum(1));
        assert_eq!(snap.enums.as_ref().unwrap().strings, vec!["N", "Y"]);
    }
}
