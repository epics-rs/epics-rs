use crate::error::{CaError, CaResult};
use crate::server::record::{
    AlarmSeverity, FieldDesc, ProcessAction, ProcessContext, ProcessOutcome, Record,
};
use crate::types::{DbFieldType, EpicsValue};

use crate::calc::{CompiledExpr, StringInputs, scalc_compile, scalc_eval, scalc_result};

const NUM_CHANNELS: usize = 16; // A-P

/// Transform record — 16 input/output channels (A-P), each with its own calc expression.
///
/// Processing: reads inputs via INPA-INPP links, evaluates CLCA-CLCP expressions
/// (each can reference all 16 variables A-P), stores results back into A-P,
/// then writes outputs via OUTA-OUTP links.
pub struct TransformRecord {
    /// `VAL` — a dummy. C `transformRecord.c:422` sets `ptran->val = 0` once in
    /// `init_record` ("Gotta have a .val field.  Make its value reproducible.")
    /// and NOTHING else ever touches it: `process()` iterates the channels from
    /// `&ptran->a`, `monitor()` posts from `&ptran->a`, and the calc loop writes
    /// `&ptran->a + i` — `->val` is never read, written or posted. It is a plain
    /// writable DBF_DOUBLE (`transformRecord.dbd:43`, no `special`), so a client
    /// put stores here and a later `caget .VAL` reads it back; that is the
    /// field's entire behaviour. Aliasing VAL to channel A (the port's previous
    /// shape) made `caget .VAL` return A and fired a `.VAL` monitor on every A
    /// change — neither happens on C.
    pub val: f64,
    pub vals: [f64; NUM_CHANNELS],
    pub calcs: [String; NUM_CHANNELS],
    compiled: [Option<CompiledExpr>; NUM_CHANNELS],
    pub inp_links: [String; NUM_CHANNELS],
    pub out_links: [String; NUM_CHANNELS],
    pub copt: i16, // calc option: 0=Conditional (calc only an unlinked, unchanged channel), 1=Always. Gates CALC-eval, NOT the OUTx write.
    pub ivla: i16, // 0=Ignore error, 1=Do Nothing
    pub prec: i16,
    /// Per-channel "value field A..P was written by a `put` since the
    /// last `process()`" flags. synApps `transformRecord` does not
    /// re-compute a channel whose value field was just `dbPut` this
    /// cycle ("don't overwrite a fresh put"). Set by `put_field` for the
    /// `A..P` value fields, cleared at the start of `process()`.
    fresh_put: [bool; NUM_CHANNELS],
    /// This cycle's pending input-link severity (`dbCommon.nsev`), pushed by
    /// the framework through [`Record::set_process_context`] before
    /// `process()` runs — C folds an MS-class link's severity into `nsev`
    /// inside `dbGetLink`, i.e. before the record body reads it
    /// (`transformRecord.c:554`).
    nsev: AlarmSeverity,
    /// `dbCommon.udf` as transform maintains it. C `transformRecord.c:521`
    /// clears `ptran->udf` at the top of every `process()` and sets it TRUE
    /// only where a channel's `sCalcPerform` fails (`:593-596`, alongside
    /// `recGblSetSevr(CALC_ALARM, INVALID_ALARM)`); `checkAlarms` (`:773-779`)
    /// then raises `UDF_ALARM` at `UDFS`. It is a per-cycle flag, not a
    /// property of any value — transform's VAL is an inert dummy (R9-62), so
    /// the framework's default `value_is_undefined()` (VAL is NaN) can never
    /// express it. Both [`Record::value_is_undefined`] and
    /// [`Record::check_alarms`] read this cell.
    calc_failed: bool,
}

impl Default for TransformRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            vals: [0.0; NUM_CHANNELS],
            calcs: Default::default(),
            compiled: Default::default(),
            inp_links: Default::default(),
            out_links: Default::default(),
            copt: 0,
            ivla: 0,
            prec: 0,
            fresh_put: [false; NUM_CHANNELS],
            nsev: AlarmSeverity::NoAlarm,
            calc_failed: false,
        }
    }
}

impl TransformRecord {
    pub fn new() -> Self {
        Self::default()
    }

