//! aCalcout record — array calc with output (synApps calc module).
//!
//! Port of `aCalcoutRecord.c` (Tim Mooney, synApps `calc`). It is the array
//! analogue of `scalcout`: the `CALC` expression is evaluated by the array
//! calc engine (`aCalcPerform`) and produces both a scalar `VAL` and an array
//! `AVAL`; an optional `OCAL` expression (selected by `DOPT`) produces `OVAL`
//! / `OAV`. Output to the `OUT` link is gated by `OOPT` and `IVOA`.
//!
//! Structure mirrors `scalcout.rs`, swapping the string engine
//! (`sCalcPerform`) for the array engine (`crate::calc::acalc_*`,
//! `ArrayInputs`, `ArrayStackValue`).
//!
//! ## Result mapping (C `aCalcPerform`, `aCalcPerform.c:1620-1644`)
//!
//! The C engine writes the stack top to BOTH a scalar `*p_dresult` and an
//! array `p_aresult[0..arraySize]`. A scalar result is broadcast into the
//! array (`toArray(ps,1)`); an array result's scalar is `arr[0]`
//! (`to_double`). `aCalcPerform` returns `-1` when the scalar result is
//! NaN/Inf. The Rust `ArrayStackValue` API matches this exactly:
//! `as_f64()` (Double→v, Array→`arr[0]`) is `VAL`, and `broadcast(n)`
//! (Double→`[v;n]`, Array→arr) is `AVAL`; a non-finite `VAL` is the
//! failure signal.
//!
//! ## DBF_NOACCESS fields modeled inert (unmodeled by design)
//!
//! Six `DBF_NOACCESS` fields carry no client-accessible value in C (no
//! `cvt_dbaddr`, opaque pointers) and so are absent from `field_list`,
//! matching how the port handles SADR/CADR/ONVx:
//! - `RPVT` (`rpvtStruct*`), `RPCL`/`ORPC` (compiled reverse-Polish buffers)
//!   — internal C pointers, no value type.
//! - `PAVL`/`PAA`/`POAV` (previous-value array scratch) — used only by C
//!   `monitor()`/`fetch_values` to detect array change; the framework's
//!   generic change-detection on `AVAL`/`AA..LL`/`OAV` subsumes them.
//!
//! The accessible `DBF_NOACCESS` arrays (`AVAL`, `AA..LL`, `OAV`) carry
//! `special(SPC_DBADDR)` and are served by C `cvt_dbaddr` as `DBF_DOUBLE`
//! arrays; here they are `DbFieldType::Double` and returned as
//! `EpicsValue::DoubleArray`.
//!
//! ## Modeled simplifications (vs. C)
//!
//! - `ODLY`/`DLYA` (delayed output) IS modeled: when an output is due and
//!   ODLY > 0, the OUT write/OEVT/forward-link are deferred by ODLY seconds
//!   via an async-pending-notify pass (DLYA=1) and a re-process continuation,
//!   matching C `aCalcoutRecord.c::process` lines 338-346/421-430. `WAIT` (CA
//!   put-callback wait) and the async `acalcPerformTask` thread are not
//!   modeled; those fields are stored but inert. `OEVT` (post-event) IS
//!   modeled — see [`AcalcoutRecord::output_event`].
//! - Link-status fields (`INAV..INLV`, `IAAV..ILLV`, `OUTV`) are static at
//!   their C post-`init_record` value: an unconfigured (constant) link
//!   reports `Constant`(3), matching C overwriting the dbd `initial("1")`
//!   for every CONSTANT link (see `AcalcoutRecord::default`). Live per-link
//!   CA-connectivity reclassification (C `checkLinks`) is not modeled.
//! - Array `AVAL`/`OAV` MDEL/ADEL deadband (C `monitor()` per-element delta
//!   vs `pavl`/`poav`, aCalcoutRecord.c:987-1021) is replaced by the
//!   framework's generic array change-detection (post on any change). Differs
//!   only when MDEL/ADEL > 0; the default 0 posts on any change in both.
//! - `IVOA` is applied on a CALC/OCAL evaluation failure (the C
//!   `CALC_ALARM`=INVALID path), matching siblings `scalcout`/`calcout`. The
//!   limit-alarm/MS-link-driven `INVALID` IVOA gate of C `execOutput` is now
//!   honoured too: the framework's §4.6 `multi_output_links` dispatch applies
//!   the same `IVOA=Don't_drive` veto the single-OUT `skip_out` path enforces,
//!   reading the committed `sevr` after `check_alarms`/`evaluate_alarms`. So a
//!   non-calc-fail `INVALID` (NaN-VAL UDF, limit, MS link) suppresses the OUT
//!   write for every severity source — not only a failed evaluation. The
//!   record-level `cached_should_output` still gates the OOPT/calc-fail
//!   decision; the framework layer is the IVOA backstop on top. SetIVOV sets
//!   only the scalar `OVAL` (C `aCalcoutRecord.c:924`), and the OUT write
//!   buffer is chosen by the resolved target element count exactly as
//!   `devaCalcoutSoft.c` does — scalar target ⇒ `VAL`/`OVAL`, array target
//!   ⇒ `AVAL`/`OAV` — so IVOV reaches a scalar `DOPT=Use OCAL` target, an
//!   array target gets the stale `OAV`, and under `DOPT=Use CALC` the
//!   substitution is a no-op (C quirk, reproduced). See
//!   `set_output_to_ivov` / `multi_output_scalar_companion`.
//! - `SIZE` (NELM vs NUSE) gates the client-advertised array capacity, C's
//!   `cvt_dbaddr` half
//!   ([`crate::server::record::FieldDeclaration::field_native_count`] →
//!   `AcalcoutRecord::dbaddr_no_elements`); the SERVED element count stays
//!   `acalcGetNumElements()`, C's `get_array_info` half. The two differ under
//!   the default `SIZE=NELM` with `0 < NUSE < NELM`, which is C's design and
//!   not a rounding. Still deviating: `NELM=0` (degenerate; dbd initial 1)
//!   advertises and serves a 1-element array, where C advertises and serves 0.
//! - `UDF` is C's `pcalc->udf`, owned directly as the framework's `dbCommon.udf`
//!   `epicsUInt8` (no shadow cell): undefined until a calc successfully defines
//!   VAL. C `aCalcoutRecord.c:305-307` clears it `else pcalc->udf = FALSE` only
//!   on a finite result (`cstat==0`) and never re-raises it in `process()`; a
//!   compile-failure/empty `CALC` (which `aCalcPerform` fails every cycle) keeps
//!   `UDF=1`, matching C. [`Record::clears_udf`] is `false` so the framework
//!   does NOT re-derive `UDF` from VAL each cycle — [`Record::check_alarms`]
//!   (C's `afterCalc` tail) is the single owner, clearing `common.udf` to 0 on a
//!   successful calc and leaving the byte untouched otherwise. A direct `caput
//!   UDF` therefore stores its raw `DBF_UCHAR` byte verbatim (`255` for `-1`,
//!   served signed) instead of collapsing to 0/1, and `checkAlarms` tests it
//!   exact-one (`udf == TRUE`, [`Record::udf_alarm_on_exact_one`]).
//! - `LALM` advances on every matched alarm level; C gates it on
//!   `if (recGblSetSevr(...))` (advance only when that severity actually
//!   raised `nsev`). Mirrors the framework-wide `rec_gbl_set_sevr` (returns
//!   void); a higher pre-existing severity can thus perturb next-cycle
//!   hysteresis vs C.

use crate::error::{CaError, CaResult};
use crate::server::record::{
    CyclePostMask, InputFetchPolicy, OutTarget, ProcessAction, ProcessOutcome, Record,
    RecordProcessResult,
};
use crate::types::{EpicsValue, PvString};

use super::calc_compile;
use crate::calc::{ArrayInputs, CompiledExpr, ExprKind, acalc_eval};
// `LINK_CON` (= 3, the `Constant` link-status index) is the value C
// `init_record` writes for an unconfigured link; shared with `calcout`.
use super::link_status::{LINK_CON, LinkRole, LinkStatusGen, post_link_status};
use crate::server::database::AsyncDbHandle;

/// Code version reported by `VERS` (C `#define VERSION 1.4`).
const VERSION: f64 = 1.4;

/// Everything one `aCalcPerform` call changed — its result, and the variable stores
/// it made on the way there.
///
/// C does not need this type: it hands aCalcPerform pointers into the record, so a
/// store IS the record write and the caller reads the effect back out of its own
/// fields. The engine here works on an owned [`ArrayInputs`], so the effect is
/// returned as a value and applied by one owner ([`AcalcoutRecord::apply_stores`]).
/// Splitting the result from the stores is what makes C's DEFERRED status honest:
/// a failing expression writes no VAL/AVAL, but the stores it already made stand.
struct CalcPass {
    /// `None` when aCalcPerform returned -1 before writing a result.
    result: Option<(f64, Vec<f64>, bool)>,
    /// `A..L` / `AA..LL` as the pass left them, plus [`ArrayInputs::amask`] — the
    /// array fields it stored into.
    inputs: ArrayInputs,
}

const ARR_NAMES: [&str; 12] = [
    "AA", "BB", "CC", "DD", "EE", "FF", "GG", "HH", "II", "JJ", "KK", "LL",
];

/// Number of NUMERIC (scalar `double`) inputs — C's `fieldIndex <=
/// acalcoutRecordINPL` boundary. The scalar inputs come first in
/// [`ACALCOUT_INPUT_LINKS`], so this is the length of the prefix that C's
/// `special()` constant re-seed applies to.
const ACALCOUT_NUMERIC_INPUTS: usize = 12;

/// `(link_field, value_field)` for every input, scalars A..L first (C's
/// `INPA..INPL`), then the arrays AA..LL (`INAA..INLL`) — the order
/// `aCalcoutRecord.c::fetch_values` reads them in.
const ACALCOUT_INPUT_LINKS: &[(&str, &str)] = &[
    ("INPA", "A"),
    ("INPB", "B"),
    ("INPC", "C"),
    ("INPD", "D"),
    ("INPE", "E"),
    ("INPF", "F"),
    ("INPG", "G"),
    ("INPH", "H"),
    ("INPI", "I"),
    ("INPJ", "J"),
    ("INPK", "K"),
    ("INPL", "L"),
    ("INAA", "AA"),
    ("INBB", "BB"),
    ("INCC", "CC"),
    ("INDD", "DD"),
    ("INEE", "EE"),
    ("INFF", "FF"),
    ("INGG", "GG"),
    ("INHH", "HH"),
    ("INII", "II"),
    ("INJJ", "JJ"),
    ("INKK", "KK"),
    ("INLL", "LL"),
];
const INP_NAMES: [&str; 12] = [
    "INPA", "INPB", "INPC", "INPD", "INPE", "INPF", "INPG", "INPH", "INPI", "INPJ", "INPK", "INPL",
];
const INA_NAMES: [&str; 12] = [
    "INAA", "INBB", "INCC", "INDD", "INEE", "INFF", "INGG", "INHH", "INII", "INJJ", "INKK", "INLL",
];
const INAV_NAMES: [&str; 12] = [
    "INAV", "INBV", "INCV", "INDV", "INEV", "INFV", "INGV", "INHV", "INIV", "INJV", "INKV", "INLV",
];
const IAAV_NAMES: [&str; 12] = [
    "IAAV", "IBBV", "ICCV", "IDDV", "IEEV", "IFFV", "IGGV", "IHHV", "IIIV", "IJJV", "IKKV", "ILLV",
];
const PA_NAMES: [&str; 12] = [
    "PA", "PB", "PC", "PD", "PE", "PF", "PG", "PH", "PI", "PJ", "PK", "PL",
];

/// AMEM/PMEM — the record's account of the array memory it has `calloc`'d, and
/// the single owner of every change to it.
///
/// `aCalcoutRecord.c` allocates each of its `double *` array buffers LAZILY, and
/// charges the allocation where it is made:
///
/// ```c
/// if (pcalc->aval == NULL) {
///     pcalc->aval = (double *)calloc(pcalc->nelm, sizeof(double));
///     pcalc->amem += pcalc->nelm * sizeof(double);
/// }
/// ```
///
/// There are seventeen such buffers — AA..LL, AVAL, OAV, PAVL, POAV, PAA — and
/// C writes that increment out at nineteen sites (`aCalcoutRecord.c:382`,
/// `:386`, `:596`, `:604`, `:612`, `:655`, `:662`, `:668`, `:697`, `:705`,
/// `:713`, `:974`, `:978`, `:1086`, `:1093`, `:1297`). The port has no lazy
/// pointers: every array is a `Vec<f64>` that exists from `Default`. So what it
/// must reproduce is not the allocation but C's LEDGER — WHICH buffers C would
/// have `calloc`'d by now, each charged exactly once at
/// `NELM * sizeof(double)`.
///
/// That ledger is [`ArrayMem`], and `amem` is private to it: no site can add to
/// AMEM without naming the buffer it is charging for, and a buffer already in
/// the set charges nothing however many times it is named. A scattered
/// `self.amem += ...` — C's own shape — cannot express "once", because the
/// `== NULL` test that made it once in C has no counterpart here.
///
/// The ledger is held in atomics and every operation takes `&self`. That is not
/// a concurrency feature: it is what lets the charge sit at C's OWN seam.
/// `cvt_dbaddr` runs when a client resolves a channel on an `SPC_DBADDR` field,
/// which reaches this port as [`AcalcoutRecord::dbaddr_capacity`] — a `&self`
/// hook, because answering "how many elements does this channel have" is a
/// read. A charge is idempotent per [`ArrayBuf`] and the test-and-set is one
/// `fetch_or`, so the ledger stays a pure function of which buffers have been
/// demanded, whoever asks and however often.
mod array_mem {
    use super::CyclePostMask;
    use std::sync::atomic::{AtomicI32, AtomicU8, AtomicU32, Ordering};

    /// `post_pending` states — no post outstanding, C's literal
    /// `DBE_VALUE|DBE_LOG`, C's `monitor_mask|DBE_VALUE|DBE_LOG`.
    const POST_NONE: u8 = 0;
    const POST_VALUE_LOG: u8 = 1;
    const POST_MONITOR_VALUE_LOG: u8 = 2;

