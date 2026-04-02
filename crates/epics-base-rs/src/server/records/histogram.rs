use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Histogram record — counts values into buckets.
pub struct HistogramRecord {
    pub val: Vec<i32>, // Bucket counts
    pub nelm: i32,     // Number of buckets
    pub ulim: f64,     // Upper limit
    pub llim: f64,     // Lower limit
    pub sgnl: String,  // Signal input link
    pub cmd: i16,      // 0=Read, 1=Clear
    pub sdel: f64,     // Signal deadband
}

impl Default for HistogramRecord {
    fn default() -> Self {
        Self {
            val: vec![0; 10],
            nelm: 10,
            ulim: 10.0,
            llim: 0.0,
            sgnl: String::new(),
            cmd: 0,
            sdel: 0.0,
        }
    }
}

impl HistogramRecord {
    pub fn new(nelm: i32, llim: f64, ulim: f64) -> Self {
        Self {
            val: vec![0; nelm as usize],
            nelm,
            ulim,
            llim,
            ..Default::default()
        }
    }

    /// Add a sample value to the histogram.
    pub fn add_sample(&mut self, value: f64) {
        if value < self.llim || value >= self.ulim || self.nelm <= 0 {
            return; // Out of range
        }
        let range = self.ulim - self.llim;
        if range <= 0.0 {
            return;
        }
        let bucket = ((value - self.llim) / range * self.nelm as f64) as usize;
        let bucket = bucket.min(self.nelm as usize - 1);
        self.val[bucket] += 1;
    }
}

static HISTOGRAM_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "NELM",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "ULIM",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LLIM",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "SGNL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "CMD",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SDEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
];

impl Record for HistogramRecord {
    fn record_type(&self) -> &'static str {
        "histogram"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if self.cmd == 1 {
            for v in &mut self.val {
                *v = 0;
            }
            self.cmd = 0;
        }
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::LongArray(self.val.clone())),
            "NELM" => Some(EpicsValue::Long(self.nelm)),
            "ULIM" => Some(EpicsValue::Double(self.ulim)),
            "LLIM" => Some(EpicsValue::Double(self.llim)),
            "SGNL" => Some(EpicsValue::String(self.sgnl.clone())),
            "CMD" => Some(EpicsValue::Short(self.cmd)),
            "SDEL" => Some(EpicsValue::Double(self.sdel)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::LongArray(arr) => {
                    self.val = arr;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            "ULIM" => match value {
                EpicsValue::Double(v) => {
                    self.ulim = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("ULIM".into())),
            },
            "LLIM" => match value {
                EpicsValue::Double(v) => {
                    self.llim = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("LLIM".into())),
            },
            "SGNL" => match value {
                EpicsValue::String(s) => {
                    self.sgnl = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SGNL".into())),
            },
            "CMD" => match value {
                EpicsValue::Short(v) => {
                    self.cmd = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("CMD".into())),
            },
            "SDEL" => match value {
                EpicsValue::Double(v) => {
                    self.sdel = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SDEL".into())),
            },
            "NELM" => Err(CaError::ReadOnlyField(name.to_string())),
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        HISTOGRAM_FIELDS
    }

    fn primary_field(&self) -> &'static str {
        "VAL"
    }
}
