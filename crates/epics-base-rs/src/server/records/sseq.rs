use super::link_status::{
    DBF_UNKNOWN, LINK_CON as LNKV_CON, LINK_STATUS_CHOICES as SSEQ_LNKV_CHOICES, LinkStatusGen,
    classify_link,
};
use crate::error::{CaError, CaResult};
use crate::server::database::AsyncDbHandle;
use crate::server::record::{
    FieldDesc, ProcessAction, ProcessOutcome, Record, RecordProcessResult,
};
use crate::types::{DbFieldType, EpicsValue, PvString};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

/// Choice labels for the SELM step-selection menu, in index order.
/// C `menu(sseqSELM)` (synApps sseqRecord.dbd): 0=All, 1=Specified, 2=Mask.
const SSEQ_SELM_CHOICES: &[&str] = &["All", "Specified", "Mask"];

/// Choice labels for the per-step wait-mode menu, in index order.
/// C `menu(sseqWAIT)` (synApps sseqRecord.dbd): 0=NoWait, 1=Wait, then
/// "After1".."AfterA" (wait for step 1..10 to finish before this one).
/// Shared by every `WAIT1`..`WAITA` field.
const SSEQ_WAIT_CHOICES: &[&str] = &[
    "NoWait", "Wait", "After1", "After2", "After3", "After4", "After5", "After6", "After7",
    "After8", "After9", "AfterA",
];

const NUM_STEPS: usize = 10;

/// `SELM` menu indices (C `menu(sseqSELM)`): the step-selection mode.
const SELM_ALL: i16 = 0;
const SELM_SPECIFIED: i16 = 1;
const SELM_MASK: i16 = 2;

/// Per-step field-name tables (1-based suffix `1`..`9`, `A`), used to
/// address `DOLn`/`DOn`/`LNKn`/`WTGn` as the `&'static str` link/target
/// fields a [`ProcessAction`] carries.
const DOL_FIELDS: [&str; NUM_STEPS] = [
    "DOL1", "DOL2", "DOL3", "DOL4", "DOL5", "DOL6", "DOL7", "DOL8", "DOL9", "DOLA",
];
const DO_FIELDS: [&str; NUM_STEPS] = [
    "DO1", "DO2", "DO3", "DO4", "DO5", "DO6", "DO7", "DO8", "DO9", "DOA",
];
const LNK_FIELDS: [&str; NUM_STEPS] = [
    "LNK1", "LNK2", "LNK3", "LNK4", "LNK5", "LNK6", "LNK7", "LNK8", "LNK9", "LNKA",
];
const WTG_FIELDS: [&str; NUM_STEPS] = [
    "WTG1", "WTG2", "WTG3", "WTG4", "WTG5", "WTG6", "WTG7", "WTG8", "WTG9", "WTGA",
];
/// Per-step link-status diagnostic field names (C `checkLinks` posts), in
/// step order. `DOLnV`/`LNKnV` carry `menu(sseqLNKV)` connection status;
/// `DTn`/`LTn` the DOL/LNK target field type; `WERRn` the wait-config error.
const DOLV_FIELDS: [&str; NUM_STEPS] = [
    "DOL1V", "DOL2V", "DOL3V", "DOL4V", "DOL5V", "DOL6V", "DOL7V", "DOL8V", "DOL9V", "DOLAV",
];
const LNKV_FIELDS: [&str; NUM_STEPS] = [
    "LNK1V", "LNK2V", "LNK3V", "LNK4V", "LNK5V", "LNK6V", "LNK7V", "LNK8V", "LNK9V", "LNKAV",
];
const DT_FIELDS: [&str; NUM_STEPS] = [
    "DT1", "DT2", "DT3", "DT4", "DT5", "DT6", "DT7", "DT8", "DT9", "DTA",
];
const LT_FIELDS: [&str; NUM_STEPS] = [
    "LT1", "LT2", "LT3", "LT4", "LT5", "LT6", "LT7", "LT8", "LT9", "LTA",
];
const WERR_FIELDS: [&str; NUM_STEPS] = [
    "WERR1", "WERR2", "WERR3", "WERR4", "WERR5", "WERR6", "WERR7", "WERR8", "WERR9", "WERRA",
];

/// State of the sseq async sequence machine.
///
/// Mirrors the C `sseqRecord.c` control flow: a sequence is a series of
/// per-step continuations driven through the framework PACT primitive,
/// never an all-at-once loop. `Idle` is the only state in which a fresh
/// `process()` trigger (scan / FLNK / `VAL` put) starts a sequence — the
/// framework's PACT entry guard already rejects a foreign trigger while
/// the record is mid-sequence, so a `process()` call reaching the body
/// with `busy == 0` is necessarily a genuine start, exactly as C branches
/// on `!pR->pact`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum SeqPhase {
    /// No sequence running.
    #[default]
    Idle,
    /// `active[cursor]` is scheduled to fire after its `DLYn` delay
    /// (C `processNextLink` → `callbackRequestDelayed`); the next
    /// continuation reads `DOLn` and writes `LNKn`.
    Fire,
    /// The machine is blocked with no per-step timer pending: either an
    /// earlier in-flight `WAITn` put-callback must complete before
    /// `active[cursor]` may fire (a barrier, C `processNextLink` lines
    /// 420-441), or the cursor has passed the last step and the sequence is
    /// draining the still-outstanding put-callbacks (C `processNextLink`
    /// lines 407-414). Re-entry is driven by [`SseqRecord::arm_wake`] when a
    /// completion (or abort) wakes the machine, not by a framework timer.
    Wait,
}

/// One in-flight `WAITn` put-callback.
///
/// C `sseqRecord.c` keeps several `dbCaPutLinkCallback`s outstanding at
/// once: `processNextLink` (sseqRecord.c:420-441) scans the earlier
/// link-groups' `waiting` flags to decide whether the next step may fire.
/// This is the Rust record-local equivalent of one such still-`waiting`
/// link-group — the set sseq owns so the framework's single-outstanding
/// async re-entry token can stay unchanged (the value primitive supersedes
/// by construction, correct for ai/ao/asyn/motor/calcout).
struct InFlight {
    /// Absolute step index (0-based `IXn`) — both the `steps[]` slot and the
    /// value an `After<n>` barrier is compared against (C
    /// `plinkGroupCurrent->index`).
    step: usize,
    /// The step's `WAITn` menu value: 1 = `Wait` (full barrier), ≥2 =
    /// `After<n>` (conditional barrier, `n == wait - 1`). Stored so the
    /// barrier scan needs no second `steps[]` lookup; equals C
    /// `plinkGroup->usePutCallback`.
    wait: i16,
    /// Set by the completion-waiter task once the downstream put (plus its
    /// FLNK/OUT chain) finishes. The machine prunes done entries and posts
    /// `WTGn=0` under the record lock; the waiter never touches record
    /// fields, so a stale waiter abandoned by a double `ABORT` cannot
    /// corrupt a fresh sequence's `WTGn`.
    done: Arc<AtomicBool>,
}

/// Which native type the connected `DOLn` link last delivered, so the
/// `LNKn` write forwards the matching DBR type. C `processCallback` selects
/// this with `dol_field_type` at the `dbGetLink` — a string-class target is
/// read with `DBR_STRING` into `s`/`STRn`, a numeric one with `DBR_DOUBLE`
/// into `dov`/`DOn` (sseqRecord.c:643-705) — and writes `LNKn` with the
/// matching type (sseqRecord.c:714-756). Captured from the actual value the
/// `ReadDbLink` read delivered, so it does not depend on the asynchronously
/// classified `dol_field_type` (`DTn`) being resolved yet.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DolKind {
    Numeric,
    String,
}

/// A single step in the string sequence.
#[derive(Clone)]
struct SseqStep {
    dly: f64,          // Delay before executing this step
    dol: String,       // Input link (DOLn)
    dov: f64,          // Numeric value (DOn)
    lnk: String,       // Output link (LNKn)
    str_val: PvString, // String value (STRn)
    // Native type the last connected-DOL read delivered (selects the LNKn
    // forward type). A `DBF_STRING` DOL source must reach `LNKn` as a string,
    // not collapse through `dov` (Double).
    dol_kind: DolKind,
    wait: i16,     // Wait mode: 0=NoWait, 1=Wait, 2..=After1..After9
    waiting: bool, // WTGn — an outstanding put-callback for this step
    // Link-status diagnostics, refreshed by `refresh_link_status` from C
    // `sseqRecord.c:checkLinks` (sseqRecord.c:848-969). Defaulted to the
    // C `init_record` classification of an empty (constant) link.
    dol_status: i16,     // DOLnV — menu(sseqLNKV) DOL connection status
    lnk_status: i16,     // LNKnV — menu(sseqLNKV) LNK connection status
    dol_field_type: i16, // DTn — DOL target field type (C dbStatic DBF_* / -1)
    lnk_field_type: i16, // LTn — LNK target field type (C dbStatic DBF_* / -1)
    wait_err: i16,       // WERRn — wait-config error (see refresh_link_status)
}

