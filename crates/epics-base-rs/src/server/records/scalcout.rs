use super::calc_compile;
use crate::error::{CaError, CaResult};
use crate::server::record::{
    InputFetchPolicy, OutTarget, ProcessAction, ProcessOutcome, Record, RecordProcessResult,
};
use crate::types::{DbFieldType, EpicsValue, PvString};

use super::link_status::{
    LINK_CON, LINK_STATUS_CHOICES, LinkRole, LinkStatusGen, post_link_status,
};
use crate::calc::StringInputs;
use crate::calc::engine::value::{SCALC_STRING_SIZE, ScalcString};
use crate::calc::{CompiledExpr, ExprKind, ScalcResult, scalc_perform};
use crate::server::database::AsyncDbHandle;

/// Code version reported by `VERS` (C `sCalcoutRecord.c:55 #define VERSION 4.1`).
const VERSION: f64 = 4.1;

/// Scalcout record — string calc with output.
///
/// Like calcout but uses the string calc engine (sCalcPerform).
/// CALC expression evaluates to SVAL (string) or VAL (numeric).
/// OCAL provides optional output calculation.
/// Output decision controlled by OOPT.
pub struct ScalcoutRecord {
    pub val: f64,
    pub sval: PvString,
    pub calc: String,
    /// C `RPCL`. Always a program: an empty or uncompilable CALC carries C's
    /// empty `END_EXPRESSION` postfix, which `sCalcPerform` refuses to run
    /// (`sCalcPerform.c:396`), so the record alarms on every process.
    compiled_calc: CompiledExpr,
    /// CLCV/OCLV — `DBF_LONG` expression-validity fields
    /// (sCalcoutRecord.dbd:75,438). C stores `sCalcPostfix()`'s RETURN VALUE
    /// (`pcalc->clcv = sCalcPostfix(...)`, sCalcoutRecord.c:464,475): 0 when the
    /// expression compiled, -1 when it did not (sCalcPostfix.c:873-881).
    pub clcv: i32,
    pub oclv: i32,
    pub oopt: i16, // 0=Every, 1=OnChange, 2=WhenZero, 3=WhenNonzero, 4=TransZero, 5=TransNonzero
    pub dopt: i16, // 0=Use CALC, 1=Use OCAL
    pub ocal: String,
    /// C `ORPC`. Same contract as [`Self::compiled_calc`].
    compiled_ocal: CompiledExpr,
    pub oval: f64,
    pub osv: PvString,
    pub ivoa: i16, // 0=Continue, 1=Don't drive, 2=Set to IVOV
    pub ivov: f64,
    pub out: String, // output link
    pub wait: i16,   // wait for output completion
    pub prec: i16,
    // MDEL / ADEL (C `sCalcoutRecord.dbd:541-550`, both DBF_DOUBLE). MDEL is
    // read on two paths: the OOPT="On Change" test
    // (`sCalcoutRecord.c:379`, `fabs(pval - val) > mdel`) and `monitor()`
    // (:821-826), which is the framework's MDEL/ADEL deadband path here
    // (`uses_monitor_deadband` defaults to true, and it reads the deadbands
    // through `get_field("MDEL")`/`("ADEL")`). Neither field existed, so the
    // deadbands read back as the framework's 0.0 default, a client put to
    // either was rejected with `FieldNotFound`, and the On-Change test had no
    // deadband to consult.
    pub mdel: f64,
    pub adel: f64,
    // Input link strings (INPA..INPL)
    pub inp_links: [String; 12],
    // String-input link names (INAA..INLL, C `sCalcoutRecord.dbd:151-223`,
    // DBF_INLINK). C `fetch_values` (890-941) reads each one as DBR_STRING into
    // AA..LL; the record had no such fields at all, so the twelve string inputs
    // could only ever be set by a client put and a `.INAA` put was rejected with
    // `FieldNotFound`. See [`Record::string_input_links`].
    pub str_inp_links: [String; 12],
    // Numeric input values A-L (mapped to vars A-P, but only 12 used)
    pub num_vals: [f64; 12],
    // String input values AA-LL
    pub str_vals: [PvString; 12],
    /// PVAL — C `sCalcoutRecord.dbd:60` `field(PVAL,DBF_DOUBLE)`, writable.
    ///
    /// The value VAL had at the END of the previous process cycle: C assigns
    /// `pcalc->pval = pcalc->val` at `sCalcoutRecord.c:397`, after the OOPT
    /// switch has read it. It is the left operand of every OOPT comparison
    /// (`:379` On Change, `:382/:385` the two transitions).
    ///
    /// It was a private `prev_val` captured at the TOP of `process()`, which is
    /// the value VAL had at the *start of this cycle* — the same thing only when
    /// nothing wrote VAL between cycles. A client `caput VAL` (or an OUT link
    /// into VAL) moved the port's previous-value cell and C's does not, so the
    /// On-Change/transition decision diverged; and PVAL, which C lets an
    /// operator write to force or suppress the next output, did not exist.
    pub pval: f64,
    /// LALM — C `sCalcoutRecord.dbd:858` `field(LALM,DBF_DOUBLE)`,
    /// `special(SPC_NOMOD)`.
    ///
    /// The level `checkAlarms` last alarmed at (`sCalcoutRecord.c:699-751`),
    /// which is what makes HYST a per-level hysteresis rather than a plain
    /// deadband. Written only by the framework's analog ladder — the single
    /// owner of the ten-field alarm surface this record shares with
    /// calc/calcout/ai/ao.
    pub lalm: f64,
    /// MLST / ALST — C `sCalcoutRecord.dbd`, both `DBF_DOUBLE`
    /// `special(SPC_NOMOD)`. `monitor()` deadbands VAL against them
    /// (sCalcoutRecord.c:828,837). The record served neither, so the
    /// framework had no cell to remember the last posted value in and
    /// treated every cycle as the first one.
    pub mlst: f64,
    pub alst: f64,
    /// PSVL — C `sCalcoutRecord.dbd:63` `field(PSVL,DBF_STRING)`,
    /// `special(SPC_NOMOD)`.
    ///
    /// The SVAL value C last posted: `monitor()` posts SVAL when it differs from
    /// PSVL and then copies it (`sCalcoutRecord.c:842-846`). So it equals SVAL
    /// after every cycle that reaches `monitor()`, and lags it across an
    /// ODLY-delayed output, whose scheduling cycle returns before `monitor()`
    /// (`:407`).
    ///
    /// The private `prev_sval` it replaces was a copy of SVAL taken at the top of
    /// `process()` and read by nothing.
    psvl: PvString,
    /// This cycle's `sCalcPerform` outcome — set when the CALC or OCAL
    /// evaluation fails (C `sCalcoutRecord.c:357-363`, `:786-808`). A per-cycle
    /// fact, not record state: `check_alarms` — the owner of this record's alarm
    /// transitions — consumes it, so it cannot outlive the cycle that set it.
    calc_alarm: bool,
    /// This cycle's `fetch_values()` outcome, pushed by the framework through
    /// `set_fetch_gate_failed`. C `sCalcoutRecord.c::process` (356) runs
    /// `sCalcPerform` only `if (fetch_values(pcalc)==0)`, and `fetch_values`
    /// (885-887) returns at the first failing numeric `dbGetLink`.
    fetch_gate_failed: bool,
    /// Output decision from the last `process()`. The framework's
    /// generic multi-output dispatch reads `multi_output_links()`
    /// unconditionally, so this caches the OOPT decision and gates
    /// the OUT-link write on it.
    cached_should_output: bool,
    /// Output delay in seconds — C `sCalcoutRecord.c` `prec->odly`. When
    /// an output should fire and `odly > 0`, the OUT-link write is deferred
    /// by `odly` seconds (C `process` lines 400-408).
    pub odly: f64,
    /// Delay-active flag — C `prec->dlya`. Set to 1 on the delaying cycle
    /// (posted DBE_VALUE) and cleared to 0 on the delayed continuation
    /// (C `process` lines 401/425). Distinguishes the continuation re-entry.
    dlya: i16,
    /// Snapshot of the delaying cycle's output decision, restored into
    /// `cached_should_output` on the continuation so the deferred OUT write
    /// honours the original cycle's OOPT result. Mirrors calcout.rs.
    pending_output: bool,
    /// `OEVT` ("Event To Issue") — C `sCalcoutRecord.c` `prec->oevt`
    /// (DBF_USHORT). When output fires and `oevt > 0`, `execOutput` posts
    /// the numeric software event (`post_event((int)oevt)`); see
    /// [`Record::output_event`].
    oevt: u16,
    /// `INAV..INLV` / `IAAV..ILLV` / `OUTV` — the per-link connection status C
    /// derives in `init_record` (`sCalcoutRecord.c:254-287`) and re-derives in
    /// `special()` on any INPx/INxx/OUT put (`:495-569`). Written only by
    /// [`Self::refresh_link_status`] (through `post_fields`), never by a
    /// client: the fields are `special(SPC_NOMOD)`.
    in_status: [i16; 12],
    str_status: [i16; 12],
    out_status: i16,
    /// Async surface + generation gate for `refresh_link_status`, the shape
    /// calcout/transform use (see `link_status::post_link_status`).
    async_ctx: Option<(String, AsyncDbHandle)>,
    link_gen: LinkStatusGen,
}

impl Default for ScalcoutRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            sval: PvString::new(),
            calc: String::new(),
            compiled_calc: CompiledExpr::empty(ExprKind::String),
            clcv: 0,
            oclv: 0,
            oopt: 0,
            dopt: 0,
            ocal: String::new(),
            compiled_ocal: CompiledExpr::empty(ExprKind::String),
            oval: 0.0,
            osv: PvString::new(),
            ivoa: 0,
            ivov: 0.0,
            out: String::new(),
            wait: 0,
            prec: 0,
            mdel: 0.0,
            adel: 0.0,
            inp_links: Default::default(),
            str_inp_links: Default::default(),
            // C's post-`init_record` value for a default record: every link is
            // CONSTANT, so every status is `scalcoutINAV_CON`.
            in_status: [LINK_CON; 12],
            str_status: [LINK_CON; 12],
            out_status: LINK_CON,
            async_ctx: None,
            link_gen: LinkStatusGen::default(),
            num_vals: [0.0; 12],
            str_vals: Default::default(),
            pval: 0.0,
            lalm: 0.0,
            mlst: 0.0,
            alst: 0.0,
            psvl: PvString::new(),
            calc_alarm: false,
            fetch_gate_failed: false,
            cached_should_output: false,
            odly: 0.0,
            dlya: 0,
            pending_output: false,
            oevt: 0,
        }
    }
}

