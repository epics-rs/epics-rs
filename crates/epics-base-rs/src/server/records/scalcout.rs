use super::calc_compile;
use crate::error::{CaError, CaResult};
use crate::server::record::{
    FieldDesc, InputFetchPolicy, ProcessAction, ProcessOutcome, Record, RecordProcessResult,
};
use crate::types::{DbFieldType, EpicsValue, PvString};

use crate::calc::StringInputs;
use crate::calc::engine::value::{ScalcString, StackValue};
use crate::calc::{CompiledExpr, ExprKind, scalc_eval, scalc_result};

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
    // Numeric input values A-L (mapped to vars A-P, but only 12 used)
    pub num_vals: [f64; 12],
    // String input values AA-LL
    pub str_vals: [PvString; 12],
    // Previous value for transition detection
    prev_val: f64,
    prev_sval: PvString,
    /// CALC_ALARM flag — set when the CALC or OCAL `sCalcPerform`
    /// evaluation fails. synApps `sCalcoutRecord` raises `CALC_ALARM`
    /// on a broken expression; the framework's `evaluate_alarms`
    /// already inspects a `CALC_ALARM` field for `scalcout`, so this
    /// flag is surfaced through `get_field("CALC_ALARM")`.
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
            num_vals: [0.0; 12],
            str_vals: Default::default(),
            prev_val: 0.0,
            prev_sval: PvString::new(),
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
        let mut inputs = StringInputs::new();
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

    /// C `sCalcoutRecord.c:357-359` — `sCalcPerform(..., &pcalc->val,
    /// pcalc->sval, ...)`: the record hands the engine the two cells and the
    /// engine fills both ([`scalc_result`]). VAL and SVAL are two views of ONE
    /// result, not two computations.
    fn apply_result(&mut self, result: &StackValue) {
        let (val, sval) = scalc_result(result);
        self.val = val;
        self.sval = PvString::from_bytes(sval.as_bytes());
    }

    /// C `sCalcoutRecord.c:374-395` — the OOPT switch. "On Change" is the
    /// numeric MDEL-deadband test `fabs(pcalc->pval - pcalc->val) > pcalc->mdel`
    /// (:379) and nothing else: SVAL does not take part, so a cycle that changed
    /// only the string result does NOT drive OUT on C, and a numeric change
    /// inside MDEL does not either.
    fn should_output(&self) -> bool {
        match self.oopt {
            0 => true,
            1 => (self.prev_val - self.val).abs() > self.mdel,
            2 => self.val == 0.0,
            3 => self.val != 0.0,
            4 => self.prev_val != 0.0 && self.val == 0.0,
            5 => self.prev_val == 0.0 && self.val != 0.0,
            _ => true,
        }
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
}