impl Default for SseqStep {
    fn default() -> Self {
        Self {
            dly: 0.0,
            dol: String::new(),
            dov: 0.0,
            lnk: String::new(),
            str_val: PvString::default(),
            dol_kind: DolKind::Numeric,
            wait: 0,
            waiting: false,
            // An empty link is a CONSTANT link with no resolvable field type
            // (C `init_record`, sseqRecord.c:204-206,222-225).
            dol_status: LNKV_CON,
            lnk_status: LNKV_CON,
            dol_field_type: DBF_UNKNOWN,
            lnk_field_type: DBF_UNKNOWN,
            wait_err: 0,
        }
    }
}

/// Sseq record — string sequence record.
///
/// Executes up to 10 steps, each with an optional delay, input link,
/// numeric value, string value, and output link. Steps are selected
/// by SELM (All, Specified, Mask) with SELN as the selection value.
///
/// Processing is a per-step async state machine built on the framework
/// PACT primitive (C `sseqRecord.c::process`/`processNextLink`/
/// `processCallback`/`putCallbackCB`/`asyncFinish`): each step waits out
/// its `DLYn` delay, reads `DOLn`, writes `LNKn`, and — when `WAITn`
/// requests it — blocks the sequence until the downstream put completes.
/// `BUSY` is held across the whole sequence and cleared at the final step.
pub struct SseqRecord {
    pub val: i32,
    pub selm: i16, // 0=All, 1=Specified, 2=Mask
    pub seln: u16,
    pub sell: String,
    pub prec: i16,
    pub abort: i16,
    pub busy: i16,
    aborting: i16,
    /// Set when `start_sequence` rejected the selection (C `process`
    /// `SELM=Specified` with `SELN>10`, or an invalid `SELM` option):
    /// the next `check_alarms` raises `SOFT_ALARM/INVALID`, mirroring C
    /// `recGblSetSevr(pR,SOFT_ALARM,INVALID_ALARM)` before `asyncFinish`.
    selm_invalid: bool,
    phase: SeqPhase,
    active: Vec<usize>,
    cursor: usize,
    steps: [SseqStep; NUM_STEPS],
    /// The `WAITn` put-callbacks currently outstanding (C the still-`waiting`
    /// entries of `pcb->plinkGroups`). A step is dispatched without awaiting
    /// its completion, so several overlap in flight; the barrier scan
    /// ([`SseqRecord::barrier_blocks`]) reads this set to decide when the
    /// next step may fire. Pruned (and `WTGn` cleared) as completions land.
    in_flight: Vec<InFlight>,
    /// The per-sequence wake bridge: a completion-waiter task notifies it
    /// when its put-callback finishes, and [`SseqRecord::arm_wake`] re-enters
    /// the machine to re-scan `in_flight`. A fresh `Notify` per sequence (set
    /// in `start_sequence`, cleared in `finish`) means a waiter abandoned by
    /// a double `ABORT` notifies a bridge nobody listens on — it cannot
    /// disturb the next sequence. `None` while `Idle`.
    seq_wake: Option<Arc<Notify>>,
    /// Canonical record name + cycle-free database handle, stashed at
    /// `add_record` via [`Record::set_async_context`]. Drives out-of-band
    /// status posts (`BUSY`/`WTGn`/`ABORTING`) and the `ABORT` finish
    /// re-entry — the surfaces a `process()` cannot reach itself.
    async_ctx: Option<(String, AsyncDbHandle)>,
    /// Monotonic generation guarding `refresh_link_status`. Each refresh
    /// classifies a *snapshot* of the `DOLn`/`LNKn`/`WAITn` link state
    /// off-thread; a later refresh (a `special()` `DOLn`/`LNKn` re-point)
    /// must win over an earlier one regardless of which spawned task finishes
    /// first. The shared `LinkStatusGen` gate enforces the invariant — only
    /// the latest classification may be published.
    link_gen: LinkStatusGen,
}

impl Default for SseqRecord {
    fn default() -> Self {
        Self {
            val: 0,
            selm: 0,
            seln: 1,
            sell: String::new(),
            prec: 0,
            abort: 0,
            busy: 0,
            aborting: 0,
            selm_invalid: false,
            phase: SeqPhase::Idle,
            active: Vec::new(),
            cursor: 0,
            steps: Default::default(),
            in_flight: Vec::new(),
            seq_wake: None,
            async_ctx: None,
            link_gen: LinkStatusGen::default(),
        }
    }
}

impl SseqRecord {
    pub fn new() -> Self {
        Self::default()
    }

    fn step_index_from_suffix(name: &str) -> Option<(usize, &str)> {
        // Parse step index from field name suffix: 1-9 or A (=10)
        if name.len() < 2 {
            return None;
        }
        let last = name.as_bytes()[name.len() - 1];
        let prefix = &name[..name.len() - 1];
        match last {
            b'1'..=b'9' => Some(((last - b'1') as usize, prefix)),
            b'A' => Some((9, prefix)),
            _ => None,
        }
    }

    /// Parse a `DOLnV` / `LNKnV` link-status field name into its 0-based
    /// step index (suffix `1`..`9` or `A`). These `menu(sseqLNKV)` fields
    /// (sseqRecord.dbd:118,125) carry the step digit *before* the trailing
    /// `V`, so the generic `step_index_from_suffix` (which keys on the last
    /// character) does not recognise them.
    fn link_status_index(name: &str) -> Option<usize> {
        let mid = name
            .strip_prefix("DOL")
            .or_else(|| name.strip_prefix("LNK"))?
            .strip_suffix('V')?;
        if mid.len() != 1 {
            return None;
        }
        match mid.as_bytes()[0] {
            c @ b'1'..=b'9' => Some((c - b'1') as usize),
            b'A' => Some(9),
            _ => None,
        }
    }

    pub fn should_execute_step(&self, step_idx: usize) -> bool {
        match self.selm {
            0 => true, // All
            1 => {
                // Specified — SELN is the 1-based step number. The 10
                // steps are numbered 1..=10 (synApps sseq DLY1..DLYA),
                // so step number `seln` selects index `seln - 1`.
                // `seln == 0` or `seln > 10` selects no step.
                let sel = self.seln as usize;
                (1..=NUM_STEPS).contains(&sel) && step_idx == sel - 1
            }
            2 => {
                // Mask — SELN is a bitmask
                (self.seln & (1 << step_idx)) != 0
            }
            _ => false,
        }
    }

    /// The value this step forwards to its `LNKn`.
    ///
    /// C `processCallback` (sseqRecord.c:643-705) reads `DOLn` typed by
    /// `dol_field_type` into both a string (`s`/`STRn`) and a double
    /// (`dov`/`DOn`), then writes `LNKn` with the matching DBR type
    /// (sseqRecord.c:714-756). A connected `DOLn` whose source is string-class
    /// must therefore reach `LNKn` as the string, byte-exact — not collapse
    /// through `DOn` (Double). `pre_process_actions` performs that typed read
    /// via `ReadDbLink` and `put_field_internal` records which native type the
    /// link delivered (`dol_kind`).
    ///
    /// Precedence: a connected `DOLn` → the type it delivered (`dol_kind`);
    /// a constant link with a non-empty `STRn` → the string constant;
    /// otherwise the `DOn` constant.
    fn step_value(&self, i: usize) -> EpicsValue {
        let s = &self.steps[i];
        let forward_string = if s.dol.is_empty() {
            // Constant link: a configured `STRn` is a string constant,
            // otherwise the `DOn` numeric constant.
            !s.str_val.is_empty()
        } else {
            // Connected `DOLn`: forward the type the link actually delivered.
            s.dol_kind == DolKind::String
        };
        if forward_string {
            EpicsValue::String(s.str_val.clone())
        } else {
            EpicsValue::Double(s.dov)
        }
    }

    /// Post machine-driven status fields (`BUSY`/`WTGn`/`ABORTING`/`ABORT`)
    /// to monitors out-of-band — the C `db_post_events` calls a record's
    /// `process()` makes inline, which the Rust framework does not perform
    /// for an `AsyncPending` cycle. Batched into one post per call so a
    /// cycle's changes never reorder; cycles are themselves serialised by
    /// the per-step re-entry chain. No-op without an async context.
    fn post_live(&self, fields: Vec<(String, EpicsValue)>) {
        if fields.is_empty() {
            return;
        }
        if let Some((name, handle)) = &self.async_ctx {
            let name = name.clone();
            let handle = handle.clone();
            tokio::spawn(async move {
                let _ = handle.post_fields(&name, fields).await;
            });
        }
    }

