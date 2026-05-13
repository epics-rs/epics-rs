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
    /// subArray-only: starting offset into the source array. Out-of-
    /// range values clamp to the source length; NORD=0 in that case.
    /// Ignored when `kind != SubArray`.
    pub indx: i32,
    /// subArray-only: declared maximum length of the source array.
    /// Used as an additional upper bound when computing the slice end:
    /// `end = min(indx + nelm, min(source_len, malm))`. Defaults to 0
    /// for non-subArray kinds — those records ignore the field
    /// altogether.
    pub malm: i32,
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
            indx: 0,
            malm: 0,
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
            // subArray-specific INDX/MALM fields. Other array record
            // kinds expose them as zero (matches C dbpr output for a
            // record type that doesn't declare the field).
            "INDX" if matches!(self.kind, ArrayKind::SubArray) => Some(EpicsValue::Long(self.indx)),
            "MALM" if matches!(self.kind, ArrayKind::SubArray) => Some(EpicsValue::Long(self.malm)),
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
            "INDX" if matches!(self.kind, ArrayKind::SubArray) => match value {
                EpicsValue::Long(v) => {
                    self.indx = v.max(0);
                    Ok(())
                }
                EpicsValue::Short(v) => {
                    self.indx = (v as i32).max(0);
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INDX".into())),
            },
            "MALM" if matches!(self.kind, ArrayKind::SubArray) => match value {
                EpicsValue::Long(v) => {
                    self.malm = v.max(0);
                    Ok(())
                }
                EpicsValue::Short(v) => {
                    self.malm = (v as i32).max(0);
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("MALM".into())),
            },
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

    /// epics-base PR #a02c310 follow-up: subArray slices its input
    /// array on `set_val`. Effective slice = source[INDX .. INDX+NELM]
    /// further capped by `min(source.len, MALM)` (MALM=0 → no extra
    /// cap, matching C dbCommon defaults where the field is set by
    /// the record initialiser). All other ArrayKind values delegate
    /// to the trait's default `put_field("VAL", ...)` write.
    fn set_val(&mut self, value: EpicsValue) -> CaResult<()> {
        if !matches!(self.kind, ArrayKind::SubArray) {
            return match self.put_field("VAL", value.clone()) {
                Ok(()) => Ok(()),
                Err(CaError::TypeMismatch(_)) => {
                    let target = self
                        .get_field("VAL")
                        .map(|v| v.db_field_type())
                        .unwrap_or(DbFieldType::Double);
                    let coerced = value.convert_to(target);
                    self.put_field("VAL", coerced)
                }
                Err(e) => Err(e),
            };
        }
        let start = self.indx.max(0) as usize;
        let take = self.nelm.max(0) as usize;
        // MALM=0 keeps the legacy "no extra cap" behaviour. When set,
        // it bounds how much of the source we're allowed to look at.
        let malm_cap = if self.malm > 0 {
            self.malm as usize
        } else {
            usize::MAX
        };
        let nelm_buf = take; // physical buffer is sized to NELM
        macro_rules! slice {
            ($v:ident, $arr:ident, $variant:ident, $zero:expr) => {{
                let src_len = $arr.len().min(malm_cap);
                let end = (start + take).min(src_len);
                let valid = if start >= src_len { 0 } else { end - start };
                let mut out: Vec<_> = if valid > 0 {
                    $arr[start..end].to_vec()
                } else {
                    Vec::new()
                };
                out.resize(nelm_buf, $zero);
                self.nord = valid as i32;
                self.val = EpicsValue::$variant(out);
            }};
        }
        match value {
            EpicsValue::CharArray(arr) => slice!(value, arr, CharArray, 0u8),
            EpicsValue::ShortArray(arr) => slice!(value, arr, ShortArray, 0i16),
            EpicsValue::LongArray(arr) => slice!(value, arr, LongArray, 0i32),
            EpicsValue::FloatArray(arr) => slice!(value, arr, FloatArray, 0.0f32),
            EpicsValue::DoubleArray(arr) => slice!(value, arr, DoubleArray, 0.0f64),
            other => {
                // Scalar fed into subArray (e.g. CA put of a single
                // number): degrade to "NORD=1 at offset 0" semantics
                // when INDX==0, else NORD=0. Matches what C does
                // through dbScalarToArray.
                if start == 0 {
                    self.nord = 1;
                    self.val = other;
                } else {
                    self.nord = 0;
                }
            }
        }
        Ok(())
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

    /// PR #a02c310 follow-up: subArray slices source[INDX..INDX+NELM]
    /// into VAL with NORD set to the actual copied length. Source
    /// shorter than INDX → NORD=0. INDX+NELM > source.len → only
    /// available tail is copied, rest zero-padded to NELM.
    #[test]
    fn subarray_slices_input_at_indx_with_nelm_take() {
        let mut r = WaveformRecord::with_kind(ArrayKind::SubArray);
        // 4-element double buffer; consume up to 4 from offset 2.
        r.put_field("NELM", EpicsValue::Long(4)).unwrap();
        r.put_field("INDX", EpicsValue::Long(2)).unwrap();
        let source = EpicsValue::DoubleArray(vec![10.0, 11.0, 12.0, 13.0, 14.0, 15.0]);
        r.set_val(source).unwrap();
        assert_eq!(r.nord, 4, "should copy 4 elements from offset 2");
        let val = r.get_field("VAL").unwrap();
        if let EpicsValue::DoubleArray(v) = val {
            assert_eq!(v, vec![12.0, 13.0, 14.0, 15.0]);
        } else {
            panic!("VAL should be DoubleArray, got {val:?}");
        }
    }

    #[test]
    fn subarray_indx_out_of_range_yields_nord_zero() {
        let mut r = WaveformRecord::with_kind(ArrayKind::SubArray);
        r.put_field("NELM", EpicsValue::Long(3)).unwrap();
        r.put_field("INDX", EpicsValue::Long(10)).unwrap();
        let source = EpicsValue::LongArray(vec![1, 2, 3]);
        r.put_field("FTVL", EpicsValue::Short(5)).unwrap(); // LONG
        r.set_val(source).unwrap();
        assert_eq!(r.nord, 0, "INDX past source.len must zero NORD");
    }

    #[test]
    fn subarray_partial_tail_zero_pads_to_nelm() {
        let mut r = WaveformRecord::with_kind(ArrayKind::SubArray);
        r.put_field("NELM", EpicsValue::Long(5)).unwrap();
        r.put_field("INDX", EpicsValue::Long(3)).unwrap();
        let source = EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0, 5.0]);
        r.set_val(source).unwrap();
        assert_eq!(r.nord, 2, "only 2 elements available from offset 3");
        // get_field("VAL") truncates to NORD — caller-visible slice
        // is only the 2 valid elements.
        if let Some(EpicsValue::DoubleArray(v)) = r.get_field("VAL") {
            assert_eq!(v, vec![4.0, 5.0]);
        } else {
            panic!("VAL must be DoubleArray of valid tail");
        }
    }

    #[test]
    fn subarray_malm_caps_visible_source_length() {
        let mut r = WaveformRecord::with_kind(ArrayKind::SubArray);
        r.put_field("NELM", EpicsValue::Long(4)).unwrap();
        r.put_field("INDX", EpicsValue::Long(0)).unwrap();
        // MALM caps how far into the source we look — even if the
        // source has 8 elements, MALM=3 keeps us to indices [0..3).
        r.put_field("MALM", EpicsValue::Long(3)).unwrap();
        let source = EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
        r.set_val(source).unwrap();
        assert_eq!(r.nord, 3, "MALM=3 limits visible source to 3 elements");
        if let Some(EpicsValue::DoubleArray(v)) = r.get_field("VAL") {
            assert_eq!(v, vec![1.0, 2.0, 3.0]);
        } else {
            panic!("VAL truncated to MALM-bound prefix");
        }
    }

    #[test]
    fn subarray_indx_malm_fields_round_trip() {
        let mut r = WaveformRecord::with_kind(ArrayKind::SubArray);
        r.put_field("INDX", EpicsValue::Long(5)).unwrap();
        r.put_field("MALM", EpicsValue::Long(100)).unwrap();
        assert_eq!(r.get_field("INDX"), Some(EpicsValue::Long(5)));
        assert_eq!(r.get_field("MALM"), Some(EpicsValue::Long(100)));
    }

    #[test]
    fn waveform_does_not_expose_indx_malm() {
        // Non-subArray record kinds must NOT expose INDX/MALM via the
        // field map — those fields are subArray-specific.
        let r = WaveformRecord::with_kind(ArrayKind::Waveform);
        assert!(r.get_field("INDX").is_none());
        assert!(r.get_field("MALM").is_none());
    }
}
