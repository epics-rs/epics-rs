use crate::error::{CaError, CaResult};
use crate::server::record::{
    FieldDesc, MENU_YES_NO, ParsedLink, ProcessAction, Record, parse_link_v2,
};
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
    pub mpst: i16, // Monitor Post Mode: 0=Always, 1=OnChange
    pub apst: i16, // Archive Post Mode: 0=Always, 1=OnChange
    pub hash: u32, // Hash of array for OnChange detection
    /// C `BUSY` (`DBF_SHORT`, `special(SPC_NOMOD)`): waveform acquisition-active
    /// flag, set by waveform device support (e.g. `devAsynXXXTimeSeries`) and
    /// read-only to CA clients. waveformRecord.dbd.pod:461. Waveform kind only.
    pub busy: bool,
    /// C `RARM` (`DBF_SHORT`, `pp(TRUE)`): re-arm acquisition control read by
    /// waveform device support: 1=start (clear, arm), 2=stop, 3=resume, 0=no-op.
    /// The device resets it to 0 each process. waveformRecord.dbd.pod:411.
    /// Waveform kind only (aai/aao/subArray do not declare it).
    pub rarm: i16,
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
    /// SIMM is DBF_MENU menu(menuYesNo), SIMS is menu(menuAlarmSevr);
    /// SIML/SIOL the sim in/out links. waveformRecord.dbd.pod:475-507,
    /// aaiRecord.dbd.pod:374-402, aaoRecord.dbd.pod:407-435. SSCN (menuScan)
    /// and OLDSIMM (menuSimm, SPC_NOMOD) are served by the common path
    /// (`CommonFields::sscn` / `CommonFields::oldsimm`) — framework state
    /// written only by the simulation-mode owner
    /// (`RecordInstance::rec_gbl_save_simm` / `rec_gbl_check_simm`).
    pub simm: i16,
    pub siml: String,
    pub siol: String,
    pub sims: i16,
    /// aao-only: output mode select, `menu(menuOmsl)` (0=supervisory,
    /// 1=closed_loop). When `closed_loop`, aao sources VAL from `DOL`
    /// before each write (C `aaoRecord.c::fetchValue`, 357). waveform/aai/
    /// subArray declare no OMSL — `aaoRecord.dbd.pod:355` is the only one
    /// of the four that does — so the field is exposed only when
    /// `kind == Aao`.
    pub omsl: i16,
    /// aao-only: desired-output-location input link. Pulled into VAL each
    /// process cycle when `omsl == closed_loop` and the link is a real
    /// (non-constant) link (C `aaoRecord.c::fetchValue` `dbGetLink`, 366).
    pub dol: String,
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
            // Intentional deviation: C `field(FTVL,DBF_MENU){ menu(menuFtype) }`
            // (waveform/aai/aao) carries no `initial(...)`, so C defaults FTVL
            // to menuFtype index 0 = DBF_STRING. The port has no string-array
            // waveform support (`reallocate_val` has no StringArray branch), so
            // a STRING default would leave FTVL=STRING with a DoubleArray VAL —
            // inconsistent. DOUBLE keeps FTVL and the VAL buffer type in sync;
            // a .db that sets FTVL re-derives VAL via the FTVL put, so this
            // default only applies to a (degenerate) omitted-FTVL waveform.
            ftvl: MENU_FTYPE_DOUBLE,
            mpst: 0,
            apst: 0,
            hash: 0,
            busy: false,
            rarm: 0,
            egu: PvString::new(),
            hopr: 0.0,
            lopr: 0.0,
            prec: 0,
            indx: 0,
            // C `subArrayRecord.dbd.pod` `field(MALM,DBF_ULONG){ initial("1") }`;
            // C `init_record` also floors MALM to 1 (subArrayRecord.c:96-97), so
            // MALM is never 0 — it is always a real source-view cap. (Ignored by
            // non-subArray kinds, which never read the field.)
            malm: 1,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sims: 0,
            // C `aaoRecord.dbd.pod` declares OMSL `menu(menuOmsl)` and DOL
            // `DBF_INLINK` with no `initial(...)`, so both default to the
            // zero value: OMSL=supervisory (no DOL fetch), DOL=constant/empty.
            omsl: 0,
            dol: String::new(),
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
            DbFieldType::UChar => (EpicsValue::UCharArray(vec![0; nelm as usize]), 2), // UCHAR
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
            1 => EpicsValue::CharArray(vec![0; n]),  // CHAR (epicsInt8)
            2 => EpicsValue::UCharArray(vec![0; n]), // UCHAR (epicsUInt8)
            3 | 4 => EpicsValue::ShortArray(vec![0; n]), // SHORT, USHORT
            5 | 6 => EpicsValue::LongArray(vec![0; n]), // LONG, ULONG
            7 => EpicsValue::Int64Array(vec![0; n]), // INT64
            8 => EpicsValue::UInt64Array(vec![0; n]), // UINT64
            9 => EpicsValue::FloatArray(vec![0.0; n]), // FLOAT
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
            EpicsValue::UCharArray(v) => v.resize(n, 0),
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
        read_only: true,
    },
    FieldDesc {
        name: "NORD",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "FTVL",
        dbf_type: DbFieldType::Short,
        read_only: true,
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
    // Waveform-only TimeSeries acquisition control (devAsynXXXTimeSeries):
    // RARM `pp(TRUE)` client-settable, BUSY `special(SPC_NOMOD)` device-set.
    // aai/aao/subArray do not declare these (they use their own field sets).
    // waveformRecord.dbd.pod:411,461.
    FieldDesc {
        name: "RARM",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "BUSY",
        dbf_type: DbFieldType::Short,
        read_only: true,
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
        read_only: true,
    },
    FieldDesc {
        name: "NORD",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "FTVL",
        dbf_type: DbFieldType::Short,
        read_only: true,
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
    // Waveform-only TimeSeries acquisition control (devAsynXXXTimeSeries):
    // RARM `pp(TRUE)` client-settable, BUSY `special(SPC_NOMOD)` device-set.
    // aai/aao/subArray do not declare these (they use their own field sets).
    // waveformRecord.dbd.pod:411,461.
    FieldDesc {
        name: "RARM",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "BUSY",
        dbf_type: DbFieldType::Short,
        read_only: true,
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
        read_only: true,
    },
    FieldDesc {
        name: "NORD",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "FTVL",
        dbf_type: DbFieldType::Short,
        read_only: true,
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
    // Waveform-only TimeSeries acquisition control (devAsynXXXTimeSeries):
    // RARM `pp(TRUE)` client-settable, BUSY `special(SPC_NOMOD)` device-set.
    // aai/aao/subArray do not declare these (they use their own field sets).
    // waveformRecord.dbd.pod:411,461.
    FieldDesc {
        name: "RARM",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "BUSY",
        dbf_type: DbFieldType::Short,
        read_only: true,
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
        read_only: true,
    },
    FieldDesc {
        name: "NORD",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "FTVL",
        dbf_type: DbFieldType::Short,
        read_only: true,
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
    // Waveform-only TimeSeries acquisition control (devAsynXXXTimeSeries):
    // RARM `pp(TRUE)` client-settable, BUSY `special(SPC_NOMOD)` device-set.
    // aai/aao/subArray do not declare these (they use their own field sets).
    // waveformRecord.dbd.pod:411,461.
    FieldDesc {
        name: "RARM",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "BUSY",
        dbf_type: DbFieldType::Short,
        read_only: true,
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
        read_only: true,
    },
    FieldDesc {
        name: "NORD",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "FTVL",
        dbf_type: DbFieldType::Short,
        read_only: true,
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
    // Waveform-only TimeSeries acquisition control (devAsynXXXTimeSeries):
    // RARM `pp(TRUE)` client-settable, BUSY `special(SPC_NOMOD)` device-set.
    // aai/aao/subArray do not declare these (they use their own field sets).
    // waveformRecord.dbd.pod:411,461.
    FieldDesc {
        name: "RARM",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "BUSY",
        dbf_type: DbFieldType::Short,
        read_only: true,
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
        read_only: true,
    },
    FieldDesc {
        name: "NORD",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "FTVL",
        dbf_type: DbFieldType::Short,
        read_only: true,
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
    // Waveform-only TimeSeries acquisition control (devAsynXXXTimeSeries):
    // RARM `pp(TRUE)` client-settable, BUSY `special(SPC_NOMOD)` device-set.
    // aai/aao/subArray do not declare these (they use their own field sets).
    // waveformRecord.dbd.pod:411,461.
    FieldDesc {
        name: "RARM",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "BUSY",
        dbf_type: DbFieldType::Short,
        read_only: true,
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
        read_only: true,
    },
    FieldDesc {
        name: "NORD",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "FTVL",
        dbf_type: DbFieldType::Short,
        read_only: true,
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
    // Waveform-only TimeSeries acquisition control (devAsynXXXTimeSeries):
    // RARM `pp(TRUE)` client-settable, BUSY `special(SPC_NOMOD)` device-set.
    // aai/aao/subArray do not declare these (they use their own field sets).
    // waveformRecord.dbd.pod:411,461.
    FieldDesc {
        name: "RARM",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "BUSY",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
];

// subArray field set. subArray shares the `WaveformRecord` struct but its C dbd
// differs from waveform/aai/aao: NELM is `pp(TRUE)` (runtime-writable) instead of
// `special(SPC_NOMOD)`, and MALM (`special(SPC_NOMOD)`) / INDX (`pp(TRUE)`) exist.
// FTVL/NORD stay `special(SPC_NOMOD)` (load-settable, runtime-immutable) as in
// every kind. `field_list()` returns this set when `kind == SubArray`, so the
// FieldDesc `read_only` flag is kind-correct by construction — it stays the single
// source the field_io runtime gate reads, with no per-kind runtime override.
macro_rules! subarray_field_list {
    ($valty:expr) => {
        &[
            FieldDesc {
                name: "VAL",
                dbf_type: $valty,
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
                read_only: true,
            },
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
            FieldDesc {
                name: "MALM",
                dbf_type: DbFieldType::Long,
                read_only: true,
            },
            FieldDesc {
                name: "INDX",
                dbf_type: DbFieldType::Long,
                read_only: false,
            },
        ]
    };
}

static SUBARRAY_FIELDS_CHAR: &[FieldDesc] = subarray_field_list!(DbFieldType::Char);
static SUBARRAY_FIELDS_SHORT: &[FieldDesc] = subarray_field_list!(DbFieldType::Short);
static SUBARRAY_FIELDS_LONG: &[FieldDesc] = subarray_field_list!(DbFieldType::Long);
static SUBARRAY_FIELDS_INT64: &[FieldDesc] = subarray_field_list!(DbFieldType::Int64);
static SUBARRAY_FIELDS_UINT64: &[FieldDesc] = subarray_field_list!(DbFieldType::UInt64);
static SUBARRAY_FIELDS_FLOAT: &[FieldDesc] = subarray_field_list!(DbFieldType::Float);
static SUBARRAY_FIELDS_DOUBLE: &[FieldDesc] = subarray_field_list!(DbFieldType::Double);

/// `menu(menuOmsl)` index for `closed_loop` (`MENU_OMSL[1]`,
/// `menu_choices.rs:61`). When `aao.omsl == closed_loop` the record sources
/// VAL from DOL each cycle (C `aaoRecord.c::fetchValue`).
const MENU_OMSL_CLOSED_LOOP: i16 = 1;

/// C `dbLinkIsConstant(&prec->dol)`: is the aao DOL a constant rather than a
/// fetchable link? Used to gate the process-time fetch (`!isConst`), so a
/// constant is never re-applied per cycle over a client caput.
///
/// [`parse_link_v2`] classifies a *scalar* numeric / quoted-string / JSON
/// constant as [`ParsedLink::Constant`], but NOT a whitespace-separated
/// numeric array literal (`"1 2 3"`) — which the C db loader parses as a
/// constant array (loaded once at init via `dbLoadLinkArray`). Recognise that
/// array form here so a constant array DOL also yields no per-cycle fetch. An
/// empty DOL has no source and is treated as constant (nothing to fetch).
fn dol_is_constant(dol: &str) -> bool {
    let trimmed = dol.trim();
    if trimmed.is_empty() {
        return true;
    }
    if matches!(parse_link_v2(dol), ParsedLink::Constant(_)) {
        return true;
    }
    trimmed
        .split_whitespace()
        .all(|tok| tok.parse::<f64>().is_ok())
}

// aao field set. aao shares the `WaveformRecord` struct and the
// waveform/aai field shape (NELM `special(SPC_NOMOD)` read_only, FTVL/NORD
// load-settable-runtime-immutable), but its C dbd ALSO declares OMSL
// `menu(menuOmsl)` and DOL `DBF_INLINK` (`aaoRecord.dbd.pod:355-360`) — the
// desired-output mode + link absent from the other three array types.
// `field_list()` returns this set only when `kind == Aao`, so OMSL/DOL are
// loadable (apply_fields gates on field_list membership) for aao alone.
macro_rules! aao_field_list {
    ($valty:expr) => {
        &[
            FieldDesc {
                name: "VAL",
                dbf_type: $valty,
                read_only: false,
            },
            FieldDesc {
                name: "NELM",
                dbf_type: DbFieldType::Long,
                read_only: true,
            },
            FieldDesc {
                name: "NORD",
                dbf_type: DbFieldType::Long,
                read_only: true,
            },
            FieldDesc {
                name: "FTVL",
                dbf_type: DbFieldType::Short,
                read_only: true,
            },
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
            FieldDesc {
                name: "OMSL",
                dbf_type: DbFieldType::Short,
                read_only: false,
            },
            FieldDesc {
                name: "DOL",
                dbf_type: DbFieldType::String,
                read_only: false,
            },
        ]
    };
}

static AAO_FIELDS_CHAR: &[FieldDesc] = aao_field_list!(DbFieldType::Char);
static AAO_FIELDS_SHORT: &[FieldDesc] = aao_field_list!(DbFieldType::Short);
static AAO_FIELDS_LONG: &[FieldDesc] = aao_field_list!(DbFieldType::Long);
static AAO_FIELDS_INT64: &[FieldDesc] = aao_field_list!(DbFieldType::Int64);
static AAO_FIELDS_UINT64: &[FieldDesc] = aao_field_list!(DbFieldType::UInt64);
static AAO_FIELDS_FLOAT: &[FieldDesc] = aao_field_list!(DbFieldType::Float);
static AAO_FIELDS_DOUBLE: &[FieldDesc] = aao_field_list!(DbFieldType::Double);

// aai field set. aai shares the `WaveformRecord` struct and the
// waveform/aao field shape (NELM `special(SPC_NOMOD)` read_only, FTVL/NORD
// load-settable-runtime-immutable), but unlike waveform it does NOT declare
// RARM/BUSY (those are waveform-only TimeSeries control fields), and unlike
// aao it has no OMSL/DOL. So aai gets its own set — the bare common shape —
// rather than sharing the `WAVEFORM_FIELDS_*` set, keeping the FieldDesc
// `read_only`/membership kind-correct by construction (aaiRecord.dbd.pod).
macro_rules! aai_field_list {
    ($valty:expr) => {
        &[
            FieldDesc {
                name: "VAL",
                dbf_type: $valty,
                read_only: false,
            },
            FieldDesc {
                name: "NELM",
                dbf_type: DbFieldType::Long,
                read_only: true,
            },
            FieldDesc {
                name: "NORD",
                dbf_type: DbFieldType::Long,
                read_only: true,
            },
            FieldDesc {
                name: "FTVL",
                dbf_type: DbFieldType::Short,
                read_only: true,
            },
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
        ]
    };
}

static AAI_FIELDS_CHAR: &[FieldDesc] = aai_field_list!(DbFieldType::Char);
static AAI_FIELDS_SHORT: &[FieldDesc] = aai_field_list!(DbFieldType::Short);
static AAI_FIELDS_LONG: &[FieldDesc] = aai_field_list!(DbFieldType::Long);
static AAI_FIELDS_INT64: &[FieldDesc] = aai_field_list!(DbFieldType::Int64);
static AAI_FIELDS_UINT64: &[FieldDesc] = aai_field_list!(DbFieldType::UInt64);
static AAI_FIELDS_FLOAT: &[FieldDesc] = aai_field_list!(DbFieldType::Float);
static AAI_FIELDS_DOUBLE: &[FieldDesc] = aai_field_list!(DbFieldType::Double);

impl Record for WaveformRecord {
    fn record_type(&self) -> &'static str {
        self.kind.as_record_type()
    }

    /// Expose the concrete record so device support can drive the
    /// device-only control fields that have no generic put path — `BUSY`
    /// is `special(SPC_NOMOD)`/read-only and `RARM` is reset by the
    /// device's own process. The time-series device support
    /// (`devAsynXXXTimeSeries`) downcasts here to apply its RARM state
    /// machine and reflect BUSY, mirroring the MotorRecord precedent.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    /// subArray finalisation, mirroring C `subArrayRecord.c::init_record`
    /// pass 0 (lines 96-104): MALM is floored to 1 (never 0), then NELM is
    /// clamped down to MALM. This runs after the loader has applied every
    /// field put, so the .db order of NELM/MALM/INDX no longer affects the
    /// result. Process-time clamping in `set_val` (the readValue equivalent)
    /// re-applies the same bounds each cycle. Non-subArray kinds have no MALM
    /// and are untouched.
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 && matches!(self.kind, ArrayKind::SubArray) {
            if self.malm < 1 {
                self.malm = 1;
            }
            if self.nelm > self.malm {
                self.nelm = self.malm;
                self.reallocate_val();
            }
        }
        Ok(())
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
            // Waveform-only RARM (re-arm control) / BUSY (acquisition-active),
            // used by waveform device support (devAsynXXXTimeSeries). aai/aao/
            // subArray do not declare them, so they are not exposed there.
            "RARM" if matches!(self.kind, ArrayKind::Waveform) => {
                Some(EpicsValue::Short(self.rarm))
            }
            "BUSY" if matches!(self.kind, ArrayKind::Waveform) => {
                Some(EpicsValue::Short(self.busy as i16))
            }
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "HOPR" => Some(EpicsValue::Double(self.hopr)),
            "LOPR" => Some(EpicsValue::Double(self.lopr)),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            // Simulation block — waveform/aai/aao only (not subArray).
            "SIMM" if self.has_sim_block() => Some(EpicsValue::Short(self.simm)),
            "SIML" if self.has_sim_block() => Some(EpicsValue::String(self.siml.clone().into())),
            "SIOL" if self.has_sim_block() => Some(EpicsValue::String(self.siol.clone().into())),
            "SIMS" if self.has_sim_block() => Some(EpicsValue::Short(self.sims)),
            // aao-only output-mode / desired-output link (aaoRecord.dbd.pod).
            "OMSL" if matches!(self.kind, ArrayKind::Aao) => Some(EpicsValue::Short(self.omsl)),
            "DOL" if matches!(self.kind, ArrayKind::Aao) => {
                Some(EpicsValue::String(self.dol.clone().into()))
            }
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                // Coerce value to match FTVL (e.g. String → CharArray for
                // FTVL=CHAR, String → UCharArray for FTVL=UCHAR).
                let value = match (&value, self.ftvl) {
                    (EpicsValue::String(s), 1) => EpicsValue::CharArray(s.as_bytes().to_vec()),
                    (EpicsValue::String(s), 2) => EpicsValue::UCharArray(s.as_bytes().to_vec()),
                    _ => value,
                };
                // Update NORD based on actual data length, capped at NELM
                // (a VAL buffer holds at most NELM elements; C dbPutField and
                // dbGetLink both bound the request to NELM, so NORD <= NELM by
                // construction). The buffer itself is resized to NELM to
                // preserve the CA channel element count.
                let nelm = self.nelm.max(0) as usize;
                match value {
                    EpicsValue::CharArray(mut arr) => {
                        self.nord = arr.len().min(nelm) as i32;
                        arr.resize(nelm, 0);
                        self.val = EpicsValue::CharArray(arr);
                    }
                    EpicsValue::UCharArray(mut arr) => {
                        self.nord = arr.len().min(nelm) as i32;
                        arr.resize(nelm, 0);
                        self.val = EpicsValue::UCharArray(arr);
                    }
                    EpicsValue::ShortArray(mut arr) => {
                        self.nord = arr.len().min(nelm) as i32;
                        arr.resize(nelm, 0);
                        self.val = EpicsValue::ShortArray(arr);
                    }
                    EpicsValue::LongArray(mut arr) => {
                        self.nord = arr.len().min(nelm) as i32;
                        arr.resize(nelm, 0);
                        self.val = EpicsValue::LongArray(arr);
                    }
                    EpicsValue::Int64Array(mut arr) => {
                        self.nord = arr.len().min(nelm) as i32;
                        arr.resize(nelm, 0);
                        self.val = EpicsValue::Int64Array(arr);
                    }
                    EpicsValue::UInt64Array(mut arr) => {
                        self.nord = arr.len().min(nelm) as i32;
                        arr.resize(nelm, 0);
                        self.val = EpicsValue::UInt64Array(arr);
                    }
                    EpicsValue::FloatArray(mut arr) => {
                        self.nord = arr.len().min(nelm) as i32;
                        arr.resize(nelm, 0.0);
                        self.val = EpicsValue::FloatArray(arr);
                    }
                    EpicsValue::DoubleArray(mut arr) => {
                        self.nord = arr.len().min(nelm) as i32;
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
                    if matches!(self.kind, ArrayKind::SubArray) {
                        // subArray: NELM is the slice length; the buffer is
                        // re-derived from the source on `set_val`, so a fresh
                        // zeroed allocation here is correct. NO MALM clamp here
                        // — C clamps NELM->MALM at init_record and at process
                        // (subArrayRecord.c:103-104, 310-311), never at field
                        // put, so the .db load order of NELM vs MALM cannot
                        // matter. `init_record` and `set_val` apply the clamp.
                        self.nelm = n;
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
                // Store INDX as given (floored at 0). NO MALM clamp here — C
                // clamps INDX->MALM-1 at process (subArrayRecord.c:313-314),
                // never at field put, so the .db load order of INDX vs MALM
                // cannot matter. `set_val` (the readValue equivalent) applies
                // the clamp.
                self.indx = v.max(0);
                Ok(())
            }
            "MALM" if matches!(self.kind, ArrayKind::SubArray) => {
                let v = match value {
                    EpicsValue::Long(v) => v,
                    EpicsValue::Short(v) => v as i32,
                    _ => return Err(CaError::TypeMismatch("MALM".into())),
                };
                // C floors MALM to 1 (subArrayRecord.c:96-97); it is never 0.
                // No NELM/INDX re-clamp here — both are clamped against MALM at
                // `init_record` (post-load) and at process (310-314), so a MALM
                // put in any .db load order is reconciled there, not here.
                self.malm = v.max(1);
                Ok(())
            }
            // Waveform-only RARM (re-arm control, pp(TRUE) — client-settable) and
            // BUSY (acquisition-active, special(SPC_NOMOD) — device-set, not
            // client-writable). The device support reads RARM and resets it to 0.
            "RARM" if matches!(self.kind, ArrayKind::Waveform) => {
                let v = match value {
                    EpicsValue::Short(v) => v,
                    EpicsValue::Long(v) => v as i16,
                    _ => return Err(CaError::TypeMismatch("RARM".into())),
                };
                self.rarm = v;
                Ok(())
            }
            "BUSY" if matches!(self.kind, ArrayKind::Waveform) => {
                Err(CaError::ReadOnlyField(name.to_string()))
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
            // aao-only OMSL (menu, resolved to a Short index by the central
            // shared_menu_choices("OMSL") path) and DOL link string. The
            // field_list AAO set carries these so apply_fields routes
            // field(OMSL/DOL,...) here rather than to common fields.
            "OMSL" if matches!(self.kind, ArrayKind::Aao) => match value {
                EpicsValue::Short(v) => {
                    self.omsl = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OMSL".into())),
            },
            "DOL" if matches!(self.kind, ArrayKind::Aao) => match value {
                EpicsValue::String(s) => {
                    self.dol = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("DOL".into())),
            },
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    // `field_list` is keyed first by `ArrayKind`, then by FTVL element type.
    // Each array kind selects its own field set, keyed then by FTVL element type:
    // subArray -> `SUBARRAY_FIELDS_*` (NELM `pp(TRUE)`, plus MALM/INDX), aao ->
    // `AAO_FIELDS_*` (common shape + OMSL/DOL), aai -> `AAI_FIELDS_*` (bare common
    // shape), waveform -> `WAVEFORM_FIELDS_*` (common shape + RARM/BUSY). The sets
    // differ exactly where the C dbd does. Selecting the set by kind makes each
    // FieldDesc `read_only`/membership kind-correct by construction, so it stays
    // the single source the field_io runtime gate (`put_record_field_from_ca_inner`)
    // reads — no per-kind runtime override.
    fn field_list(&self) -> &'static [FieldDesc] {
        if matches!(self.kind, ArrayKind::SubArray) {
            return match self.ftvl {
                1 | 2 => SUBARRAY_FIELDS_CHAR,
                3 | 4 => SUBARRAY_FIELDS_SHORT,
                5 | 6 => SUBARRAY_FIELDS_LONG,
                7 => SUBARRAY_FIELDS_INT64,
                8 => SUBARRAY_FIELDS_UINT64,
                9 => SUBARRAY_FIELDS_FLOAT,
                _ => SUBARRAY_FIELDS_DOUBLE,
            };
        }
        // aao adds OMSL/DOL to the common shape (aaoRecord.dbd.pod).
        if matches!(self.kind, ArrayKind::Aao) {
            return match self.ftvl {
                1 | 2 => AAO_FIELDS_CHAR,
                3 | 4 => AAO_FIELDS_SHORT,
                5 | 6 => AAO_FIELDS_LONG,
                7 => AAO_FIELDS_INT64,
                8 => AAO_FIELDS_UINT64,
                9 => AAO_FIELDS_FLOAT,
                _ => AAO_FIELDS_DOUBLE,
            };
        }
        // aai is the bare common shape — no RARM/BUSY (waveform-only) and no
        // OMSL/DOL (aao-only) (aaiRecord.dbd.pod).
        if matches!(self.kind, ArrayKind::Aai) {
            return match self.ftvl {
                1 | 2 => AAI_FIELDS_CHAR,
                3 | 4 => AAI_FIELDS_SHORT,
                5 | 6 => AAI_FIELDS_LONG,
                7 => AAI_FIELDS_INT64,
                8 => AAI_FIELDS_UINT64,
                9 => AAI_FIELDS_FLOAT,
                _ => AAI_FIELDS_DOUBLE,
            };
        }
        // waveform: common shape + RARM/BUSY (waveformRecord.dbd.pod).
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

    /// aao `OMSL=closed_loop` desired-output pull. C `aaoRecord.c::fetchValue`
    /// (357-377): an aao whose `omsl == closed_loop` sources its array from
    /// `DOL` before writing. At PROCESS time C fetches only when DOL is a
    /// *non-constant* link (`!init && !isConst` → `dbGetLink`); a constant DOL
    /// is loaded once at init via `dbLoadLinkArray` and is a per-cycle no-op.
    /// This pre-input hook mirrors the process-time `!isConst` arm: it emits a
    /// `ReadDbLink { DOL -> VAL }` only for a real link — the framework reads
    /// the link's native array and applies it via `put_field("VAL", ...)`,
    /// which sets `NORD = element count` exactly as C's `nord = nReq`. A
    /// constant or empty DOL emits nothing, so a constant is never re-applied
    /// over a client caput to VAL (C's `!dbLinkIsConstant` gate). Supervisory
    /// mode and the other three array kinds (no OMSL/DOL) return no actions.
    ///
    /// Residual: the init-time constant-array load (`dbLoadLinkArray`, the
    /// `init && isConst` arm) is not ported — this record has no array-literal
    /// constant-link parser, so a `field(DOL,"1 2 3")` constant array is not
    /// seeded into VAL at init. A non-constant closed_loop DOL (the common
    /// tracking case) is fully covered.
    fn pre_input_link_actions(&mut self) -> Vec<ProcessAction> {
        if !matches!(self.kind, ArrayKind::Aao) || self.omsl != MENU_OMSL_CLOSED_LOOP {
            return Vec::new();
        }
        // C `!dbLinkIsConstant(&prec->dol)`: only a real (DB/CA/PVA) link is
        // fetched at process time; a constant (scalar, array literal, or
        // empty) is not re-applied each cycle.
        if dol_is_constant(&self.dol) {
            return Vec::new();
        }
        vec![ProcessAction::ReadDbLink {
            link_field: "DOL",
            target_field: "VAL",
        }]
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
        // C subArrayRecord.c process (readValue) clamps NELM and INDX against
        // MALM every cycle (310-311, 313-314); MALM is always >= 1
        // (init_record floors it, 96-97). This is the readValue equivalent, so
        // apply the same clamps here — the slice is then correct regardless of
        // the .db load order of NELM/MALM/INDX.
        let malm = self.malm.max(1);
        if self.nelm > malm {
            self.nelm = malm;
        }
        if self.indx >= malm {
            self.indx = malm - 1;
        }
        let start = self.indx.max(0) as usize;
        let take = self.nelm.max(0) as usize;
        let malm_cap = malm as usize; // MALM is always a real source-view cap
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
            EpicsValue::UCharArray(arr) => slice!(value, arr, UCharArray, 0u8),
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

    /// aao alone declares OMSL/DOL (`aaoRecord.dbd.pod`). `apply_fields` gates
    /// `field(OMSL/DOL,...)` on `field_list` membership, so the aao set must
    /// carry both names and the waveform/aai set must not — otherwise the
    /// loader would misroute them to common fields and the fetch would never
    /// arm.
    #[test]
    fn aao_field_list_includes_omsl_dol_other_kinds_do_not() {
        let aao = WaveformRecord::with_kind(ArrayKind::Aao);
        let names: Vec<&str> = aao.field_list().iter().map(|f| f.name).collect();
        assert!(names.contains(&"OMSL"), "aao field_list must carry OMSL");
        assert!(names.contains(&"DOL"), "aao field_list must carry DOL");

        for kind in [ArrayKind::Waveform, ArrayKind::Aai, ArrayKind::SubArray] {
            let r = WaveformRecord::with_kind(kind);
            let names: Vec<&str> = r.field_list().iter().map(|f| f.name).collect();
            assert!(
                !names.contains(&"OMSL") && !names.contains(&"DOL"),
                "{kind:?} must not declare OMSL/DOL"
            );
        }
    }

    /// RARM (re-arm, `pp(TRUE)` settable) and BUSY (acquisition-active,
    /// `special(SPC_NOMOD)` read-only) are waveform-only TimeSeries control
    /// fields. They appear in the waveform field set with the correct read_only
    /// flags and nowhere else; aai/aao/subArray expose neither.
    #[test]
    fn waveform_rarm_busy_fields_waveform_only() {
        let wf = WaveformRecord::with_kind(ArrayKind::Waveform);
        let rarm = wf
            .field_list()
            .iter()
            .find(|f| f.name == "RARM")
            .expect("waveform field_list must carry RARM");
        let busy = wf
            .field_list()
            .iter()
            .find(|f| f.name == "BUSY")
            .expect("waveform field_list must carry BUSY");
        assert!(!rarm.read_only, "RARM is pp(TRUE) — client-settable");
        assert!(busy.read_only, "BUSY is special(SPC_NOMOD) — read-only");

        // RARM round-trips through put/get; BUSY is read-only (device-set) and
        // reflects the struct flag.
        let mut wf = WaveformRecord::with_kind(ArrayKind::Waveform);
        wf.put_field("RARM", EpicsValue::Short(1)).unwrap();
        assert_eq!(wf.get_field("RARM"), Some(EpicsValue::Short(1)));
        assert_eq!(wf.get_field("BUSY"), Some(EpicsValue::Short(0)));
        assert!(
            wf.put_field("BUSY", EpicsValue::Short(1)).is_err(),
            "BUSY must reject CA puts (SPC_NOMOD)"
        );
        wf.busy = true;
        assert_eq!(wf.get_field("BUSY"), Some(EpicsValue::Short(1)));

        // aai/aao/subArray declare neither field: not in field_list, get None,
        // put errors.
        for kind in [ArrayKind::Aai, ArrayKind::Aao, ArrayKind::SubArray] {
            let mut r = WaveformRecord::with_kind(kind);
            let names: Vec<&str> = r.field_list().iter().map(|f| f.name).collect();
            assert!(
                !names.contains(&"RARM") && !names.contains(&"BUSY"),
                "{kind:?} must not declare RARM/BUSY"
            );
            assert_eq!(r.get_field("RARM"), None, "{kind:?} RARM get must be None");
            assert_eq!(r.get_field("BUSY"), None, "{kind:?} BUSY get must be None");
            assert!(r.put_field("RARM", EpicsValue::Short(1)).is_err());
        }
    }

    /// `as_any_mut` exposes the concrete record so device support can drive the
    /// fields with no generic put path — read-only BUSY and the device-reset
    /// RARM (the TimeSeries device support uses exactly this downcast).
    #[test]
    fn waveform_as_any_mut_downcasts_to_concrete_record() {
        let mut r: Box<dyn Record> = Box::new(WaveformRecord::with_kind(ArrayKind::Waveform));
        let wf = r
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<WaveformRecord>())
            .expect("waveform must expose itself via as_any_mut");
        // Device-only writes that the generic put path cannot do: BUSY is
        // read-only, RARM is reset by the device after applying it.
        wf.busy = true;
        wf.rarm = 0;
        assert_eq!(r.get_field("BUSY"), Some(EpicsValue::Short(1)));
        assert_eq!(r.get_field("RARM"), Some(EpicsValue::Short(0)));
    }

    /// OMSL (resolved to a Short index by the central menu path) and DOL
    /// round-trip through aao's get/put; non-aao kinds expose neither.
    #[test]
    fn aao_omsl_dol_round_trip_and_kind_gated() {
        let mut aao = WaveformRecord::with_kind(ArrayKind::Aao);
        aao.put_field("OMSL", EpicsValue::Short(MENU_OMSL_CLOSED_LOOP))
            .unwrap();
        aao.put_field("DOL", EpicsValue::String("src.VAL".into()))
            .unwrap();
        assert_eq!(
            aao.get_field("OMSL"),
            Some(EpicsValue::Short(MENU_OMSL_CLOSED_LOOP))
        );
        assert_eq!(
            aao.get_field("DOL"),
            Some(EpicsValue::String("src.VAL".into()))
        );

        // waveform has no OMSL/DOL: get returns None, put is FieldNotFound.
        let mut wf = WaveformRecord::with_kind(ArrayKind::Waveform);
        assert_eq!(wf.get_field("OMSL"), None);
        assert_eq!(wf.get_field("DOL"), None);
        assert!(wf.put_field("OMSL", EpicsValue::Short(1)).is_err());
        assert!(
            wf.put_field("DOL", EpicsValue::String("src.VAL".into()))
                .is_err()
        );
    }

    /// C `aaoRecord.c::fetchValue` process arm (`!init && !isConst`): an aao
    /// with `omsl == closed_loop` and a real (non-constant) DOL link emits a
    /// `ReadDbLink { DOL -> VAL }` pre-input action so the framework pulls the
    /// source array into VAL (which sets NORD) before the write.
    #[test]
    fn aao_closed_loop_real_link_emits_read_db_link() {
        let mut aao = WaveformRecord::with_kind(ArrayKind::Aao);
        aao.omsl = MENU_OMSL_CLOSED_LOOP;
        aao.dol = "srcWaveform.VAL".to_string();
        assert_eq!(
            aao.pre_input_link_actions(),
            vec![ProcessAction::ReadDbLink {
                link_field: "DOL",
                target_field: "VAL",
            }]
        );

        // A bare record name parses to a DB link too (parse_link_v2), so it
        // also arms the fetch.
        aao.dol = "srcWaveform".to_string();
        assert_eq!(aao.pre_input_link_actions().len(), 1);
    }

    /// C gates the process-time fetch on `!dbLinkIsConstant`: a constant DOL
    /// (numeric/array literal) is loaded once at init, never re-applied per
    /// cycle. An empty DOL has no source at all. Both emit no action, so a
    /// client caput to VAL is not clobbered.
    #[test]
    fn aao_closed_loop_constant_or_empty_dol_emits_nothing() {
        let mut aao = WaveformRecord::with_kind(ArrayKind::Aao);
        aao.omsl = MENU_OMSL_CLOSED_LOOP;

        aao.dol = "1 2 3".to_string(); // constant array literal
        assert!(aao.pre_input_link_actions().is_empty());

        aao.dol = "42".to_string(); // constant scalar literal
        assert!(aao.pre_input_link_actions().is_empty());

        aao.dol = String::new(); // unset
        assert!(aao.pre_input_link_actions().is_empty());

        aao.dol = "   ".to_string(); // whitespace-only
        assert!(aao.pre_input_link_actions().is_empty());
    }

    /// Supervisory mode (the default) never fetches DOL, even with a real
    /// link configured (C `if(prec->omsl != menuOmslclosed_loop) return 0`).
    /// And the other three array kinds have no OMSL/DOL, so they never fetch
    /// regardless of the struct's `omsl` value.
    #[test]
    fn supervisory_and_non_aao_kinds_emit_nothing() {
        let mut aao = WaveformRecord::with_kind(ArrayKind::Aao);
        aao.omsl = 0; // supervisory
        aao.dol = "srcWaveform.VAL".to_string();
        assert!(aao.pre_input_link_actions().is_empty());

        for kind in [ArrayKind::Waveform, ArrayKind::Aai, ArrayKind::SubArray] {
            let mut r = WaveformRecord::with_kind(kind);
            // Force the would-be fetch state; the kind gate must still win.
            r.omsl = MENU_OMSL_CLOSED_LOOP;
            r.dol = "srcWaveform.VAL".to_string();
            assert!(
                r.pre_input_link_actions().is_empty(),
                "{kind:?} has no OMSL/DOL and must not fetch"
            );
        }
    }

    /// The framework applies the fetched array through `put_field("VAL", ...)`,
    /// which sets `NORD = element count` — the contract C `fetchValue` relies
    /// on (`prec->nord = nReq`). Pin it for the aao DOL-pull path.
    #[test]
    fn aao_val_put_sets_nord_for_dol_pull() {
        let mut aao = WaveformRecord::with_kind(ArrayKind::Aao);
        aao.nelm = 8;
        aao.put_field("VAL", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
            .unwrap();
        assert_eq!(aao.nord, 3, "NORD must equal the pulled element count");
    }

    /// NORD is capped at NELM by construction at every VAL array write
    /// (C dbPutField / dbGetLink bound the request to NELM). Boundary
    /// cases: source < NELM (NORD = source), == NELM, and > NELM
    /// (NORD = NELM, the previously over-reported case). Exercised here
    /// on a plain waveform — the cap lives in the shared put_field VAL
    /// arm, so it covers aao DOL pulls and every other internal delivery.
    #[test]
    fn put_val_caps_nord_at_nelm() {
        let mut wf = WaveformRecord::with_kind(ArrayKind::Waveform);
        wf.nelm = 4;

        // source < NELM
        wf.put_field("VAL", EpicsValue::LongArray(vec![1, 2]))
            .unwrap();
        assert_eq!(wf.nord, 2, "source < NELM: NORD == source length");

        // source == NELM
        wf.put_field("VAL", EpicsValue::LongArray(vec![1, 2, 3, 4]))
            .unwrap();
        assert_eq!(wf.nord, 4, "source == NELM: NORD == NELM");

        // source > NELM: NORD must clamp to NELM, not the source length
        wf.put_field("VAL", EpicsValue::LongArray(vec![1, 2, 3, 4, 5, 6, 7]))
            .unwrap();
        assert_eq!(wf.nord, 4, "source > NELM: NORD must clamp to NELM");
        // and the served VAL holds exactly NORD (== NELM) valid elements
        let val = wf.get_field("VAL").unwrap();
        if let EpicsValue::LongArray(v) = val {
            assert_eq!(v, vec![1, 2, 3, 4], "VAL serves exactly NELM elements");
        } else {
            panic!("VAL should be LongArray, got {val:?}");
        }
    }

    /// PR #a02c310 follow-up: subArray slices source[INDX..INDX+NELM]
    /// into VAL with NORD set to the actual copied length. Source
    /// shorter than INDX → NORD=0. INDX+NELM > source.len → only
    /// available tail is copied, rest zero-padded to NELM.
    #[test]
    fn subarray_slices_input_at_indx_with_nelm_take() {
        let mut r = WaveformRecord::with_kind(ArrayKind::SubArray);
        // 4-element double buffer; consume up to 4 from offset 2.
        // MALM is the source-view cap (C floors it to >= 1), so it must be at
        // least the source length to expose the whole source — a real subArray
        // .db sets MALM to the upstream waveform's NELM.
        r.put_field("MALM", EpicsValue::Long(6)).unwrap();
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
        // MALM=20 leaves INDX=10 un-clamped (10 < MALM), so the slice starts
        // past the 3-element source and yields NORD=0. With MALM at/under the
        // source length C would instead clamp INDX to MALM-1 (313-314) and read
        // a tail; this case isolates the genuine "INDX beyond source" path.
        r.put_field("MALM", EpicsValue::Long(20)).unwrap();
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
        // MALM=5 == source length: the whole source is visible, the slice from
        // offset 3 has only 2 valid elements and zero-pads the rest to NELM.
        r.put_field("MALM", EpicsValue::Long(5)).unwrap();
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
    fn subarray_malm_defaults_to_one_and_floors_zero() {
        // C `subArrayRecord.dbd` `initial("1")` + `init_record` floor
        // (subArrayRecord.c:96-97): MALM is never 0.
        let r = WaveformRecord::with_kind(ArrayKind::SubArray);
        assert_eq!(r.get_field("MALM"), Some(EpicsValue::Long(1)));
        let mut z = WaveformRecord::with_kind(ArrayKind::SubArray);
        z.put_field("MALM", EpicsValue::Long(0)).unwrap();
        assert_eq!(
            z.get_field("MALM"),
            Some(EpicsValue::Long(1)),
            "MALM put of 0 floors back to 1"
        );
    }

    #[test]
    fn subarray_init_record_clamps_nelm_to_malm_independent_of_load_order() {
        // The defect this closes: per-put clamping made the clamped NELM depend
        // on whether the .db set NELM before or after MALM. C clamps NELM->MALM
        // in init_record (96-104) post-load, so the order is irrelevant. Both
        // orders must converge to NELM == MALM after init_record.
        let mut nelm_first = WaveformRecord::with_kind(ArrayKind::SubArray);
        nelm_first.put_field("NELM", EpicsValue::Long(50)).unwrap();
        nelm_first.put_field("MALM", EpicsValue::Long(8)).unwrap();
        nelm_first.init_record(0).unwrap();
        assert_eq!(nelm_first.nelm, 8, "NELM clamped down to MALM at init");

        let mut malm_first = WaveformRecord::with_kind(ArrayKind::SubArray);
        malm_first.put_field("MALM", EpicsValue::Long(8)).unwrap();
        malm_first.put_field("NELM", EpicsValue::Long(50)).unwrap();
        malm_first.init_record(0).unwrap();
        assert_eq!(
            malm_first.nelm, 8,
            "same clamp regardless of NELM/MALM .db load order"
        );
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
            // OLDSIMM is special(SPC_NOMOD) and lives in the common fields
            // (the simulation owner's latch) — readable, not client-writable.
            let mut inst = RecordInstance::new("WF:OLDSIMM".into(), rec);
            assert!(matches!(
                inst.put_common_field("OLDSIMM", EpicsValue::Short(1)),
                Err(crate::error::CaError::ReadOnlyField(_))
            ));
            assert_eq!(inst.get_common_field("OLDSIMM"), Some(EpicsValue::Short(0)));
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