    /// Build the selection mask and start a sequence (C `process`, the
    /// `!pact` branch). Returns the first step's scheduling outcome, or a
    /// `Complete` (running the forward link) when nothing is selected.
    fn start_sequence(&mut self, live: &mut Vec<(String, EpicsValue)>) -> ProcessOutcome {
        // C `process` (sseqRecord.c:302-305) raises `busy` at the top of a
        // start, before the selection is resolved — so an invalid selection
        // posts the same `busy` 1→0 transition C does.
        if self.busy == 0 {
            self.busy = 1;
            live.push(("BUSY".to_string(), EpicsValue::Short(1)));
            // Re-classify the link diagnostics at each genuine start. This is
            // the stand-in for C's 0.5s `checkLinks` re-poll timer (skipped —
            // epics-base-rs surfaces no connection-change signal): it picks up
            // a LOCAL link whose target record was loaded after this record's
            // init (init then saw EXT_NC; now it resolves to LOC).
            self.refresh_link_status();
        }
        // C `process` (sseqRecord.c:318-335): resolve the selection. With
        // `SELM=Specified` an out-of-range `SELN` (> the 10 steps) is an
        // error — `recGblSetSevr(pR,SOFT_ALARM,INVALID_ALARM)` then
        // `asyncFinish`. `SELN==0` selects nothing WITHOUT an alarm (the
        // empty-active path below). An unknown `SELM` option alarms the
        // same way. `SELM=All`/`Mask` never raise the selection alarm.
        self.selm_invalid = false;
        match self.selm {
            x if x == SELM_ALL || x == SELM_MASK => {}
            x if x == SELM_SPECIFIED => {
                if self.seln > NUM_STEPS as u16 {
                    self.selm_invalid = true;
                    self.finish(live);
                    return ProcessOutcome::complete();
                }
            }
            _ => {
                self.selm_invalid = true;
                self.finish(live);
                return ProcessOutcome::complete();
            }
        }
        // C `process` (sseqRecord.c:338-344) clears every `waiting` flag
        // before building the list.
        for i in 0..NUM_STEPS {
            if self.steps[i].waiting {
                self.steps[i].waiting = false;
                live.push((WTG_FIELDS[i].to_string(), EpicsValue::Short(0)));
            }
        }
        // Fresh per-sequence concurrency state: the in-flight put-callback set
        // (C `pcb->plinkGroups` re-initialised each `process`) and the wake
        // bridge that re-enters the machine when one completes.
        self.in_flight.clear();
        self.seq_wake = Some(Arc::new(Notify::new()));
        // C `process` (sseqRecord.c:346-365): a step joins the active list
        // when it is selected AND has a non-constant `LNKn` or `DOLn`.
        self.active = (0..NUM_STEPS)
            .filter(|&i| {
                self.should_execute_step(i)
                    && (!self.steps[i].lnk.is_empty() || !self.steps[i].dol.is_empty())
            })
            .collect();
        self.cursor = 0;
        if self.active.is_empty() {
            // Nothing selected (C `asyncFinish` still runs `recGblFwdLink`).
            self.finish(live);
            return ProcessOutcome::complete();
        }
        self.advance_sequence(live)
    }

    /// A `ProcessOutcome` that leaves the record async-pending with no
    /// framework action — the machine is blocked and will be re-entered by
    /// [`Self::arm_wake`] (a completion or abort wake), not a timer.
    fn async_pending() -> ProcessOutcome {
        ProcessOutcome {
            result: RecordProcessResult::AsyncPending,
            actions: Vec::new(),
            device_did_compute: false,
        }
    }

    /// Drop completed in-flight put-callbacks and post their `WTGn=0`.
    ///
    /// The completion-waiter only sets the `done` flag; the machine clears
    /// `WTGn` here, under the record lock, so the post always belongs to the
    /// current sequence (C `putCallbackCB` posts `waiting=0`, but doing it
    /// owner-side keeps a stale waiter from clobbering a fresh sequence).
    fn prune_done(&mut self, live: &mut Vec<(String, EpicsValue)>) {
        let mut done_steps = Vec::new();
        self.in_flight.retain(|f| {
            if f.done.load(Ordering::SeqCst) {
                done_steps.push(f.step);
                false
            } else {
                true
            }
        });
        for i in done_steps {
            if self.steps[i].waiting {
                self.steps[i].waiting = false;
                live.push((WTG_FIELDS[i].to_string(), EpicsValue::Short(0)));
            }
        }
    }

    /// Whether firing the step at absolute index `current_abs` must wait for
    /// an earlier in-flight put-callback.
    ///
    /// C `processNextLink` (sseqRecord.c:420-441) scans the earlier
    /// link-groups still `waiting`: a `Wait` (full barrier) blocks the next
    /// step unconditionally, while an `After<n>` blocks it only when
    /// `(usePutCallback - 2) < plinkGroupCurrent->index`. Every `in_flight`
    /// entry was dispatched at an earlier cursor than the current step, so
    /// membership alone supplies C's `ix < pcb->index` (the set is pruned of
    /// completed entries first, so each survivor is still `waiting`).
    fn barrier_blocks(&self, current_abs: usize) -> bool {
        let cur = current_abs as i16;
        self.in_flight
            .iter()
            .any(|f| f.wait == 1 || (f.wait >= 2 && (f.wait - 2) < cur))
    }

    /// Advance the sequence from the current cursor (C `processNextLink`):
    /// prune completed put-callbacks, then either finish, drain, block on a
    /// barrier, or schedule the current step's `DLYn` delay.
    fn advance_sequence(&mut self, live: &mut Vec<(String, EpicsValue)>) -> ProcessOutcome {
        self.prune_done(live);
        if self.cursor >= self.active.len() {
            // C `processNextLink` plinkGroupCurrent == NULL (sseqRecord.c:
            // 407-417): finish only once every outstanding put-callback has
            // drained; otherwise return and let a completion wake the finish.
            if self.in_flight.is_empty() {
                self.finish(live);
                return ProcessOutcome::complete();
            }
            self.phase = SeqPhase::Wait;
            self.arm_wake();
            return Self::async_pending();
        }
        let current = self.active[self.cursor];
        if self.barrier_blocks(current) {
            // An earlier in-flight step must complete before this one fires
            // (C `processNextLink` set `waitingForPutCallback` and returned).
            self.phase = SeqPhase::Wait;
            self.arm_wake();
            return Self::async_pending();
        }
        // Cleared to fire: schedule after `DLYn`. C `processNextLink` requests
        // the callback after `DLYn` (`callbackRequestDelayed`) or immediately
        // when `DLYn == 0` (`callbackRequest`); `ReprocessAfter` is the
        // uniform port of both, so there is no special-cased `DLYn == 0`.
        self.phase = SeqPhase::Fire;
        let dly = std::time::Duration::from_secs_f64(self.steps[current].dly.max(0.0));
        ProcessOutcome {
            result: RecordProcessResult::AsyncPending,
            actions: vec![ProcessAction::ReprocessAfter(dly)],
            device_did_compute: false,
        }
    }

    /// Fire `active[cursor]` (C `processCallback`): forward the step value to
    /// `LNKn`, then advance. A `WAITn` step is dispatched WITHOUT blocking the
    /// machine — its put-callback joins `in_flight` and the sequence moves on,
    /// so several callbacks overlap exactly as C runs them.
    fn fire_current_step(&mut self, live: &mut Vec<(String, EpicsValue)>) -> ProcessOutcome {
        let i = self.active[self.cursor];
        let value = self.step_value(i);
        let has_lnk = !self.steps[i].lnk.is_empty();
        // C `processCallback` (sseqRecord.c:717,739,763) uses the
        // put-WITH-completion (`dbCaPutLinkCallback`, setting `waiting`) only
        // when `usePutCallback` (`WAITn != NoWait`).
        let waits = self.steps[i].wait != 0;

        if waits && has_lnk {
            // Non-blocking dispatch: the put-callback goes in flight and the
            // machine advances. C `processCallback` increments `pcb->index`
            // and calls `processNextLink` straight after firing, leaving the
            // just-fired step `waiting` for the barrier scan to honour.
            self.dispatch_waiting_step(i, value, live);
            self.cursor += 1;
            return self.advance_sequence(live);
        }

        // No-wait step: a plain `dbPutLink` (`WriteDbLink`), then advance in
        // the same cycle. The write rides ahead of the next step's scheduling
        // action (or the drain / `Complete` tail), so `LNKn` lands before the
        // sequence moves on.
        let mut actions = Vec::new();
        if has_lnk {
            actions.push(ProcessAction::WriteDbLink {
                link_field: LNK_FIELDS[i],
                value,
            });
        }
        self.cursor += 1;
        let mut next = self.advance_sequence(live);
        actions.append(&mut next.actions);
        next.actions = actions;
        next
    }

