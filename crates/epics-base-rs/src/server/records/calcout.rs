use crate::server::record::{CycleCell, CycleRead, CycleValue};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use super::calc_compile;
use super::link_status::{
    LINK_CON, LINK_STATUS_CHOICES, LinkCheck, LinkCheckSpec, LinkCheckState, LinkRole, LinkRule,
    LinkStatusGen, LinkStatusPosts, post_link_status,
};
use crate::error::{CaError, CaResult};
use crate::server::database::AsyncDbHandle;
use crate::server::recgbl::PutReport;
use crate::server::record::{
    FieldMetadataOverride, InputFetchPolicy, ProcessAction, ProcessOutcome, Record,
    RecordProcessResult,
};
#[cfg(test)]
use crate::types::DbFieldType;
use crate::types::{EpicsValue, PvString};

/// Per-input link-status diagnostic field names (`INAV`..`INUV`), one per
/// calc input A..U, in C `menu(calcoutINAV)` order
/// (calcoutRecord.dbd.pod:865-1005). `OUTV` (the OUT-link status) is handled
/// separately because the OUT link is a common field, not a calcout field.
const CALCOUT_INAV_FIELDS: [&str; 21] = [
    "INAV", "INBV", "INCV", "INDV", "INEV", "INFV", "INGV", "INHV", "INIV", "INJV", "INKV", "INLV",
    "INMV", "INNV", "INOV", "INPV", "INQV", "INRV", "INSV", "INTV", "INUV",
];

/// The value field a link's `special()` posts, by link index (INPA..INPU, OUT):
/// C `pvalue = &prec->a + lnkIndex` (calcoutRecord.c:370), whose slot past `U`
/// is `OVAL`.
const CALCOUT_LINK_VALUE_FIELDS: [&str; 22] = [
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
    "T", "U", "OVAL",
];

/// C `checkLinks`' walk from `&prec->inpa`/`&prec->inav`: INPA..INPU, then
/// OUT (calcoutRecord.c:741-760).
static CALCOUT_LINK_CHECK: LinkCheckSpec = LinkCheckSpec {
    links: &[
        ("INPA", "INAV"),
        ("INPB", "INBV"),
        ("INPC", "INCV"),
        ("INPD", "INDV"),
        ("INPE", "INEV"),
        ("INPF", "INFV"),
        ("INPG", "INGV"),
        ("INPH", "INHV"),
        ("INPI", "INIV"),
        ("INPJ", "INJV"),
        ("INPK", "INKV"),
        ("INPL", "INLV"),
        ("INPM", "INMV"),
        ("INPN", "INNV"),
        ("INPO", "INOV"),
        ("INPP", "INPV"),
        ("INPQ", "INQV"),
        ("INPR", "INRV"),
        ("INPS", "INSV"),
        ("INPT", "INTV"),
        ("INPU", "INUV"),
        ("OUT", "OUTV"),
    ],
    rule: LinkRule::Volatile,
    // C `calcoutRecord.c:752`, `:757`.
    mask: crate::server::recgbl::EventMask::VALUE,
    repost_local: false,
    in_cycle: true,
};

/// Where CLCV's post sits in calcout's put-post table, after every link's.
const CLCV_POST: usize = 2 * CALCOUT_LINK_VALUE_FIELDS.len();

/// C `calcoutRecord.c:89` `int calcoutODLYprecision = 2;` — the precision
/// `get_precision` serves for `ODLY`, the output-delay field.
static CALCOUT_ODLY_PRECISION: AtomicI32 = AtomicI32::new(2);

/// The iocsh knob `calcoutODLYprecision`, read and written by `var`.
pub(crate) fn calcout_odly_precision() -> i32 {
    CALCOUT_ODLY_PRECISION.load(Ordering::Relaxed)
}

/// See [`calcout_odly_precision`].
pub(crate) fn set_calcout_odly_precision(value: i32) {
    CALCOUT_ODLY_PRECISION.store(value, Ordering::Relaxed);
}

/// C `calcoutRecord.c:91` `double calcoutODLYlimit = 100000;` — the control
/// upper `get_control_double` serves for `ODLY`, over a literal `0.0` lower.
static CALCOUT_ODLY_LIMIT: AtomicU64 = AtomicU64::new(100000f64.to_bits());

/// The iocsh knob `calcoutODLYlimit`, read and written by `var`.
pub(crate) fn calcout_odly_limit() -> f64 {
    f64::from_bits(CALCOUT_ODLY_LIMIT.load(Ordering::Relaxed))
}

/// See [`calcout_odly_limit`].
pub(crate) fn set_calcout_odly_limit(value: f64) {
    CALCOUT_ODLY_LIMIT.store(value.to_bits(), Ordering::Relaxed);
}

/// Calcout record — calc with output.
pub struct CalcoutRecord {
    pub val: f64,
    pub calc: String,
    pub oopt: i16, // Output Option: 0=Every, 1=OnChange, 2=WhenZero, 3=WhenNonzero, 4=TransZero, 5=TransNonzero
    cached_should_output: bool, // Cached result from process() for framework
    // C `calcoutRecord.c::execOutput:620-625`: on a DOPT=Use_OVAL output cycle,
    // a successful OCAL `calcPerform` sets `udf = isnan(oval)` (NOT VAL-based),
    // which raises UDF_ALARM and lets IVOA gate the OUT write. `Some(_)` carries
    // that per-cycle decision to `value_is_undefined()`; `None` (Use_VAL, an OCAL
    // calc error, or a non-output cycle) leaves udf VAL-based, matching C.
    ocal_udf_override: Option<bool>,
    pub dopt: i16, // Data Option: 0=Use CALC, 1=Use OCAL
    pub ocal: String,
    pub oval: f64,
    /// C `prec->povl`: the OVAL `monitor()` last compared and posted.
    povl: f64,
    pub ivoa: i16, // Invalid Output Action: 0=Continue, 1=Don't drive, 2=Set to IVOV
    pub ivov: f64,
    // Input links (INPA..INPU)
    pub inpa: PvString,
    pub inpb: PvString,
    pub inpc: PvString,
    pub inpd: PvString,
    pub inpe: PvString,
    pub inpf: PvString,
    pub inpg: PvString,
    pub inph: PvString,
    pub inpi: PvString,
    pub inpj: PvString,
    pub inpk: PvString,
    pub inpl: PvString,
    pub inpm: PvString,
    pub inpn: PvString,
    pub inpo: PvString,
    pub inpp: PvString,
    pub inpq: PvString,
    pub inpr: PvString,
    pub inps: PvString,
    pub inpt: PvString,
    pub inpu: PvString,
    // Input values (A..U)
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
    pub g: f64,
    pub h: f64,
    pub i: f64,
    pub j: f64,
    pub k: f64,
    pub l: f64,
    pub m: f64,
    pub n: f64,
    pub o: f64,
    pub p: f64,
    pub q: f64,
    pub r: f64,
    pub s: f64,
    pub t: f64,
    pub u: f64,
    // The inputs this cycle's `advance_prev_all` found apart from LA..LU,
    // bit i for input i; zero on a cycle that ran no advance (the ODLY
    // delaying cycle, which C leaves before `monitor()`).
    input_changes: u32,
    // Previous values LA..LU
    pub la: f64,
    pub lb: f64,
    pub lc: f64,
    pub ld: f64,
    pub le: f64,
    pub lf: f64,
    pub lg: f64,
    pub lh: f64,
    pub li: f64,
    pub lj: f64,
    pub lk: f64,
    pub ll: f64,
    pub lm: f64,
    pub ln: f64,
    pub lo: f64,
    pub lp: f64,
    pub lq: f64,
    pub lr: f64,
    pub ls: f64,
    pub lt: f64,
    pub lu: f64,
    // Display/engineering
    pub egu: PvString,
    pub prec: i16,
    pub hopr: f64,
    pub lopr: f64,
    // Monitor deadband
    pub adel: f64,
    pub mdel: f64,
    pub lalm: f64,
    pub alst: f64,
    pub mlst: f64,
    // Previous values for output determination
    pub pval: f64, // previous VAL (externally readable like C)
    // Output delay (ODLY) — C `calcoutRecord.c` `prec->odly`. When an
    // output should fire and ODLY > 0, the OUT-link write is deferred
    // by ODLY seconds via a delayed re-process.
    pub odly: f64,
    // Delay-active flag (DLYA) — C `prec->dlya`. Set to 1 while an ODLY
    // delay is pending, cleared on the delayed re-process. Externally
    // readable (DBF_SHORT) so clients can observe the pending state.
    pub dlya: u16,
    /// This cycle set or cleared DLYA, which C posts from `process()` with a
    /// literal `DBE_VALUE` (`calcoutRecord.c:279`, `:296`).
    dlya_moved: bool,
    // OEVT ("Event To Issue") — C `calcoutRecord.c` `prec->oevt` (DBF_STRING).
    // When output fires and OEVT names a non-empty event, `execOutput` posts
    // it (`postEvent(eventNameToHandle(oevt))`) right after `writeValue`; see
    // [`Record::output_event`].
    pub oevt: PvString,
    // Internal: captured output decision while an ODLY delay is pending.
    // The delayed re-process must write the output that the *original*
    // cycle decided on, not re-evaluate should_output() against the
    // (by then stale) pval/val.
    pending_output: bool,
    /// `prec->pact` at `process()` entry: C takes the delayed-output arm only
    /// with PACT held, so a re-entry after a device-link put cleared it
    /// (`dbAccess.c:1194`) is a fresh cycle.
    pact: bool,
    // This cycle's `calcPerform` outcome (C `calcoutRecord.c:238-241` for CALC,
    // `:622` for OCAL). A per-cycle fact, not record state: `check_alarms` — the
    // owner of this record's alarm transitions — consumes it, so it cannot
    // outlive the cycle that set it. `Some(msg)` also carries WHICH failure C's
    // amsg names: `"calcPerform"` (CALC, `:239`) or `"OCAL calcPerform"`
    // (OCAL, `:622`). C raises CALC before OCAL and `recGblSetSevrMsg` is
    // raise-only, so when both fail in one cycle the CALC message wins — which
    // `get_or_insert` at the two set sites reproduces (CALC runs first).
    calc_alarm: Option<&'static str>,
    // This cycle's `fetch_values()` outcome, pushed by the framework through
    // `set_fetch_gate_failed`. C `calcoutRecord.c::process` (237) runs
    // `calcPerform` only `if (fetch_values(prec) == 0)`: a failed input link
    // freezes VAL/UDF and raises no CALC_ALARM, while the OOPT switch, the
    // output and the monitors still run against the frozen VAL.
    fetch_gate_failed: bool,
    // This cycle reached one of C's two `prec->udf` writes: the CALC pass's
    // `else prec->udf = isnan(prec->val)` (`calcoutRecord.c:241`) or
    // `execOutput`'s `else prec->udf = isnan(prec->oval)` (`:624`). Both sit on
    // a `calcPerform` SUCCESS arm, the CALC one additionally inside the
    // `fetch_values` gate. Consumed by `check_alarms`, which owns the write;
    // a cycle that sets neither leaves UDF frozen, as C does.
    value_computed: bool,
    // Cached compiled expressions (RPCL/ORPC equivalents)
    // C `RPCL` / `ORPC`. Always a program: an empty or uncompilable CALC/OCAL
    // carries C's empty `END_EXPRESSION` postfix, which `calcPerform` refuses to
    // run, so the record alarms on every process. See [`calc_compile`].
    rpcl: crate::calc::Rpcl<CALCOUT_RPCL_SIZE>,
    orpc: crate::calc::Rpcl<CALCOUT_RPCL_SIZE>,
    // CLCV/OCLV — `DBF_LONG` expression-validity fields
    // (calcoutRecord.dbd.pod:729,1049). C stores `postfix()`'s RETURN VALUE
    // here (`prec->clcv = postfix(...)`, calcoutRecord.c:327,338), i.e. 0 when
    // the expression compiled and -1 when it did not — not the CALC_ERR_* code,
    // which only reaches the errlog line.
    pub clcv: i32,
    pub oclv: i32,
    // Per-input link connection status INAV..INUV and the OUT-link status
    // OUTV, menu(calcoutINAV), in `CALCOUT_LINK_CHECK` order. C
    // `calcoutRecord.c::init_record` (calcoutRecord.c:160-189) classifies
    // each INPA..INPU input link and the OUT link into these; `checkLinks`
    // moves them as their CA links connect.
    link_status: [i16; 22],
    link_check: LinkCheckState,
    // The common OUT-link text as `init_links` saw it at load, for the init
    // classification; a runtime OUT put classifies nothing.
    out: Box<[u8]>,
    // The posts the last `special()` owes, a bit per put-post table entry:
    // one put's worth, so a mark no put path drains is overwritten, never
    // queued.
    put_posts: u64,
    // Async surface for posting the live INAV..INUV/OUTV diagnostics
    // (C `checkLinks`), wired by `set_async_context`.
    async_ctx: Option<(crate::types::RecordName, AsyncDbHandle)>,
    // A refused CALC/OCAL put's report, made by `special()` under the put's
    // hold and printed by the put's owner after it.
    put_report: Option<PutReport>,
    // Monotonic generation guarding the link-status refresh. Each refresh
    // classifies a *snapshot* of the link strings off-thread; a later
    // refresh (an INP/OUT re-point) must win over an earlier one regardless
    // of which spawned task finishes first. The shared `LinkStatusGen` gate
    // enforces the invariant — only the latest classification may be
    // published. See `link_status::LinkStatusGen`.
    link_gen: LinkStatusGen,
}

