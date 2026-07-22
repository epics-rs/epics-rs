use super::link_status::{
    DBF_NOACCESS, DBF_UNKNOWN, LINK_CON as LNKV_CON, LINK_LOC as LNKV_LOC,
    LINK_STATUS_CHOICES as SSEQ_LNKV_CHOICES, LinkRole, LinkStatusGen, classify_link,
    link_is_external,
};
use crate::error::{CaError, CaResult};
use crate::server::database::AsyncDbHandle;
use crate::server::record::{
    CyclePostMask, LinkReadAs, LinkType, OutTarget, ProcessAction, ProcessOutcome, Record,
    RecordProcessResult, parse_link_v2,
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

/// Size of a step's string view `STRn` — C `char s[40]` (`MAX_STRING_SIZE`,
/// sseqRecord.h). The `CHAR`/`UCHAR` destination arm of `processCallback`
/// clamps its element count to this buffer (`if (n_elements>40) n_elements=40`,
/// sseqRecord.c:683,766).
const SSEQ_STRING_SIZE: usize = 40;

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
const STR_FIELDS: [&str; NUM_STEPS] = [
    "STR1", "STR2", "STR3", "STR4", "STR5", "STR6", "STR7", "STR8", "STR9", "STRA",
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
/// C `waitConfigErr` (`checkLinks`, sseqRecord.c:912-933): the user asked to
/// wait on a link C cannot attach a put-completion callback to.
///
/// C raises it in exactly ONE of `checkLinks`'s three link-type branches — the
/// link is a `DB_LINK` (a local PV, so `LNKnV == LOC`) and `WAITn` is not
/// `NoWait` — and rescinds it in the other two (`CA_LINK`, and
/// `CONSTANT`/unset). A `WAITn` on a constant is NOT an error: there is no put
/// at all, so there is nothing to wait for.
///
/// This is the same question [`SseqRecord::lnk_is_ca`] asks for the fire-time
/// wait gate, from the other side: `fire_current_step` drops the wait on a
/// non-CA link, and `WERRn` is the record telling the user it was dropped.
fn wait_config_err(wait: i16, lnk_status: i16) -> i16 {
    i16::from(wait != 0 && lnk_status == LNKV_LOC)
}

/// Both halves of every step's value pair — the fields whose ONLY post path is
/// the record's own mark ([`Record::fields_posted_only_when_marked`]).
///
/// C never change-detects `DOn`/`STRn`: every post of either is an explicit
/// `db_post_events` made by the code that WROTE it, with that call site's mask
/// and that call site's own comparison (`special()` :1108-1116/:1128-1131,
/// `processCallback` :657-661/:676-680). Change detection cannot reproduce
/// them — it knows neither which of the two views was written (so it cannot
/// tell `DBE_VALUE|DBE_LOG` from a bare `DBE_VALUE`) nor that a client `caput`
/// to `DOn` already posted `DOn` on the put path. So the record marks both, and
/// the framework's change loop must not see them at all.
static VALUE_PAIR_FIELDS: [&str; 2 * NUM_STEPS] = [
    "DO1", "DO2", "DO3", "DO4", "DO5", "DO6", "DO7", "DO8", "DO9", "DOA", "STR1", "STR2", "STR3",
    "STR4", "STR5", "STR6", "STR7", "STR8", "STR9", "STRA",
];

/// Which half of a step's value pair a write came FROM — the other half is
/// DERIVED from it, and the two post with different masks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StepView {
    /// The write set `DOn`; `STRn` was re-rendered from it.
    Numeric,
    /// The write set `STRn`; `DOn` was re-read from it as `atof(s)`.
    String,
}

/// What a write to a step's value pair actually MOVED — the writer's report,
/// and the only thing that decides what posts. C tests each view separately
/// (`if (d != plinkGroup->dov)`, `if (strcmp(str, plinkGroup->s))`) and posts
/// only the ones that moved, so a re-write of the same value posts nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct StepWrite {
    /// `dov` differs from what it was before the write.
    numeric_changed: bool,
    /// `s` differs from what it was before the write.
    string_changed: bool,
}

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

/// A single step in the string sequence.
#[derive(Clone)]
struct SseqStep {
    dly: f64,          // Delay before executing this step
    dol: String,       // Input link (DOLn)
    dov: f64,          // Numeric value (DOn)
    lnk: String,       // Output link (LNKn)
    str_val: PvString, // String value (STRn)
    wait: i16,         // Wait mode: 0=NoWait, 1=Wait, 2..=After1..After9
    // WTGn — an outstanding put-callback for this step. C's `plinkGroup->waiting`
    // is a raw `DBF_SHORT` the machine drives 0/1; the machine treats non-zero
    // as "waiting". It is `DBF_SHORT` and not a bool because WTGA carries no
    // `special(SPC_NOMOD)` in `sseqRecord.dbd` (unlike WTG1..WTG9), so a client
    // `caput SQ.WTGA 5` stores 5 verbatim and reads it back — a bool would
    // collapse it to 1. See `ix` for the twin 10th-group `.dbd` omission.
    waiting: i16,
    // IXn — the step's own index. C's `plinkGroup->index`, a stored `DBF_SHORT`
    // (`initial(0)`..`initial(9)`), `special(SPC_NOMOD)` for IX1..IX9 but NOT
    // for IXA (the `.dbd` omits it), so `caput SQ.IXA 5` stores 5 where the
    // 1..9 puts are refused by the read-only gate. Stored, not computed from the
    // slot, so IXA can hold a client override the way C does.
    ix: i16,
    // Link-status diagnostics, refreshed by `refresh_link_status` from C
    // `sseqRecord.c:checkLinks` (sseqRecord.c:848-969). Defaulted to the
    // C `init_record` classification of an empty (constant) link.
    dol_status: i16,     // DOLnV — menu(sseqLNKV) DOL connection status
    lnk_status: i16,     // LNKnV — menu(sseqLNKV) LNK connection status
    dol_field_type: i16, // DTn — DOL target field type (C dbStatic DBF_* / -1)
    lnk_field_type: i16, // LTn — LNK target field type (C dbStatic DBF_* / -1)
    wait_err: i16,       // WERRn — wait-config error (see refresh_link_status)
    /// The `LNKn` TARGET as the framework resolved it for THIS cycle
    /// (`ProcessAction::ResolveOutTarget`, requested in `pre_process_actions`
    /// for the step about to fire) — C's `checkLinks`-cached `lnk_field_type`
    /// plus the `dbGetNelements` C re-reads at fire time. `fire_current_step`
    /// makes C's whole switch decision from it before anything is issued.
    /// `UNRESOLVED` until the framework fills it, which is the same answer C's
    /// `default:` arm acts on.
    lnk_target: OutTarget,
}

impl SseqStep {
    /// Write the step's value FROM its numeric view, re-deriving the string
    /// view — the single owner of `dov → s` (C `cvtDoubleToString(dov, s,
    /// pR->prec)`).
    ///
    /// `DOn` and `STRn` are two views of ONE value, and C reconciles them at
    /// every write site: `special()` on a `DOn` put (sseqRecord.c:1108-1116),
    /// `special()` on a `STRn` put (:1128-1131), `init_record` (:242-249), and
    /// the `processCallback` link read (:657-661, :676-680). A write that
    /// updates one view and leaves the other stale is the defect — so no site
    /// assigns `dov`/`str_val` directly; every one goes through this pair.
    ///
    /// `PREC` is `DBF_SHORT` and reaches C's
    /// `cvtDoubleToString(double, char*, epicsUInt16 prec)` by implicit
    /// conversion, which REINTERPRETS a negative value rather than clamping it:
    /// `PREC=-1` arrives as 65535, takes `cvtFast.c:111`'s `precision > 8`
    /// branch, caps at 17 and renders `%*.*e` — `STRn` of 3.7 becomes
    /// ` 3.70000000000000018e+00`, not `4`. `as u16` is that reinterpretation.
    fn set_numeric(&mut self, dov: f64, prec: i16) -> StepWrite {
        let s = PvString::from(crate::types::cvt_double_to_string(dov, prec as u16));
        let w = StepWrite {
            numeric_changed: self.dov != dov,
            string_changed: self.str_val != s,
        };
        self.dov = dov;
        self.str_val = s;
        w
    }

