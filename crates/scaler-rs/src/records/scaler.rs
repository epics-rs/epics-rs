use std::any::Any;
use std::time::Instant;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::record::{FieldDesc, ProcessAction, ProcessOutcome, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

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
    pub egu: String,
    pub out: String,
    pub cout: String,
    pub coutp: String,

    // --- Per-channel arrays (64 channels) ---
    pub d: [i16; MAX_SCALER_CHANNELS],
    pub g: [i16; MAX_SCALER_CHANNELS],
    pub pr: [u32; MAX_SCALER_CHANNELS],
    pub s: [u32; MAX_SCALER_CHANNELS],
    pub nm: [String; MAX_SCALER_CHANNELS],

    // --- Delay tracking ---
    delay_start: Option<Instant>,
    /// The autocount hold time (seconds) the current SCALER_STATE_WAITING
    /// period was scheduled with. C scalerRecord.c computes `dly_sec`
    /// once (`MAX(dly1, scaler_wait_time)` after a user count) and the
    /// `autoCallbackFunc` fires after exactly that interval; the port
    /// must compare elapsed time against the scheduled value, not the
    /// live `dly1` (which the user may change mid-wait).
    autocount_delay: f64,

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

    /// Set by `special("CNT")` to request a `COUTP` link fire. C
    /// `scalerRecord.c:623-624` calls `dbPutLink(&pscal->coutp, ...)`
    /// inside `special()` itself, before the CNT-triggered `scanOnce()`.
    /// `special()` here cannot emit `ProcessAction`s, so it raises this
    /// flag and the CNT-triggered `process()` emits the `WriteDbLink`
    /// and clears it.
    coutp_pending: bool,
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
            egu: String::new(),
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
            nm: std::array::from_fn(|_| String::new()),
            delay_start: None,
            autocount_delay: 0.0,
            done_flag: false,
            reqstart_old_pr1: 0,
            coutp_pending: false,
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
                return Some(std::time::Duration::from_secs_f64(1.0 / rate as f64));
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
    fn build_autocount_actions(&self) -> Vec<ProcessAction> {
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
            actions.push(ProcessAction::ReprocessAfter(
                std::time::Duration::from_secs_f64(1.0 / self.rat1 as f64),
            ));
        }
        actions
    }
}

// Full FIELDS including indexed fields
use std::sync::LazyLock;