    /// Dispatch a `WAITn` step's put-with-completion without blocking the
    /// machine: record it in `in_flight`, raise `WTGn`, and spawn a waiter
    /// that marks the entry done and wakes the machine on completion.
    ///
    /// The seam is `AsyncDbHandle::put_link_notify` (the non-blocking sibling
    /// of `ProcessAction::WriteDbLinkNotify`): it issues the same OUT-link
    /// put-notify but hands back the completion receiver instead of wiring it
    /// to a single superseding re-entry token, so concurrent callbacks do not
    /// clobber one another. The waiter never touches record fields — it only
    /// sets `done` and notifies — so a waiter abandoned by a double `ABORT`
    /// cannot corrupt a later sequence (C `putCallbackCB`'s `waiting == 0`
    /// guard against abandoned callbacks, sseqRecord.c:540-560).
    fn dispatch_waiting_step(
        &mut self,
        i: usize,
        value: EpicsValue,
        live: &mut Vec<(String, EpicsValue)>,
    ) {
        self.steps[i].waiting = true;
        live.push((WTG_FIELDS[i].to_string(), EpicsValue::Short(1)));
        let done = Arc::new(AtomicBool::new(false));
        self.in_flight.push(InFlight {
            step: i,
            wait: self.steps[i].wait,
            done: done.clone(),
        });
        if let (Some((name, handle)), Some(wake)) = (&self.async_ctx, &self.seq_wake) {
            let name = name.clone();
            let handle = handle.clone();
            let wake = wake.clone();
            let link = self.steps[i].lnk.clone();
            tokio::spawn(async move {
                if let Some(rx) = handle.put_link_notify(&name, &link, value).await {
                    // `Err` means the put vanished without firing — treat it as
                    // completion so the step is never stranded.
                    let _ = rx.await;
                }
                done.store(true, Ordering::SeqCst);
                wake.notify_one();
            });
        }
    }

    /// Park a task on the per-sequence wake bridge that re-enters the
    /// machine's `process()` once a put-callback completion (or an abort)
    /// notifies it. Called only while blocked in phase `Wait`, where no
    /// per-step `ReprocessAfter` is pending — so minting the re-entry token
    /// in the woken task supersedes no delay timer, keeping the framework's
    /// single-outstanding token model intact.
    ///
    /// A single `Notify` permit coalesces multiple completions; the re-entry
    /// re-scans the whole `in_flight` set, so no wake is lost. Each woken task
    /// is one-shot and re-enters before the next `arm_wake`, so at most one
    /// task is ever parked on the bridge.
    fn arm_wake(&self) {
        let (Some((name, handle)), Some(wake)) = (self.async_ctx.as_ref(), self.seq_wake.as_ref())
        else {
            return;
        };
        let name = name.clone();
        let handle = handle.clone();
        let wake = wake.clone();
        tokio::spawn(async move {
            wake.notified().await;
            reenter_now(&name, &handle).await;
        });
    }

    /// Notify the per-sequence wake bridge so a parked [`Self::arm_wake`] task
    /// re-enters the machine. Used by `ABORT` to drive the drain when the
    /// machine is blocked in phase `Wait` (the parked task fires the current,
    /// un-superseded re-entry token — so no second re-entry task competes for
    /// the bridge). No-op while `Idle`.
    fn wake_machine(&self) {
        if let Some(wake) = &self.seq_wake {
            wake.notify_one();
        }
    }

    /// Drain outstanding put-callbacks during an abort (C `processNextLink`
    /// end path under `pR->abort`): no new step fires; prune completed
    /// callbacks and finish once they have all drained, otherwise stay
    /// blocked and let the next completion wake the finish.
    fn drain_abort(&mut self, live: &mut Vec<(String, EpicsValue)>) -> ProcessOutcome {
        self.prune_done(live);
        if self.in_flight.is_empty() {
            self.finish(live);
            return ProcessOutcome::complete();
        }
        self.phase = SeqPhase::Wait;
        self.arm_wake();
        Self::async_pending()
    }

    /// Finish the sequence (C `asyncFinish`): clear `abort`/`aborting`,
    /// every `waiting` flag, and `busy`; return to `Idle`. The framework's
    /// `Complete` tail runs `recGblFwdLink` and posts `VAL`; this only
    /// resets the machine state and queues the status posts C makes inline.
    fn finish(&mut self, live: &mut Vec<(String, EpicsValue)>) {
        if self.abort != 0 {
            self.abort = 0;
            live.push(("ABORT".to_string(), EpicsValue::Short(0)));
        }
        if self.aborting != 0 {
            self.aborting = 0;
            live.push(("ABORTING".to_string(), EpicsValue::Short(0)));
        }
        for i in 0..NUM_STEPS {
            if self.steps[i].waiting {
                self.steps[i].waiting = false;
                live.push((WTG_FIELDS[i].to_string(), EpicsValue::Short(0)));
            }
        }
        if self.busy != 0 {
            self.busy = 0;
            live.push(("BUSY".to_string(), EpicsValue::Short(0)));
        }
        self.active.clear();
        self.cursor = 0;
        self.phase = SeqPhase::Idle;
        // Drop the per-sequence concurrency state. Any waiter still running
        // holds its own `done` Arc and a clone of the (now replaced-on-next-
        // start) wake bridge, so it cannot touch this record's fields.
        self.in_flight.clear();
        self.seq_wake = None;
    }

    /// Drive an immediate abort completion from `special` (C
    /// `epicsTimerCancel` + `callbackRequest`): supersede any pending
    /// `DLYn` re-entry, then re-enter so the next `process()` runs the abort
    /// drain at once. The superseded timer, when it wakes, re-enters nothing
    /// (the `AsyncToken` generation gate). Used when the machine is in phase
    /// `Fire` (a delay is pending); a phase-`Wait` abort instead wakes the
    /// already-parked bridge task via [`Self::wake_machine`].
    fn force_finish_reentry(&self) {
        if let Some((name, handle)) = &self.async_ctx {
            let name = name.clone();
            let handle = handle.clone();
            tokio::spawn(async move {
                handle.cancel_async_reentry(&name).await;
                reenter_now(&name, &handle).await;
            });
        }
    }

    /// Recompute the per-step DOL/LNK link-status diagnostics
    /// (`DOLnV`/`LNKnV`/`DTn`/`LTn`/`WERRn`) and post them out-of-band,
    /// mirroring C `sseqRecord.c:checkLinks` (sseqRecord.c:848-969) and the
    /// `init_record` link classification (sseqRecord.c:202-250).
    ///
    /// Spawned because resolving a LOCAL link's target field type is an
    /// async cross-record lookup; the spawned task begins with a
    /// `yield_now` so that the init trigger (called from
    /// `set_async_context`, before this record is inserted into the
    /// database map) still posts after the record is registered. The
    /// record write-lock is always released before the task runs, so a
    /// self-targeting link cannot deadlock.
    ///
    /// Divergences from C, all forced by what epics-base-rs surfaces:
    ///   * EXTERNAL (CA/PVA) links cannot be introspected here — there is no
    ///     client-side connection state or remote field type — so an
    ///     external link reports `EXT_NC` and `DTn`/`LTn` = unknown, not C's
    ///     `EXT`/`EXT_NC` connection toggle (sseqRecord.c:862-941). This is
    ///     a cross-crate limitation: epics-base-rs has no CA/PVA client.
    ///   * `WERRn` is INVERTED from C. C raises it for a local DB link with
    ///     `WAITn` set, because it cannot `dbCaPutLinkCallback` a non-CA
    ///     link (sseqRecord.c:915-927). The Rust put-with-completion seam
    ///     (`put_link_notify`) DOES complete on a local PP link, so that is
    ///     no longer an error.
    ///     `WERRn` is redefined for the Rust-meaningful misconfig: `WAITn`
    ///     set on a link that can never deliver a completion — a
    ///     Constant/unset (`CON`) link.
    ///   * C's 0.5s connection re-poll timer (sseqRecord.c:957-963) is
    ///     skipped: epics-base-rs surfaces no link connection-change signal
    ///     to drive it. The refresh runs at record init, on `special()` of
    ///     a DOL/LNK/WAIT field, and at sequence start instead.
    fn refresh_link_status(&self) {
        let Some((name, handle)) = &self.async_ctx else {
            return;
        };
        let name = name.clone();
        let handle = handle.clone();
        let groups: Vec<(String, String, i16)> = (0..NUM_STEPS)
            .map(|i| {
                let s = &self.steps[i];
                (s.dol.clone(), s.lnk.clone(), s.wait)
            })
            .collect();
        let link_gen = self.link_gen.clone();
        // Stamp this refresh. A concurrent later refresh (a runtime DOLn/LNKn
        // re-point through `special()`) issues a newer token; this task then
        // sees its token is stale and drops its post so an init-time snapshot
        // finishing late cannot clobber the newer classification.
        let token = link_gen.next();
        tokio::spawn(async move {
            // Let `add_record` finish registering this record before the
            // init post (this task may be spawned from `set_async_context`,
            // which runs just before the record is inserted into the map).
            tokio::task::yield_now().await;
            let mut fields: Vec<(String, EpicsValue)> = Vec::with_capacity(NUM_STEPS * 5);
            for (i, (dol, lnk, wait)) in groups.iter().enumerate() {
                let (dol_status, dol_ft) = classify_link(&handle, dol).await;
                let (lnk_status, lnk_ft) = classify_link(&handle, lnk).await;
                // Redefined `WERRn`: `WAITn` on a link that can never deliver
                // a completion (a Constant/unset = `CON` link). See the
                // method doc-comment and sseqRecord.c:915-927.
                let werr = if *wait != 0 && lnk_status == LNKV_CON {
                    1
                } else {
                    0
                };
                fields.push((
                    DOLV_FIELDS[i].to_string(),
                    EpicsValue::Enum(dol_status as u16),
                ));
                fields.push((
                    LNKV_FIELDS[i].to_string(),
                    EpicsValue::Enum(lnk_status as u16),
                ));
                fields.push((DT_FIELDS[i].to_string(), EpicsValue::Short(dol_ft)));
                fields.push((LT_FIELDS[i].to_string(), EpicsValue::Short(lnk_ft)));
                fields.push((WERR_FIELDS[i].to_string(), EpicsValue::Short(werr)));
            }
            // Publish only if no newer refresh was issued meanwhile.
            if link_gen.is_current(token) {
                let _ = handle.post_fields(&name, fields).await;
            }
        });
    }