    /// One of the seventeen lazily-`calloc`'d `double *` buffers of
    /// `aCalcoutRecord.c`.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(super) enum ArrayBuf {
        /// `pcalc->aa[i]` — one of AA..LL. Allocated by `cvt_dbaddr` (`:595`),
        /// `get_array_info` (`:654`), `put_array_info` (`:696`),
        /// `fetch_values` (`:1085`), and by `aCalcPerform` itself when the
        /// expression STORES into it (`aCalcPerform.c:474`, `:515`), which the
        /// record charges from the before/after pointer count (`:1293-1298`).
        Arr(usize),
        /// `pcalc->aval` — `process` (`:381`) plus the three dbAddr hooks.
        Aval,
        /// `pcalc->oav` — `process` (`:385`) plus the three dbAddr hooks.
        Oav,
        /// `pcalc->pavl` — `monitor` (`:973`), the previous-AVAL copy.
        Pavl,
        /// `pcalc->poav` — `monitor` (`:977`), the previous-OAV copy.
        Poav,
        /// `pcalc->paa` — `fetch_values`' ONE comparison scratch buffer
        /// (`:1092`), shared by all twelve array links.
        Paa,
    }

    impl ArrayBuf {
        fn bit(self) -> u32 {
            match self {
                ArrayBuf::Arr(i) => 1 << i,
                ArrayBuf::Aval => 1 << 12,
                ArrayBuf::Oav => 1 << 13,
                ArrayBuf::Pavl => 1 << 14,
                ArrayBuf::Poav => 1 << 15,
                ArrayBuf::Paa => 1 << 16,
            }
        }
    }

    /// C `pcalc->nelm * sizeof(double)`, the charge for one buffer. C's `nelm`
    /// is `epicsUInt32` and the product widens to `size_t` before landing in the
    /// `epicsInt32` AMEM, so the truncation is C's.
    fn charge(nelm: u32) -> i32 {
        (u64::from(nelm) * size_of::<f64>() as u64) as i32
    }

    #[derive(Default)]
    pub(super) struct ArrayMem {
        amem: AtomicI32,
        pmem: AtomicI32,
        /// The set of [`ArrayBuf`]s already charged — C's `ppd[i] != NULL`.
        allocated: AtomicU32,
        /// C's `db_post_events(pcalc, &pcalc->amem, ...)`, waiting for the
        /// framework's post point, carrying the mask of the C site that made it.
        /// Drained by `AcalcoutRecord::take_cycle_posted_fields`.
        /// [`POST_NONE`] = no post outstanding.
        post_pending: AtomicU8,
    }

    impl ArrayMem {
        pub(super) fn amem(&self) -> i32 {
            self.amem.load(Ordering::Relaxed)
        }

        pub(super) fn pmem(&self) -> i32 {
            self.pmem.load(Ordering::Relaxed)
        }

        /// The RAW store — a `.db` field or an autosave restore landing in
        /// `put_field`, C's `dbPut` into the field itself. It replaces the
        /// reported number and deliberately does NOT touch the ledger: the
        /// buffers C has allocated are a fact about the record, not about what
        /// a client wrote into AMEM, so a later allocation still adds its own
        /// charge on top — which is exactly `pcalc->amem += ...` in C.
        pub(super) fn store_amem(&self, value: i32) {
            self.amem.store(value, Ordering::Relaxed);
        }

        pub(super) fn store_pmem(&self, value: i32) {
            self.pmem.store(value, Ordering::Relaxed);
        }

        /// C's bare `calloc` + `amem +=` pair — the sites in `process`
        /// (`:379-387`), `monitor` (`:972-979`), `fetch_values` (`:1084-1094`)
        /// and `call_aCalcPerform` (`:1293-1298`), none of which posts or
        /// touches PMEM: `monitor()`'s tail does that for the whole cycle at
        /// once ([`Self::sync`]).
        ///
        /// Returns true when this call was the allocation. The test-and-set is
        /// one `fetch_or`, so the charge lands exactly once even when two
        /// channel creations race on the same field under the record's READ
        /// lock — the property that makes the ledger safe to run from `&self`.
        pub(super) fn allocate(&self, buf: ArrayBuf, nelm: u32) -> bool {
            let bit = buf.bit();
            if self.allocated.fetch_or(bit, Ordering::Relaxed) & bit != 0 {
                return false;
            }
            self.amem.fetch_add(charge(nelm), Ordering::Relaxed);
            true
        }

        /// The SPC_DBADDR hooks' shape instead — `cvt_dbaddr` (`:594-599`),
        /// `get_array_info` (`:653-658`) and `put_array_info` (`:695-700`) each
        /// follow the charge immediately with
        ///
        /// ```c
        /// db_post_events(pcalc, &pcalc->amem, DBE_VALUE|DBE_LOG);
        /// pcalc->pmem = pcalc->amem;
        /// ```
        ///
        /// because they run on a `dbGet`/`dbPut`, with no `monitor()` behind
        /// them to close the cycle. The post is a LITERAL `DBE_VALUE|DBE_LOG`
        /// there — no alarm mask is in scope.
        pub(super) fn allocate_for_dbaddr(&self, buf: ArrayBuf, nelm: u32) {
            if self.allocate(buf, nelm) {
                self.post_pending.store(POST_VALUE_LOG, Ordering::Relaxed);
                self.pmem
                    .store(self.amem.load(Ordering::Relaxed), Ordering::Relaxed);
            }
        }

        /// C `monitor()`'s tail (`:1044-1047`) — the ONE place a process cycle
        /// commits the charges it accumulated:
        ///
        /// ```c
        /// if (pcalc->amem != pcalc->pmem) {
        ///     db_post_events(pcalc, &pcalc->amem, monitor_mask|DBE_VALUE|DBE_LOG);
        ///     pcalc->pmem = pcalc->amem;
        /// }
        /// ```
        ///
        /// PMEM is therefore not a second AMEM: it is the value AMEM had when it
        /// was last POSTED, which is why C compares the two rather than keeping a
        /// dirty flag.
        pub(super) fn sync(&self) {
            let amem = self.amem.load(Ordering::Relaxed);
            if amem != self.pmem.load(Ordering::Relaxed) {
                self.post_pending
                    .store(POST_MONITOR_VALUE_LOG, Ordering::Relaxed);
                self.pmem.store(amem, Ordering::Relaxed);
            }
        }

        pub(super) fn take_post(&self) -> Option<CyclePostMask> {
            match self.post_pending.swap(POST_NONE, Ordering::Relaxed) {
                POST_VALUE_LOG => Some(CyclePostMask::ValueLog),
                POST_MONITOR_VALUE_LOG => Some(CyclePostMask::MonitorValueLog),
                _ => None,
            }
        }
    }
}

use array_mem::{ArrayBuf, ArrayMem};

/// aCalcout record state. Field comments cite the `aCalcoutRecord.dbd`
/// declaration; behaviour cites `aCalcoutRecord.c`.
pub struct AcalcoutRecord {
    // --- result ---
    pub val: f64,
    aval: Vec<f64>,
    /// `PVAL` — previous `VAL`, captured at the end of `process()`. Drives the
    /// `OOPT` On-Change/Transition tests (C `afterCalc`). The engine's `VAL`
    /// token is fed directly from `val`/`oval` per pass (see `build_inputs`),
    /// not from `pval`.
    pval: f64,

    // --- array sizing ---
    nelm: u32,
    /// `NUSE` — how many elements of each array the expression uses. The
    /// record's ONE invariant on it is `nuse <= nelm`, and it is re-established
    /// only through [`AcalcoutRecord::clamp_nuse`]; nothing else may write this
    /// field down.
    nuse: u32,
    /// Set by [`AcalcoutRecord::clamp_nuse`] when it actually clamped, drained by
    /// `take_cycle_posted_fields`. It is C's `db_post_events(&pcalc->nuse, ...)`
    /// waiting for the framework's post point — and it lives INSIDE the clamp
    /// owner so that no site can correct NUSE without telling the subscribers.
    /// It carries the MASK of the C call site that made it: the `init_record` and
    /// `process` clamps post `DBE_VALUE|DBE_LOG` (`:189`, `:376`), the
    /// `special(NUSE)` clamp a bare `DBE_VALUE` (`:497`).
    nuse_post_pending: Option<CyclePostMask>,

    // --- CALC ---
    pub calc: String,
    /// C `RPCL`. Always a program: an empty or uncompilable CALC carries C's
    /// empty `END_EXPRESSION` postfix, which `aCalcPerform` refuses to run
    /// (`aCalcPerform.c:312-314`), so the record alarms on every process.
    compiled_calc: CompiledExpr,
    clcv: i32,

    // --- OCAL / output-data option ---
    pub ocal: String,
    /// C `ORPC`. Same contract as [`Self::compiled_calc`].
    compiled_ocal: CompiledExpr,
    oclv: i32,
    dopt: i16, // 0=Use CALC, 1=Use OCAL
    oval: f64,
    oav: Vec<f64>,
    povl: f64,

    // --- scalar inputs A..L ---
    num_vals: [f64; 12],
    inp_links: [String; 12],
    pa: [f64; 12], // PA..PL

    // --- array inputs AA..LL ---
    arr_vals: [Vec<f64>; 12],
    ina_links: [String; 12],

    // --- link status, derived from the links by `refresh_link_status` ---
    inav: [i16; 12], // INAV..INLV
    iaav: [i16; 12], // IAAV..ILLV
    outv: i16,
    /// Async surface + generation gate for `refresh_link_status`, the shape
    /// calcout/scalcout/transform use (see `link_status::post_link_status`).
    async_ctx: Option<(String, AsyncDbHandle)>,
    link_gen: LinkStatusGen,

    // --- output link + options ---
    out: String,
    oopt: i16, // 0..6, see acalcoutOOPT
    odly: f64,
    wait: i16,
    dlya: u16,
    oevt: u16,
    ivoa: i16, // shared menuIvoa
    ivov: f64,

    // --- display ---
    egu: PvString,
    prec: i16,
    hopr: f64,
    lopr: f64,

    // --- alarm limits ---
    hihi: f64,
    lolo: f64,
    high: f64,
    low: f64,
    hhsv: i16,
    llsv: i16,
    hsv: i16,
    lsv: i16,
    hyst: f64,
    adel: f64,
    mdel: f64,
    lalm: f64,
    alst: f64,
    mlst: f64,

    // --- diagnostics (mostly inert) ---
    size: i16, // 0=NELM, 1=NUSE
    cstat: i32,
    cact: u8,
    amask: u32,
    /// AMEM + PMEM, and the ledger of which array buffers C would have
    /// `calloc`'d by now. The ONE thing allowed to add to AMEM — see
    /// [`array_mem`].
    mem: ArrayMem,
    newm: u32,

    // --- process flags ---
    calc_alarm: bool,
    /// This cycle's `fetch_values()` outcome, pushed by the framework through
    /// `set_fetch_gate_failed`. C `aCalcoutRecord.c::process` (399) runs
    /// `doCalc` + `afterCalc` only `if (fetch_values(pcalc)==0)`, and
    /// `fetch_values` (1066-1067) returns at the first failing `dbGetLink`.
    fetch_gate_failed: bool,
    cached_should_output: bool,
    /// The output decision captured on an ODLY delaying cycle, restored into
    /// `cached_should_output` on the continuation so the deferred OUT write
    /// fires once after the delay. Mirrors `scalcout`/`calcout`.
    pending_output: bool,
}

impl Default for AcalcoutRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            aval: Vec::new(),
            pval: 0.0,
            nelm: 1, // dbd initial("1")
            nuse: 0, // dbd initial("0")
            nuse_post_pending: None,
            calc: String::new(),
            compiled_calc: CompiledExpr::empty(ExprKind::Array),
            clcv: 0,
            ocal: String::new(),
            compiled_ocal: CompiledExpr::empty(ExprKind::Array),
            oclv: 0,
            dopt: 0,
            oval: 0.0,
            oav: Vec::new(),
            povl: 0.0,
            num_vals: [0.0; 12],
            inp_links: Default::default(),
            pa: [0.0; 12],
            arr_vals: Default::default(),
            ina_links: Default::default(),
            // Link status reports `Constant` for an unconfigured link: C
            // `init_record` rewrites the dbd static `initial("1")` to
            // `acalcoutINAV_CON` (= 3) for every CONSTANT link
            // (aCalcoutRecord.c:210-216,242), and a default record has all
            // links constant. The dbd `1` is never observed. Matches sibling
            // `calcout` (`in_status: [LINK_CON; 21]`).
            inav: [LINK_CON; 12],
            iaav: [LINK_CON; 12],
            outv: LINK_CON,
            async_ctx: None,
            link_gen: LinkStatusGen::default(),
            out: String::new(),
            oopt: 0,
            odly: 0.0,
            wait: 0,
            dlya: 0,
            oevt: 0,
            ivoa: 0,
            ivov: 0.0,
            egu: PvString::new(),
            prec: 0,
            hopr: 0.0,
            lopr: 0.0,
            hihi: 0.0,
            lolo: 0.0,
            high: 0.0,
            low: 0.0,
            hhsv: 0,
            llsv: 0,
            hsv: 0,
            lsv: 0,
            hyst: 0.0,
            adel: 0.0,
            mdel: 0.0,
            lalm: 0.0,
            alst: 0.0,
            mlst: 0.0,
            size: 0,
            cstat: 0,
            cact: 0,
            amask: 0,
            mem: ArrayMem::default(),
            newm: 0,
            calc_alarm: false,
            fetch_gate_failed: false,
            cached_should_output: false,
            pending_output: false,
        }
    }
}

impl AcalcoutRecord {
    pub fn new() -> Self {
        Self::default()
    }

    /// The ONE place `NUSE > NELM` is corrected — C does it identically at each
    /// of its three sites, and each time it POSTS:
    ///
    /// ```c
    /// if (pcalc->nuse > pcalc->nelm) {
    ///     pcalc->nuse = pcalc->nelm;
    ///     db_post_events(pcalc, &pcalc->nuse, DBE_VALUE|DBE_LOG);
    /// }
    /// ```
    /// `init_record` pass 0 (`aCalcoutRecord.c:188-190`), `process`
    /// (`:374-377`), `special(NUSE)` (`:495-497`, `DBE_VALUE` there).
    ///
    /// The comment C leaves at the process site names the trigger: *"Make sure.
    /// Autosave is capable of setting NUSE to an illegal value."* — a restore
    /// writes NUSE and NELM in whatever order the .sav file lists them, so the
    /// record can legitimately be handed `NUSE > NELM` and must repair it. The
    /// repair is only half done without the post: the client that wrote the
    /// illegal value, and every monitor, would keep reading it back while the
    /// record used the clamped one.
    ///
    /// Clamping and posting are therefore ONE operation. The pending post lives
    /// in this owner's own cell and is drained by `take_cycle_posted_fields`, so
    /// a caller cannot perform the clamp and forget the post.
    fn clamp_nuse(&mut self, mask: CyclePostMask) -> bool {
        if self.nuse > self.nelm {
            self.nuse = self.nelm;
            self.nuse_post_pending = Some(mask);
            return true;
        }
        false
    }

    /// Current array element count (C `acalcGetNumElements`,
    /// `aCalcoutRecord.c:160-168`): `NUSE` when `0 < NUSE < NELM`, else
    /// `NELM`. At least 1 (C always `calloc(nelm)` with `nelm >= 1`).
    /// C `monitor()`'s array-memory half, and the ONLY way `process()` may say
    /// "this cycle completed".
    ///
    /// `monitor()` allocates the two previous-value buffers before it uses them
    /// and then closes the cycle's AMEM accounting:
    ///
    /// ```c
    /// if (pcalc->pavl == NULL) { pcalc->pavl = calloc(...); pcalc->amem += ...; }
    /// if (pcalc->poav == NULL) { pcalc->poav = calloc(...); pcalc->amem += ...; }
    /// ...
    /// if (pcalc->amem != pcalc->pmem) { db_post_events(...); pcalc->pmem = pcalc->amem; }
    /// ```
    ///
    /// C reaches it from `process()`'s tail (`:442`) — so from every path that
    /// does NOT `return(ASYNC)` first. That is exactly the set of paths here
    /// that return [`ProcessOutcome::complete`]: the ODLY continuation (C's
    /// `pcalc->dlya` branch falls through to the tail), the failed fetch gate
    /// (C skips `doCalc`/`afterCalc` and falls through), and the normal tail.
    /// The ODLY *delaying* cycle is the one C returns ASYNC from before
    /// `monitor()`, and it returns `AsyncPendingNotify` here — it must not come
    /// through this owner, and cannot, because it does not build a `complete()`.
    ///
    /// Returning the outcome rather than being a plain side-effect call is the
    /// point: a completing return that skipped the previous-value charges would
    /// have to write `ProcessOutcome::complete()` itself, and there is now no
    /// reason to.
    fn complete_cycle(&mut self) -> ProcessOutcome {
        self.mem.allocate(ArrayBuf::Pavl, self.nelm);
        self.mem.allocate(ArrayBuf::Poav, self.nelm);
        self.mem.sync();
        ProcessOutcome::complete()
    }

    /// The `SPC_DBADDR` field name → the buffer C's `cvt_dbaddr` /
    /// `get_array_info` / `put_array_info` allocate for it
    /// (`aCalcoutRecord.c:589-617`, `:646-670`, `:685-717`). The three hooks
    /// share one `if/else if/else if` over the same fourteen names, so they
    /// share one mapping here.
    fn dbaddr_buf(field: &str) -> Option<ArrayBuf> {
        match field {
            "AVAL" => Some(ArrayBuf::Aval),
            "OAV" => Some(ArrayBuf::Oav),
            _ => Self::arr_index(field).map(ArrayBuf::Arr),
        }
    }

    fn num_elements(&self) -> usize {
        let n = if self.nuse > 0 && self.nuse < self.nelm {
            self.nuse
        } else {
            self.nelm
        };
        (n as usize).max(1)
    }

    /// C `cvt_dbaddr`'s `paddr->no_elements` (`aCalcoutRecord.c:627-631`) — the
    /// CAPACITY the database layer sees for an SPC_DBADDR array field, and so the
    /// bound `dbPut` clamps a client write to (`dbAccess.c:1322`, `:1361-1362`).
    ///
    /// ```c
    /// if (pcalc->size == acalcoutSIZE_NUSE)
    ///     paddr->no_elements = acalcGetNumElements( pcalc );  /* the NUSE window */
    /// else
    ///     paddr->no_elements = pcalc->nelm;                   /* the whole buffer */
    /// ```
    ///
    /// This is NOT [`Self::num_elements`]. SIZE defaults to NELM — it is the first
    /// choice of the `acalcoutSIZE` menu (`aCalcoutRecord.dbd:32-35`) — so by
    /// default a client may write the WHOLE `nelm` buffer, including the tail
    /// `[numElements, nelm)` that NUSE currently hides. Only SIZE=NUSE narrows the
    /// client to the window, and that is the whole point of the field: it exists so
    /// a client can be told the smaller size (`:618-625`).
    fn dbaddr_no_elements(&self) -> usize {
        if self.size == ACALCOUT_SIZE_NUSE {
            self.num_elements()
        } else {
            (self.nelm as usize).max(1)
        }
    }

    /// Serve an internal `f64` array as a `DoubleArray` of the client-reported
    /// element count (C `get_array_info` → `acalcGetNumElements`), padding
    /// with 0 when the buffer is short.
    fn array_field_value(&self, data: &[f64]) -> EpicsValue {
        let n = self.num_elements();
        let mut out = data.to_vec();
        out.resize(n, 0.0);
        EpicsValue::DoubleArray(out)
    }

