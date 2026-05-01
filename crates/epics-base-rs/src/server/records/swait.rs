use crate::calc::{CompiledExpr, scalc_compile, scalc_eval};
use crate::calc::StringInputs;
use crate::calc::engine::value::StackValue;
use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

// swait (string wait) record from synApps calc module.
// Functionally equivalent to scalcout, but uses INxN/INxP naming for input links
// (INxN = link name, INxP = process passive flag) instead of INPx.
// Supports 12 numeric inputs (A-L), a CALC expression, and a single output link.
// DOL/DOLD: desired output; DOPT: 0=use CALC, 1=use DOLD; OOPT: output condition.
pub struct SwaitRecord {
    pub val: f64,
    pub calc: String,
    compiled_calc: Option<CompiledExpr>,
    pub oopt: i16,
    pub dopt: i16,
    pub dold: f64,
    pub oval: f64,
    pub out: String,
    pub prec: i16,
    // INxN: input link names; INxP: process passive flags (0/1)
    pub inp_names: [String; 12], // INAN..INLN
    pub inp_passive: [i16; 12],  // INAP..INLP
    // numeric input values A-L
    pub num_vals: [f64; 12],
    prev_val: f64,
    cached_should_output: bool,
}

impl Default for SwaitRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            calc: String::new(),
            compiled_calc: None,
            oopt: 0,
            dopt: 0,
            dold: 0.0,
            oval: 0.0,
            out: String::new(),
            prec: 0,
            inp_names: Default::default(),
            inp_passive: [0; 12],
            num_vals: [0.0; 12],
            prev_val: 0.0,
            cached_should_output: true,
        }
    }
}

// Channel letters A-L in order
const CHAN: [char; 12] = ['A','B','C','D','E','F','G','H','I','J','K','L'];

impl SwaitRecord {
    fn recompile(&mut self) {
        self.compiled_calc = scalc_compile(&self.calc).ok();
    }

    fn build_inputs(&self) -> StringInputs {
        let mut inputs = StringInputs { num_vars: [0.0; 16], str_vars: Default::default() };
        for i in 0..12 {
            inputs.num_vars[i] = self.num_vals[i];
        }
        inputs
    }

    fn eval_should_output(&self) -> bool {
        match self.oopt {
            0 => true,
            1 => self.val != self.prev_val,
            2 => self.val == 0.0,
            3 => self.val != 0.0,
            4 => self.prev_val != 0.0 && self.val == 0.0,
            5 => self.prev_val == 0.0 && self.val != 0.0,
            _ => false,
        }
    }

    fn inp_name_index(name: &str) -> Option<usize> {
        // INxN: INAN, INBN, INCN, INDN, INEN, INFN, INGN, INHN, ININ, INJN, INKN, INLN
        let bytes = name.as_bytes();
        if bytes.len() == 4 && bytes[0] == b'I' && bytes[1] == b'N' && bytes[3] == b'N' {
            CHAN.iter().position(|&c| c == bytes[2] as char)
        } else {
            None
        }
    }

    fn inp_passive_index(name: &str) -> Option<usize> {
        // INxP: INAP, INBP, INCP, ...
        let bytes = name.as_bytes();
        if bytes.len() == 4 && bytes[0] == b'I' && bytes[1] == b'N' && bytes[3] == b'P' {
            CHAN.iter().position(|&c| c == bytes[2] as char)
        } else {
            None
        }
    }

    fn num_val_index(name: &str) -> Option<usize> {
        // Single letter A-L
        if name.len() == 1 {
            CHAN.iter().position(|&c| c.to_string() == name)
        } else {
            None
        }
    }


}

static SWAIT_FIELDS_SCALAR: &[FieldDesc] = &[
    FieldDesc { name: "VAL",  dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "CALC", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "OOPT", dbf_type: DbFieldType::Short,  read_only: false },
    FieldDesc { name: "DOPT", dbf_type: DbFieldType::Short,  read_only: false },
    FieldDesc { name: "DOLD", dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "OVAL", dbf_type: DbFieldType::Double, read_only: true  },
    // OUT and OUTN are intentionally absent: both route to RecordInstance::common.out
    // via put_common_field so that parsed_out is populated for output dispatch.
    // OUTN is swait's output link field name; RecordInstance handles the alias.
    FieldDesc { name: "PREC", dbf_type: DbFieldType::Short,  read_only: false },
    FieldDesc { name: "A",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "B",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "C",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "D",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "E",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "F",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "G",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "H",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "I",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "J",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "K",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "L",    dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "INAN", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INBN", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INCN", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INDN", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INEN", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INFN", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INGN", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INHN", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "ININ", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INJN", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INKN", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INLN", dbf_type: DbFieldType::String, read_only: false },
    FieldDesc { name: "INAP", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "INBP", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "INCP", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "INDP", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "INEP", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "INFP", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "INGP", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "INHP", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "INIP", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "INJP", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "INKP", dbf_type: DbFieldType::Short, read_only: false },
    FieldDesc { name: "INLP", dbf_type: DbFieldType::Short, read_only: false },
];

