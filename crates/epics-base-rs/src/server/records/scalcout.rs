use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

use crate::calc::StringInputs;
use crate::calc::engine::value::StackValue;
use crate::calc::{CompiledExpr, scalc_compile, scalc_eval};

/// Scalcout record — string calc with output.
///
/// Like calcout but uses the string calc engine (sCalcPerform).
/// CALC expression evaluates to SVAL (string) or VAL (numeric).
/// OCAL provides optional output calculation.
/// Output decision controlled by OOPT.
pub struct ScalcoutRecord {
    pub val: f64,
    pub sval: String,
    pub calc: String,
    compiled_calc: Option<CompiledExpr>,
    pub oopt: i16, // 0=Every, 1=OnChange, 2=WhenZero, 3=WhenNonzero, 4=TransZero, 5=TransNonzero
    pub dopt: i16, // 0=Use CALC, 1=Use OCAL
    pub ocal: String,
    compiled_ocal: Option<CompiledExpr>,
    pub oval: f64,
    pub osv: String,
    pub ivoa: i16, // 0=Continue, 1=Don't drive, 2=Set to IVOV
    pub ivov: f64,
    pub out: String, // output link
    pub wait: i16,   // wait for output completion
    pub prec: i16,
    // Input link strings (INPA..INPL)
    pub inp_links: [String; 12],
    // Numeric input values A-L (mapped to vars A-P, but only 12 used)
    pub num_vals: [f64; 12],
    // String input values AA-LL
    pub str_vals: [String; 12],
    // Previous value for transition detection
    prev_val: f64,
    prev_sval: String,
    /// CALC_ALARM flag — set when the CALC or OCAL `sCalcPerform`
    /// evaluation fails. synApps `sCalcoutRecord` raises `CALC_ALARM`
    /// on a broken expression; the framework's `evaluate_alarms`
    /// already inspects a `CALC_ALARM` field for `scalcout`, so this
    /// flag is surfaced through `get_field("CALC_ALARM")`.
    calc_alarm: bool,
    /// Output decision from the last `process()`. The framework's
    /// generic multi-output dispatch reads `multi_output_links()`
    /// unconditionally, so this caches the OOPT decision and gates
    /// the OUT-link write on it.
    cached_should_output: bool,
}

impl Default for ScalcoutRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            sval: String::new(),
            calc: String::new(),
            compiled_calc: None,
            oopt: 0,
            dopt: 0,
            ocal: String::new(),
            compiled_ocal: None,
            oval: 0.0,
            osv: String::new(),
            ivoa: 0,
            ivov: 0.0,
            out: String::new(),
            wait: 0,
            prec: 0,
            inp_links: Default::default(),
            num_vals: [0.0; 12],
            str_vals: Default::default(),
            prev_val: 0.0,
            prev_sval: String::new(),
            calc_alarm: false,
            cached_should_output: false,
        }
    }
}

impl ScalcoutRecord {
    pub fn new() -> Self {
        Self::default()
    }

    fn build_inputs(&self) -> StringInputs {
        let mut inputs = StringInputs::new();
        for i in 0..12 {
            inputs.num_vars[i] = self.num_vals[i];
            inputs.str_vars[i] = self.str_vals[i].clone();
        }
        inputs
    }

    fn apply_result(&mut self, result: &StackValue) {
        match result {
            StackValue::Double(v) => {
                self.val = *v;
                self.sval = format!("{}", v);
            }
            StackValue::Str(s) => {
                self.sval = s.clone();
                self.val = s.parse::<f64>().unwrap_or(0.0);
            }
        }
    }

    fn should_output(&self) -> bool {
        match self.oopt {
            0 => true,
            1 => (self.val - self.prev_val).abs() > f64::EPSILON || self.sval != self.prev_sval,
            2 => self.val == 0.0,
            3 => self.val != 0.0,
            4 => self.prev_val != 0.0 && self.val == 0.0,
            5 => self.prev_val == 0.0 && self.val != 0.0,
            _ => true,
        }
    }

    fn recompile_calc(&mut self) {
        self.compiled_calc = if self.calc.is_empty() {
            None
        } else {
            scalc_compile(&self.calc).ok()
        };
    }