impl Default for CalcoutRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            calc: String::new(),
            oopt: 0,
            cached_should_output: false,
            ocal_udf_override: None,
            dopt: 0,
            ocal: String::new(),
            oval: 0.0,
            povl: 0.0,
            ivoa: 0,
            ivov: 0.0,
            inpa: PvString::new(),
            inpb: PvString::new(),
            inpc: PvString::new(),
            inpd: PvString::new(),
            inpe: PvString::new(),
            inpf: PvString::new(),
            inpg: PvString::new(),
            inph: PvString::new(),
            inpi: PvString::new(),
            inpj: PvString::new(),
            inpk: PvString::new(),
            inpl: PvString::new(),
            inpm: PvString::new(),
            inpn: PvString::new(),
            inpo: PvString::new(),
            inpp: PvString::new(),
            inpq: PvString::new(),
            inpr: PvString::new(),
            inps: PvString::new(),
            inpt: PvString::new(),
            inpu: PvString::new(),
            a: 0.0,
            b: 0.0,
            c: 0.0,
            d: 0.0,
            e: 0.0,
            f: 0.0,
            g: 0.0,
            h: 0.0,
            i: 0.0,
            j: 0.0,
            k: 0.0,
            l: 0.0,
            m: 0.0,
            n: 0.0,
            o: 0.0,
            p: 0.0,
            q: 0.0,
            r: 0.0,
            s: 0.0,
            t: 0.0,
            u: 0.0,
            input_changes: 0,
            la: 0.0,
            lb: 0.0,
            lc: 0.0,
            ld: 0.0,
            le: 0.0,
            lf: 0.0,
            lg: 0.0,
            lh: 0.0,
            li: 0.0,
            lj: 0.0,
            lk: 0.0,
            ll: 0.0,
            lm: 0.0,
            ln: 0.0,
            lo: 0.0,
            lp: 0.0,
            lq: 0.0,
            lr: 0.0,
            ls: 0.0,
            lt: 0.0,
            lu: 0.0,
            egu: PvString::new(),
            prec: 0,
            hopr: 0.0,
            lopr: 0.0,
            adel: 0.0,
            mdel: 0.0,
            lalm: 0.0,
            alst: 0.0,
            mlst: 0.0,
            pval: 0.0,
            odly: 0.0,
            dlya: 0,
            dlya_moved: false,
            oevt: PvString::new(),
            pending_output: false,
            pact: false,
            calc_alarm: None,
            fetch_gate_failed: false,
            value_computed: false,
            rpcl: crate::calc::Rpcl::empty(),
            orpc: crate::calc::Rpcl::empty(),
            clcv: 0,
            oclv: 0,
            // C `init_record` leaves an empty/unconfigured link CON
            // (calcoutRecord.c:166-167); the refresh re-classifies once the
            // async context exists.
            link_status: [LINK_CON; 22],
            link_check: LinkCheckState::default(),
            out: Box::default(),
            put_posts: 0,
            async_ctx: None,
            put_report: None,
            link_gen: LinkStatusGen::default(),
        }
    }
}

/// `calcoutRecord.dbd` `rpcl`/`orpc[INFIX_TO_POSTFIX_SIZE(160)]`.
const CALCOUT_RPCL_SIZE: usize = crate::calc::infix_to_postfix_size(160);

impl CalcoutRecord {
    /// C's CALC compile, the same in `init_record` (calcoutRecord.c:191-197)
    /// and `special()` (`:325-333`): a refused `postfix()` writes
    /// `recGblRecordError(S_db_badField, prec, pmessage)` then the
    /// `calcErrorStr` line, both naming the record. Returned, not printed:
    /// `special()` runs under the put's hold.
    fn recompile_calc(&mut self, pmessage: &'static str) -> Option<PutReport> {
        let compiled = calc_compile::postfix_into(&self.calc, &mut self.rpcl);
        let report = compiled.put_report_in_expression(pmessage, "CALC", &self.calc);
        self.clcv = compiled.status;
        report
    }

    /// The OCAL half (`:199-205`, `:336-342`): reported only under DOPT
    /// "Use OCAL"; OCLV is -1 either way.
    fn recompile_ocal(&mut self, pmessage: &'static str) -> Option<PutReport> {
        let compiled = calc_compile::postfix_into(&self.ocal, &mut self.orpc);
        let report = compiled
            .put_report_in_expression(pmessage, "OCAL", &self.ocal)
            .filter(|_| self.dopt == 1);
        self.oclv = compiled.status;
        report
    }

    /// `init_record` is not under a put's hold, so it prints at once.
    fn init_report(&self, report: Option<PutReport>) {
        if let Some(report) = report {
            let name = self
                .async_ctx
                .as_ref()
                .map_or("Unknown".into(), |(n, _)| n.to_str_lossy());
            report.emit(&name, "");
        }
    }

    /// The 21 `LX` advances as one step. C reaches them in `monitor`
    /// (`calcoutRecord.c:679-685`), which `process` calls at `:306` — below
    /// the ODLY early return at `:282`, so both the immediate cycle and the
    /// delayed continuation run them and the delaying cycle does not.
    fn advance_prev_all(&mut self) {
        let prev = [
            &mut self.la,
            &mut self.lb,
            &mut self.lc,
            &mut self.ld,
            &mut self.le,
            &mut self.lf,
            &mut self.lg,
            &mut self.lh,
            &mut self.li,
            &mut self.lj,
            &mut self.lk,
            &mut self.ll,
            &mut self.lm,
            &mut self.ln,
            &mut self.lo,
            &mut self.lp,
            &mut self.lq,
            &mut self.lr,
            &mut self.ls,
            &mut self.lt,
            &mut self.lu,
        ];
        let new = [
            self.a, self.b, self.c, self.d, self.e, self.f, self.g, self.h, self.i, self.j, self.k,
            self.l, self.m, self.n, self.o, self.p, self.q, self.r, self.s, self.t, self.u,
        ];
        let mut changes = 0;
        for (i, (new, prev)) in new.into_iter().zip(prev).enumerate() {
            if new != *prev {
                *prev = new;
                changes |= 1 << i;
            }
        }
        self.input_changes = changes;
    }

    /// C `calcoutRecord.c::execOutput` (613-627), the "Determine output data"
    /// half: the DOPT switch that fills OVAL and the `udf = isnan(oval)` it
    /// leaves behind on the Use_OVAL branch.
    ///
    /// The single owner of that switch, called from the two places C calls
    /// `execOutput`: the immediate output at `:283` and the delayed
    /// continuation at `:296`. C's ODLY arm returns at `:282` with the switch
    /// UNRUN, so on a delayed cycle OCAL runs against the A..U present at
    /// EXPIRY. Evaluating it while scheduling made the delay a no-op for every
    /// input that moved inside the window.
    ///
    /// `calcPerform(&prec->a, &prec->oval, prec->orpc)` (`:621`) takes the
    /// record's own A..U as its arg set, so this pass reads back the CALC
    /// pass's stores and writes its own into the same cells.
    fn exec_output_data(&mut self) {
        if self.dopt != 1 {
            self.oval = self.val;
            return;
        }
        // Use OCAL. C `:621` calls calcPerform on ORPC unconditionally on this
        // branch — an empty OCAL with DOPT=Use_OCAL is the empty program, so it
        // fails and raises CALC_ALARM instead of leaving OVAL stale and silent.
        // `presult = &oval`, so the OCAL `VAL` token reads the *previous* OVAL,
        // not VAL.
        let mut inputs = crate::calc::NumericInputs::with_vars(self.get_vars());
        inputs.prev_val = self.oval;
        match crate::calc::eval(&self.orpc, &mut inputs) {
            Ok(v) => {
                self.oval = v;
                // C `:624`: `prec->udf = isnan(prec->oval)` on the
                // successful-OCAL branch. A NaN OVAL then raises UDF_ALARM
                // (`:628`) so IVOA gates the OUT write — without this a finite
                // VAL but NaN OVAL drives NaN to OUT with NO_ALARM.
                self.ocal_udf_override = Some(self.oval.is_nan());
                self.value_computed = true;
            }
            // C `:622`: OCAL calcPerform failure raises CALC_ALARM (amsg "OCAL
            // calcPerform") and leaves udf VAL-based (no override).
            // `get_or_insert` keeps a prior CALC "calcPerform" if CALC also
            // failed this cycle — matching C's raise-only order.
            Err(_) => {
                self.calc_alarm.get_or_insert("OCAL calcPerform");
            }
        }
        self.apply_stores(&inputs.vars);
    }

