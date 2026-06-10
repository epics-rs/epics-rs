use crate::error::{CaError, CaResult};
use crate::server::database::AsyncDbHandle;
use crate::server::record::{
    FieldDesc, ProcessAction, ProcessOutcome, Record, RecordProcessResult,
};
use crate::types::{DbFieldType, EpicsValue, PvString};

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

/// Choice labels for the per-step DOL/LNK link-status menu, in index
/// order. C `menu(sseqLNKV)` (sseqRecord.dbd:20): 0=Ext PV NC, 1=Ext PV
/// OK, 2=Local PV, 3=Constant. Shared by every `DOLnV`/`LNKnV` field.
const SSEQ_LNKV_CHOICES: &[&str] = &["Ext PV NC", "Ext PV OK", "Local PV", "Constant"];

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
    /// `active[cursor]` issued a put-WITH-completion (`WAITn != NoWait`,
    /// C `dbCaPutLinkCallback`); the next continuation arrives when the
    /// downstream completes (C `putCallbackCB`).
    Wait,
}

/// A single step in the string sequence.
#[derive(Clone, Default)]
struct SseqStep {
    dly: f64,          // Delay before executing this step
    dol: String,       // Input link (DOLn)
    dov: f64,          // Numeric value (DOn)
    lnk: String,       // Output link (LNKn)
    str_val: PvString, // String value (STRn)
    wait: i16,         // Wait mode: 0=NoWait, 1=Wait, 2..=After1..After9
    waiting: bool,     // WTGn — an outstanding put-callback for this step
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
    /// Canonical record name + cycle-free database handle, stashed at
    /// `add_record` via [`Record::set_async_context`]. Drives out-of-band
    /// status posts (`BUSY`/`WTGn`/`ABORTING`) and the `ABORT` finish
    /// re-entry — the surfaces a `process()` cannot reach itself.
    async_ctx: Option<(String, AsyncDbHandle)>,
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
            async_ctx: None,
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
    /// C `processCallback` (sseqRecord.c:643-705) reads `DOLn` into both a
    /// string (`s`/`STRn`) and a double (`dov`/`DOn`) and writes whichever
    /// the destination field type wants. Here `pre_process_actions` has
    /// already folded a connected `DOLn` into `DOn` (a numeric read), so
    /// the precedence is: a connected `DOLn` → `DOn`; otherwise a non-empty
    /// `STRn` constant → the string; otherwise the `DOn` constant. A
    /// string-typed `DOLn` source coerces through `DOn` (Double) — the one
    /// value path `ReadDbLink` exposes (documented limitation).
    fn step_value(&self, i: usize) -> EpicsValue {
        let s = &self.steps[i];
        if s.dol.is_empty() && !s.str_val.is_empty() {
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
        self.schedule_current_step(live)
    }

    /// Schedule `active[cursor]` to fire after its `DLYn` delay, or finish
    /// the sequence when the cursor has passed the last active step.
    ///
    /// C `processNextLink` requests the per-step callback after `DLYn`
    /// (`callbackRequestDelayed`) or immediately when `DLYn == 0`
    /// (`callbackRequest`). `ReprocessAfter` is the uniform port of both:
    /// a `Duration::ZERO` delay still re-enters through the same path, so
    /// there is no special-cased `DLYn == 0` branch.
    fn schedule_current_step(&mut self, live: &mut Vec<(String, EpicsValue)>) -> ProcessOutcome {
        if self.cursor >= self.active.len() {
            self.finish(live);
            return ProcessOutcome::complete();
        }
        self.phase = SeqPhase::Fire;
        let i = self.active[self.cursor];
        let dly = std::time::Duration::from_secs_f64(self.steps[i].dly.max(0.0));
        ProcessOutcome {
            result: RecordProcessResult::AsyncPending,
            actions: vec![ProcessAction::ReprocessAfter(dly)],
            device_did_compute: false,
        }
    }

    /// Fire `active[cursor]` (C `processCallback`): forward the step value
    /// to `LNKn`, then either block for the put-callback (`WAITn`) or
    /// advance to the next step.
    fn fire_current_step(&mut self, live: &mut Vec<(String, EpicsValue)>) -> ProcessOutcome {
        let i = self.active[self.cursor];
        let value = self.step_value(i);
        let has_lnk = !self.steps[i].lnk.is_empty();
        // C `processCallback` (sseqRecord.c:717,739,763) uses
        // `dbCaPutLinkCallback` — the put-WITH-completion that sets the
        // `waiting` flag — only when `usePutCallback` (`WAITn != NoWait`).
        // `WriteDbLinkNotify` carries that completion wait; for a local /
        // bare target it drains immediately (no target process joins the
        // wait-set), matching C's synchronous `dbPutLink` there.
        let waits = self.steps[i].wait != 0;

        if waits && has_lnk {
            self.steps[i].waiting = true;
            live.push((WTG_FIELDS[i].to_string(), EpicsValue::Short(1)));
            self.phase = SeqPhase::Wait;
            return ProcessOutcome {
                result: RecordProcessResult::AsyncPending,
                actions: vec![ProcessAction::WriteDbLinkNotify {
                    link_field: LNK_FIELDS[i],
                    value,
                }],
                device_did_compute: false,
            };
        }

        // No-wait step: a plain `dbPutLink` (`WriteDbLink`), then advance in
        // the same cycle. The write rides ahead of the next step's
        // scheduling action (or the `Complete` tail's link-write phase for
        // the last step), so `LNKn` lands before the sequence moves on.
        let mut actions = Vec::new();
        if has_lnk {
            actions.push(ProcessAction::WriteDbLink {
                link_field: LNK_FIELDS[i],
                value,
            });
        }
        self.cursor += 1;
        let mut next = self.schedule_current_step(live);
        actions.append(&mut next.actions);
        next.actions = actions;
        next
    }

    /// Advance after a `WAITn` put-callback completion (C `putCallbackCB`):
    /// clear the step's `waiting` flag, then schedule the next step.
    fn after_wait(&mut self, live: &mut Vec<(String, EpicsValue)>) -> ProcessOutcome {
        let i = self.active[self.cursor];
        if self.steps[i].waiting {
            self.steps[i].waiting = false;
            live.push((WTG_FIELDS[i].to_string(), EpicsValue::Short(0)));
        }
        self.cursor += 1;
        self.schedule_current_step(live)
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
    }

    /// Drive an immediate abort completion from `special` (C
    /// `epicsTimerCancel` + `callbackRequest`): supersede any pending
    /// `DLYn` re-entry, then mint a fresh re-entry wired to an
    /// already-fired completion so the next `process()` runs the
    /// `abort != 0` finish at once. The superseded timer, when it wakes,
    /// re-enters nothing (the `AsyncToken` generation gate).
    fn force_finish_reentry(&self) {
        if let Some((name, handle)) = &self.async_ctx {
            let name = name.clone();
            let handle = handle.clone();
            tokio::spawn(async move {
                handle.cancel_async_reentry(&name).await;
                if let Some(token) = handle.mint_async_token(&name).await {
                    let (waitset, completion) = AsyncDbHandle::new_put_notify();
                    waitset.leave();
                    handle.reprocess_on_notify(token, completion);
                }
            });
        }
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
/// The read-only diagnostics fall into two groups. `WTGn` (outstanding
/// put-callback) and top-level `ABORTING` are driven LIVE by the sequence
/// machine (C `sseqRecord.c::processCallback`/`putCallbackCB`/`special`/
/// `asyncFinish`) and posted via `post_live`. `DTn`/`LTn` (DOL/LNK link
/// field type), `WERRn` (wait-config error), `IXn` (step index) and
/// `DOLnV`/`LNKnV` (link connection status, `menu(sseqLNKV)`) come from C
/// `checkLinks` — link introspection not modelled here — so they expose
/// their DBD init defaults.
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
        // An abort in flight finishes the sequence on its next re-entry,
        // ahead of any step work — C `process` takes the `pact` (completion)
        // path straight to `asyncFinish`, and `processCallback` /
        // `putCallbackCB` short-circuit to `process` when `pR->abort`.
        let outcome = if self.busy != 0 && self.abort != 0 {
            self.finish(&mut live);
            ProcessOutcome::complete()
        } else {
            // `busy == 0` (phase `Idle`) is a genuine start: the framework
            // PACT entry guard already rejected any foreign trigger while a
            // sequence was running, so reaching the body un-busy mirrors C's
            // `!pR->pact`. `busy == 1` is a per-step continuation.
            match self.phase {
                SeqPhase::Idle => self.start_sequence(&mut live),
                SeqPhase::Fire => self.fire_current_step(&mut live),
                SeqPhase::Wait => self.after_wait(&mut live),
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
        // C `sseqRecord.c::special` handles `ABORT` (SPC_MOD) entirely
        // outside the process cycle — a put to `ABORT` does NOT process the
        // record (only `VAL` is process-passive). `pR->abort` already holds
        // the value the put just stored.
        if !after || !field.eq_ignore_ascii_case("ABORT") {
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
            // Second abort while already aborting (C sseqRecord.c:1179-1190):
            // a downstream put-callback may be hung. Clear every `waiting`
            // flag, drop the remaining steps, and force the finish now.
            for i in 0..NUM_STEPS {
                if self.steps[i].waiting {
                    self.steps[i].waiting = false;
                    live.push((WTG_FIELDS[i].to_string(), EpicsValue::Short(0)));
                }
            }
            self.cursor = self.active.len();
            self.force_finish_reentry();
            self.post_live(live);
            return Ok(());
        }
        self.aborting = 1;
        live.push(("ABORTING".to_string(), EpicsValue::Short(1)));
        // C cancels a pending `DLYn` delay timer and completes the abort
        // immediately (sseqRecord.c:1194-1215). When instead waiting on a
        // put-callback (phase `Wait`), C does NOT force a re-entry — it lets
        // the outstanding callback arrive and finish via the `abort` branch
        // (sseqRecord.c:1161-1164). A still-stuck callback is escaped by a
        // second abort above.
        if self.phase == SeqPhase::Fire {
            self.force_finish_reentry();
        }
        self.post_live(live);
        Ok(())
    }

    fn set_async_context(&mut self, name: String, db: AsyncDbHandle) {
        self.async_ctx = Some((name, db));
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
    // per-step `LNKn` writes are driven here, in `process()`, via
    // `WriteDbLink`/`WriteDbLinkNotify` (C `sseqRecord.c::processCallback`),
    // not by the generic multi-output block. The retired
    // `dispatch_multi_output` `MultiOut::Sseq` arm was the old all-at-once
    // path and no longer exists.

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
                // the last char) does not recognise these. C init value is
                // sseqLNKV_EXT (1); the live connection status is computed
                // by `sseqRecord.c:checkLinks` (part of the async sequence
                // machine not yet ported).
                if Self::link_status_index(name).is_some() {
                    return Some(EpicsValue::Enum(1));
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
                        // Per-step diagnostics (sseqRecord.dbd). Read-only;
                        // their live values come from the async sequence
                        // owner (`sseqRecord.c:processNextLink`/`checkLinks`),
                        // not yet ported — exposed at their C init defaults
                        // so REC.DTn/LTn/WERRn/WTGn/IXn can be opened.
                        "DT" => Some(EpicsValue::Short(0)),
                        "LT" => Some(EpicsValue::Short(0)),
                        "WERR" => Some(EpicsValue::Short(0)),
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
        // openable/readable (clients previously got field-not-found).
        // They expose their sseqRecord.dbd init defaults: the live values
        // are driven by the async sequence owner, not yet ported.
        let rec = SseqRecord::new();
        assert_eq!(rec.get_field("DT1"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("LT1"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("WERR1"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("WTG1"), Some(EpicsValue::Short(0)));
        // A-suffix step variants.
        assert_eq!(rec.get_field("DTA"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("WTGA"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("WERRA"), Some(EpicsValue::Short(0)));
        // DOLnV / LNKnV menu init = sseqLNKV_EXT (1).
        assert_eq!(rec.get_field("DOL1V"), Some(EpicsValue::Enum(1)));
        assert_eq!(rec.get_field("LNK1V"), Some(EpicsValue::Enum(1)));
        assert_eq!(rec.get_field("DOLAV"), Some(EpicsValue::Enum(1)));
        assert_eq!(rec.get_field("LNKAV"), Some(EpicsValue::Enum(1)));
        // Top-level ABORTING.
        assert_eq!(rec.get_field("ABORTING"), Some(EpicsValue::Short(0)));
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