impl Record for SwaitRecord {
    fn record_type(&self) -> &'static str {
        "swait"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        SWAIT_FIELDS_SCALAR
    }

    fn uses_monitor_deadband(&self) -> bool {
        true
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            self.recompile();
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.prev_val = self.val;

        if let Some(ref compiled) = self.compiled_calc {
            let mut inputs = self.build_inputs();
            if let Ok(result) = scalc_eval(compiled, &mut inputs) {
                self.val = match result {
                    StackValue::Double(v) => v,
                    StackValue::Str(s) => s.parse::<f64>().unwrap_or(0.0),
                };
            }
        }

        // Cache before framework calls should_output() via trait dispatch.
        self.cached_should_output = self.eval_should_output();
        if self.cached_should_output {
            self.oval = if self.dopt == 1 { self.dold } else { self.val };
        }

        Ok(ProcessOutcome::complete())
    }

    fn should_output(&self) -> bool {
        self.cached_should_output
    }

    fn val(&self) -> Option<EpicsValue> {
        Some(EpicsValue::Double(self.val))
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL"  => Some(EpicsValue::Double(self.val)),
            "CALC" => Some(EpicsValue::String(self.calc.clone())),
            "OOPT" => Some(EpicsValue::Short(self.oopt)),
            "DOPT" => Some(EpicsValue::Short(self.dopt)),
            "DOLD" => Some(EpicsValue::Double(self.dold)),
            "OVAL" => Some(EpicsValue::Double(self.oval)),
            // OUTN is aliased to common.out via RecordInstance; not stored locally.
            "PREC" => Some(EpicsValue::Short(self.prec)),
            _ => {
                if let Some(idx) = Self::num_val_index(name) {
                    return Some(EpicsValue::Double(self.num_vals[idx]));
                }
                if let Some(idx) = Self::inp_name_index(name) {
                    return Some(EpicsValue::String(self.inp_names[idx].clone()));
                }
                if let Some(idx) = Self::inp_passive_index(name) {
                    return Some(EpicsValue::Short(self.inp_passive[idx]));
                }
                None
            }
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                self.val = value.to_f64().ok_or_else(|| CaError::TypeMismatch("VAL".into()))?;
            }
            "CALC" => {
                if let EpicsValue::String(s) = value {
                    self.calc = s;
                    self.recompile();
                } else {
                    return Err(CaError::TypeMismatch("CALC".into()));
                }
            }
            "OOPT" => { if let EpicsValue::Short(v) = value { self.oopt = v; } }
            "DOPT" => { if let EpicsValue::Short(v) = value { self.dopt = v; } }
            "DOLD" => {
                self.dold = value.to_f64().ok_or_else(|| CaError::TypeMismatch("DOLD".into()))?;
            }
            // OUTN falls through to put_common_field which mirrors to common.out.
            "PREC" => { if let EpicsValue::Short(v) = value { self.prec = v; } }
            _ => {
                if let Some(idx) = Self::num_val_index(name) {
                    self.num_vals[idx] =
                        value.to_f64().ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                } else if let Some(idx) = Self::inp_name_index(name) {
                    if let EpicsValue::String(s) = value {
                        self.inp_names[idx] = s;
                    } else {
                        return Err(CaError::TypeMismatch(name.into()));
                    }
                } else if let Some(idx) = Self::inp_passive_index(name) {
                    if let EpicsValue::Short(v) = value {
                        self.inp_passive[idx] = v;
                    } else {
                        return Err(CaError::TypeMismatch(name.into()));
                    }
                } else {
                    return Err(CaError::FieldNotFound(name.to_string()));
                }
            }
        }
        Ok(())
    }

    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        &[
            ("INAN", "A"), ("INBN", "B"), ("INCN", "C"), ("INDN", "D"),
            ("INEN", "E"), ("INFN", "F"), ("INGN", "G"), ("INHN", "H"),
            ("ININ", "I"), ("INJN", "J"), ("INKN", "K"), ("INLN", "L"),
        ]
    }
}