static ALL_FIELDS: LazyLock<Vec<FieldDesc>> = LazyLock::new(|| {
    let mut fields = vec![
        FieldDesc {
            name: "VAL",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "FREQ",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "CNT",
            dbf_type: DbFieldType::Short,
            read_only: false,
        },
        FieldDesc {
            name: "PCNT",
            dbf_type: DbFieldType::Short,
            read_only: true,
        },
        FieldDesc {
            name: "SS",
            dbf_type: DbFieldType::Short,
            read_only: true,
        },
        FieldDesc {
            name: "US",
            dbf_type: DbFieldType::Short,
            read_only: true,
        },
        FieldDesc {
            name: "CONT",
            dbf_type: DbFieldType::Short,
            read_only: false,
        },
        FieldDesc {
            name: "RATE",
            dbf_type: DbFieldType::Float,
            read_only: false,
        },
        FieldDesc {
            name: "RAT1",
            dbf_type: DbFieldType::Float,
            read_only: false,
        },
        FieldDesc {
            name: "DLY",
            dbf_type: DbFieldType::Float,
            read_only: false,
        },
        FieldDesc {
            name: "DLY1",
            dbf_type: DbFieldType::Float,
            read_only: false,
        },
        FieldDesc {
            name: "NCH",
            dbf_type: DbFieldType::Short,
            read_only: true,
        },
        FieldDesc {
            name: "TP",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "TP1",
            dbf_type: DbFieldType::Double,
            read_only: false,
        },
        FieldDesc {
            name: "T",
            dbf_type: DbFieldType::Double,
            read_only: true,
        },
        FieldDesc {
            name: "VERS",
            dbf_type: DbFieldType::Float,
            read_only: true,
        },
        FieldDesc {
            name: "PREC",
            dbf_type: DbFieldType::Short,
            read_only: false,
        },
        FieldDesc {
            name: "EGU",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "OUT",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "COUT",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
        FieldDesc {
            name: "COUTP",
            dbf_type: DbFieldType::String,
            read_only: false,
        },
    ];
    for i in 1..=MAX_SCALER_CHANNELS {
        let s: &'static str = Box::leak(format!("S{}", i).into_boxed_str());
        fields.push(FieldDesc {
            name: s,
            dbf_type: DbFieldType::Long,
            read_only: true,
        });
    }
    for i in 1..=MAX_SCALER_CHANNELS {
        let pr: &'static str = Box::leak(format!("PR{}", i).into_boxed_str());
        fields.push(FieldDesc {
            name: pr,
            dbf_type: DbFieldType::Long,
            read_only: false,
        });
    }
    for i in 1..=MAX_SCALER_CHANNELS {
        let g: &'static str = Box::leak(format!("G{}", i).into_boxed_str());
        fields.push(FieldDesc {
            name: g,
            dbf_type: DbFieldType::Short,
            read_only: false,
        });
    }
    for i in 1..=MAX_SCALER_CHANNELS {
        let d: &'static str = Box::leak(format!("D{}", i).into_boxed_str());
        fields.push(FieldDesc {
            name: d,
            dbf_type: DbFieldType::Short,
            read_only: false,
        });
    }
    for i in 1..=MAX_SCALER_CHANNELS {
        let nm: &'static str = Box::leak(format!("NM{}", i).into_boxed_str());
        fields.push(FieldDesc {
            name: nm,
            dbf_type: DbFieldType::String,
            read_only: false,
        });
    }
    fields
});

impl Record for ScalerRecord {
    fn record_type(&self) -> &'static str {
        "scaler"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let prev_scaler_state = self.ss;
        let mut just_finished_user_count = false;
        let mut just_started_user_count = false;
        let mut actions = Vec::new();
        let mut fire_coutp = false;
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

        // DLY-wait bridge. C scalerRecord.c handles the DLY delay in a
        // separate `delayCallbackFunc` (scalerRecord.c:216-231): when the
        // delay expires it sets `us = USER_STATE_REQSTART` and scanOnce()s
        // the record. The port collapses that callback into process():
        // while still WAITING, schedule a reprocess and return — counting
        // has not started, so the rest of process() (CNT block, autocount)
        // has nothing to do (the autocount block requires us==IDLE anyway,
        // and the CNT block requires REQSTART/WAITING + cnt — handled on
        // the post-expiry cycle).
        if self.us == USER_STATE_WAITING && self.cnt != 0 {
            if let Some(start) = self.delay_start {
                let dly = self.dly.max(0.0) as f64;
                let elapsed = start.elapsed().as_secs_f64();
                if elapsed >= dly {
                    self.us = USER_STATE_REQSTART;
                    self.delay_start = None;
                } else {
                    // updateCounts with us==WAITING zeroes the displayed
                    // counts (C scalerRecord.c:571-575).
                    self.update_counts();
                    let remaining = std::time::Duration::from_secs_f64(dly - elapsed);
                    return Ok(ProcessOutcome::complete_with(vec![
                        ProcessAction::ReprocessAfter(remaining),
                    ]));
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

                    // Set directions from gates
                    for i in 0..MAX_SCALER_CHANNELS {
                        self.d[i] = self.g[i];
                    }

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
            actions.push(ProcessAction::ReprocessAfter(reprocess));
        }

        // COUT/COUTP
        if just_started_user_count || just_finished_user_count {
            actions.push(ProcessAction::WriteDbLink {
                link_field: "COUT",
                value: EpicsValue::Short(self.cnt),
            });
            if just_finished_user_count {
                fire_coutp = true;
            }
        }
        // C scalerRecord.c:623-624 — `special("CNT")` fires COUTP on every
        // CNT write; `special()` deferred it to this CNT-triggered process.
        if self.coutp_pending {
            self.coutp_pending = false;
            fire_coutp = true;
        }
        if fire_coutp {
            actions.push(ProcessAction::WriteDbLink {
                link_field: "COUTP",
                value: EpicsValue::Short(self.cnt),
            });
        }

        // VAL = T on completion
        if self.ss == SCALER_STATE_IDLE && self.pcnt == 0 && self.us == USER_STATE_IDLE {
            if prev_scaler_state == SCALER_STATE_COUNTING {
                self.val = self.t;
            }
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
                actions.push(ProcessAction::ReprocessAfter(
                    std::time::Duration::from_secs_f64(dly_sec),
                ));
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
                    actions.push(ProcessAction::ReprocessAfter(
                        std::time::Duration::from_secs_f64(remaining),
                    ));
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

    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after {
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
                // passes the redundant-command guard. `special()` cannot
                // emit actions; raise a flag the CNT-triggered `process()`
                // turns into a `WriteDbLink`.
                self.coutp_pending = true;
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
                                // C:643-647 — cancel the pending delay
                                // watchdog (delayCallbackFunc).
                                self.delay_start = None;
                                self.us = USER_STATE_IDLE;
                            }
                            USER_STATE_REQSTART => {
                                self.us = USER_STATE_IDLE;
                            }
                            _ => {}
                        }
                    }
                } else {
                    // C:657-661 — schedule the delayed start callback.
                    self.us = USER_STATE_WAITING;
                    self.delay_start = Some(Instant::now());
                }
            }
            // C scalerRecord.c:664-668 — CONT just rescans; the framework
            // handles process-passive rescan, so nothing to do here.
            "CONT" => {}
            // C scalerRecord.c:670-677 — TP (truncating tp->pr1, d1=g1=1).
            "TP" => {
                self.tp_to_pr1();
            }
            // C has NO special() case for TP1 or RAT1 — leave unchanged.
            "TP1" => {}
            // C scalerRecord.c:690-693 — RATE clamped to [0, 60].
            "RATE" => {
                self.rate = self.rate.clamp(0.0, 60.0);
            }
            _ => {
                if field == "PR1" {
                    self.pr1_to_tp();
                    if self.tp > 0.0 {
                        self.d[0] = 1;
                        self.g[0] = 1;
                    }
                } else if let Some(i) = parse_indexed_field(field, "PR") {
                    if self.pr[i] > 0 {
                        self.d[i] = 1;
                        self.g[i] = 1;
                    }
                } else if let Some(i) = parse_indexed_field(field, "G") {
                    if self.g[i] != 0 && self.pr[i] == 0 {
                        self.pr[i] = 1000;
                    }
                }
            }
        }
        Ok(())
    }

    fn should_fire_forward_link(&self) -> bool {
        self.ss == SCALER_STATE_IDLE && self.us == USER_STATE_IDLE && self.pcnt == 0
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
            "EGU" => return Some(EpicsValue::String(self.egu.clone().into())),
            "OUT" => return Some(EpicsValue::String(self.out.clone().into())),
            "COUT" => return Some(EpicsValue::String(self.cout.clone().into())),
            "COUTP" => return Some(EpicsValue::String(self.coutp.clone().into())),
            _ => {}
        }
        if let Some(i) = parse_indexed_field(name, "NM") {
            return Some(EpicsValue::String(self.nm[i].clone().into()));
        }
        if let Some(i) = parse_indexed_field(name, "PR") {
            return Some(EpicsValue::Long(self.pr[i] as i32));
        }
        if let Some(i) = parse_indexed_field(name, "S") {
            return Some(EpicsValue::Long(self.s[i] as i32));
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
                    self.egu = v.as_str_lossy().into_owned();
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
                            self.nm[i] = v.as_str_lossy().into_owned();
                            Ok(())
                        }
                        _ => Err(CaError::TypeMismatch(name.into())),
                    }
                } else if let Some(i) = parse_indexed_field(name, "PR") {
                    match value {
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
        // Allow framework to write S1-S64 (read-only) from device support read
        if let Some(i) = parse_indexed_field(name, "S") {
            match value {
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

    fn field_list(&self) -> &'static [FieldDesc] {
        let fields: &Vec<FieldDesc> = &ALL_FIELDS;
        unsafe { std::slice::from_raw_parts(fields.as_ptr(), fields.len()) }
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
