use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
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
    pub prev_vals: [f64; NUM_CHANNELS],
    pub calcs: [String; NUM_CHANNELS],
    compiled: [Option<CompiledExpr>; NUM_CHANNELS],
    pub inp_links: [String; NUM_CHANNELS],
    pub out_links: [String; NUM_CHANNELS],
    pub copt: i16, // 0=Conditional (only if calc non-empty), 1=Always
    pub ivla: i16, // 0=Ignore error, 1=Do Nothing
    pub prec: i16,
}

impl Default for TransformRecord {
    fn default() -> Self {
        Self {
            vals: [0.0; NUM_CHANNELS],
            prev_vals: [0.0; NUM_CHANNELS],
            calcs: Default::default(),
            compiled: Default::default(),
            inp_links: Default::default(),
            out_links: Default::default(),
            copt: 0,
            ivla: 0,
            prec: 0,
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

impl Record for TransformRecord {
    fn record_type(&self) -> &'static str {
        "transform"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // Save previous values
        self.prev_vals = self.vals;

        // Evaluate each calc expression A-P
        for i in 0..NUM_CHANNELS {
            if let Some(ref compiled) = self.compiled[i] {
                let mut inputs = NumericInputs { vars: self.vals };
                match eval(compiled, &mut inputs) {
                    Ok(result) => {
                        self.vals[i] = result;
                    }
                    Err(_) => {
                        if self.ivla == 1 {
                            // Do Nothing — restore all values
                            self.vals = self.prev_vals;
                            return Ok(ProcessOutcome::complete());
                        }
                        // Ignore error — continue
                    }
                }
            }
        }
        Ok(ProcessOutcome::complete())
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
            return Some(EpicsValue::String(self.calcs[idx].clone()));
        }
        if let Some(idx) = Self::inp_field_index(name) {
            return Some(EpicsValue::String(self.inp_links[idx].clone()));
        }
        if let Some(idx) = Self::out_field_index(name) {
            return Some(EpicsValue::String(self.out_links[idx].clone()));
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
                    self.calcs[idx] = s;
                    self.recompile(idx);
                    return Ok(());
                }
                _ => return Err(CaError::TypeMismatch(name.into())),
            }
        }
        if let Some(idx) = Self::inp_field_index(name) {
            match value {
                EpicsValue::String(s) => {
                    self.inp_links[idx] = s;
                    return Ok(());
                }
                _ => return Err(CaError::TypeMismatch(name.into())),
            }
        }
        if let Some(idx) = Self::out_field_index(name) {
            match value {
                EpicsValue::String(s) => {
                    self.out_links[idx] = s;
                    return Ok(());
                }
                _ => return Err(CaError::TypeMismatch(name.into())),
            }
        }
        Err(CaError::FieldNotFound(name.to_string()))
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

    fn multi_output_links(&self) -> &[(&'static str, &'static str)] {
        &[
            ("OUTA", "A"),
            ("OUTB", "B"),
            ("OUTC", "C"),
            ("OUTD", "D"),
            ("OUTE", "E"),
            ("OUTF", "F"),
            ("OUTG", "G"),
            ("OUTH", "H"),
            ("OUTI", "I"),
            ("OUTJ", "J"),
            ("OUTK", "K"),
            ("OUTL", "L"),
            ("OUTM", "M"),
            ("OUTN", "N"),
            ("OUTO", "O"),
            ("OUTP", "P"),
        ]
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        TRANSFORM_FIELDS
    }
}

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