impl ScalcoutRecord {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the calc inputs. `prev_val` / `prev_sval` are the cells C passes as
    /// `presult` / `psresult`, which the `VAL` (`FETCH_VAL`,
    /// sCalcPerform.c:921-925) and `SVAL` (`FETCH_SVAL`, :927-932) tokens push:
    /// for CALC those are `&pcalc->val, pcalc->sval` (C `sCalcoutRecord.c:357`)
    /// and for OCAL `&pcalc->oval, pcalc->osv` (`:768-769`) — in every case the
    /// *previous* result.
    fn build_inputs(&self, prev_val: f64, prev_sval: &PvString) -> StringInputs {
        // C `sCalcPerform(&pcalc->a, MAX_FIELDS, (char **)(pcalc->strs),
        // STRING_MAX_FIELDS, ...)` (`sCalcoutRecord.c:357`, `:768`) — both counts
        // are 12 (`:191-192`). The engine can address more args than that; scalcout
        // does not supply them, so `M`/`@12` and above fetch 0 and store nowhere.
        let mut inputs = StringInputs::with_counts(12, 12);
        for i in 0..12 {
            inputs.num_vars[i] = self.num_vals[i];
            // C `FETCH_AA` copies the record's `char[40]` field into the 40-byte
            // stack element (`sCalcPerform.c:872`), so the engine sees the same
            // bytes, bounded the same way — no re-encoding on either side.
            inputs.str_vars[i] = ScalcString::from_c(self.str_vals[i].as_bytes());
        }
        inputs.prev_val = prev_val;
        inputs.prev_sval = ScalcString::from_c(prev_sval.as_bytes());
        inputs
    }

    /// Land the cycle's variable stores back in A..L and AA..LL — the inverse of
    /// [`Self::build_inputs`], and the record's ONLY write-back of an engine var
    /// set.
    ///
    /// C `sCalcPerform(&pcalc->a, MAX_FIELDS, (char **)(pcalc->strs), …)`
    /// (`sCalcoutRecord.c:357`) hands the engine pointers INTO the record, so
    /// both store families write the record's fields directly:
    /// `parg[op - STORE_A] = *pd--` (`sCalcPerform.c:429-433`) for A..L, and
    /// `strncpy(psarg[op - STORE_AA], ps->s, …)` (`:888-894`) for AA..LL. The
    /// engine here evaluates an owned copy, so `CALC="A:=A+1;A"` and
    /// `CALC="AA:='x';…"` both stored into a temporary that was then dropped.
    fn apply_stores(&mut self, inputs: &StringInputs) {
        self.num_vals[..12].copy_from_slice(&inputs.num_vars[..12]);
        for i in 0..12 {
            self.str_vals[i] = PvString::from_bytes(inputs.str_vars[i].as_bytes().to_vec());
        }
    }

    /// C `sCalcoutRecord.c:357-359` — `sCalcPerform(..., &pcalc->val,
    /// pcalc->sval, ..., pcalc->prec)`: the record hands the engine the two
    /// cells and the engine fills both ([`scalc_perform`]). VAL and SVAL are
    /// two views of ONE result, not two computations.
    fn apply_result(&mut self, result: &ScalcResult) {
        self.val = result.val;
        self.sval = PvString::from_bytes(result.sval.as_bytes());
    }

    /// C `sCalcoutRecord.c::execOutput` (755-777), the "Determine output data"
    /// half: the DOPT switch that fills OVAL/OSV.
    ///
    /// The single owner of that switch, called from the two places C calls
    /// `execOutput`: the immediate output at `:410` and the delayed
    /// continuation at `:429`. C's ODLY arm returns at `:407` with the switch
    /// UNRUN, so on a delayed cycle OCAL runs against the A..L / AA..LL present
    /// at EXPIRY. Evaluating it while scheduling made the delay a no-op for
    /// every input that moved inside the window — and made an OCAL with stores
    /// (`A:=A+1`) land at scheduling time.
    ///
    /// Runs on EVERY output cycle, before the IVOA decision (whose Don't_drive
    /// `break` is at `:795`), so OVAL/OSV are recomputed even when the OUT
    /// write is vetoed.
    fn exec_output_data(&mut self) {
        if self.dopt != 1 {
            // Use CALC result.
            self.oval = self.val;
            self.osv = self.sval.clone();
            return;
        }
        // Use OCAL. C `:768` calls sCalcPerform on ORPC unconditionally on this
        // branch, so an empty, uncompilable or failing OCAL are one case — the
        // empty program fails like any other broken one. `presult = &pcalc->oval,
        // psresult = pcalc->osv`, so the VAL/SVAL tokens in OCAL read the
        // previous OVAL/OSV, not the VAL/SVAL this cycle computed; only the
        // RESULT cells differ between the passes, the arg set is the record's
        // own A..L / AA..LL that CALC stored into.
        let mut inputs = self.build_inputs(self.oval, &self.osv);
        match scalc_perform(&self.compiled_ocal, &mut inputs, self.prec) {
            // As on the CALC side: a non-finite OCAL result is C's -1 with the
            // cells written, and `execOutput` reads only the status — so it
            // takes the OVAL=-1 sentinel branch too.
            Ok(result) if !result.non_finite => {
                // The OCAL-side mirror of `apply_result`: C passes
                // `&pcalc->oval, pcalc->osv` and the SAME `pcalc->prec` to the
                // same sCalcPerform (`:768-770`), so the same epilogue fills
                // both cells.
                self.oval = result.val;
                self.osv = PvString::from_bytes(result.sval.as_bytes());
            }
            _ => {
                // C `:771-773`: a failed OCAL sCalcPerform forces OVAL=-1 and
                // OSV="***ERROR***" — the OCAL-side mirror of the CALC-fail
                // VAL=-1 sentinel.
                self.oval = -1.0;
                self.osv = PvString::from("***ERROR***");
                self.calc_alarm = true;
            }
        }
        self.apply_stores(&inputs);
    }

    /// C `sCalcoutRecord.c:374-395` — the OOPT switch. "On Change" is the
    /// numeric MDEL-deadband test `fabs(pcalc->pval - pcalc->val) > pcalc->mdel`
    /// (:379) and nothing else: SVAL does not take part, so a cycle that changed
    /// only the string result does NOT drive OUT on C, and a numeric change
    /// inside MDEL does not either.
    ///
    /// The switch DECIDES an output; it does not veto one. C's `doOutput` is
    /// initialised to 0 (`sCalcoutRecord.c:326`) and only a case that fires sets
    /// it — so `Never` (menu index 6, `sCalcoutRecord.dbd:17`) is
    /// `doOutput = 0` (`:393-395`), and so is any index the switch does not name.
    /// A catch-all of `true` here inverted `OOPT="Never"` outright: `CALC="7"`
    /// wrote 7.0 to the OUT target every cycle where C writes nothing.
    fn should_output(&self) -> bool {
        match self.oopt {
            0 => true,                                     // Every Time
            1 => (self.pval - self.val).abs() > self.mdel, // On Change
            2 => self.val == 0.0,                          // When Zero
            3 => self.val != 0.0,                          // When Non-zero
            4 => self.pval != 0.0 && self.val == 0.0,      // Transition To Zero
            5 => self.pval == 0.0 && self.val != 0.0,      // Transition To Non-zero
            6 => false,                                    // Never
            _ => false,                                    // C's `doOutput = 0` init
        }
    }

    /// C `monitor()` (`sCalcoutRecord.c:842-846`): SVAL is posted when it
    /// differs from PSVL, and PSVL then takes its value. The framework owns the
    /// post (SVAL is change-detected against its last posted value, which is the
    /// same test); this owns the copy, at the same point — the end of every cycle
    /// that reaches `monitor()`. The ODLY-scheduling cycle is not one of them: C
    /// `return(0)`s at `:407`, so PSVL lags SVAL for the length of the delay.
    fn sync_psvl(&mut self) {
        self.psvl = self.sval.clone();
    }

    /// C `sCalcoutRecord.c::special:463-471` (and the same two lines in
    /// `init_record`): compile into RPCL and store `sCalcPostfix()`'s return
    /// status in CLCV. An empty CALC compiles to an empty program with status 0
    /// (sCalcPostfix.c:432-434) — unlike base `postfix()`, which calls it
    /// CALC_ERR_NULL_ARG.
    fn recompile_calc(&mut self) {
        let compiled = calc_compile::scalc_postfix("scalcout", "CALC", &self.calc);
        self.clcv = compiled.status;
        self.compiled_calc = compiled.program;
    }

    /// C `sCalcoutRecord.c::special:474-482` — same, into ORPC/OCLV.
    fn recompile_ocal(&mut self) {
        let compiled = calc_compile::scalc_postfix("scalcout", "OCAL", &self.ocal);
        self.oclv = compiled.status;
        self.compiled_ocal = compiled.program;
    }

    fn var_index(name: &str) -> Option<usize> {
        if name.len() == 1 {
            let c = name.as_bytes()[0];
            if c >= b'A' && c <= b'L' {
                return Some((c - b'A') as usize);
            }
        }
        None
    }

    fn str_var_index(name: &str) -> Option<usize> {
        const NAMES: [&str; 12] = [
            "AA", "BB", "CC", "DD", "EE", "FF", "GG", "HH", "II", "JJ", "KK", "LL",
        ];
        NAMES.iter().position(|&n| n == name)
    }

    fn inp_index(name: &str) -> Option<usize> {
        const NAMES: [&str; 12] = [
            "INPA", "INPB", "INPC", "INPD", "INPE", "INPF", "INPG", "INPH", "INPI", "INPJ", "INPK",
            "INPL",
        ];
        NAMES.iter().position(|&n| n == name)
    }