    fn get_vars(&self) -> [f64; 21] {
        [
            self.a, self.b, self.c, self.d, self.e, self.f, self.g, self.h, self.i, self.j, self.k,
            self.l, self.m, self.n, self.o, self.p, self.q, self.r, self.s, self.t, self.u,
        ]
    }

    /// Land the cycle's variable stores back in A..U — the inverse of
    /// [`Self::get_vars`], and the record's ONLY write-back of an engine var set.
    ///
    /// C hands `calcPerform` a pointer into the record (`&prec->a`), so a store
    /// opcode (`calcPerform.c:101-123`) IS the field write. Both of the cycle's
    /// passes are handed that SAME pointer — `calcPerform(&prec->a, &prec->val,
    /// rpcl)` (`calcoutRecord.c:238`) and `calcPerform(&prec->a, &prec->oval,
    /// orpc)` (`:621`) — so OCAL reads what CALC stored, and the two share one
    /// var set here for the same reason.
    fn apply_stores(&mut self, vars: &[f64; 21]) {
        [
            self.a, self.b, self.c, self.d, self.e, self.f, self.g, self.h, self.i, self.j, self.k,
            self.l, self.m, self.n, self.o, self.p, self.q, self.r, self.s, self.t, self.u,
        ] = *vars;
    }

    /// C `calcoutRecord.c:257`:
    ///
    /// ```c
    /// doOutput = ! (fabs(prec->pval - prec->val) <= prec->mdel);
    /// ```
    ///
    /// The negated `<=` is not a spelling of `>`: every IEEE comparison
    /// involving NaN is false, so a NaN VAL (`CALC="0/0"`, or any input that
    /// went undefined) makes C's `<=` false and `doOutput` TRUE, while a `>`
    /// makes it FALSE. This record was written to agree with `scalcout` /
    /// `acalcout`, whose own upstreams (`sCalcoutRecord.c:379`,
    /// `aCalcoutRecord.c:318`) genuinely DO use `>`; that is why the `>` looked
    /// right. It cost the OUT link on every NaN cycle, and with it the target's
    /// FLNK chain.
    ///
    /// Spelled as the three-way compare rather than as C's negation: `None` is
    /// the NaN arm C reaches by falling through a false `<=`, and naming it
    /// stops the next reader from "simplifying" this back to `>`. `Less` and
    /// `Equal` are both inside the deadband, which is what makes C's `<=`
    /// inclusive — an exact-MDEL change does not fire.
    fn should_output(&self) -> bool {
        match self.oopt {
            0 => true, // Every Time
            // On Change
            1 => matches!(
                (self.pval - self.val).abs().partial_cmp(&self.mdel),
                None | Some(std::cmp::Ordering::Greater)
            ),
            2 => self.val == 0.0,                     // When Zero
            3 => self.val != 0.0,                     // When Non-zero
            4 => self.pval != 0.0 && self.val == 0.0, // Transition to Zero
            5 => self.pval == 0.0 && self.val != 0.0, // Transition to Non-zero
            _ => false, // Unknown: C's `doOutput = 0` default (`:270-272`)
        }
    }

    /// The 21 input link strings (INPA..INPU) in input order A..U.
    fn input_links(&self) -> [PvString; 21] {
        [
            self.inpa.clone(),
            self.inpb.clone(),
            self.inpc.clone(),
            self.inpd.clone(),
            self.inpe.clone(),
            self.inpf.clone(),
            self.inpg.clone(),
            self.inph.clone(),
            self.inpi.clone(),
            self.inpj.clone(),
            self.inpk.clone(),
            self.inpl.clone(),
            self.inpm.clone(),
            self.inpn.clone(),
            self.inpo.clone(),
            self.inpp.clone(),
            self.inpq.clone(),
            self.inpr.clone(),
            self.inps.clone(),
            self.inpt.clone(),
            self.inpu.clone(),
        ]
    }

    /// Map an `INAV`..`INUV` field name to the input index 0..21 (A..U), or
    /// `None` for any other name (including `OUTV`, which the caller handles
    /// separately). The status fields are `IN<letter>V`, distinct from the
    /// `INP<letter>` link fields (which have no trailing `V`).
    fn input_status_index(name: &str) -> Option<usize> {
        let mid = name.strip_prefix("IN")?.strip_suffix('V')?;
        let [c] = mid.as_bytes() else { return None };
        match c {
            b'A'..=b'U' => Some((c - b'A') as usize),
            _ => None,
        }
    }

    /// The `link_status` slot of `INAV`..`INUV` or `OUTV`.
    fn status_index(name: &str) -> Option<usize> {
        match name {
            "OUTV" => Some(21),
            _ => Self::input_status_index(name),
        }
    }

    /// C's `lnkIndex = fieldIndex - calcoutRecordINPA` (calcoutRecord.c:368)
    /// for the 22 link fields `special()` handles: INPA..INPU are 0..=20 and
    /// OUT, which follows INPU in the record, is 21.
    fn link_index(name: &str) -> Option<usize> {
        if name == "OUT" {
            return Some(21);
        }
        match name.strip_prefix("INP")?.as_bytes() {
            [c @ b'A'..=b'U'] => Some((c - b'A') as usize),
            _ => None,
        }
    }

    /// Classify every INP A..U link and the OUT link into their
    /// `menu(calcoutINAV)` connection status and post the live
    /// `INAV`..`INUV`/`OUTV` diagnostics, mirroring C
    /// `calcoutRecord.c::init_record` (calcoutRecord.c:160-189) and the
    /// `checkLinksCallback` re-poll. Runs at record init only
    /// (`set_async_context`, `init_links`): a runtime link put is `special()`'s,
    /// which C answers without classifying. No-op without an async context.
    fn refresh_link_status(&self) {
        let mut links: Vec<(&'static str, PvString, LinkRole)> = self
            .input_links()
            .into_iter()
            .enumerate()
            .map(|(i, link)| (CALCOUT_INAV_FIELDS[i], link, LinkRole::Input))
            .collect();
        links.push(("OUTV", PvString::from(&*self.out), LinkRole::Output));
        // C `calcoutRecord.c:404`, `:752`, `:757`.
        post_link_status(
            self.async_ctx.as_ref(),
            &self.link_gen,
            links,
            LinkRule::Volatile,
            CALCOUT_LINK_CHECK.mask,
            LinkStatusPosts::All,
        );
    }
}

/// Choice labels for the output-execute-option menu, in index order.
/// C `menu(calcoutOOPT)` (`calcoutRecord.dbd.pod:33-39`).
const CALCOUT_OOPT_CHOICES: &[&str] = &[
    "Every Time",
    "On Change",
    "When Zero",
    "When Non-zero",
    "Transition To Zero",
    "Transition To Non-zero",
];

/// Choice labels for the output-data-option menu, in index order.
/// C `menu(calcoutDOPT)` (`calcoutRecord.dbd.pod:41-43`).
const CALCOUT_DOPT_CHOICES: &[&str] = &["Use CALC", "Use OCAL"];

/// VAL, A..U and OVAL: what `calcoutRecord.c::monitor` posts.
const CALCOUT_PROCESS_POSTED: [&str; 23] = [
    "VAL", "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q",
    "R", "S", "T", "U", "OVAL",
];

impl Record for CalcoutRecord {
    // The menus are `i16` served as ENUM, so they map no slot.
    crate::slot_map!(crate::server::record::dbd_generated::slots::calcout;
        VAL => val,
        PVAL => pval,
        CLCV => clcv,
        ODLY => odly,
        OCLV => oclv,
        IVOV => ivov,
        EGU => egu,
        PREC => prec,
        HOPR => hopr,
        LOPR => lopr,
        ADEL => adel,
        MDEL => mdel,
        A => a,
        B => b,
        C => c,
        D => d,
        E => e,
        F => f,
        G => g,
        H => h,
        I => i,
        J => j,
        K => k,
        L => l,
        M => m,
        N => n,
        O => o,
        P => p,
        Q => q,
        R => r,
        S => s,
        T => t,
        U => u,
        OVAL => oval;
        read_only:
        LA => la,
        LB => lb,
        LC => lc,
        LD => ld,
        LE => le,
        LF => lf,
        LG => lg,
        LH => lh,
        LI => li,
        LJ => lj,
        LK => lk,
        LL => ll,
        LM => lm,
        LN => ln,
        LO => lo,
        LP => lp,
        LQ => lq,
        LR => lr,
        LS => ls,
        LT => lt,
        LU => lu,
        LALM => lalm,
        ALST => alst,
        MLST => mlst,
        DLYA => dlya);

    fn stamp_point(&self) -> crate::server::record::StampPoint {
        crate::server::record::StampPoint::AfterAlarms
    }

    /// C posts these only from `checkLinks` (`calcoutRecord.c:404`, `:752`,
    /// `:757`), which the port's link-status refresh owns.
    fn event_posted_fields(&self) -> &'static [&'static str] {
        &[
            "INAV", "INBV", "INCV", "INDV", "INEV", "INFV", "INGV", "INHV", "INIV", "INJV", "INKV",
            "INLV", "INMV", "INNV", "INOV", "INPV", "INQV", "INRV", "INSV", "INTV", "INUV", "OUTV",
        ]
    }

    /// What `put_field` stores for `A`..`U`, without the by-name lookups
    /// that precede it on the default path.
    fn put_multi_input_f64(&mut self, field: &'static str, value: f64) -> CaResult<()> {
        let var = match field.as_bytes() {
            b"A" => &mut self.a,
            b"B" => &mut self.b,
            b"C" => &mut self.c,
            b"D" => &mut self.d,
            b"E" => &mut self.e,
            b"F" => &mut self.f,
            b"G" => &mut self.g,
            b"H" => &mut self.h,
            b"I" => &mut self.i,
            b"J" => &mut self.j,
            b"K" => &mut self.k,
            b"L" => &mut self.l,
            b"M" => &mut self.m,
            b"N" => &mut self.n,
            b"O" => &mut self.o,
            b"P" => &mut self.p,
            b"Q" => &mut self.q,
            b"R" => &mut self.r,
            b"S" => &mut self.s,
            b"T" => &mut self.t,
            b"U" => &mut self.u,
            _ => return self.put_field_internal(field, EpicsValue::Double(value)),
        };
        *var = value;
        Ok(())
    }