    /// True for the per-step link-config fields whose put must re-classify
    /// the link diagnostics (C `sseqRecord.c:special` `SPC_MOD` →
    /// `checkLinks`): `DOLn` / `LNKn` (the link strings) and `WAITn` (the
    /// wait mode that drives `WERRn`). The read-only `DOLnV`/`LNKnV` status
    /// menus are NOT included — they are outputs of this classification.
    fn is_link_config_field(name: &str) -> bool {
        matches!(
            Self::step_index_from_suffix(name),
            Some((_, "DOL")) | Some((_, "LNK")) | Some((_, "WAIT"))
        )
    }
}

/// Mint a fresh async re-entry token wired to an already-fired completion,
/// so the record's `process()` runs again as soon as the framework
/// schedules it (C `callbackRequest`). Shared by `arm_wake` (after a
/// completion notify) and `force_finish_reentry` (after cancelling a
/// pending delay). Minting supersedes any prior pending token; callers
/// invoke it only where that is intended (phase `Wait`, where nothing is
/// pending, or right after `cancel_async_reentry`).
async fn reenter_now(name: &str, handle: &AsyncDbHandle) {
    if let Some(token) = handle.mint_async_token(name).await {
        let (waitset, completion) = AsyncDbHandle::new_put_notify();
        waitset.leave();
        handle.reprocess_on_notify(token, completion);
    }
}

/// Build the full `sseq` field table. The 7 base record fields plus the
/// top-level `ABORTING` status, then the C "struct linkGroup" — 13 fields
/// per step (sseqRecord.dbd) for each of the 10 steps (suffixes `1`..`9`,
/// `A`), in DBD declaration order. `concat!` materialises each per-step
/// field name as a `&'static str`, keeping the table a compile-time
/// `static` while the per-step shape is generated once (no copy-paste
/// drift across 130 entries).
///
/// The read-only diagnostics are all driven LIVE. `WTGn` (outstanding
/// put-callback) and top-level `ABORTING` come from the sequence machine
/// (C `sseqRecord.c::processCallback`/`putCallbackCB`/`special`/
/// `asyncFinish`), posted via `post_live`. `DTn`/`LTn` (DOL/LNK link
/// field type), `WERRn` (wait-config error) and `DOLnV`/`LNKnV` (link
/// connection status, `menu(sseqLNKV)`) come from C `checkLinks`, posted
/// by `refresh_link_status` (with the external-link / `WERRn` divergences
/// documented there). `IXn` (step index) is a fixed per-step constant.
macro_rules! sseq_fields {
    ($($s:literal),+ $(,)?) => {
        &[
            FieldDesc { name: "VAL", dbf_type: DbFieldType::Long, read_only: false },
            // SELM is DBF_MENU menu(sseqSELM) (sseqRecord.dbd:34) — served
            // as DBR_ENUM with the menu's choice labels (SSEQ_SELM_CHOICES).
            FieldDesc { name: "SELM", dbf_type: DbFieldType::Enum, read_only: false },
            FieldDesc { name: "SELN", dbf_type: DbFieldType::UShort, read_only: false },
            FieldDesc { name: "SELL", dbf_type: DbFieldType::String, read_only: false },
            FieldDesc { name: "PREC", dbf_type: DbFieldType::Short, read_only: false },
            FieldDesc { name: "ABORT", dbf_type: DbFieldType::Short, read_only: false },
            FieldDesc { name: "ABORTING", dbf_type: DbFieldType::Short, read_only: true },
            FieldDesc { name: "BUSY", dbf_type: DbFieldType::Short, read_only: true },
            $(
                FieldDesc { name: concat!("DLY", $s), dbf_type: DbFieldType::Double, read_only: false },
                FieldDesc { name: concat!("DOL", $s), dbf_type: DbFieldType::String, read_only: false },
                FieldDesc { name: concat!("DO", $s), dbf_type: DbFieldType::Double, read_only: false },
                FieldDesc { name: concat!("LNK", $s), dbf_type: DbFieldType::String, read_only: false },
                FieldDesc { name: concat!("STR", $s), dbf_type: DbFieldType::String, read_only: false },
                FieldDesc { name: concat!("DT", $s), dbf_type: DbFieldType::Short, read_only: true },
                FieldDesc { name: concat!("LT", $s), dbf_type: DbFieldType::Short, read_only: true },
                // WAITn is DBF_MENU menu(sseqWAIT) — see SSEQ_WAIT_CHOICES.
                FieldDesc { name: concat!("WAIT", $s), dbf_type: DbFieldType::Short, read_only: false },
                FieldDesc { name: concat!("WERR", $s), dbf_type: DbFieldType::Short, read_only: true },
                FieldDesc { name: concat!("WTG", $s), dbf_type: DbFieldType::Short, read_only: true },
                FieldDesc { name: concat!("IX", $s), dbf_type: DbFieldType::Short, read_only: true },
                // DOLnV / LNKnV are DBF_MENU menu(sseqLNKV) — served as
                // DBR_ENUM with SSEQ_LNKV_CHOICES.
                FieldDesc { name: concat!("DOL", $s, "V"), dbf_type: DbFieldType::Enum, read_only: true },
                FieldDesc { name: concat!("LNK", $s, "V"), dbf_type: DbFieldType::Enum, read_only: true },
            )+
        ]
    };
}

static SSEQ_FIELDS: &[FieldDesc] = sseq_fields!("1", "2", "3", "4", "5", "6", "7", "8", "9", "A");