    /// C `transformRecord.c` compiles every CLCx with **sCalcPostfix**, not
    /// base's `postfix()`: `POSTFIX_SIZE` is `SCALC_INFIX_TO_POSTFIX_SIZE(...)`
    /// (`:208`) and the evaluator is `sCalcPerform` (`:593`). The two engines
    /// are not interchangeable — they have different element tables, and, the
    /// reason this matters here, different failure rules (see the eval site in
    /// `process`).
    ///
    /// C's `postfix_ok = *pclcbuf && (*prpcbuf != BAD_EXPRESSION)` (`:585`): an
    /// EMPTY CLCx is not compiled and not evaluated — which is why the empty
    /// case is `None` rather than sCalc's empty-but-valid program.
    fn recompile(&mut self, idx: usize) {
        if self.calcs[idx].is_empty() {
            self.compiled[idx] = None;
        } else {
            self.compiled[idx] = scalc_compile(&self.calcs[idx]).ok();
        }
    }

    fn channel_index(name: &str) -> Option<usize> {
        if name.len() == 1 {
            let c = name.as_bytes()[0];
            if c >= b'A' && c <= b'P' {
                return Some((c - b'A') as usize);
            }
        }
        None
    }

    fn calc_field_index(name: &str) -> Option<usize> {
        if name.len() == 4 && name.starts_with("CLC") {
            let c = name.as_bytes()[3];
            if c >= b'A' && c <= b'P' {
                return Some((c - b'A') as usize);
            }
        }
        None
    }

    fn inp_field_index(name: &str) -> Option<usize> {
        if name.len() == 4 && name.starts_with("INP") {
            let c = name.as_bytes()[3];
            if c >= b'A' && c <= b'P' {
                return Some((c - b'A') as usize);
            }
        }
        None
    }

    fn out_field_index(name: &str) -> Option<usize> {
        if name.len() == 4 && name.starts_with("OUT") {
            let c = name.as_bytes()[3];
            if c >= b'A' && c <= b'P' {
                return Some((c - b'A') as usize);
            }
        }
        None
    }
}

