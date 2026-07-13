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
//! `as_f64()` (Double→v, Array→arr[0]) is `VAL`, and `broadcast(n)`
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
//! - `SIZE` (NELM vs NUSE) is stored but does not gate the client-advertised
//!   array capacity: the served element count is always
//!   `acalcGetNumElements()` (C `get_array_info`), matching C's served data.
//!   C `cvt_dbaddr` additionally advertises `NELM` as the channel capacity
//!   under the default `SIZE=NELM`; the single-length model serves that count
//!   directly, so the advertised capacity differs only when `NUSE` is set to
//!   `0 < NUSE < NELM` under `SIZE=NELM`. `NELM=0` (degenerate; dbd
//!   initial 1) serves a 1-element array, where C serves 0.
//! - `UDF` follows the framework `value_is_undefined()` (NaN VAL ⇒ UDF). A
//!   NaN result correctly keeps UDF; but a compile-failure/empty `CALC` on the
//!   first process shows `UDF=0` (VAL still 0.0) where C keeps `UDF=1` (C sets
//!   `udf=FALSE` only on a successful calc). Narrow: only before the first
//!   successful calc.
//! - `LALM` advances on every matched alarm level; C gates it on
//!   `if (recGblSetSevr(...))` (advance only when that severity actually
//!   raised `nsev`). Mirrors the framework-wide `rec_gbl_set_sevr` (returns
//!   void); a higher pre-existing severity can thus perturb next-cycle
//!   hysteresis vs C.

use crate::error::{CaError, CaResult};
use crate::server::record::{
    CyclePostMask, FieldDesc, InputFetchPolicy, ProcessAction, ProcessOutcome, Record,
    RecordProcessResult,
};
use crate::types::{DbFieldType, EpicsValue, PvString};