    fn recompile_ocal(&mut self) {
        self.compiled_ocal = if self.ocal.is_empty() {
            None
        } else {
            scalc_compile(&self.ocal).ok()
        };
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

impl Record for ScalcoutRecord {
    fn record_type(&self) -> &'static str {
        "scalcout"
    }

    // C recScalcout.c IVOA=set_to_IVOV: oval = ivov (and osv = isvv
    // for string output side, but OUT writeback only reads OVAL).
    fn apply_invalid_output_value(&mut self, ivov: EpicsValue) -> CaResult<()> {
        self.put_field("OVAL", ivov)
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.prev_val = self.val;
        self.prev_sval = self.sval.clone();

        // Evaluate CALC. A fresh cycle clears CALC_ALARM; a broken CALC
        // (compile failure OR an sCalcPerform eval failure) re-raises
        // it. synApps `sCalcoutRecord` raises CALC_ALARM on any broken
        // expression; without this a failing scalcout expression
        // silently kept the previous cycle's VAL/SVAL with no invalid
        // indication.
        self.calc_alarm = false;

        // A non-empty CALC that did not compile is itself a broken
        // expression — flag it (scalc_eval is never reached for None).
        let calc_failed = if self.calc.is_empty() {
            false
        } else if let Some(ref compiled) = self.compiled_calc {
            let mut inputs = self.build_inputs();
            match scalc_eval(compiled, &mut inputs) {
                Ok(result) => {
                    self.apply_result(&result);
                    false
                }
                Err(_) => true,
            }
        } else {
            // calc non-empty but compile failed
            true
        };

        if calc_failed {
            self.calc_alarm = true;
            // Invalid calc — check IVOA
            match self.ivoa {
                1 => {
                    // Don't drive output — suppress the OUT write.
                    self.cached_should_output = false;
                    return Ok(ProcessOutcome::complete());
                }
                2 => {
                    self.val = self.ivov;
                }
                _ => {} // Continue
            }
        }

        // Determine output
        let do_output = self.should_output();
        self.cached_should_output = do_output;
        if do_output {
            if self.dopt == 1 {
                // Use OCAL. A broken OCAL (compile OR eval failure)
                // raises CALC_ALARM — sibling calcout.rs does the same,
                // and synApps `sCalcoutRecord` raises CALC_ALARM on an
                // OCAL sCalcPerform failure. Previously an OCAL error
                // was discarded and OVAL/OSV kept stale values with no
                // alarm.
                if !self.ocal.is_empty() {
                    if let Some(ref compiled) = self.compiled_ocal {
                        let mut inputs = self.build_inputs();
                        match scalc_eval(compiled, &mut inputs) {
                            Ok(result) => match &result {
                                StackValue::Double(v) => {
                                    self.oval = *v;
                                    self.osv = format!("{}", v);
                                }
                                StackValue::Str(s) => {
                                    self.osv = s.clone();
                                    self.oval = s.parse::<f64>().unwrap_or(0.0);
                                }
                            },
                            Err(_) => {
                                self.calc_alarm = true;
                            }
                        }
                    } else {
                        // OCAL non-empty but compile failed.
                        self.calc_alarm = true;
                    }
                }
            } else {
                // Use CALC result
                self.oval = self.val;
                self.osv = self.sval.clone();
            }
        }
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "SVAL" => Some(EpicsValue::String(self.sval.clone().into())),
            "CALC" => Some(EpicsValue::String(self.calc.clone().into())),
            "OOPT" => Some(EpicsValue::Short(self.oopt)),
            "DOPT" => Some(EpicsValue::Short(self.dopt)),
            "OCAL" => Some(EpicsValue::String(self.ocal.clone().into())),
            "OVAL" => Some(EpicsValue::Double(self.oval)),
            "OSV" => Some(EpicsValue::String(self.osv.clone().into())),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Double(self.ivov)),
            "OUT" => Some(EpicsValue::String(self.out.clone().into())),
            "WAIT" => Some(EpicsValue::Short(self.wait)),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "CALC_ALARM" => Some(EpicsValue::Char(if self.calc_alarm { 1 } else { 0 })),
            _ => {
                if let Some(idx) = Self::var_index(name) {
                    return Some(EpicsValue::Double(self.num_vals[idx]));
                }
                if let Some(idx) = Self::str_var_index(name) {
                    return Some(EpicsValue::String(self.str_vals[idx].clone().into()));
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
                    self.sval = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SVAL".into())),
            },
            "CALC" => match value {
                EpicsValue::String(s) => {
                    self.calc = s.as_str_lossy().into_owned();
                    self.recompile_calc();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("CALC".into())),
            },
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
                    self.recompile_ocal();
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
                    self.osv = s.as_str_lossy().into_owned();
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
                            self.str_vals[idx] = s.as_str_lossy().into_owned();
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

    fn field_list(&self) -> &'static [FieldDesc] {
        SCALCOUT_FIELDS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scalcout_default() {
        let rec = ScalcoutRecord::new();
        assert_eq!(rec.record_type(), "scalcout");
        assert_eq!(rec.val, 0.0);
        assert_eq!(rec.sval, "");
    }

    #[test]
    fn test_scalcout_numeric_calc() {
        let mut rec = ScalcoutRecord::new();
        rec.put_field("A", EpicsValue::Double(3.0)).unwrap();
        rec.put_field("B", EpicsValue::Double(4.0)).unwrap();
        rec.put_field("CALC", EpicsValue::String("A+B".into()))
            .unwrap();
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
        rec.process().unwrap();
        assert_eq!(rec.sval, "hello world");
    }

    #[test]
    fn test_scalcout_oopt_every() {
        let mut rec = ScalcoutRecord::new();
        rec.put_field("CALC", EpicsValue::String("42".into()))
            .unwrap();
        rec.put_field("OOPT", EpicsValue::Short(0)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.oval, 42.0);
    }

    #[test]
    fn test_scalcout_oopt_on_change() {
        let mut rec = ScalcoutRecord::new();
        rec.put_field("CALC", EpicsValue::String("A".into()))
            .unwrap();
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
        rec.put_field("OCAL", EpicsValue::String("A*2".into()))
            .unwrap();
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
        rec.put_field("OCAL", EpicsValue::String("AA".into()))
            .unwrap();
        rec.put_field("DOPT", EpicsValue::Short(1)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.osv, "hi");
    }

    #[test]
    fn test_scalcout_ivoa_dont_drive() {
        let mut rec = ScalcoutRecord::new();
        // Use an expression that will fail to compile
        rec.calc = "???invalid".into();
        rec.compiled_calc = None;
        rec.put_field("IVOA", EpicsValue::Short(1)).unwrap();
        rec.process().unwrap();
        // No compiled calc → nothing happens, oval stays 0
        assert_eq!(rec.oval, 0.0);
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
}