static TRANSFORM_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "COPT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IVLA",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "PREC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    // CLCA-CLCP
    FieldDesc {
        name: "CLCA",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CLCB",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CLCC",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CLCD",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CLCE",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CLCF",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CLCG",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CLCH",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CLCI",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CLCJ",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CLCK",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CLCL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CLCM",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CLCN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CLCO",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CLCP",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    // INPA-INPP
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
        name: "INPM",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPO",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPP",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    // OUTA-OUTP
    FieldDesc {
        name: "OUTA",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTB",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTC",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTD",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTE",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTF",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTG",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTH",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTI",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTJ",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTK",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTM",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTO",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OUTP",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    // A-P values
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
    FieldDesc {
        name: "M",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "N",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "O",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "P",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
];

/// Choice labels for the calculation-option menu, in index order.
/// C `menu(transformCOPT)` (synApps `transformRecord.dbd`): 0=Conditional
/// (only recompute outputs whose inputs changed), 1=Always.
const TRANSFORM_COPT_CHOICES: &[&str] = &["Conditional", "Always"];

/// Choice labels for the invalid-link-action menu, in index order.
/// C `menu(transformIVLA)` (synApps `transformRecord.dbd`): 0="Ignore
/// error", 1="Do Nothing".
const TRANSFORM_IVLA_CHOICES: &[&str] = &["Ignore error", "Do Nothing"];

impl Record for TransformRecord {
    fn record_type(&self) -> &'static str {
        "transform"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // C `transformRecord.c:521` `ptran->udf = FALSE;` — the very first
        // thing every cycle does, including the IVLA-abandoned one below (C
        // clears it before the test), so the flag only ever reports THIS
        // cycle's calc failures.
        self.calc_failed = false;

        // IVLA="Do Nothing" + an INVALID input severity: C
        // `transformRecord.c:554-560` abandons the WHOLE cycle —
        //
        //   if ((ptran->nsev >= INVALID_ALARM) && (ptran->ivla == transformIVLA_DO_NOTHING)) {
        //       recGblGetTimeStamp(ptran); checkAlarms(ptran);
        //       recGblResetAlarms(ptran); ptran->pact = FALSE; return (0);
        //   }
        //
        // — no calc for ANY channel, none of the 16 OUTx `dbPutLink` writes,
        // no `monitor()`, no `recGblFwdLink()`. Only the timestamp and the
        // alarm commit run, which is exactly `CompleteAlarmOnly`. The input
        // links have already been read into A..P by this point in C (the fetch
        // loop precedes this test), and the framework's multi-input apply is
        // likewise already done, so the channels carry the fresh input values;
        // they are simply not published, calculated on, or driven out.
        //
        // `nsev` is the framework's pending severity for THIS cycle, folded
        // from the MS-class input links before `process()` — the same cell C
        // reads. IVLA is NOT a per-channel calc-failure policy: C never
        // restores a channel's previous value on a calc error (see the eval
        // arm below), so the port's old per-channel value-restore was invented
        // behaviour and is gone.
        if self.nsev >= AlarmSeverity::Invalid && self.ivla == 1 {
            return Ok(ProcessOutcome::complete_alarm_only());
        }

        // Snapshot and clear the fresh-put flags for this cycle.
        let fresh_put = std::mem::take(&mut self.fresh_put);

        // Evaluate each calc expression A-P. synApps `transformRecord.c`
        // (the `if (((no_inlink && !new_value) || copt==ALWAYS) &&
        // postfix_ok)` gate) uses COPT to decide whether CLCx is
        // EVALUATED — it does NOT gate the OUTx write below.
        //   Conditional (COPT=0): compute a channel only when it has NO
        //     input link AND was not freshly put (`no_inlink &&
        //     !new_value`); a channel driven by its INPx link or by a
        //     fresh `put` keeps that value instead of being overwritten
        //     by its CLCx.
        //   Always (COPT=1): compute whenever CLCx is valid, regardless.
        // C's `new_value = !same || map_bit`; for a no-input channel the
        // value changes between cycles only via a `put` (which also sets
        // `map_bit` = our `fresh_put`; the framework's INPx propagation
        // does NOT mark `fresh_put`), so `new_value` reduces to
        // `fresh_put` and `no_inlink && !new_value` is exactly
        // `no_inlink && !fresh_put`, needing no separate last-value
        // tracking. For an input-linked channel `no_inlink` is false, so
        // that term is false and `new_value` is unused.
        for i in 0..NUM_CHANNELS {
            let no_inlink = self.inp_links[i].is_empty();
            let do_calc = (no_inlink && !fresh_put[i]) || self.copt == 1;
            if !do_calc {
                continue;
            }
            if let Some(ref compiled) = self.compiled[i] {
                // C `transformRecord.c:593`:
                //
                //   sCalcPerform(&ptran->a, 16, NULL, 0, pval, NULL, 0, prpcbuf, ptran->prec)
                //
                // — the sCalc engine, with the record's sixteen channels as the
                // numeric args and no string args. NOT base's `calcPerform`.
                // The engines differ in the rule that decides this record's
                // alarm: `sCalcPerform` ends with
                //
                //   return (((isnan(*presult)||isinf(*presult)) ? -1 : 0));   // :2056
                //
                // so a non-finite result — `1/0` → +inf, `0/0` → NaN — is a
                // FAILURE, and `:593-596` turns it into CALC_ALARM/INVALID +
                // udf. Base's `calcPerform` has no such check and returns 0 with
                // the infinity in hand, which is what the port used to do:
                // `CLCx = "1/0"` yielded `inf` with NO_ALARM. `scalc_eval` is
                // the port's `sCalcPerform`, and it already owns the
                // non-finite rule (`CalcError::NonFiniteResult`).
                let mut inputs = StringInputs::new();
                inputs.num_vars[..NUM_CHANNELS].copy_from_slice(&self.vals);
                // `pval = &ptran->a + i` (`:564`, `:569`) is C's `presult`, and
                // the `VAL` token (`FETCH_VAL`) pushes `*presult` — *this
                // channel's* current value, not a record-wide previous VAL.
                inputs.prev_val = self.vals[i];
                match scalc_eval(compiled, &mut inputs) {
                    Ok(result) => {
                        // C's epilogue with `psresult == NULL`: `*presult` takes
                        // the double, coercing a string result through `atof`.
                        // `scalc_result` is the single owner of that coercion.
                        self.vals[i] = scalc_result(&result).0;
                    }
                    Err(_) => {
                        // C `transformRecord.c:593-596`:
                        //
                        //   if (sCalcPerform(...)) {
                        //       recGblSetSevr(ptran, CALC_ALARM, INVALID_ALARM);
                        //       ptran->udf = TRUE;
                        //   }
                        //
                        // `*pval` is left untouched and the loop continues with
                        // the next channel. The severity is raised by
                        // `check_alarms` below (the framework's `checkAlarms`
                        // slot) off this flag. IVLA plays no part here — it
                        // gates the whole cycle on the INPUT severity (see the
                        // top of `process`), never a single channel's calc.
                        self.calc_failed = true;
                    }
                }
            }
        }

        // Write every channel with a non-constant OUTx, UNCONDITIONALLY.
        // synApps `transformRecord.c` consults COPT only for calc-eval
        // (above); its output loop writes every `plink->type != CONSTANT`
        // OUTx each process, COPT untouched. The classic INPx -> A -> OUTx
        // passthrough / fan-out (empty CLCx) must therefore drive its OUTx
        // even in the default Conditional mode; the prior COPT/CLCx gate
        // here silently dropped it.
        let mut actions = Vec::new();
        for i in 0..NUM_CHANNELS {
            if self.out_links[i].is_empty() {
                continue;
            }
            actions.push(ProcessAction::WriteDbLink {
                link_field: OUT_FIELD_NAMES[i],
                value: EpicsValue::Double(self.vals[i]),
            });
        }
        Ok(ProcessOutcome::complete_with(actions))
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        if name == "VAL" {
            // The dummy result field — never written by process()/monitor().
            return Some(EpicsValue::Double(self.val));
        }
        if name == "COPT" {
            return Some(EpicsValue::Short(self.copt));
        }
        if name == "IVLA" {
            return Some(EpicsValue::Short(self.ivla));
        }
        if name == "PREC" {
            return Some(EpicsValue::Short(self.prec));
        }
        if let Some(idx) = Self::channel_index(name) {
            return Some(EpicsValue::Double(self.vals[idx]));
        }
        if let Some(idx) = Self::calc_field_index(name) {
            return Some(EpicsValue::String(self.calcs[idx].clone().into()));
        }
        if let Some(idx) = Self::inp_field_index(name) {
            return Some(EpicsValue::String(self.inp_links[idx].clone().into()));
        }
        if let Some(idx) = Self::out_field_index(name) {
            return Some(EpicsValue::String(self.out_links[idx].clone().into()));
        }
        None
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        if name == "VAL" {
            // Stored and readable back, but inert: no calc, output link or
            // monitor consumes it (C `transformRecord.c` never reads `->val`).
            self.val = value
                .to_f64()
                .ok_or_else(|| CaError::TypeMismatch("VAL".into()))?;
            return Ok(());
        }
        if name == "COPT" {
            match value {
                EpicsValue::Short(v) => {
                    self.copt = v;
                    return Ok(());
                }
                _ => return Err(CaError::TypeMismatch("COPT".into())),
            }
        }
        if name == "IVLA" {
            match value {
                EpicsValue::Short(v) => {
                    self.ivla = v;
                    return Ok(());
                }
                _ => return Err(CaError::TypeMismatch("IVLA".into())),
            }
        }
        if name == "PREC" {
            match value {
                EpicsValue::Short(v) => {
                    self.prec = v;
                    return Ok(());
                }
                _ => return Err(CaError::TypeMismatch("PREC".into())),
            }
        }
        if let Some(idx) = Self::channel_index(name) {
            self.vals[idx] = value
                .to_f64()
                .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
            return Ok(());
        }
        if let Some(idx) = Self::calc_field_index(name) {
            match value {
                EpicsValue::String(s) => {
                    self.calcs[idx] = s.as_str_lossy().into_owned();
                    self.recompile(idx);
                    return Ok(());
                }
                _ => return Err(CaError::TypeMismatch(name.into())),
            }
        }
        if let Some(idx) = Self::inp_field_index(name) {
            match value {
                EpicsValue::String(s) => {
                    self.inp_links[idx] = s.as_str_lossy().into_owned();
                    return Ok(());
                }
                _ => return Err(CaError::TypeMismatch(name.into())),
            }
        }
        if let Some(idx) = Self::out_field_index(name) {
            match value {
                EpicsValue::String(s) => {
                    self.out_links[idx] = s.as_str_lossy().into_owned();
                    return Ok(());
                }
                _ => return Err(CaError::TypeMismatch(name.into())),
            }
        }
        Err(CaError::FieldNotFound(name.to_string()))
    }

    /// S5 — mark a value channel (VAL / A..P) "freshly put" when it is
    /// written by an *external* put. The framework calls `special(field,
    /// true)` only on the CA / database-access put path
    /// (`field_io.rs`); the multi-input-link propagation
    /// (`processing.rs`) writes A..P via `put_field` directly *without*
    /// `special()`, so input-linked channels are NOT marked fresh and
    /// still re-compute from their CLCx every cycle. The next
    /// `process()` skips re-computing a fresh-put channel so a CA put to
    /// `transform.A` survives one cycle.
    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if after {
            // C `transformRecord.c:698-704` marks the "new value" bitmap only
            // for a field in the `A..P` range (`i = fieldIndex -
            // transformRecordA; if ((i >= 0) && (i < MAX_FIELDS))`). VAL sits
            // below `transformRecordA`, so a put to VAL marks nothing — it is
            // not a channel.
            if let Some(i) = Self::channel_index(field) {
                self.fresh_put[i] = true;
            }
        }
        Ok(())
    }

    /// Adopt the framework's per-cycle `dbCommon` snapshot. `nsev` — this
    /// cycle's pending severity, already carrying every MS-class input link's
    /// alarm — is what C `transformRecord.c:554` tests against `IVLA`.
    fn set_process_context(&mut self, ctx: &ProcessContext) {
        self.nsev = ctx.nsev;
    }

    /// A channel whose INPx link failed to read is ZEROED. C
    /// `transformRecord.c:537-541`, in the input loop:
    ///
    /// ```c
    /// if (plink->type != CONSTANT) {
    ///     status = dbGetLink(plink, DBR_DOUBLE, pval, NULL, NULL);
    ///     if (!RTN_SUCCESS(status)) { *pval = 0.; }
    /// }
    /// ```
    ///
    /// This is transform-specific: `calcRecord.c::fetch_values` (427-443)
    /// leaves `*pvalue` at its stale value on the same failure, and so do
    /// sub/sel/swait. So the zeroing lives here, not in the framework's shared
    /// multi-input apply.
    ///
    /// The framework reports the links that produced a value this cycle; a
    /// channel is zeroed when its link is CONFIGURED (non-empty — C's `type !=
    /// CONSTANT`) yet absent from that list. An unset channel is C's CONSTANT
    /// link: not read, not zeroed. A constant-valued link ("5") always
    /// resolves, so it never reaches the zeroing branch either.
    ///
    /// Runs before `process()` (the framework's report point), which is where C
    /// does it — the zero is what the calc loop and the OUTx write then see.
    fn set_resolved_input_links(&mut self, resolved: &[&'static str]) {
        for i in 0..NUM_CHANNELS {
            if !self.inp_links[i].is_empty() && !resolved.contains(&INP_FIELD_NAMES[i]) {
                self.vals[i] = 0.0;
            }
        }
    }

    /// C `transformRecord.c:593-595`: a channel whose `sCalcPerform` failed
    /// raises `recGblSetSevr(ptran, CALC_ALARM, INVALID_ALARM)`. Raised from
    /// the `checkAlarms` slot, which the framework runs BEFORE
    /// `rec_gbl_check_udf` — so on a calc failure CALC_ALARM lands first and
    /// the equal-severity UDF_ALARM (`checkAlarms`, `:773-779`) cannot displace
    /// it under `rec_gbl_set_sevr`'s strict-greater rule. Same order, same
    /// outcome as C.
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        if self.calc_failed {
            crate::server::recgbl::rec_gbl_set_sevr(
                common,
                crate::server::recgbl::alarm_status::CALC_ALARM,
                AlarmSeverity::Invalid,
            );
        }
    }

    /// C `transformRecord.c:793-794` throws away `recGblResetAlarms`'s mask —
    /// it assigns `monitor_mask = DBE_VALUE|DBE_LOG` over it — and the A..P
    /// change loop (:796-806) posts every one of the sixteen value fields with
    /// that literal. No transform field ever carries an alarm bit, so a
    /// `DBE_ALARM`-only subscriber on `.A` is notified on no cycle at all.
    fn fields_posted_without_alarm_bits(&self) -> &'static [&'static str] {
        &[
            "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P",
        ]
    }

    /// C `transformRecord.c::monitor()` (`:786-808`) is the record's ONLY
    /// `db_post_events` caller, and it posts exactly the sixteen channels
    /// A..P — the changed ones (plus every one of them on the first post).
    /// Nothing else. In particular it does NOT post `VAL`: transform's VAL is
    /// an inert dummy that `init_record` zeroes once (`:422`, *"Gotta have a
    /// .val field"*) and no other line of the record reads, writes or posts.
    ///
    /// Declaring the closed set is what stops the framework from inventing a
    /// `.VAL` monitor: the deadband post fires whenever ANY class fired, and
    /// on an alarm cycle the alarm bits alone are enough — so a transform
    /// whose input went INVALID was posting `.VAL` where C posts nothing.
    fn process_posted_fields(&self) -> Option<&'static [&'static str]> {
        Some(&[
            "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P",
        ])
    }

    /// Transform's UDF is C's `ptran->udf`: cleared at the top of every
    /// `process()` and set only by a failing channel calc. It is NOT derived
    /// from VAL — VAL is an inert dummy (R9-62).
    fn value_is_undefined(&self) -> bool {
        self.calc_failed
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
            ("INPM", "M"),
            ("INPN", "N"),
            ("INPO", "O"),
            ("INPP", "P"),
        ]
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        TRANSFORM_FIELDS
    }

    /// Record-specific `DBF_MENU` fields, served as `DBR_ENUM` with the
    /// menu's choice labels in `.dbd` index order (`transformRecord.dbd`):
    /// `COPT` is `menu(transformCOPT)`, `IVLA` is `menu(transformIVLA)`.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "COPT" => Some(TRANSFORM_COPT_CHOICES),
            "IVLA" => Some(TRANSFORM_IVLA_CHOICES),
            _ => None,
        }
    }
}

