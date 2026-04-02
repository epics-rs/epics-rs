use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Compress record — circular buffer with compression algorithms.
pub struct CompressRecord {
    pub val: Vec<f64>,
    pub nsam: i32, // Number of samples (buffer size)
    pub alg: i16,  // 0=N to 1 Low, 1=N to 1 High, 2=N to 1 Mean, 3=Circular Buffer
    pub n: i32,    // Number of values to compress
    pub off: i32,  // Current write offset
    // Internal accumulator for N-to-1 algorithms
    accum: Vec<f64>,
}

impl Default for CompressRecord {
    fn default() -> Self {
        Self {
            val: vec![0.0; 10],
            nsam: 10,
            alg: 3, // Circular Buffer by default
            n: 1,
            off: 0,
            accum: Vec::new(),
        }
    }
}

impl CompressRecord {
    pub fn new(nsam: i32, alg: i16) -> Self {
        Self {
            val: vec![0.0; nsam as usize],
            nsam,
            alg,
            ..Default::default()
        }
    }

    /// Push a value into the compress record.
    pub fn push_value(&mut self, input: f64) {
        match self.alg {
            3 => {
                // Circular buffer
                let idx = self.off as usize % self.nsam as usize;
                self.val[idx] = input;
                self.off += 1;
            }
            _ => {
                // N-to-1 algorithms
                self.accum.push(input);
                if self.accum.len() >= self.n as usize {
                    let compressed = match self.alg {
                        0 => self.accum.iter().cloned().fold(f64::INFINITY, f64::min), // Low
                        1 => self.accum.iter().cloned().fold(f64::NEG_INFINITY, f64::max), // High
                        2 => self.accum.iter().sum::<f64>() / self.accum.len() as f64, // Mean
                        _ => 0.0,
                    };
                    let idx = self.off as usize % self.nsam as usize;
                    self.val[idx] = compressed;
                    self.off += 1;
                    self.accum.clear();
                }
            }
        }
    }
}

static COMPRESS_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "NSAM",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "ALG",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "N",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "OFF",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
];

impl Record for CompressRecord {
    fn record_type(&self) -> &'static str {
        "compress"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::DoubleArray(self.val.clone())),
            "NSAM" => Some(EpicsValue::Long(self.nsam)),
            "ALG" => Some(EpicsValue::Short(self.alg)),
            "N" => Some(EpicsValue::Long(self.n)),
            "OFF" => Some(EpicsValue::Long(self.off)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::DoubleArray(arr) => {
                    self.val = arr;
                    Ok(())
                }
                EpicsValue::Double(v) => {
                    self.push_value(v);
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            "ALG" => match value {
                EpicsValue::Short(v) => {
                    self.alg = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("ALG".into())),
            },
            "N" => match value {
                EpicsValue::Long(v) => {
                    self.n = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("N".into())),
            },
            "NSAM" | "OFF" => Err(CaError::ReadOnlyField(name.to_string())),
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        COMPRESS_FIELDS
    }

    fn primary_field(&self) -> &'static str {
        "VAL"
    }
}