    /// Zero `[from, numElements)` — the REST OF THE WINDOW after a writer that
    /// delivered fewer elements than the window holds. `[numElements, nelm)`, the
    /// part of the buffer NUSE currently hides, is NOT touched: it keeps whatever
    /// was in it and reappears if NUSE grows again.
    ///
    /// Both of the record's array writers do this, in the same words:
    ///
    /// ```c
    /// /* fetch_values, aCalcoutRecord.c:1100-1102 — the input link */
    /// if (nRequest<numElements)
    ///     for (j=nRequest; j<numElements; j++) (*pavalue)[j] = 0;
    ///
    /// /* put_array_info, aCalcoutRecord.c:729-731 — the client dbPut */
    /// if ( pd && (nNew < numElements) )
    ///     for (i=nNew; i<numElements; i++) pd[i] = 0.;
    /// ```
    ///
    /// The client half is not optional and not the record's own idea: `dbPut`
    /// calls `put_array_info` for every SPC_DBADDR field it writes
    /// (`dbAccess.c:1366-1369`), with `nNew` = the element count that arrived.
    /// So a caput of two elements into a ten-element window leaves eight zeros
    /// behind it, not eight stale values — which is the opposite of what W10-A8
    /// asserted here.
    /// (C's `if (nNew < numElements)` / `if (nRequest<numElements)` is the guard
    /// on both loops: a writer that filled the window, or overran it, zeroes
    /// nothing.)
    fn zero_fill_window(buf: &mut [f64], from: usize, window: usize) {
        let len = buf.len();
        let (from, window) = (from.min(len), window.min(len));
        if from >= window {
            return;
        }
        for v in &mut buf[from..window] {
            *v = 0.0;
        }
    }

    /// The ONE owner of every write into an SPC_DBADDR array field — AA..LL, AVAL
    /// and OAV, the three that C's `put_array_info` serves
    /// (`aCalcoutRecord.c:677-731`), whether the writer is a client `dbPut` or an
    /// input link.
    ///
    /// A write SPLICES its elements into the record's permanent `calloc(nelm)`
    /// buffer and zeroes the rest of the WINDOW behind them. Both halves belong to
    /// this owner; a caller that did one without the other is the bug it exists to
    /// make unwritable.
    ///
    /// * SPLICE: C allocates each array field ONCE, at `calloc(pcalc->nelm,
    ///   sizeof(double))` (`:695-698`, `:1084-1086`), and that buffer lives for the
    ///   record's lifetime — nothing ever replaces it, so a write is always INTO it
    ///   and never a swap of it.
    /// * ZERO-FILL: `[nNew, numElements)`, and `numElements` here is always the NUSE
    ///   window — `acalcGetNumElements` in BOTH writers (`put_array_info:727-731`,
    ///   `fetch_values:1100-1102`), whatever the writer's bound was. See
    ///   [`Self::zero_fill_window`].
    ///
    /// `bound` is NOT part of the rule — it is the WRITER's, and the two writers do
    /// not agree:
    ///
    /// * the input link asks `dbGetLink` for `nRequest = acalcGetNumElements(pcalc)`
    ///   elements (`:1097-1098`), so it can never deliver more than the window;
    /// * a client `dbPut` is clamped at `paddr->no_elements`
    ///   (`dbAccess.c:1361-1362`) — [`Self::dbaddr_no_elements`], which is the whole
    ///   `nelm` buffer under the default SIZE=NELM.
    ///
    /// So a client CAN write the tail `[numElements, nelm)` that NUSE hides, and a
    /// link cannot. Taking the bound as a parameter is what stops the owner from
    /// deciding for a writer whose rule it does not know: R14-7 gave both writers
    /// the link's window, and the tail became unwritable.
    fn write_array_field(
        &mut self,
        src: &[f64],
        bound: usize,
        select: impl FnOnce(&mut Self) -> &mut Vec<f64>,
    ) {
        let nelm = (self.nelm as usize).max(1);
        let window = self.num_elements();
        let buf = select(self);
        buf.resize(nelm, 0.0);
        let n = src.len().min(bound).min(nelm);
        buf[..n].copy_from_slice(&src[..n]);
        Self::zero_fill_window(buf, n, window);
    }

    fn build_inputs(&self, n: usize, prev_val: f64, prev_aval: &[f64]) -> ArrayInputs {
        // C `aCalcPerform(&pcalc->a, MAX_FIELDS, &pcalc->aa, ARRAY_MAX_FIELDS,
        // numElements, ...)` (`aCalcoutRecord.c:1283`, `:1288`) — both counts are 12
        // (`:156-157`). Args past that are not the record's to write: `M`/`@12`
        // fetch 0, `@@12 :=` stores nothing and flags no AMASK bit.
        let mut inputs = ArrayInputs::with_counts(n, 12, 12);
        for i in 0..12 {
            inputs.num_vars[i] = self.num_vals[i];
        }
        for i in 0..12 {
            let mut arr = self.arr_vals[i].clone();
            arr.resize(n, 0.0);
            inputs.arrays[i] = arr;
        }
        // C `aCalcPerform`'s `VAL`/`FETCH_VAL` token reads `*p_dresult` — the
        // field this pass writes its result into: `prec->val` for the CALC
        // pass, `prec->oval` for the OCAL pass (aCalcoutRecord.c CALC vs OCAL
        // `aCalcPerform` calls; `aCalcPerform.c:528-532`). The caller supplies
        // that field's pre-write value as `prev_val`.
        inputs.prev_val = prev_val;
        // The `AVAL` token is the same story one dimension up: it reads
        // `p_aresult` (`aCalcPerform.c:534-539`), which is `prec->aval` for the
        // CALC pass and `prec->oav` for the OCAL pass (`aCalcoutRecord.c:1283-1290`).
        inputs.prev_aval = prev_aval.to_vec();
        inputs.prev_aval.resize(n, 0.0);
        inputs
    }

    /// Evaluate a compiled expression over the current inputs. `prev_val` seeds
    /// the `VAL` token (see [`Self::build_inputs`]).
    ///
    /// The pass returns EVERYTHING it changed, not just its result, because a C
    /// aCalcPerform call changes more than its result: C hands it pointers to the
    /// record's own `a..p` and `aa..ll` fields (`aCalcoutRecord.c:1283-1285`), so a
    /// store opcode (`A := ...`, `AA := ...`) writes the record IN PLACE. The engine
    /// here evaluates over an owned [`ArrayInputs`], so the pass hands its effect
    /// back and [`Self::apply_stores`] is the one place that lands it.
    ///
    /// `result`:
    /// * `Some((scalar, array, finite))` — `finite=false` still carries a result,
    ///   because C stores `*p_dresult`/`aval` and only *then* returns -1 for a
    ///   NaN/Inf scalar (`aCalcPerform.c:1622-1644`); the caller writes the NaN/Inf
    ///   into VAL/AVAL (C drives it to OUT under the default `IVOA=Continue`) and
    ///   still raises CALC_ALARM.
    /// * `None` — aCalcPerform returned -1 before writing a result (`:1602-1605`), so
    ///   VAL/AVAL keep their previous values. The STORES still stand: C's status is
    ///   deferred to the end of the expression, and the stores it already made went
    ///   straight into the record's fields on the way there.
    fn eval(
        &self,
        compiled: &CompiledExpr,
        n: usize,
        prev_val: f64,
        prev_aval: &[f64],
    ) -> CalcPass {
        let mut inputs = self.build_inputs(n, prev_val, prev_aval);
        let result = match acalc_eval(compiled, &mut inputs) {
            Ok(result) => {
                let v = result.as_f64().unwrap_or(0.0);
                // C fills AVAL from a scalar result with `toArray(ps,1)`
                // (`aCalcPerform.c:1624`) — the same promotion as anywhere else,
                // so a NaN scalar fills AVAL with ZEROS while VAL keeps the NaN
                // (compiled aCalcPerform: `ACOS(2)` -> st=-1 d=nan a=[0 x8]).
                let mut arr = result.to_array(n);
                arr.resize(n, 0.0);
                Some((v, arr, v.is_finite()))
            }
            Err(_) => None,
        };
        CalcPass { result, inputs }
    }

    /// Land one pass's variable stores back into the record's fields — C's store
    /// opcodes writing through the pointers it was handed (`aCalcPerform.c:456-491`).
    ///
    /// The single owner of that write-back: no other path may copy an `ArrayInputs`
    /// into `num_vals`/`arr_vals`, so a store cannot land without its AMASK bit and a
    /// bit cannot be set without its store. The caller owns AMASK's accumulation
    /// across the two passes, because C does: the CALC pass is handed `&pcalc->amask`
    /// and resets it (`:326`), and the OCAL pass ORs its own mask in
    /// (`aCalcoutRecord.c:1288-1291`).
    ///
    /// Only the first `n` elements of an array field are written — C's store loops run
    /// `j < arraySize` (`:483-486`), where arraySize is NUSE (or NELM), so elements
    /// past NUSE keep whatever the field already held.
    fn apply_stores(&mut self, inputs: &ArrayInputs, n: usize) {
        let scalars = self.num_vals.len();
        self.num_vals.copy_from_slice(&inputs.num_vars[..scalars]);
        for (dst, src) in self.arr_vals.iter_mut().zip(&inputs.arrays) {
            if dst.len() < n {
                dst.resize(n, 0.0);
            }
            dst[..n].copy_from_slice(&src[..n]);
        }
    }

    /// OOPT output decision (C `afterCalc` switch, `aCalcoutRecord.c:313-335`).
    /// On-Change uses `MDEL`; index 6 ("Never") suppresses output.
    fn oopt_should_output(&self) -> bool {
        match self.oopt {
            0 => true,                                     // Every Time
            1 => (self.pval - self.val).abs() > self.mdel, // On Change
            2 => self.val == 0.0,                          // When Zero
            3 => self.val != 0.0,                          // When Non-zero
            4 => self.pval != 0.0 && self.val == 0.0,      // Transition To Zero
            5 => self.pval == 0.0 && self.val != 0.0,      // Transition To Non-zero
            6 => false,                                    // Never
            // C's `doOutput` is initialised to 0 (`aCalcoutRecord.c:283`) and
            // only a case that fires sets it, so an index the switch does not
            // name drives NO output — the same rule that makes `Never` a 0.
            _ => false,
        }
    }

    /// menuIvoaSet_output_to_IVOV (C `execOutput`, `aCalcoutRecord.c:923-924`):
    /// `pcalc->oval = pcalc->ivov;` — the scalar `OVAL` ONLY. `writeValue`
    /// then picks the buffer by DOPT and target nelm (`devaCalcoutSoft.c`),
    /// so the substitution is observable exactly where C makes it so:
    /// under `DOPT=Use OCAL` a scalar OUT target receives `IVOV` (the
    /// `&oval` buffer) while an array target receives the stale `OAV`;
    /// under `DOPT=Use CALC` the buffer is `VAL`/`AVAL`, which IVOV never
    /// touches, so the substitution is a no-op — a C quirk this port
    /// reproduces (aCalcPerform fills `OVAL`+`OAV` together, so IVOV is
    /// the only point where `OVAL` and `OAV[0]` decouple).
    fn set_output_to_ivov(&mut self) {
        self.oval = self.ivov;
    }

    /// C `aCalcoutRecord.c::special:471-478` — `pcalc->clcv =
    /// aCalcPostfix(...)`. The value stored is aCalcPostfix's RETURN STATUS,
    /// which is **-1** on failure (aCalcPostfix.c:801-809), not a generic 1 and
    /// not the CALC_ERR_* code. An empty CALC is a valid empty program with
    /// status 0 (aCalcPostfix.c:439-441).
    fn recompile_calc(&mut self) {
        let compiled = calc_compile::acalc_postfix("acalcout", "CALC", &self.calc);
        self.clcv = compiled.status;
        self.compiled_calc = compiled.program;
    }

    /// C `aCalcoutRecord.c::special:483-490` — same, into ORPC/OCLV.
    fn recompile_ocal(&mut self) {
        let compiled = calc_compile::acalc_postfix("acalcout", "OCAL", &self.ocal);
        self.oclv = compiled.status;
        self.compiled_ocal = compiled.program;
    }

    fn num_index(name: &str) -> Option<usize> {
        if name.len() == 1 {
            let c = name.as_bytes()[0];
            if (b'A'..=b'L').contains(&c) {
                return Some((c - b'A') as usize);
            }
        }
        None
    }

    fn arr_index(name: &str) -> Option<usize> {
        ARR_NAMES.iter().position(|&n| n == name)
    }

    fn inp_index(name: &str) -> Option<usize> {
        INP_NAMES.iter().position(|&n| n == name)
    }

    fn ina_index(name: &str) -> Option<usize> {
        INA_NAMES.iter().position(|&n| n == name)
    }

    fn inav_index(name: &str) -> Option<usize> {
        INAV_NAMES.iter().position(|&n| n == name)
    }

    fn iaav_index(name: &str) -> Option<usize> {
        IAAV_NAMES.iter().position(|&n| n == name)
    }

    /// The link-connection-status menu fields INAV..INLV, IAAV..ILLV and OUTV
    /// (`aCalcoutRecord.dbd:246-419`, `special(SPC_NOMOD)`): served read-only,
    /// their value derived from the link at init (see [`Self::implements_field`]).
    fn is_link_status_field(name: &str) -> bool {
        name == "OUTV" || Self::inav_index(name).is_some() || Self::iaav_index(name).is_some()
    }

    /// Classify all 25 links and publish INAV..INLV / IAAV..ILLV / OUTV,
    /// mirroring C `aCalcoutRecord.c::init_record` (209-243) and the
    /// `special()` re-classification (528-569). No-op without an async context.
    fn refresh_link_status(&self) {
        let mut links: Vec<(&'static str, String, LinkRole)> = Vec::with_capacity(25);
        for i in 0..12 {
            links.push((INAV_NAMES[i], self.inp_links[i].clone(), LinkRole::Input));
            links.push((IAAV_NAMES[i], self.ina_links[i].clone(), LinkRole::Input));
        }
        links.push(("OUTV", self.out.clone(), LinkRole::Output));
        // C `aCalcoutRecord.c:242`, `:569`, `:1157`.
        post_link_status(
            self.async_ctx.as_ref(),
            &self.link_gen,
            links,
            crate::server::recgbl::EventMask::VALUE,
        );
    }

    fn pa_index(name: &str) -> Option<usize> {
        PA_NAMES.iter().position(|&n| n == name)
    }

    /// Read `value` as a `DBR_DOUBLE` array, mirroring C `dbGetLink(...,
    /// DBR_DOUBLE, *pavalue, ...)`: an array source delivers its elements; a
    /// scalar source delivers a single element (the rest are zero-padded at
    /// use).
    fn coerce_array(value: EpicsValue) -> Option<Vec<f64>> {
        match value {
            EpicsValue::DoubleArray(v) => Some(v),
            EpicsValue::FloatArray(v) => Some(v.into_iter().map(|x| x as f64).collect()),
            EpicsValue::LongArray(v) => Some(v.into_iter().map(|x| x as f64).collect()),
            EpicsValue::ShortArray(v) => Some(v.into_iter().map(|x| x as f64).collect()),
            EpicsValue::UShortArray(v) => Some(v.into_iter().map(|x| x as f64).collect()),
            EpicsValue::ULongArray(v) => Some(v.into_iter().map(|x| x as f64).collect()),
            EpicsValue::Int64Array(v) => Some(v.into_iter().map(|x| x as f64).collect()),
            EpicsValue::UInt64Array(v) => Some(v.into_iter().map(|x| x as f64).collect()),
            EpicsValue::EnumArray(v) => Some(v.into_iter().map(|x| x as f64).collect()),
            // DBF_CHAR is signed epicsInt8 (see `EpicsValue::to_f64`).
            EpicsValue::CharArray(v) => Some(v.into_iter().map(|x| (x as i8) as f64).collect()),
            other => other.to_f64().map(|v| vec![v]),
        }
    }
}

/// `menu(acalcoutOOPT)` (`aCalcoutRecord.dbd`): like `scalcoutOOPT` with the
/// trailing "Never" choice (index 6).
const ACALCOUT_OOPT_CHOICES: &[&str] = &[
    "Every Time",
    "On Change",
    "When Zero",
    "When Non-zero",
    "Transition To Zero",
    "Transition To Non-zero",
    "Never",
];

/// `menu(acalcoutDOPT)`: 0="Use CALC", 1="Use OCAL".
const ACALCOUT_DOPT_CHOICES: &[&str] = &["Use CALC", "Use OCAL"];

/// `menu(acalcoutWAIT)`: 0=NoWait, 1=Wait.
const ACALCOUT_WAIT_CHOICES: &[&str] = &["NoWait", "Wait"];

/// `menu(acalcoutSIZE)`: 0=report NELM, 1=report NUSE to clients.
const ACALCOUT_SIZE_CHOICES: &[&str] = &["NELM", "NUSE"];

/// `acalcoutSIZE_NUSE` — the SECOND choice of the menu, so 1. The FIRST, and
/// therefore the default a record gets when the .db says nothing, is
/// `acalcoutSIZE_NELM` = 0 (`aCalcoutRecord.dbd:32-35`).
const ACALCOUT_SIZE_NUSE: i16 = 1;

/// `menu(acalcoutINAV)` — input-link PV status (`INAV..ILLV`, `OUTV`).
const ACALCOUT_INAV_CHOICES: &[&str] = &["Ext PV NC", "Ext PV OK", "Local PV", "Constant"];

impl Record for AcalcoutRecord {
    /// C `aCalcoutRecord.c::init_record` (:171-281) ends without touching
    /// MLST/ALST/LALM, unlike the `calcout` it is modelled on
    /// (`calcoutRecord.c:217-219`). An `acalcout` given a nonzero initial
    /// VAL posts that value once on its first cycle in C.
    fn seed_deadband_tracking(&mut self) {}