/// OUTA..OUTP field names, indexed by channel 0..15. Used by
/// `process()` to name the per-channel OUT link for a `WriteDbLink`
/// action — `ProcessAction::link_field` requires a `&'static str`.
static OUT_FIELD_NAMES: [&str; NUM_CHANNELS] = [
    "OUTA", "OUTB", "OUTC", "OUTD", "OUTE", "OUTF", "OUTG", "OUTH", "OUTI", "OUTJ", "OUTK", "OUTL",
    "OUTM", "OUTN", "OUTO", "OUTP",
];

/// INPA..INPP field names, indexed by channel 0..15 — the link-field spelling
/// the framework reports back through [`Record::set_resolved_input_links`].
static INP_FIELD_NAMES: [&str; NUM_CHANNELS] = [
    "INPA", "INPB", "INPC", "INPD", "INPE", "INPF", "INPG", "INPH", "INPI", "INPJ", "INPK", "INPL",
    "INPM", "INPN", "INPO", "INPP",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::record::CommonFields;

    /// An expression that compiles clean and FAILS at eval — C
    /// `sCalcPerform()` returning non-zero.
    ///
    /// `"1/0"` is the whole thing: it is a well-formed sCalc expression, it
    /// evaluates to `+inf`, and `sCalcPerform` ends
    /// `return (((isnan(*presult)||isinf(*presult)) ? -1 : 0));`
    /// (sCalcPerform.c:2056) — so a non-finite result IS the failure. This is
    /// reachable through the record's own CLCx put; no hand-built postfix
    /// program is needed (the port previously evaluated CLCx with base's
    /// numeric engine, which has no such check and hands back the infinity
    /// with a zero return — hence `1/0` yielding `inf` and NO_ALARM).
    const DIVIDE_BY_ZERO: &str = "1/0";

    /// R9-63 — a failing channel calc raises CALC_ALARM/INVALID and sets UDF.
    ///
    /// C `transformRecord.c:593-596`:
    /// `if (sCalcPerform(...)) { recGblSetSevr(ptran, CALC_ALARM, INVALID_ALARM);
    /// ptran->udf = TRUE; }`, and `checkAlarms` (`:773-779`) then raises
    /// UDF_ALARM at UDFS. The port raised nothing at all.
    #[test]
    fn r9_63_calc_failure_raises_calc_alarm_and_udf() {
        let mut rec = TransformRecord::new();
        rec.put_field("CLCA", EpicsValue::String(DIVIDE_BY_ZERO.into()))
            .unwrap();
        rec.process().unwrap();

        assert!(
            rec.value_is_undefined(),
            "a failing calc sets udf=TRUE (C transformRecord.c:595)"
        );

        let mut common = CommonFields::default();
        rec.check_alarms(&mut common);
        assert_eq!(
            common.nsev,
            AlarmSeverity::Invalid,
            "CALC_ALARM is raised at INVALID_ALARM severity"
        );
        assert_eq!(
            common.nsta,
            crate::server::recgbl::alarm_status::CALC_ALARM,
            "the status is CALC_ALARM, not UDF_ALARM — C raises CALC first and \
             recGblSetSevr is strict-greater, so the equal-severity UDF_ALARM \
             that checkAlarms adds cannot displace it"
        );
    }

    /// The flag is per-cycle: C clears `ptran->udf` at the top of every
    /// `process()` (`transformRecord.c:521`), so a cycle whose calc succeeds
    /// clears the alarm the previous failure raised.
    #[test]
    fn r9_63_calc_success_clears_the_previous_failure() {
        let mut rec = TransformRecord::new();
        rec.put_field("CLCA", EpicsValue::String(DIVIDE_BY_ZERO.into()))
            .unwrap();
        rec.process().unwrap();
        assert!(rec.value_is_undefined());

        rec.put_field("CLCA", EpicsValue::String("5".into()))
            .unwrap();
        rec.process().unwrap();
        assert!(
            !rec.value_is_undefined(),
            "a clean cycle clears udf (C sets udf = FALSE on entry)"
        );
        let mut common = CommonFields::default();
        rec.check_alarms(&mut common);
        assert_eq!(
            common.nsev,
            AlarmSeverity::NoAlarm,
            "no CALC_ALARM on a cycle whose calcs all succeeded"
        );
    }

    #[test]
    fn test_transform_default() {
        let rec = TransformRecord::new();
        assert_eq!(rec.record_type(), "transform");
        assert_eq!(rec.vals, [0.0; 16]);
        assert_eq!(rec.copt, 0);
    }

    #[test]
    fn test_transform_put_get_values() {
        let mut rec = TransformRecord::new();
        rec.put_field("A", EpicsValue::Double(1.0)).unwrap();
        rec.put_field("B", EpicsValue::Double(2.0)).unwrap();
        assert_eq!(rec.get_field("A"), Some(EpicsValue::Double(1.0)));
        assert_eq!(rec.get_field("B"), Some(EpicsValue::Double(2.0)));
    }

    #[test]
    fn test_transform_put_get_calc() {
        let mut rec = TransformRecord::new();
        rec.put_field("CLCA", EpicsValue::String("B+C".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("CLCA"),
            Some(EpicsValue::String("B+C".into()))
        );
    }

    #[test]
    fn test_transform_put_get_links() {
        let mut rec = TransformRecord::new();
        rec.put_field("INPA", EpicsValue::String("pv1".into()))
            .unwrap();
        rec.put_field("OUTA", EpicsValue::String("pv2".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("INPA"),
            Some(EpicsValue::String("pv1".into()))
        );
        assert_eq!(
            rec.get_field("OUTA"),
            Some(EpicsValue::String("pv2".into()))
        );
    }

    /// A `VAL` token in `CLCx` reads *that channel's* current value: C
    /// `transformRecord.c:593` passes `pval = &ptran->a + i` as `presult`
    /// (`:564`, `:569`), so each channel gets its own result cell — not one
    /// record-wide previous VAL, and not 0.
    #[test]
    fn r5_2_sibling_clc_val_token_reads_that_channels_value() {
        let mut rec = TransformRecord::new();
        rec.put_field("B", EpicsValue::Double(1.0)).unwrap();
        rec.put_field("C", EpicsValue::Double(100.0)).unwrap();
        rec.put_field("CLCB", EpicsValue::String("VAL*2".into()))
            .unwrap();
        rec.put_field("CLCC", EpicsValue::String("VAL+1".into()))
            .unwrap();

        // Each CLCx evaluates against its own channel's value: a single
        // record-wide previous VAL (or a 0 seed) could not produce both.
        rec.process().unwrap();
        assert_eq!(rec.vals[1], 2.0, "B = VAL(B)*2 = 1*2");
        assert_eq!(rec.vals[2], 101.0, "C = VAL(C)+1 = 100+1");
        rec.process().unwrap();
        assert_eq!(rec.vals[1], 4.0, "B = 2*2");
        assert_eq!(rec.vals[2], 102.0, "C = 101+1");
    }

    #[test]
    fn test_transform_process_simple() {
        let mut rec = TransformRecord::new();
        rec.put_field("B", EpicsValue::Double(3.0)).unwrap();
        rec.put_field("C", EpicsValue::Double(4.0)).unwrap();
        rec.put_field("CLCA", EpicsValue::String("B+C".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(rec.vals[0], 7.0); // A = B+C = 3+4 = 7
    }

    #[test]
    fn test_transform_process_chain() {
        let mut rec = TransformRecord::new();
        rec.put_field("A", EpicsValue::Double(2.0)).unwrap();
        rec.put_field("CLCB", EpicsValue::String("A*3".into()))
            .unwrap();
        rec.put_field("CLCC", EpicsValue::String("B+1".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(rec.vals[1], 6.0); // B = A*3 = 6
        assert_eq!(rec.vals[2], 7.0); // C = B+1 = 7 (uses updated B)
    }

    #[test]
    fn test_transform_process_no_calc() {
        let mut rec = TransformRecord::new();
        rec.put_field("A", EpicsValue::Double(5.0)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.vals[0], 5.0); // A unchanged — no calc
    }

    #[test]
    fn test_transform_ivla_do_nothing() {
        let mut rec = TransformRecord::new();
        rec.put_field("A", EpicsValue::Double(10.0)).unwrap();
        rec.put_field("IVLA", EpicsValue::Short(1)).unwrap();
        // Use invalid expression that fails to compile — compiled[0] stays None
        rec.calcs[0] = "???invalid".into();
        rec.compiled[0] = None;
        rec.process().unwrap();
        assert_eq!(rec.vals[0], 10.0); // Unchanged — no valid calc
    }

    #[test]
    fn test_transform_ivla_ignore() {
        let mut rec = TransformRecord::new();
        rec.put_field("A", EpicsValue::Double(10.0)).unwrap();
        rec.put_field("B", EpicsValue::Double(5.0)).unwrap();
        rec.put_field("IVLA", EpicsValue::Short(0)).unwrap();
        // CLCA has no valid calc (empty), CLCB evaluates
        rec.put_field("CLCB", EpicsValue::String("A+1".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(rec.vals[0], 10.0); // A unchanged
        assert_eq!(rec.vals[1], 11.0); // B = A+1 = 10+1 = 11
    }

    #[test]
    fn test_transform_all_channels() {
        let mut rec = TransformRecord::new();
        // Set all 16 channels
        for (i, ch) in ('A'..='P').enumerate() {
            let name = ch.to_string();
            rec.put_field(&name, EpicsValue::Double(i as f64)).unwrap();
            assert_eq!(rec.get_field(&name), Some(EpicsValue::Double(i as f64)));
        }
    }

    #[test]
    fn test_transform_field_list() {
        let rec = TransformRecord::new();
        let fields = rec.field_list();
        assert!(fields.len() > 60); // 4 + 16*4 = 68 fields
    }

    #[test]
    fn test_transform_field_not_found() {
        let mut rec = TransformRecord::new();
        assert!(rec.put_field("ZZZ", EpicsValue::Double(1.0)).is_err());
        assert!(rec.get_field("ZZZ").is_none());
    }

    #[test]
    fn test_transform_type_mismatch() {
        let mut rec = TransformRecord::new();
        assert!(rec.put_field("CLCA", EpicsValue::Double(1.0)).is_err());
        assert!(
            rec.put_field("COPT", EpicsValue::String("x".into()))
                .is_err()
        );
    }

    #[test]
    fn test_transform_recompile_on_calc_change() {
        let mut rec = TransformRecord::new();
        rec.put_field("A", EpicsValue::Double(2.0)).unwrap();
        rec.put_field("CLCB", EpicsValue::String("A*2".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(rec.vals[1], 4.0);

        // Change calc expression
        rec.put_field("CLCB", EpicsValue::String("A*3".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(rec.vals[1], 6.0);
    }

    /// R9-62 — `VAL` is a constant-0 dummy, NOT an alias of channel A.
    ///
    /// C `transformRecord.c:422` sets `ptran->val = 0` once at init and no
    /// other line in the record reads or writes `->val`: `process()` and
    /// `monitor()` both walk the channels from `&ptran->a`. So `caget .VAL`
    /// returns 0 no matter what A computes, and a `.VAL` monitor never fires.
    /// The superseded `test_transform_val_is_a` asserted `VAL == 42` here,
    /// pinning an alias C does not have.
    #[test]
    fn r9_62_val_is_a_constant_zero_dummy_not_channel_a() {
        let mut rec = TransformRecord::new();
        rec.put_field("CLCA", EpicsValue::String("42".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(
            rec.get_field("A"),
            Some(EpicsValue::Double(42.0)),
            "CLCA computed channel A"
        );
        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::Double(0.0)),
            "VAL stays at its init value — process() never touches ->val"
        );
    }

    /// A client put to VAL is stored and read back (plain writable DBF_DOUBLE,
    /// `transformRecord.dbd:43`), but it is inert: it does not become channel
    /// A, and a subsequent process leaves it alone.
    #[test]
    fn r9_62_val_put_is_stored_but_never_feeds_a_channel() {
        let mut rec = TransformRecord::new();
        rec.put_field("VAL", EpicsValue::Double(7.0)).unwrap();
        assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Double(7.0)));
        assert_eq!(
            rec.get_field("A"),
            Some(EpicsValue::Double(0.0)),
            "a put to VAL must not land in channel A"
        );
        rec.put_field("CLCA", EpicsValue::String("3".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(rec.get_field("A"), Some(EpicsValue::Double(3.0)));
        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::Double(7.0)),
            "process() leaves the put-stored VAL untouched"
        );
    }
}