    /// Write the step's value FROM its string view, re-deriving the numeric
    /// view — the single owner of `s → dov` (C `dov = atof(s)`). The string
    /// is kept byte-exact; a non-numeric string is `0.0`, exactly as `atof`
    /// reports it.
    fn set_string(&mut self, s: PvString) -> StepWrite {
        let dov = EpicsValue::String(s.clone()).to_f64().unwrap_or(0.0);
        let w = StepWrite {
            numeric_changed: self.dov != dov,
            string_changed: self.str_val != s,
        };
        self.dov = dov;
        self.str_val = s;
        w
    }
}

impl Default for SseqStep {
    fn default() -> Self {
        Self {
            dly: 0.0,
            dol: String::new(),
            dov: 0.0,
            lnk: String::new(),
            str_val: PvString::default(),
            wait: 0,
            waiting: 0,
            // Fixed up to the slot index by `SseqRecord::default` (C
            // `initial(0)`..`initial(9)`); a bare array `Default` cannot know
            // its own position.
            ix: 0,
            // An empty link is a CONSTANT link. C `init_record` marks the DOL
            // one `DBF_NOACCESS` — it consumed the constant, once — and the LNK
            // one `DBF_unknown`, since a constant output has no target
            // (sseqRecord.c:204-206,222-225).
            dol_status: LNKV_CON,
            lnk_status: LNKV_CON,
            dol_field_type: DBF_NOACCESS,
            lnk_field_type: DBF_UNKNOWN,
            wait_err: 0,
            lnk_target: OutTarget::UNRESOLVED,
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
    /// The `DOn`/`STRn` posts this cycle's writes OWE, each with the mask of
    /// the C `db_post_events` call site that made it (see `mark_value_write`).
    /// Drained by [`Record::take_cycle_posted_fields`] — on a put by the put
    /// path, on a process cycle by the monitor loop.
    pending_posts: Vec<(&'static str, CyclePostMask)>,
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
        let mut steps: [SseqStep; NUM_STEPS] = Default::default();
        // C `sseqRecord.dbd` seeds each `IXn` with `initial(n-1)` — the step's
        // own 0-based index; the array `Default` cannot set it per slot.
        for (i, step) in steps.iter_mut().enumerate() {
            step.ix = i as i16;
        }
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
            steps,
            in_flight: Vec::new(),
            seq_wake: None,
            async_ctx: None,
            pending_posts: Vec::new(),
            link_gen: LinkStatusGen::default(),
        }
    }
}

impl SseqRecord {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the posts a write to step `i`'s value pair owes, from the WRITER's
    /// own report of what moved — the single owner of the `DOn`/`STRn` post
    /// decision.
    ///
    /// C's rule is uniform across all four write sites: the view the write came
    /// FROM posts `DBE_VALUE|DBE_LOG`, the view it DERIVED posts a bare
    /// `DBE_VALUE`, and each posts only if its own comparison moved.
    ///
    /// ```c
    /// /* processCallback, numeric arm (sseqRecord.c:672-683) */
    /// if (d != plinkGroup->dov) db_post_events(pR, &plinkGroup->dov, DBE_VALUE|DBE_LOG);
    /// cvtDoubleToString(plinkGroup->dov, str, pR->prec);
    /// if (strcmp(str, plinkGroup->s)) { strcpy(...); db_post_events(pR, &plinkGroup->s, DBE_VALUE); }
    /// /* special(), DOn put (sseqRecord.c:1112-1116) — dbPut already posted DOn */
    /// cvtDoubleToString(plinkGroup->dov, str, pR->prec);
    /// if (strcmp(str, plinkGroup->s)) { strcpy(...); db_post_events(pR, &plinkGroup->s, DBE_VALUE); }
    /// ```
    ///
    /// `posts_direct` is that `dbPut` difference and nothing else: on a client
    /// put the framework already posts the field written, so `special()` posts
    /// the DERIVED view alone; on a `processCallback` link read there is no put
    /// path, so the record owes BOTH.
    fn mark_value_write(&mut self, i: usize, wrote: StepView, w: StepWrite, posts_direct: bool) {
        let (direct, direct_changed, derived, derived_changed) = match wrote {
            StepView::Numeric => (
                DO_FIELDS[i],
                w.numeric_changed,
                STR_FIELDS[i],
                w.string_changed,
            ),
            StepView::String => (
                STR_FIELDS[i],
                w.string_changed,
                DO_FIELDS[i],
                w.numeric_changed,
            ),
        };
        if posts_direct && direct_changed {
            self.pending_posts.push((direct, CyclePostMask::ValueLog));
        }
        if derived_changed {
            self.pending_posts.push((derived, CyclePostMask::Value));
        }
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
            crate::runtime::task::spawn(async move {
                let _ = handle.post_fields(&name, fields);
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
            if self.steps[i].waiting != 0 {
                self.steps[i].waiting = 0;
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
            if self.steps[i].waiting != 0 {
                self.steps[i].waiting = 0;
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

    /// Whether step `i`'s `LNKn` is what C calls a `CA_LINK` — the single
    /// question C's put-with-completion turns on (`plinkGroup->lnk.type ==
    /// CA_LINK`, sseqRecord.c:717/739/763) and the one `checkLinks` turns
    /// `WERRn` on (:912-933).
    ///
    /// C decides it at `dbInitLink` by LOCALITY: an explicit `CA` link, or a
    /// PV name that is not a record of this IOC, is a `CA_LINK`; a name that
    /// resolves here is a `DB_LINK`; an empty link is `CONSTANT`. Both halves
    /// are answered here — the scheme from the link string itself, the
    /// locality from the cached `LNKnV` classification (`checkLinks`'s own
    /// `lnk_status`, which [`link_is_external`] reads as C's `CA_LINK`).
    ///
    /// C's `lnk.type` is likewise a CACHE, set at `init_record` and refreshed
    /// by `special()`/`checkLinks`. The port's cache is filled by the spawned
    /// [`Self::refresh_link_status`], so a DB-syntax link whose classification
    /// has not landed yet reads its `CON` default and is treated as NOT a CA
    /// link — the local reading, which is what the same link gets in C when
    /// the name does resolve here.
    fn lnk_is_ca(&self, i: usize) -> bool {
        match parse_link_v2(&self.steps[i].lnk).link_type() {
            // Explicit `ca://` / `pva://`: a CA link whatever it resolves to,
            // so this needs no classification round-trip.
            LinkType::Ca | LinkType::Other => true,
            // DB syntax: a CA link exactly when the name is NOT on this IOC.
            LinkType::Db => link_is_external(self.steps[i].lnk_status),
            // Constant / unset: C `CONSTANT`, never a CA link.
            LinkType::Empty | LinkType::Constant => false,
        }
    }

    /// Fire `active[cursor]` (C `processCallback`): forward the step value to
    /// `LNKn`, then advance. A `WAITn` step is dispatched WITHOUT blocking the
    /// machine — its put-callback joins `in_flight` and the sequence moves on,
    /// so several callbacks overlap exactly as C runs them.
    ///
    /// C's `processCallback` switch (sseqRecord.c:714-792) is ONE decision with
    /// two halves — the buffer that goes on the wire, and whether a
    /// put-callback (hence `waiting`) is issued at all — and its `default:`
    /// arm (:790) makes neither. So the buffer is decided FIRST, here, from the
    /// `LNKn` target this cycle's [`ProcessAction::ResolveOutTarget`] put in
    /// `lnk_target`; `None` is C's `default: break` and the step does nothing:
    /// no put, no `WTGn`, no `in_flight` entry to strand the sequence.
    ///
    /// Both put seams then carry the buffer this one decision produced — the
    /// framework never re-resolves the target and never makes a second,
    /// possibly different, no-put call.
    fn fire_current_step(&mut self, live: &mut Vec<(String, EpicsValue)>) -> ProcessOutcome {
        let i = self.active[self.cursor];
        let buffer = self.typed_output_buffer(LNK_FIELDS[i], &self.steps[i].lnk_target);
        // C `processCallback` (sseqRecord.c:717,739,763) uses the
        // put-WITH-completion (`dbCaPutLinkCallback`, setting `waiting`) only
        // when `usePutCallback && (plinkGroup->lnk.type == CA_LINK)` — and only
        // INSIDE a class arm, i.e. only where a put is made at all. A `WAITn` on
        // a DB link takes the plain `dbPutLink` and never waits — C cannot
        // attach a callback to it, and says so through `WERRn` (`checkLinks`,
        // :915-927).
        let waits = self.steps[i].wait != 0 && self.lnk_is_ca(i);

        if let Some(value) = buffer {
            if waits {
                // Non-blocking dispatch: the put-callback goes in flight and the
                // machine advances. C `processCallback` increments `pcb->index`
                // and calls `processNextLink` straight after firing, leaving the
                // just-fired step `waiting` for the barrier scan to honour.
                self.dispatch_waiting_step(i, value, live);
                self.cursor += 1;
                return self.advance_sequence(live);
            }

            // No-wait step: a plain `dbPutLink`, then advance in the same cycle.
            // The write rides ahead of the next step's scheduling action (or the
            // drain / `Complete` tail), so `LNKn` lands before the sequence
            // moves on.
            let mut actions = vec![ProcessAction::WriteDbLink {
                link_field: LNK_FIELDS[i],
                value,
            }];
            self.cursor += 1;
            let mut next = self.advance_sequence(live);
            actions.append(&mut next.actions);
            next.actions = actions;
            return next;
        }

        // C `default: break` — the target's class resolves to nothing this
        // record can put (empty/constant `LNKn`, a name that resolves nowhere,
        // an `INT64`/`UINT64` destination). No put, and therefore no wait.
        self.cursor += 1;
        self.advance_sequence(live)
    }

    /// Dispatch a `WAITn` step's put-with-completion without blocking the
    /// machine: record it in `in_flight`, raise `WTGn`, and spawn a waiter
    /// that marks the entry done and wakes the machine on completion.
    ///
    /// Called ONLY with the buffer [`Self::fire_current_step`] already decided
    /// on — C raises `waiting` inside the `dbCaPutLinkCallback` branch of a
    /// class arm (sseqRecord.c:727-729, :748-750, :779-781), never before the
    /// switch has said a put happens. `WTGn` up here and "no put" down there
    /// cannot come apart, because there is no "down there" left to decide.
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
        self.steps[i].waiting = 1;
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
            crate::runtime::task::spawn(async move {
                if let Some(rx) = handle
                    .put_link_notify(&name, LNK_FIELDS[i], &link, value)
                    .await
                {
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
        crate::runtime::task::spawn(async move {
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
            if self.steps[i].waiting != 0 {
                self.steps[i].waiting = 0;
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
            crate::runtime::task::spawn(async move {
                handle.cancel_async_reentry(&name);
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
        let sched = handle.clone();
        // Through the database's `iocInit` owner — see `schedule_record_init`.
        // The parking key; `name` itself moves into the future below.
        let init_key = name.clone();
        sched.schedule_record_init(&init_key, async move {
            let mut fields: Vec<(String, EpicsValue)> = Vec::with_capacity(NUM_STEPS * 5);
            for (i, (dol, lnk, wait)) in groups.iter().enumerate() {
                let (dol_status, dol_ft) = classify_link(&handle, dol, LinkRole::Input);
                let (lnk_status, lnk_ft) = classify_link(&handle, lnk, LinkRole::Output);
                let werr = wait_config_err(*wait, lnk_status);
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
                let _ = handle.post_fields(&name, fields);
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
    if let Some(token) = handle.mint_async_token(name) {
        let (waitset, completion) = AsyncDbHandle::new_put_notify();
        waitset.leave();
        handle.reprocess_on_notify(token, completion);
    }
}

impl Record for SseqRecord {
    fn record_type(&self) -> &'static str {
        "sseq"
    }

    /// C `sseqRecord.c::init_record` (197-200) rounds EVERY `DLYn` to a whole
    /// OS clock tick before the record can run:
    ///
    /// ```c
    /// for (index = 0; index < NUM_LINKS; index++, plinkGroup++) {
    ///     plinkGroup->dly = epicsThreadSleepQuantum() *
    ///         NINT(plinkGroup->dly/epicsThreadSleepQuantum());
    ///     db_post_events(pR, &plinkGroup->dly, DBE_VALUE);
    /// ```
    ///
    /// so a `DLY3=0.003` loaded from the db file is a 0.0 s delay, not 3 ms —
    /// the delay the record actually waits and the value a client reads back
    /// are the same rounded number. (C's `db_post_events` here runs during
    /// iocInit, before any client can subscribe, so it has no observable
    /// effect; the rounded field value does.)
    ///
    /// The same loop then reconciles each step's value pair
    /// (sseqRecord.c:242-249): a `.db` file may set `DOn`, `STRn`, or
    /// neither, and C makes the two views agree before the record can run —
    /// a configured `STRn` wins (`dov = atof(s)`), otherwise `STRn` is
    /// rendered from `DOn` at the record's PREC. Without it a
    /// `field(DO1,"3")` record starts with an empty `STR1`.
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            let prec = self.prec;
            for step in &mut self.steps {
                step.dly = crate::runtime::time::quantize_to_sleep_quantum(step.dly);
                // No mark: C's init-time `db_post_events` (:245, :248) run
                // before any client can subscribe.
                if step.str_val.is_empty() {
                    step.set_numeric(step.dov, prec);
                } else {
                    step.set_string(step.str_val.clone());
                }
            }
        }
        Ok(())
    }

    /// C `special()` re-quantizes on a put to any `DLYn` and posts DLY1 — see
    /// `special()` for the C quirk that makes it DLY1 and not the field written.
    ///
    /// The other `special()` side effect — the partner view of a `DOn`/`STRn`
    /// put — is NOT here: a static field-name list cannot say "only if it
    /// changed", and C posts the partner only when its own comparison moved. The
    /// writer reports that instead (`mark_value_write` →
    /// [`Record::take_cycle_posted_fields`]).
    fn monitor_side_effect_fields(&self, put_field: &str) -> &'static [&'static str] {
        match Self::step_index_from_suffix(put_field) {
            Some((_, "DLY")) => &["DLY1"],
            _ => &[],
        }
    }

    /// Every post of a `DOn`/`STRn` is one the record's own writer made
    /// (`mark_value_write`), with that C call site's mask and comparison.
    fn take_cycle_posted_fields(&mut self) -> Vec<(&'static str, CyclePostMask)> {
        std::mem::take(&mut self.pending_posts)
    }

    /// C change-detects neither view of a step's value pair; see
    /// `VALUE_PAIR_FIELDS`.
    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &VALUE_PAIR_FIELDS
    }

    /// C `sseqRecord.c:1155` posts the re-quantized DLY1 with a bare
    /// `DBE_VALUE`, not the framework default `DBE_VALUE | DBE_LOG`.
    fn value_only_change_fields(&self) -> &'static [&'static str] {
        &["DLY1"]
    }

    /// C `sseqRecord.c::asyncFinish` (474) posts `VAL` on EVERY process:
    ///
    /// ```c
    /// MonitorMask = DBE_VALUE | recGblResetAlarms(pR);
    /// if (MonitorMask) db_post_events(pR, &pR->val, MonitorMask);
    /// ```
    ///
    /// `DBE_VALUE` is set unconditionally, so a run of `caput VAL 1;2;2;3` posts
    /// FOUR value events — the repeated `2` posts again — where the framework
    /// default MDEL/deadband path dedupes the unchanged repeat to three. There
    /// is no value-change gate and no `DBE_LOG`: the mask is `DBE_VALUE` plus the
    /// alarm bits alone. Encode that as "value class always, archive class
    /// never": the change gate contributes nothing ([`Self::monitor_value_changed`]
    /// `= Some(false)`), and [`Self::monitor_always_post`] forces `DBE_VALUE`
    /// on while leaving `DBE_LOG` off. The alarm bits still reach `VAL` through
    /// the deadband post's `alarm_bits`, so an alarm transition posts it exactly
    /// as C's `recGblResetAlarms` term does.
    fn monitor_value_changed(&self) -> Option<bool> {
        Some(false)
    }

    fn monitor_always_post(&self) -> (bool, bool) {
        (true, false)
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
            let mut actions = Vec::with_capacity(2);
            if !self.steps[i].dol.is_empty() {
                actions.push(ProcessAction::ReadDbLink {
                    link_field: DOL_FIELDS[i],
                    target_field: DO_FIELDS[i],
                });
            }
            // The step's OUT switch (sseqRecord.c:706-793) needs the `LNKn`
            // target's class, and `fire_current_step` needs it BEFORE it decides
            // anything — the same decision says which buffer goes out and
            // whether a put-callback (and `WTGn`) is raised. Requested every
            // fire, unconditionally: a step whose `LNKn` is empty or points
            // nowhere must see THIS cycle's "no target", not a stale one.
            actions.push(ProcessAction::ResolveOutTarget {
                link_field: LNK_FIELDS[i],
            });
            return actions;
        }
        Vec::new()
    }

    fn set_resolved_out_target(&mut self, link_field: &str, target: OutTarget) {
        if let Some((i, "LNK")) = Self::step_index_from_suffix(link_field) {
            self.steps[i].lnk_target = target;
        }
    }

    /// C `sseqRecord.c::asyncFinish` posts VAL (`:474`) and runs
    /// `recGblFwdLink` (`:499`) BEFORE `recGblGetTimeStamp` (`:501`), so the
    /// first VAL monitor event carries the record's pre-update timestamp and
    /// TIME advances only for the following BUSY post and the next cycle. The
    /// framework `Complete` tail defers the restamp accordingly.
    fn restamps_time_after_completion(&self) -> bool {
        true
    }

    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after {
            return Ok(());
        }
        // A put to any DLYn re-quantizes DLY1 — yes, DLY1, not the field that
        // was written. C `sseqRecord.c::special` (1140-1156) computes
        // `lnkIndex` from the put field and then never applies it, unlike the
        // STRn case right above it which does `plinkGroup += lnkIndex`:
        //
        //     lnkIndex = ((char *)paddr->pfield - (char *)&pR->dly1) /
        //                sizeof(struct linkGroup);
        //     plinkGroup = (struct linkGroup *)&pR->dly1;
        //     plinkGroup->dly = epicsThreadSleepQuantum() *
        //                       NINT(plinkGroup->dly/epicsThreadSleepQuantum());
        //     db_post_events(pR, &plinkGroup->dly, DBE_VALUE);
        //
        // So `plinkGroup` still points at the DLY1 group. The put DLYn keeps
        // its raw value and DLY1 is what gets rounded and posted. Present since
        // the record was moved into the calc module (calc c9115f8) and never
        // corrected upstream; it is C's observable behaviour, so it is the
        // contract. (Every DLYn IS quantized — at init, see `init_record`.)
        if Self::step_index_from_suffix(field).is_some_and(|(_, p)| p == "DLY") {
            self.steps[0].dly = crate::runtime::time::quantize_to_sleep_quantum(self.steps[0].dly);
            // The DBE_VALUE post of DLY1 is the framework's, via
            // `monitor_side_effect_fields` + `value_only_change_fields`.
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
        // ABORTING is `special(SPC_MOD)` but C `sseqRecord.c::special` has NO
        // `case(sseqRecordABORTING)` — it falls to the `default:` arm,
        // `recGblDbaddrError(S_db_badChoice, ...); return(S_db_badChoice)`
        // (sseqRecord.c:1218-1220). `dbPut` had already stored the value (so
        // `aborting` reads back what the client wrote), but the put STATUS is a
        // failure. ABORTING is the ONLY SPC_MOD sseq field with no switch case;
        // DLYn/DOn/DOLn/LNKn/STRn/WAITn all have one and are accepted, so this
        // is the whole rejected-family besides ABORT-when-idle below.
        if field.eq_ignore_ascii_case("ABORTING") {
            return Err(CaError::BadChoice("ABORTING".to_string()));
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
            // C: "no activity to abort" (sseqRecord.c:1173-1178) — reset `abort`,
            // post, and `return(-1)`: the put is REJECTED. The reset/post is done
            // first (as C does), only the status is a failure — the abort state
            // machine is untouched.
            if self.abort != 0 {
                self.abort = 0;
                live.push(("ABORT".to_string(), EpicsValue::Short(0)));
            }
            self.post_live(live);
            return Err(CaError::BadField("ABORT".to_string()));
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
                if self.steps[i].waiting != 0 {
                    self.steps[i].waiting = 0;
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
    // via `WriteDbLink`, a `WAITn` step via the `put_link_notify` seam, both
    // carrying the buffer `fire_current_step` already decided on (C
    // `sseqRecord.c::processCallback`) — not by the generic multi-output block.
    // The retired `dispatch_multi_output` `MultiOut::Sseq` arm was the old
    // all-at-once path and no longer exists.

    /// C `processCallback`'s SOURCE switch (sseqRecord.c:640-705) — the read
    /// twin of the destination switch below. The step reads `DOLn` with the DBR
    /// class the SOURCE's DBF class asks for, never the source's native value:
    ///
    /// - `DBF_STRING`/`ENUM`/`MENU`/`DEVICE`/`INLINK`/`OUTLINK`/`FWDLINK`
    ///   (:644-661) — `DBR_STRING` into `s`/`STRn`, then `dov = atof(s)`. For an
    ///   `ENUM`/`MENU` source that is the state LABEL ("Busy"), so `DOn` becomes
    ///   `atof("Busy") == 0`, NOT the state index. The seven classes are the same
    ///   set [`OutTarget::puts_as_string`] reports for the destination switch.
    /// - `DBF_SHORT`..`DBF_DOUBLE` (:663-680) — `DBR_DOUBLE` into `dov`/`DOn`,
    ///   then `STRn` is re-rendered at the record's PREC.
    /// - `DBF_CHAR`/`DBF_UCHAR` (:682-700) — `min(n_elements, 40)` bytes into the
    ///   40-byte `s`, then `dov = atof(s)`. The long-string idiom; a scalar
    ///   `CHAR` source contributes its one byte, as C's `n_elements == 1` read
    ///   does.
    /// - anything else (:703, `default: break`) — **no read at all**: a CONSTANT
    ///   `DOLn` (C's `init_record` pins it to `DBF_NOACCESS`, :206, so the switch
    ///   never matches again — the constant is seeded once, at init, see
    ///   [`Self::constant_init_links`]), a source whose type does not resolve,
    ///   and `DBF_INT64`/`DBF_UINT64`, which the switch has no case for.
    ///
    /// `SELL` is not part of this switch: C reads it with a FIXED
    /// `dbGetLink(&pR->sell, DBF_USHORT, &pR->seln, 0, 0)` (:314-317), so it
    /// keeps the framework's native read and `SELN`'s own put coercion.
    fn input_link_read_as(&self, link_field: &str, source: &OutTarget) -> Option<LinkReadAs> {
        let Some((_, "DOL")) = Self::step_index_from_suffix(link_field) else {
            return Some(LinkReadAs::Native);
        };
        if source.puts_as_string {
            return Some(LinkReadAs::String);
        }
        match source.field_type? {
            DbFieldType::String | DbFieldType::Enum => Some(LinkReadAs::String),
            DbFieldType::Char | DbFieldType::UChar => Some(LinkReadAs::CharArrayAsString {
                // C `if (n_elements>40) n_elements=40` against `char s[40]`.
                max_elements: source.element_count.clamp(1, SSEQ_STRING_SIZE as i64) as usize,
            }),
            DbFieldType::Short
            | DbFieldType::UShort
            | DbFieldType::Long
            | DbFieldType::ULong
            | DbFieldType::Float
            | DbFieldType::Double => Some(LinkReadAs::Double),
            DbFieldType::Int64 | DbFieldType::UInt64 => None,
        }
    }

    /// C `init_record` (sseqRecord.c:203-207): a CONSTANT `DOLn` is loaded ONCE,
    /// here — `recGblInitConstantLink(&dol, DBF_DOUBLE, &dov)` then
    /// `(&dol, DBF_STRING, s)`, followed by the `s[0] ? dov = atof(s)` tail
    /// (:243-245), whose net effect is the record's own `set_string` of the link
    /// TEXT. Seeding `STRn` (not `DOn`) reproduces both halves: the string view
    /// keeps the constant's raw text ("1.50" stays "1.50", not the PREC
    /// rendering of 1.5) and the numeric view follows as `atof`.
    ///
    /// The per-cycle read never sees a constant again — C pins its
    /// `dol_field_type` to `DBF_NOACCESS`, which falls to `processCallback`'s
    /// `default: break` ([`Self::input_link_read_as`]).
    ///
    /// `SELL → SELN` is the other seed C declares (`sseqRecord.c:186-191`,
    /// `recGblInitConstantLink(&pR->sell, DBF_USHORT, &pR->seln)` — the same one
    /// `dfanout`/`seq` declare). Without it a constant `SELL` reached `SELN`
    /// only through the per-cycle `ReadDbLink`, which re-applied it on every
    /// process and stomped a client's `caput SELN`; now that the link layer
    /// delivers nothing for a constant at process time, init is the only path
    /// left — and it is C's.
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        (0..NUM_STEPS)
            .map(|i| crate::server::record::ConstantInitLink::new(DOL_FIELDS[i], STR_FIELDS[i]))
            .chain(std::iter::once(
                crate::server::record::ConstantInitLink::new("SELL", "SELN"),
            ))
            .collect()
    }

    /// C `processCallback`'s destination switch (sseqRecord.c:706-793): the
    /// step forwards the view of its value that the `LNKn` TARGET's DBF class
    /// asks for, never the one its `DOLn` source happened to deliver.
    ///
    /// `DOn`/`STRn` are two views of one value (C `dov` / `s`), so both are
    /// always available here — which one goes on the wire is the destination's
    /// choice, exactly as C's `switch (plinkGroup->lnk_field_type)`:
    ///
    /// - `DBF_STRING`/`ENUM`/`MENU`/`DEVICE`/`INLINK`/`OUTLINK`/`FWDLINK`
    ///   (:714-736) — `DBR_STRING` from `s`/`STRn`. All seven classes are
    ///   reported by [`OutTarget::puts_as_string`] (the port's `DbFieldType`
    ///   is a DBR wire type, with no `Menu`/`Device` variant to match on).
    /// - `DBF_SHORT`/`USHORT`/`LONG`/`ULONG`/`FLOAT`/`DOUBLE` (:738-760) —
    ///   `DBR_DOUBLE` from `dov`/`DOn`.
    /// - `DBF_CHAR`/`DBF_UCHAR` (:762-790) — `n_elements > 1` (the long-string
    ///   idiom: a `CHAR` waveform) puts `min(n_elements, 40)` bytes of the
    ///   40-byte `s` as a char array; a scalar `CHAR` target takes `DBR_DOUBLE`
    ///   from `dov`.
    /// - anything else (:792, `default: break`) — **no put at all**. That
    ///   covers a `LNKn` whose type does not resolve (constant link,
    ///   disconnected CA link: `dbGetLinkDBFtype` → `DBF_unknown`) and, in C
    ///   as here, `DBF_INT64`/`DBF_UINT64`: the switch has no case for the
    ///   64-bit integer classes, so an `int64out` destination is silently not
    ///   written. That is C's behaviour, reproduced rather than repaired.
    fn typed_output_buffer(&self, link_field: &str, target: &OutTarget) -> Option<EpicsValue> {
        let Some((i, "LNK")) = Self::step_index_from_suffix(link_field) else {
            return None;
        };
        let step = &self.steps[i];
        if target.puts_as_string {
            return Some(EpicsValue::String(step.str_val.clone()));
        }
        match target.field_type? {
            DbFieldType::String | DbFieldType::Enum => {
                Some(EpicsValue::String(step.str_val.clone()))
            }
            DbFieldType::Char | DbFieldType::UChar => {
                if target.element_count > 1 {
                    // C puts `n` bytes straight out of the 40-byte `s`, so the
                    // string is NUL-padded out to the requested count.
                    let n = target.element_count.min(SSEQ_STRING_SIZE as i64) as usize;
                    let mut buf = step.str_val.as_bytes().to_vec();
                    buf.resize(n, 0);
                    Some(EpicsValue::CharArray(buf))
                } else {
                    Some(EpicsValue::Double(step.dov))
                }
            }
            DbFieldType::Short
            | DbFieldType::UShort
            | DbFieldType::Long
            | DbFieldType::ULong
            | DbFieldType::Float
            | DbFieldType::Double => Some(EpicsValue::Double(step.dov)),
            // C's switch has no `DBF_INT64`/`DBF_UINT64` case → `default:
            // break` → no put.
            DbFieldType::Int64 | DbFieldType::UInt64 => None,
        }
    }

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
                        "WTG" => Some(EpicsValue::Short(self.steps[idx].waiting)),
                        // IXn holds the step's own 0-based index
                        // (sseqRecord.dbd initial: IX1=0 .. IXA=9). Served from
                        // storage so a `caput SQ.IXA` override survives (IX1..IX9
                        // are read-only, so their cell stays at the init index).
                        "IX" => Some(EpicsValue::Short(step.ix)),
                        _ => None,
                    };
                }
                None
            }
        }
    }

    fn put_field_internal(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        // The framework's typed link read (`ProcessAction::ReadDbLink` →
        // `DOn`) delivers what C's `dbGetLink` delivered for the class the
        // step's `DOLn` SOURCE resolved to (see `input_link_read_as`): a
        // `DBR_STRING` for the string-class and `CHAR`-array arms, a
        // `DBR_DOUBLE` for the numeric one. Both land in the step's value pair
        // through its two owners below — a string keeps its bytes and `dov`
        // follows as `atof(s)`; a double re-renders `STRn` at PREC. A *client*
        // put to `DOn` (`put_field`) is a plain `DBF_DOUBLE` convert, so this
        // string-preserving capture is internal-only. Which view then reaches
        // `LNKn` is the DESTINATION's choice, not the source's — see
        // `typed_output_buffer`.
        if let Some((idx, "DO")) = Self::step_index_from_suffix(name) {
            match value {
                // C `processCallback` string arm (:657-661): the bytes are kept
                // exactly and `dov` follows as `atof(s)`.
                EpicsValue::String(s) => {
                    let w = self.steps[idx].set_string(s);
                    // Both views are the record's to post here: no put path ran,
                    // so nothing else posts `STRn` (:661) or `DOn` (:665).
                    self.mark_value_write(idx, StepView::String, w, true);
                    return Ok(());
                }
                // C `processCallback` numeric arm (:676-680): after reading
                // `dov` it renders `s` with `cvtDoubleToString(dov, str, prec)`
                // rather than leaving the prior string stale.
                other => {
                    let dov = other
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                    let prec = self.prec;
                    let w = self.steps[idx].set_numeric(dov, prec);
                    self.mark_value_write(idx, StepView::Numeric, w, true);
                    return Ok(());
                }
            }
        }
        // Every OTHER field takes the framework's internal-put path, which is what
        // applies C's `dbGetLink(DBF_<target>)` coercion: the target field's DBF
        // type, an array source reduced to element 0 (C asks for one element, so
        // `dbGet` converts offset 0), and a pvalink NTEnum carrier collapsed. Ending
        // in `self.put_field` skipped all of it and left those writes at the mercy of
        // whatever types each typed arm happened to accept — `SELN`, the target of
        // the SELL link read, would reject an array source outright. acalcout's
        // override (`acalcout.rs:1672`) already routes through here; this is the
        // same rule.
        crate::server::record::put_field_internal_default(self, name, value)
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
                    let prec = self.prec;
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
                        // C `special()` on a `DOn` put (sseqRecord.c:1108-1116)
                        // re-renders `STRn` from the new `DOn` at the record's
                        // PREC — the two are one value, never independent.
                        "DO" => {
                            let dov = value
                                .to_f64()
                                .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                            let w = step.set_numeric(dov, prec);
                            // The put path posts `DOn` itself (C `dbPut`), so
                            // `special()` posts only the derived `STRn`.
                            self.mark_value_write(idx, StepView::Numeric, w, false);
                            Ok(())
                        }
                        "LNK" => match value {
                            EpicsValue::String(s) => {
                                step.lnk = s.as_str_lossy().into_owned();
                                Ok(())
                            }
                            _ => Err(CaError::TypeMismatch(name.into())),
                        },
                        // C `special()` on a `STRn` put (sseqRecord.c:1128-1131)
                        // re-reads `DOn` as `atof(s)`.
                        "STR" => match value {
                            EpicsValue::String(s) => {
                                let w = step.set_string(s);
                                self.mark_value_write(idx, StepView::String, w, false);
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
                        // WTG1..WTG9 are `special(SPC_NOMOD)` — the read-only
                        // gate refuses a client put, and only the machine's own
                        // `post_fields` (`put_field_internal`) reaches here with
                        // the 0/1 flag it just set. WTGA carries NO `SPC_NOMOD`
                        // in `sseqRecord.dbd`, so a client `caput SQ.WTGA v` also
                        // lands here and C stores `v` verbatim in the raw short.
                        // Store the raw `DBF_SHORT` either way (`as i16`), not a
                        // 0/1 bool, so the client override reads back unchanged.
                        "WTG" => {
                            step.waiting = value
                                .to_f64()
                                .ok_or_else(|| CaError::TypeMismatch(name.into()))?
                                as i16;
                            Ok(())
                        }
                        // IX1..IX9 are `special(SPC_NOMOD)` (gate-refused);
                        // IXA is not, so `caput SQ.IXA v` stores `v` in the step's
                        // raw index short exactly as C does. Reached only for IXA
                        // on the client path (1..9 are refused upstream).
                        "IX" => {
                            step.ix = value
                                .to_f64()
                                .ok_or_else(|| CaError::TypeMismatch(name.into()))?
                                as i16;
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
    use super::super::link_status::LINK_EXT_NC as LNKV_EXT_NC;
    use super::*;
    use crate::server::record::FieldDeclaration;

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
        // DO1/STR1 are ONE value (C `special()`): the later `STR1="hello"` put
        // re-read DO1 as `atof("hello")` = 0.0, dropping the earlier 42.0.
        assert_eq!(rec.get_field("DO1"), Some(EpicsValue::Double(0.0)));
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
        // As above: the `STRA="step10"` put re-read DOA as `atof` = 0.0.
        assert_eq!(rec.get_field("DOA"), Some(EpicsValue::Double(0.0)));
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
        // status, and the field-type code of the link's DIRECTION —
        // `DBF_NOACCESS` (17) for the consumed DOL constant, `DBF_unknown` (-1)
        // for the LNK constant, which has no target (sseqRecord.c:204-225).
        let rec = SseqRecord::new();
        assert_eq!(rec.get_field("DT1"), Some(EpicsValue::Short(17)));
        assert_eq!(rec.get_field("LT1"), Some(EpicsValue::Short(-1)));
        // WERRn = 0: an empty link with the default NoWait is not a misconfig.
        assert_eq!(rec.get_field("WERR1"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("WTG1"), Some(EpicsValue::Short(0)));
        // A-suffix step variants.
        assert_eq!(rec.get_field("DTA"), Some(EpicsValue::Short(17)));
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

    /// R16-3 — `WERRn` follows C `checkLinks` (sseqRecord.c:912-933): raised
    /// ONLY for a local DB link with `WAITn` set, cleared for a CA link and
    /// for a constant. One case per branch of C's link-type switch, times the
    /// `WAITn` on/off boundary.
    #[test]
    fn test_sseq_wait_config_err_matches_c_check_links() {
        // DB_LINK (local PV) + wait: the one branch C raises. C cannot
        // dbCaPutLinkCallback a DB link, so the requested wait is dropped.
        assert_eq!(wait_config_err(1, LNKV_LOC), 1);
        // Same link, NoWait: rescinded.
        assert_eq!(wait_config_err(0, LNKV_LOC), 0);
        // CA_LINK: the wait works — never an error, wait or not. Both external
        // statuses ("Ext PV NC" = 0, "Ext PV OK" = 1) are CA links to C.
        assert_eq!(wait_config_err(1, LNKV_EXT_NC), 0);
        assert_eq!(wait_config_err(1, 1), 0);
        // CONSTANT / unset: no put is issued at all, so nothing to wait for.
        // C's `else` branch rescinds the error here; the pre-fix port RAISED
        // it, which is the inversion R16-3 names.
        assert_eq!(wait_config_err(1, LNKV_CON), 0);
        assert_eq!(wait_config_err(0, LNKV_CON), 0);
    }

    /// R16-3 — the fire-time wait gate keys on C's `lnk.type == CA_LINK`
    /// (sseqRecord.c:717/739/763), which `dbInitLink` decides by LOCALITY.
    /// One case per `LinkType`, plus the DB-syntax local/remote split.
    #[test]
    fn test_sseq_lnk_is_ca_matches_c_link_type() {
        let mut rec = SseqRecord::new();
        // Empty / constant → C CONSTANT: never a CA link.
        assert!(!rec.lnk_is_ca(0));
        rec.put_field("LNK1", EpicsValue::String("3.14".into()))
            .unwrap();
        assert!(!rec.lnk_is_ca(0));
        // Explicit ca:// → CA_LINK regardless of what it resolves to.
        rec.put_field("LNK1", EpicsValue::String("ca://other:pv.VAL".into()))
            .unwrap();
        assert!(rec.lnk_is_ca(0));
        // DB syntax naming a record of THIS IOC → DB_LINK.
        rec.put_field("LNK1", EpicsValue::String("local:pv.VAL PP".into()))
            .unwrap();
        rec.steps[0].lnk_status = LNKV_LOC;
        assert!(!rec.lnk_is_ca(0));
        // DB syntax naming a PV that is NOT on this IOC → C resolves it
        // through CA, so it is a CA_LINK.
        rec.steps[0].lnk_status = LNKV_EXT_NC;
        assert!(rec.lnk_is_ca(0));
        // DB syntax not yet classified (the refresh has not landed): read as
        // the LOCAL case, never as a CA link — a step must never park on a
        // put-callback the link cannot deliver.
        rec.steps[0].lnk_status = LNKV_CON;
        assert!(!rec.lnk_is_ca(0));
    }

    /// R16-2 — `DOn` and `STRn` are two views of ONE value, reconciled at
    /// every write. C does it at four sites; the port had only the link-read
    /// one. Boundaries: numeric put → string view re-rendered at PREC; string
    /// put → numeric view re-read as `atof`; a non-numeric string → 0.0; the
    /// pair does not go stale across a put to the other half.
    #[test]
    fn test_sseq_do_str_are_one_value() {
        let mut rec = SseqRecord::new();
        rec.put_field("PREC", EpicsValue::Short(2)).unwrap();

        // C special(DOn): cvtDoubleToString(dov, s, prec).
        rec.put_field("DO1", EpicsValue::Double(3.7)).unwrap();
        assert_eq!(rec.get_field("DO1"), Some(EpicsValue::Double(3.7)));
        assert_eq!(
            rec.get_field("STR1"),
            Some(EpicsValue::String("3.70".into())),
            "a DOn put must re-render STRn at the record's PREC"
        );

        // C special(STRn): dov = atof(s).
        rec.put_field("STR1", EpicsValue::String("5".into()))
            .unwrap();
        assert_eq!(rec.get_field("STR1"), Some(EpicsValue::String("5".into())));
        assert_eq!(
            rec.get_field("DO1"),
            Some(EpicsValue::Double(5.0)),
            "a STRn put must re-read DOn as atof(STRn)"
        );

        // A non-numeric string is atof → 0.0, and the string stays byte-exact.
        rec.put_field("STRA", EpicsValue::String("abc".into()))
            .unwrap();
        assert_eq!(rec.get_field("DOA"), Some(EpicsValue::Double(0.0)));
        assert_eq!(
            rec.get_field("STRA"),
            Some(EpicsValue::String("abc".into()))
        );

        // STRn="abc" then DOn=3.7 → the string follows the LAST write, so the
        // step forwards "3.70", not the stale "abc".
        rec.put_field("DOA", EpicsValue::Double(3.7)).unwrap();
        assert_eq!(
            rec.get_field("STRA"),
            Some(EpicsValue::String("3.70".into())),
            "the later DOn put must overwrite the earlier STRn"
        );
    }

    /// R16-2, init boundary — C `init_record` (sseqRecord.c:242-249)
    /// reconciles the pair before the record can run: a `.db`-configured
    /// `STRn` wins (`dov = atof(s)`), otherwise `STRn` is rendered from `DOn`.
    #[test]
    fn test_sseq_init_reconciles_the_value_pair() {
        // field(DO1,"3") with no STR1 → C renders STR1 = "3.00" at PREC=2.
        let mut rec = SseqRecord::new();
        rec.put_field("PREC", EpicsValue::Short(2)).unwrap();
        rec.steps[0].dov = 3.0;
        rec.init_record(0).unwrap();
        assert_eq!(
            rec.get_field("STR1"),
            Some(EpicsValue::String("3.00".into()))
        );

        // field(STR2,"7.5") with no DO2 → C takes dov = atof(s) = 7.5.
        let mut rec = SseqRecord::new();
        rec.steps[1].str_val = PvString::from("7.5");
        rec.init_record(0).unwrap();
        assert_eq!(rec.get_field("DO2"), Some(EpicsValue::Double(7.5)));
        assert_eq!(
            rec.get_field("STR2"),
            Some(EpicsValue::String("7.5".into())),
            "the configured string is kept byte-exact, not re-rendered"
        );
    }

    /// R16-2 / R17-4 — a put to one view posts the other, but the WRITER names
    /// it, with C's mask (a bare `DBE_VALUE`) and C's change test. The static
    /// list this test used to assert could express neither, so it over-posted:
    /// it re-posted the partner even when C's `strcmp`/`!=` guard did not fire,
    /// and it posted it with `DBE_VALUE|DBE_LOG`.
    #[test]
    fn test_sseq_value_pair_posts_its_partner() {
        let mut rec = SseqRecord::new();
        rec.put_field("PREC", EpicsValue::Short(2)).unwrap();

        rec.put_field("DO1", EpicsValue::Double(3.7)).unwrap();
        assert_eq!(
            rec.take_cycle_posted_fields(),
            vec![("STR1", CyclePostMask::Value)],
            "a DOn put posts only the DERIVED STRn (dbPut posts DOn itself)"
        );

        rec.put_field("DO1", EpicsValue::Double(3.7)).unwrap();
        assert!(
            rec.take_cycle_posted_fields().is_empty(),
            "STR1 still renders \"3.70\": C's strcmp guard posts nothing"
        );

        rec.put_field("STRA", EpicsValue::String("5".into()))
            .unwrap();
        assert_eq!(
            rec.take_cycle_posted_fields(),
            vec![("DOA", CyclePostMask::Value)],
            "a STRn put posts only the derived DOn = atof(s)"
        );

        // The DLY quirk is a genuine static side effect and stays one.
        assert_eq!(rec.monitor_side_effect_fields("DLY3"), &["DLY1"]);
        assert!(rec.monitor_side_effect_fields("DO1").is_empty());
        assert!(rec.monitor_side_effect_fields("LNK1").is_empty());
    }

    /// The `processCallback` link-read arms owe BOTH posts — no put path ran, so
    /// nothing else posts the view that was read (sseqRecord.c:676-680).
    #[test]
    fn test_sseq_link_read_posts_both_views() {
        let mut rec = SseqRecord::new();
        rec.put_field("PREC", EpicsValue::Short(2)).unwrap();

        rec.put_field_internal("DO1", EpicsValue::Double(3.7))
            .unwrap();
        assert_eq!(
            rec.take_cycle_posted_fields(),
            vec![
                ("DO1", CyclePostMask::ValueLog),
                ("STR1", CyclePostMask::Value)
            ],
            "numeric arm: dov with DBE_VALUE|DBE_LOG, the re-rendered s with DBE_VALUE"
        );

        rec.put_field_internal("DO2", EpicsValue::String("abc".into()))
            .unwrap();
        assert_eq!(
            rec.take_cycle_posted_fields(),
            vec![("STR2", CyclePostMask::ValueLog)],
            "string arm: s with DBE_VALUE|DBE_LOG; dov stayed 0.0 (atof(\"abc\")), \
             and C posts it only `if (d != plinkGroup->dov)`"
        );
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
        // The step diagnostics are `special(SPC_NOMOD)` in `sseqRecord.dbd`;
        // the control and step-config fields are not.
        //
        // `ABORTING` used to be in the read-only set. It is not SPC_NOMOD:
        // `sseqRecord.dbd:820-824` declares it `special(SPC_MOD)`, which is
        // C's "writable, and call the record's `special()` on the write" — the
        // opposite of a no-modify. Only the hand-written table said otherwise,
        // and this test pinned the hand-written table. The declaration is the
        // `.dbd`.
        let rec = SseqRecord::new();
        let fields = rec.field_list();
        for ro_name in [
            "DT1", "LT1", "WERR1", "WTG1", "IX1", "DOL1V", "LNK1V", "DTA", "LNKAV",
        ] {
            let f = fields
                .iter()
                .find(|f| f.name == ro_name)
                .unwrap_or_else(|| panic!("{ro_name} present in field_list"));
            assert!(
                f.read_only,
                "{ro_name} is special(SPC_NOMOD) in sseqRecord.dbd"
            );
        }
        // WTGA and IXA are the twin `.dbd` omission: WTG1..WTG9 / IX1..IX9 all
        // carry `special(SPC_NOMOD)`, but the 10th group's WTGA/IXA do NOT (both
        // the vendored and the upstream `sseqRecord.dbd`), so C leaves them
        // writable. They belong in the writable set, not the read-only one.
        for rw_name in [
            "VAL", "ABORT", "ABORTING", "DLY1", "WAIT1", "LNK1", "STRA", "WTGA", "IXA",
        ] {
            let f = fields.iter().find(|f| f.name == rw_name).unwrap();
            assert!(
                !f.read_only,
                "{rw_name} carries no special(SPC_NOMOD) in sseqRecord.dbd"
            );
        }
    }

    /// The 10th-group `.dbd` omission, end to end: `WTG1..WTG9` / `IX1..IX9` are
    /// `special(SPC_NOMOD)` and their put is refused by the read-only gate, but
    /// `WTGA` / `IXA` carry no `SPC_NOMOD`, so C stores a client put verbatim in
    /// the raw `DBF_SHORT` and reads it back. The port modelled `IXn` as a
    /// computed slot index (a put returned `FieldNotFound`) and `WTGn` as a bool
    /// (a put of 5 read back as 1); now both are stored raw shorts.
    #[test]
    fn sseq_tenth_group_wtga_ixa_store_client_put_like_c() {
        // Defaults: IXn is its 0-based index, WTGn is 0.
        let rec = SseqRecord::new();
        assert_eq!(rec.get_field("IX1"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("IX9"), Some(EpicsValue::Short(8)));
        assert_eq!(rec.get_field("IXA"), Some(EpicsValue::Short(9)));
        assert_eq!(rec.get_field("WTGA"), Some(EpicsValue::Short(0)));

        // A put to IXA / WTGA stores the raw short verbatim (the read-only gate
        // that would block IX1..IX9 / WTG1..WTG9 lives in the framework, above
        // `put_field`; C's dbPut stores these two because they lack SPC_NOMOD).
        let mut rec = SseqRecord::new();
        rec.put_field("IXA", EpicsValue::Short(5)).unwrap();
        rec.put_field("WTGA", EpicsValue::Short(5)).unwrap();
        assert_eq!(
            rec.get_field("IXA"),
            Some(EpicsValue::Short(5)),
            "IXA must store the client put, not stay at the init index 9"
        );
        assert_eq!(
            rec.get_field("WTGA"),
            Some(EpicsValue::Short(5)),
            "WTGA must store the raw short 5, not collapse to a 0/1 bool"
        );
        // The write is isolated to the 10th group.
        assert_eq!(rec.get_field("IX1"), Some(EpicsValue::Short(0)));
        assert_eq!(rec.get_field("WTG1"), Some(EpicsValue::Short(0)));
    }

    /// C `special()` REJECTS two SPC_MOD puts the port used to accept:
    ///   * ABORTING — no `case` in the switch, so it hits `default:` and
    ///     returns `S_db_badChoice` on every put (sseqRecord.c:1218-1220).
    ///   * ABORT when the record is idle (`busy == 0`) — "no activity to abort",
    ///     `return(-1)` (sseqRecord.c:1173-1178).
    ///
    /// The value dbPut already stored still reads back; only the put STATUS is a
    /// failure. Every other SPC_MOD field (DLYn/DOn/DOLn/LNKn/STRn/WAITn) has a
    /// real switch case and is accepted — verified here for DLY1/WAIT1.
    #[test]
    fn sseq_special_rejects_aborting_and_idle_abort_like_c() {
        // ABORTING put: rejected, but the stored value still reads back.
        let mut rec = SseqRecord::new();
        rec.put_field("ABORTING", EpicsValue::Short(5)).unwrap();
        let err = rec.special("ABORTING", true).unwrap_err();
        assert!(
            matches!(err, CaError::BadChoice(_)),
            "ABORTING is C's default:S_db_badChoice, got {err:?}"
        );
        assert_eq!(
            rec.get_field("ABORTING"),
            Some(EpicsValue::Short(5)),
            "the put value dbPut stored stays written even though the put failed"
        );

        // ABORT put while idle (busy == 0): rejected; abort is reset to 0.
        let mut rec = SseqRecord::new();
        assert_eq!(rec.get_field("BUSY"), Some(EpicsValue::Short(0)));
        rec.put_field("ABORT", EpicsValue::Short(1)).unwrap();
        let err = rec.special("ABORT", true).unwrap_err();
        assert!(
            matches!(err, CaError::BadField(_)),
            "idle ABORT is C's -1 rejection, got {err:?}"
        );
        assert_eq!(
            rec.get_field("ABORT"),
            Some(EpicsValue::Short(0)),
            "C resets abort to 0 in the no-activity path"
        );

        // The accepted SPC_MOD siblings still return Ok (C has switch cases).
        let mut rec = SseqRecord::new();
        rec.put_field("DLY1", EpicsValue::Double(0.02)).unwrap();
        rec.special("DLY1", true).unwrap();
        rec.special("WAIT1", true).unwrap();
    }

    /// R17-1: a NEGATIVE `PREC` reaches C's
    /// `cvtDoubleToString(dov, s, epicsUInt16 prec)` REINTERPRETED, not clamped
    /// — `PREC=-1` is 65535, so `STRn` is the 17-digit `%*.*e` rendering.
    /// Ground truth, compiled libCom: `cvtDoubleToString(3.7, b, (short)-1)` →
    /// ` 3.70000000000000018e+00`.
    #[test]
    fn negative_prec_renders_strn_as_seventeen_digit_exponential() {
        let mut rec = SseqRecord::new();
        rec.put_field("PREC", EpicsValue::Short(-1)).unwrap();
        rec.put_field("DO1", EpicsValue::Double(3.7)).unwrap();
        assert_eq!(
            rec.get_field("STR1"),
            Some(EpicsValue::String(" 3.70000000000000018e+00".into())),
            "PREC=-1 is epicsUInt16 65535, not a clamp to 0 (which printed \"4\")"
        );

        // A non-negative PREC is untouched.
        rec.put_field("PREC", EpicsValue::Short(2)).unwrap();
        rec.put_field("DO1", EpicsValue::Double(3.7)).unwrap();
        assert_eq!(
            rec.get_field("STR1"),
            Some(EpicsValue::String("3.70".into()))
        );
    }
}