    fn record_type(&self) -> &'static str {
        "acalcout"
    }

    /// C `aCalcoutRecord.c::init_record` compiles CALC/OCAL into RPCL/ORPC and
    /// stores the postfix status in CLCV/OCLV (aCalcoutRecord.c:245-261) —
    /// the load-time half of the compile owner. A put goes through
    /// `special()` instead; `put_field` only stores the string, as C's dbPut
    /// does.
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            // C `init_record` pass 0 (`aCalcoutRecord.c:188-190`) — a .db or an
            // autosave restore can name NUSE > NELM, and the record must not
            // enter its first cycle holding it.
            self.clamp_nuse(CyclePostMask::ValueLog);
            self.recompile_calc();
            self.recompile_ocal();
        }
        Ok(())
    }

    /// The link-status menus (INAV..INLV, IAAV..ILLV, OUTV) are served read-only
    /// by `get_field`, but the record owns no WRITE path for them — they are
    /// `SPC_NOMOD`, classified from the link at init (`aCalcoutRecord.c:209-243`)
    /// and held as the record's `Default` of `Constant`(3) (the port has no
    /// separate init_record seed step). The loader's `.dbd`-initial seed and
    /// `.db field()` apply both key on this predicate to decide whether to WRITE
    /// a field; answering `false` keeps them from storing the `.dbd`
    /// `initial("1")` over the init-derived `Constant` — which is exactly the
    /// corruption that made a loaded record read `Ext PV OK`(1). The read is
    /// unaffected: `resolve_field` consults `get_field` independently.
    fn implements_field(&self, name: &str) -> bool {
        if Self::is_link_status_field(name) {
            return false;
        }
        self.get_field(name).is_some()
    }

    /// C `aCalcoutRecord.c::special:469-491` — a put to CALC/OCAL recompiles
    /// into RPCL/ORPC, stores `aCalcPostfix()`'s return status in CLCV/OCLV,
    /// posts DBE_VALUE for it, and returns 0: the put is ACCEPTED.
    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after {
            return Ok(());
        }
        match field {
            "CALC" => self.recompile_calc(),
            "OCAL" => self.recompile_ocal(),
            // C `aCalcoutRecord.c:494-501`:
            //
            //   case acalcoutRecordNUSE:
            //       if (pcalc->nuse > pcalc->nelm) {
            //           pcalc->nuse = pcalc->nelm;
            //           db_post_events(pcalc,&pcalc->nuse,DBE_VALUE);
            //           return(-1);
            //       }
            //       return(0);
            //
            // The clamped value STAYS and is posted — and the put still FAILS.
            // C's `dbPut` propagates the nonzero `dbPutSpecial` status
            // (`dbAccess.c:1399-1405`), so the client is told its NUSE was
            // illegal, the record does not run its `pp(TRUE)` cycle, and dbPut
            // makes no post of its own. The port silently accepted the put.
            "NUSE" => {
                if self.clamp_nuse(CyclePostMask::Value) {
                    return Err(CaError::InvalidValue(
                        "NUSE exceeds NELM; clamped to NELM".into(),
                    ));
                }
            }
            _ => {}
        }
        // C `aCalcoutRecord.c:503-533` re-classifies the link a put just
        // re-pointed — the INPA..INPL, INAA..INLL and OUT cases together.
        if Self::inp_index(field).is_some() || Self::ina_index(field).is_some() || field == "OUT" {
            self.refresh_link_status();
        }
        Ok(())
    }

    fn set_async_context(&mut self, name: String, db: AsyncDbHandle) {
        self.async_ctx = Some((name, db));
        // C `init_record` (aCalcoutRecord.c:209-243) classifies all 25 links
        // before any process. Every one of them is an acalcout-owned field
        // (OUT included), already applied when `add_record` runs.
        self.refresh_link_status();
    }

    /// C posts the validity field explicitly from `special()`
    /// (`db_post_events(pcalc, &pcalc->clcv, DBE_VALUE)`,
    /// aCalcoutRecord.c:478,490).
    fn monitor_side_effect_fields(&self, put_field: &str) -> &'static [&'static str] {
        match put_field {
            "CALC" => &["CLCV"],
            "OCAL" => &["OCLV"],
            _ => &[],
        }
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // ODLY continuation: the delayed re-process scheduled by a previous
        // cycle (C `aCalcoutRecord.c::process` `pact==TRUE` + `dlya` branch,
        // lines 421-430). Do NOT re-evaluate CALC/OCAL/OOPT — C clears DLYA and
        // runs `execOutput` directly; the alarm state captured by the delaying
        // cycle persists. Restore the captured output decision, clear DLYA, and
        // let the framework write the OUT link + post OEVT. Mirrors `scalcout`.
        if self.dlya == 1 {
            self.dlya = 0;
            self.cached_should_output = self.pending_output;
            self.pending_output = false;
            return Ok(self.complete_cycle());
        }

        let n = self.num_elements();

        // C `process` line 374-377 — through the one owner, so the post C makes
        // here cannot be dropped.
        self.clamp_nuse(CyclePostMask::ValueLog);

        // C `process` lines 379-387, under its own comment: "If we're getting
        // processed, we can no longer put off allocating memory". AVAL and OAV
        // are the two buffers a process cycle always needs, so they are charged
        // here, ahead of the fetch gate and of any calculation — a record whose
        // input link fails still pays for them.
        self.mem.allocate(ArrayBuf::Aval, self.nelm);
        self.mem.allocate(ArrayBuf::Oav, self.nelm);

        // C `process` line 393-395: snapshot scalar inputs (so PA..PL track
        // the values used in this calc). The framework fetches inputs before
        // `process()`, so these equal the current A..L. Runs BEFORE the fetch
        // gate below — C does it before calling `fetch_values`.
        self.pa = self.num_vals;

        // C `aCalcoutRecord.c::process` (399-414) wraps BOTH `doCalc` and
        // `afterCalc` in `if (fetch_values(pcalc)==0)`, and `fetch_values`
        // (1066-1067) returns at the first failing `dbGetLink`. So a failed
        // input link skips more here than in calc/calcout/sCalcout, whose gate
        // covers only the calc: `afterCalc` (aCalcoutRecord.c:281-345) is where
        // the CALC_ALARM/UDF update, `checkAlarms`, the OOPT decision, the
        // `pval = val` advance and the output all live, so NONE of them happen.
        // VAL/AVAL/OVAL/OAV and PVAL freeze, no OUT is written, and no limit
        // alarm is re-evaluated. Only C's tail — timestamp, monitors, forward
        // link — still runs, and the framework owns that.
        if self.fetch_gate_failed {
            self.cached_should_output = false;
            return Ok(self.complete_cycle());
        }

        self.calc_alarm = false;
        self.cstat = 0;

        // --- CALC (C call_aCalcPerform, first aCalcPerform → val, aval) ---
        //
        // C calls aCalcPerform unconditionally (`aCalcoutRecord.c:263`): RPCL is
        // always a program, and an empty or uncompilable CALC is the empty one,
        // which aCalcPerform fails (`aCalcPerform.c:312-314`) → cstat=-1 →
        // CALC_ALARM every process. So there is no "no expression" case here.
        let mut calc_failed = false;
        // CALC pass: the `VAL` token reads `prec->val` (this pass's result
        // field) before it is overwritten.
        let pass = self.eval(&self.compiled_calc, n, self.val, &self.aval);
        // C passes `&pcalc->amask` straight into aCalcPerform, which zeroes it at
        // entry (`aCalcPerform.c:326`) — so the CALC pass REPLACES the record's mask
        // and nothing carries over from the previous process.
        self.amask = pass.inputs.amask;
        self.apply_stores(&pass.inputs, n);
        match pass.result {
            Some((v, arr, finite)) => {
                self.val = v;
                self.aval = arr;
                if !finite {
                    calc_failed = true; // NaN/Inf result: C returns -1
                }
            }
            None => calc_failed = true,
        }

        // --- OCAL (C call_aCalcPerform: evaluated EVERY cycle when DOPT=Use
        //     OCAL, before afterCalc → oval, oav) ---
        if self.dopt == 1 {
            // OCAL pass: the `VAL` token reads `prec->oval` (this pass's
            // result field, C `p_dresult = &oval`), NOT `prec->val`, so an
            // OCAL accumulator like "VAL+1" runs on OVAL.
            //
            // It sees the CALC pass's stores, because C's two calls share the same
            // record fields — which is why `apply_stores` above must land before this
            // `eval` rebuilds its inputs from them.
            let pass = self.eval(&self.compiled_ocal, n, self.oval, &self.oav);
            // C `aCalcoutRecord.c:1288-1291` — the OCAL pass gets a LOCAL mask that is
            // then OR'd in, so a CALC-pass store stays flagged.
            self.amask |= pass.inputs.amask;
            self.apply_stores(&pass.inputs, n);
            match pass.result {
                Some((v, arr, finite)) => {
                    self.oval = v;
                    self.oav = arr;
                    if !finite {
                        calc_failed = true; // NaN/Inf result: C returns -1
                    }
                }
                None => calc_failed = true,
            }
        }

        // C `call_aCalcPerform` (`:1279-1298`) counts the non-NULL AA..LL
        // pointers before the first `aCalcPerform` and after the second, and
        // charges the difference:
        //
        // ```c
        // if (numAllocatedArraysPost > numAllocatedArraysPre) {
        //     pcalc->amem += (numAllocatedArraysPost-numAllocatedArraysPre)
        //                    * pcalc->nelm * sizeof(double);
        //     db_post_events(pcalc,&pcalc->amem, DBE_VALUE|DBE_LOG);
        // }
        // ```
        //
        // The arrays that can have appeared are exactly the ones the expression
        // STORED into: `aCalcPerform` allocates only in `STORE_AA..STORE_LL`
        // and `A_ASTORE` (`aCalcPerform.c:474`, `:515`), and both set the AMASK
        // bit it allocated for (`:487`, `:524`). So AMASK — already the union of
        // the two passes here, as it is in C (`:1291`) — names the candidates,
        // and the ledger drops the ones that were already allocated, which is
        // what C's before/after count does.
        for i in 0..ARR_NAMES.len() {
            if self.amask & (1 << i) != 0 {
                self.mem.allocate(ArrayBuf::Arr(i), self.nelm);
            }
        }

        if calc_failed {
            // C afterCalc line 304-305: cstat != 0 → CALC_ALARM (raised in
            // `check_alarms`). The UDF clear is C's `else` arm of the SAME test,
            // so it lives beside the CALC_ALARM raise in `check_alarms` (C's
            // `afterCalc` tail): a failed calc leaves `common.udf` untouched.
            self.calc_alarm = true;
            self.cstat = -1;
        }

        // --- OOPT decision (C afterCalc switch :313-335, using the
        //     just-computed VAL and the previous PVAL). `oopt_fires` is the
        //     OOPT-only decision — it IS C's `doOutput`, and gates the ODLY
        //     delay + DLYA pulse + completion (afterCalc :338). The
        //     IVOA=Don't_drive veto removes only the OUT write (C checks it
        //     inside execOutput, :920-921), NOT the defer — so it folds into
        //     `write_out`, never the defer gate.
        let oopt_fires = self.oopt_should_output();
        let mut write_out = oopt_fires;
        if calc_failed && self.ivoa == 1 {
            // menuIvoaDon_t_drive_outputs: suppress the OUT write (and OEVT) via
            // `cached_should_output`, since the generic `multi_output_links`
            // block (processing.rs) does not honour the framework severity
            // skip_out. C still defers under Don't_drive and runs the forward
            // link ODLY-late, so the defer stays gated on `oopt_fires`.
            write_out = false;
        }
        self.cached_should_output = write_out;

        // C afterCalc line 336: pval = val, captured BEFORE execOutput's IVOA
        // can overwrite VAL (so PVAL holds the failed VAL, matching C).
        self.pval = self.val;

        // IVOA=Set_output_to_IVOV is owned by the framework IVOA dispatch via
        // `apply_invalid_output_value` (processing.rs) on the Complete path,
        // matching C `execOutput` (aCalcoutRecord.c:923-924) which runs the
        // `oval=ivov` substitution inside execOutput — on the ODLY
        // *continuation*, never the delaying cycle. An earlier inline
        // `set_output_to_ivov()` here ran on the delaying cycle, so a direct get
        // of VAL/OVAL during the ODLY window observed IVOV ODLY-seconds early;
        // it was also redundant with the hook. The hook is the single owner.

        // POVL tracks OVAL (C monitor line 1039-1042) AFTER execOutput; the
        // framework posts OVAL on change, so this is the exposed previous-OVAL
        // value only.
        self.povl = self.oval;

        // ODLY (C `aCalcoutRecord.c::process`/`afterCalc` lines 338-346): when
        // an output should fire and ODLY > 0, defer the OUT-link write by ODLY
        // seconds. The delaying cycle sets DLYA=1, posts it (DBE_VALUE),
        // schedules the delayed callback, and `return(ASYNC)` BEFORE
        // `monitor()`/`recGblFwdLink()`/`execOutput` — so VAL/AVAL/OVAL monitors,
        // the OUT write, OEVT, and the forward link all fire on the delayed
        // (continuation) cycle, not now. Model this as an async-pending-notify
        // pass: post only DLYA now, suppress this cycle's output via
        // `cached_should_output=false`, and re-process after the delay; the
        // `dlya == 1` branch at the top then emits. IVOV substitution (C
        // `execOutput`) likewise runs on the continuation via the framework
        // hook, not now. Mirrors `scalcout`/`calcout`.
        if oopt_fires && self.odly > 0.0 {
            self.dlya = 1;
            self.pending_output = write_out;
            self.cached_should_output = false;
            let delay = crate::runtime::time::duration_from_secs(self.odly);
            return Ok(ProcessOutcome {
                result: RecordProcessResult::AsyncPendingNotify(vec![(
                    "DLYA".to_string(),
                    EpicsValue::UShort(1),
                )]),
                actions: vec![ProcessAction::ReprocessAfter(delay)],
                device_did_compute: false,
                post_write_fields: Vec::new(),
            });
        }

        Ok(self.complete_cycle())
    }

    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        use crate::server::recgbl::{self, alarm_status};
        use crate::server::record::AlarmSeverity;

        // C reaches `checkAlarms` only through `afterCalc` (aCalcoutRecord.c:310),
        // which the `fetch_values()` gate (:399) skips wholesale — so a cycle
        // whose input fetch failed re-evaluates no alarm at all: neither
        // CALC_ALARM nor the HIHI/LOLO limits, and LALM does not move.
        if self.fetch_gate_failed {
            return;
        }

        // C afterCalc line 304-305: a failed aCalcPerform raises CALC_ALARM.
        //
        // `calc_alarm` is a pending flag with ONE producer (the calc, above) and
        // ONE consumer (this hook) — the same shape as `calc`/`calcout`/
        // `scalcout`/`swait`. Take it, so no later reader can mistake it for
        // "the record is INVALID": severity belongs to `common`, and the record
        // must ask the framework, never this flag. It survives the ODLY
        // delaying cycle un-taken on purpose — that cycle returns
        // `AsyncPendingNotify` before the framework runs `check_alarms` (C
        // returns ASYNC before `monitor()`, so `nsta`/`nsev` stay pending too),
        // and the DLYA continuation consumes it here, exactly where C's
        // `recGblResetAlarms` commits the pending `nsev`.
        if std::mem::take(&mut self.calc_alarm) {
            // C `aCalcoutRecord.c:305` uses PLAIN `recGblSetSevr(pcalc,
            // CALC_ALARM, INVALID_ALARM)` — NULL message (empty namsg); PVA
            // falls back to the "CALC" condition string. No fabricated literal.
            recgbl::rec_gbl_set_sevr(common, alarm_status::CALC_ALARM, AlarmSeverity::Invalid);
            // C `aCalcoutRecord.c:304-307`: the failed-calc branch does NOT
            // touch `udf`, so a `caput UDF <byte>` made before a failing cycle
            // keeps its raw byte here — byte fidelity (`255` for `-1`).
        } else {
            // C `aCalcoutRecord.c:307` `else pcalc->udf = FALSE`: a finite calc
            // DEFINES VAL. This hook (C's `afterCalc` tail) is the SINGLE owner
            // of `common.udf` — [`Self::clears_udf`] is false, so the framework
            // never re-derives it from VAL and never collapses the put byte.
            common.udf = 0;
        }

        // C checkAlarms line 845: `if (pcalc->udf == TRUE)` — EXACT-ONE (`TRUE`
        // is 1). The guard returns before the limit check; UDF_ALARM itself is
        // raised centrally by `rec_gbl_check_udf` with the same exact-one flag
        // ([`Self::udf_alarm_on_exact_one`]). A byte that is neither 0 nor 1
        // (from a direct `caput UDF 255`) is NOT undefined here, matching C.
        if common.udf == 1 {
            return;
        }

        let val = self.val;
        let hyst = self.hyst;
        let lalm = self.lalm;

        // Per-level hysteresis (C `aCalcoutRecord.c:866-890`). A zero severity
        // disables that level (C `if (hhsv && ...)`), and each level's LALM
        // latch is gated on `recGblSetSevr` actually raising the severity
        // (`:867`, `:873`, `:879`, `:885`) — a level that fires under an
        // already-higher pending alarm must not arm the latch.
        if self.hhsv != 0 && (val >= self.hihi || (lalm == self.hihi && val >= self.hihi - hyst)) {
            if recgbl::rec_gbl_set_sevr(
                common,
                alarm_status::HIHI_ALARM,
                AlarmSeverity::from_u16(self.hhsv as u16),
            ) {
                self.lalm = self.hihi;
            }
        } else if self.llsv != 0
            && (val <= self.lolo || (lalm == self.lolo && val <= self.lolo + hyst))
        {
            if recgbl::rec_gbl_set_sevr(
                common,
                alarm_status::LOLO_ALARM,
                AlarmSeverity::from_u16(self.llsv as u16),
            ) {
                self.lalm = self.lolo;
            }
        } else if self.hsv != 0
            && (val >= self.high || (lalm == self.high && val >= self.high - hyst))
        {
            if recgbl::rec_gbl_set_sevr(
                common,
                alarm_status::HIGH_ALARM,
                AlarmSeverity::from_u16(self.hsv as u16),
            ) {
                self.lalm = self.high;
            }
        } else if self.lsv != 0 && (val <= self.low || (lalm == self.low && val <= self.low + hyst))
        {
            if recgbl::rec_gbl_set_sevr(
                common,
                alarm_status::LOW_ALARM,
                AlarmSeverity::from_u16(self.lsv as u16),
            ) {
                self.lalm = self.low;
            }
        } else {
            // C checkAlarms line 890: out of alarm by at least hyst.
            self.lalm = val;
        }
    }

    /// aCalcout does NOT re-derive UDF from VAL each cycle: C `pcalc->udf` is
    /// cleared only on a finite calc (`aCalcoutRecord.c:307`) and left otherwise
    /// — never recomputed from the value. Returning `false` makes
    /// [`Record::check_alarms`] the single owner of `common.udf` (the C
    /// `afterCalc` tail); the framework's per-cycle `udf = value_is_undefined()`
    /// re-derivation is suppressed, so a direct `caput UDF <byte>` keeps its raw
    /// `DBF_UCHAR` byte (`255` for `-1`) instead of collapsing to 0/1. A default
    /// record (empty CALC, which C's `aCalcPerform` fails every cycle) reads
    /// UDF=1 — the C-parity divergence the oracle measured.
    fn clears_udf(&self) -> bool {
        false
    }

    /// C `aCalcoutRecord.c:845` guards `checkAlarms` with `if (pcalc->udf ==
    /// TRUE)` — EXACT-ONE. With [`Self::clears_udf`] false the byte can hold a
    /// `caput`-supplied value other than 0/1, so this must match C's `== TRUE`
    /// (1): a byte of `255` is not undefined and raises no UDF_ALARM.
    fn udf_alarm_on_exact_one(&self) -> bool {
        true
    }

    /// IVOA=SetIVOV severity hook. The framework calls this when SEVR is
    /// INVALID and IVOA=2. Overriding the trait default (which writes VAL via
    /// `set_val`) targets `OVAL` — C's `pcalc->oval = pcalc->ivov`, see
    /// `set_output_to_ivov` and the module doc.
    ///
    /// The ONLY record-side gate is the output decision: C reaches the IVOA
    /// switch (aCalcoutRecord.c:912-934) through `execOutput` (`:895-936`),
    /// which runs only when `afterCalc`'s OOPT decision says the record
    /// outputs (:338-359, and the DLYA continuation at :421-430). The
    /// severity test inside it is `if (pcalc->nsev < INVALID_ALARM)` (:904)
    /// — the RECORD's severity, from ANY source: a CALC failure, an MS input
    /// link, a limit alarm at INVALID severity, UDF at UDFS=INVALID. It was
    /// gated on `calc_alarm` as well, which silently narrowed C's rule to the
    /// calc failure alone — an acalcout driven INVALID by HIHI/HHSV or by an
    /// MS link then drove the computed value at IVOA=SetIVOV instead of IVOV.
    /// The framework already evaluated `nsev` (it is why this hook was
    /// called), so asking a private flag for the severity is both wrong and
    /// redundant.
    fn apply_invalid_output_value(&mut self, _ivov: EpicsValue) -> CaResult<()> {
        if self.cached_should_output {
            self.set_output_to_ivov();
        }
        Ok(())
    }

    /// The CHANNEL capacity of the fourteen `special(SPC_DBADDR)` array fields
    /// — `AVAL`, `AA..LL`, `OAV` — which is `Self::dbaddr_no_elements`, NOT
    /// the served `Self::num_elements`. C computes the two in two hooks and
    /// they disagree by design under the default `SIZE=NELM` with a `NUSE`
    /// window open: `cvt_dbaddr` (`aCalcoutRecord.c:627-631`) reports `NELM`
    /// while `get_array_info` (`:672`) reports `acalcGetNumElements`.
    ///
    /// Advertising the served count instead sized the client's buffer at
    /// `NUSE`, and `ca_element_count` is settled once at create-channel time,
    /// so that client never saw the array widen when `NUSE` grew — the exact
    /// reconnect problem the `SIZE` menu's own comment (`:619-626`) describes,
    /// arriving under the setting chosen to avoid it.
    fn dbaddr_capacity(&self, field: &str) -> Option<u32> {
        // This IS `cvt_dbaddr`, and its first act is the allocation
        // (`aCalcoutRecord.c:589-617`) — the record cannot hand out a `pfield`
        // for a buffer it has not `calloc`'d yet, so the charge belongs here and
        // at no other read seam:
        //
        // ```c
        // if (ppd[i] == NULL) {
        //     ppd[i] = (double *)calloc(pcalc->nelm, sizeof(double));
        //     pcalc->amem += pcalc->nelm * sizeof(double);
        //     db_post_events(pcalc, &pcalc->amem, DBE_VALUE|DBE_LOG);
        //     pcalc->pmem = pcalc->amem;
        // }
        // paddr->pfield = ppd[i];
        // ```
        //
        // C reaches it from `dbNameToAddr`, i.e. once per resolved channel;
        // the port reaches it from `FieldDeclaration::field_native_count`,
        // whose callers are CA create-channel (`epics-ca-rs` `server/tcp.rs`)
        // and the three `dbNameToAddr`-shaped iocsh commands. `get_field` is
        // NOT this seam even though C's `get_array_info` (`:653-658`) charges
        // too: the framework reads every field through `get_field` for its own
        // change detection on every cycle, and C's `dbGet` does no such thing,
        // so charging there would allocate all fourteen buffers on cycle one.
        //
        // `field_native_count` has already refused any field the `.dbd` does
        // not declare `special(SPC_DBADDR)`, which is exactly AVAL, AA..LL and
        // OAV (`aCalcoutRecord.dbd`) — the fourteen buffers C allocates here.
        if let Some(buf) = Self::dbaddr_buf(field) {
            self.mem.allocate_for_dbaddr(buf, self.nelm);
        }
        Some(self.dbaddr_no_elements() as u32)
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "AVAL" => Some(self.array_field_value(&self.aval)),
            "PVAL" => Some(EpicsValue::Double(self.pval)),
            "NELM" => Some(EpicsValue::ULong(self.nelm)),
            "NUSE" => Some(EpicsValue::ULong(self.nuse)),
            "CALC" => Some(EpicsValue::String(self.calc.clone().into())),
            "CLCV" => Some(EpicsValue::Long(self.clcv)),
            "OCAL" => Some(EpicsValue::String(self.ocal.clone().into())),
            "OCLV" => Some(EpicsValue::Long(self.oclv)),
            "DOPT" => Some(EpicsValue::Short(self.dopt)),
            "OVAL" => Some(EpicsValue::Double(self.oval)),
            "OAV" => Some(self.array_field_value(&self.oav)),
            "POVL" => Some(EpicsValue::Double(self.povl)),
            "OUT" => Some(EpicsValue::String(self.out.clone().into())),
            "OOPT" => Some(EpicsValue::Short(self.oopt)),
            "ODLY" => Some(EpicsValue::Double(self.odly)),
            "WAIT" => Some(EpicsValue::Short(self.wait)),
            "DLYA" => Some(EpicsValue::UShort(self.dlya)),
            "OEVT" => Some(EpicsValue::UShort(self.oevt)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Double(self.ivov)),
            "OUTV" => Some(EpicsValue::Short(self.outv)),
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "HOPR" => Some(EpicsValue::Double(self.hopr)),
            "LOPR" => Some(EpicsValue::Double(self.lopr)),
            "HIHI" => Some(EpicsValue::Double(self.hihi)),
            "LOLO" => Some(EpicsValue::Double(self.lolo)),
            "HIGH" => Some(EpicsValue::Double(self.high)),
            "LOW" => Some(EpicsValue::Double(self.low)),
            "HHSV" => Some(EpicsValue::Short(self.hhsv)),
            "LLSV" => Some(EpicsValue::Short(self.llsv)),
            "HSV" => Some(EpicsValue::Short(self.hsv)),
            "LSV" => Some(EpicsValue::Short(self.lsv)),
            "HYST" => Some(EpicsValue::Double(self.hyst)),
            "ADEL" => Some(EpicsValue::Double(self.adel)),
            "MDEL" => Some(EpicsValue::Double(self.mdel)),
            "LALM" => Some(EpicsValue::Double(self.lalm)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
            "MLST" => Some(EpicsValue::Double(self.mlst)),
            "SIZE" => Some(EpicsValue::Short(self.size)),
            "NEWM" => Some(EpicsValue::ULong(self.newm)),
            "CACT" => Some(EpicsValue::Char(self.cact)),
            "CSTAT" => Some(EpicsValue::Long(self.cstat)),
            "AMASK" => Some(EpicsValue::ULong(self.amask)),
            "AMEM" => Some(EpicsValue::Long(self.mem.amem())),
            "PMEM" => Some(EpicsValue::Long(self.mem.pmem())),
            "VERS" => Some(EpicsValue::Double(VERSION)),
            _ => {
                if let Some(idx) = Self::num_index(name) {
                    return Some(EpicsValue::Double(self.num_vals[idx]));
                }
                if let Some(idx) = Self::arr_index(name) {
                    return Some(self.array_field_value(&self.arr_vals[idx]));
                }
                if let Some(idx) = Self::inp_index(name) {
                    return Some(EpicsValue::String(self.inp_links[idx].clone().into()));
                }
                if let Some(idx) = Self::ina_index(name) {
                    return Some(EpicsValue::String(self.ina_links[idx].clone().into()));
                }
                if let Some(idx) = Self::inav_index(name) {
                    return Some(EpicsValue::Short(self.inav[idx]));
                }
                if let Some(idx) = Self::iaav_index(name) {
                    return Some(EpicsValue::Short(self.iaav[idx]));
                }
                if let Some(idx) = Self::pa_index(name) {
                    return Some(EpicsValue::Double(self.pa[idx]));
                }
                None
            }
        }
    }

    /// C `fetch_values` sets NEWM when an INAA..INLL link delivers an array
    /// DIFFERENT from the one the field held (`aCalcoutRecord.c:1089-1106`):
    ///
    /// ```c
    /// for (j=0; j<numElements; j++) pcalc->paa[j] = (*pavalue)[j];   /* save */
    /// nRequest = acalcGetNumElements(pcalc);
    /// status = dbGetLink(plink, DBR_DOUBLE, *pavalue, 0, &nRequest); /* fetch */
    /// if (nRequest<numElements) for (j=nRequest; j<numElements; j++) (*pavalue)[j] = 0;
    /// for (j=0; j<numElements; j++) {
    ///     if (pcalc->paa[j] != (*pavalue)[j]) {pcalc->newm |= 1<<i; break;}
    /// }
    /// ```
    ///
    /// and `monitor()` posts exactly the NEWM-flagged arrays and clears the mask
    /// (`:1031-1036`) — see [`Self::take_cycle_posted_fields`].
    ///
    /// The comparison window is `acalcGetNumElements` (NUSE, else NELM) with the
    /// tail zero-filled: `AcalcoutRecord::array_field_value` is exactly that
    /// view, so comparing it across the write is C's compare.
    ///
    /// This is the INTERNAL write path — the framework's input-link delivery
    /// (`multi_input_links`: INAA..INLL -> AA..LL). A client caput lands in
    /// `put_field` and does NOT set NEWM, which is C: only `fetch_values` sets it.
    fn put_field_internal(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        let Some(i) = Self::arr_index(name) else {
            return crate::server::record::put_field_internal_default(self, name, value);
        };
        // C `fetch_values` (`:1082-1094`) allocates the array field and, on the
        // first array link it ever serves, the shared PAA compare buffer —
        // ahead of the save/fetch/compare below, because it is about to write
        // through both. Neither charge posts or advances PMEM here: this runs
        // inside the process cycle, and `complete_cycle` closes it.
        self.mem.allocate(ArrayBuf::Arr(i), self.nelm);
        self.mem.allocate(ArrayBuf::Paa, self.nelm);
        let before = self.array_field_value(&self.arr_vals[i]);
        // The LINK's bound, and it is this path's alone: C asks the link for exactly
        // `nRequest = acalcGetNumElements(pcalc)` elements — the NUSE window
        // (`aCalcoutRecord.c:1097-1098`) — so however long the source array is, no
        // more than a window's worth ever arrives. The bound is at the REQUEST, so
        // it belongs here, at the writer, and not in the shared owner: a client
        // `dbPut` reaching that same owner is bounded at `paddr->no_elements`
        // instead, which is the whole NELM buffer by default.
        let value = match Self::coerce_array(value) {
            Some(mut src) => {
                src.truncate(self.num_elements());
                EpicsValue::DoubleArray(src)
            }
            None => return Err(CaError::TypeMismatch(name.into())),
        };
        // The write itself — splice into the calloc(nelm) buffer, then zero the
        // rest of the window — is [`Self::write_array_field`], reached through
        // `put_field`. C's two writers do the same two things in the same order
        // (`fetch_values:1097-1102` and `put_array_info:727-731`); the only thing
        // that is this path's alone is NEWM, which only `fetch_values` sets.
        crate::server::record::put_field_internal_default(self, name, value)?;
        if self.array_field_value(&self.arr_vals[i]) != before {
            self.newm |= 1 << i;
        }
        Ok(())
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                self.val = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("VAL".into()))?;
                Ok(())
            }
            // AVAL and OAV are SPC_DBADDR array fields too (`aCalcoutRecord.c:702`,
            // `:711`), so a client put into either takes the same
            // `put_array_info` treatment as AA..LL: splice into the calloc(nelm)
            // buffer, zero the rest of the window. Replacing the whole vector
            // dropped both invariants at once.
            "AVAL" => {
                let src = Self::coerce_array(value)
                    .ok_or_else(|| CaError::TypeMismatch("AVAL".into()))?;
                // C `put_array_info` (`:703-708`) — the dbAddr shape: charge,
                // post AMEM, commit PMEM, all before the splice.
                self.mem.allocate_for_dbaddr(ArrayBuf::Aval, self.nelm);
                self.write_array_field(&src, self.dbaddr_no_elements(), |r| &mut r.aval);
                Ok(())
            }
            "PVAL" => {
                self.pval = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("PVAL".into()))?;
                Ok(())
            }
            "NELM" => {
                self.nelm = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("NELM".into()))?
                    as u32;
                Ok(())
            }
            // The RAW store, C's `dbPut`. The `nuse <= nelm` invariant is NOT
            // enforced here: `put_field` is also how a `.db` field and an
            // autosave restore land, and C's loader stores those verbatim — a
            // `field(NUSE,"5")` written BEFORE `field(NELM,"10")` would
            // otherwise be clamped against the DEFAULT nelm of 1 and silently
            // become 1. C repairs the invariant in `special()` and in
            // `init_record`, once every value is in.
            "NUSE" => {
                self.nuse = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("NUSE".into()))?
                    as u32;
                Ok(())
            }
            // C `dbPut` stores the string; `special()` compiles it and records
            // aCalcPostfix()'s status in CLCV (see `Self::special`).
            "CALC" => match value {
                EpicsValue::String(s) => {
                    self.calc = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("CALC".into())),
            },
            "CLCV" => {
                self.clcv = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("CLCV".into()))?
                    as i32;
                Ok(())
            }
            "OCAL" => match value {
                EpicsValue::String(s) => {
                    self.ocal = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OCAL".into())),
            },
            "OCLV" => {
                self.oclv = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("OCLV".into()))?
                    as i32;
                Ok(())
            }
            "DOPT" => match value {
                EpicsValue::Short(v) => {
                    self.dopt = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("DOPT".into())),
            },
            "OVAL" => {
                self.oval = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("OVAL".into()))?;
                Ok(())
            }
            "OAV" => {
                let src =
                    Self::coerce_array(value).ok_or_else(|| CaError::TypeMismatch("OAV".into()))?;
                // C `put_array_info` (`:711-716`).
                self.mem.allocate_for_dbaddr(ArrayBuf::Oav, self.nelm);
                self.write_array_field(&src, self.dbaddr_no_elements(), |r| &mut r.oav);
                Ok(())
            }
            "POVL" => {
                self.povl = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("POVL".into()))?;
                Ok(())
            }
            "OUT" => match value {
                EpicsValue::String(s) => {
                    self.out = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OUT".into())),
            },
            "OOPT" => match value {
                EpicsValue::Short(v) => {
                    self.oopt = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OOPT".into())),
            },
            "ODLY" => {
                self.odly = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("ODLY".into()))?;
                Ok(())
            }
            "WAIT" => match value {
                EpicsValue::Short(v) => {
                    self.wait = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("WAIT".into())),
            },
            "DLYA" => {
                self.dlya = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("DLYA".into()))?
                    as u16;
                Ok(())
            }
            "OEVT" => {
                self.oevt = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("OEVT".into()))?
                    as u16;
                Ok(())
            }
            "IVOA" => match value {
                EpicsValue::Short(v) => {
                    self.ivoa = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("IVOA".into())),
            },
            "IVOV" => {
                self.ivov = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("IVOV".into()))?;
                Ok(())
            }
            // OUTV is `special(SPC_NOMOD)` (aCalcoutRecord.dbd:414-419): its
            // value is DERIVED from the output link at init/`special`
            // (aCalcoutRecord.c:216,538). A CLIENT put is refused upstream by
            // `RecordInstance::is_no_mod`, which is the one gate every record's
            // SPC_NOMOD fields share; this arm is the link-status refresh's
            // write (`post_fields` -> `put_field_internal`), its only writer.
            "OUTV" => {
                self.outv = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("OUTV".into()))?
                    as i16;
                Ok(())
            }
            "EGU" => match value {
                EpicsValue::String(s) => {
                    self.egu = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("EGU".into())),
            },
            "PREC" => match value {
                EpicsValue::Short(v) => {
                    self.prec = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("PREC".into())),
            },
            "HOPR" => {
                self.hopr = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("HOPR".into()))?;
                Ok(())
            }
            "LOPR" => {
                self.lopr = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("LOPR".into()))?;
                Ok(())
            }
            "HIHI" => {
                self.hihi = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("HIHI".into()))?;
                Ok(())
            }
            "LOLO" => {
                self.lolo = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("LOLO".into()))?;
                Ok(())
            }
            "HIGH" => {
                self.high = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("HIGH".into()))?;
                Ok(())
            }
            "LOW" => {
                self.low = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("LOW".into()))?;
                Ok(())
            }
            "HHSV" => match value {
                EpicsValue::Short(v) => {
                    self.hhsv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("HHSV".into())),
            },
            "LLSV" => match value {
                EpicsValue::Short(v) => {
                    self.llsv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("LLSV".into())),
            },
            "HSV" => match value {
                EpicsValue::Short(v) => {
                    self.hsv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("HSV".into())),
            },
            "LSV" => match value {
                EpicsValue::Short(v) => {
                    self.lsv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("LSV".into())),
            },
            "HYST" => {
                self.hyst = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("HYST".into()))?;
                Ok(())
            }
            "ADEL" => {
                self.adel = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("ADEL".into()))?;
                Ok(())
            }
            "MDEL" => {
                self.mdel = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("MDEL".into()))?;
                Ok(())
            }
            // SPC_NOMOD trackers — accept the framework's internal deadband/
            // alarm writes (put_coerced "MLST"/"ALST", check_alarms "LALM").
            "LALM" => {
                self.lalm = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("LALM".into()))?;
                Ok(())
            }
            "ALST" => {
                self.alst = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("ALST".into()))?;
                Ok(())
            }
            "MLST" => {
                self.mlst = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("MLST".into()))?;
                Ok(())
            }
            "SIZE" => match value {
                EpicsValue::Short(v) => {
                    self.size = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIZE".into())),
            },
            "NEWM" => {
                self.newm = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("NEWM".into()))?
                    as u32;
                Ok(())
            }
            "CACT" => {
                self.cact = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("CACT".into()))?
                    as u8;
                Ok(())
            }
            "CSTAT" => {
                self.cstat = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("CSTAT".into()))?
                    as i32;
                Ok(())
            }
            "AMASK" => {
                self.amask = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("AMASK".into()))?
                    as u32;
                Ok(())
            }
            "AMEM" => {
                self.mem.store_amem(
                    value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch("AMEM".into()))?
                        as i32,
                );
                Ok(())
            }
            "PMEM" => {
                self.mem.store_pmem(
                    value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch("PMEM".into()))?
                        as i32,
                );
                Ok(())
            }
            // VERS is a fixed code-version constant; accept and ignore writes.
            "VERS" => Ok(()),
            _ => {
                if let Some(idx) = Self::num_index(name) {
                    self.num_vals[idx] = value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                    return Ok(());
                }
                if let Some(idx) = Self::arr_index(name) {
                    let src = Self::coerce_array(value)
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                    // C `put_array_info` (`:694-700`).
                    self.mem.allocate_for_dbaddr(ArrayBuf::Arr(idx), self.nelm);
                    self.write_array_field(&src, self.dbaddr_no_elements(), |r| {
                        &mut r.arr_vals[idx]
                    });
                    return Ok(());
                }
                if let Some(idx) = Self::inp_index(name) {
                    match value {
                        EpicsValue::String(s) => {
                            self.inp_links[idx] = s.as_str_lossy().into_owned();
                            return Ok(());
                        }
                        _ => return Err(CaError::TypeMismatch(name.into())),
                    }
                }
                if let Some(idx) = Self::ina_index(name) {
                    match value {
                        EpicsValue::String(s) => {
                            self.ina_links[idx] = s.as_str_lossy().into_owned();
                            return Ok(());
                        }
                        _ => return Err(CaError::TypeMismatch(name.into())),
                    }
                }
                // INAV..INLV / IAAV..ILLV are `special(SPC_NOMOD)`
                // (aCalcoutRecord.dbd:246-413): DERIVED from the link, refused
                // to clients by `is_no_mod` — see the `OUTV` arm above.
                if let Some(idx) = Self::inav_index(name) {
                    self.inav[idx] = value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?
                        as i16;
                    return Ok(());
                }
                if let Some(idx) = Self::iaav_index(name) {
                    self.iaav[idx] = value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?
                        as i16;
                    return Ok(());
                }
                if let Some(idx) = Self::pa_index(name) {
                    self.pa[idx] = value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                    return Ok(());
                }
                Err(CaError::FieldNotFound(name.to_string()))
            }
        }
    }

    /// Scalar inputs INPA..INPL → A..L and array inputs INAA..INLL → AA..LL
    /// (C `aCalcoutRecord.c::fetch_values`, all read as `DBR_DOUBLE`). The
    /// array pairs depend on the generic multi-input fetch passing array
    /// values through to `put_field` (see `processing.rs`): the scalar
    /// `to_f64` coercion drops arrays, so without that path the AA..LL links
    /// would not populate.
    /// C `aCalcoutRecord.c:209-216`: a CONSTANT input link is loaded into its
    /// value field ONCE, at `init_record`; `dbGetLink` then delivers nothing
    /// for it on every later process, so a client's `caput REC.A 99` stands.
    ///
    /// But only the NUMERIC half, and C says so in its own comment:
    ///
    /// ```c
    /// for (i=0; i<(MAX_FIELDS+ARRAY_MAX_FIELDS+1); i++, plink++, pvalue++, plinkValid++) {
    ///     if (plink->type == CONSTANT) {
    ///         /* Don't InitConstantLink the array links or the output link. */
    ///         if (i < MAX_FIELDS) {
    ///             recGblInitConstantLink(plink,DBF_DOUBLE,pvalue);
    ///             db_post_events(pcalc,pvalue,DBE_VALUE);
    ///         }
    ///         *plinkValid = acalcoutINAV_CON;
    /// ```
    ///
    /// The reason is the same one that limits [`Self::special_reseed_input_links`]:
    /// `pvalue` walks `&pcalc->a` as a scalar `double *`, so it has no meaning
    /// past INPL — `field(INAA,"5")` gives AA a `CON` link status and NOTHING
    /// else, and AA stays all zeros. Seeding it here wrote AA[0]=5, and once
    /// AMEM became real that showed up as a boot-time charge for a buffer C
    /// never allocated.
    ///
    /// It is the SAME guard as the `special()` one — C's `i < MAX_FIELDS` and
    /// its `fieldIndex <= acalcoutRecordINPL` — so it is the same slice, and
    /// the two cannot drift apart.
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        crate::server::record::seed_input_links(self.special_reseed_input_links())
    }

    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        ACALCOUT_INPUT_LINKS
    }

    /// C `aCalcoutRecord.c::special` (534-540) — the constant re-seed, under C's
    /// `if (fieldIndex <= acalcoutRecordINPL)` guard: only the NUMERIC inputs
    /// A..L are re-loaded. The ARRAY inputs AA..LL are not — C's
    /// `pvalue = &pcalc->a + lnkIndex` is a scalar `double *`, so an array link
    /// gets `INAV = CON` (the link-status refresh below) and nothing else. That
    /// guard is this slice: the first twelve entries of `multi_input_links`.
    fn special_reseed_input_links(&self) -> &[(&'static str, &'static str)] {
        &ACALCOUT_INPUT_LINKS[..ACALCOUT_NUMERIC_INPUTS]
    }

    /// C `aCalcoutRecord.c::fetch_values` (1068-1071, 1097) `return`s at the
    /// first failing `dbGetLink` — in the scalar INPA..INPL loop and then in the
    /// array INAA..INLL loop, in exactly the order `multi_input_links` lists
    /// them — and `process` (399) gates `doCalc` + `afterCalc` on the status.
    fn input_fetch_policy(&self) -> InputFetchPolicy {
        InputFetchPolicy::AbortOnFirstFailure
    }

    /// ...but only the SCALAR loop reaches its `dbGetLink` unconditionally. The
    /// ARRAY loop is wrapped in `if ((*plinkValid==acalcoutINAV_EXT) ||
    /// (*plinkValid==acalcoutINAV_LOC))` (`aCalcoutRecord.c:1078`), so an array
    /// link C believes cannot deliver is never read and never gates the cycle —
    /// which is why a typo'd or not-yet-loaded INAA leaves the record computing
    /// in C and killed it here. See [`Record::input_link_failure_is_inert`] for
    /// why this port answers the status question at read time.
    fn input_link_failure_is_inert(&self, link_field: &str) -> bool {
        ACALCOUT_INPUT_LINKS[ACALCOUT_NUMERIC_INPUTS..]
            .iter()
            .any(|(lf, _)| *lf == link_field)
    }

    fn set_fetch_gate_failed(&mut self, failed: bool) {
        self.fetch_gate_failed = failed;
    }

    /// The OUT link receives the array result: AVAL when DOPT=Use CALC, OAV
    /// when DOPT=Use OCAL (C `devaCalcoutSoft::write_acalcout`). Gated on
    /// the last cycle's OOPT/IVOA decision. The scalar companion below
    /// supplies the `nelm == 1 ? &val : aval` / `nelm == 1 ? &oval : oav`
    /// buffer choice — necessary since IVOA=Set_output_to_IVOV decouples
    /// `OVAL` from `OAV[0]` (see `set_output_to_ivov`).
    fn multi_output_links(&self) -> &[(&'static str, &'static str)] {
        if !self.cached_should_output {
            &[]
        } else if self.dopt == 1 {
            &[("OUT", "OAV")]
        } else {
            &[("OUT", "AVAL")]
        }
    }

    /// C `devaCalcoutSoft.c::write_acalcout` (75-87): when the effective
    /// element count resolves to 1, the write buffer is the scalar field
    /// — `&pcalc->val` under DOPT=Use CALC, `&pcalc->oval` under Use OCAL
    /// — not element 0 of the array.
    ///
    /// The effective count is C's `nelm`: the target's `no_elements` /
    /// `dbCaGetNelements` (resolved by the framework into `target`), clamped
    /// by the source count `i = (nuse > 0) ? nuse : nelm` (:82-83) — the
    /// staged array's served length here, so a 1-element source picks the
    /// scalar whatever the target is.
    fn multi_output_buffer(
        &self,
        link_field: &str,
        staged: EpicsValue,
        target: &OutTarget,
    ) -> EpicsValue {
        if link_field != "OUT" {
            return staged;
        }
        if staged.count() > 1 && target.element_count > 1 {
            return staged;
        }
        let scalar = if self.dopt == 1 { "OVAL" } else { "VAL" };
        self.get_field(scalar).unwrap_or(staged)
    }

    /// `OEVT` ("Event To Issue"): post the numeric output event when output
    /// fires. C `aCalcoutRecord.c` `execOutput` does `if (pcalc->oevt > 0)
    /// post_event((int)pcalc->oevt);` right after `writeValue`, gated to the
    /// same OOPT/calc-fail/ODLY decision as the OUT write (`cached_should_output`)
    /// — the framework adds the IVOA `Don't_drive` veto. Stringified so the
    /// numeric event matches a `SCAN="Event"` record's `EVNT`.
    fn output_event(&self) -> Option<String> {
        if self.cached_should_output && self.oevt > 0 {
            Some(self.oevt.to_string())
        } else {
            None
        }
    }

    /// Record-specific `DBF_MENU` fields. `HHSV..LSV` (menuAlarmSevr) and
    /// `IVOA` (menuIvoa) are shared menus resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "OOPT" => Some(ACALCOUT_OOPT_CHOICES),
            "DOPT" => Some(ACALCOUT_DOPT_CHOICES),
            "WAIT" => Some(ACALCOUT_WAIT_CHOICES),
            "SIZE" => Some(ACALCOUT_SIZE_CHOICES),
            _ => {
                if Self::inav_index(field).is_some()
                    || Self::iaav_index(field).is_some()
                    || field == "OUTV"
                {
                    Some(ACALCOUT_INAV_CHOICES)
                } else {
                    None
                }
            }
        }
    }

    /// Scalar inputs `A..L` are re-posted with the alarm bits on a cycle whose
    /// alarm transition fired, even when their value did not change: C
    /// `monitor()` posts each with `monitor_mask|DBE_VALUE|DBE_LOG` when
    /// `(*pnew != *pprev) || (monitor_mask & DBE_ALARM)`
    /// (aCalcoutRecord.c:1024-1029). The array inputs (`newm`-gated) and `OVAL`
    /// (change-gated) are NOT in this set, so they are omitted.
    fn alarm_cycle_monitored_fields(&self) -> &'static [&'static str] {
        &["A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L"]
    }

    /// SPC_NOMOD / internal trackers that this record mutates in `process()` /
    /// `check_alarms` but C `monitor()` never posts. Listing them keeps the
    /// framework's generic change-detection from over-posting their monitors
    /// (cf. permissive/state OVAL).
    /// PMEM is here for the reason the list exists: `aCalcoutRecord.c` posts
    /// `&pcalc->amem` at all nine of its post sites and `&pcalc->pmem` at none
    /// of them. PMEM is AMEM's last-posted copy — the guard in `monitor()`'s
    /// `if (pcalc->amem != pcalc->pmem)` — so it moves on every cycle that
    /// charges memory, and change detection would turn each of those into a
    /// `.PMEM` event C never sends.
    fn event_posted_fields(&self) -> &'static [&'static str] {
        &[
            "PVAL", "POVL", "LALM", "ALST", "MLST", "CSTAT", "PMEM", "PA", "PB", "PC", "PD", "PE",
            "PF", "PG", "PH", "PI", "PJ", "PK", "PL",
        ]
    }

    /// C posts an aCalcout array field from a per-cycle BIT MASK, and neither
    /// mask consults the value — a store that writes the value the field already
    /// held still posts.
    ///
    /// `afterCalc` (`aCalcoutRecord.c:294-298`), the AMASK half:
    ///
    /// ```c
    /// /* post array fields that aCalcPerform wrote to. */
    /// for (j=0, panew=&pcalc->aa; j<ARRAY_MAX_FIELDS; j++, panew++) {
    ///     if (*panew && (pcalc->amask & (1<<j))) {
    ///         db_post_events(pcalc, *panew, DBE_VALUE|DBE_LOG);
    ///     }
    /// }
    /// ```
    ///
    /// AMASK is the mask of arrays the EXPRESSION stored into: aCalcPerform
    /// zeroes it at the top of every run (`aCalcPerform.c:326`) and sets bit i
    /// in `STORE_AA..STORE_LL` (`:485-487`), so it is exactly this cycle's
    /// stores. `AA := AA` sets the bit, changes nothing, and still posts AA —
    /// the case the framework's change-detection loop drops.
    ///
    /// The other mask is NEWM — the arrays whose INAA..INLL LINK delivered a
    /// changed value this cycle (`fetch_values`, see
    /// [`AcalcoutRecord::put_field_internal`]). `monitor()` (`:1031-1036`) posts
    /// those the same way and then clears the mask:
    ///
    /// ```c
    /// for (i=0, panew=&pcalc->aa; i<ARRAY_MAX_FIELDS; i++, panew++) {
    ///     if (*panew && (pcalc->newm & (1<<i))) {
    ///         db_post_events(pcalc, *panew, monitor_mask|DBE_VALUE|DBE_LOG);
    ///     }
    /// }
    /// pcalc->newm = 0;
    /// ```
    ///
    /// A link-delivered change is usually also a value change, so the framework's
    /// change detection covers most of it — but not when the field was moved
    /// under the subscriber by a client caput, which posts the put's value and
    /// does NOT advance `last_posted`. The link then re-delivering the ORIGINAL
    /// value is a no-op to change detection and a NEWM post in C: without this
    /// the caput value is the last thing the subscriber ever hears, and it is
    /// wrong.
    ///
    /// AMASK needs no clearing here: [`AcalcoutRecord::process`] assigns
    /// `self.amask` from the pass's mask on every cycle, which is C's
    /// `*amask = 0` at the head of aCalcPerform. NEWM does — it accumulates
    /// across `fetch_values` calls until `monitor()` zeroes it, which is why this
    /// hook TAKES.
    fn take_cycle_posted_fields(&mut self) -> Vec<(&'static str, CyclePostMask)> {
        let (amask, newm) = (self.amask, self.newm);
        self.newm = 0;
        let mut marks = Vec::new();
        // C's NUSE clamp post, with the mask of whichever C site made it. Marked
        // by `clamp_nuse`, the only site that can clamp.
        if let Some(mask) = self.nuse_post_pending.take() {
            marks.push(("NUSE", mask));
        }
        // C's `db_post_events(pcalc, &pcalc->amem, ...)`, marked by whichever
        // `ArrayMem` operation made it — `monitor()`'s tail with the cycle's
        // `monitor_mask`, or a `put_array_info` charge with a literal
        // DBE_VALUE|DBE_LOG. The put path has no process cycle behind it, so
        // without the mark that post has no carrier at all.
        if let Some(mask) = self.mem.take_post() {
            marks.push(("AMEM", mask));
        }
        for (i, name) in ARR_NAMES.iter().enumerate() {
            let bit = 1u32 << i;
            // C `afterCalc` (`:296`) runs first, and with a LITERAL mask — the
            // alarm bits are not in scope there.
            if amask & bit != 0 {
                marks.push((*name, CyclePostMask::ValueLog));
            }
            // C `monitor()` (`:1034`) runs after, with `monitor_mask` folded in.
            // An array the expression STORED into and whose input link ALSO
            // delivered a change is in both masks, and C posts it from both
            // loops — two events, two masks.
            if newm & bit != 0 {
                marks.push((*name, CyclePostMask::MonitorValueLog));
            }
        }
        marks
    }

    /// AA..LL post from AMASK/NEWM and from nothing else. C `monitor()` keeps a
    /// previous copy of every SCALAR input (PA..PL, `aCalcoutRecord.c:1024-1029`)
    /// and of OVAL (POVL, `:1039`) and compares them — but it keeps NO previous
    /// copy of an array and compares no array anywhere. The only array
    /// comparison in the record is inside `fetch_values` (`:1104-1106`), against
    /// the link's own previous delivery, and its result IS the NEWM bit.
    ///
    /// (PAA exists, but it is `fetch_values`' scratch buffer for exactly that
    /// comparison — one buffer reused for every link, not a per-field previous
    /// value.)
    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &ARR_NAMES
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::record::FieldDeclaration;

    #[test]
    fn test_acalcout_default_matches_dbd_initials() {
        let rec = AcalcoutRecord::new();
        assert_eq!(rec.record_type(), "acalcout");
        assert_eq!(rec.get_field("NELM"), Some(EpicsValue::ULong(1)));
        assert_eq!(rec.get_field("NUSE"), Some(EpicsValue::ULong(0)));
        assert_eq!(rec.get_field("VERS"), Some(EpicsValue::Double(1.4)));
        // Link status = Constant(3): C init_record overwrites the dbd
        // initial("1") to acalcoutINAV_CON for the (default) constant links.
        assert_eq!(rec.get_field("INAV"), Some(EpicsValue::Short(3)));
        assert_eq!(rec.get_field("ILLV"), Some(EpicsValue::Short(3)));
        assert_eq!(rec.get_field("OUTV"), Some(EpicsValue::Short(3)));
        assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Double(0.0)));
    }

    #[test]
    fn link_status_fields_are_no_mod_and_read_the_derived_value() {
        use crate::server::record::RecordInstance;
        // INAV..INLV / IAAV..ILLV / OUTV are `special(SPC_NOMOD)` — derived from
        // the link, not client-settable. The refusal is the framework's one
        // SPC_NOMOD gate (`is_no_mod` -> `ECA_NOWTACCESS`, C `S_db_noMod`); the
        // record's own `put_field` is the refresh's write path and must accept,
        // which is why the refusal is asserted here and not on `put_field`.
        // A default record has all-constant links → every field = 3
        // (`acalcoutINAV_CON`). This is the C-parity case the oracle measured:
        // `caput ACALCOUT.INAV 0` then caget → C returns 3, not the put value.
        let inst = RecordInstance::new("A:LS".into(), AcalcoutRecord::new());
        for name in ["INAV", "INLV", "IAAV", "ILLV", "OUTV"] {
            assert_eq!(
                inst.resolve_field(name),
                Some(EpicsValue::Short(LINK_CON)),
                "{name} should read derived Constant(3)"
            );
            assert!(
                inst.is_no_mod(name),
                "{name} is SPC_NOMOD — a client put must be refused"
            );
        }
    }

    #[test]
    fn test_acalcout_field_list_count() {
        // 138 dbd record fields minus 6 DBF_NOACCESS internal pointer/scratch
        // fields (RPVT, PAVL, PAA, POAV, RPCL, ORPC) = 132 modeled fields.
        assert_eq!(AcalcoutRecord::default().field_list().len(), 132);
    }

    #[test]
    fn test_acalcout_scalar_calc() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("A", EpicsValue::Double(3.0)).unwrap();
        rec.put_field("B", EpicsValue::Double(4.0)).unwrap();
        rec.put_field("CALC", EpicsValue::String("A+B".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, 7.0);
        // Scalar result broadcasts into AVAL (C aCalcPerform toArray).
        assert_eq!(
            rec.get_field("AVAL"),
            Some(EpicsValue::DoubleArray(vec![7.0]))
        );
    }

    #[test]
    fn test_acalcout_array_calc_broadcasts_and_aval() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("NELM", EpicsValue::ULong(4)).unwrap();
        rec.put_field("AA", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0]))
            .unwrap();
        rec.put_field("CALC", EpicsValue::String("AA+1".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.process().unwrap();
        assert_eq!(
            rec.get_field("AVAL"),
            Some(EpicsValue::DoubleArray(vec![2.0, 3.0, 4.0, 5.0]))
        );
        // VAL is the array's first element (C to_double).
        assert_eq!(rec.val, 2.0);
    }

    #[test]
    fn test_acalcout_dopt_use_ocal_array() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("NELM", EpicsValue::ULong(3)).unwrap();
        rec.put_field("AA", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
            .unwrap();
        rec.put_field("CALC", EpicsValue::String("AA".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("OCAL", EpicsValue::String("AA*2".into()))
            .unwrap();
        rec.special("OCAL", true).unwrap();
        rec.put_field("DOPT", EpicsValue::Short(1)).unwrap();
        rec.process().unwrap();
        assert_eq!(
            rec.get_field("AVAL"),
            Some(EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
        );
        assert_eq!(
            rec.get_field("OAV"),
            Some(EpicsValue::DoubleArray(vec![2.0, 4.0, 6.0]))
        );
        assert_eq!(rec.oval, 2.0);
    }

    #[test]
    fn test_acalcout_oopt_every_outputs_aval() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("CALC", EpicsValue::String("42".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("OOPT", EpicsValue::Short(0)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.multi_output_links(), &[("OUT", "AVAL")]);
    }

    #[test]
    fn test_acalcout_oopt_never_suppresses_output() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("CALC", EpicsValue::String("42".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("OOPT", EpicsValue::Short(6)).unwrap(); // Never
        rec.process().unwrap();
        assert_eq!(rec.multi_output_links(), &[]);
    }

    #[test]
    fn test_acalcout_oopt_on_change_uses_mdel() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("CALC", EpicsValue::String("A".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("OOPT", EpicsValue::Short(1)).unwrap(); // On Change
        rec.put_field("MDEL", EpicsValue::Double(0.5)).unwrap();

        rec.put_field("A", EpicsValue::Double(5.0)).unwrap();
        rec.process().unwrap();
        assert!(rec.cached_should_output); // 0 -> 5 exceeds MDEL

        // No input change -> within MDEL -> no output.
        rec.process().unwrap();
        assert!(!rec.cached_should_output);
    }

    #[test]
    fn test_acalcout_dopt_use_ocal_drives_oav() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("CALC", EpicsValue::String("1".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("OCAL", EpicsValue::String("2".into()))
            .unwrap();
        rec.special("OCAL", true).unwrap();
        rec.put_field("DOPT", EpicsValue::Short(1)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.multi_output_links(), &[("OUT", "OAV")]);
    }

    /// The OCAL pass's `VAL` token reads the previous `OVAL` (C `p_dresult =
    /// &oval`), not the previous `VAL`, so an `OCAL` accumulator runs on OVAL.
    /// CALC pins VAL=5 each cycle; OVAL must accumulate 10/20/30. If `VAL` read
    /// `prec->val` it would stall at 15.
    #[test]
    fn test_acalcout_ocal_val_token_reads_oval() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("CALC", EpicsValue::String("5".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("OCAL", EpicsValue::String("VAL+10".into()))
            .unwrap();
        rec.special("OCAL", true).unwrap();
        rec.put_field("DOPT", EpicsValue::Short(1)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.get_field("OVAL"), Some(EpicsValue::Double(10.0)));
        rec.process().unwrap();
        assert_eq!(rec.get_field("OVAL"), Some(EpicsValue::Double(20.0)));
        rec.process().unwrap();
        assert_eq!(rec.get_field("OVAL"), Some(EpicsValue::Double(30.0)));
    }

    #[test]
    fn test_acalcout_calc_failure_sets_calc_alarm() {
        let mut rec = AcalcoutRecord::new();
        // Non-empty CALC that fails to compile.
        rec.calc = "???bad".into();
        rec.compiled_calc = CompiledExpr::empty(ExprKind::Array);
        rec.process().unwrap();
        assert!(rec.calc_alarm);
        assert_eq!(rec.cstat, -1);
    }

    /// UDF (C `pcalc->udf`) is cleared ONLY by a successful calc and is owned by
    /// `check_alarms` (C's `afterCalc` tail), NOT re-derived from VAL. C
    /// `aCalcoutRecord.c:305-307` sets `udf=FALSE` only on a finite result and
    /// never re-raises it; the oracle measured `ACALCOUT.UDF` C=1, port=0 for
    /// the default (empty-CALC) record, because the framework's `isnan(VAL=0.0)`
    /// default reported 0.
    #[test]
    fn udf_cleared_only_by_a_successful_calc() {
        use crate::server::record::CommonFields;
        let mut rec = AcalcoutRecord::new();
        let mut common = CommonFields::default();
        // Init: undefined, mirroring C `iocInit` udf=TRUE.
        assert_eq!(common.udf, 1, "fresh common is undefined (init 1)");
        // Empty CALC fails aCalcPerform every cycle → UDF stays set (C keeps 1).
        rec.process().unwrap();
        rec.check_alarms(&mut common);
        assert_eq!(
            common.udf, 1,
            "empty CALC failed → UDF stays 1, not isnan()=0"
        );
        // A finite result clears UDF (C `else pcalc->udf = FALSE`).
        rec.put_field("CALC", EpicsValue::String("1+1".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.process().unwrap();
        rec.check_alarms(&mut common);
        assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Double(2.0)));
        assert_eq!(common.udf, 0, "finite result clears UDF");
        // Monotonic: a later NON-finite result does NOT re-raise UDF — C raises
        // CALC_ALARM (`if(cstat)`) and leaves `udf` at FALSE.
        rec.put_field("CALC", EpicsValue::String("1e300*1e300".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.process().unwrap();
        assert!(rec.calc_alarm, "non-finite result raises CALC_ALARM");
        rec.check_alarms(&mut common);
        assert_eq!(
            common.udf, 0,
            "a failed calc after a success leaves UDF cleared, matching C"
        );
    }

    /// A failing calc leaves `common.udf` UNTOUCHED — so a direct `caput UDF
    /// <byte>` keeps its raw `DBF_UCHAR` byte across the `pp(TRUE)` re-process
    /// (`255` for `-1`, served signed). C `aCalcoutRecord.c:304-307` clears udf
    /// only in the `else` (success) arm; the oracle measured `caput UDF -1/255`
    /// → C=-1, port=1 because the wave-3 boolean cell collapsed every nonzero
    /// byte to 1.
    #[test]
    fn failing_calc_leaves_the_udf_byte_untouched() {
        use crate::server::record::CommonFields;
        let mut rec = AcalcoutRecord::new(); // empty CALC → fails every cycle
        let mut common = CommonFields::default();
        // A `caput UDF 255` (or `-1`, stored 255 in the signed DBF_UCHAR) put
        // this raw byte; the empty-CALC re-process must not collapse it.
        common.udf = 255;
        rec.process().unwrap();
        rec.check_alarms(&mut common);
        assert_eq!(
            common.udf, 255,
            "empty-CALC re-process must not touch the put byte"
        );
        // `caput UDF 0` likewise stands on a failing-calc record.
        common.udf = 0;
        rec.process().unwrap();
        rec.check_alarms(&mut common);
        assert_eq!(common.udf, 0, "caput UDF 0 stands over a failing calc");
    }

    /// A NaN/Inf calc result is written into VAL/AVAL (not left stale): C
    /// `aCalcPerform` stores `*p_dresult` before its non-finite tail returns -1
    /// (`aCalcPerform.c:1644`), so VAL holds the non-finite value and
    /// CALC_ALARM is raised.
    ///
    /// R8-7: the expression here used to be `1/0`, on the belief that the array
    /// engine divides in IEEE and yields NaN. It does not — C answers
    /// `myMAXFLOAT` (1e35) and `st=0` for `1/0` (`aCalcPerform.c:690-696`), so
    /// that expression pinned invented behaviour and could never reach this
    /// path. `1e300*1e300` is the C-verified non-finite case: the compiled
    /// `aCalcPerform` prints `st=-1 d=inf`.
    #[test]
    fn test_acalcout_nonfinite_result_written_to_val() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("CALC", EpicsValue::String("1e300*1e300".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.process().unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => {
                assert!(!v.is_finite(), "VAL = {v}, expected non-finite")
            }
            other => panic!("expected Double VAL, got {other:?}"),
        }
        assert!(rec.calc_alarm);
    }

    /// R9-2 — a NaN SCALAR result reaches AVAL through C's `toArray(ps,1)`
    /// (`aCalcPerform.c:1624`), whose promotion fills 0 for a NaN
    /// (`to_array`, :135-138). So VAL keeps the NaN and AVAL is all ZEROS, not
    /// all NaN. Compiled aCalcPerform: `ACOS(2)` -> st=-1 d=nan a=[0 x8].
    #[test]
    fn test_acalcout_nan_scalar_fills_aval_with_zeros() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("NELM", EpicsValue::ULong(4)).unwrap();
        rec.put_field("CALC", EpicsValue::String("ACOS(2)".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.process().unwrap();
        match rec.get_field("VAL") {
            Some(EpicsValue::Double(v)) => assert!(v.is_nan(), "VAL = {v}, expected NaN"),
            other => panic!("expected Double VAL, got {other:?}"),
        }
        assert_eq!(rec.aval, vec![0.0; 4], "C fills AVAL with zeros for a NaN");
        assert!(rec.calc_alarm);
    }

    #[test]
    fn test_acalcout_ivoa_dont_drive_on_failure() {
        let mut rec = AcalcoutRecord::new();
        rec.calc = "???bad".into();
        rec.compiled_calc = CompiledExpr::empty(ExprKind::Array);
        rec.put_field("IVOA", EpicsValue::Short(1)).unwrap(); // Don't drive
        rec.process().unwrap();
        assert!(!rec.cached_should_output);
        assert_eq!(rec.multi_output_links(), &[]);
    }

    #[test]
    fn test_acalcout_ivoa_set_ivov_on_failure() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("NELM", EpicsValue::ULong(2)).unwrap();
        rec.calc = "???bad".into();
        rec.compiled_calc = CompiledExpr::empty(ExprKind::Array);
        rec.put_field("IVOA", EpicsValue::Short(2)).unwrap(); // Set to IVOV
        rec.put_field("IVOV", EpicsValue::Double(9.0)).unwrap();
        rec.process().unwrap();
        let val_before = rec.val;
        let aval_before = rec.get_field("AVAL");
        // IVOV substitution is owned by the framework IVOA dispatch via
        // `apply_invalid_output_value` on the Complete path; C's arm is
        // `pcalc->oval = pcalc->ivov;` ALONE (aCalcoutRecord.c:924) —
        // VAL/AVAL/OAV keep the failed cycle's values.
        rec.apply_invalid_output_value(EpicsValue::Double(9.0))
            .unwrap();
        assert_eq!(rec.oval, 9.0, "C sets only the scalar OVAL");
        assert_eq!(rec.val, val_before, "VAL is not IVOV's target");
        assert_eq!(
            rec.get_field("AVAL"),
            aval_before,
            "AVAL is not IVOV's target"
        );
    }

    /// The number a `softIoc` running `record(acalcout,"T:ACO") {}` reports.
    /// C boots with every array pointer NULL and charges nothing until something
    /// forces an allocation; the FIRST process forces four, at NELM=1 and
    /// `sizeof(double)` each:
    ///
    /// * AVAL and OAV in `process` (`aCalcoutRecord.c:379-387`),
    /// * PAVL and POAV in `monitor` (`:972-979`),
    ///
    /// and `monitor`'s tail then commits PMEM (`:1044-1047`). 4 * 8 = 32.
    #[test]
    fn boot_charges_nothing_and_one_process_charges_the_four_buffers_c_allocates() {
        let mut rec = AcalcoutRecord::new();
        assert_eq!(rec.get_field("AMEM"), Some(EpicsValue::Long(0)));
        assert_eq!(rec.get_field("PMEM"), Some(EpicsValue::Long(0)));

        rec.process().unwrap();
        assert_eq!(
            rec.get_field("AMEM"),
            Some(EpicsValue::Long(32)),
            "AVAL + OAV + PAVL + POAV at NELM=1"
        );
        assert_eq!(
            rec.get_field("PMEM"),
            Some(EpicsValue::Long(32)),
            "monitor()'s tail committed it"
        );
    }

    /// The `== NULL` boundary. C charges a buffer on the cycle that allocates it
    /// and never again; every later cycle takes the `!= NULL` arm and adds
    /// nothing, so AMEM is flat and `amem == pmem` keeps `monitor()` silent.
    #[test]
    fn a_buffer_is_charged_once_however_many_cycles_run() {
        let mut rec = AcalcoutRecord::new();
        rec.process().unwrap();
        let _ = rec.take_cycle_posted_fields();

        rec.process().unwrap();
        assert_eq!(rec.get_field("AMEM"), Some(EpicsValue::Long(32)));
        assert_eq!(rec.get_field("PMEM"), Some(EpicsValue::Long(32)));
        assert!(
            !rec.take_cycle_posted_fields()
                .iter()
                .any(|(f, _)| *f == "AMEM"),
            "amem == pmem, so monitor() posts nothing (:1044)"
        );
    }

    /// The charge is `pcalc->nelm * sizeof(double)`, not a constant: the same
    /// four buffers cost 4 * 4 * 8 at NELM=4.
    #[test]
    fn the_charge_per_buffer_is_nelm_doubles() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("NELM", EpicsValue::ULong(4)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.get_field("AMEM"), Some(EpicsValue::Long(128)));
    }

    /// A failed input link is the other completing path through C's `process`:
    /// `if (fetch_values(pcalc)==0)` skips `doCalc`/`afterCalc`, but the AVAL/OAV
    /// charge sits ABOVE that test (`:379-387`) and `monitor()` still runs at the
    /// tail (`:442`), so the cycle pays the full 32 all the same.
    #[test]
    fn a_failed_fetch_gate_still_charges_and_commits() {
        let mut rec = AcalcoutRecord::new();
        rec.set_fetch_gate_failed(true);
        rec.process().unwrap();
        assert_eq!(rec.get_field("AMEM"), Some(EpicsValue::Long(32)));
        assert_eq!(rec.get_field("PMEM"), Some(EpicsValue::Long(32)));
    }

    /// The NON-completing path. C's ODLY delaying cycle `return(ASYNC)`s from
    /// `afterCalc` (`:346`) before `process` reaches `monitor()`, so PAVL/POAV
    /// are not allocated and PMEM is not committed on it — AMEM already carries
    /// `process`'s own AVAL+OAV charge, and the two disagree until the delayed
    /// continuation closes the cycle.
    #[test]
    fn the_odly_delaying_cycle_does_not_reach_monitor() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("CALC", EpicsValue::String("1".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("ODLY", EpicsValue::Double(0.05)).unwrap();

        rec.process().unwrap();
        assert_eq!(rec.dlya, 1, "the delaying cycle");
        assert_eq!(
            rec.get_field("AMEM"),
            Some(EpicsValue::Long(16)),
            "AVAL + OAV only"
        );
        assert_eq!(
            rec.get_field("PMEM"),
            Some(EpicsValue::Long(0)),
            "no monitor() ran to commit it"
        );

        rec.process().unwrap();
        assert_eq!(rec.get_field("AMEM"), Some(EpicsValue::Long(32)));
        assert_eq!(rec.get_field("PMEM"), Some(EpicsValue::Long(32)));
    }

    /// `aCalcPerform` allocates an AA..LL buffer when the expression STORES into
    /// it (`aCalcPerform.c:474`) and flags it in AMASK; `call_aCalcPerform`
    /// charges the arrays that appeared (`aCalcoutRecord.c:1293-1298`). A CALC
    /// that only READS an array allocates nothing.
    #[test]
    fn an_expression_store_into_aa_charges_it_and_a_read_does_not() {
        let mut stored = AcalcoutRecord::new();
        stored
            .put_field("CALC", EpicsValue::String("AA:=3;SUM(AA)".into()))
            .unwrap();
        stored.special("CALC", true).unwrap();
        stored.process().unwrap();
        assert_ne!(stored.amask & 1, 0, "STORE_AA set the mask bit");
        assert_eq!(
            stored.get_field("AMEM"),
            Some(EpicsValue::Long(40)),
            "the four process/monitor buffers plus AA"
        );

        let mut read = AcalcoutRecord::new();
        read.put_field("CALC", EpicsValue::String("AA+1".into()))
            .unwrap();
        read.special("CALC", true).unwrap();
        read.process().unwrap();
        assert_eq!(
            read.get_field("AMEM"),
            Some(EpicsValue::Long(32)),
            "a fetch allocates nothing"
        );
    }

    /// The dbAddr sites do not wait for `monitor()`: `put_array_info` charges,
    /// posts AMEM with a LITERAL `DBE_VALUE|DBE_LOG`, and commits PMEM on the
    /// spot (`aCalcoutRecord.c:694-700`) — there is no process cycle behind a
    /// `dbPut` to close it.
    #[test]
    fn a_client_put_into_an_array_field_charges_posts_and_commits_at_once() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("NELM", EpicsValue::ULong(2)).unwrap();
        rec.put_field("AA", EpicsValue::DoubleArray(vec![1.0, 2.0]))
            .unwrap();
        assert_eq!(rec.get_field("AMEM"), Some(EpicsValue::Long(16)));
        assert_eq!(
            rec.get_field("PMEM"),
            Some(EpicsValue::Long(16)),
            "pmem = amem, right there (:699)"
        );
        assert_eq!(
            rec.take_cycle_posted_fields(),
            vec![("AMEM", CyclePostMask::ValueLog)],
            "C's literal DBE_VALUE|DBE_LOG (:698)"
        );

        rec.put_field("AA", EpicsValue::DoubleArray(vec![3.0, 4.0]))
            .unwrap();
        assert_eq!(
            rec.get_field("AMEM"),
            Some(EpicsValue::Long(16)),
            "the second put finds it allocated"
        );
        assert!(
            !rec.take_cycle_posted_fields()
                .iter()
                .any(|(f, _)| *f == "AMEM"),
            "and posts nothing"
        );
    }

    /// C `init_record` (`aCalcoutRecord.c:209-216`) runs
    /// `recGblInitConstantLink` only for `i < MAX_FIELDS` — its own comment
    /// says "Don't InitConstantLink the array links or the output link" —
    /// because `pvalue` is a scalar `double *` walking `&pcalc->a`. A constant
    /// INAA therefore leaves AA untouched, and with it AMEM: no buffer was
    /// allocated for a link that delivered nothing.
    #[test]
    fn a_constant_array_link_is_not_seeded_and_charges_nothing() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("INPA", EpicsValue::String("7".into()))
            .unwrap();
        rec.put_field("INAA", EpicsValue::String("5".into()))
            .unwrap();

        for seed in rec.constant_init_links() {
            crate::server::record::rec_gbl_init_constant_link(&mut rec, &seed);
        }

        assert_eq!(
            rec.get_field("A"),
            Some(EpicsValue::Double(7.0)),
            "the scalar half IS seeded (:213)"
        );
        assert_eq!(
            rec.get_field("AA"),
            Some(EpicsValue::DoubleArray(vec![0.0])),
            "the array half is not (:212)"
        );
        assert_eq!(
            rec.get_field("AMEM"),
            Some(EpicsValue::Long(0)),
            "and so nothing was allocated for it"
        );
    }

    /// C posts `&pcalc->amem` at every one of its nine post sites and
    /// `&pcalc->pmem` at none, so PMEM must never reach a subscriber.
    #[test]
    fn pmem_is_never_posted() {
        let rec = AcalcoutRecord::new();
        assert!(rec.event_posted_fields().contains(&"PMEM"));
    }

    /// C `cvt_dbaddr` (`aCalcoutRecord.c:589-617`) allocates the buffer it is
    /// about to hand out a `pfield` for, charges it, posts AMEM with a literal
    /// `DBE_VALUE|DBE_LOG` and commits PMEM — all before the record has ever
    /// processed. Resolving a channel on `.AA` is what runs it, and here that
    /// is `field_native_count` → `dbaddr_capacity`.
    #[test]
    fn resolving_a_channel_on_an_array_field_charges_it_like_cvt_dbaddr() {
        let rec = AcalcoutRecord::new();
        assert_eq!(rec.get_field("AMEM"), Some(EpicsValue::Long(0)));

        assert_eq!(rec.field_native_count("AA"), Some(1));
        assert_eq!(
            rec.get_field("AMEM"),
            Some(EpicsValue::Long(8)),
            "one buffer at NELM=1"
        );
        assert_eq!(
            rec.get_field("PMEM"),
            Some(EpicsValue::Long(8)),
            "pmem = amem, right there (:598)"
        );

        // A second client resolving the same channel finds it allocated.
        assert_eq!(rec.field_native_count("AA"), Some(1));
        assert_eq!(rec.get_field("AMEM"), Some(EpicsValue::Long(8)));
        // A different field is a different buffer.
        assert_eq!(rec.field_native_count("OAV"), Some(1));
        assert_eq!(rec.get_field("AMEM"), Some(EpicsValue::Long(16)));
    }

    /// The reason the charge is at `dbaddr_capacity` and not at `get_field`.
    /// The framework reads EVERY field through `get_field` on every cycle to
    /// change-detect it; C's `dbGet` does nothing of the kind. A process cycle
    /// must therefore charge its four buffers and no more — not the fourteen
    /// `SPC_DBADDR` ones — however many times the record is read.
    #[test]
    fn an_internal_process_cycle_does_not_charge_the_dbaddr_buffers() {
        let mut rec = AcalcoutRecord::new();
        rec.process().unwrap();

        // The change-detection walk, in full.
        for desc in rec.field_list() {
            let _ = rec.get_field(desc.name);
        }
        rec.process().unwrap();
        for desc in rec.field_list() {
            let _ = rec.get_field(desc.name);
        }

        assert_eq!(
            rec.get_field("AMEM"),
            Some(EpicsValue::Long(32)),
            "AVAL + OAV + PAVL + POAV only — reading AA..LL allocates nothing"
        );
    }

    /// C repairs `NUSE > NELM` in `special()`, NOT in the field store: `dbPut`
    /// writes the value the client sent and `dbPutSpecial(paddr,1)` then clamps
    /// it, posts it, and RETURNS -1 so the put fails (`aCalcoutRecord.c:494-501`).
    /// The store itself must stay raw — it is also the `.db` / autosave load path,
    /// where NELM may not have arrived yet.
    #[test]
    fn test_acalcout_nuse_special_clamps_posts_and_refuses_the_put() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("NELM", EpicsValue::ULong(4)).unwrap();

        rec.put_field("NUSE", EpicsValue::ULong(10)).unwrap();
        assert_eq!(
            rec.get_field("NUSE"),
            Some(EpicsValue::ULong(10)),
            "the raw store keeps what the client sent; special() has not run yet"
        );

        let status = rec.special("NUSE", true);
        assert!(status.is_err(), "C returns -1, so the put must fail");
        assert_eq!(
            rec.get_field("NUSE"),
            Some(EpicsValue::ULong(4)),
            "and the clamped value stays"
        );
        assert_eq!(
            rec.take_cycle_posted_fields(),
            vec![("NUSE", CyclePostMask::Value)],
            "posted with C's literal DBE_VALUE (:497)"
        );
    }

    /// The load-order case the raw store protects: a `.db` that lists NUSE before
    /// NELM. C stores both, then `init_record` pass 0 clamps once with the final
    /// NELM (`:188-190`) — NUSE=5 is legal under NELM=10 and survives. Clamping
    /// inside the store would have measured it against the DEFAULT nelm of 1.
    #[test]
    fn nuse_is_clamped_at_init_against_the_final_nelm_not_the_default() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("NUSE", EpicsValue::ULong(5)).unwrap();
        rec.put_field("NELM", EpicsValue::ULong(10)).unwrap();

        rec.init_record(0).unwrap();

        assert_eq!(rec.get_field("NUSE"), Some(EpicsValue::ULong(5)));
        assert!(
            rec.take_cycle_posted_fields().is_empty(),
            "nothing was clamped, so nothing is posted"
        );
    }

    /// ...and the same init pass DOES repair a genuinely illegal restore.
    #[test]
    fn init_clamps_an_illegal_restored_nuse_and_posts_it() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("NELM", EpicsValue::ULong(4)).unwrap();
        rec.put_field("NUSE", EpicsValue::ULong(99)).unwrap();

        rec.init_record(0).unwrap();

        assert_eq!(rec.get_field("NUSE"), Some(EpicsValue::ULong(4)));
        assert_eq!(
            rec.take_cycle_posted_fields(),
            vec![("NUSE", CyclePostMask::ValueLog)],
            "C's init post is DBE_VALUE|DBE_LOG (:189)"
        );
    }

    #[test]
    fn test_acalcout_size_nuse_reports_nuse_count() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("NELM", EpicsValue::ULong(8)).unwrap();
        rec.put_field("NUSE", EpicsValue::ULong(3)).unwrap();
        rec.put_field("AA", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0, 5.0]))
            .unwrap();
        rec.put_field("CALC", EpicsValue::String("AA".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.process().unwrap();
        // num_elements = NUSE (3) since 0 < NUSE < NELM.
        assert_eq!(
            rec.get_field("AVAL"),
            Some(EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
        );
    }

    #[test]
    fn test_acalcout_field_not_found() {
        let mut rec = AcalcoutRecord::new();
        assert!(rec.put_field("ZZZ", EpicsValue::Double(1.0)).is_err());
        assert!(rec.get_field("ZZZ").is_none());
    }

    #[test]
    fn test_acalcout_menu_choices() {
        let rec = AcalcoutRecord::new();
        assert_eq!(rec.menu_field_choices("OOPT"), Some(ACALCOUT_OOPT_CHOICES));
        assert_eq!(rec.menu_field_choices("DOPT"), Some(ACALCOUT_DOPT_CHOICES));
        assert_eq!(rec.menu_field_choices("WAIT"), Some(ACALCOUT_WAIT_CHOICES));
        assert_eq!(rec.menu_field_choices("SIZE"), Some(ACALCOUT_SIZE_CHOICES));
        assert_eq!(rec.menu_field_choices("INAV"), Some(ACALCOUT_INAV_CHOICES));
        assert_eq!(rec.menu_field_choices("ILLV"), Some(ACALCOUT_INAV_CHOICES));
        assert_eq!(rec.menu_field_choices("OUTV"), Some(ACALCOUT_INAV_CHOICES));
        // HHSV / IVOA are shared menus, not record-specific.
        assert_eq!(rec.menu_field_choices("HHSV"), None);
        assert_eq!(rec.menu_field_choices("IVOA"), None);
    }
}
