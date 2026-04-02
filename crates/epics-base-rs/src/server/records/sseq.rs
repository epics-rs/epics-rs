use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

const NUM_STEPS: usize = 10;

/// A single step in the string sequence.
#[derive(Clone, Default)]
struct SseqStep {
    dly: f64,        // Delay before executing this step
    dol: String,     // Input link (DOLn)
    dov: f64,        // Numeric value (DOn)
    lnk: String,     // Output link (LNKn)
    str_val: String, // String value (STRn)
    wait: i16,       // Wait mode: 0=NoWait, 1=Wait, 2..=After1..After9
}

/// Sseq record — string sequence record.
///
/// Executes up to 10 steps, each with an optional delay, input link,
/// numeric value, string value, and output link. Steps are selected
/// by SELM (All, Specified, Mask) with SELN as the selection value.
pub struct SseqRecord {
    pub val: i32,
    pub selm: i16, // 0=All, 1=Specified, 2=Mask
    pub seln: u16,
    pub sell: String,
    pub prec: i16,
    pub abort: i16,
    pub busy: i16,
    steps: [SseqStep; NUM_STEPS],
}

impl Default for SseqRecord {
    fn default() -> Self {
        Self {
            val: 0,
            selm: 0,
            seln: 1,
            sell: String::new(),
            prec: 0,
            abort: 0,
            busy: 0,
            steps: Default::default(),
        }
    }
}

impl SseqRecord {
    pub fn new() -> Self {
        Self::default()
    }

    fn step_index_from_suffix(name: &str) -> Option<(usize, &str)> {
        // Parse step index from field name suffix: 1-9 or A (=10)
        if name.len() < 2 {
            return None;
        }
        let last = name.as_bytes()[name.len() - 1];
        let prefix = &name[..name.len() - 1];
        match last {
            b'1'..=b'9' => Some(((last - b'1') as usize, prefix)),
            b'A' => Some((9, prefix)),
            _ => None,
        }
    }

    pub fn should_execute_step(&self, step_idx: usize) -> bool {
        match self.selm {
            0 => true, // All
            1 => {
                // Specified — SELN selects which step (1-based)
                let sel = self.seln as usize;
                if sel >= 1 && sel <= 9 {
                    step_idx == sel - 1
                } else if sel == 10 {
                    step_idx == 9
                } else {
                    false
                }
            }
            2 => {
                // Mask — SELN is a bitmask
                (self.seln & (1 << step_idx)) != 0
            }
            _ => false,
        }
    }
}