use super::calc_compile;
use crate::calc::{ArrayInputs, CompiledExpr, ExprKind, acalc_eval};
// `LINK_CON` (= 3, the `Constant` link-status index) is the value C
// `init_record` writes for an unconfigured link; shared with `calcout`.
use super::link_status::LINK_CON;

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
    nuse: u32,

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

    // --- link status (static, dbd initials) ---
    inav: [i16; 12], // INAV..INLV
    iaav: [i16; 12], // IAAV..ILLV
    outv: i16,

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
    amem: i32,
    pmem: i32,
    newm: u32,

    // --- process flags ---
    calc_alarm: bool,
    /// This cycle's `fetch_values()` outcome, pushed by the framework through
    /// `set_fetch_gate_failed`. C `aCalcoutRecord.c::process` (399) runs
    /// `doCalc` + `afterCalc` only `if (fetch_values(pcalc)==0)`, and
    /// `fetch_values` (1068-1071) returns at the first failing `dbGetLink`.
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
            amem: 0,
            pmem: 0,
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

    /// Current array element count (C `acalcGetNumElements`,
    /// `aCalcoutRecord.c:160-168`): `NUSE` when `0 < NUSE < NELM`, else
    /// `NELM`. At least 1 (C always `calloc(nelm)` with `nelm >= 1`).
    fn num_elements(&self) -> usize {
        let n = if self.nuse > 0 && self.nuse < self.nelm {
            self.nuse
        } else {
            self.nelm
        };
        (n as usize).max(1)
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
    /// /* put_array_info, aCalcoutRecord.c:726-731 — the client dbPut */
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
    /// The whole rule, and it is ONE rule: a write touches `[0, numElements)` and
    /// nothing else. It splices its elements into the record's permanent
    /// `calloc(nelm)` buffer, is BOUNDED at the window, and zeroes the rest of the
    /// window behind it. Three halves of one invariant, none of them the caller's
    /// business — a caller that did any without the others is exactly the bug this
    /// function exists to make unwritable.
    ///
    /// * SPLICE: C allocates each array field ONCE, at `calloc(pcalc->nelm,
    ///   sizeof(double))` (`:695-698`, `:1084-1086`), and that buffer lives for the
    ///   record's lifetime — nothing ever replaces it, so a write is always INTO it
    ///   and never a swap of it.
    /// * BOUND: both writers ask for exactly `numElements` and can deliver no more.
    ///   The link reads `nRequest = acalcGetNumElements(pcalc)` elements
    ///   (`:1096-1097`), and `dbPut` clamps its request to `paddr->no_elements`
    ///   (`dbAccess.c:1361-1362`), which `cvt_dbaddr` set to the same
    ///   `acalcGetNumElements` (`:628`). A source LONGER than the window is
    ///   truncated at it — it cannot reach into `[numElements, nelm)`, which is the
    ///   region the splice exists to preserve.
    /// * ZERO-FILL: [`Self::zero_fill_window`], `[nNew, numElements)`.
    fn client_put_array(&mut self, src: &[f64], select: impl FnOnce(&mut Self) -> &mut Vec<f64>) {
        let nelm = (self.nelm as usize).max(1);
        let window = self.num_elements();
        let buf = select(self);
        buf.resize(nelm, 0.0);
        let n = src.len().min(window);
        buf[..n].copy_from_slice(&src[..n]);
        Self::zero_fill_window(buf, n, window);
    }

    fn build_inputs(&self, n: usize, prev_val: f64, prev_aval: &[f64]) -> ArrayInputs {
        let mut inputs = ArrayInputs::new(n);
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
    /// (`aCalcoutRecord.c:1289-1291`).
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
            _ => true,
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

static ACALCOUT_FIELDS: &[FieldDesc] = &[
    // result
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "AVAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "PVAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    // sizing — SPC_NOMOD NELM kept read_only, .db-settable via the loader path
    FieldDesc {
        name: "NELM",
        dbf_type: DbFieldType::ULong,
        read_only: true,
    },
    FieldDesc {
        name: "NUSE",
        dbf_type: DbFieldType::ULong,
        read_only: false,
    },
    // CALC / OCAL
    FieldDesc {
        name: "CALC",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CLCV",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "OCAL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OCLV",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "DOPT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "OVAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "OAV",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "POVL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    // scalar inputs A..L
    FieldDesc {
        name: "A",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "B",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "C",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "D",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "E",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "F",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "G",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "H",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "I",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "J",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "K",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "L",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    // array inputs AA..LL (DBF_NOACCESS double arrays via SPC_DBADDR)
    FieldDesc {
        name: "AA",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "BB",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "CC",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DD",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "EE",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "FF",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "GG",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "HH",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "II",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "JJ",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "KK",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    // input links (scalar + array)
    FieldDesc {
        name: "INPA",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPB",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPC",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPD",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPE",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPF",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPG",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPH",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPI",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPJ",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPK",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INAA",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INBB",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INCC",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INDD",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INEE",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INFF",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INGG",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INHH",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INII",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INJJ",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INKK",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INLL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUT",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    // link status (SPC_NOMOD, menu acalcoutINAV)
    FieldDesc {
        name: "INAV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "INBV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "INCV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "INDV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "INEV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "INFV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "INGV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "INHV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "INIV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "INJV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "INKV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "INLV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "IAAV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "IBBV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "ICCV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "IDDV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "IEEV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "IFFV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "IGGV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "IHHV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "IIIV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "IJJV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "IKKV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "ILLV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "OUTV",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    // output options
    FieldDesc {
        name: "OOPT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "ODLY",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "WAIT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DLYA",
        dbf_type: DbFieldType::UShort,
        read_only: true,
    },
    FieldDesc {
        name: "OEVT",
        dbf_type: DbFieldType::UShort,
        read_only: false,
    },
    FieldDesc {
        name: "IVOA",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IVOV",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    // display
    FieldDesc {
        name: "EGU",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "PREC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "HOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    // alarm limits
    FieldDesc {
        name: "HIHI",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOLO",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "HIGH",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOW",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "HHSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "LLSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "HSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "LSV",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "HYST",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "ADEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "MDEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    // previous scalar inputs PA..PL (SPC_NOMOD trackers)
    FieldDesc {
        name: "PA",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "PB",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "PC",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "PD",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "PE",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "PF",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "PG",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "PH",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "PI",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "PJ",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "PK",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "PL",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    // alarm trackers (SPC_NOMOD)
    FieldDesc {
        name: "LALM",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "ALST",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "MLST",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    // diagnostics
    FieldDesc {
        name: "NEWM",
        dbf_type: DbFieldType::ULong,
        read_only: true,
    },
    FieldDesc {
        name: "CACT",
        dbf_type: DbFieldType::Char,
        read_only: true,
    },
    FieldDesc {
        name: "CSTAT",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "AMASK",
        dbf_type: DbFieldType::ULong,
        read_only: true,
    },
    FieldDesc {
        name: "SIZE",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "AMEM",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "PMEM",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "VERS",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
];

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

/// `menu(acalcoutINAV)` — input-link PV status (`INAV..ILLV`, `OUTV`).
const ACALCOUT_INAV_CHOICES: &[&str] = &["Ext PV NC", "Ext PV OK", "Local PV", "Constant"];

impl Record for AcalcoutRecord {
    fn record_type(&self) -> &'static str {
        "acalcout"
    }

    /// C `acalcoutRecord.c::init_record` compiles CALC/OCAL into RPCL/ORPC and
    /// stores the postfix status in CLCV/OCLV (aCalcoutRecord.c:245-261) —
    /// the load-time half of the compile owner. A put goes through
    /// `special()` instead; `put_field` only stores the string, as C's dbPut
    /// does.
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            self.recompile_calc();
            self.recompile_ocal();
        }
        Ok(())
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
            _ => {}
        }
        Ok(())
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
            return Ok(ProcessOutcome::complete());
        }

        let n = self.num_elements();

        // C `process` line 374-377: clamp a stale NUSE > NELM.
        if self.nuse > self.nelm {
            self.nuse = self.nelm;
        }

        // C `process` line 393-395: snapshot scalar inputs (so PA..PL track
        // the values used in this calc). The framework fetches inputs before
        // `process()`, so these equal the current A..L. Runs BEFORE the fetch
        // gate below — C does it before calling `fetch_values`.
        self.pa = self.num_vals;

        // C `aCalcoutRecord.c::process` (399-414) wraps BOTH `doCalc` and
        // `afterCalc` in `if (fetch_values(pcalc)==0)`, and `fetch_values`
        // (1068-1071) returns at the first failing `dbGetLink`. So a failed
        // input link skips more here than in calc/calcout/sCalcout, whose gate
        // covers only the calc: `afterCalc` (aCalcoutRecord.c:281-345) is where
        // the CALC_ALARM/UDF update, `checkAlarms`, the OOPT decision, the
        // `pval = val` advance and the output all live, so NONE of them happen.
        // VAL/AVAL/OVAL/OAV and PVAL freeze, no OUT is written, and no limit
        // alarm is re-evaluated. Only C's tail — timestamp, monitors, forward
        // link — still runs, and the framework owns that.
        if self.fetch_gate_failed {
            self.cached_should_output = false;
            return Ok(ProcessOutcome::complete());
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
            // C `aCalcoutRecord.c:1289-1291` — the OCAL pass gets a LOCAL mask that is
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

        if calc_failed {
            // C afterCalc line 304-305: cstat != 0 → CALC_ALARM (raised in
            // `check_alarms`).
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
            let delay = std::time::Duration::from_secs_f64(self.odly);
            return Ok(ProcessOutcome {
                result: RecordProcessResult::AsyncPendingNotify(vec![(
                    "DLYA".to_string(),
                    EpicsValue::UShort(1),
                )]),
                actions: vec![ProcessAction::ReprocessAfter(delay)],
                device_did_compute: false,
            });
        }

        Ok(ProcessOutcome::complete())
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
            recgbl::rec_gbl_set_sevr_msg(
                common,
                alarm_status::CALC_ALARM,
                AlarmSeverity::Invalid,
                "CALC expression evaluation failed",
            );
        }

        // C checkAlarms line 845-852: the UDF guard returns before the limit
        // check. The framework set `common.udf` from `value_is_undefined()`
        // before this hook (a NaN VAL keeps UDF and raises UDF_ALARM).
        if common.udf {
            return;
        }

        let val = self.val;
        let hyst = self.hyst;
        let lalm = self.lalm;

        // Per-level hysteresis (C checkAlarms line 865-891). A zero severity
        // disables that level (C `if (hhsv && ...)`).
        if self.hhsv != 0 && (val >= self.hihi || (lalm == self.hihi && val >= self.hihi - hyst)) {
            recgbl::rec_gbl_set_sevr(
                common,
                alarm_status::HIHI_ALARM,
                AlarmSeverity::from_u16(self.hhsv as u16),
            );
            self.lalm = self.hihi;
        } else if self.llsv != 0
            && (val <= self.lolo || (lalm == self.lolo && val <= self.lolo + hyst))
        {
            recgbl::rec_gbl_set_sevr(
                common,
                alarm_status::LOLO_ALARM,
                AlarmSeverity::from_u16(self.llsv as u16),
            );
            self.lalm = self.lolo;
        } else if self.hsv != 0
            && (val >= self.high || (lalm == self.high && val >= self.high - hyst))
        {
            recgbl::rec_gbl_set_sevr(
                common,
                alarm_status::HIGH_ALARM,
                AlarmSeverity::from_u16(self.hsv as u16),
            );
            self.lalm = self.high;
        } else if self.lsv != 0 && (val <= self.low || (lalm == self.low && val <= self.low + hyst))
        {
            recgbl::rec_gbl_set_sevr(
                common,
                alarm_status::LOW_ALARM,
                AlarmSeverity::from_u16(self.lsv as u16),
            );
            self.lalm = self.low;
        } else {
            // C checkAlarms line 890: out of alarm by at least hyst.
            self.lalm = val;
        }
    }

    /// IVOA=SetIVOV severity hook. The framework calls this when SEVR is
    /// INVALID and IVOA=2. Overriding the trait default (which writes VAL via
    /// `set_val`) targets `OVAL` — C's `pcalc->oval = pcalc->ivov`, see
    /// `set_output_to_ivov` and the module doc.
    ///
    /// The ONLY record-side gate is the output decision: C reaches the IVOA
    /// switch through `execOutput` (aCalcoutRecord.c:933-937), and `execOutput`
    /// runs only when `afterCalc`'s OOPT decision says the record outputs
    /// (:338-352, and the DLYA continuation at :425-428). The severity test
    /// inside it is `if (pcalc->nsev < INVALID_ALARM)` (:915) — the RECORD's
    /// severity, from ANY source: a CALC failure, an MS input link, a limit
    /// alarm at INVALID severity, UDF at UDFS=INVALID. It was gated on
    /// `calc_alarm` as well, which silently narrowed C's rule to the calc
    /// failure alone — an acalcout driven INVALID by HIHI/HHSV or by an MS link
    /// then drove the computed value at IVOA=SetIVOV instead of IVOV. The
    /// framework already evaluated `nsev` (it is why this hook was called), so
    /// asking a private flag for the severity is both wrong and redundant.
    fn apply_invalid_output_value(&mut self, _ivov: EpicsValue) -> CaResult<()> {
        if self.cached_should_output {
            self.set_output_to_ivov();
        }
        Ok(())
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
            "AMEM" => Some(EpicsValue::Long(self.amem)),
            "PMEM" => Some(EpicsValue::Long(self.pmem)),
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
    /// DIFFERENT from the one the field held (`aCalcoutRecord.c:1088-1106`):
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
    /// tail zero-filled: [`AcalcoutRecord::array_field_value`] is exactly that
    /// view, so comparing it across the write is C's compare.
    ///
    /// This is the INTERNAL write path — the framework's input-link delivery
    /// (`multi_input_links`: INAA..INLL -> AA..LL). A client caput lands in
    /// `put_field` and does NOT set NEWM, which is C: only `fetch_values` sets it.
    fn put_field_internal(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        let Some(i) = Self::arr_index(name) else {
            return crate::server::record::put_field_internal_default(self, name, value);
        };
        let before = self.array_field_value(&self.arr_vals[i]);
        // The write itself — splice into the calloc(nelm) buffer, then zero the
        // rest of the window — is [`Self::client_put_array`], reached through
        // `put_field`. C's two writers do the same two things in the same order
        // (`fetch_values:1096-1102` and `put_array_info:726-731`); the only thing
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
                self.client_put_array(&src, |r| &mut r.aval);
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
            "NUSE" => {
                self.nuse = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("NUSE".into()))?
                    as u32;
                // C special(NUSE) line 494-501 clamps NUSE to NELM.
                if self.nuse > self.nelm {
                    self.nuse = self.nelm;
                }
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
                self.client_put_array(&src, |r| &mut r.oav);
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
            "OUTV" => match value {
                EpicsValue::Short(v) => {
                    self.outv = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OUTV".into())),
            },
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
                self.amem = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("AMEM".into()))?
                    as i32;
                Ok(())
            }
            "PMEM" => {
                self.pmem = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("PMEM".into()))?
                    as i32;
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
                    self.client_put_array(&src, |r| &mut r.arr_vals[idx]);
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
    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        &[
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
        ]
    }

    /// C `aCalcoutRecord.c::fetch_values` (1068-1071, 1097) `return`s at the
    /// first failing `dbGetLink` — in the scalar INPA..INPL loop and then in the
    /// array INAA..INLL loop, in exactly the order `multi_input_links` lists
    /// them — and `process` (399) gates `doCalc` + `afterCalc` on the status.
    fn input_fetch_policy(&self) -> InputFetchPolicy {
        InputFetchPolicy::AbortOnFirstFailure
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

    /// C `devaCalcoutSoft.c::write_acalcout` (84-87): when the effective
    /// element count resolves to 1, the write buffer is the scalar field
    /// — `&pcalc->val` under DOPT=Use CALC, `&pcalc->oval` under Use OCAL
    /// — not element 0 of the array. The OUT dispatch resolves the target
    /// count and makes the pick (`PvDatabase::multi_out_buffer_choice`).
    fn multi_output_scalar_companion(&self, link_field: &str) -> Option<&'static str> {
        if link_field == "OUT" {
            Some(if self.dopt == 1 { "OVAL" } else { "VAL" })
        } else {
            None
        }
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

    fn field_list(&self) -> &'static [FieldDesc] {
        ACALCOUT_FIELDS
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
    fn event_posted_fields(&self) -> &'static [&'static str] {
        &[
            "PVAL", "POVL", "LALM", "ALST", "MLST", "CSTAT", "PA", "PB", "PC", "PD", "PE", "PF",
            "PG", "PH", "PI", "PJ", "PK", "PL",
        ]
    }

    /// C posts an aCalcout array field from a per-cycle BIT MASK, and neither
    /// mask consults the value — a store that writes the value the field already
    /// held still posts.
    ///
    /// `afterCalc` (`aCalcoutRecord.c:293-297`), the AMASK half:
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
    /// previous copy of every SCALAR input (PA..PL, `aCalcoutRecord.c:1023-1029`)
    /// and of OVAL (POVL, `:1039`) and compares them — but it keeps NO previous
    /// copy of an array and compares no array anywhere. The only array
    /// comparison in the record is inside `fetch_values` (`:1103-1105`), against
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

    #[test]
    fn test_acalcout_nuse_clamped_to_nelm() {
        let mut rec = AcalcoutRecord::new();
        rec.put_field("NELM", EpicsValue::ULong(4)).unwrap();
        rec.put_field("NUSE", EpicsValue::ULong(10)).unwrap();
        assert_eq!(rec.get_field("NUSE"), Some(EpicsValue::ULong(4)));
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
