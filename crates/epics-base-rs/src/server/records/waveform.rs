use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, MENU_YES_NO, Record};
use crate::types::{DbFieldType, EpicsValue, PvString};

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
    pub egu: PvString,
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
    /// Simulation block (waveform/aai/aao only; subArray has no sim block).
    /// SIMM is DBF_MENU menu(menuYesNo), SIMS is menu(menuAlarmSevr),
    /// OLDSIMM is menu(menuSimm) special(SPC_NOMOD); SIML/SIOL the sim
    /// in/out links. waveformRecord.dbd.pod:475-507, aaiRecord.dbd.pod:374-402,
    /// aaoRecord.dbd.pod:407-435. SSCN (menuScan) is served by the common
    /// path (common.sscn).
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    pub oldsimm: i16,
}

/// Type aliases for documentation / pattern-match clarity. All point
/// at [`WaveformRecord`] — runtime type discrimination is the
/// [`ArrayKind`] field.
pub type AaiRecord = WaveformRecord;
pub type AaoRecord = WaveformRecord;
pub type SubArrayRecord = WaveformRecord;

/// menuFtype constants for FTVL field.
const MENU_FTYPE_DOUBLE: i16 = 10;

/// `menu(waveformPOST)` choice labels for the `MPST`/`APST` fields, in
/// `.dbd` value order (`waveformRecord.dbd.pod:20-23`). The order is the
/// *reverse* of `menu(menuPost)` — "Always" is index 0 here — and is
/// wire-visible, so this record keeps its own table rather than the
/// shared `MENU_POST`. `aai`/`aao` use the identically-ordered
/// `menu(aaiPOST)`/`menu(aaoPOST)`, so the same table serves every
/// [`ArrayKind`].
const WAVEFORM_POST: &[&str] = &["Always", "On Change"];

/// `menu(waveformPOST)` indices: `Always` posts every cycle, `On Change`
/// posts only when the array-content hash differs from the stored `HASH`.
const WAVEFORM_POST_ALWAYS: i16 = 0;
const WAVEFORM_POST_ONCHANGE: i16 = 1;

/// `epicsOldString` width — a STRING-FTVL element occupies a fixed
/// `MAX_STRING_SIZE`-byte slot in `bptr`, so the hash sees that many bytes
/// per element (null-padded), matching C's raw buffer layout.
const MAX_STRING_SIZE: usize = 40;