    /// INAA..INLL — the link name feeding string input AA..LL, in the same
    /// index order as [`Self::str_var_index`] (C keeps them in one contiguous
    /// array starting at `pcalc->inaa`, walked with `sFldnames`,
    /// sCalcoutRecord.c:200-201,890-891).
    fn str_inp_index(name: &str) -> Option<usize> {
        SCALCOUT_STRING_INPUT_LINKS
            .iter()
            .position(|(lf, _)| *lf == name)
    }

    /// The link-connection-status fields INAV..INLV (numeric inputs),
    /// IAAV..ILLV (string inputs) and OUTV (output)
    /// (`sCalcoutRecord.dbd:229-402`, DBF_MENU `menu(scalcoutINAV)`,
    /// `special(SPC_NOMOD)`).
    ///
    /// Each is DERIVED from its link, never client-set: C `init_record`
    /// (`sCalcoutRecord.c`, same shape as `aCalcoutRecord.c:208-242`) stores
    /// `scalcoutINAV_CON` (=3) for every CONSTANT link, and a default record
    /// has all links constant. Like the sibling `acalcout` record this port
    /// classifies the status STATICALLY (no live re-derivation on a link
    /// re-point), so every field reads `Constant`. The dbd `initial("1")` is
    /// C's pre-init placeholder that `init_record` overwrites — serving it raw
    /// (what `declared_default` did for these un-modeled fields) reported
    /// `Ext PV OK` where C reports `Constant`.
    fn is_link_status_field(name: &str) -> bool {
        name == "OUTV" || SCALCOUT_INAV_NAMES.contains(&name) || SCALCOUT_IAAV_NAMES.contains(&name)
    }

    /// Classify all 25 links and publish INAV..INLV / IAAV..ILLV / OUTV,
    /// mirroring C `sCalcoutRecord.c::init_record` (254-287) and the
    /// `special()` re-classification (495-569). No-op without an async context.
    fn refresh_link_status(&self) {
        let mut links: Vec<(&'static str, String, LinkRole)> = Vec::with_capacity(25);
        for i in 0..12 {
            links.push((
                SCALCOUT_INAV_NAMES[i],
                self.inp_links[i].clone(),
                LinkRole::Input,
            ));
            links.push((
                SCALCOUT_IAAV_NAMES[i],
                self.str_inp_links[i].clone(),
                LinkRole::Input,
            ));
        }
        links.push(("OUTV", self.out.clone(), LinkRole::Output));
        post_link_status(self.async_ctx.as_ref(), &self.link_gen, links);
    }
}

/// INAV..INLV — numeric-input connection-status field names (channel A..L).
static SCALCOUT_INAV_NAMES: [&str; 12] = [
    "INAV", "INBV", "INCV", "INDV", "INEV", "INFV", "INGV", "INHV", "INIV", "INJV", "INKV", "INLV",
];

/// IAAV..ILLV — string-input connection-status field names (channel AA..LL).
static SCALCOUT_IAAV_NAMES: [&str; 12] = [
    "IAAV", "IBBV", "ICCV", "IDDV", "IEEV", "IFFV", "IGGV", "IHHV", "IIIV", "IJJV", "IKKV", "ILLV",
];

/// C `sCalcoutRecord.c::fetch_values`' second loop (890-941): INAA..LL → AA..LL.
/// Order is C's `sFldnames` order, i.e. the `str_vals` index order.
static SCALCOUT_STRING_INPUT_LINKS: &[(&str, &str)] = &[
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

/// Choice labels for the `scalcout` output-execute-option menu, in index
/// order. C `menu(scalcoutOOPT)` (`sCalcoutRecord.dbd`): like `calcoutOOPT`
/// but with a trailing "Never" choice (index 6) that suppresses output
/// entirely.
const SCALCOUT_OOPT_CHOICES: &[&str] = &[
    "Every Time",
    "On Change",
    "When Zero",
    "When Non-zero",
    "Transition To Zero",
    "Transition To Non-zero",
    "Never",
];

/// Choice labels for the `scalcout` output-data-option menu, in index
/// order. C `menu(scalcoutDOPT)` (`sCalcoutRecord.dbd`): 0="Use CALC"
/// (result of `CALC`), 1="Use OCAL" (result of the `OCAL` expression).
const SCALCOUT_DOPT_CHOICES: &[&str] = &["Use CALC", "Use OCAL"];

/// Choice labels for the `scalcout` wait-for-completion menu, in index
/// order. C `menu(scalcoutWAIT)` (`sCalcoutRecord.dbd`): 0=NoWait, 1=Wait.
const SCALCOUT_WAIT_CHOICES: &[&str] = &["NoWait", "Wait"];

impl Record for ScalcoutRecord {
    /// C `sCalcoutRecord.c::init_record` (:203-322) ends without touching
    /// LALM; every write to it is in `checkAlarms` (:727-750).
    fn seed_deadband_tracking(&mut self) {}

    fn record_type(&self) -> &'static str {
        "scalcout"
    }

    /// The link-status menus (INAV..INLV, IAAV..ILLV, OUTV) are served read-only
    /// by `get_field`, but the record owns no WRITE path for them — they are
    /// `SPC_NOMOD`, derived from the link (see `Self::is_link_status_field`).
    /// The loader's `.dbd`-initial seed and `.db field()` apply both key on this
    /// predicate to decide whether to WRITE a field; answering `false` keeps
    /// them from storing the `.dbd` `initial("1")` over the init-derived
    /// `Constant`. The read is unaffected: `resolve_field` consults `get_field`
    /// independently.
    fn implements_field(&self, name: &str) -> bool {
        if Self::is_link_status_field(name) {
            return false;
        }
        self.get_field(name).is_some()
    }

    /// C `scalcoutRecord.c::init_record` compiles CALC/OCAL into RPCL/ORPC and
    /// stores the postfix status in CLCV/OCLV (sCalcoutRecord.c:245-261) —
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

    /// C `sCalcoutRecord.c::special:462-482` — a put to CALC/OCAL recompiles
    /// into RPCL/ORPC, stores `sCalcPostfix()`'s return status in CLCV/OCLV,
    /// posts DBE_VALUE for it, and returns 0: the put is ACCEPTED even for a
    /// garbage expression (unlike calcRecord, which refuses it — R8-1).
    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after {
            return Ok(());
        }
        match field {
            "CALC" => self.recompile_calc(),
            "OCAL" => self.recompile_ocal(),
            _ => {}
        }
        // C `sCalcoutRecord.c:481-569` re-classifies the link a put just
        // re-pointed — the INPA..INPL, INAA..INLL and OUT cases together.
        if Self::inp_index(field).is_some()
            || Self::str_inp_index(field).is_some()
            || field == "OUT"
        {
            self.refresh_link_status();
        }
        Ok(())
    }

    fn set_async_context(&mut self, name: String, db: AsyncDbHandle) {
        self.async_ctx = Some((name, db));
        // C `init_record` (sCalcoutRecord.c:254-287) classifies all 25 links
        // before any process. Every one of them is a scalcout-owned field
        // (OUT included), already applied when `add_record` runs.
        self.refresh_link_status();
    }