impl Record for SseqRecord {
    fn record_type(&self) -> &'static str {
        "sseq"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let mut live = Vec::new();
        // An abort in flight drains the outstanding put-callbacks, then
        // finishes — C `processNextLink` under `pR->abort` returns until the
        // last callback drains, and `process` takes the `pact` path to
        // `asyncFinish`. `aborting` is the machine's own drain flag, set by
        // `special` the moment an abort is accepted, so this gate fires ahead
        // of any step work.
        let outcome = if self.busy != 0 && self.aborting != 0 {
            self.drain_abort(&mut live)
        } else {
            // `busy == 0` (phase `Idle`) is a genuine start: the framework
            // PACT entry guard already rejected any foreign trigger while a
            // sequence was running, so reaching the body un-busy mirrors C's
            // `!pR->pact`. `busy == 1` is a per-step continuation: phase
            // `Fire` fires the current step, phase `Wait` re-scans the
            // in-flight set (barrier release or drain).
            match self.phase {
                SeqPhase::Idle => self.start_sequence(&mut live),
                SeqPhase::Fire => self.fire_current_step(&mut live),
                SeqPhase::Wait => self.advance_sequence(&mut live),
            }
        };
        self.post_live(live);
        Ok(outcome)
    }

    fn pre_input_link_actions(&mut self) -> Vec<ProcessAction> {
        // C `process` (sseqRecord.c:314-317) reads `SELL` into `SELN` before
        // building the selection mask, and only when `SELM != All`. This is
        // the earliest hook (it runs before the selection is resolved), and
        // only at a sequence start (`busy == 0`); a continuation must not
        // re-read `SELL` mid-sequence.
        if self.busy == 0 && self.selm != 0 && !self.sell.is_empty() {
            vec![ProcessAction::ReadDbLink {
                link_field: "SELL",
                target_field: "SELN",
            }]
        } else {
            Vec::new()
        }
    }

    fn pre_process_actions(&mut self) -> Vec<ProcessAction> {
        // C `processCallback` reads the current step's `DOLn` AFTER its delay
        // has elapsed (sseqRecord.c:643-666). The per-step `ReprocessAfter`
        // re-enters in phase `Fire` with `cursor` on that step, so the DOL
        // read is scoped to exactly that step here — not all steps up front.
        if self.phase == SeqPhase::Fire && self.cursor < self.active.len() {
            let i = self.active[self.cursor];
            if !self.steps[i].dol.is_empty() {
                return vec![ProcessAction::ReadDbLink {
                    link_field: DOL_FIELDS[i],
                    target_field: DO_FIELDS[i],
                }];
            }
        }
        Vec::new()
    }

    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after {
            return Ok(());
        }
        // A put to a DOL/LNK/WAIT field re-classifies the link diagnostics:
        // C `sseqRecord.c::special` (`SPC_MOD`) re-runs `checkLinks` so the
        // `DOLnV`/`LNKnV`/`DTn`/`LTn`/`WERRn` surface tracks the new link.
        // The link strings/wait mode the put just stored are re-read here.
        if Self::is_link_config_field(field) {
            self.refresh_link_status();
            return Ok(());
        }
        // C `sseqRecord.c::special` handles `ABORT` (SPC_MOD) entirely
        // outside the process cycle — a put to `ABORT` does NOT process the
        // record (only `VAL` is process-passive). `pR->abort` already holds
        // the value the put just stored.
        if !field.eq_ignore_ascii_case("ABORT") {
            return Ok(());
        }
        let mut live = Vec::new();
        if self.busy == 0 {
            // C: "no activity to abort" — drop the request.
            if self.abort != 0 {
                self.abort = 0;
                live.push(("ABORT".to_string(), EpicsValue::Short(0)));
            }
            self.post_live(live);
            return Ok(());
        }
        if self.aborting != 0 {
            // Second abort while already draining (C sseqRecord.c:1179-1190):
            // a downstream put-callback is hung. Abandon every outstanding
            // callback — clear its `waiting`/`WTGn`, drop the in-flight set —
            // and finish now. The abandoned waiters set their (now-dropped)
            // `done` Arcs and notify the per-sequence bridge nobody listens
            // on, so they are harmless (C's `waiting == 0` abandoned-callback
            // guard).
            for i in 0..NUM_STEPS {
                if self.steps[i].waiting {
                    self.steps[i].waiting = false;
                    live.push((WTG_FIELDS[i].to_string(), EpicsValue::Short(0)));
                }
            }
            self.in_flight.clear();
            self.cursor = self.active.len();
            // With `in_flight` empty the drain finishes immediately on
            // re-entry: wake the parked phase-`Wait` task, or mint a fresh
            // re-entry if somehow not blocked.
            if self.phase == SeqPhase::Wait {
                self.wake_machine();
            } else {
                self.force_finish_reentry();
            }
            self.post_live(live);
            return Ok(());
        }
        self.aborting = 1;
        live.push(("ABORTING".to_string(), EpicsValue::Short(1)));
        // No further step dispatches; the drain only waits out the in-flight
        // callbacks.
        self.cursor = self.active.len();
        // C cancels a pending `DLYn` delay timer and completes the abort
        // immediately (sseqRecord.c:1194-1215); when instead blocked on a
        // put-callback it lets the outstanding callback wake the finish
        // (sseqRecord.c:1161-1164). Phase `Fire` has a `ReprocessAfter`
        // pending, so cancel it and re-enter to drain; phase `Wait` already
        // has a parked bridge task, so just wake it (firing its current,
        // un-superseded token — no competing re-entry task).
        if self.phase == SeqPhase::Fire {
            self.force_finish_reentry();
        } else if self.phase == SeqPhase::Wait {
            self.wake_machine();
        }
        self.post_live(live);
        Ok(())
    }

    fn set_async_context(&mut self, name: String, db: AsyncDbHandle) {
        self.async_ctx = Some((name, db));
        // C `init_record` classifies every DOL/LNK link and posts the
        // initial status (sseqRecord.c:202-250). This is the framework's
        // init-time hook, so refresh now that the async surface exists.
        self.refresh_link_status();
    }

    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        // C `process` raises `recGblSetSevr(pR,SOFT_ALARM,INVALID_ALARM)` for
        // a bad `SELM`/`SELN` selection just before `asyncFinish`
        // (sseqRecord.c:321,333). `start_sequence` flagged it; raise it here,
        // in the framework `checkAlarms` hook, so it accumulates into
        // `nsta`/`nsev` like every other record alarm.
        if self.selm_invalid {
            crate::server::recgbl::rec_gbl_set_sevr(
                common,
                crate::server::recgbl::alarm_status::SOFT_ALARM,
                crate::server::record::AlarmSeverity::Invalid,
            );
        }
    }

    // `SseqRecord` does NOT implement `Record::multi_output_links`: the
    // per-step `LNKn` writes are driven here, in `process()` — a no-wait step
    // via `WriteDbLink`, a `WAITn` step via the `put_link_notify` seam
    // (C `sseqRecord.c::processCallback`) — not by the generic multi-output
    // block. The retired `dispatch_multi_output` `MultiOut::Sseq` arm was the
    // old all-at-once path and no longer exists.

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Long(self.val)),
            // SELM is DBF_MENU (sseqRecord.dbd:34): served as DBR_ENUM,
            // labels from menu_field_choices.
            "SELM" => Some(EpicsValue::Enum(self.selm as u16)),
            "SELN" => Some(EpicsValue::UShort(self.seln)),
            "SELL" => Some(EpicsValue::String(self.sell.clone().into())),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "ABORT" => Some(EpicsValue::Short(self.abort)),
            // ABORTING (sseqRecord.dbd:820) is machine-driven: C
            // `sseqRecord.c:special`/`asyncFinish` toggle it across an abort.
            // The sequence machine here holds it live (1 while an abort is
            // draining the outstanding step, 0 otherwise) and posts it via
            // `post_live`; read-only to clients.
            "ABORTING" => Some(EpicsValue::Short(self.aborting)),
            "BUSY" => Some(EpicsValue::Short(self.busy)),
            _ => {
                // DOLnV / LNKnV link-status menu (menu(sseqLNKV),
                // sseqRecord.dbd:118,125). The step digit sits before the
                // trailing `V`, so `step_index_from_suffix` (which keys on
                // the last char) does not recognise these. Live connection
                // status from `refresh_link_status` (C `checkLinks`); the
                // default for an unconfigured (empty) link is `CON`.
                if let Some(idx) = Self::link_status_index(name) {
                    let status = if name.starts_with("DOL") {
                        self.steps[idx].dol_status
                    } else {
                        self.steps[idx].lnk_status
                    };
                    return Some(EpicsValue::Enum(status as u16));
                }
                if let Some((idx, prefix)) = Self::step_index_from_suffix(name) {
                    let step = &self.steps[idx];
                    return match prefix {
                        "DLY" => Some(EpicsValue::Double(step.dly)),
                        "DOL" => Some(EpicsValue::String(step.dol.clone().into())),
                        "DO" => Some(EpicsValue::Double(step.dov)),
                        "LNK" => Some(EpicsValue::String(step.lnk.clone().into())),
                        "STR" => Some(EpicsValue::String(step.str_val.clone())),
                        "WAIT" => Some(EpicsValue::Short(step.wait)),
                        // Per-step link diagnostics, refreshed by
                        // `refresh_link_status` (C `checkLinks`): DOL/LNK
                        // target field type and the wait-config error. The
                        // default for an unconfigured link is unknown (-1).
                        "DT" => Some(EpicsValue::Short(step.dol_field_type)),
                        "LT" => Some(EpicsValue::Short(step.lnk_field_type)),
                        "WERR" => Some(EpicsValue::Short(step.wait_err)),
                        // WTGn — live "outstanding put-callback" flag for
                        // this step (C `processCallback`/`putCallbackCB`
                        // toggle `waiting`); posted via `post_live`.
                        "WTG" => Some(EpicsValue::Short(self.steps[idx].waiting as i16)),
                        // IXn holds the step's own 0-based index
                        // (sseqRecord.dbd initial: IX1=0 .. IXA=9).
                        "IX" => Some(EpicsValue::Short(idx as i16)),
                        _ => None,
                    };
                }
                None
            }
        }
    }

    fn put_field_internal(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        // The framework's typed link read (`ProcessAction::ReadDbLink` →
        // `DOn`) delivers the `DOLn` target's NATIVE value here. C
        // `processCallback` reads `DOLn` by `dol_field_type`: a string-class
        // source is read with `DBR_STRING` and kept byte-exact in `s`/`STRn`
        // (then `dov = atof(s)`), NOT collapsed to a double
        // (sseqRecord.c:643-705). A *client* put to `DOn` (`put_field`) is a
        // plain `DBF_DOUBLE` convert, so this string-preserving capture is
        // internal-only; it also records which native type the link delivered
        // so the `LNKn` write forwards the matching DBR type.
        if let Some((idx, "DO")) = Self::step_index_from_suffix(name) {
            match value {
                EpicsValue::String(s) => {
                    // Numeric view tracks C's `dov = atof(s)`; `str_val` keeps
                    // the bytes exactly (no lossy conversion on the value).
                    let dov = EpicsValue::String(s.clone()).to_f64().unwrap_or(0.0);
                    let step = &mut self.steps[idx];
                    step.dov = dov;
                    step.str_val = s;
                    step.dol_kind = DolKind::String;
                    return Ok(());
                }
                other => {
                    let dov = other
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                    // C `processCallback` numeric arm: after reading `dov` it
                    // runs `cvtDoubleToString(dov, str, prec)` and copies the
                    // formatted value back into `s`/`STRn`, posting it when it
                    // changed (sseqRecord.c:676-679). Mirror that so a numeric
                    // `DOLn` refreshes `STRn` with the record-PREC rendering of
                    // the value rather than leaving the prior string stale.
                    let prec = self.prec.max(0) as u16;
                    let s = crate::types::cvt_double_to_string(dov, prec);
                    let step = &mut self.steps[idx];
                    step.dov = dov;
                    step.str_val = PvString::from(s);
                    step.dol_kind = DolKind::Numeric;
                    return Ok(());
                }
            }
        }
        self.put_field(name, value)
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                self.val = match value {
                    EpicsValue::Long(v) => v,
                    _ => value
                        .to_f64()
                        .map(|v| v as i32)
                        .ok_or_else(|| CaError::TypeMismatch("VAL".into()))?,
                };
                Ok(())
            }
            // SELM is DBF_MENU (sseqRecord.dbd:34): a client put arrives
            // converted to Enum; internal callers may still pass a Short
            // index. Store the menu index either way.
            "SELM" => match value {
                EpicsValue::Enum(v) => {
                    self.selm = v as i16;
                    Ok(())
                }
                EpicsValue::Short(v) => {
                    self.selm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SELM".into())),
            },
            // SELN is `DBF_USHORT` (sseqRecord.dbd:40): client puts arrive
            // converted to UShort; internal SELL link reads pass a Short.
            "SELN" => match value {
                EpicsValue::UShort(v) => {
                    self.seln = v;
                    Ok(())
                }
                EpicsValue::Short(v) => {
                    self.seln = v as u16;
                    Ok(())
                }
                _ => {
                    let v = value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch("SELN".into()))?;
                    self.seln = v as u16;
                    Ok(())
                }
            },
            "SELL" => match value {
                EpicsValue::String(s) => {
                    self.sell = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SELL".into())),
            },
            "PREC" => match value {
                EpicsValue::Short(v) => {
                    self.prec = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("PREC".into())),
            },
            "ABORT" => match value {
                EpicsValue::Short(v) => {
                    self.abort = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("ABORT".into())),
            },
            // BUSY / ABORTING are read-only to clients (the `field_io`
            // read-only gate rejects external puts before this point), but
            // the sequence machine drives them through `post_fields`
            // (`put_field_internal`), which lands here. Store the value the
            // machine already set so the monitor post reflects it.
            "BUSY" => {
                self.busy = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("BUSY".into()))?
                    as i16;
                Ok(())
            }
            "ABORTING" => {
                self.aborting = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("ABORTING".into()))?
                    as i16;
                Ok(())
            }
            _ => {
                // DOLnV / LNKnV link-status menus are read-only to clients;
                // the link-status refresh (`post_fields` → `put_field_internal`)
                // lands here to store the connection status it just computed.
                if let Some(idx) = Self::link_status_index(name) {
                    let v = value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?
                        as i16;
                    if name.starts_with("DOL") {
                        self.steps[idx].dol_status = v;
                    } else {
                        self.steps[idx].lnk_status = v;
                    }
                    return Ok(());
                }
                if let Some((idx, prefix)) = Self::step_index_from_suffix(name) {
                    let step = &mut self.steps[idx];
                    return match prefix {
                        "DLY" => {
                            step.dly = value
                                .to_f64()
                                .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                            Ok(())
                        }
                        "DOL" => match value {
                            EpicsValue::String(s) => {
                                step.dol = s.as_str_lossy().into_owned();
                                Ok(())
                            }
                            _ => Err(CaError::TypeMismatch(name.into())),
                        },
                        "DO" => {
                            step.dov = value
                                .to_f64()
                                .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                            Ok(())
                        }
                        "LNK" => match value {
                            EpicsValue::String(s) => {
                                step.lnk = s.as_str_lossy().into_owned();
                                Ok(())
                            }
                            _ => Err(CaError::TypeMismatch(name.into())),
                        },
                        "STR" => match value {
                            EpicsValue::String(s) => {
                                step.str_val = s;
                                Ok(())
                            }
                            _ => Err(CaError::TypeMismatch(name.into())),
                        },
                        "WAIT" => match value {
                            EpicsValue::Short(v) => {
                                step.wait = v;
                                Ok(())
                            }
                            _ => Err(CaError::TypeMismatch(name.into())),
                        },
                        // WTGn is read-only to clients; the machine posts it
                        // via `post_fields` (`put_field_internal`), which
                        // lands here. Store the flag the machine already set.
                        "WTG" => {
                            step.waiting = value
                                .to_f64()
                                .ok_or_else(|| CaError::TypeMismatch(name.into()))?
                                != 0.0;
                            Ok(())
                        }
                        // DTn / LTn / WERRn are read-only to clients; the
                        // link-status refresh posts them via `post_fields`
                        // (`put_field_internal`), landing here. Store the
                        // value the refresh just computed.
                        "DT" => {
                            step.dol_field_type = value
                                .to_f64()
                                .ok_or_else(|| CaError::TypeMismatch(name.into()))?
                                as i16;
                            Ok(())
                        }
                        "LT" => {
                            step.lnk_field_type = value
                                .to_f64()
                                .ok_or_else(|| CaError::TypeMismatch(name.into()))?
                                as i16;
                            Ok(())
                        }
                        "WERR" => {
                            step.wait_err = value
                                .to_f64()
                                .ok_or_else(|| CaError::TypeMismatch(name.into()))?
                                as i16;
                            Ok(())
                        }
                        _ => Err(CaError::FieldNotFound(name.to_string())),
                    };
                }
                Err(CaError::FieldNotFound(name.to_string()))
            }
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        SSEQ_FIELDS
    }

    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "SELM" => Some(SSEQ_SELM_CHOICES),
            // The per-step `WAIT1`..`WAITA` fields are `menu(sseqWAIT)`.
            _ if matches!(Self::step_index_from_suffix(field), Some((_, "WAIT"))) => {
                Some(SSEQ_WAIT_CHOICES)
            }
            // The per-step `DOLnV`/`LNKnV` link-status fields are
            // `menu(sseqLNKV)` (sseqRecord.dbd:118,125).
            _ if Self::link_status_index(field).is_some() => Some(SSEQ_LNKV_CHOICES),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sseq_default() {
        let rec = SseqRecord::new();
        assert_eq!(rec.record_type(), "sseq");
        assert_eq!(rec.val, 0);
        assert_eq!(rec.selm, 0);
        assert_eq!(rec.seln, 1);
    }

    #[test]
    fn test_sseq_put_get_val() {
        let mut rec = SseqRecord::new();
        rec.put_field("VAL", EpicsValue::Long(42)).unwrap();
        assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Long(42)));
    }

    #[test]
    fn test_sseq_put_get_selm() {
        let mut rec = SseqRecord::new();
        // SELM is DBF_MENU (sseqRecord.dbd:34): served as DBR_ENUM. A Short
        // put is tolerated; the read-back is the native Enum index.
        rec.put_field("SELM", EpicsValue::Short(2)).unwrap();
        assert_eq!(rec.get_field("SELM"), Some(EpicsValue::Enum(2)));
    }

    #[test]
    fn test_sseq_step_fields() {
        let mut rec = SseqRecord::new();
        rec.put_field("DLY1", EpicsValue::Double(1.5)).unwrap();
        rec.put_field("DO1", EpicsValue::Double(42.0)).unwrap();
        rec.put_field("STR1", EpicsValue::String("hello".into()))
            .unwrap();
        rec.put_field("LNK1", EpicsValue::String("target.VAL".into()))
            .unwrap();
        rec.put_field("DOL1", EpicsValue::String("source.VAL".into()))
            .unwrap();
        rec.put_field("WAIT1", EpicsValue::Short(1)).unwrap();

        assert_eq!(rec.get_field("DLY1"), Some(EpicsValue::Double(1.5)));
        assert_eq!(rec.get_field("DO1"), Some(EpicsValue::Double(42.0)));
        assert_eq!(
            rec.get_field("STR1"),
            Some(EpicsValue::String("hello".into()))
        );
        assert_eq!(
            rec.get_field("LNK1"),
            Some(EpicsValue::String("target.VAL".into()))
        );
        assert_eq!(
            rec.get_field("DOL1"),
            Some(EpicsValue::String("source.VAL".into()))
        );
        assert_eq!(rec.get_field("WAIT1"), Some(EpicsValue::Short(1)));
    }

    #[test]
    fn test_sseq_step_a_suffix() {
        let mut rec = SseqRecord::new();
        rec.put_field("DLYA", EpicsValue::Double(2.0)).unwrap();
        rec.put_field("DOA", EpicsValue::Double(99.0)).unwrap();
        rec.put_field("STRA", EpicsValue::String("step10".into()))
            .unwrap();
        rec.put_field("LNKA", EpicsValue::String("out10.VAL".into()))
            .unwrap();

        assert_eq!(rec.get_field("DLYA"), Some(EpicsValue::Double(2.0)));
        assert_eq!(rec.get_field("DOA"), Some(EpicsValue::Double(99.0)));
        assert_eq!(
            rec.get_field("STRA"),
            Some(EpicsValue::String("step10".into()))
        );
        assert_eq!(
            rec.get_field("LNKA"),
            Some(EpicsValue::String("out10.VAL".into()))
        );
    }

    #[test]
    fn test_sseq_all_steps() {
        let mut rec = SseqRecord::new();
        // Set all 10 steps
        for i in 1..=9 {
            let dly_name = format!("DLY{}", i);
            rec.put_field(&dly_name, EpicsValue::Double(i as f64))
                .unwrap();
        }
        rec.put_field("DLYA", EpicsValue::Double(10.0)).unwrap();

        for i in 1..=9 {
            let dly_name = format!("DLY{}", i);
            assert_eq!(rec.get_field(&dly_name), Some(EpicsValue::Double(i as f64)));
        }
        assert_eq!(rec.get_field("DLYA"), Some(EpicsValue::Double(10.0)));
    }

    #[test]
    fn test_sseq_selm_all() {
        let rec = SseqRecord::new();
        for i in 0..NUM_STEPS {
            assert!(rec.should_execute_step(i));
        }
    }

    #[test]
    fn test_sseq_selm_specified() {
        let mut rec = SseqRecord::new();
        rec.selm = 1; // Specified
        rec.seln = 3; // Select step 3
        assert!(!rec.should_execute_step(0));
        assert!(!rec.should_execute_step(1));
        assert!(rec.should_execute_step(2)); // step 3 is index 2
        assert!(!rec.should_execute_step(3));
    }

    #[test]
    fn test_sseq_selm_mask() {
        let mut rec = SseqRecord::new();
        rec.selm = 2; // Mask
        rec.seln = 0b0000_0101; // Steps 1 and 3
        assert!(rec.should_execute_step(0));
        assert!(!rec.should_execute_step(1));
        assert!(rec.should_execute_step(2));
        assert!(!rec.should_execute_step(3));
    }

    #[test]
    fn test_sseq_process() {
        let mut rec = SseqRecord::new();
        rec.process().unwrap();
        assert_eq!(rec.busy, 0);
    }

    #[test]
    fn test_sseq_field_not_found() {
        let mut rec = SseqRecord::new();
        assert!(rec.put_field("ZZZ", EpicsValue::Double(1.0)).is_err());
        assert!(rec.get_field("ZZZ").is_none());
    }

    #[test]
    fn test_sseq_type_mismatch() {
        let mut rec = SseqRecord::new();
        assert!(
            rec.put_field("SELM", EpicsValue::String("x".into()))
                .is_err()
        );
        assert!(rec.put_field("STR1", EpicsValue::Double(1.0)).is_err());
    }

    #[test]
    fn test_sseq_field_list() {
        let rec = SseqRecord::new();
        let fields = rec.field_list();
        // 8 base (VAL/SELM/SELN/SELL/PREC/ABORT/ABORTING/BUSY) + 13 per
        // step * 10 steps = 138 fields (full sseqRecord.dbd surface).
        assert_eq!(fields.len(), 138);
        // The generated per-step table must not collide on a field name.
        let mut names: Vec<&str> = fields.iter().map(|f| f.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "field names must be unique");
    }

    #[test]
    fn test_sseq_status_fields_openable() {
        // The per-step diagnostics and top-level ABORTING must be
        // openable/readable (clients previously got field-not-found). With
        // no async context wired, the link-status fields hold their C
        // `init_record` classification of an empty (constant) link: `CON`
        // status and `DBF_unknown` (-1) field type (sseqRecord.c:204-225).
        let rec = SseqRecord::new();
        assert_eq!(rec.get_field("DT1"), Some(EpicsValue::Short(-1)));
        assert_eq!(rec.get_field("LT1"), Some(EpicsValue::Short(-1)));
        // WERRn = 0: an empty link with the default NoWait is not a misconfig.
        assert_eq!(rec.get_field("WERR1"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("WTG1"), Some(EpicsValue::Short(0)));
        // A-suffix step variants.
        assert_eq!(rec.get_field("DTA"), Some(EpicsValue::Short(-1)));
        assert_eq!(rec.get_field("WTGA"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("WERRA"), Some(EpicsValue::Short(0)));
        // DOLnV / LNKnV: empty link → sseqLNKV_CON (3).
        assert_eq!(rec.get_field("DOL1V"), Some(EpicsValue::Enum(3)));
        assert_eq!(rec.get_field("LNK1V"), Some(EpicsValue::Enum(3)));
        assert_eq!(rec.get_field("DOLAV"), Some(EpicsValue::Enum(3)));
        assert_eq!(rec.get_field("LNKAV"), Some(EpicsValue::Enum(3)));
        // Top-level ABORTING.
        assert_eq!(rec.get_field("ABORTING"), Some(EpicsValue::Short(0)));
    }

    #[test]
    fn test_sseq_werr_wait_on_constant_link_default() {
        // WERRn is redefined (vs C): WAITn set on a link that can never
        // deliver a completion — a Constant/unset (CON) link. A bare
        // SseqRecord with no async context still classifies an empty link
        // as CON, so a WAIT on it reads as a config error once a refresh
        // posts. Here, with no runtime, the default WERRn stays 0 (the
        // refresh that raises it needs the async surface); this pins that
        // the static default is non-erroring. The live raise/clear boundary
        // is covered by the async integration test.
        let mut rec = SseqRecord::new();
        rec.put_field("WAIT1", EpicsValue::Short(1)).unwrap();
        assert_eq!(rec.get_field("WERR1"), Some(EpicsValue::Short(0)));
        // The internal status post-back path stores what the refresh computes.
        rec.put_field("WERR1", EpicsValue::Short(1)).unwrap();
        assert_eq!(rec.get_field("WERR1"), Some(EpicsValue::Short(1)));
    }

    #[test]
    fn test_sseq_ix_is_step_index() {
        // IXn holds the step's own 0-based index (sseqRecord.dbd
        // initial: IX1=0 .. IXA=9).
        let rec = SseqRecord::new();
        assert_eq!(rec.get_field("IX1"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("IX2"), Some(EpicsValue::Short(1)));
        assert_eq!(rec.get_field("IX9"), Some(EpicsValue::Short(8)));
        assert_eq!(rec.get_field("IXA"), Some(EpicsValue::Short(9)));
    }

    #[test]
    fn test_sseq_link_status_menu_choices() {
        // DOLnV / LNKnV are menu(sseqLNKV); served as DBR_ENUM with the
        // four connection-status labels. The link fields they shadow
        // (DOL1 / LNK1) are NOT menus.
        let rec = SseqRecord::new();
        let choices = rec
            .menu_field_choices("DOL1V")
            .expect("DOL1V is a menu field");
        assert_eq!(choices, &["Ext PV NC", "Ext PV OK", "Local PV", "Constant"]);
        assert_eq!(rec.menu_field_choices("LNKAV"), Some(SSEQ_LNKV_CHOICES));
        assert!(rec.menu_field_choices("DOL1").is_none());
        assert!(rec.menu_field_choices("LNK1").is_none());
    }

    #[test]
    fn test_sseq_status_fields_read_only() {
        // Diagnostics are SPC_NOMOD in the DBD (sseqRecord.dbd); ABORTING's
        // live transition is machine-owned. All are read-only here. The
        // writable control/step fields stay writable.
        let rec = SseqRecord::new();
        let fields = rec.field_list();
        for ro_name in [
            "ABORTING", "DT1", "LT1", "WERR1", "WTG1", "IX1", "DOL1V", "LNK1V", "DTA", "LNKAV",
        ] {
            let f = fields
                .iter()
                .find(|f| f.name == ro_name)
                .unwrap_or_else(|| panic!("{ro_name} present in field_list"));
            assert!(f.read_only, "{ro_name} must be read-only");
        }
        for rw_name in ["VAL", "ABORT", "DLY1", "WAIT1", "LNK1", "STRA"] {
            let f = fields.iter().find(|f| f.name == rw_name).unwrap();
            assert!(!f.read_only, "{rw_name} stays writable");
        }
    }
}