static SSEQ_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "SELM",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SELN",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SELL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "PREC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "ABORT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "BUSY",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    // Steps 1-9
    FieldDesc {
        name: "DLY1",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DOL1",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "DO1",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LNK1",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "STR1",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "WAIT1",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DLY2",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DOL2",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "DO2",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LNK2",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "STR2",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "WAIT2",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DLY3",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DOL3",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "DO3",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LNK3",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "STR3",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "WAIT3",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DLY4",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DOL4",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "DO4",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LNK4",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "STR4",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "WAIT4",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DLY5",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DOL5",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "DO5",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LNK5",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "STR5",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "WAIT5",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DLY6",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DOL6",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "DO6",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LNK6",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "STR6",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "WAIT6",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DLY7",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DOL7",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "DO7",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LNK7",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "STR7",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "WAIT7",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DLY8",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DOL8",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "DO8",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LNK8",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "STR8",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "WAIT8",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DLY9",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DOL9",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "DO9",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LNK9",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "STR9",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "WAIT9",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    // Step 10 (A suffix)
    FieldDesc {
        name: "DLYA",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DOLA",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "DOA",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LNKA",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "STRA",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "WAITA",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
];

impl Record for SseqRecord {
    fn record_type(&self) -> &'static str {
        "sseq"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // In a real EPICS implementation, processing would:
        // 1. Read SELL link to get SELN
        // 2. For each selected step: wait DLY, read DOL→DO, write DO/STR→LNK
        // Since we can't do async delays or link I/O in process(),
        // we just mark which steps would execute.
        // The actual link processing is handled by the database layer.
        self.busy = 1;

        // Steps are processed by the database layer reading DOL links
        // and writing to LNK links. Our process() just validates state.

        self.busy = 0;
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Long(self.val)),
            "SELM" => Some(EpicsValue::Short(self.selm)),
            "SELN" => Some(EpicsValue::Short(self.seln as i16)),
            "SELL" => Some(EpicsValue::String(self.sell.clone())),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "ABORT" => Some(EpicsValue::Short(self.abort)),
            "BUSY" => Some(EpicsValue::Short(self.busy)),
            _ => {
                if let Some((idx, prefix)) = Self::step_index_from_suffix(name) {
                    let step = &self.steps[idx];
                    return match prefix {
                        "DLY" => Some(EpicsValue::Double(step.dly)),
                        "DOL" => Some(EpicsValue::String(step.dol.clone())),
                        "DO" => Some(EpicsValue::Double(step.dov)),
                        "LNK" => Some(EpicsValue::String(step.lnk.clone())),
                        "STR" => Some(EpicsValue::String(step.str_val.clone())),
                        "WAIT" => Some(EpicsValue::Short(step.wait)),
                        _ => None,
                    };
                }
                None
            }
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                self.val = match value {
                    EpicsValue::Long(v) => v,
                    _ => value
                        .to_f64()
                        .map(|v| v as i32)
                        .ok_or_else(|| CaError::TypeMismatch("VAL".into()))?,
                };
                Ok(())
            }
            "SELM" => match value {
                EpicsValue::Short(v) => {
                    self.selm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SELM".into())),
            },
            "SELN" => match value {
                EpicsValue::Short(v) => {
                    self.seln = v as u16;
                    Ok(())
                }
                _ => {
                    let v = value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch("SELN".into()))?;
                    self.seln = v as u16;
                    Ok(())
                }
            },
            "SELL" => match value {
                EpicsValue::String(s) => {
                    self.sell = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SELL".into())),
            },
            "PREC" => match value {
                EpicsValue::Short(v) => {
                    self.prec = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("PREC".into())),
            },
            "ABORT" => match value {
                EpicsValue::Short(v) => {
                    self.abort = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("ABORT".into())),
            },
            _ => {
                if let Some((idx, prefix)) = Self::step_index_from_suffix(name) {
                    let step = &mut self.steps[idx];
                    return match prefix {
                        "DLY" => {
                            step.dly = value
                                .to_f64()
                                .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                            Ok(())
                        }
                        "DOL" => match value {
                            EpicsValue::String(s) => {
                                step.dol = s;
                                Ok(())
                            }
                            _ => Err(CaError::TypeMismatch(name.into())),
                        },
                        "DO" => {
                            step.dov = value
                                .to_f64()
                                .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                            Ok(())
                        }
                        "LNK" => match value {
                            EpicsValue::String(s) => {
                                step.lnk = s;
                                Ok(())
                            }
                            _ => Err(CaError::TypeMismatch(name.into())),
                        },
                        "STR" => match value {
                            EpicsValue::String(s) => {
                                step.str_val = s;
                                Ok(())
                            }
                            _ => Err(CaError::TypeMismatch(name.into())),
                        },
                        "WAIT" => match value {
                            EpicsValue::Short(v) => {
                                step.wait = v;
                                Ok(())
                            }
                            _ => Err(CaError::TypeMismatch(name.into())),
                        },
                        _ => Err(CaError::FieldNotFound(name.to_string())),
                    };
                }
                Err(CaError::FieldNotFound(name.to_string()))
            }
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        SSEQ_FIELDS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sseq_default() {
        let rec = SseqRecord::new();
        assert_eq!(rec.record_type(), "sseq");
        assert_eq!(rec.val, 0);
        assert_eq!(rec.selm, 0);
        assert_eq!(rec.seln, 1);
    }

    #[test]
    fn test_sseq_put_get_val() {
        let mut rec = SseqRecord::new();
        rec.put_field("VAL", EpicsValue::Long(42)).unwrap();
        assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Long(42)));
    }

    #[test]
    fn test_sseq_put_get_selm() {
        let mut rec = SseqRecord::new();
        rec.put_field("SELM", EpicsValue::Short(2)).unwrap();
        assert_eq!(rec.get_field("SELM"), Some(EpicsValue::Short(2)));
    }

    #[test]
    fn test_sseq_step_fields() {
        let mut rec = SseqRecord::new();
        rec.put_field("DLY1", EpicsValue::Double(1.5)).unwrap();
        rec.put_field("DO1", EpicsValue::Double(42.0)).unwrap();
        rec.put_field("STR1", EpicsValue::String("hello".into()))
            .unwrap();
        rec.put_field("LNK1", EpicsValue::String("target.VAL".into()))
            .unwrap();
        rec.put_field("DOL1", EpicsValue::String("source.VAL".into()))
            .unwrap();
        rec.put_field("WAIT1", EpicsValue::Short(1)).unwrap();

        assert_eq!(rec.get_field("DLY1"), Some(EpicsValue::Double(1.5)));
        assert_eq!(rec.get_field("DO1"), Some(EpicsValue::Double(42.0)));
        assert_eq!(
            rec.get_field("STR1"),
            Some(EpicsValue::String("hello".into()))
        );
        assert_eq!(
            rec.get_field("LNK1"),
            Some(EpicsValue::String("target.VAL".into()))
        );
        assert_eq!(
            rec.get_field("DOL1"),
            Some(EpicsValue::String("source.VAL".into()))
        );
        assert_eq!(rec.get_field("WAIT1"), Some(EpicsValue::Short(1)));
    }

    #[test]
    fn test_sseq_step_a_suffix() {
        let mut rec = SseqRecord::new();
        rec.put_field("DLYA", EpicsValue::Double(2.0)).unwrap();
        rec.put_field("DOA", EpicsValue::Double(99.0)).unwrap();
        rec.put_field("STRA", EpicsValue::String("step10".into()))
            .unwrap();
        rec.put_field("LNKA", EpicsValue::String("out10.VAL".into()))
            .unwrap();

        assert_eq!(rec.get_field("DLYA"), Some(EpicsValue::Double(2.0)));
        assert_eq!(rec.get_field("DOA"), Some(EpicsValue::Double(99.0)));
        assert_eq!(
            rec.get_field("STRA"),
            Some(EpicsValue::String("step10".into()))
        );
        assert_eq!(
            rec.get_field("LNKA"),
            Some(EpicsValue::String("out10.VAL".into()))
        );
    }

    #[test]
    fn test_sseq_all_steps() {
        let mut rec = SseqRecord::new();
        // Set all 10 steps
        for i in 1..=9 {
            let dly_name = format!("DLY{}", i);
            rec.put_field(&dly_name, EpicsValue::Double(i as f64))
                .unwrap();
        }
        rec.put_field("DLYA", EpicsValue::Double(10.0)).unwrap();

        for i in 1..=9 {
            let dly_name = format!("DLY{}", i);
            assert_eq!(rec.get_field(&dly_name), Some(EpicsValue::Double(i as f64)));
        }
        assert_eq!(rec.get_field("DLYA"), Some(EpicsValue::Double(10.0)));
    }

    #[test]
    fn test_sseq_selm_all() {
        let rec = SseqRecord::new();
        for i in 0..NUM_STEPS {
            assert!(rec.should_execute_step(i));
        }
    }

    #[test]
    fn test_sseq_selm_specified() {
        let mut rec = SseqRecord::new();
        rec.selm = 1; // Specified
        rec.seln = 3; // Select step 3
        assert!(!rec.should_execute_step(0));
        assert!(!rec.should_execute_step(1));
        assert!(rec.should_execute_step(2)); // step 3 is index 2
        assert!(!rec.should_execute_step(3));
    }

    #[test]
    fn test_sseq_selm_mask() {
        let mut rec = SseqRecord::new();
        rec.selm = 2; // Mask
        rec.seln = 0b0000_0101; // Steps 1 and 3
        assert!(rec.should_execute_step(0));
        assert!(!rec.should_execute_step(1));
        assert!(rec.should_execute_step(2));
        assert!(!rec.should_execute_step(3));
    }

    #[test]
    fn test_sseq_process() {
        let mut rec = SseqRecord::new();
        rec.process().unwrap();
        assert_eq!(rec.busy, 0);
    }

    #[test]
    fn test_sseq_field_not_found() {
        let mut rec = SseqRecord::new();
        assert!(rec.put_field("ZZZ", EpicsValue::Double(1.0)).is_err());
        assert!(rec.get_field("ZZZ").is_none());
    }

    #[test]
    fn test_sseq_type_mismatch() {
        let mut rec = SseqRecord::new();
        assert!(
            rec.put_field("SELM", EpicsValue::String("x".into()))
                .is_err()
        );
        assert!(rec.put_field("STR1", EpicsValue::Double(1.0)).is_err());
    }

    #[test]
    fn test_sseq_field_list() {
        let rec = SseqRecord::new();
        let fields = rec.field_list();
        // 7 base + 6*10 = 67 fields
        assert!(fields.len() >= 67);
    }
}
