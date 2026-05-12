use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Which EPICS record-type name an [`ArrayRecord`] reports. The four
/// upstream array record types (`waveform`, `aai`, `aao`, `subArray`)
/// share the same scalar fields and DBR encoding; differentiation is
/// only at the record-type string and (for `aao`) the output-record
/// flag. Keeping them as one storage type avoids 1500 LOC of
/// duplication while preserving each type's identity to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayKind {
    Waveform,
    Aai,
    Aao,
    SubArray,
}

impl ArrayKind {
    pub fn as_record_type(self) -> &'static str {
        match self {
            Self::Waveform => "waveform",
            Self::Aai => "aai",
            Self::Aao => "aao",
            Self::SubArray => "subArray",
        }
    }

    /// `aao` is an output record (the framework calls `device.write`);
    /// the rest are input. Drives [`Record::can_device_write`].
    pub fn is_output(self) -> bool {
        matches!(self, Self::Aao)
    }
}

/// Waveform record — manual Record impl (no macro). Also serves as the
/// storage for `aai`, `aao`, and `subArray` since the four share their
/// scalar surface. The [`Self::kind`] field selects the reported
/// `record_type()` and the output/input distinction.
pub struct WaveformRecord {
    pub kind: ArrayKind,
    pub val: EpicsValue,
    pub nelm: i32,
    pub nord: i32,
    pub ftvl: i16,
    pub mpst: i16,  // Monitor Post Mode: 0=Always, 1=OnChange
    pub apst: i16,  // Archive Post Mode: 0=Always, 1=OnChange
    pub hash: u32,  // Hash of array for OnChange detection
    pub busy: bool, // Record is busy (async operation pending)
    pub egu: String,
    pub hopr: f64,
    pub lopr: f64,
    pub prec: i16,
}

/// Type aliases for documentation / pattern-match clarity. All point
/// at [`WaveformRecord`] — runtime type discrimination is the
/// [`ArrayKind`] field.
pub type AaiRecord = WaveformRecord;
pub type AaoRecord = WaveformRecord;
pub type SubArrayRecord = WaveformRecord;

/// menuFtype constants for FTVL field.
const MENU_FTYPE_DOUBLE: i16 = 10;

impl Default for WaveformRecord {
    fn default() -> Self {
        Self {
            kind: ArrayKind::Waveform,
            val: EpicsValue::DoubleArray(Vec::new()),
            nelm: 1,
            nord: 0,
            ftvl: MENU_FTYPE_DOUBLE,
            mpst: 0,
            apst: 0,
            hash: 0,
            busy: false,
            egu: String::new(),
            hopr: 0.0,
            lopr: 0.0,
            prec: 0,
        }
    }
}

impl WaveformRecord {
    /// Construct an array record with an explicit [`ArrayKind`].
    /// Lets `db_loader::create_record` mint `aai`, `aao`, or `subArray`
    /// without needing distinct types per record-type name.
    pub fn with_kind(kind: ArrayKind) -> Self {
        Self {
            kind,
            ..Default::default()
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
            ..Default::default()
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
        self.kind.as_record_type()
    }

    /// `aao` is an output record; the rest of the array family read
    /// from INP. Output records take the device-write path in
    /// processing.rs (or fall through to the soft-link write when the
    /// DTYP is empty / "Soft Channel").
    fn can_device_write(&self) -> bool {
        self.kind.is_output()
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => {
                // Return only NORD valid elements, not the full NELM buffer.
                // CA clients use the returned element count to interpret the
                // data (e.g. PyDMImageView computes height = count / width).
                let mut val = self.val.clone();
                val.truncate(self.nord.max(0) as usize);
                Some(val)
            }
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
                    if n <= 0 {
                        return Err(CaError::InvalidValue(format!(
                            "NELM must be positive, got {n}"
                        )));
                    }
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

#[cfg(test)]
mod array_kind_tests {
    use super::*;

    #[test]
    fn waveform_default_kind() {
        let r = WaveformRecord::default();
        assert_eq!(r.record_type(), "waveform");
        assert!(!r.can_device_write(), "waveform is input-only");
    }

    #[test]
    fn aai_record_type_and_input() {
        let r = WaveformRecord::with_kind(ArrayKind::Aai);
        assert_eq!(r.record_type(), "aai");
        assert!(!r.can_device_write(), "aai is input");
    }

    #[test]
    fn aao_is_output() {
        let r = WaveformRecord::with_kind(ArrayKind::Aao);
        assert_eq!(r.record_type(), "aao");
        assert!(r.can_device_write(), "aao must take the device-write path");
    }

    #[test]
    fn sub_array_record_type() {
        let r = WaveformRecord::with_kind(ArrayKind::SubArray);
        assert_eq!(r.record_type(), "subArray");
        assert!(!r.can_device_write(), "subArray is input");
    }

    #[test]
    fn aliases_resolve_to_waveform_record() {
        // The type aliases are documentation-only; constructing
        // through them must yield the same concrete struct.
        let a: AaiRecord = WaveformRecord::with_kind(ArrayKind::Aai);
        let b: AaoRecord = WaveformRecord::with_kind(ArrayKind::Aao);
        let c: SubArrayRecord = WaveformRecord::with_kind(ArrayKind::SubArray);
        assert_eq!(a.record_type(), "aai");
        assert_eq!(b.record_type(), "aao");
        assert_eq!(c.record_type(), "subArray");
    }
}