    /// C posts the validity field explicitly from `special()`
    /// (`db_post_events(pcalc, &pcalc->clcv, DBE_VALUE)`,
    /// sCalcoutRecord.c:470,481); CLCV/OCLV are not `pp(TRUE)`, so nothing
    /// else would post them.
    fn monitor_side_effect_fields(&self, put_field: &str) -> &'static [&'static str] {
        match put_field {
            "CALC" => &["CLCV"],
            "OCAL" => &["OCLV"],
            _ => &[],
        }
    }

    /// C's post carries a literal `DBE_VALUE`, not `DBE_VALUE | DBE_LOG`.
    fn value_only_change_fields(&self) -> &'static [&'static str] {
        &["CLCV", "OCLV"]
    }

    /// Every `db_post_events` C's scalcout makes from a process cycle:
    /// `VAL` (`sCalcoutRecord.c:840`), `SVAL` (`:843`), `OSV` (`:848`),
    /// `A`..`L` (`:856`), `AA`..`LL` (`:862`), `OVAL` (`:867`) — all of
    /// `monitor()` — and `DLYA` (`:402`, `:426`).
    ///
    /// PVAL and PSVL are the reason this list exists: C's `monitor()` posts
    /// NEITHER (they are the cells it compares against), so exposing them as
    /// fields must not hand a subscriber events C does not send. Declaring C's
    /// set — rather than blacklisting the two — keeps that true for any field
    /// added later.
    fn process_posted_fields(&self) -> Option<&'static [&'static str]> {
        Some(&[
            "VAL", "SVAL", "OVAL", "OSV", "DLYA", "A", "B", "C", "D", "E", "F", "G", "H", "I", "J",
            "K", "L", "AA", "BB", "CC", "DD", "EE", "FF", "GG", "HH", "II", "JJ", "KK", "LL",
        ])
    }

    // C recScalcout.c IVOA=set_to_IVOV: oval = ivov (and osv = isvv
    // for string output side, but OUT writeback only reads OVAL).
    //
    // As in `calcout`, C's `oval = ivov` lives inside the `if (doOutput)`-gated
    // `execOutput` (sCalcoutRecord.c), so a non-output INVALID cycle must NOT
    // clobber OVAL to IVOV. Gate on `cached_should_output` (this cycle's
    // doOutput decision). The calc-failure `val = ivov` substitution earlier in
    // `process()` is a separate, pre-existing path and is unaffected here.
    fn apply_invalid_output_value(&mut self, ivov: EpicsValue) -> CaResult<()> {
        if self.cached_should_output {
            self.put_field("OVAL", ivov)
        } else {
            Ok(())
        }
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // ODLY continuation: this is the delayed re-process scheduled by a
        // previous cycle (C `sCalcoutRecord.c::process` `pact==TRUE` + `dlya`
        // branch, lines 421-432). No input fetch and no CALC pass — C's `pact`
        // arm re-enters below them — but `execOutput` DOES run here (`:429`),
        // and the DOPT switch is its first half (`:757-777`), so OVAL/OSV are
        // computed now, from the A..L / AA..LL present at expiry. Mirrors
        // calcout.rs.
        if self.dlya == 1 {
            self.dlya = 0;
            self.cached_should_output = self.pending_output;
            self.pending_output = false;
            self.exec_output_data();
            self.sync_psvl();
            return Ok(ProcessOutcome::complete());
        }

        // C `sCalcoutRecord.c::process` (356-367) runs the calc only
        // `if (fetch_values(pcalc)==0)`, and its `fetch_values` (885-887)
        // returns at the FIRST failing numeric `dbGetLink`. A failed input link
        // therefore freezes VAL/SVAL/UDF and raises no CALC_ALARM; the OOPT
        // switch, ODLY and the output below still run against the frozen VAL,
        // exactly as in C where the gate wraps only the `sCalcPerform` block.
        //
        // C `sCalcoutRecord.c:357-360` — inside that gate, sCalcPerform runs
        // unconditionally and a non-zero return is the failure. RPCL is always
        // a program, so "empty CALC", "CALC that would not compile" and "CALC
        // that failed at run time" are one case here, exactly as in C: the
        // empty program fails (`sCalcPerform.c:396`) and the record alarms
        // every process.
        // ONE var set for the whole cycle: C hands BOTH passes the same
        // `&pcalc->a` and `pcalc->strs` (`sCalcoutRecord.c:357`, `:768`), so a
        // CALC-pass store (`A:=A+1`, `AA:="x"`) is what the OCAL pass fetches,
        // and both land in the record's A..L / AA..LL ([`Self::apply_stores`]).
        let mut inputs = self.build_inputs(self.val, &self.sval);
        let calc_failed = if self.fetch_gate_failed {
            false
        } else {
            // C `sCalcoutRecord.c:357-359` — presult = &pcalc->val,
            // psresult = pcalc->sval: the VAL/SVAL tokens read this cycle's
            // pre-evaluation VAL/SVAL, i.e. the cells the results are about to
            // overwrite. (PVAL/PSVL are a different pair — see their docs.)
            match scalc_perform(&self.compiled_calc, &mut inputs, self.prec) {
                // C's two `-1`s are ONE failure to this record: `:357-364` tests
                // the return code alone, so a non-finite result (cells written,
                // status -1) and an operator refusal (nothing written) both take
                // the VAL=-1 / SVAL="***ERROR***" branch below.
                Ok(result) if !result.non_finite => {
                    self.apply_result(&result);
                    false
                }
                _ => true,
            }
        };

        // C `sCalcoutRecord.c:366` `else pcalc->udf = FALSE` — a successful
        // fetch+calc DEFINES VAL/SVAL. The clear is C's `else` arm of the
        // fetch+calc test, so it lives in `check_alarms` (C's post-calc tail)
        // alongside the CALC_ALARM raise: a failed calc or failed input fetch
        // (`fetch_gate_failed`, which C's `if (fetch_values==0)` gate skips)
        // both leave `common.udf` untouched.

        // IVOA=Don't_drive on a failed calc vetoes the OUT WRITE only. C
        // applies the veto inside `execOutput` (sCalcoutRecord.c:430), which
        // runs AFTER the ODLY decision — so an OOPT-fires + ODLY>0 cycle must
        // still schedule the delay, pulse DLYA, and fire FLNK on the
        // continuation; only the OUT link stays unwritten. Modelling the veto
        // as an early `return` skipped the ODLY branch entirely.
        let mut ivoa_veto_out = false;
        if calc_failed {
            self.calc_alarm = true;
            // C `sCalcoutRecord.c:361-363`: a failed sCalcPerform forces
            // VAL=-1 and SVAL="***ERROR***" (the CALC_ALARM severity itself
            // is raised by `check_alarms` from this flag). Before
            // this the failed cycle kept the previous VAL/SVAL with no value
            // sentinel, diverging from C.
            self.val = -1.0;
            self.sval = PvString::from("***ERROR***");
            // IVOA on the INVALID cycle. C applies it inside `execOutput`
            // (sCalcoutRecord.c:786-808): Don't_drive skips the write,
            // Set_to_IVOV sets `oval = ivov` (line 798) — NOT `val`. Only the
            // Don't_drive veto needs an in-record flag here; the OVAL=IVOV
            // substitution is owned by the framework's IVOA gate
            // (`apply_invalid_output_value`), which fires because the CALC_ALARM
            // raised by `check_alarms` drives this cycle INVALID. Setting
            // `self.val = ivov` here was wrong: it clobbered VAL (C keeps
            // VAL=-1) and duplicated the framework's OVAL write.
            if self.ivoa == 1 {
                ivoa_veto_out = true; // Don't drive outputs
            }
        }

        // OOPT decides whether output fires — this gates the ODLY delay + DLYA
        // pulse + completion (C `doOutput`). The IVOA=Don't_drive veto removes
        // only the OUT write. `write_out == oopt_fires` on every
        // non-Don't_drive path, so OVAL/OUT behaviour is unchanged there.
        let oopt_fires = self.should_output();
        let write_out = oopt_fires && !ivoa_veto_out;
        // C `sCalcoutRecord.c:397` — `pcalc->pval = pcalc->val`, immediately
        // after the OOPT switch read it and before `execOutput`. The advance is
        // unconditional: it happens on a cycle that drives no output, and it
        // happens on the ODLY-scheduling cycle, which returns at `:407` before
        // `monitor()`.
        self.pval = self.val;
        // The CALC pass's stores. C wrote them into A..L / AA..LL through
        // `&pcalc->a` / `pcalc->strs` as the pass ran, above the ODLY early
        // return, so a deferred cycle carries them just the same;
        // `exec_output_data` reads them back for the OCAL pass at expiry.
        self.apply_stores(&inputs);

        // ODLY (C `sCalcoutRecord.c::process` lines 399-408): when an output
        // should fire and ODLY > 0, defer the OUT-link write by ODLY seconds.
        // The delaying cycle sets DLYA=1, posts it (DBE_VALUE), schedules the
        // delayed callback, and `return 0` BEFORE `monitor()`/`recGblFwdLink()`
        // — so VAL/OVAL monitors and the forward link fire once on the delayed
        // (continuation) cycle, not now. Model this as an async-pending-notify
        // pass: post only DLYA now, suppress this cycle's output, and re-process
        // after the delay; the `dlya == 1` branch at the top then emits.
        // Mirrors calcout.rs.
        if oopt_fires && self.odly > 0.0 {
            self.dlya = 1;
            self.pending_output = write_out;
            self.cached_should_output = false;
            let delay = crate::runtime::time::duration_from_secs(self.odly);
            return Ok(ProcessOutcome {
                result: RecordProcessResult::AsyncPendingNotify(vec![(
                    "DLYA".to_string(),
                    EpicsValue::Short(1),
                )]),
                actions: vec![ProcessAction::ReprocessAfter(delay)],
                device_did_compute: false,
            });
        }

        self.cached_should_output = write_out;
        if oopt_fires {
            // C `:410`, the immediate arm of `if (doOutput)`. Gated on
            // `oopt_fires`, not `write_out`: the IVOA Don't_drive veto removes
            // the write, not the DOPT switch that precedes it.
            self.exec_output_data();
        }
        self.sync_psvl();
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "SVAL" => Some(EpicsValue::String(self.sval.clone())),
            "PVAL" => Some(EpicsValue::Double(self.pval)),
            "LALM" => Some(EpicsValue::Double(self.lalm)),
            "MLST" => Some(EpicsValue::Double(self.mlst)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
            "PSVL" => Some(EpicsValue::String(self.psvl.clone())),
            "CALC" => Some(EpicsValue::String(self.calc.clone().into())),
            "CLCV" => Some(EpicsValue::Long(self.clcv)),
            "OCLV" => Some(EpicsValue::Long(self.oclv)),
            "OOPT" => Some(EpicsValue::Short(self.oopt)),
            "DOPT" => Some(EpicsValue::Short(self.dopt)),
            "OCAL" => Some(EpicsValue::String(self.ocal.clone().into())),
            "OVAL" => Some(EpicsValue::Double(self.oval)),
            "OSV" => Some(EpicsValue::String(self.osv.clone())),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Double(self.ivov)),
            "OUT" => Some(EpicsValue::String(self.out.clone().into())),
            "WAIT" => Some(EpicsValue::Short(self.wait)),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "MDEL" => Some(EpicsValue::Double(self.mdel)),
            "ADEL" => Some(EpicsValue::Double(self.adel)),
            "ODLY" => Some(EpicsValue::Double(self.odly)),
            "DLYA" => Some(EpicsValue::Short(self.dlya)),
            "OEVT" => Some(EpicsValue::UShort(self.oevt)),
            // Fixed code-version constant, C `pcalc->vers = VERSION` in
            // init_record; never the `.dbd` initial.
            "VERS" => Some(EpicsValue::Double(VERSION)),
            _ => {
                if let Some(idx) = Self::var_index(name) {
                    return Some(EpicsValue::Double(self.num_vals[idx]));
                }
                if let Some(idx) = Self::str_var_index(name) {
                    return Some(EpicsValue::String(self.str_vals[idx].clone()));
                }
                if let Some(idx) = Self::inp_index(name) {
                    return Some(EpicsValue::String(self.inp_links[idx].clone().into()));
                }
                if let Some(idx) = Self::str_inp_index(name) {
                    return Some(EpicsValue::String(self.str_inp_links[idx].clone().into()));
                }
                if name == "OUTV" {
                    return Some(EpicsValue::Enum(self.out_status as u16));
                }
                if let Some(idx) = SCALCOUT_INAV_NAMES.iter().position(|&n| n == name) {
                    return Some(EpicsValue::Enum(self.in_status[idx] as u16));
                }
                if let Some(idx) = SCALCOUT_IAAV_NAMES.iter().position(|&n| n == name) {
                    return Some(EpicsValue::Enum(self.str_status[idx] as u16));
                }
                None
            }
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                self.val = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("VAL".into()))?;
                Ok(())
            }
            "SVAL" => match value {
                EpicsValue::String(s) => {
                    self.sval = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SVAL".into())),
            },
            // PVAL is writable in C (no SPC_NOMOD): writing it aims the next
            // OOPT comparison. PSVL is SPC_NOMOD — the framework refuses the put
            // from `read_only` in the field list, so there is no arm for it.
            "PVAL" => {
                self.pval = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("PVAL".into()))?;
                Ok(())
            }
            // SPC_NOMOD to a client (the field list refuses that put); this arm
            // is the framework analog ladder's write, C `pcalc->lalm = <level>`.
            "LALM" => {
                self.lalm = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("LALM".into()))?;
                Ok(())
            }
            "MLST" => {
                self.mlst = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("MLST".into()))?;
                Ok(())
            }
            "ALST" => {
                self.alst = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("ALST".into()))?;
                Ok(())
            }
            // C `dbPut` stores the string; `special()` compiles it and records
            // the sCalcPostfix() status in CLCV (see `Self::special`).
            "CALC" => match value {
                EpicsValue::String(s) => {
                    self.calc = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("CALC".into())),
            },
            // Plain DBF_LONG fields in C — writable; the next CALC/OCAL put
            // overwrites them.
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
            "OOPT" => match value {
                EpicsValue::Short(v) => {
                    self.oopt = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OOPT".into())),
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
            "OVAL" => {
                self.oval = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("OVAL".into()))?;
                Ok(())
            }
            "OSV" => match value {
                EpicsValue::String(s) => {
                    self.osv = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OSV".into())),
            },
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
            "OUT" => {
                if let EpicsValue::String(s) = value {
                    self.out = s.as_str_lossy().into_owned();
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch("OUT".into()))
                }
            }
            "WAIT" => {
                self.wait = value.to_f64().unwrap_or(0.0) as i16;
                Ok(())
            }
            "PREC" => match value {
                EpicsValue::Short(v) => {
                    self.prec = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("PREC".into())),
            },
            "MDEL" => {
                self.mdel = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("MDEL".into()))?;
                Ok(())
            }
            "ADEL" => {
                self.adel = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("ADEL".into()))?;
                Ok(())
            }
            "ODLY" => {
                self.odly = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("ODLY".into()))?;
                Ok(())
            }
            "DLYA" => Err(CaError::ReadOnlyField("DLYA".into())),
            "OEVT" => {
                self.oevt = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("OEVT".into()))?
                    as u16;
                Ok(())
            }
            // VERS is a fixed code-version constant; accept and ignore writes.
            "VERS" => Ok(()),
            _ => {
                if let Some(idx) = Self::var_index(name) {
                    self.num_vals[idx] = value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                    return Ok(());
                }
                if let Some(idx) = Self::str_var_index(name) {
                    match value {
                        EpicsValue::String(s) => {
                            self.str_vals[idx] = s;
                            return Ok(());
                        }
                        _ => return Err(CaError::TypeMismatch(name.into())),
                    }
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
                if let Some(idx) = Self::str_inp_index(name) {
                    match value {
                        EpicsValue::String(s) => {
                            self.str_inp_links[idx] = s.as_str_lossy().into_owned();
                            return Ok(());
                        }
                        _ => return Err(CaError::TypeMismatch(name.into())),
                    }
                }
                // The link-status menus are `special(SPC_NOMOD)` to clients;
                // this arm exists for the link-status refresh (`post_fields`
                // -> `put_field_internal`), their only writer.
                if Self::is_link_status_field(name) {
                    let status = value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?
                        as i16;
                    if name == "OUTV" {
                        self.out_status = status;
                    } else if let Some(idx) = SCALCOUT_INAV_NAMES.iter().position(|&n| n == name) {
                        self.in_status[idx] = status;
                    } else if let Some(idx) = SCALCOUT_IAAV_NAMES.iter().position(|&n| n == name) {
                        self.str_status[idx] = status;
                    }
                    return Ok(());
                }
                Err(CaError::FieldNotFound(name.to_string()))
            }
        }
    }

    /// C `sCalcoutRecord.c:258 (numeric INPA..L only — "Don't InitConstantLink the string links")`: every CONSTANT input link is loaded into its value
    /// field ONCE, at `init_record` (`recGblInitConstantLink(plink,
    /// DBF_DOUBLE, pvalue)`); `dbGetLink` then delivers nothing for it on
    /// every later process, so a client's `caput REC.A 99` stands.
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        crate::server::record::seed_input_links(self.multi_input_links())
    }

    /// C `sCalcoutRecord.c::special` (512-517) — same re-seed as calcout, under
    /// C's `if (fieldIndex <= scalcoutRecordINPL)` guard: only the NUMERIC
    /// inputs A..L are re-loaded (`INAA..INLL` still get `INAV = CON`, which the
    /// link-status refresh below covers, but no constant re-load). The port's
    /// `multi_input_links` IS that numeric table — the string inputs are not in
    /// it — so it maps exactly.
    fn special_reseed_input_links(&self) -> &[(&'static str, &'static str)] {
        self.multi_input_links()
    }

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
        ]
    }

    /// C `sCalcoutRecord.c::fetch_values` (885-887) `return`s at the first
    /// failing numeric `dbGetLink`, and `process` (356) gates `sCalcPerform` on
    /// the status. This policy covers the NUMERIC loop only; the string-input
    /// loop that follows it in C swallows its own errors and returns 0, and it
    /// is declared separately as [`Record::string_input_links`].
    fn input_fetch_policy(&self) -> InputFetchPolicy {
        InputFetchPolicy::AbortOnFirstFailure
    }

    /// C `sCalcoutRecord.c::fetch_values` (890-941) — INAA..INLL → AA..LL, read
    /// as DBR_STRING, outside the fetch gate.
    fn string_input_links(&self) -> &'static [(&'static str, &'static str)] {
        SCALCOUT_STRING_INPUT_LINKS
    }

    fn set_fetch_gate_failed(&mut self, failed: bool) {
        self.fetch_gate_failed = failed;
    }

    /// C `sCalcoutRecord.c:357-363` (CALC) and `:786-808` (OCAL, inside
    /// `execOutput`) — a failed `sCalcPerform` is `recGblSetSevr(pcalc,
    /// CALC_ALARM, INVALID_ALARM)`, raised in `process()` before `checkAlarms`.
    ///
    /// Consuming the flag keeps it a per-cycle fact: a cycle whose input fetch
    /// failed runs no `sCalcPerform` (`:356`) and raises nothing.
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        // C `sCalcoutRecord.c:356` gates the whole calc+afterCalc body on
        // `if (fetch_values(pcalc)==0)`; a failed input fetch runs no
        // `sCalcPerform`, raises nothing, and leaves `udf` untouched.
        if self.fetch_gate_failed {
            return;
        }
        if std::mem::take(&mut self.calc_alarm) {
            // C `sCalcoutRecord.c:364,774` use PLAIN `recGblSetSevr(pcalc,
            // CALC_ALARM, INVALID_ALARM)` — NULL message (empty namsg); PVA
            // falls back to the "CALC" condition string. No fabricated literal.
            crate::server::recgbl::rec_gbl_set_sevr(
                common,
                crate::server::recgbl::alarm_status::CALC_ALARM,
                crate::server::record::AlarmSeverity::Invalid,
            );
            // C's failed-calc branch leaves `udf` untouched, so a `caput UDF
            // <byte>` before a failing cycle keeps its raw byte — byte fidelity.
        } else {
            // C `sCalcoutRecord.c:366` `else pcalc->udf = FALSE`: a successful
            // fetch+calc DEFINES VAL/SVAL. This hook is the SINGLE owner of
            // `common.udf` — [`Self::clears_udf`] is false, so the framework
            // never re-derives it from VAL and never collapses the put byte.
            common.udf = 0;
        }
    }

    /// sCalcout does NOT re-derive UDF from VAL each cycle: C `pcalc->udf` is
    /// cleared only inside the fetch gate on a successful `sCalcPerform`
    /// (`sCalcoutRecord.c:366`) and left otherwise — never recomputed from the
    /// value. Returning `false` makes [`Record::check_alarms`] the single owner
    /// of `common.udf` (the C post-calc tail); the framework's per-cycle `udf =
    /// value_is_undefined()` re-derivation is suppressed, so a direct `caput UDF
    /// <byte>` keeps its raw `DBF_UCHAR` byte (`255` for `-1`) instead of
    /// collapsing to 0/1. A default record (empty CALC, which C's `sCalcPerform`
    /// fails every cycle) reads UDF=1 — the C-parity divergence the oracle
    /// measured.
    fn clears_udf(&self) -> bool {
        false
    }

    /// C `sCalcoutRecord.c:705` guards `checkAlarms` with `if (pcalc->udf ==
    /// TRUE)` — EXACT-ONE. With [`Self::clears_udf`] false the byte can hold a
    /// `caput`-supplied value other than 0/1, so this must match C's `== TRUE`
    /// (1): a byte of `255` is not undefined and raises no UDF_ALARM.
    fn udf_alarm_on_exact_one(&self) -> bool {
        true
    }

    /// scalcout writes its computed output to the `OUT` link. The
    /// framework's generic multi-output dispatch reads the `OUT` field
    /// for the link string and `OVAL` for the value. Gated on the last
    /// cycle's OOPT decision (`cached_should_output`) so a
    /// condition-not-met cycle does not write the OUT link. Previously
    /// `OUT` was stored but never written — the scalcout output side
    /// was a dead feature.
    fn multi_output_links(&self) -> &[(&'static str, &'static str)] {
        if self.cached_should_output {
            &[("OUT", "OVAL")]
        } else {
            &[]
        }
    }

    /// C `devsCalcoutSoft.c::write_scalcout` (66-144) routes the OUT put by
    /// the TARGET field's DBF type — the staged `OVAL` is only one of three
    /// buffers:
    ///
    /// - `DBF_STRING`/`ENUM`/`MENU`/`DEVICE`/`INLINK`/`OUTLINK`/`FWDLINK`
    ///   (:83-89, :131-134) — `DBR_STRING` from `OSV`, the OCAL string
    ///   result. All seven classes are reported by
    ///   [`OutTarget::puts_as_string`], which the target resolution settles:
    ///   the port's `DbFieldType` is a DBR wire type and has no `Menu` or
    ///   `Device` variant, so matching on it alone missed every menu target
    ///   (`PRIO`, `STAT`, `SEVR`, `DISS`, `ACKT`, …) and `DTYP`.
    /// - `DBF_CHAR`/`DBF_UCHAR` with `n_elements > 1` (:92-96, :136-138) —
    ///   `DBF_CHAR` array of `min(n_elements, sizeof(sval)) = min(n, 40)`
    ///   bytes. C reads that buffer from `OSV` on the ASYNCHRONOUS branch
    ///   (CA link + `WAIT`, :94) but from `SVAL` — the CALC string, not the
    ///   OCAL one — on the synchronous branch (:137). The asymmetry is C's;
    ///   it is reproduced, not repaired.
    /// - anything else, including an unresolved (disconnected CA) target
    ///   whose `dbCaGetLinkDBFtype` returned `-1` (:99, :140) —
    ///   `DBR_DOUBLE` from `OVAL`, the staged value.
    fn multi_output_buffer(
        &self,
        link_field: &str,
        staged: EpicsValue,
        target: &OutTarget,
    ) -> EpicsValue {
        if link_field != "OUT" {
            return staged;
        }
        if target.puts_as_string {
            return EpicsValue::String(self.osv.clone());
        }
        match target.field_type {
            Some(DbFieldType::Char) | Some(DbFieldType::UChar) if target.element_count > 1 => {
                // `n_elements > sizeof(pscalcout->sval)` clamp — the C
                // buffers are `char[SCALC_STRING_SIZE]` (40).
                let n = target.element_count.min(SCALC_STRING_SIZE as i64) as usize;
                // C puts `n` bytes straight out of the 40-byte field, so the
                // string is NUL-padded out to the requested count.
                let async_put = target.is_ca_link && self.wait != 0;
                let src = if async_put { &self.osv } else { &self.sval };
                let mut buf = src.as_bytes().to_vec();
                buf.resize(n, 0);
                EpicsValue::CharArray(buf)
            }
            _ => staged,
        }
    }

    /// `OEVT` ("Event To Issue"): post the numeric output event when output
    /// fires. C `sCalcoutRecord.c` `execOutput` does `if (pcalc->oevt > 0)
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

    /// Record-specific `DBF_MENU` fields, served as `DBR_ENUM` with the
    /// menu's choice labels in `.dbd` index order (`sCalcoutRecord.dbd`):
    /// `OOPT` is `menu(scalcoutOOPT)`, `DOPT` is `menu(scalcoutDOPT)`, `WAIT`
    /// is `menu(scalcoutWAIT)`. `IVOA` is a shared menu resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "OOPT" => Some(SCALCOUT_OOPT_CHOICES),
            "DOPT" => Some(SCALCOUT_DOPT_CHOICES),
            "WAIT" => Some(SCALCOUT_WAIT_CHOICES),
            _ if Self::is_link_status_field(field) => Some(LINK_STATUS_CHOICES),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_status_fields_read_derived_constant_and_reject_put() {
        use crate::server::record::RecordInstance;
        // INAV..INLV / IAAV..ILLV / OUTV are `special(SPC_NOMOD)`, DERIVED from
        // the link. A default record's links are all constant, so C reports
        // `Constant`(3). Before this fix these fields were declared but
        // un-modeled, so `resolve_field` fell through to `declared_default` and
        // served the dbd `initial("1")` (`Ext PV OK`) / OUTV's `initial` — the
        // C-parity divergence the oracle measured (`caget SCALCOUT.INAV` → 1,
        // C → 3). The put is already refused by the framework read-only gate
        // (`is_no_mod`), matching C `S_db_noMod`; the fix is the read value.
        let inst = RecordInstance::new("S:LS".into(), ScalcoutRecord::new());
        for name in ["INAV", "INLV", "IAAV", "ILLV", "OUTV"] {
            assert_eq!(
                inst.resolve_field(name),
                Some(EpicsValue::Enum(LINK_CON as u16)),
                "{name} should read derived Constant(3), not the dbd initial"
            );
            assert!(
                inst.is_no_mod(name),
                "{name} is SPC_NOMOD — a client put must be refused"
            );
            assert_eq!(
                inst.record.menu_field_choices(name),
                Some(LINK_STATUS_CHOICES),
                "{name} exposes the link-status choice labels"
            );
        }
    }

    /// VERS is the code-version constant (C `sCalcoutRecord.c:55 #define
    /// VERSION 4.1`, written `pcalc->vers = VERSION` in init_record), NOT the
    /// `.dbd` `initial("1")`. A write is accepted-and-ignored, matching
    /// acalcout.
    #[test]
    fn vers_is_the_version_constant_and_ignores_writes() {
        let mut rec = ScalcoutRecord::new();
        assert_eq!(rec.get_field("VERS"), Some(EpicsValue::Double(4.1)));
        assert!(rec.put_field("VERS", EpicsValue::Double(99.0)).is_ok());
        assert_eq!(rec.get_field("VERS"), Some(EpicsValue::Double(4.1)));
    }

    /// `OSV` ("Output string value") is `DBF_STRING`, not a severity menu: the
    /// name-based `shared_menu_choices` map lists `OSV` as `menuAlarmSevr` for
    /// the bi/bo severity field of the same name, but the declared type must
    /// win so a `caput SCALCOUT.OSV <string>` is not rejected as a bad choice.
    #[test]
    fn osv_is_a_string_field_not_a_severity_menu() {
        use crate::server::record::record_instance::menu_choices_of;
        let rec = ScalcoutRecord::new();
        assert_eq!(
            menu_choices_of(&rec, "OSV"),
            None,
            "OSV is DBF_STRING — the name-based severity menu must not apply"
        );
        // A real Enum severity field still resolves via the shared map, and
        // scalcout's own declared menus are unaffected.
        assert!(menu_choices_of(&rec, "HHSV").is_some());
        assert!(menu_choices_of(&rec, "OOPT").is_some());
    }

    /// UDF (C `pcalc->udf`) is cleared ONLY by a successful calc and is owned by
    /// `check_alarms` (C's post-calc tail), NOT re-derived from VAL. C
    /// `sCalcoutRecord.c:356-366` sets `udf=FALSE` only inside the fetch gate on
    /// a successful `sCalcPerform` and never re-raises it. The oracle measured
    /// `SCALCOUT.UDF` C=1, port=0 for the default (empty-CALC) record, because
    /// the framework's `isnan(VAL=0.0)` default reported 0.
    #[test]
    fn udf_cleared_only_by_a_successful_calc() {
        use crate::server::record::CommonFields;
        let mut rec = ScalcoutRecord::new();
        let mut common = CommonFields::default();
        // Init: undefined, mirroring C `iocInit` udf=TRUE.
        assert_eq!(common.udf, 1, "fresh common is undefined (init 1)");
        // Empty CALC fails sCalcPerform every cycle → UDF stays set (C keeps 1).
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
        // Monotonic: a later FAILED calc (empty CALC) does NOT re-raise UDF —
        // C raises CALC_ALARM (`if(stat)`) and leaves `udf` at FALSE.
        rec.put_field("CALC", EpicsValue::String("".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.process().unwrap();
        rec.check_alarms(&mut common);
        assert_eq!(
            common.udf, 0,
            "a failed calc after a success leaves UDF cleared, matching C"
        );
    }

    /// A failing calc leaves `common.udf` UNTOUCHED — so a direct `caput UDF
    /// <byte>` keeps its raw `DBF_UCHAR` byte across the `pp(TRUE)` re-process
    /// (`255` for `-1`, served signed). C `sCalcoutRecord.c:356-366` clears udf
    /// only in the `else` (success) arm; the oracle measured `caput UDF -1/255`
    /// → C=-1, port=1 because the wave-3 boolean cell collapsed every nonzero
    /// byte to 1.
    #[test]
    fn failing_calc_leaves_the_udf_byte_untouched() {
        use crate::server::record::CommonFields;
        let mut rec = ScalcoutRecord::new(); // empty CALC → fails every cycle
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

    /// R9-74 (family): OOPT="On Change" is the numeric MDEL deadband test
    /// `fabs(pval - val) > mdel` (C `sCalcoutRecord.c:379`). A numeric change
    /// that stays inside MDEL must NOT drive OUT.
    #[test]
    fn r9_74_scalcout_on_change_honours_mdel_deadband() {
        let mut rec = ScalcoutRecord::new();
        rec.put_field("CALC", EpicsValue::String("A".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("OOPT", EpicsValue::Short(1)).unwrap();
        rec.put_field("MDEL", EpicsValue::Double(2.0)).unwrap();

        // The decision a cycle made is `multi_output_links()` — C advances
        // `pval = val` inside the cycle (`:397`), so re-running the OOPT test
        // afterwards would only ever compare VAL against itself.
        rec.put_field("A", EpicsValue::Double(1.0)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, 1.0);
        assert!(
            rec.multi_output_links().is_empty(),
            "|pval - val| = 1.0 is inside MDEL=2.0 — C does not drive OUT"
        );

        rec.put_field("A", EpicsValue::Double(5.0)).unwrap();
        rec.process().unwrap();
        assert!(
            !rec.multi_output_links().is_empty(),
            "|1.0 - 5.0| = 4.0 exceeds MDEL=2.0 — C drives OUT"
        );
    }

    /// R9-74 (family): SVAL takes no part in the OOPT="On Change" test — C's
    /// switch (`sCalcoutRecord.c:378-380`) compares only PVAL against VAL. A
    /// cycle whose only change is the string result leaves VAL at 0.0 and must
    /// not drive OUT even with the default MDEL=0.
    #[test]
    fn r9_74_scalcout_on_change_ignores_string_result_change() {
        let mut rec = ScalcoutRecord::new();
        rec.put_field("CALC", EpicsValue::String("AA".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("OOPT", EpicsValue::Short(1)).unwrap();

        rec.put_field("AA", EpicsValue::String("first".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(rec.sval, "first");
        assert_eq!(rec.val, 0.0, "a non-numeric string result converts to 0.0");

        rec.put_field("AA", EpicsValue::String("second".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(rec.sval, "second");
        assert!(
            rec.multi_output_links().is_empty(),
            "SVAL changed but VAL did not — C's On-Change test is numeric only"
        );
    }

    #[test]
    fn test_scalcout_default() {
        let rec = ScalcoutRecord::new();
        assert_eq!(rec.record_type(), "scalcout");
        assert_eq!(rec.val, 0.0);
        assert_eq!(rec.sval, "");
    }

    /// The CALC `VAL` token reads the previous VAL (C `sCalcoutRecord.c:357`
    /// passes `presult = &pcalc->val`), so `CALC="VAL+1"` counts up.
    #[test]
    fn r5_2_sibling_calc_val_token_reads_previous_val() {
        let mut rec = ScalcoutRecord::new();
        rec.put_field("CALC", EpicsValue::String("VAL+1".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, 1.0);
        rec.process().unwrap();
        assert_eq!(rec.val, 2.0);
        rec.process().unwrap();
        assert_eq!(rec.val, 3.0);
    }

    /// The OCAL `VAL` token reads the previous OVAL, not the VAL this cycle just
    /// computed (C `sCalcoutRecord.c:768-769` passes `presult = &pcalc->oval`).
    #[test]
    fn r5_2_sibling_ocal_val_token_reads_previous_oval() {
        let mut rec = ScalcoutRecord::new();
        // CALC pins VAL at 100 every cycle; if OCAL's VAL token read VAL, OVAL
        // would be 101 forever.
        rec.put_field("CALC", EpicsValue::String("100".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("OCAL", EpicsValue::String("VAL+1".into()))
            .unwrap();
        rec.special("OCAL", true).unwrap();
        rec.put_field("DOPT", EpicsValue::Short(1)).unwrap(); // Use OCAL
        rec.put_field("OOPT", EpicsValue::Short(0)).unwrap(); // Every Time

        rec.process().unwrap();
        assert_eq!(rec.val, 100.0);
        assert_eq!(rec.oval, 1.0, "OCAL VAL token = previous OVAL (0) + 1");
        rec.process().unwrap();
        assert_eq!(rec.oval, 2.0, "previous OVAL (1) + 1");
        rec.process().unwrap();
        assert_eq!(rec.oval, 3.0, "previous OVAL (2) + 1");
        assert_eq!(rec.val, 100.0, "VAL is untouched by OCAL");
    }

    #[test]
    fn test_scalcout_numeric_calc() {
        let mut rec = ScalcoutRecord::new();
        rec.put_field("A", EpicsValue::Double(3.0)).unwrap();
        rec.put_field("B", EpicsValue::Double(4.0)).unwrap();
        rec.put_field("CALC", EpicsValue::String("A+B".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, 7.0);
    }

    #[test]
    fn test_scalcout_string_calc() {
        let mut rec = ScalcoutRecord::new();
        rec.put_field("AA", EpicsValue::String("hello".into()))
            .unwrap();
        rec.put_field("BB", EpicsValue::String(" world".into()))
            .unwrap();
        rec.put_field("CALC", EpicsValue::String("AA+BB".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.sval, "hello world");
    }

    #[test]
    fn test_scalcout_oopt_every() {
        let mut rec = ScalcoutRecord::new();
        rec.put_field("CALC", EpicsValue::String("42".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("OOPT", EpicsValue::Short(0)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.oval, 42.0);
    }

    #[test]
    fn test_scalcout_oopt_on_change() {
        let mut rec = ScalcoutRecord::new();
        rec.put_field("CALC", EpicsValue::String("A".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("OOPT", EpicsValue::Short(1)).unwrap();

        // First process — value changes from 0 to 5
        rec.put_field("A", EpicsValue::Double(5.0)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.oval, 5.0);

        // Second process — no change
        rec.process().unwrap();
        // OVAL stays the same since it's "On Change" and nothing changed
        assert_eq!(rec.oval, 5.0);
    }

    #[test]
    fn test_scalcout_dopt_use_ocal() {
        let mut rec = ScalcoutRecord::new();
        rec.put_field("A", EpicsValue::Double(10.0)).unwrap();
        rec.put_field("CALC", EpicsValue::String("A".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("OCAL", EpicsValue::String("A*2".into()))
            .unwrap();
        rec.special("OCAL", true).unwrap();
        rec.put_field("DOPT", EpicsValue::Short(1)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, 10.0); // CALC result
        assert_eq!(rec.oval, 20.0); // OCAL result
    }

    #[test]
    fn test_scalcout_string_vars() {
        let mut rec = ScalcoutRecord::new();
        rec.put_field("AA", EpicsValue::String("test".into()))
            .unwrap();
        assert_eq!(rec.get_field("AA"), Some(EpicsValue::String("test".into())));
        rec.put_field("LL", EpicsValue::String("last".into()))
            .unwrap();
        assert_eq!(rec.get_field("LL"), Some(EpicsValue::String("last".into())));
    }

    #[test]
    fn test_scalcout_field_not_found() {
        let mut rec = ScalcoutRecord::new();
        assert!(rec.put_field("ZZZ", EpicsValue::Double(1.0)).is_err());
        assert!(rec.get_field("ZZZ").is_none());
    }

    #[test]
    fn test_scalcout_ocal_string() {
        let mut rec = ScalcoutRecord::new();
        rec.put_field("AA", EpicsValue::String("hi".into()))
            .unwrap();
        rec.put_field("CALC", EpicsValue::String("1".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("OCAL", EpicsValue::String("AA".into()))
            .unwrap();
        rec.special("OCAL", true).unwrap();
        rec.put_field("DOPT", EpicsValue::Short(1)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.osv, "hi");
    }

    #[test]
    fn test_scalcout_ivoa_dont_drive() {
        let mut rec = ScalcoutRecord::new();
        // Use an expression that will fail to compile
        rec.calc = "???invalid".into();
        rec.compiled_calc = CompiledExpr::empty(ExprKind::String);
        rec.put_field("IVOA", EpicsValue::Short(1)).unwrap();
        rec.put_field("OUT", EpicsValue::String("sink.VAL".into()))
            .unwrap();
        rec.process().unwrap();
        // Don't_drive suppresses only the OUT *write* — multi_output_links is
        // empty (cached_should_output false).
        assert!(
            rec.multi_output_links().is_empty(),
            "IVOA=Don't_drive suppresses the OUT write"
        );
        // C `execOutput` still runs the DOPT switch on this output cycle
        // (sCalcoutRecord.c:760-777, before the Don't_drive break at :795), so
        // OVAL is recomputed from VAL (=-1 calc-fail sentinel), NOT left stale.
        assert_eq!(
            rec.oval, -1.0,
            "Don't_drive recomputes OVAL from the calc-fail VAL=-1, not 0"
        );
    }

    #[test]
    fn scalcout_ivoa_dont_drive_still_delays_via_odly() {
        // R47 gap: calc-fail + IVOA=Don't_drive + OOPT-fires + ODLY>0. C
        // schedules the ODLY delay regardless of IVOA (sCalcoutRecord.c:399-408
        // is upstream of execOutput, where the Don't_drive veto applies at
        // :430 on the continuation) — so the record still pulses DLYA 1→0 and
        // fires FLNK on the continuation; only the OUT write is suppressed. It
        // must NOT complete immediately as the old IVOA==1 early-return did.
        let mut rec = ScalcoutRecord::new();
        rec.calc = "???invalid".into();
        rec.compiled_calc = CompiledExpr::empty(ExprKind::String); // calc fails
        rec.put_field("IVOA", EpicsValue::Short(1)).unwrap(); // Don't drive
        rec.put_field("OUT", EpicsValue::String("sink.VAL".into()))
            .unwrap();
        rec.put_field("ODLY", EpicsValue::Double(0.05)).unwrap();
        // OOPT=0 (Every): output fires.
        let outcome = rec.process().unwrap();
        // Delaying cycle: DLYA set, ReprocessAfter scheduled, OUT suppressed.
        assert_eq!(
            rec.get_field("DLYA"),
            Some(EpicsValue::Short(1)),
            "Don't_drive must still delay: DLYA set"
        );
        assert!(
            rec.multi_output_links().is_empty(),
            "Don't_drive suppresses the OUT write on the delaying cycle"
        );
        assert!(
            outcome
                .actions
                .iter()
                .any(|a| matches!(a, ProcessAction::ReprocessAfter(_))),
            "Don't_drive + ODLY>0 schedules the delayed re-process"
        );
        // Continuation: DLYA clears; OUT stays unwritten (the veto holds).
        rec.process().unwrap();
        assert_eq!(
            rec.get_field("DLYA"),
            Some(EpicsValue::Short(0)),
            "DLYA cleared on the continuation"
        );
        assert!(
            rec.multi_output_links().is_empty(),
            "Don't_drive: OUT stays unwritten on the continuation"
        );
    }

    #[test]
    fn test_scalcout_ivoa_set_ivov() {
        let mut rec = ScalcoutRecord::new();
        // Empty calc → no error path, just test the field storage
        rec.put_field("IVOA", EpicsValue::Short(2)).unwrap();
        rec.put_field("IVOV", EpicsValue::Double(99.0)).unwrap();
        assert_eq!(rec.get_field("IVOA"), Some(EpicsValue::Short(2)));
        assert_eq!(rec.get_field("IVOV"), Some(EpicsValue::Double(99.0)));
    }

    #[test]
    fn scalcout_calc_fail_sets_error_sentinel() {
        // C `sCalcoutRecord.c:361-363`: a failed sCalcPerform forces VAL=-1
        // and SVAL="***ERROR***". Start VAL from a known-good value so the
        // sentinel is unambiguous (not just the default 0).
        let mut rec = ScalcoutRecord::new();
        rec.put_field("CALC", EpicsValue::String("A".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("A", EpicsValue::Double(7.0)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, 7.0, "good calc seeds VAL");
        // Now break the CALC: a bare binary operator underflows the stack.
        rec.put_field("CALC", EpicsValue::String("+".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, -1.0, "calc-fail forces VAL=-1 (C:361)");
        assert_eq!(rec.sval, "***ERROR***", "calc-fail forces SVAL (C:363)");
    }

    #[test]
    fn scalcout_ocal_fail_sets_oval_error_sentinel() {
        // C execOutput Use_OVAL (sCalcoutRecord.c:771-773): a failed OCAL
        // sCalcPerform forces OVAL=-1 and OSV="***ERROR***" — the OCAL-side
        // mirror of the CALC-fail VAL sentinel. CALC itself stays valid here,
        // so only the OCAL/OVAL side carries the sentinel.
        let mut rec = ScalcoutRecord::new();
        rec.put_field("CALC", EpicsValue::String("A".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("A", EpicsValue::Double(3.0)).unwrap();
        rec.put_field("DOPT", EpicsValue::Short(1)).unwrap(); // Use OCAL
        rec.put_field("OCAL", EpicsValue::String("5".into()))
            .unwrap();
        rec.special("OCAL", true).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.oval, 5.0, "good OCAL seeds OVAL");
        assert_eq!(rec.val, 3.0, "CALC side is unaffected");
        // Break OCAL: a bare binary operator underflows the stack.
        rec.put_field("OCAL", EpicsValue::String("+".into()))
            .unwrap();
        rec.special("OCAL", true).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.oval, -1.0, "OCAL-fail forces OVAL=-1 (C:771)");
        assert_eq!(rec.osv, "***ERROR***", "OCAL-fail forces OSV (C:772)");
        // VAL stays at the good CALC result — the sentinel is OCAL-side only.
        assert_eq!(
            rec.val, 3.0,
            "OCAL-fail must NOT touch VAL (C sets oval, not val)"
        );
    }

    /// A scalcout whose CALC yields SVAL="calc-str" and whose OCAL yields
    /// OSV="ocal-str", with OVAL=7 — the three C write buffers all distinct,
    /// so `multi_output_buffer` cannot pass a boundary by coincidence.
    fn three_buffer_scalcout() -> ScalcoutRecord {
        let mut rec = ScalcoutRecord::new();
        rec.put_field("CALC", EpicsValue::String("AA".into()))
            .unwrap();
        rec.special("CALC", true).unwrap();
        rec.put_field("AA", EpicsValue::String("calc-str".into()))
            .unwrap();
        rec.put_field("DOPT", EpicsValue::Short(1)).unwrap(); // Use OCAL
        rec.put_field("OCAL", EpicsValue::String("BB".into()))
            .unwrap();
        rec.special("OCAL", true).unwrap();
        rec.put_field("BB", EpicsValue::String("ocal-str".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(rec.sval, "calc-str");
        assert_eq!(rec.osv, "ocal-str");
        rec.oval = 7.0;
        rec
    }

    /// A target as `PvDatabase::resolve_out_target` would report it for a
    /// STRING/ENUM/numeric field: those two DBR types are in C's string class
    /// on their own. A `DBF_MENU`/`DBF_DEVICE` target is NOT expressible as a
    /// `DbFieldType` — see [`out_target_menu`].
    fn out_target(
        field_type: Option<DbFieldType>,
        element_count: i64,
        is_ca_link: bool,
    ) -> OutTarget {
        OutTarget {
            field_type,
            element_count,
            is_ca_link,
            puts_as_string: matches!(
                field_type,
                Some(DbFieldType::String) | Some(DbFieldType::Enum)
            ),
        }
    }

    /// A `DBF_MENU` target (`PRIO`, `STAT`, `SEVR`, `DISS`, `ACKT`, …) as the
    /// resolver reports it: the menu INDEX is a short, so `field_type` is
    /// `Short` — nothing in the DBR type says "menu". The class comes from
    /// `puts_as_string`, which the target record settles.
    fn out_target_menu() -> OutTarget {
        OutTarget {
            field_type: Some(DbFieldType::Short),
            element_count: 1,
            is_ca_link: false,
            puts_as_string: true,
        }
    }

    /// R14-61 boundary: a `DBF_STRING` target takes the computed string OSV
    /// (C `devsCalcoutSoft.c:83-89` / `:131-134` — `DBR_STRING` from `&osv`),
    /// never the numeric OVAL.
    #[test]
    fn r14_61_string_target_gets_osv() {
        let rec = three_buffer_scalcout();
        let staged = EpicsValue::Double(rec.oval);
        assert_eq!(
            rec.multi_output_buffer(
                "OUT",
                staged,
                &out_target(Some(DbFieldType::String), 1, false)
            ),
            EpicsValue::String("ocal-str".into())
        );
    }

    /// A `DBF_ENUM`/`MENU` target is in the same C case list as `DBF_STRING`
    /// — the enum's choice label is matched from the string.
    #[test]
    fn r14_61_enum_target_gets_osv() {
        let rec = three_buffer_scalcout();
        let staged = EpicsValue::Double(rec.oval);
        assert_eq!(
            rec.multi_output_buffer(
                "OUT",
                staged,
                &out_target(Some(DbFieldType::Enum), 1, false)
            ),
            EpicsValue::String("ocal-str".into())
        );
    }

    /// R15-64 boundary: a `DBF_MENU` / `DBF_DEVICE` target — `PRIO`, `STAT`,
    /// `SEVR`, `DISS`, `ACKT`, `DTYP` — is in C's `DBR_STRING` case list
    /// (`devsCalcoutSoft.c:128-130`) just like STRING and ENUM, and the port's
    /// DBR-typed `field_type` (a menu index is a `Short`) cannot say so. The
    /// class travels in `puts_as_string`; matching on `field_type` alone sent
    /// the numeric OVAL to every menu target.
    #[test]
    fn r15_64_menu_target_gets_osv() {
        let rec = three_buffer_scalcout();
        let staged = EpicsValue::Double(rec.oval);
        assert_eq!(
            rec.multi_output_buffer("OUT", staged, &out_target_menu()),
            EpicsValue::String("ocal-str".into()),
            "a DBF_MENU target takes DBR_STRING from OSV, not DBR_DOUBLE from OVAL"
        );
    }

    /// The complement: a plain numeric target still takes the staged OVAL —
    /// C's `default:` arm (`devsCalcoutSoft.c:140`).
    #[test]
    fn r15_64_numeric_target_still_gets_oval() {
        let rec = three_buffer_scalcout();
        let staged = EpicsValue::Double(rec.oval);
        assert_eq!(
            rec.multi_output_buffer(
                "OUT",
                staged,
                &out_target(Some(DbFieldType::Double), 1, false)
            ),
            EpicsValue::Double(7.0)
        );
    }

    /// R14-61 boundary: a CHAR array target on the SYNCHRONOUS branch takes
    /// SVAL — C `devsCalcoutSoft.c:137` puts `&pscalcout->sval` (the CALC
    /// string), not OSV. The bytes are the 40-byte C buffer's first
    /// `n_elements`, so the string is NUL-padded out to the target count.
    #[test]
    fn r14_61_char_array_target_sync_gets_sval_bytes() {
        let rec = three_buffer_scalcout();
        let staged = EpicsValue::Double(rec.oval);
        let mut want = b"calc-str".to_vec();
        want.resize(12, 0);
        assert_eq!(
            rec.multi_output_buffer(
                "OUT",
                staged,
                &out_target(Some(DbFieldType::Char), 12, false)
            ),
            EpicsValue::CharArray(want)
        );
    }

    /// R14-61 boundary: the ASYNCHRONOUS branch (CA link + WAIT) takes OSV
    /// instead — C `devsCalcoutSoft.c:94`. This asymmetry is C's.
    #[test]
    fn r14_61_char_array_target_async_gets_osv_bytes() {
        let mut rec = three_buffer_scalcout();
        rec.put_field("WAIT", EpicsValue::Short(1)).unwrap();
        let staged = EpicsValue::Double(rec.oval);
        let mut want = b"ocal-str".to_vec();
        want.resize(12, 0);
        assert_eq!(
            rec.multi_output_buffer(
                "OUT",
                staged,
                &out_target(Some(DbFieldType::Char), 12, true)
            ),
            EpicsValue::CharArray(want)
        );
    }

    /// WAIT only reaches the async branch on a CA link (C `:76`
    /// `plink->type == CA_LINK && pscalcout->wait`): WAIT=1 on a DB link stays
    /// synchronous, so the buffer is still SVAL.
    #[test]
    fn r14_61_char_array_target_wait_on_db_link_stays_sync() {
        let mut rec = three_buffer_scalcout();
        rec.put_field("WAIT", EpicsValue::Short(1)).unwrap();
        let staged = EpicsValue::Double(rec.oval);
        let mut want = b"calc-str".to_vec();
        want.resize(12, 0);
        assert_eq!(
            rec.multi_output_buffer(
                "OUT",
                staged,
                &out_target(Some(DbFieldType::Char), 12, false)
            ),
            EpicsValue::CharArray(want)
        );
    }

    /// R14-61 boundary: `n_elements > sizeof(sval)` clamps to 40
    /// (C `:91` / `:136`).
    #[test]
    fn r14_61_char_array_target_clamps_to_40() {
        let rec = three_buffer_scalcout();
        let staged = EpicsValue::Double(rec.oval);
        let out = rec.multi_output_buffer(
            "OUT",
            staged,
            &out_target(Some(DbFieldType::UChar), 100, false),
        );
        match out {
            EpicsValue::CharArray(b) => assert_eq!(b.len(), 40),
            other => panic!("expected a CHAR array, got {other:?}"),
        }
    }

    /// A CHAR *scalar* target (`n_elements == 1`) fails C's `n_elements > 1`
    /// test and falls through to the `DBR_DOUBLE` put of OVAL (`:96-99`).
    #[test]
    fn r14_61_char_scalar_target_gets_oval() {
        let rec = three_buffer_scalcout();
        let staged = EpicsValue::Double(rec.oval);
        assert_eq!(
            rec.multi_output_buffer(
                "OUT",
                staged.clone(),
                &out_target(Some(DbFieldType::Char), 1, false)
            ),
            staged
        );
    }

    /// A numeric target takes the staged OVAL (`DBR_DOUBLE`, C `:140`).
    #[test]
    fn r14_61_numeric_target_gets_oval() {
        let rec = three_buffer_scalcout();
        let staged = EpicsValue::Double(rec.oval);
        assert_eq!(
            rec.multi_output_buffer(
                "OUT",
                staged.clone(),
                &out_target(Some(DbFieldType::Double), 1, false)
            ),
            staged
        );
    }

    /// A disconnected CA target reports no type (`dbCaGetLinkDBFtype` → -1,
    /// `dbCa.c:695-704`), which in C leaves the `default:` arm — `DBR_DOUBLE`
    /// from OVAL.
    #[test]
    fn r14_61_disconnected_ca_target_gets_oval() {
        let rec = three_buffer_scalcout();
        let staged = EpicsValue::Double(rec.oval);
        assert_eq!(
            rec.multi_output_buffer(
                "OUT",
                staged.clone(),
                &OutTarget {
                    is_ca_link: true,
                    ..OutTarget::UNRESOLVED
                }
            ),
            staged
        );
    }
}