static SCALCOUT_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "SVAL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CALC",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    // sCalcoutRecord.dbd:75,438 — `field(CLCV,DBF_LONG)` / `field(OCLV,DBF_LONG)`.
    FieldDesc {
        name: "CLCV",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "OCLV",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "OOPT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DOPT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "OCAL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OVAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "OSV",
        dbf_type: DbFieldType::String,
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
    FieldDesc {
        name: "PREC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "MDEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "ADEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "ODLY",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DLYA",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "OEVT",
        dbf_type: DbFieldType::UShort,
        read_only: false,
    },
    // Input links
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
    // Numeric vars A-L
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
    // String vars AA-LL
    FieldDesc {
        name: "AA",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "BB",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CC",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "DD",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "EE",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "FF",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "GG",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "HH",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "II",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "JJ",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "KK",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "LL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
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
    fn record_type(&self) -> &'static str {
        "scalcout"
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
        Ok(())
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
        // branch, lines 421-432). Do NOT re-evaluate CALC / OCAL / should_output
        // — C clears DLYA and runs `execOutput` directly. Honour the output
        // decision the original cycle captured, clear DLYA, and let the
        // framework write the OUT link. Mirrors calcout.rs.
        if self.dlya == 1 {
            self.dlya = 0;
            self.cached_should_output = self.pending_output;
            self.pending_output = false;
            return Ok(ProcessOutcome::complete());
        }

        self.prev_val = self.val;
        self.prev_sval = self.sval.clone();

        // Evaluate CALC. A fresh cycle clears CALC_ALARM; a broken CALC
        // (compile failure OR an sCalcPerform eval failure) re-raises
        // it. synApps `sCalcoutRecord` raises CALC_ALARM on any broken
        // expression; without this a failing scalcout expression
        // silently kept the previous cycle's VAL/SVAL with no invalid
        // indication.
        self.calc_alarm = false;

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
        let calc_failed = if self.fetch_gate_failed {
            false
        } else {
            // C `sCalcoutRecord.c:357-359` — presult = &pcalc->val,
            // psresult = pcalc->sval: VAL/SVAL both read this cycle's
            // pre-evaluation values (captured in prev_val/prev_sval above).
            let mut inputs = self.build_inputs(self.val, &self.sval);
            match scalc_eval(&self.compiled_calc, &mut inputs) {
                Ok(result) => {
                    self.apply_result(&result);
                    false
                }
                Err(_) => true,
            }
        };

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
            // is raised by the framework from the CALC_ALARM field). Before
            // this the failed cycle kept the previous VAL/SVAL with no value
            // sentinel, diverging from C.
            self.val = -1.0;
            self.sval = PvString::from("***ERROR***");
            // IVOA on the INVALID cycle. C applies it inside `execOutput`
            // (sCalcoutRecord.c:786-808): Don't_drive skips the write,
            // Set_to_IVOV sets `oval = ivov` (line 798) — NOT `val`. Only the
            // Don't_drive veto needs an in-record flag here; the OVAL=IVOV
            // substitution is owned by the framework's IVOA gate
            // (`apply_invalid_output_value`), which fires because the
            // CALC_ALARM the framework raises in `evaluate_alarms` drives this
            // cycle INVALID. Setting `self.val = ivov` here was wrong: it
            // clobbered VAL (C keeps VAL=-1) and duplicated the framework's
            // OVAL write.
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
        // C `execOutput` (sCalcoutRecord.c:760-777) computes OVAL/OSV via the
        // DOPT switch on EVERY output cycle, *before* the IVOA decision (the
        // Don't_drive `break` is at :795). So OVAL is recomputed even when the
        // OUT write is vetoed — gate this on `oopt_fires`, not `write_out`.
        // (`write_out` still gates the OUT write below via cached_should_output;
        // on every non-Don't_drive path the two are equal, so OVAL is unchanged
        // there.)
        if oopt_fires {
            if self.dopt == 1 {
                // Use OCAL. C `execOutput` (sCalcoutRecord.c:768) calls
                // sCalcPerform on ORPC unconditionally on this branch, so an
                // empty, uncompilable or failing OCAL are one case — the empty
                // program fails like any other broken one.
                //
                // C `sCalcoutRecord.c:768-770` — presult = &pcalc->oval,
                // psresult = pcalc->osv, so the VAL/SVAL tokens in OCAL
                // read the previous OVAL/OSV, not the VAL/SVAL this
                // cycle just computed.
                let mut inputs = self.build_inputs(self.oval, &self.osv);
                match scalc_eval(&self.compiled_ocal, &mut inputs) {
                    Ok(result) => {
                        // The OCAL-side mirror of `apply_result`: C passes
                        // `&pcalc->oval, pcalc->osv` to the same sCalcPerform
                        // (`sCalcoutRecord.c:768-769`), so the same epilogue
                        // fills both cells.
                        let (oval, osv) = scalc_result(&result);
                        self.oval = oval;
                        self.osv = PvString::from_bytes(osv.as_bytes());
                    }
                    Err(_) => {
                        // C execOutput Use_OVAL (sCalcoutRecord.c:771-773):
                        // a failed OCAL sCalcPerform forces OVAL=-1 and
                        // OSV="***ERROR***" — the OCAL-side mirror of the
                        // CALC-fail VAL=-1 sentinel.
                        self.oval = -1.0;
                        self.osv = PvString::from("***ERROR***");
                        self.calc_alarm = true;
                    }
                }
            } else {
                // Use CALC result
                self.oval = self.val;
                self.osv = self.sval.clone();
            }
        }

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
            let delay = std::time::Duration::from_secs_f64(self.odly);
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
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "SVAL" => Some(EpicsValue::String(self.sval.clone())),
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
            "CALC_ALARM" => Some(EpicsValue::Char(if self.calc_alarm { 1 } else { 0 })),
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
                Err(CaError::FieldNotFound(name.to_string()))
            }
        }
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
    /// the status. (C's string-input loop that follows cannot fail the gate — it
    /// swallows its own errors into the input string and returns 0 — and the
    /// port does not fetch INAA..INLL at all, so only the numeric links are
    /// represented here.)
    fn input_fetch_policy(&self) -> InputFetchPolicy {
        InputFetchPolicy::AbortOnFirstFailure
    }

    fn set_fetch_gate_failed(&mut self, failed: bool) {
        self.fetch_gate_failed = failed;
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

    fn field_list(&self) -> &'static [FieldDesc] {
        SCALCOUT_FIELDS
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
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        rec.put_field("A", EpicsValue::Double(1.0)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, 1.0);
        assert!(
            !rec.should_output(),
            "|pval - val| = 1.0 is inside MDEL=2.0 — C does not drive OUT"
        );

        rec.put_field("A", EpicsValue::Double(5.0)).unwrap();
        rec.process().unwrap();
        assert!(
            rec.should_output(),
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
            !rec.should_output(),
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
}