    /// C `monitor()` posts every input A..U when the alarm moved, changed or
    /// not: `if (*pnew != *pprev || monitor_mask & DBE_ALARM)` with
    /// `monitor_mask | DBE_VALUE | DBE_LOG` (calcoutRecord.c:681-682).
    fn alarm_cycle_monitored_fields(&self) -> crate::server::record::DeclaredFields {
        &const {
            crate::server::record::declared_set(
                crate::server::record::dbd_generated::CALCOUT_FIELDS,
                crate::server::records::calc::CALC_INPUT_FIELDS,
            )
        }
    }

    fn prev_compared_inputs(&self) -> (&'static [&'static str], u32) {
        (
            &crate::server::records::calc::CALC_INPUT_FIELDS,
            self.input_changes,
        )
    }

    fn alarm_cycle_posts_value_log(&self) -> bool {
        true
    }

    fn acts_before_process(&self) -> bool {
        false
    }

    fn cycle_hooks(&self) -> crate::server::record::CycleHooks {
        crate::server::record::CycleHooks::WRITE_POSTED
            .union(crate::server::record::CycleHooks::PUT_REPORT)
            .union(crate::server::record::CycleHooks::PROCESS_POSTED)
            .union(crate::server::record::CycleHooks::INPUT_FETCH)
            .union(crate::server::record::CycleHooks::CONTINUATION)
            .union(crate::server::record::CycleHooks::MONITOR_SETS)
            .union(crate::server::record::CycleHooks::OUTPUT_HOOKS)
            .union(crate::server::record::CycleHooks::LINK_CHECK)
    }

    fn link_check(&mut self) -> Option<LinkCheck<'_>> {
        Some(LinkCheck {
            spec: &CALCOUT_LINK_CHECK,
            statuses: &mut self.link_status,
            field_types: &mut [],
            tail: None,
            state: &mut self.link_check,
        })
    }

    fn take_put_report(&mut self) -> Option<PutReport> {
        self.put_report.take()
    }

    fn has_array_monitor_post(&self) -> bool {
        false
    }

    /// `calcoutRecord.c:124-126` discards the dset status and seeds.
    fn seed_deadband_tracking(&mut self, _dset_init: crate::server::device_support::DsetInit) {
        crate::server::record::seed_trackers_from_val(self);
        self.povl = self.oval;
    }

    /// `monitor()` posts VAL, A..U and OVAL alone (`calcoutRecord.c:660-691`);
    /// DLYA posts from `process()` through [`Self::take_process_posted_fields`].
    fn process_posted_fields(&self) -> Option<crate::server::record::DeclaredFields> {
        Some(
            &const {
                crate::server::record::declared_set(
                    crate::server::record::dbd_generated::CALCOUT_FIELDS,
                    CALCOUT_PROCESS_POSTED,
                )
            },
        )
    }

    /// C `calcoutRecord.c:687-690` posts OVAL with `monitor_mask | DBE_VALUE |
    /// DBE_LOG` outside the `if (monitor_mask)` that gates VAL.
    fn fields_posted_with_value_mask(
        &self,
    ) -> &'static [(&'static str, crate::server::record::ValuePostGate)] {
        &[("OVAL", crate::server::record::ValuePostGate::OnChangeForced)]
    }

    /// C `if (prec->povl != prec->oval) { ...; prec->povl = prec->oval; }`
    /// (`calcoutRecord.c:687-690`), minus the post. Outside init, `monitor()`
    /// is the only writer of `povl`, so an OVAL put posts again from its cycle
    /// unless the cycle sets back the value `povl` holds.
    fn take_secondary_value_changes(
        &mut self,
        _value_mask: crate::server::recgbl::EventMask,
    ) -> crate::server::record::SecondaryTake {
        crate::server::record::SecondaryTake::unguarded(u64::from(
            crate::server::record::SecondaryTake::advance(&mut self.povl, self.oval),
        ))
    }

    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &["OVAL"]
    }

    fn take_process_posted_fields(&mut self) -> crate::server::record::CycleMarks {
        use crate::server::record::{CycleMarks, CyclePostMask};
        const DLYA_POST_NAMED: [(&str, CyclePostMask); 1] = [("DLYA", CyclePostMask::Value)];
        static DLYA_POST: [(
            crate::server::record::FieldIx,
            crate::server::record::CyclePostMask,
        ); 1] = crate::server::record::declared_post_table(
            crate::server::record::dbd_generated::CALCOUT_FIELDS,
            DLYA_POST_NAMED,
        );
        CycleMarks::of(&DLYA_POST, u128::from(std::mem::take(&mut self.dlya_moved)))
    }

    fn record_type(&self) -> &'static str {
        "calcout"
    }

    /// `calcoutRecord.c:417-423` `get_linkNumber` — identical to `calc`'s.
    fn link_backed_metadata_field(&self, field: &str) -> Option<String> {
        crate::server::record::calc_class_link_backed_metadata_field(field)
    }

    /// `ODLY` is the one field in base whose graphic case is neither the
    /// record's limits, nor a link, nor a plain `default:` arm
    /// (`calcoutRecord.c:490-493`):
    ///
    /// ```c
    /// case indexof(ODLY):
    ///     recGblGetGraphicDouble(paddr,pgd);
    ///     pgd->lower_disp_limit = 0.0;
    /// ```
    ///
    /// It calls recGbl for the type range and then OVERWRITES the lower with a
    /// literal 0 — a delay cannot be negative. So `ODLY` serves the DBF_DOUBLE
    /// upper of 1e300 with a lower of 0, which no single routed arm produces.
    /// `get_units` (`:425-444`) and `get_precision` (`:446-465`) each test
    /// `ODLY` FIRST and return early with a literal — `"s"` and
    /// `calcoutODLYprecision` (`= 2`, `:89`) — so ODLY never reaches the EGU /
    /// PREC arms those functions serve to every other DBF_DOUBLE field. The
    /// early return is what makes this an override rather than a default: the
    /// record's own EGU/PREC would otherwise win.
    ///
    /// `get_control_double` (`:506-530`) gives ODLY its own `case` alongside
    /// the eight VAL-class fields, and answers it `0.0 .. calcoutODLYlimit`
    /// (`= 100000`, `:91`) rather than the HOPR/LOPR those eight take — so the
    /// control slot's answer is a literal here too, and a different one from
    /// the graphic slot's `0 .. 1e300` two fields up.
    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        field
            .eq_ignore_ascii_case("ODLY")
            .then(|| FieldMetadataOverride {
                units: Some("s".into()),
                precision: Some(calcout_odly_precision() as i16),
                disp_limits: Some((1e300, 0.0)),
                ctrl_limits: Some((calcout_odly_limit(), 0.0)),
                ..Default::default()
            })
    }

    // C raises UDF_ALARM from BOTH `checkAlarms` and `execOutput`, and
    // `recGblSetSevr` only raises (never lowers), so the effective UDF
    // condition is the OR of the two:
    //   * `checkAlarms` (`calcoutRecord.c:244`, BEFORE the output switch) sees
    //     `udf = isnan(VAL)` (set at line 241) and raises UDF_ALARM if VAL is
    //     NaN — independent of OVAL.
    //   * `execOutput` (`calcoutRecord.c:620-630`, Use_OVAL output cycle) then
    //     sets `udf = isnan(OVAL)` and raises UDF_ALARM if OVAL is NaN.
    // So a NaN VAL keeps the record INVALID even when OCAL yields a finite OVAL
    // (the `checkAlarms` raise stands). `ocal_udf_override` carries the
    // execOutput half — `Some(true)` when a Use_OVAL output cycle produced a
    // NaN OVAL — and is OR'd with the VAL-NaN half. `None` (Use_VAL / OCAL calc
    // error / non-output cycle) leaves udf purely VAL-based, matching the trait
    // default. (Residual: C's udf *field* ends at `isnan(OVAL)` on a Use_OVAL
    // output cycle, so for NaN VAL + finite OVAL C reports UDF field 0 with
    // SEVR INVALID; Rust's single value_is_undefined() reports the field as 1.
    // The SEVR/STAT — the parity-critical observables — match.)
    fn value_is_undefined(&self) -> bool {
        self.val.is_nan() || matches!(self.ocal_udf_override, Some(true))
    }

    // C recCalcout.c IVOA=set_to_IVOV: oval = ivov; the OUT writeback
    // then sends OVAL. VAL is the calc *result* and remains intact.
    //
    // The `oval = ivov` substitution lives inside `execOutput`
    // (calcoutRecord.c:646), which `process` calls ONLY under the
    // `if (doOutput)` gate (calcoutRecord.c:276). So a non-output INVALID
    // cycle (OOPT condition not met) must NOT clobber OVAL to IVOV — the
    // retained OVAL stands and no spurious OVAL monitor is posted.
    // `cached_should_output` is this cycle's doOutput decision. This is NOT
    // additionally gated on a calc-failure (unlike acalcout): calcout's hook
    // runs after the framework's `evaluate_alarms`, so the INVALID severity it
    // sees already covers calc/limit/MS — exactly as C `execOutput` applies
    // IVOA on any `nsev >= INVALID_ALARM`.
    fn apply_invalid_output_value(&mut self, ivov: EpicsValue) -> CaResult<()> {
        if self.cached_should_output {
            self.put_field("OVAL", ivov)
        } else {
            Ok(())
        }
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            // C `calcoutRecord.c::init_record:191-205` — `clcv = postfix(...)`
            // and `oclv = postfix(...)`, UNCONDITIONALLY, both logged but never
            // fatal. Base `postfix()` refuses an empty expression
            // (`postfix.c:235-240`: CALC_ERR_NULL_ARG, return -1), so C's own
            // default `field(OCAL,"")` record inits with OCLV = -1 and a
            // `field(CALC,"")` one with CLCV = -1. The port skipped the compile
            // when the field was empty and left the validity code at 0, so a
            // record that C reports as invalid looked healthy — and CLCV then
            // depended on whether the value arrived from the db file or from a
            // later put, which is not a distinction C makes. The compile is the
            // single owner of CLCV/RPCL on both paths.
            let report = self.recompile_calc("calcout: init_record: Illegal CALC field");
            self.init_report(report);
            let report = self.recompile_ocal("calcout: init_record: Illegal OCAL field");
            self.init_report(report);
            self.pval = self.val;
            self.mlst = self.val;
            self.alst = self.val;
            self.lalm = self.val;
        }
        Ok(())
    }

    fn set_process_continuation(
        &mut self,
        _continuation: bool,
        pact: crate::server::record::Pact<'_>,
    ) {
        self.pact = pact.held();
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.input_changes = 0;
        // ODLY continuation: this is the delayed re-process scheduled by a
        // previous cycle (C `calcoutRecord.c::process` `pact==TRUE` + `dlya`
        // branch, `:294-301`). No input fetch and no CALC pass — C's `pact`
        // arm re-enters below them — but `execOutput` DOES run here, and the
        // DOPT switch is its first half, so OVAL is computed now, from the
        // A..U present at expiry.
        if self.pact && self.dlya == 1 {
            self.dlya = 0;
            self.dlya_moved = true;
            self.cached_should_output = self.pending_output;
            self.pending_output = false;
            self.exec_output_data();
            self.advance_prev_all();
            return Ok(ProcessOutcome::complete());
        }

        // NOTE: pval is updated AFTER CALC evaluation (at the end),
        // not before. It holds the previous cycle's value for
        // transition detection in should_output().

        // C `calcoutRecord.c::process` (237-243) runs the calc only
        // `if (fetch_values(prec) == 0)`. A failed input link freezes VAL and
        // UDF and raises no CALC_ALARM; the OOPT decision below then runs
        // against that frozen VAL (C leaves the switch OUTSIDE the gate), so an
        // OOPT of Every_Time still drives OUT with the previous value.
        // The arg set is the record's own A..U: C hands BOTH passes
        // `&prec->a`, so a CALC-pass store (`A:=A+1`) is what the OCAL pass
        // fetches, and both land in the record's fields. Hence the store below
        // lands before `exec_output_data` reads the vars back; two independent
        // copies made CALC's stores invisible to OCAL and dropped them both.
        let mut inputs = crate::calc::NumericInputs::with_vars(self.get_vars());
        if !self.fetch_gate_failed {
            // C `calcoutRecord.c:238-241` — `calcPerform` runs unconditionally
            // inside the fetch gate and a -1 is CALC_ALARM/INVALID with VAL
            // unchanged. RPCL is always a program: an empty or uncompilable
            // CALC is the empty one, and it fails here every cycle rather than
            // being silently skipped.
            //
            // C `calcPerform(&prec->a, &prec->val, rpcl)` (calcoutRecord.c:238)
            // passes `presult = &val`, so the CALC `VAL` token reads the
            // *previous* VAL. Seed before `self.val` is overwritten below.
            inputs.prev_val = self.val;
            match crate::calc::eval(&self.rpcl, &mut inputs) {
                Ok(v) => {
                    self.val = v;
                    // C `:241` `else prec->udf = isnan(prec->val)` — this arm
                    // only; the write itself is made in `check_alarms`.
                    self.value_computed = true;
                }
                // C `calcoutRecord.c:239`: CALC failure → amsg "calcPerform".
                Err(_) => {
                    self.calc_alarm.get_or_insert("calcPerform");
                }
            }
        }

        // C `:240` `prec->udf = isnan(prec->val)` — the CALC-pass half of udf.
        // The OCAL half lives in `exec_output_data` (C `execOutput:624`) and is
        // therefore absent during an ODLY window, exactly as in C, where udf is
        // VAL-based until the delayed `execOutput` runs.
        self.ocal_udf_override = None;

        // The CALC pass's stores. C wrote them into A..U through `&prec->a` as
        // the pass ran; `exec_output_data` reads them back for the OCAL pass.
        self.apply_stores(&inputs.vars);
        // C `:255-272` reads PVAL in the OOPT switch and `:275` advances it,
        // both above the ODLY branch — so PVAL advances even on a cycle that
        // returns to wait the delay out.
        let do_output = self.should_output();
        self.pval = self.val;

        // ODLY (C `calcoutRecord.c::process` lines 276-288): when an
        // output should fire and ODLY > 0, defer the OUT-link write by
        // ODLY seconds. Set DLYA, suppress this cycle's output, and ask
        // the framework to re-process after the delay. The continuation
        // branch at the top of process() then emits the captured output.
        if do_output && self.odly > 0.0 {
            self.dlya = 1;
            self.dlya_moved = true;
            self.pending_output = true;
            self.cached_should_output = false;
            let delay = crate::runtime::time::duration_from_secs(self.odly);
            // C `calcoutRecord.c::process` (lines 277-282): the delaying
            // cycle sets DLYA, posts it (`db_post_events(&prec->dlya,
            // DBE_VALUE)`), schedules the delayed callback, and `return 0`
            // — BEFORE `monitor()` (306) and `recGblFwdLink()` (307). So
            // VAL/OVAL monitors and the forward link are NOT emitted on the
            // delaying cycle; they fire once on the delayed (callback) cycle.
            // Model this as an async-pending-notify pass: the framework
            // posts only DLYA now and defers the FLNK + VAL/OVAL snapshot to
            // the Complete continuation (the `dlya == 1` branch at the top
            // of process()). The previous `complete_with` ran the full
            // snapshot + FLNK tail on the delaying cycle, so VAL/OVAL posted
            // ODLY-seconds early and the forward link fired twice.
            return Ok(ProcessOutcome {
                result: RecordProcessResult::AsyncPendingNotify(Vec::new()),
                actions: vec![ProcessAction::ReprocessAfter(delay)],
                device_did_compute: false,
                status: Ok(()),
            });
        }

        self.cached_should_output = do_output;
        if do_output {
            // C `:283`, the immediate arm of `if (doOutput)`.
            self.exec_output_data();
        }
        self.advance_prev_all();
        Ok(ProcessOutcome::complete())
    }

    /// C reads `prec->inpa..inpu` off the record and copies nothing; the generic
    /// `get_field` path hands back an owned `EpicsValue` per link, which is
    /// 21 clones on every cycle of a record that wires none of them.
    fn link_text_ref(&self, link_field: &str) -> Option<&[u8]> {
        let [b'I', b'N', b'P', slot] = *link_field.as_bytes() else {
            return None;
        };
        Some(match slot {
            b'A' => self.inpa.as_bytes(),
            b'B' => self.inpb.as_bytes(),
            b'C' => self.inpc.as_bytes(),
            b'D' => self.inpd.as_bytes(),
            b'E' => self.inpe.as_bytes(),
            b'F' => self.inpf.as_bytes(),
            b'G' => self.inpg.as_bytes(),
            b'H' => self.inph.as_bytes(),
            b'I' => self.inpi.as_bytes(),
            b'J' => self.inpj.as_bytes(),
            b'K' => self.inpk.as_bytes(),
            b'L' => self.inpl.as_bytes(),
            b'M' => self.inpm.as_bytes(),
            b'N' => self.inpn.as_bytes(),
            b'O' => self.inpo.as_bytes(),
            b'P' => self.inpp.as_bytes(),
            b'Q' => self.inpq.as_bytes(),
            b'R' => self.inpr.as_bytes(),
            b'S' => self.inps.as_bytes(),
            b'T' => self.inpt.as_bytes(),
            b'U' => self.inpu.as_bytes(),
            _ => return None,
        })
    }

    fn cycle_cell(&self, cell: CycleCell) -> CycleRead {
        match cell {
            CycleCell::Val => CycleRead::Value(CycleValue::Double(self.val)),
            CycleCell::Oval => CycleRead::Value(CycleValue::Double(self.oval)),
            CycleCell::Mdel => CycleRead::Value(CycleValue::Double(self.mdel)),
            CycleCell::Adel => CycleRead::Value(CycleValue::Double(self.adel)),
            CycleCell::Mlst => CycleRead::Value(CycleValue::Double(self.mlst)),
            CycleCell::Alst => CycleRead::Value(CycleValue::Double(self.alst)),
            CycleCell::Lalm => CycleRead::Value(CycleValue::Double(self.lalm)),
            _ => CycleRead::Absent,
        }
    }

    fn put_cycle_cell(&mut self, cell: CycleCell, value: CycleValue) -> CaResult<()> {
        match (cell, value) {
            (CycleCell::Lalm, CycleValue::Double(v)) => self.lalm = v,
            (CycleCell::Alst, CycleValue::Double(v)) => self.alst = v,
            (CycleCell::Mlst, CycleValue::Double(v)) => self.mlst = v,
            (cell, value) => return self.put_field(cell.name(), value.into()),
        }
        Ok(())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "CALC" => Some(EpicsValue::String(self.calc.clone().into())),
            "CLCV" => Some(EpicsValue::Long(self.clcv)),
            "OCLV" => Some(EpicsValue::Long(self.oclv)),
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "HOPR" => Some(EpicsValue::Double(self.hopr)),
            "LOPR" => Some(EpicsValue::Double(self.lopr)),
            "ADEL" => Some(EpicsValue::Double(self.adel)),
            "MDEL" => Some(EpicsValue::Double(self.mdel)),
            "LALM" => Some(EpicsValue::Double(self.lalm)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
            "MLST" => Some(EpicsValue::Double(self.mlst)),
            "PVAL" => Some(EpicsValue::Double(self.pval)),
            "OOPT" => Some(EpicsValue::Short(self.oopt)),
            "ODLY" => Some(EpicsValue::Double(self.odly)),
            "DLYA" => Some(EpicsValue::UShort(self.dlya)),
            "OEVT" => Some(EpicsValue::String(self.oevt.clone())),
            "DOPT" => Some(EpicsValue::Short(self.dopt)),
            "OCAL" => Some(EpicsValue::String(self.ocal.clone().into())),
            "OVAL" => Some(EpicsValue::Double(self.oval)),
            "POVL" => Some(EpicsValue::Double(self.povl)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Double(self.ivov)),
            "INPA" => Some(EpicsValue::String(self.inpa.clone().into())),
            "INPB" => Some(EpicsValue::String(self.inpb.clone().into())),
            "INPC" => Some(EpicsValue::String(self.inpc.clone().into())),
            "INPD" => Some(EpicsValue::String(self.inpd.clone().into())),
            "INPE" => Some(EpicsValue::String(self.inpe.clone().into())),
            "INPF" => Some(EpicsValue::String(self.inpf.clone().into())),
            "INPG" => Some(EpicsValue::String(self.inpg.clone().into())),
            "INPH" => Some(EpicsValue::String(self.inph.clone().into())),
            "INPI" => Some(EpicsValue::String(self.inpi.clone().into())),
            "INPJ" => Some(EpicsValue::String(self.inpj.clone().into())),
            "INPK" => Some(EpicsValue::String(self.inpk.clone().into())),
            "INPL" => Some(EpicsValue::String(self.inpl.clone().into())),
            "INPM" => Some(EpicsValue::String(self.inpm.clone().into())),
            "INPN" => Some(EpicsValue::String(self.inpn.clone().into())),
            "INPO" => Some(EpicsValue::String(self.inpo.clone().into())),
            "INPP" => Some(EpicsValue::String(self.inpp.clone().into())),
            "INPQ" => Some(EpicsValue::String(self.inpq.clone().into())),
            "INPR" => Some(EpicsValue::String(self.inpr.clone().into())),
            "INPS" => Some(EpicsValue::String(self.inps.clone().into())),
            "INPT" => Some(EpicsValue::String(self.inpt.clone().into())),
            "INPU" => Some(EpicsValue::String(self.inpu.clone().into())),
            "A" => Some(EpicsValue::Double(self.a)),
            "B" => Some(EpicsValue::Double(self.b)),
            "C" => Some(EpicsValue::Double(self.c)),
            "D" => Some(EpicsValue::Double(self.d)),
            "E" => Some(EpicsValue::Double(self.e)),
            "F" => Some(EpicsValue::Double(self.f)),
            "G" => Some(EpicsValue::Double(self.g)),
            "H" => Some(EpicsValue::Double(self.h)),
            "I" => Some(EpicsValue::Double(self.i)),
            "J" => Some(EpicsValue::Double(self.j)),
            "K" => Some(EpicsValue::Double(self.k)),
            "L" => Some(EpicsValue::Double(self.l)),
            "M" => Some(EpicsValue::Double(self.m)),
            "N" => Some(EpicsValue::Double(self.n)),
            "O" => Some(EpicsValue::Double(self.o)),
            "P" => Some(EpicsValue::Double(self.p)),
            "Q" => Some(EpicsValue::Double(self.q)),
            "R" => Some(EpicsValue::Double(self.r)),
            "S" => Some(EpicsValue::Double(self.s)),
            "T" => Some(EpicsValue::Double(self.t)),
            "U" => Some(EpicsValue::Double(self.u)),
            "LA" => Some(EpicsValue::Double(self.la)),
            "LB" => Some(EpicsValue::Double(self.lb)),
            "LC" => Some(EpicsValue::Double(self.lc)),
            "LD" => Some(EpicsValue::Double(self.ld)),
            "LE" => Some(EpicsValue::Double(self.le)),
            "LF" => Some(EpicsValue::Double(self.lf)),
            "LG" => Some(EpicsValue::Double(self.lg)),
            "LH" => Some(EpicsValue::Double(self.lh)),
            "LI" => Some(EpicsValue::Double(self.li)),
            "LJ" => Some(EpicsValue::Double(self.lj)),
            "LK" => Some(EpicsValue::Double(self.lk)),
            "LL" => Some(EpicsValue::Double(self.ll)),
            "LM" => Some(EpicsValue::Double(self.lm)),
            "LN" => Some(EpicsValue::Double(self.ln)),
            "LO" => Some(EpicsValue::Double(self.lo)),
            "LP" => Some(EpicsValue::Double(self.lp)),
            "LQ" => Some(EpicsValue::Double(self.lq)),
            "LR" => Some(EpicsValue::Double(self.lr)),
            "LS" => Some(EpicsValue::Double(self.ls)),
            "LT" => Some(EpicsValue::Double(self.lt)),
            "LU" => Some(EpicsValue::Double(self.lu)),
            // INAV..INUV / OUTV link-status menus (menu(calcoutINAV),
            // calcoutRecord.dbd.pod:865-1012), served as DBR_ENUM; labels
            // from menu_field_choices. Live status from refresh_link_status.
            _ => Self::status_index(name).map(|i| EpicsValue::Enum(self.link_status[i] as u16)),
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::Double(v) => {
                    self.val = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            // C `dbPut` stores the string, then `special()` compiles it and
            // records the postfix() status in CLCV — see `Self::special`.
            "CALC" => match value {
                EpicsValue::String(s) => {
                    self.calc = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("CALC".into())),
            },
            // Plain DBF_LONG fields in C (calcoutRecord.dbd.pod:729,1049) — a
            // client may write them; the next CALC/OCAL put overwrites them.
            "CLCV" => {
                self.clcv = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("CLCV".into()))?
                    as i32;
                Ok(())
            }
            "OCLV" => {
                self.oclv = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("OCLV".into()))?
                    as i32;
                Ok(())
            }
            // PVAL (DBF_DOUBLE, "Previous Value", calcoutRecord.dbd.pod:718-720)
            // carries no `special()`/`pp()`, so C `dbPut` stores it verbatim like
            // any plain field — a client `caput CO.PVAL 5` succeeds. It is
            // TRANSIENT: `process()` overwrites `pval = val` at the end of each
            // cycle (calcoutRecord.c:263 `prec->pval = prec->val`), so the stored
            // value stands only until the next process. Mirrors VAL's DBF_DOUBLE
            // arm above (same type, same record). The port previously had no arm
            // and rejected the put with `FieldNotFound`.
            "PVAL" => match value {
                EpicsValue::Double(v) => {
                    self.pval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("PVAL".into())),
            },
            "EGU" => match value {
                EpicsValue::String(s) => {
                    self.egu.set_bytes(s.as_bytes());
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
            "HOPR" => match value {
                EpicsValue::Double(v) => {
                    self.hopr = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "LOPR" => match value {
                EpicsValue::Double(v) => {
                    self.lopr = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ADEL" => match value {
                EpicsValue::Double(v) => {
                    self.adel = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "MDEL" => match value {
                EpicsValue::Double(v) => {
                    self.mdel = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "LALM" => match value {
                EpicsValue::Double(v) => {
                    self.lalm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ALST" => match value {
                EpicsValue::Double(v) => {
                    self.alst = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "MLST" => match value {
                EpicsValue::Double(v) => {
                    self.mlst = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OOPT" => match value {
                EpicsValue::Short(v) => {
                    self.oopt = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OOPT".into())),
            },
            "ODLY" => match value {
                EpicsValue::Double(v) => {
                    self.odly = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("ODLY".into())),
            },
            "DLYA" => Err(CaError::ReadOnlyField("DLYA".into())),
            "OEVT" => match value {
                EpicsValue::String(s) => {
                    self.oevt = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OEVT".into())),
            },
            "DOPT" => match value {
                EpicsValue::Short(v) => {
                    self.dopt = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("DOPT".into())),
            },
            "OCAL" => match value {
                EpicsValue::String(s) => {
                    self.ocal = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OCAL".into())),
            },
            "OVAL" => match value {
                EpicsValue::Double(v) => {
                    self.oval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OVAL".into())),
            },
            "POVL" => match value {
                EpicsValue::Double(v) => {
                    self.povl = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("POVL".into())),
            },
            "IVOA" => match value {
                EpicsValue::Short(v) => {
                    self.ivoa = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("IVOA".into())),
            },
            "IVOV" => match value {
                EpicsValue::Double(v) => {
                    self.ivov = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("IVOV".into())),
            },
            "INPA" => match value {
                EpicsValue::String(s) => {
                    self.inpa = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPA".into())),
            },
            "INPB" => match value {
                EpicsValue::String(s) => {
                    self.inpb = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPB".into())),
            },
            "INPC" => match value {
                EpicsValue::String(s) => {
                    self.inpc = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPC".into())),
            },
            "INPD" => match value {
                EpicsValue::String(s) => {
                    self.inpd = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPD".into())),
            },
            "INPE" => match value {
                EpicsValue::String(s) => {
                    self.inpe = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPE".into())),
            },
            "INPF" => match value {
                EpicsValue::String(s) => {
                    self.inpf = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPF".into())),
            },
            "INPG" => match value {
                EpicsValue::String(s) => {
                    self.inpg = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPG".into())),
            },
            "INPH" => match value {
                EpicsValue::String(s) => {
                    self.inph = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPH".into())),
            },
            "INPI" => match value {
                EpicsValue::String(s) => {
                    self.inpi = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPI".into())),
            },
            "INPJ" => match value {
                EpicsValue::String(s) => {
                    self.inpj = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPJ".into())),
            },
            "INPK" => match value {
                EpicsValue::String(s) => {
                    self.inpk = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPK".into())),
            },
            "INPL" => match value {
                EpicsValue::String(s) => {
                    self.inpl = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPL".into())),
            },
            "INPM" => match value {
                EpicsValue::String(s) => {
                    self.inpm = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPM".into())),
            },
            "INPN" => match value {
                EpicsValue::String(s) => {
                    self.inpn = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPN".into())),
            },
            "INPO" => match value {
                EpicsValue::String(s) => {
                    self.inpo = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPO".into())),
            },
            "INPP" => match value {
                EpicsValue::String(s) => {
                    self.inpp = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPP".into())),
            },
            "INPQ" => match value {
                EpicsValue::String(s) => {
                    self.inpq = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPQ".into())),
            },
            "INPR" => match value {
                EpicsValue::String(s) => {
                    self.inpr = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPR".into())),
            },
            "INPS" => match value {
                EpicsValue::String(s) => {
                    self.inps = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPS".into())),
            },
            "INPT" => match value {
                EpicsValue::String(s) => {
                    self.inpt = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPT".into())),
            },
            "INPU" => match value {
                EpicsValue::String(s) => {
                    self.inpu = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPU".into())),
            },
            "A" | "B" | "C" | "D" | "E" | "F" | "G" | "H" | "I" | "J" | "K" | "L" | "M" | "N"
            | "O" | "P" | "Q" | "R" | "S" | "T" | "U" => {
                let v = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                match name {
                    "A" => self.a = v,
                    "B" => self.b = v,
                    "C" => self.c = v,
                    "D" => self.d = v,
                    "E" => self.e = v,
                    "F" => self.f = v,
                    "G" => self.g = v,
                    "H" => self.h = v,
                    "I" => self.i = v,
                    "J" => self.j = v,
                    "K" => self.k = v,
                    "L" => self.l = v,
                    "M" => self.m = v,
                    "N" => self.n = v,
                    "O" => self.o = v,
                    "P" => self.p = v,
                    "Q" => self.q = v,
                    "R" => self.r = v,
                    "S" => self.s = v,
                    "T" => self.t = v,
                    "U" => self.u = v,
                    _ => unreachable!(),
                }
                Ok(())
            }
            _ => {
                // INAV..INUV / OUTV link-status menus are read-only to
                // clients (SPC_NOMOD, calcoutRecord.dbd.pod:867); the
                // link-status refresh (`post_fields` → `put_field_internal`)
                // lands here to store the connection status it just computed.
                let Some(idx) = Self::status_index(name) else {
                    return Err(CaError::FieldNotFound(name.to_string()));
                };
                self.link_status[idx] = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch(name.into()))?
                    as i16;
                Ok(())
            }
        }
    }

    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "OOPT" => Some(CALCOUT_OOPT_CHOICES),
            "DOPT" => Some(CALCOUT_DOPT_CHOICES),
            // INAV..INUV / OUTV link-status menus (menu(calcoutINAV)).
            "OUTV" => Some(LINK_STATUS_CHOICES),
            _ if Self::input_status_index(field).is_some() => Some(LINK_STATUS_CHOICES),
            _ => None,
        }
    }

    /// C `calcoutRecord.c:163`: every CONSTANT input link is loaded into its value
    /// field ONCE, at `init_record` (`recGblInitConstantLink(plink,
    /// DBF_DOUBLE, pvalue)`); `dbGetLink` then delivers nothing for it on
    /// every later process, so a client's `caput REC.A 99` stands.
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        crate::server::record::seed_input_links(self.multi_input_links())
    }

    /// The 21 `INPA..INPU` texts read straight off the record's own fields, not
    /// through the default body's 21 name matches on `link_text_ref`.
    /// See [`Record::set_input_link_slots`].
    fn set_input_link_slots(&self) -> Option<(u64, u64)> {
        crate::server::record::input_link_slots_of(&[
            self.inpa.as_bytes(),
            self.inpb.as_bytes(),
            self.inpc.as_bytes(),
            self.inpd.as_bytes(),
            self.inpe.as_bytes(),
            self.inpf.as_bytes(),
            self.inpg.as_bytes(),
            self.inph.as_bytes(),
            self.inpi.as_bytes(),
            self.inpj.as_bytes(),
            self.inpk.as_bytes(),
            self.inpl.as_bytes(),
            self.inpm.as_bytes(),
            self.inpn.as_bytes(),
            self.inpo.as_bytes(),
            self.inpp.as_bytes(),
            self.inpq.as_bytes(),
            self.inpr.as_bytes(),
            self.inps.as_bytes(),
            self.inpt.as_bytes(),
            self.inpu.as_bytes(),
        ])
    }

    fn multi_input_links(&self) -> &'static [(&'static str, &'static str)] {
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
            ("INPM", "M"),
            ("INPN", "N"),
            ("INPO", "O"),
            ("INPP", "P"),
            ("INPQ", "Q"),
            ("INPR", "R"),
            ("INPS", "S"),
            ("INPT", "T"),
            ("INPU", "U"),
        ]
    }

    /// C `calcoutRecord.c::fetch_values` (694-709) reads every INP link and
    /// keeps the FIRST failing status; `process` (237) gates `calcPerform` on it.
    fn input_fetch_policy(&self) -> InputFetchPolicy {
        InputFetchPolicy::ReadAllGateOnFailure
    }

    fn set_fetch_gate_failed(&mut self, failed: bool) {
        self.fetch_gate_failed = failed;
    }

    /// Both of C's `prec->udf` writes sit on a `calcPerform` success arm
    /// (`calcoutRecord.c:241`, `:624`), the CALC one inside the `fetch_values`
    /// gate (`:237`), so a failed input link or a failed CALC/OCAL leaves UDF at
    /// its previous value. The write lives with the compute now — see
    /// [`Self::check_alarms`].
    fn clears_udf(&self) -> bool {
        false
    }

    fn should_output(&self) -> bool {
        self.cached_should_output
    }

    /// `OEVT` ("Event To Issue"): post the named output event when output
    /// fires. C `calcoutRecord.c` `execOutput` does `if (prec->epvt)
    /// postEvent(prec->epvt);` right after `writeValue`, gated to the same
    /// OOPT/calc-fail/ODLY decision as the OUT write (`cached_should_output`,
    /// the framework's `should_output()` gate) — the framework adds the IVOA
    /// `Don't_drive` veto. The event name is posted verbatim (a numeric name
    /// is canonicalised by the event router to match a `SCAN="Event"` record's
    /// `EVNT`). An empty `OEVT` is C's `epvt == NULL` (no event).
    fn output_event(&self) -> Option<PvString> {
        if self.cached_should_output && !self.oevt.as_bytes().trim_ascii().is_empty() {
            Some(self.oevt.clone())
        } else {
            None
        }
    }

    fn can_device_write(&self) -> bool {
        // calcout has a soft OUT link, not device support
        false
    }

    fn set_async_context(&mut self, name: crate::types::RecordName, db: AsyncDbHandle) {
        self.async_ctx = Some((name, db));
        // C `init_record` (calcoutRecord.c:160-189) classifies every INP and
        // the OUT link into INAV..INUV/OUTV. The INP links are calcout fields
        // already applied (record fields load before `add_record`), so
        // classify them now. The OUT link is a common field not yet applied
        // at `add_record`; it is captured later in `init_links`. The
        // generation gate lets that later, fuller refresh supersede this one.
        self.refresh_link_status();
    }

    fn init_links(&mut self, common: &crate::server::record::CommonFields) {
        // C `calcoutRecord.c::init_record` (calcoutRecord.c:160-189)
        // classifies the OUT link at `i == CALCPERFORM_NARGS`, at load —
        // before any process. The OUT link is a common field invisible to
        // `set_async_context` (which ran before the common fields were
        // applied), so capture it here, once the framework has resolved it,
        // and classify so a passive never-processed record already shows OUTV.
        self.out = common.text.out.as_bytes().into();
        // A load-time `special()` marked posts nobody could watch; this full
        // classification supersedes them, so the first runtime put must not
        // drain them.
        self.put_posts = 0;
        self.refresh_link_status();
    }

    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after {
            return Ok(());
        }
        // C `calcoutRecord.c::special:326-345` — CALC/OCAL recompile into
        // RPCL/ORPC and store postfix()'s RETURN STATUS in CLCV/OCLV (0 or -1),
        // post DBE_VALUE for the validity field, and return 0: the put is
        // ACCEPTED even when the expression is garbage. This is the opposite
        // disposition from calcRecord (which returns S_db_badField and fails
        // the put, R8-1) — both run off the one compile owner, and C's
        // asymmetry is deliberate: calcout carries the error in a field,
        // calc carries it in the put status.
        match field {
            "CALC" => {
                self.put_report = self.recompile_calc("calcout: special(): Illegal CALC field");
                self.put_posts = 1 << CLCV_POST;
                return Ok(());
            }
            "OCAL" => {
                self.put_report = self.recompile_ocal("calcout: special(): Illegal OCAL field");
                self.put_posts = 1 << (CLCV_POST + 1);
                return Ok(());
            }
            _ => {}
        }
        // C `calcoutRecord.c:366-405`. `dbPutFieldLink` runs this pass before
        // `dbAddLink` (dbAccess.c:1181-1208), so the link has no `lset` yet and
        // `dbLinkIsConstant` holds for EVERY put: the status reads Constant
        // whatever the target, and `recGblInitConstantLink` loads nothing.
        // Taking the link's generation drops its classification still in
        // flight; the other links' stand.
        if let Some(idx) = Self::link_index(field) {
            self.link_status[idx] = LINK_CON;
            self.link_gen.next_for(1 << idx);
            self.put_posts = 0b11 << (2 * idx);
        }
        Ok(())
    }

    /// The posts `special()` makes, each a literal `DBE_VALUE`: a link put's
    /// value and status (calcoutRecord.c:377,404), and CLCV/OCLV after a
    /// CALC/OCAL put (`:335`, `:344`). calcout declares no `CYCLE_POSTED`
    /// hook, so only the put path drains this.
    fn take_cycle_posted_fields(&mut self) -> crate::server::record::CycleMarks {
        use crate::server::record::{CycleMarks, CyclePostMask};
        /// Link `i`'s value then status at `2i`, `2i + 1`; OUT's status is
        /// OUTV. CLCV and OCLV follow at [`CLCV_POST`].
        const PUT_POSTS_NAMED: [(&str, CyclePostMask); CLCV_POST + 2] = {
            let mut table = [("", CyclePostMask::Value); CLCV_POST + 2];
            let mut i = 0;
            while i < CALCOUT_LINK_VALUE_FIELDS.len() {
                table[2 * i].0 = CALCOUT_LINK_VALUE_FIELDS[i];
                table[2 * i + 1].0 = if i < CALCOUT_INAV_FIELDS.len() {
                    CALCOUT_INAV_FIELDS[i]
                } else {
                    "OUTV"
                };
                i += 1;
            }
            table[CLCV_POST].0 = "CLCV";
            table[CLCV_POST + 1].0 = "OCLV";
            table
        };
        static PUT_POSTS: [(
            crate::server::record::FieldIx,
            crate::server::record::CyclePostMask,
        ); CLCV_POST + 2] = crate::server::record::declared_post_table(
            crate::server::record::dbd_generated::CALCOUT_FIELDS,
            PUT_POSTS_NAMED,
        );
        CycleMarks::of(&PUT_POSTS, u128::from(std::mem::take(&mut self.put_posts)))
    }

    /// C's post carries a literal `DBE_VALUE`, not `DBE_VALUE | DBE_LOG`.
    fn value_only_change_fields(&self) -> &'static [&'static str] {
        &["CLCV", "OCLV"]
    }

    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        // C's two `prec->udf` writes (`calcoutRecord.c:241` CALC, `:624` OCAL),
        // applied here because `check_alarms` is the record's only hook holding
        // `CommonFields` and it runs before `recGblCheckUDF`. A cycle whose
        // `fetch_values` failed reaches neither write, so UDF freezes — the
        // blanket re-derive used to clear it and report a never-computed record
        // as defined.
        if std::mem::take(&mut self.value_computed) {
            common.udf = self.value_is_undefined() as u8;
        }

        // C `calcoutRecord.c:238-241` (CALC) and `:622` (OCAL, inside
        // `execOutput`) — a failed `calcPerform` is `recGblSetSevr(prec,
        // CALC_ALARM, INVALID_ALARM)`, raised in `process()` before
        // `checkAlarms(prec)`. Consuming the flag keeps it a per-cycle fact: a
        // cycle whose input fetch failed runs no `calcPerform` (`:237`) and
        // raises nothing.
        if let Some(msg) = self.calc_alarm.take() {
            // C `calcoutRecord.c:239` (CALC) / `:622` (OCAL) attach the exact
            // amsg via `recGblSetSevrMsg`: "calcPerform" / "OCAL calcPerform".
            // (calcout is the one calc-family record whose C uses a message;
            // calc/scalcout/acalcout/swait use plain recGblSetSevr.)
            crate::server::recgbl::rec_gbl_set_sevr_msg(
                common,
                crate::server::recgbl::alarm_status::CALC_ALARM,
                crate::server::record::AlarmSeverity::Invalid,
                msg,
            );
        }
    }
}

#[cfg(test)]
mod link_status_tests {
    use super::*;
    use crate::server::record::dbd_generated;

    // The link-status menu choice labels, C `menu(calcoutINAV)`
    // (calcoutRecord.dbd.pod:45-50): identical to sseqLNKV.
    const CHOICES: &[&str] = &["Ext PV NC", "Ext PV OK", "Local PV", "Constant"];
    const LOC: u16 = 2;
    const CON: u16 = 3;

    // Boundary: the `IN<letter>V` status field name maps to the input
    // index A..U, and is distinct from the `INP<letter>` link field (no
    // trailing `V`) and from `OUTV` (handled separately).
    #[test]
    fn input_status_index_boundaries() {
        assert_eq!(CalcoutRecord::input_status_index("INAV"), Some(0)); // input A
        assert_eq!(CalcoutRecord::input_status_index("INUV"), Some(20)); // input U (last)
        assert_eq!(CalcoutRecord::input_status_index("INPV"), Some(15)); // status of input P
        // OUTV is not an input-status field (caller handles it).
        assert_eq!(CalcoutRecord::input_status_index("OUTV"), None);
        // INP<letter> link fields have no trailing V → not a status field.
        assert_eq!(CalcoutRecord::input_status_index("INPA"), None);
        assert_eq!(CalcoutRecord::input_status_index("INPU"), None);
        // 'V' is past 'U' (CALCPERFORM_NARGS == 21) → no such input.
        assert_eq!(CalcoutRecord::input_status_index("INVV"), None);
        // Two-letter middle → not a single input.
        assert_eq!(CalcoutRecord::input_status_index("INABV"), None);
    }

    // Boundary: C's `lnkIndex` covers INPA..INPU and OUT, nothing else.
    #[test]
    fn link_index_boundaries() {
        assert_eq!(CalcoutRecord::link_index("INPA"), Some(0));
        assert_eq!(CalcoutRecord::link_index("INPU"), Some(20));
        assert_eq!(CalcoutRecord::link_index("OUT"), Some(21));
        assert_eq!(CalcoutRecord::link_index("INPV"), None); // past U
        assert_eq!(CalcoutRecord::link_index("INAV"), None); // status, not link
        assert_eq!(CalcoutRecord::link_index("CALC"), None);
    }

    // Every INAV..INUV and OUTV serves the menu(calcoutINAV) labels; the
    // INP link fields do not.
    #[test]
    fn link_status_menu_labels_served() {
        let rec = CalcoutRecord::default();
        for f in CALCOUT_INAV_FIELDS.iter().chain(std::iter::once(&"OUTV")) {
            assert_eq!(
                rec.menu_field_choices(f),
                Some(CHOICES),
                "{f} must serve menu(calcoutINAV) labels"
            );
        }
        assert_eq!(rec.menu_field_choices("INPA"), None);
    }

    // Default-constructed record: empty/unconfigured links classify CON
    // (C calcoutRecord.c:166-167), served as DBR_ENUM index 3.
    #[test]
    fn link_status_defaults_to_con() {
        let rec = CalcoutRecord::default();
        assert_eq!(rec.get_field("INAV"), Some(EpicsValue::Enum(CON)));
        assert_eq!(rec.get_field("INUV"), Some(EpicsValue::Enum(CON)));
        assert_eq!(rec.get_field("OUTV"), Some(EpicsValue::Enum(CON)));
    }

    // The internal link-status refresh writes through put_field
    // (post_fields → put_field_internal); a write must round-trip.
    #[test]
    fn link_status_internal_put_roundtrips() {
        let mut rec = CalcoutRecord::default();
        rec.put_field("INAV", EpicsValue::Enum(LOC)).unwrap();
        rec.put_field("OUTV", EpicsValue::Enum(LOC)).unwrap();
        assert_eq!(rec.get_field("INAV"), Some(EpicsValue::Enum(LOC)));
        assert_eq!(rec.get_field("OUTV"), Some(EpicsValue::Enum(LOC)));
        // A non-status unknown field still errors.
        assert!(rec.put_field("NOSUCH", EpicsValue::Enum(0)).is_err());
    }

    // All 22 status fields are in the field table as DBF_MENU→Enum,
    // read-only to clients (SPC_NOMOD, calcoutRecord.dbd.pod:867).
    #[test]
    fn link_status_fields_are_read_only_enum_in_table() {
        for name in CALCOUT_INAV_FIELDS.iter().chain(std::iter::once(&"OUTV")) {
            let fd = dbd_generated::CALCOUT_FIELDS
                .iter()
                .find(|f| f.name == *name)
                .unwrap_or_else(|| panic!("{name} missing from dbd_generated::CALCOUT_FIELDS"));
            assert_eq!(fd.dbf_type, DbFieldType::Enum, "{name} must be ENUM");
            assert!(fd.read_only, "{name} must be read-only (SPC_NOMOD)");
        }
        assert_eq!(CALCOUT_INAV_FIELDS.len(), 21);
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;

    /// PVAL (DBF_DOUBLE, no special/pp) accepts a client put and stores it
    /// verbatim — including NaN and the infinities, which are ordinary
    /// DBF_DOUBLE values. The store is TRANSIENT: `process()` overwrites
    /// `pval = val` each cycle, so a following process replaces it.
    #[test]
    fn pval_put_stores_transient_double() {
        let mut rec = CalcoutRecord::default();
        rec.put_field("PVAL", EpicsValue::Double(-1.0)).unwrap();
        assert_eq!(rec.pval, -1.0);
        rec.put_field("PVAL", EpicsValue::Double(f64::INFINITY))
            .unwrap();
        assert_eq!(rec.pval, f64::INFINITY);
        rec.put_field("PVAL", EpicsValue::Double(f64::NAN)).unwrap();
        assert!(rec.pval.is_nan());
        // Transient: a process cycle latches pval = val (0.0 here).
        rec.init_record(0).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.pval, rec.val);
    }

    /// CALC `VAL` token reads the previous VAL (C `presult = &val`,
    /// calcoutRecord.c:238), so `CALC="VAL+1"` counts up.
    #[test]
    fn calc_val_token_reads_previous_val() {
        let mut rec = CalcoutRecord {
            calc: "VAL+1".to_string(),
            ..Default::default()
        };
        rec.init_record(0).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, 1.0);
        rec.process().unwrap();
        assert_eq!(rec.val, 2.0);
    }

    /// OOPT="On Change" is C's negated inclusive deadband
    /// (`calcoutRecord.c:257`), not `>`. The boundaries that separate the two
    /// spellings: NaN on either side (every IEEE comparison with NaN is false,
    /// so the negation fires and `>` does not) and |PVAL-VAL| exactly equal to
    /// MDEL (C's `<=` is inclusive, so it must NOT fire — the case a naive
    /// `>=` rewrite breaks).
    #[test]
    fn on_change_matches_c_negated_deadband_at_nan_and_at_mdel() {
        let nan = f64::NAN;
        for (pval, val, mdel, want, why) in [
            (0.0, nan, 0.0, true, "VAL NaN, PVAL finite"),
            (nan, 0.0, 0.0, true, "PVAL NaN, VAL finite"),
            (
                nan,
                nan,
                0.0,
                true,
                "both NaN: fabs(NaN-NaN) is NaN, <= false",
            ),
            (1.0, nan, 5.0, true, "NaN ignores MDEL entirely"),
            (
                0.0,
                1.0,
                1.0,
                false,
                "|PVAL-VAL| exactly MDEL: C's <= is inclusive",
            ),
            (0.0, 1.5, 1.0, true, "|PVAL-VAL| above MDEL"),
            (2.0, 2.0, 0.0, false, "unchanged finite value, MDEL 0"),
        ] {
            let rec = CalcoutRecord {
                oopt: 1,
                pval,
                val,
                mdel,
                ..Default::default()
            };
            assert_eq!(
                rec.should_output(),
                want,
                "{why}: PVAL={pval} VAL={val} MDEL={mdel}"
            );
        }
    }

    /// OCAL `VAL` token reads the previous OVAL, not VAL (C `presult =
    /// &oval`, calcoutRecord.c:621). With DOPT="Use OCAL" and OOPT="Every
    /// Time", `OCAL="VAL+1"` makes OVAL count up while VAL stays 0.
    #[test]
    fn ocal_val_token_reads_previous_oval() {
        let mut rec = CalcoutRecord {
            // CALC empty → VAL stays 0; only OCAL drives OVAL.
            ocal: "VAL+1".to_string(),
            dopt: 1, // Use OCAL
            oopt: 0, // Every Time → should_output() always true
            ..Default::default()
        };
        rec.init_record(0).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, 0.0);
        assert_eq!(rec.oval, 1.0);
        rec.process().unwrap();
        assert_eq!(rec.oval, 2.0);
        rec.process().unwrap();
        assert_eq!(rec.oval, 3.0);
    }

    /// C `calcoutRecord.c:239` (CALC) and `:622` (OCAL) attach DISTINCT amsg
    /// literals via `recGblSetSevrMsg`: "calcPerform" and "OCAL calcPerform".
    /// pvxs serves these verbatim on alarm.message (iocsource.cpp:230-236);
    /// calcout is the one calc-family record whose C uses a message at all.
    #[test]
    fn calcout_calc_fail_amsg_is_calcperform_ocal_fail_is_ocal_calcperform() {
        use crate::server::record::CommonFields;

        // CALC fails (empty program), DOPT=Use VAL so no OCAL runs → "calcPerform".
        let mut rec = CalcoutRecord::default();
        rec.init_record(0).unwrap();
        rec.process().unwrap();
        let mut common = CommonFields::default();
        rec.check_alarms(&mut common);
        assert_eq!(common.nsta, crate::server::recgbl::alarm_status::CALC_ALARM);
        assert_eq!(common.namsg, "calcPerform");

        // CALC succeeds, OCAL fails (empty), DOPT=Use OCAL → "OCAL calcPerform".
        let mut rec = CalcoutRecord {
            calc: "5".to_string(),
            ocal: String::new(),
            dopt: 1,
            oopt: 0,
            ..Default::default()
        };
        rec.init_record(0).unwrap();
        rec.process().unwrap();
        let mut common = CommonFields::default();
        rec.check_alarms(&mut common);
        assert_eq!(common.namsg, "OCAL calcPerform");

        // Both fail in one cycle: C raises CALC first (raise-only), so the
        // CALC message "calcPerform" wins over the later OCAL setSevr.
        let mut rec = CalcoutRecord {
            calc: String::new(),
            ocal: String::new(),
            dopt: 1,
            oopt: 0,
            ..Default::default()
        };
        rec.init_record(0).unwrap();
        rec.process().unwrap();
        let mut common = CommonFields::default();
        rec.check_alarms(&mut common);
        assert_eq!(
            common.namsg, "calcPerform",
            "both-fail keeps the CALC message (C raises CALC before OCAL)"
        );
    }
}
