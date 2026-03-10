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
        let val = match ftvl {
            DbFieldType::Double => EpicsValue::DoubleArray(vec![0.0; nelm as usize]),
            DbFieldType::Long => EpicsValue::LongArray(vec![0; nelm as usize]),
            DbFieldType::Char => EpicsValue::CharArray(vec![0; nelm as usize]),
            _ => EpicsValue::DoubleArray(vec![0.0; nelm as usize]),
        };
        Self {
            val,
            nelm,
            nord: 0,
            ftvl: ftvl as i16,
        }
    }

    /// Reallocate VAL buffer to match current FTVL and NELM.
    ///
    /// FTVL uses menuFtype indices: CHAR=1, LONG=5, FLOAT=9, DOUBLE=10, etc.
    fn reallocate_val(&mut self) {
        let n = self.nelm.max(0) as usize;
        self.val = match self.ftvl {
            5 | 6 => EpicsValue::LongArray(vec![0; n]),       // LONG, ULONG
            1 | 2 => EpicsValue::CharArray(vec![0; n]),       // CHAR, UCHAR
            _ => EpicsValue::DoubleArray(vec![0.0; n]),       // DOUBLE, FLOAT, etc.
        };
        self.nord = 0;
    }
}

static WAVEFORM_FIELDS: &[FieldDesc] = &[
    FieldDesc { name: "VAL", dbf_type: DbFieldType::Double, read_only: false },
    FieldDesc { name: "NELM", dbf_type: DbFieldType::Long, read_only: false },
    FieldDesc { name: "NORD", dbf_type: DbFieldType::Long, read_only: true },
    FieldDesc { name: "FTVL", dbf_type: DbFieldType::Short, read_only: false },
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
                // Update NORD based on actual data length
                match &value {
                    EpicsValue::DoubleArray(arr) => self.nord = arr.len() as i32,
                    EpicsValue::LongArray(arr) => self.nord = arr.len() as i32,
                    EpicsValue::CharArray(arr) => self.nord = arr.len() as i32,
                    _ => self.nord = 1,
                }
                self.val = value;
                Ok(())
            }
            "NELM" => {
                if let EpicsValue::Long(n) = value {
                    self.nelm = n;
                    self.reallocate_val();
                    Ok(())
                } else {
                    Err(CaError::InvalidValue(format!("NELM requires Long, got {value:?}")))
                }
            }
            "FTVL" => {
                if let EpicsValue::Short(v) = value {
                    self.ftvl = v;
                    self.reallocate_val();
                    Ok(())
                } else {
                    Err(CaError::InvalidValue(format!("FTVL requires Short, got {value:?}")))
                }
            }
            "NORD" => {
                Err(CaError::ReadOnlyField(name.to_string()))
            }
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        WAVEFORM_FIELDS
    }
}