/// Port of EPICS `epicsMemHash` (epicsString.c:378-388), the array-content
/// hash used by waveform/aai/aao `monitor()` for On Change detection. It is
/// a Jenkins one-at-a-time variant that consumes bytes in pairs, applying
/// formula A to even byte positions and formula B to odd ones. C
/// dereferences `char` — signed on the x86_64 / aarch64 reference builds —
/// so each byte is sign-extended to 32 bits before the XOR; `b as i8 as
/// u32` reproduces that exactly.
fn epics_mem_hash(bytes: &[u8], seed: u32) -> u32 {
    let mut hash = seed;
    for (i, &b) in bytes.iter().enumerate() {
        let c = b as i8 as u32;
        if i % 2 == 0 {
            hash ^= !((hash << 11) ^ c ^ (hash >> 5));
        } else {
            hash ^= (hash << 7) ^ c ^ (hash >> 3);
        }
    }
    hash
}

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
            egu: PvString::new(),
            hopr: 0.0,
            lopr: 0.0,
            prec: 0,
            indx: 0,
            malm: 0,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            oldsimm: 0,
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

    /// True for the kinds whose `.dbd` declares a simulation block
    /// (waveform/aai/aao). `subArray` is a pure array-slicing record with
    /// no SIMM/SIML/SIOL/SIMS/OLDSIMM fields (`subArrayRecord.dbd.pod`), so
    /// it must not answer those names.
    fn has_sim_block(&self) -> bool {
        !matches!(self.kind, ArrayKind::SubArray)
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
            DbFieldType::Int64 => (EpicsValue::Int64Array(vec![0; nelm as usize]), 7), // INT64
            DbFieldType::UInt64 => (EpicsValue::UInt64Array(vec![0; nelm as usize]), 8), // UINT64
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
            7 => EpicsValue::Int64Array(vec![0; n]),    // INT64
            8 => EpicsValue::UInt64Array(vec![0; n]),   // UINT64
            9 => EpicsValue::FloatArray(vec![0.0; n]),  // FLOAT
            _ => EpicsValue::DoubleArray(vec![0.0; n]), // DOUBLE, etc.
        };
        self.nord = 0;
    }

    /// Resize the VAL buffer to the current NELM **while preserving
    /// existing element data** — shrink truncates, grow zero-pads, and
    /// NORD is clamped to the new length.
    ///
    /// C parity: `waveformRecord` does not support a destructive
    /// run-time NELM change — `init_record` allocates `bptr` once and a
    /// freely-writable NELM that wiped VAL would lose the waveform
    /// contents a CA client just stored. Keeping the data on resize is
    /// the non-destructive equivalent.
    fn resize_val_preserving(&mut self) {
        let n = self.nelm.max(0) as usize;
        match &mut self.val {
            EpicsValue::CharArray(v) => v.resize(n, 0),
            EpicsValue::ShortArray(v) => v.resize(n, 0),
            EpicsValue::LongArray(v) => v.resize(n, 0),
            EpicsValue::Int64Array(v) => v.resize(n, 0),
            EpicsValue::UInt64Array(v) => v.resize(n, 0),
            EpicsValue::FloatArray(v) => v.resize(n, 0.0),
            EpicsValue::DoubleArray(v) => v.resize(n, 0.0),
            EpicsValue::EnumArray(v) => v.resize(n, 0),
            EpicsValue::StringArray(v) => v.resize(n, PvString::new()),
            // VAL is not currently an array variant — fall back to a
            // fresh allocation sized to the new NELM.
            _ => {
                self.reallocate_val();
                return;
            }
        }
        if (self.nord as usize) > n {
            self.nord = n as i32;
        }
    }

    /// Serialize the first `NORD` elements of `VAL` to their native
    /// (little-endian on the reference builds) byte layout — the bytes C
    /// `monitor()` feeds to `epicsMemHash` over `nord * dbValueSize(ftvl)`
    /// (waveformRecord.c:306-307). Each element contributes exactly its
    /// `dbValueSize` bytes; a STRING element occupies a fixed
    /// `MAX_STRING_SIZE` slot, null-padded.
    fn array_content_bytes(&self) -> Vec<u8> {
        let n = self.nord.max(0) as usize;
        let mut out = Vec::new();
        match &self.val {
            EpicsValue::CharArray(v) => out.extend(v.iter().take(n).copied()),
            EpicsValue::ShortArray(v) => {
                for x in v.iter().take(n) {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            EpicsValue::UShortArray(v) | EpicsValue::EnumArray(v) => {
                for x in v.iter().take(n) {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            EpicsValue::LongArray(v) => {
                for x in v.iter().take(n) {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            EpicsValue::ULongArray(v) => {
                for x in v.iter().take(n) {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            EpicsValue::FloatArray(v) => {
                for x in v.iter().take(n) {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            EpicsValue::DoubleArray(v) => {
                for x in v.iter().take(n) {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            EpicsValue::Int64Array(v) => {
                for x in v.iter().take(n) {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            EpicsValue::UInt64Array(v) => {
                for x in v.iter().take(n) {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            EpicsValue::StringArray(v) => {
                for s in v.iter().take(n) {
                    let mut slot = [0u8; MAX_STRING_SIZE];
                    let bytes = s.as_bytes();
                    let copy = bytes.len().min(MAX_STRING_SIZE - 1);
                    slot[..copy].copy_from_slice(&bytes[..copy]);
                    out.extend_from_slice(&slot);
                }
            }
            _ => {}
        }
        out
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
    // Display/control metadata fields. Typed storage + get_field/put_field
    // already back these; they MUST be in field_list so the db loader applies
    // field(EGU/HOPR/LOPR/PREC, ...) to that storage rather than routing them
    // to common fields (where the record's own get_field shadows them with
    // defaults, zeroing DBR_GR/DBR_CTRL limits). waveformRecord.c declares
    // EGU/HOPR/LOPR/PREC as record fields.
    FieldDesc {
        name: "EGU",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "HOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "PREC",
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
    // Display/control metadata fields. Typed storage + get_field/put_field
    // already back these; they MUST be in field_list so the db loader applies
    // field(EGU/HOPR/LOPR/PREC, ...) to that storage rather than routing them
    // to common fields (where the record's own get_field shadows them with
    // defaults, zeroing DBR_GR/DBR_CTRL limits). waveformRecord.c declares
    // EGU/HOPR/LOPR/PREC as record fields.
    FieldDesc {
        name: "EGU",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "HOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "PREC",
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
    // Display/control metadata fields. Typed storage + get_field/put_field
    // already back these; they MUST be in field_list so the db loader applies
    // field(EGU/HOPR/LOPR/PREC, ...) to that storage rather than routing them
    // to common fields (where the record's own get_field shadows them with
    // defaults, zeroing DBR_GR/DBR_CTRL limits). waveformRecord.c declares
    // EGU/HOPR/LOPR/PREC as record fields.
    FieldDesc {
        name: "EGU",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "HOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "PREC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
];

static WAVEFORM_FIELDS_INT64: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Int64,
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
    // Display/control metadata fields. Typed storage + get_field/put_field
    // already back these; they MUST be in field_list so the db loader applies
    // field(EGU/HOPR/LOPR/PREC, ...) to that storage rather than routing them
    // to common fields (where the record's own get_field shadows them with
    // defaults, zeroing DBR_GR/DBR_CTRL limits). waveformRecord.c declares
    // EGU/HOPR/LOPR/PREC as record fields.
    FieldDesc {
        name: "EGU",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "HOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "PREC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
];

static WAVEFORM_FIELDS_UINT64: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::UInt64,
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
    // Display/control metadata fields. Typed storage + get_field/put_field
    // already back these; they MUST be in field_list so the db loader applies
    // field(EGU/HOPR/LOPR/PREC, ...) to that storage rather than routing them
    // to common fields (where the record's own get_field shadows them with
    // defaults, zeroing DBR_GR/DBR_CTRL limits). waveformRecord.c declares
    // EGU/HOPR/LOPR/PREC as record fields.
    FieldDesc {
        name: "EGU",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "HOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "PREC",
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
    // Display/control metadata fields. Typed storage + get_field/put_field
    // already back these; they MUST be in field_list so the db loader applies
    // field(EGU/HOPR/LOPR/PREC, ...) to that storage rather than routing them
    // to common fields (where the record's own get_field shadows them with
    // defaults, zeroing DBR_GR/DBR_CTRL limits). waveformRecord.c declares
    // EGU/HOPR/LOPR/PREC as record fields.
    FieldDesc {
        name: "EGU",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "HOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "PREC",
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
    // Display/control metadata fields. Typed storage + get_field/put_field
    // already back these; they MUST be in field_list so the db loader applies
    // field(EGU/HOPR/LOPR/PREC, ...) to that storage rather than routing them
    // to common fields (where the record's own get_field shadows them with
    // defaults, zeroing DBR_GR/DBR_CTRL limits). waveformRecord.c declares
    // EGU/HOPR/LOPR/PREC as record fields.
    FieldDesc {
        name: "EGU",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "HOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "PREC",
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

    /// `MPST`/`APST` are `DBF_MENU menu(waveformPOST)`
    /// (`waveformRecord.dbd.pod:523-533`), served as `DBR_ENUM`. `FTVL`
    /// (`menu(menuFtype)`) is a shared menu resolved centrally.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "MPST" | "APST" => Some(WAVEFORM_POST),
            // SIMM is menu(menuYesNo) (NO/YES) on the array records. SIMS
            // (menuAlarmSevr) and OLDSIMM (menuSimm) resolve via the shared
            // menu registry. Only the kinds that carry a sim block answer.
            "SIMM" if self.has_sim_block() => Some(MENU_YES_NO),
            _ => None,
        }
    }

    /// C waveform/aai/aao `monitor()` (waveformRecord.c:291-326): MPST/APST
    /// "Always vs On Change" posting. In Always mode the corresponding bit
    /// posts every cycle; in On Change mode the array content is hashed
    /// (`epicsMemHash` over `nord * dbValueSize(ftvl)` native bytes) and the
    /// bit posts — plus `HASH` is updated and reported changed — only when
    /// the hash differs from the stored `HASH`. `subArray` has no such
    /// mechanism, so it (and every non-array record) keeps the default
    /// `None` and the generic deadband decision.
    fn array_monitor_post(&mut self) -> Option<crate::server::record::ArrayMonitorPost> {
        if matches!(self.kind, ArrayKind::SubArray) {
            return None;
        }
        let mut post_value = self.mpst == WAVEFORM_POST_ALWAYS;
        let mut post_archive = self.apst == WAVEFORM_POST_ALWAYS;
        let mut hash_changed = false;
        if self.mpst == WAVEFORM_POST_ONCHANGE || self.apst == WAVEFORM_POST_ONCHANGE {
            let h = epics_mem_hash(&self.array_content_bytes(), 0);
            if h != self.hash {
                self.hash = h;
                hash_changed = true;
                if self.mpst == WAVEFORM_POST_ONCHANGE {
                    post_value = true;
                }
                if self.apst == WAVEFORM_POST_ONCHANGE {
                    post_archive = true;
                }
            }
        }
        Some(crate::server::record::ArrayMonitorPost {
            post_value,
            post_archive,
            hash_changed,
        })
    }

    /// `HASH` is posted by C `monitor()` with a literal `DBE_VALUE` only on
    /// a hash change (waveformRecord.c:317-319), never through VAL's change
    /// detection — exclude it from the generic change-detection loop so it
    /// is neither double-posted nor spuriously posted in Always mode.
    fn event_posted_fields(&self) -> &'static [&'static str] {
        if matches!(self.kind, ArrayKind::SubArray) {
            &[]
        } else {
            &["HASH"]
        }
    }

    // EGU/HOPR/LOPR/PREC are backed by typed storage and exposed through both
    // get_field/put_field and field_list, so populate_display_info reads the
    // loaded values for the DBR_GR display limits (waveformRecord.c:251-252).
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
            "MPST" => Some(EpicsValue::Short(self.mpst)),
            "APST" => Some(EpicsValue::Short(self.apst)),
            // HASH (DBF_ULONG) — the On Change content hash. Only the
            // waveform/aai/aao kinds declare it; subArray has no such field.
            "HASH" if !matches!(self.kind, ArrayKind::SubArray) => {
                Some(EpicsValue::ULong(self.hash))
            }
            // subArray-specific INDX/MALM fields. Other array record
            // kinds expose them as zero (matches C dbpr output for a
            // record type that doesn't declare the field).
            "INDX" if matches!(self.kind, ArrayKind::SubArray) => Some(EpicsValue::Long(self.indx)),
            "MALM" if matches!(self.kind, ArrayKind::SubArray) => Some(EpicsValue::Long(self.malm)),
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "HOPR" => Some(EpicsValue::Double(self.hopr)),
            "LOPR" => Some(EpicsValue::Double(self.lopr)),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            // Simulation block — waveform/aai/aao only (not subArray).
            "SIMM" if self.has_sim_block() => Some(EpicsValue::Short(self.simm)),
            "SIML" if self.has_sim_block() => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" if self.has_sim_block() => Some(EpicsValue::String(self.siol.clone().into())),
            "SIMS" if self.has_sim_block() => Some(EpicsValue::Short(self.sims)),
            "OLDSIMM" if self.has_sim_block() => Some(EpicsValue::Short(self.oldsimm)),
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
                    EpicsValue::Int64Array(mut arr) => {
                        self.nord = arr.len() as i32;
                        arr.resize(nelm, 0);
                        self.val = EpicsValue::Int64Array(arr);
                    }
                    EpicsValue::UInt64Array(mut arr) => {
                        self.nord = arr.len() as i32;
                        arr.resize(nelm, 0);
                        self.val = EpicsValue::UInt64Array(arr);
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
                    // C parity for subArray: clamp NELM <= MALM
                    // (subArrayRecord.c:310-311 in `readValue`,
                    // init at line 103-104). Other array kinds do
                    // not have MALM and are unaffected.
                    if matches!(self.kind, ArrayKind::SubArray) {
                        // subArray: NELM is the slice length, clamped
                        // to MALM; the buffer is re-derived from the
                        // source on `set_val`, so a fresh zeroed
                        // allocation here is correct.
                        self.nelm = if self.malm > 0 { n.min(self.malm) } else { n };
                        self.reallocate_val();
                    } else {
                        // waveform/aai/aao: preserve the existing
                        // element data instead of wiping VAL.
                        self.nelm = n;
                        self.resize_val_preserving();
                    }
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
            "MPST" => {
                if let EpicsValue::Short(v) = value {
                    self.mpst = v;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch("MPST".into()))
                }
            }
            "APST" => {
                if let EpicsValue::Short(v) = value {
                    self.apst = v;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch("APST".into()))
                }
            }
            "NORD" => Err(CaError::ReadOnlyField(name.to_string())),
            "INDX" if matches!(self.kind, ArrayKind::SubArray) => {
                let v = match value {
                    EpicsValue::Long(v) => v,
                    EpicsValue::Short(v) => v as i32,
                    _ => return Err(CaError::TypeMismatch("INDX".into())),
                };
                // C parity (subArrayRecord.c::readValue:313-314):
                // `if (indx >= malm) indx = malm - 1`. When MALM is
                // 0 (not yet configured) keep the legacy `max(0)`
                // floor only.
                let v = v.max(0);
                self.indx = if self.malm > 0 {
                    v.min(self.malm - 1)
                } else {
                    v
                };
                Ok(())
            }
            "MALM" if matches!(self.kind, ArrayKind::SubArray) => {
                let v = match value {
                    EpicsValue::Long(v) => v,
                    EpicsValue::Short(v) => v as i32,
                    _ => return Err(CaError::TypeMismatch("MALM".into())),
                };
                self.malm = v.max(0);
                // C parity (subArrayRecord.c::init_record:103-104):
                // shrinking MALM below NELM also clamps NELM. Apply
                // the same re-clamp on each MALM put.
                if self.malm > 0 && self.nelm > self.malm {
                    self.nelm = self.malm;
                }
                if self.malm > 0 && self.indx >= self.malm {
                    self.indx = self.malm - 1;
                }
                Ok(())
            }
            "EGU" => {
                if let EpicsValue::String(s) = value {
                    self.egu = s;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch("EGU".into()))
                }
            }
            "HOPR" => {
                self.hopr = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("HOPR".into()))?;
                Ok(())
            }
            "LOPR" => {
                self.lopr = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("LOPR".into()))?;
                Ok(())
            }
            "PREC" => {
                self.prec = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("PREC".into()))?
                    as i16;
                Ok(())
            }
            // Simulation block — waveform/aai/aao only (not subArray).
            "SIMM" if self.has_sim_block() => match value {
                EpicsValue::Short(v) => {
                    self.simm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIMM".into())),
            },
            "SIML" if self.has_sim_block() => match value {
                EpicsValue::String(s) => {
                    self.siml = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIML".into())),
            },
            "SIOL" if self.has_sim_block() => match value {
                EpicsValue::String(s) => {
                    self.siol = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIOL".into())),
            },
            "SIMS" if self.has_sim_block() => match value {
                EpicsValue::Short(v) => {
                    self.sims = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("SIMS".into())),
            },
            // OLDSIMM is special(SPC_NOMOD) — saved copy, not client-writable.
            "OLDSIMM" if self.has_sim_block() => Err(CaError::ReadOnlyField(name.to_string())),
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        match self.ftvl {
            1 | 2 => WAVEFORM_FIELDS_CHAR,
            3 | 4 => WAVEFORM_FIELDS_SHORT,
            5 | 6 => WAVEFORM_FIELDS_LONG,
            7 => WAVEFORM_FIELDS_INT64,
            8 => WAVEFORM_FIELDS_UINT64,
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
    fn epics_mem_hash_matches_c_reference_vectors() {
        // Reference values produced by the verbatim C `epicsMemHash`
        // (epicsString.c:378-388) compiled on this machine (signed char,
        // little-endian). The CharArray vector includes high-bit bytes
        // (0x80/0xFF) to pin the signed-char sign extension.
        let mut da = Vec::new();
        da.extend_from_slice(&1.0f64.to_le_bytes());
        da.extend_from_slice(&2.0f64.to_le_bytes());
        assert_eq!(epics_mem_hash(&da, 0), 0xa23a_aba6);

        let mut la = Vec::new();
        for x in [1i32, 2, 3] {
            la.extend_from_slice(&x.to_le_bytes());
        }
        assert_eq!(epics_mem_hash(&la, 0), 0x3429_76d1);

        assert_eq!(epics_mem_hash(&[0x00, 0x80, 0xFF, 0x7F], 0), 0x7be0_007f);
        // Odd length exercises the mid-pair break in the C loop.
        assert_eq!(epics_mem_hash(&[0xAA, 0xBB, 0xCC], 0), 0x06ab_0bfc);
        assert_eq!(epics_mem_hash(&[], 0), 0);
    }

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

    #[test]
    fn br_r13_waveform_ftvl_uint64_storage_and_field_type() {
        // a `waveform` with `FTVL = UINT64` (menuFtype index 8)
        // must allocate a `UInt64Array` VAL buffer and advertise VAL as
        // `DbFieldType::UInt64`. On main FTVL 8 fell through to
        // `DoubleArray` / `DbFieldType::Double`, so unsigned-64 waveforms
        // were not representable.
        let mut r = WaveformRecord::with_kind(ArrayKind::Waveform);
        r.put_field("NELM", EpicsValue::Long(3)).unwrap();
        r.put_field("FTVL", EpicsValue::Short(8)).unwrap(); // UINT64

        // VAL buffer is a UInt64Array (NORD=0 fresh → empty, still typed).
        match r.get_field("VAL") {
            Some(EpicsValue::UInt64Array(_)) => {}
            other => panic!("FTVL=UINT64 VAL must be UInt64Array, got {other:?}"),
        }

        // QSRV introspects VAL through field_list — must be UInt64.
        let val_dbf = r
            .field_list()
            .iter()
            .find(|f| f.name == "VAL")
            .map(|f| f.dbf_type);
        assert_eq!(val_dbf, Some(DbFieldType::UInt64));

        // A value above i64::MAX round-trips without precision loss.
        let big = u64::MAX - 9;
        r.put_field("VAL", EpicsValue::UInt64Array(vec![big, 0, 1]))
            .unwrap();
        match r.get_field("VAL") {
            Some(EpicsValue::UInt64Array(v)) => assert_eq!(v[0], big),
            other => panic!("expected UInt64Array, got {other:?}"),
        }

        // INT64 (index 7) likewise allocates a typed Int64Array buffer.
        let mut r2 = WaveformRecord::with_kind(ArrayKind::Waveform);
        r2.put_field("NELM", EpicsValue::Long(2)).unwrap();
        r2.put_field("FTVL", EpicsValue::Short(7)).unwrap(); // INT64
        assert!(matches!(
            r2.get_field("VAL"),
            Some(EpicsValue::Int64Array(_))
        ));
        let i64_dbf = r2
            .field_list()
            .iter()
            .find(|f| f.name == "VAL")
            .map(|f| f.dbf_type);
        assert_eq!(i64_dbf, Some(DbFieldType::Int64));
    }

    /// MPST/APST are `menu(waveformPOST)` served as DBR_ENUM. The base
    /// snapshot path promotes the stored Short to `Enum` and attaches the
    /// labels in `.dbd` value order — which is REVERSED vs `menu(menuPost)`:
    /// "Always" is index 0, "On Change" is index 1.
    #[test]
    fn waveform_mpst_apst_snapshot_is_enum_with_reversed_post_labels() {
        use crate::server::record::RecordInstance;
        let mut rec = WaveformRecord::with_kind(ArrayKind::Waveform);
        rec.put_field("MPST", EpicsValue::Short(0)).unwrap();
        assert_eq!(rec.get_field("MPST"), Some(EpicsValue::Short(0)));
        let inst = RecordInstance::new("WF:MPST".into(), rec);
        let snap = inst.snapshot_for_field("MPST").unwrap();
        assert_eq!(snap.value, EpicsValue::Enum(0));
        assert_eq!(
            snap.enums.as_ref().unwrap().strings,
            vec!["Always", "On Change"],
            "waveformPOST index 0 must be \"Always\" (reverse of menuPost)"
        );
    }

    /// The simulation block (SIMM/SIML/SIOL/SIMS/OLDSIMM) is served on
    /// waveform/aai/aao but NOT subArray. SIMM is menu(menuYesNo); SIMS
    /// (menuAlarmSevr) and OLDSIMM (menuSimm) resolve via the shared
    /// registry, so their wire labels come from the central tables.
    #[test]
    fn waveform_sim_block_served_per_kind() {
        use crate::server::record::RecordInstance;

        for kind in [ArrayKind::Waveform, ArrayKind::Aai, ArrayKind::Aao] {
            let mut rec = WaveformRecord::with_kind(kind);
            rec.put_field("SIMM", EpicsValue::Short(1)).unwrap();
            assert_eq!(rec.get_field("SIMM"), Some(EpicsValue::Short(1)));
            rec.put_field("SIML", EpicsValue::String("sim:mode".into()))
                .unwrap();
            assert_eq!(
                rec.get_field("SIML"),
                Some(EpicsValue::String("sim:mode".into()))
            );
            rec.put_field("SIOL", EpicsValue::String("sim:in".into()))
                .unwrap();
            assert_eq!(
                rec.get_field("SIOL"),
                Some(EpicsValue::String("sim:in".into()))
            );
            rec.put_field("SIMS", EpicsValue::Short(2)).unwrap();
            assert_eq!(rec.get_field("SIMS"), Some(EpicsValue::Short(2)));
            // OLDSIMM is special(SPC_NOMOD) — readable, not writable.
            assert!(matches!(
                rec.put_field("OLDSIMM", EpicsValue::Short(1)),
                Err(crate::error::CaError::ReadOnlyField(_))
            ));
            assert_eq!(rec.get_field("OLDSIMM"), Some(EpicsValue::Short(0)));
        }

        // SIMM snapshot carries the NO/YES menuYesNo labels on these records.
        let mut wf = WaveformRecord::with_kind(ArrayKind::Waveform);
        wf.put_field("SIMM", EpicsValue::Short(1)).unwrap();
        let inst = RecordInstance::new("WF:SIMM".into(), wf);
        let snap = inst.snapshot_for_field("SIMM").unwrap();
        assert_eq!(snap.value, EpicsValue::Enum(1));
        assert_eq!(snap.enums.as_ref().unwrap().strings, vec!["NO", "YES"]);
        // OLDSIMM resolves to the three-choice menuSimm via the shared registry.
        let snap_old = inst.snapshot_for_field("OLDSIMM").unwrap();
        assert_eq!(
            snap_old.enums.as_ref().unwrap().strings,
            vec!["NO", "YES", "RAW"]
        );

        // subArray has no sim block — those names must not resolve.
        let sub = WaveformRecord::with_kind(ArrayKind::SubArray);
        assert_eq!(sub.get_field("SIMM"), None);
        assert_eq!(sub.get_field("OLDSIMM"), None);
        let mut sub_mut = WaveformRecord::with_kind(ArrayKind::SubArray);
        assert!(matches!(
            sub_mut.put_field("SIMM", EpicsValue::Short(1)),
            Err(crate::error::CaError::FieldNotFound(_))
        ));
    }

    #[test]
    fn br_r13_waveform_new_from_uint64_dbf_type() {
        // `WaveformRecord::new(_, DbFieldType::UInt64)` must mint
        // a UInt64Array VAL and FTVL index 8, not fall through to Double.
        let r = WaveformRecord::new(4, DbFieldType::UInt64);
        assert_eq!(r.ftvl, 8);
        // The VAL buffer is a UInt64Array sized to NELM; `get_field`
        // truncates to NORD (0 when fresh), so check the buffer directly.
        assert!(matches!(&r.val, EpicsValue::UInt64Array(v) if v.len() == 4));
    }
}
