use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Waveform record — manual Record impl (no macro).
pub struct WaveformRecord {
    pub val: EpicsValue,
    pub nelm: i32,
    pub nord: i32,
    pub ftvl: i16,
}

/// menuFtype constants for FTVL field.
const MENU_FTYPE_DOUBLE: i16 = 10;

impl Default for WaveformRecord {
    fn default() -> Self {
        Self {
            val: EpicsValue::DoubleArray(Vec::new()),
            nelm: 1,
            nord: 0,
            ftvl: MENU_FTYPE_DOUBLE,
        }
    }
}

impl WaveformRecord {
    pub fn new(nelm: i32, ftvl: DbFieldType) -> Self {
        // Map DBR type to menuFtype index for the ftvl field.
        // DBR and menuFtype have different numbering.
        let (val, ftvl_idx) = match ftvl {
            DbFieldType::Char => (EpicsValue::CharArray(vec![0; nelm as usize]), 1), // CHAR
            DbFieldType::Short => (EpicsValue::ShortArray(vec![0; nelm as usize]), 3), // SHORT
            DbFieldType::Long => (EpicsValue::LongArray(vec![0; nelm as usize]), 5), // LONG
            DbFieldType::Float => (EpicsValue::FloatArray(vec![0.0; nelm as usize]), 9), // FLOAT
            DbFieldType::Double => (EpicsValue::DoubleArray(vec![0.0; nelm as usize]), 10), // DOUBLE
            _ => (EpicsValue::DoubleArray(vec![0.0; nelm as usize]), 10),
        };
        Self {
            val,
            nelm,
            nord: 0,
            ftvl: ftvl_idx,
        }
    }

    /// Reallocate VAL buffer to match current FTVL and NELM.
    ///
    /// menuFtype indices: STRING=0, CHAR=1, UCHAR=2, SHORT=3, USHORT=4,
    /// LONG=5, ULONG=6, INT64=7, UINT64=8, FLOAT=9, DOUBLE=10, ENUM=11
    fn reallocate_val(&mut self) {
        let n = self.nelm.max(0) as usize;
        self.val = match self.ftvl {
            1 | 2 => EpicsValue::CharArray(vec![0; n]), // CHAR, UCHAR
            3 | 4 => EpicsValue::ShortArray(vec![0; n]), // SHORT, USHORT
            5 | 6 => EpicsValue::LongArray(vec![0; n]), // LONG, ULONG
            9 => EpicsValue::FloatArray(vec![0.0; n]),  // FLOAT
            _ => EpicsValue::DoubleArray(vec![0.0; n]), // DOUBLE, etc.
        };
        self.nord = 0;
    }
}

static WAVEFORM_FIELDS_CHAR: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Char,
        read_only: false,
    },
    FieldDesc {
        name: "NELM",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "NORD",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "FTVL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
];

static WAVEFORM_FIELDS_SHORT: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "NELM",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "NORD",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "FTVL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
];

static WAVEFORM_FIELDS_LONG: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "NELM",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "NORD",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "FTVL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
];

static WAVEFORM_FIELDS_FLOAT: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Float,
        read_only: false,
    },
    FieldDesc {
        name: "NELM",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "NORD",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "FTVL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
];

static WAVEFORM_FIELDS_DOUBLE: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "NELM",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "NORD",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "FTVL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
];

impl Record for WaveformRecord {
    fn record_type(&self) -> &'static str {
        "waveform"
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(self.val.clone()),
            "NELM" => Some(EpicsValue::Long(self.nelm)),
            "NORD" => Some(EpicsValue::Long(self.nord)),
            "FTVL" => Some(EpicsValue::Short(self.ftvl)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                // Coerce value to match FTVL (e.g. String → CharArray for FTVL=CHAR)
                let value = match (&value, self.ftvl) {
                    (EpicsValue::String(s), 1 | 2) => EpicsValue::CharArray(s.as_bytes().to_vec()),
                    _ => value,
                };
                // Update NORD based on actual data length, but keep array
                // at NELM size to preserve CA channel element count.
                let nelm = self.nelm.max(0) as usize;
                match value {
                    EpicsValue::CharArray(mut arr) => {
                        self.nord = arr.len() as i32;
                        arr.resize(nelm, 0);
                        self.val = EpicsValue::CharArray(arr);
                    }
                    EpicsValue::ShortArray(mut arr) => {
                        self.nord = arr.len() as i32;
                        arr.resize(nelm, 0);
                        self.val = EpicsValue::ShortArray(arr);
                    }
                    EpicsValue::LongArray(mut arr) => {
                        self.nord = arr.len() as i32;
                        arr.resize(nelm, 0);
                        self.val = EpicsValue::LongArray(arr);
                    }
                    EpicsValue::FloatArray(mut arr) => {
                        self.nord = arr.len() as i32;
                        arr.resize(nelm, 0.0);
                        self.val = EpicsValue::FloatArray(arr);
                    }
                    EpicsValue::DoubleArray(mut arr) => {
                        self.nord = arr.len() as i32;
                        arr.resize(nelm, 0.0);
                        self.val = EpicsValue::DoubleArray(arr);
                    }
                    other => {
                        self.nord = 1;
                        self.val = other;
                    }
                }
                Ok(())
            }
            "NELM" => {
                if let EpicsValue::Long(n) = value {
                    self.nelm = n;
                    self.reallocate_val();
                    Ok(())
                } else {
                    Err(CaError::InvalidValue(format!(
                        "NELM requires Long, got {value:?}"
                    )))
                }
            }
            "FTVL" => {
                if let EpicsValue::Short(v) = value {
                    self.ftvl = v;
                    self.reallocate_val();
                    Ok(())
                } else {
                    Err(CaError::InvalidValue(format!(
                        "FTVL requires Short, got {value:?}"
                    )))
                }
            }
            "NORD" => Err(CaError::ReadOnlyField(name.to_string())),
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        match self.ftvl {
            1 | 2 => WAVEFORM_FIELDS_CHAR,
            3 | 4 => WAVEFORM_FIELDS_SHORT,
            5 | 6 => WAVEFORM_FIELDS_LONG,
            9 => WAVEFORM_FIELDS_FLOAT,
            _ => WAVEFORM_FIELDS_DOUBLE,
        }
    }
}
