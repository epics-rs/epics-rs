use crate::error::{CaError, CaResult};
use crate::server::record::{
    AlarmSeverity, FieldDesc, ProcessAction, ProcessContext, ProcessOutcome, Record,
};
use crate::types::{DbFieldType, EpicsValue};

use crate::calc::NumericInputs;
use crate::calc::{CompiledExpr, compile, eval};

const NUM_CHANNELS: usize = 16; // A-P

/// Transform record — 16 input/output channels (A-P), each with its own calc expression.
///
/// Processing: reads inputs via INPA-INPP links, evaluates CLCA-CLCP expressions
/// (each can reference all 16 variables A-P), stores results back into A-P,
/// then writes outputs via OUTA-OUTP links.
pub struct TransformRecord {
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
}

impl Default for TransformRecord {
    fn default() -> Self {
        Self {
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
        }
    }
}

impl TransformRecord {
    pub fn new() -> Self {
        Self::default()
    }

    fn recompile(&mut self, idx: usize) {
        if self.calcs[idx].is_empty() {
            self.compiled[idx] = None;
        } else {
            self.compiled[idx] = compile(&self.calcs[idx]).ok();
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
                let mut inputs = NumericInputs::new();
                inputs.vars[..NUM_CHANNELS].copy_from_slice(&self.vals);
                // C `transformRecord.c:593` calls `sCalcPerform(&ptran->a, 16,
                // ..., pval, ...)` with `pval = &ptran->a + i` (`:564`, `:569`),
                // so the `VAL` token in CLCx pushes *this channel's* current
                // value — not a single record-wide previous VAL, and not 0.
                inputs.prev_val = self.vals[i];
                match eval(compiled, &mut inputs) {
                    Ok(result) => {
                        self.vals[i] = result;
                    }
                    Err(_) => {
                        // C `transformRecord.c:593-596`: a failing
                        // `sCalcPerform` leaves `*pval` untouched and raises
                        // CALC_ALARM/INVALID + `udf = TRUE`; the loop
                        // continues with the next channel. IVLA plays no part
                        // here — it gates the whole cycle on the INPUT
                        // severity (see the top of `process`), never a single
                        // channel's calc result.
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
            return Some(EpicsValue::Double(self.vals[0]));
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
            self.vals[0] = value
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
            let idx = if field == "VAL" {
                Some(0)
            } else {
                Self::channel_index(field)
            };
            if let Some(i) = idx {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_transform_val_is_a() {
        let mut rec = TransformRecord::new();
        rec.put_field("CLCA", EpicsValue::String("42".into()))
            .unwrap();
        rec.process().unwrap();
        // VAL returns vals[0] which is A
        assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Double(42.0)));
    }
}
