use crate::error::{CaError, CaResult};

// DBR type ranges (matches db_access.h):
//   native (0..=6), STS (7..=13), TIME (14..=20), GR (21..=27),
//   CTRL (28..=34), special PUT_ACKT (35), PUT_ACKS (36),
//   STSACK_STRING (37), CLASS_NAME (38).

// Native scalar types (also exposed via DbFieldType enum)
pub const DBR_STRING: u16 = 0;
pub const DBR_SHORT: u16 = 1;
pub const DBR_FLOAT: u16 = 2;
pub const DBR_ENUM: u16 = 3;
pub const DBR_CHAR: u16 = 4;
pub const DBR_LONG: u16 = 5;
pub const DBR_DOUBLE: u16 = 6;
// `DBR_INT` is the libca alias for `DBR_SHORT`.
pub const DBR_INT: u16 = DBR_SHORT;

// Status-only metadata layer (CA-261)
pub const DBR_STS_STRING: u16 = 7;
pub const DBR_STS_SHORT: u16 = 8;
pub const DBR_STS_FLOAT: u16 = 9;
pub const DBR_STS_ENUM: u16 = 10;
pub const DBR_STS_CHAR: u16 = 11;
pub const DBR_STS_LONG: u16 = 12;
pub const DBR_STS_DOUBLE: u16 = 13;
pub const DBR_STS_INT: u16 = DBR_STS_SHORT;

// Status + timestamp layer (CA-262)
pub const DBR_TIME_STRING: u16 = 14;
pub const DBR_TIME_SHORT: u16 = 15;
pub const DBR_TIME_FLOAT: u16 = 16;
pub const DBR_TIME_ENUM: u16 = 17;
pub const DBR_TIME_CHAR: u16 = 18;
pub const DBR_TIME_LONG: u16 = 19;
pub const DBR_TIME_DOUBLE: u16 = 20;
pub const DBR_TIME_INT: u16 = DBR_TIME_SHORT;

// Status + graphic (display limits / units / precision) layer (CA-263)
pub const DBR_GR_STRING: u16 = 21;
pub const DBR_GR_SHORT: u16 = 22;
pub const DBR_GR_FLOAT: u16 = 23;
pub const DBR_GR_ENUM: u16 = 24;
pub const DBR_GR_CHAR: u16 = 25;
pub const DBR_GR_LONG: u16 = 26;
pub const DBR_GR_DOUBLE: u16 = 27;
pub const DBR_GR_INT: u16 = DBR_GR_SHORT;

// Status + graphic + control limits layer (CA-264)
pub const DBR_CTRL_STRING: u16 = 28;
pub const DBR_CTRL_SHORT: u16 = 29;
pub const DBR_CTRL_FLOAT: u16 = 30;
pub const DBR_CTRL_ENUM: u16 = 31;
pub const DBR_CTRL_CHAR: u16 = 32;
pub const DBR_CTRL_LONG: u16 = 33;
pub const DBR_CTRL_DOUBLE: u16 = 34;
pub const DBR_CTRL_INT: u16 = DBR_CTRL_SHORT;

// Special alarm-acknowledgement / introspection types
pub const DBR_PUT_ACKT: u16 = 35;
pub const DBR_PUT_ACKS: u16 = 36;
pub const DBR_STSACK_STRING: u16 = 37;
/// Returns the IOC's record-type class name as a 40-byte string
/// (CA-268, db_access.h: `DBR_CLASS_NAME`).
pub const DBR_CLASS_NAME: u16 = 38;

/// Last allocated DBR type code, matching the C `LAST_BUFFER_TYPE` macro.
pub const LAST_BUFFER_TYPE: u16 = DBR_CLASS_NAME;

/// EPICS DBR field types (native types only)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum DbFieldType {
    String = 0,
    Short = 1, // aka Int16
    Float = 2,
    Enum = 3,
    Char = 4, // aka UInt8
    Long = 5, // aka Int32
    Double = 6,
    /// Internal-only type for int64in/int64out records.
    /// No CA wire type 7 exists; over CA these PVs appear as Double (type 6).
    Int64 = 7,
    /// Internal-only type for unsigned 64-bit EPICS fields (C `DBF_UINT64`,
    /// dbStatic `dbfType` index 8). The CA wire protocol has no 64-bit
    /// type, so over CA these PVs appear as Double (type 6); over PVA they
    /// are served natively as `ulong`. Mirrors `Int64`'s CA handling.
    UInt64 = 8,
    /// Internal-only type for unsigned 16-bit EPICS fields (C `DBF_USHORT`,
    /// dbStatic `dbfType`). The CA wire protocol has no unsigned types, so
    /// the IOC promotes `DBF_USHORT` to the next signed type that holds its
    /// full `0..=65535` range — `DBR_LONG` (the C `dbDBRnewToDBRold` table,
    /// `db_convert.h`: `5, /*DBR_USHORT to DBR_LONG*/`). Over PVA pvxs
    /// serves it natively as `ushort` (`ioc/typeutils.cpp:38-40`:
    /// `DBR_USHORT -> TypeCode::UInt16`). The discriminant is an internal
    /// marker (not a CA wire code); see [`Self::ca_wire_type`].
    UShort = 9,
    /// Internal-only type for unsigned 32-bit EPICS fields (C `DBF_ULONG`,
    /// dbStatic `dbfType`). `0..=4294967295` does not fit in `i32`, so the
    /// IOC promotes `DBF_ULONG` to `DBR_DOUBLE` over CA exactly like
    /// `UInt64`/`Int64` (`db_convert.h`: `6, /*DBR_ULONG to DBR_DOUBLE*/`).
    /// Over PVA pvxs serves it natively as `uint` (`ioc/typeutils.cpp:43-44`:
    /// `DBR_ULONG -> TypeCode::UInt32`).
    ULong = 10,
    /// Internal-only type for unsigned 8-bit EPICS fields (C `DBF_UCHAR`,
    /// dbStatic `dbfType` index 2 / `waveform` `FTVL=UCHAR`). Unlike the
    /// signed `Char` (epicsInt8), this is epicsUInt8. The CA wire protocol
    /// has no unsigned types, so the IOC promotes `DBF_UCHAR` to `DBR_CHAR`
    /// — the same 1-byte wire type as `Char` (the C `dbDBRnewToDBRold` table,
    /// `db_convert.h`: `4, /*DBR_UCHAR to DBR_CHAR*/`); the raw bytes are
    /// identical, only the signedness of the interpretation differs. Over PVA
    /// pvxs serves it natively as `ubyte` (`ioc/typeutils.cpp:34-35`:
    /// `DBR_UCHAR -> TypeCode::UInt8`), distinct from `Char`'s signed `byte`.
    /// The discriminant is an internal marker (not a CA wire code); see
    /// [`Self::ca_wire_type`].
    UChar = 11,
}

/// C's dbStatic `dbfType` code — the `DBF_*` token a `.dbd` declares and
/// the number `dbStaticLib` stores, exports and prints (`dbFldTypes.h:24-43`
/// @R7.0.10).
///
/// A SECOND fact about a field, not a renumbering of [`DbFieldType`]:
/// that enum's discriminants are the CA **wire** codes and are load-bearing
/// on the protocol, whereas these are dbStatic's own indices, and the two
/// orders disagree from the second entry on (`DBF_CHAR` is 1 here and
/// `DBR_SHORT` is 1 there). Anything that prints C's `%d` type number —
/// `dba`'s `Field Type` row, dbStatic's `dbDumpField` — needs this one.
///
/// It also has six codes [`DbFieldType`] cannot express at all: a served
/// type is one of twelve scalars, while a *declaration* can additionally be
/// `MENU`, `DEVICE`, the three link kinds, or `NOACCESS`. That is why
/// [`super::super::server::record::FieldDesc::declared_dbf`] carries this
/// type rather than deriving it — the collapse is one-way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(i16)]
pub enum DbfCode {
    /// `DBF_STRING` (0).
    String = 0,
    /// `DBF_CHAR` (1) — `epicsInt8`.
    Char = 1,
    /// `DBF_UCHAR` (2) — `epicsUInt8`.
    UChar = 2,
    /// `DBF_SHORT` (3).
    Short = 3,
    /// `DBF_USHORT` (4).
    UShort = 4,
    /// `DBF_LONG` (5).
    Long = 5,
    /// `DBF_ULONG` (6).
    ULong = 6,
    /// `DBF_INT64` (7).
    Int64 = 7,
    /// `DBF_UINT64` (8).
    UInt64 = 8,
    /// `DBF_FLOAT` (9).
    Float = 9,
    /// `DBF_DOUBLE` (10).
    Double = 10,
    /// `DBF_ENUM` (11).
    Enum = 11,
    /// `DBF_MENU` (12) — served as an enum, declared as a menu.
    Menu = 12,
    /// `DBF_DEVICE` (13) — the `DTYP` field, served as an enum.
    Device = 13,
    /// `DBF_INLINK` (14).
    Inlink = 14,
    /// `DBF_OUTLINK` (15).
    Outlink = 15,
    /// `DBF_FWDLINK` (16).
    Fwdlink = 16,
    /// `DBF_NOACCESS` (17) — a C-internal with no dbStatic representation.
    NoAccess = 17,
}

impl DbfCode {
    /// The token without its `DBF_` prefix — C's `dbf[]` (`dbTest.c:76-81`),
    /// which is `pamapdbfType`'s `strvalue` (`dbFldTypes.h:51-70`) with that
    /// prefix stripped. `dba` prints `DBF_%s` around it and dbStatic prints
    /// the full name, so one table serves both.
    pub const fn name(self) -> &'static str {
        match self {
            DbfCode::String => "STRING",
            DbfCode::Char => "CHAR",
            DbfCode::UChar => "UCHAR",
            DbfCode::Short => "SHORT",
            DbfCode::UShort => "USHORT",
            DbfCode::Long => "LONG",
            DbfCode::ULong => "ULONG",
            DbfCode::Int64 => "INT64",
            DbfCode::UInt64 => "UINT64",
            DbfCode::Float => "FLOAT",
            DbfCode::Double => "DOUBLE",
            DbfCode::Enum => "ENUM",
            DbfCode::Menu => "MENU",
            DbfCode::Device => "DEVICE",
            DbfCode::Inlink => "INLINK",
            DbfCode::Outlink => "OUTLINK",
            DbfCode::Fwdlink => "FWDLINK",
            DbfCode::NoAccess => "NOACCESS",
        }
    }

    /// C's `mapDBFToDBR` (`dbAccess.c:76-95`) — the DBR code `dbEntryToAddr`
    /// stores in `paddr->dbr_field_type` (`:639`), which is what a client
    /// asking for this field's *native* type is answered with.
    ///
    /// `DBR_*` here is `dbFldTypes.h:75-90`'s numbering, where every
    /// `DBR_x` is `#define`d to its `DBF_x`, so the map is a projection of
    /// this enum onto itself and needs no second type: it collapses `MENU`
    /// and `DEVICE` onto `ENUM` and the three link types onto `STRING`, and
    /// is the identity on the other thirteen.
    ///
    /// NOT the CA wire type. That numbering is the older, narrower
    /// `db_access.h` one carried by [`DbFieldType`]'s own discriminants and
    /// reached through [`DbFieldType::ca_wire_type`]; the two disagree on
    /// every code above `DBR_SHORT`.
    pub const fn dbr_code(self) -> DbfCode {
        match self {
            DbfCode::Menu | DbfCode::Device => DbfCode::Enum,
            DbfCode::Inlink | DbfCode::Outlink | DbfCode::Fwdlink => DbfCode::String,
            other => other,
        }
    }
}

impl DbFieldType {
    /// The dbStatic code for the type this field is **served** as.
    ///
    /// The mapping only ever answers one of the twelve scalar codes,
    /// because that is all a served type can be. It is therefore NOT a way
    /// to recover a declaration: `ai.INP` and `dbCommon.FLNK` are both
    /// served as [`DbFieldType::String`] and this returns
    /// [`DbfCode::String`] for both, while their declarations are
    /// `DBF_INLINK` and `DBF_FWDLINK`. Read
    /// [`crate::server::record::FieldDesc::declared_dbf`] when the
    /// question is what the `.dbd` said.
    pub const fn dbf_code(self) -> DbfCode {
        match self {
            DbFieldType::String => DbfCode::String,
            DbFieldType::Char => DbfCode::Char,
            DbFieldType::UChar => DbfCode::UChar,
            DbFieldType::Short => DbfCode::Short,
            DbFieldType::UShort => DbfCode::UShort,
            DbFieldType::Long => DbfCode::Long,
            DbFieldType::ULong => DbfCode::ULong,
            DbFieldType::Int64 => DbfCode::Int64,
            DbFieldType::UInt64 => DbfCode::UInt64,
            DbFieldType::Float => DbfCode::Float,
            DbFieldType::Double => DbfCode::Double,
            DbFieldType::Enum => DbfCode::Enum,
        }
    }

    /// This type as the CA **wire** carries its value.
    ///
    /// The single owner of the one row where the wire and the database
    /// disagree. `db_access.h:40` is `typedef epicsUInt8 dbr_char_t;`, so a
    /// `DBR_CHAR` element off the network is UNSIGNED; the `DBF_CHAR` it
    /// shares a name with is `epicsInt8` (`epicsTypes.h:44`). Every other
    /// row names the same type twice, and `DBF_UCHAR` has no wire code of
    /// its own — it promotes to `DBR_CHAR` (`db_convert.h`
    /// `dbDBRnewToDBRold`), which is why one carrier serves both.
    ///
    /// [`Self::from_u16`] and [`crate::types::native_type_for_dbr`] answer
    /// the DATABASE question: which field type does this code name. Neither
    /// is the wire's answer, so every site that turns *received* CA bytes
    /// into a value composes one of them with this. The naive answer costs
    /// a sign: byte `0xC8` is 200 to C and -56 without this.
    ///
    /// The signed reading is not lost, it is just not the carrier's. C
    /// re-creates it at the DISPLAY step — `val2str` assigns the
    /// `dbr_char_t` into a plain `char` before `sprintf("%d")`
    /// (`ca/src/tools/tool_lib.c:114`, `:160-161`) — which is why C's own
    /// `caget` prints -56 for a byte the wire called 200.
    pub fn wire_carrier(self) -> Self {
        match self {
            Self::Char => Self::UChar,
            other => other,
        }
    }

    /// The DATABASE field type a `DBF_` index names. **Not** the carrier of
    /// a CA wire payload — compose with [`Self::wire_carrier`] for that.
    pub fn from_u16(v: u16) -> CaResult<Self> {
        match v {
            0 => Ok(Self::String),
            1 => Ok(Self::Short),
            2 => Ok(Self::Float),
            3 => Ok(Self::Enum),
            4 => Ok(Self::Char),
            5 => Ok(Self::Long),
            6 => Ok(Self::Double),
            _ => Err(CaError::UnsupportedType(v)),
        }
    }

    /// Size in bytes for a single element of this type's native carrier.
    ///
    /// This is the carrier width (`UShort` = 2, `ULong` = 4), not the
    /// CA-wire-promoted width: the CA value path always promotes via
    /// [`crate::types::EpicsValue::dbr_type`] first and sizes buffers off
    /// the promoted type (`UShort`→`Long`=4, `ULong`→`Double`=8), so this
    /// width is never used to size a CA value array for the unsigned types.
    pub fn element_size(&self) -> usize {
        match self {
            Self::String => 40, // MAX_STRING_SIZE
            Self::Short | Self::Enum | Self::UShort => 2,
            Self::Float | Self::Long | Self::ULong => 4,
            Self::Char | Self::UChar => 1,
            Self::Double | Self::Int64 | Self::UInt64 => 8,
        }
    }

    /// Return the wire type code as a `u16`. The internal-only types have
    /// no CA wire code, so they report the signed CA type the IOC promotes
    /// them to (C `dbDBRnewToDBRold`, `db_convert.h`): `Int64`/`UInt64`/
    /// `ULong` → `DBR_DOUBLE` (6), `UShort` → `DBR_LONG` (5, the smallest
    /// signed CA type that holds the full `0..=65535` range), `UChar` →
    /// `DBR_CHAR` (4, same 1-byte wire type — the bytes are identical, only
    /// the interpretation is unsigned).
    pub fn ca_wire_type(&self) -> u16 {
        match self {
            Self::Int64 | Self::UInt64 | Self::ULong => Self::Double as u16,
            Self::UShort => Self::Long as u16,
            Self::UChar => Self::Char as u16,
            other => *other as u16,
        }
    }

    /// Return the `DBR_STS_xxx` type code for this native type
    /// (Int64 maps to `DBR_STS_DOUBLE`).
    pub fn sts_dbr_type(&self) -> u16 {
        self.ca_wire_type() + 7
    }

    /// Return the `DBR_TIME_xxx` type code for this native type
    /// (Int64 maps to `DBR_TIME_DOUBLE`).
    pub fn time_dbr_type(&self) -> u16 {
        self.ca_wire_type() + 14
    }

    /// Return the `DBR_GR_xxx` type code for this native type
    /// (Int64 maps to `DBR_GR_DOUBLE`).
    pub fn gr_dbr_type(&self) -> u16 {
        self.ca_wire_type() + 21
    }

    /// Return the `DBR_CTRL_xxx` type code for this native type
    /// (Int64 maps to `DBR_CTRL_DOUBLE`).
    pub fn ctrl_dbr_type(&self) -> u16 {
        self.ca_wire_type() + 28
    }

    /// Calculate total buffer size for N elements of this type.
    /// Equivalent to C EPICS dbValueSize(type) * count.
    pub fn buffer_size(&self, count: usize) -> usize {
        self.element_size() * count
    }

    /// Map field type to request type (C EPICS mapDBFToDBR).
    /// DBF_MENU and DBF_DEVICE map to DBR_ENUM in C EPICS.
    /// In Rust these are already represented as DbFieldType::Enum,
    /// so this is an identity mapping for documentation/completeness.
    pub fn to_dbr_type(&self) -> DbFieldType {
        *self
    }
}

/// dbStatic link-field classes — the three `dbfType` values that mark a
/// record field as a *link* rather than a value
/// (`dbFldTypes.h`: `DBF_INLINK`=14, `DBF_OUTLINK`=15, `DBF_FWDLINK`=16).
///
/// pvxs rejects a QSRV group PUT to any field whose
/// `dbChannelFinalFieldType` falls in `DBF_INLINK..=DBF_FWDLINK`
/// (`ioc/groupsource.cpp:596-606`). [`dbf_link_class`] answers the same
/// question from the port's dbStatic field table — the generated
/// `FieldDesc::declared_dbf`. Consumers gate "is this field a link" on
/// `dbf_link_class(..).is_some()` (or [`is_link_dbf_type`] when they
/// already hold a dbStatic code), rather than maintaining their own
/// partial spelling lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DbfLinkClass {
    /// `DBF_INLINK` (14) — an input link (`INP`, `DOL`, `SIML`, …).
    InLink = DBF_INLINK,
    /// `DBF_OUTLINK` (15) — an output link (`OUT`, `LNKn`, …).
    OutLink = DBF_OUTLINK,
    /// `DBF_FWDLINK` (16) — a forward link (`FLNK`).
    FwdLink = DBF_FWDLINK,
}

/// `dbFldTypes.h` `DBF_INLINK`.
pub const DBF_INLINK: u8 = 14;
/// `dbFldTypes.h` `DBF_OUTLINK`.
pub const DBF_OUTLINK: u8 = 15;
/// `dbFldTypes.h` `DBF_FWDLINK`.
pub const DBF_FWDLINK: u8 = 16;

impl DbfLinkClass {
    /// The dbStatic `dbfType` numeric code (`dbFldTypes.h`).
    pub fn dbf_type(self) -> u8 {
        self as u8
    }
}

/// True iff `dbf_type` is a link class — the exact
/// `DBF_INLINK <= t <= DBF_FWDLINK` range check pvxs applies in
/// `ioc/groupsource.cpp:596-606`. Use when a caller already holds a
/// dbStatic field-type code; [`dbf_link_class`] is the entry point that
/// resolves the code from the record type and field name first.
pub fn is_link_dbf_type(dbf_type: u8) -> bool {
    (DBF_INLINK..=DBF_FWDLINK).contains(&dbf_type)
}

/// Classify a record field by its dbStatic link class, or `None` when it
/// is not a link field. The single canonical owner of the "is this field a
/// link" rule for the Rust port.
///
/// The answer is the `.dbd` declaration, read through
/// [`declared_field`](crate::server::record::declared_field) — the by-name
/// half of the same lookup a caller holding a record does with
/// `field_desc_of`. Nothing here reasons about the field's spelling.
///
/// It used to. `FieldDesc` carried only the type a field is SERVED as, which
/// is `DBF_STRING` for all three link classes, so the class had to be
/// reconstructed from the name: an exact list, plus `INP`/`DOL`/`OUT`/`LNK`
/// followed by exactly one alphanumeric, plus per-record-type exceptions for
/// `SIOL` and `LNK*`. Measured against the declarations `declared_dbf` now
/// carries, that rule was wrong at 86 fields — 24 `INAA..INLL` inputs on
/// `acalcout`/`scalcout` and 52 more on `motor`, `table`, `scaler`, `epid` and
/// `throttle` that it called non-links, and 10 non-links (`*.OUTV`,
/// `calcout.INPV`, `compress.INPN`, five `swait` fields) that it called links.
/// A one-character-suffix rule cannot express a two-character suffix, and no
/// exception list catches a family it has never been told about.
///
/// `record_type` no longer resolves ambiguity — it selects the declaration
/// table. A type `dbd_generated` does not cover (`motor`, `table`, …) must
/// have registered its factory for its own fields to resolve; until then only
/// `dbCommon` answers. That is the same condition under which the record could
/// be instantiated at all.
pub fn dbf_link_class(record_type: &str, field: &str) -> Option<DbfLinkClass> {
    let desc = crate::server::record::declared_field(record_type, field.trim())?;
    match desc.declared_dbf {
        DbfCode::Inlink => Some(DbfLinkClass::InLink),
        DbfCode::Outlink => Some(DbfLinkClass::OutLink),
        DbfCode::Fwdlink => Some(DbfLinkClass::FwdLink),
        _ => None,
    }
}

/// Calculate buffer size for a DBR type including metadata, matching C
/// `dbr_size_n(TYPE, COUNT) = dbr_size[TYPE] + (COUNT-1)*dbr_value_size[TYPE]`.
///
/// the metadata length is taken from
/// `crate::types::codec::dbr_meta_size` — the single owner that the
/// serializers (`serialize_dbr` / `encode_dbr`) emit against — so the
/// explicit-count pad/truncate and no-read-access frame paths size
/// TIME / GR / CTRL bodies exactly as the encoder writes them. A
/// `metadata_matches_encoded_length` test pins `encoded_len ==
/// dbr_buffer_size` across the whole (dbr_type, native) matrix, so the
/// sizer can no longer drift from the encoder.
pub fn dbr_buffer_size(dbr_type: u16, native_type: DbFieldType, count: usize) -> usize {
    // DBR_CLASS_NAME (38) is always one MAX_STRING_SIZE (40) string,
    // regardless of `count` or `native_type` — it carries no value[]
    // array, so the generic meta+value formula does not apply.
    if dbr_type == DBR_CLASS_NAME {
        return 40;
    }
    let value_size = native_type.element_size() * count;
    crate::types::codec::dbr_meta_size(dbr_type, native_type) + value_size
}

/// Extract the native DBF type index (0-6) from any DBR type code.
fn dbr_native_index(dbr_type: u16) -> Option<u16> {
    match dbr_type {
        0..=6 => Some(dbr_type),
        7..=13 => Some(dbr_type - 7),
        14..=20 => Some(dbr_type - 14),
        21..=27 => Some(dbr_type - 21),
        28..=34 => Some(dbr_type - 28),
        // Alarm-acknowledge writes carry a single u16, so map them to
        // Short for codec purposes. STSACK_STRING returns a string body
        // so it maps to String.
        35 | 36 => Some(1), // DBR_PUT_ACKT / DBR_PUT_ACKS — u16
        37 => Some(0),      // DBR_STSACK_STRING — value is a string
        // DBR_CLASS_NAME is a single fixed 40-byte string carrying the
        // record's recordType. Treat as String for codec purposes.
        38 => Some(0),
        _ => None,
    }
}

/// The DATABASE field type a CA DBR code is named after.
///
/// **Not** the carrier of a payload that arrived over the wire: compose
/// with [`DbFieldType::wire_carrier`] for that. The two answers differ for
/// the CHAR row only, and that one row is the whole of CA's signedness
/// mismatch.
pub fn native_type_for_dbr(dbr_type: u16) -> CaResult<DbFieldType> {
    match dbr_native_index(dbr_type) {
        Some(idx) => DbFieldType::from_u16(idx),
        None => Err(CaError::UnsupportedType(dbr_type)),
    }
}

/// DBR request-type names indexed by type code, mirroring the C
/// `dbr_text[]` table (`ca/src/client/access.cpp`). Index 0 =
/// `DBR_STRING` … index 38 = `DBR_CLASS_NAME`.
const DBR_TEXT: [&str; (LAST_BUFFER_TYPE + 1) as usize] = [
    "DBR_STRING",
    "DBR_SHORT",
    "DBR_FLOAT",
    "DBR_ENUM",
    "DBR_CHAR",
    "DBR_LONG",
    "DBR_DOUBLE",
    "DBR_STS_STRING",
    "DBR_STS_SHORT",
    "DBR_STS_FLOAT",
    "DBR_STS_ENUM",
    "DBR_STS_CHAR",
    "DBR_STS_LONG",
    "DBR_STS_DOUBLE",
    "DBR_TIME_STRING",
    "DBR_TIME_SHORT",
    "DBR_TIME_FLOAT",
    "DBR_TIME_ENUM",
    "DBR_TIME_CHAR",
    "DBR_TIME_LONG",
    "DBR_TIME_DOUBLE",
    "DBR_GR_STRING",
    "DBR_GR_SHORT",
    "DBR_GR_FLOAT",
    "DBR_GR_ENUM",
    "DBR_GR_CHAR",
    "DBR_GR_LONG",
    "DBR_GR_DOUBLE",
    "DBR_CTRL_STRING",
    "DBR_CTRL_SHORT",
    "DBR_CTRL_FLOAT",
    "DBR_CTRL_ENUM",
    "DBR_CTRL_CHAR",
    "DBR_CTRL_LONG",
    "DBR_CTRL_DOUBLE",
    "DBR_PUT_ACKT",
    "DBR_PUT_ACKS",
    "DBR_STSACK_STRING",
    "DBR_CLASS_NAME",
];

/// Resolve a DBR request-type name to its type code, mirroring the C
/// `dbr_text_to_type` macro (`db_access.h`): an exact, **case-sensitive**
/// `strcmp` search of the `dbr_text[]` table. Returns the matching code
/// (`0..=38`) or `None` when no name matches.
///
/// The case sensitivity is faithful to C — the `caget`/`caput` tools
/// feed `-d <type>` straight through this search, so `-d DBR_TIME_FLOAT`
/// resolves while `-d dbr_time_float` does not (the C tool then reverts
/// to its plain/native request). Callers that accept the bare family
/// (`caget -d TIME_FLOAT`) retry with a `DBR_` prefix, exactly as
/// `caget.c` does.
pub fn dbr_text_to_type(text: &str) -> Option<u16> {
    DBR_TEXT.iter().position(|&n| n == text).map(|i| i as u16)
}

/// Resolve a DBR type code to its name, mirroring the C
/// `dbr_type_to_text` macro (`db_access.h`): an index into the same
/// `dbr_text[]` table, with C's `"DBR_invalid"` for anything outside
/// `0..=38`. Inverse of [`dbr_text_to_type`], and the single owner of
/// that direction — the CA client's exception block
/// (`CA.Client.Exception ... type=%s`) and `caget -d`'s "Request type:"
/// line both read the names from here.
pub fn dbr_type_to_text(code: u16) -> &'static str {
    DBR_TEXT
        .get(code as usize)
        .copied()
        .unwrap_or("DBR_invalid")
}

#[cfg(test)]
mod buffer_size_tests {
    use super::*;

    /// STS meta size is per-type. `dbr_sts_double` carries a
    /// 4-byte `dbr_long_t` RISC_pad (db_access.h:233-238) → meta 8.
    #[test]
    fn sts_double_meta_is_8() {
        // scalar: 8 (meta) + 8 (value) = 16
        assert_eq!(dbr_buffer_size(DBR_STS_DOUBLE, DbFieldType::Double, 1), 16);
        // n elements: 8 + 8*n
        assert_eq!(
            dbr_buffer_size(DBR_STS_DOUBLE, DbFieldType::Double, 5),
            8 + 8 * 5
        );
    }

    /// `dbr_sts_char` carries a 1-byte RISC_pad
    /// (db_access.h:218-223) → meta 5.
    #[test]
    fn sts_char_meta_is_5() {
        assert_eq!(dbr_buffer_size(DBR_STS_CHAR, DbFieldType::Char, 1), 6);
        assert_eq!(dbr_buffer_size(DBR_STS_CHAR, DbFieldType::Char, 10), 5 + 10);
    }

    /// types with no STS RISC pad keep the flat 4-byte meta.
    #[test]
    fn sts_short_meta_is_4() {
        assert_eq!(dbr_buffer_size(DBR_STS_SHORT, DbFieldType::Short, 1), 6);
        assert_eq!(dbr_buffer_size(DBR_STS_LONG, DbFieldType::Long, 1), 8);
        assert_eq!(dbr_buffer_size(DBR_STS_FLOAT, DbFieldType::Float, 1), 8);
    }

    /// Plain values carry no metadata.
    #[test]
    fn plain_value_size_only() {
        assert_eq!(dbr_buffer_size(DBR_DOUBLE, DbFieldType::Double, 3), 24);
    }

    /// TIME structs carry a per-type RISC pad before `value[0]`
    /// (C `dbr_time_*`, db_access.h:250-300). The pre-fix flat 12-byte
    /// TIME meta truncated double/short/enum/char bodies.
    #[test]
    fn time_meta_includes_risc_pad() {
        // double: 12 + RISC_pad(4) + value(8) = 24 (was wrongly 20).
        assert_eq!(dbr_buffer_size(DBR_TIME_DOUBLE, DbFieldType::Double, 1), 24);
        // short/enum: 12 + pad(2) + value(2) = 16.
        assert_eq!(dbr_buffer_size(DBR_TIME_SHORT, DbFieldType::Short, 1), 16);
        assert_eq!(dbr_buffer_size(DBR_TIME_ENUM, DbFieldType::Enum, 1), 16);
        // char: 12 + pad(3) + value(1) = 16.
        assert_eq!(dbr_buffer_size(DBR_TIME_CHAR, DbFieldType::Char, 1), 16);
        // float/long: no pad (value already 4-aligned at offset 12).
        assert_eq!(dbr_buffer_size(DBR_TIME_FLOAT, DbFieldType::Float, 1), 16);
        assert_eq!(dbr_buffer_size(DBR_TIME_LONG, DbFieldType::Long, 1), 16);
        // Explicit count scales the value array after the pad.
        assert_eq!(
            dbr_buffer_size(DBR_TIME_DOUBLE, DbFieldType::Double, 4),
            16 + 8 * 4
        );
    }

    /// GR/CTRL metadata is per native type (the pre-fix single
    /// broad formula over-padded short/char/float/long and dropped the
    /// enum `no_str` word).
    #[test]
    fn gr_ctrl_meta_is_per_type() {
        // GR (6 limits): head(4) + layout.
        assert_eq!(dbr_buffer_size(DBR_GR_SHORT, DbFieldType::Short, 1), 24 + 2);
        assert_eq!(dbr_buffer_size(DBR_GR_FLOAT, DbFieldType::Float, 1), 40 + 4);
        assert_eq!(
            dbr_buffer_size(DBR_GR_DOUBLE, DbFieldType::Double, 1),
            64 + 8
        );
        assert_eq!(dbr_buffer_size(DBR_GR_CHAR, DbFieldType::Char, 1), 19 + 1);
        assert_eq!(dbr_buffer_size(DBR_GR_LONG, DbFieldType::Long, 1), 36 + 4);
        // Enum: head(4) + no_str(2) + 16*26 strings = 422, value(2).
        assert_eq!(dbr_buffer_size(DBR_GR_ENUM, DbFieldType::Enum, 1), 422 + 2);
        // CTRL adds two control limits.
        assert_eq!(
            dbr_buffer_size(DBR_CTRL_DOUBLE, DbFieldType::Double, 1),
            80 + 8
        );
        assert_eq!(
            dbr_buffer_size(DBR_CTRL_SHORT, DbFieldType::Short, 1),
            28 + 2
        );
        assert_eq!(dbr_buffer_size(DBR_CTRL_CHAR, DbFieldType::Char, 1), 21 + 1);
    }
}

#[cfg(test)]
mod dbf_link_class_tests {
    use super::*;

    #[test]
    fn dbcommon_links_classified_uniformly() {
        // Present on every record (dbCommon.dbd).
        assert_eq!(dbf_link_class("ai", "FLNK"), Some(DbfLinkClass::FwdLink));
        assert_eq!(dbf_link_class("ao", "SDIS"), Some(DbfLinkClass::InLink));
        assert_eq!(dbf_link_class("calc", "TSEL"), Some(DbfLinkClass::InLink));
    }

    #[test]
    fn record_specific_link_families_the_old_name_list_missed() {
        // The families the reviewed partial spelling list omitted, each a
        // DBF_INLINK/OUTLINK in EPICS Base `*.dbd.pod`:
        //   seqRecord DOL0 (INLINK) / LNK0 (OUTLINK) / DOLA / DOLF / LNKF
        assert_eq!(dbf_link_class("seq", "DOL0"), Some(DbfLinkClass::InLink));
        assert_eq!(dbf_link_class("seq", "LNK0"), Some(DbfLinkClass::OutLink));
        assert_eq!(dbf_link_class("seq", "DOLA"), Some(DbfLinkClass::InLink));
        assert_eq!(dbf_link_class("seq", "DOLF"), Some(DbfLinkClass::InLink));
        assert_eq!(dbf_link_class("seq", "LNKF"), Some(DbfLinkClass::OutLink));
        //   selRecord NVL (INLINK); histogramRecord SVL (INLINK)
        assert_eq!(dbf_link_class("sel", "NVL"), Some(DbfLinkClass::InLink));
        assert_eq!(
            dbf_link_class("histogram", "SVL"),
            Some(DbfLinkClass::InLink)
        );
        //   calc/aSub INPA..INPU (INLINK); dfanout/aSub OUTA (OUTLINK)
        assert_eq!(dbf_link_class("calc", "INPA"), Some(DbfLinkClass::InLink));
        assert_eq!(dbf_link_class("aSub", "INPU"), Some(DbfLinkClass::InLink));
        assert_eq!(
            dbf_link_class("dfanout", "OUTA"),
            Some(DbfLinkClass::OutLink)
        );
        // `fanout` declares LNK0..LNKF and no OUT family at all; the name
        // rule this replaced answered `OutLink` for a field that does not
        // exist on the type.
        assert_eq!(dbf_link_class("fanout", "OUTA"), None);
        //   printf INP0..INP9 (INLINK)
        assert_eq!(dbf_link_class("printf", "INP0"), Some(DbfLinkClass::InLink));
    }

    /// **The invariant this function exists to hold**: for every record type
    /// and every field, `dbf_link_class` agrees with the `.dbd` declaration.
    ///
    /// One case per declaration, not one per link family — a per-family test
    /// is what let 86 fields drift, because a family nobody wrote a case for
    /// is invisible to it. This walks the table instead, so a new record type
    /// or a new link family is covered the day it is generated.
    #[test]
    fn every_declared_field_classifies_as_its_dbd_says() {
        use crate::server::record::dbd_generated::{DB_COMMON_FIELDS, RECORD_TYPES, record_fields};
        let mut wrong = Vec::new();
        for rt in RECORD_TYPES {
            let own = record_fields(rt).unwrap_or(&[]);
            for d in DB_COMMON_FIELDS.iter().chain(own.iter()) {
                let declared = match d.declared_dbf {
                    DbfCode::Inlink => Some(DbfLinkClass::InLink),
                    DbfCode::Outlink => Some(DbfLinkClass::OutLink),
                    DbfCode::Fwdlink => Some(DbfLinkClass::FwdLink),
                    _ => None,
                };
                let got = dbf_link_class(rt, d.name);
                if got != declared {
                    wrong.push(format!("{rt}.{} declared={declared:?} got={got:?}", d.name));
                }
            }
        }
        assert!(
            wrong.is_empty(),
            "{} fields disagree with their declaration:\n{}",
            wrong.len(),
            wrong.join("\n")
        );
    }

    /// The two shapes the name rule could not express, pinned by name so a
    /// regression names itself rather than appearing as a count.
    #[test]
    fn the_two_shapes_the_name_rule_could_not_express() {
        // A two-character suffix. `INAA..INLL` are 12 declared `DBF_INLINK`
        // inputs on each of `acalcout` and `scalcout`; the one-alphanumeric
        // suffix rule called all 24 non-links.
        for rt in ["acalcout", "scalcout"] {
            for f in ["INAA", "INBB", "INLL"] {
                assert_eq!(
                    dbf_link_class(rt, f),
                    Some(DbfLinkClass::InLink),
                    "{rt}.{f}"
                );
            }
        }
        // A non-link that merely spells like one. `OUTV` is the output-value
        // field, `INPV` the input-value one; neither is a link.
        for (rt, f) in [
            ("acalcout", "OUTV"),
            ("calcout", "OUTV"),
            ("scalcout", "OUTV"),
            ("calcout", "INPV"),
            ("compress", "INPN"),
            ("swait", "DOLN"),
            ("swait", "OUTN"),
            ("swait", "DOLV"),
            ("swait", "OUTV"),
            ("swait", "DOLD"),
        ] {
            assert_eq!(dbf_link_class(rt, f), None, "{rt}.{f} is not a link field");
        }
    }

    /// A record type outside `dbd_generated` resolves through the registry a
    /// factory registration fills — the by-name mirror of `field_list`.
    #[test]
    fn a_registered_downstream_type_resolves_its_own_declarations() {
        use crate::server::record::{FieldDesc, register_declared_fields};
        static FIELDS: &[FieldDesc] = &[FieldDesc {
            name: "RDBL",
            dbf_type: DbFieldType::String,
            declared_dbf: DbfCode::Inlink,
            runtime_typed: false,
            read_only: false,
            special: crate::server::record::Special::None,
            declared_special: crate::server::record::Special::None,
            pp: false,
            asl: crate::server::record::Asl::Asl1,
            size: 0,
            extra: None,
            menu: None,
            initial: None,
            interest: 0,
            prop: false,
            prompt: None,
            promptgroup: None,
            base: crate::server::record::Base::Decimal,
        }];

        // Before registration only `dbCommon` answers for an unknown type.
        assert_eq!(dbf_link_class("zzTestRec", "RDBL"), None);
        assert_eq!(
            dbf_link_class("zzTestRec", "FLNK"),
            Some(DbfLinkClass::FwdLink)
        );

        register_declared_fields("zzTestRec", FIELDS);
        assert_eq!(
            dbf_link_class("zzTestRec", "RDBL"),
            Some(DbfLinkClass::InLink),
            "the name rule this replaced answered None for motor's RDBL"
        );
    }

    #[test]
    fn siol_class_depends_on_record_direction() {
        // boRecord.dbd.pod:318 SIOL=DBF_OUTLINK; ai/bi SIOL=DBF_INLINK.
        assert_eq!(dbf_link_class("bo", "SIOL"), Some(DbfLinkClass::OutLink));
        assert_eq!(dbf_link_class("ao", "SIOL"), Some(DbfLinkClass::OutLink));
        assert_eq!(dbf_link_class("ai", "SIOL"), Some(DbfLinkClass::InLink));
        assert_eq!(dbf_link_class("bi", "SIOL"), Some(DbfLinkClass::InLink));
        // SIML is always DBF_INLINK regardless of direction.
        assert_eq!(dbf_link_class("bo", "SIML"), Some(DbfLinkClass::InLink));
        assert_eq!(dbf_link_class("ai", "SIML"), Some(DbfLinkClass::InLink));
    }

    /// Every `field(SIOL,DBF_*)` in the record types this workspace ports,
    /// read out of the C dbds. synApps `busy` is the one non-Base output
    /// record — it derives from `bo` and declares `field(SIOL,DBF_OUTLINK)`
    /// (`busyRecord.dbd`) — while synApps `swait` (`swaitRecord.dbd`) and
    /// `mca` (`mcaRecord.dbd`) declare `DBF_INLINK`. A missing output entry is
    /// silent: the classifier defaults to `InLink`, and the CP/CPP mask C
    /// applies to an output link (`dbStaticLib.c:2380-2391`) is then skipped.
    #[test]
    fn siol_direction_matches_every_c_dbd_this_workspace_ports() {
        for rtype in [
            "ao",
            "bo",
            "busy",
            "longout",
            "int64out",
            "mbbo",
            "mbboDirect",
            "stringout",
            "lso",
            "aao",
        ] {
            assert_eq!(
                dbf_link_class(rtype, "SIOL"),
                Some(DbfLinkClass::OutLink),
                "{rtype} declares field(SIOL,DBF_OUTLINK)"
            );
        }
        for rtype in [
            "ai",
            "bi",
            "mbbi",
            "mbbiDirect",
            "longin",
            "int64in",
            "stringin",
            "lsi",
            "event",
            "waveform",
            "aai",
            "histogram",
            "swait",
            // `mca` declares `field(SIOL,DBF_INLINK)` too, but it lives in
            // `mca-rs` and reaches the by-name lookup only once that crate
            // registers its factory — which no `epics-base-rs` unit test
            // does. Asserting it here only ever tested the name rule.
        ] {
            assert_eq!(
                dbf_link_class(rtype, "SIOL"),
                Some(DbfLinkClass::InLink),
                "{rtype} declares field(SIOL,DBF_INLINK)"
            );
        }
    }

    /// The consequence a misclassified SIOL actually has: `check_link_assignment`
    /// turns the class into a [`LinkFieldType`], and only the `Out` arm applies
    /// C's `modifiers &= ~(pvlOptCPP|pvlOptCP)`. Classified as an input, a
    /// `busy` SIOL would keep a CPP that C strips.
    #[test]
    fn a_busy_siol_discards_cp_the_way_an_output_link_must() {
        use crate::server::record::{
            LinkFieldType, LinkProcessPolicy, ParsedLink, parse_link_field,
        };

        let ftype = match dbf_link_class("busy", "SIOL").unwrap() {
            DbfLinkClass::InLink => LinkFieldType::In,
            DbfLinkClass::OutLink => LinkFieldType::Out,
            DbfLinkClass::FwdLink => LinkFieldType::Fwd,
        };
        match parse_link_field("TGT.VAL CPP", ftype) {
            ParsedLink::Db(db) => assert_eq!(db.policy, LinkProcessPolicy::NoProcess),
            other => panic!("expected a db link, got {other:?}"),
        }
    }

    /// `LNK0..LNKF` shares
    /// its spelling across two DBF classes — `fanoutRecord` declares the
    /// family `DBF_FWDLINK`, while `seqRecord`/`sseqRecord` declare it
    /// `DBF_OUTLINK`. The classifier must resolve by `record_type`, not
    /// collapse every `LNK*` to OutLink by prefix.
    #[test]
    fn lnk_class_depends_on_record_type() {
        // fanoutRecord.dbd.pod field(LNK0,DBF_FWDLINK) … field(LNKF,…).
        assert_eq!(
            dbf_link_class("fanout", "LNK0"),
            Some(DbfLinkClass::FwdLink),
            "fanout LNK0 is DBF_FWDLINK (16), not OUTLINK"
        );
        assert_eq!(
            dbf_link_class("fanout", "LNKF"),
            Some(DbfLinkClass::FwdLink),
            "fanout LNKF is DBF_FWDLINK (16), not OUTLINK"
        );
        // seqRecord.dbd.pod field(LNK0,DBF_OUTLINK): the default direction
        // for the shared spelling.
        assert_eq!(dbf_link_class("seq", "LNK0"), Some(DbfLinkClass::OutLink));
        assert_eq!(dbf_link_class("seq", "LNKF"), Some(DbfLinkClass::OutLink));
        // synApps sseqRecord LNK1..LNK9 are also DBF_OUTLINK (LNK10 is the
        // two-char spelling the one-alnum suffix rule deliberately rejects).
        assert_eq!(dbf_link_class("sseq", "LNK1"), Some(DbfLinkClass::OutLink));
        // fanout's dbCommon FLNK is still a forward link (unchanged).
        assert_eq!(
            dbf_link_class("fanout", "FLNK"),
            Some(DbfLinkClass::FwdLink)
        );
    }

    #[test]
    fn plain_value_fields_are_not_links() {
        for f in [
            "VAL", "EGU", "PREC", "HOPR", "RVAL", "DESC", "A", "B", "OVAL",
        ] {
            assert_eq!(dbf_link_class("ai", f), None, "{f} must not be a link");
        }
    }

    #[test]
    fn case_insensitive_and_trims() {
        assert_eq!(dbf_link_class("ai", "inp"), Some(DbfLinkClass::InLink));
        assert_eq!(dbf_link_class("ao", " OUT "), Some(DbfLinkClass::OutLink));
    }

    #[test]
    fn dbf_codes_and_range_match_pvxs() {
        assert_eq!(DbfLinkClass::InLink.dbf_type(), 14);
        assert_eq!(DbfLinkClass::OutLink.dbf_type(), 15);
        assert_eq!(DbfLinkClass::FwdLink.dbf_type(), 16);
        // pvxs groupsource.cpp:596-606 range check.
        assert!(is_link_dbf_type(DBF_INLINK));
        assert!(is_link_dbf_type(DBF_OUTLINK));
        assert!(is_link_dbf_type(DBF_FWDLINK));
        assert!(!is_link_dbf_type(13)); // DBF_DEVICE
        assert!(!is_link_dbf_type(17)); // DBF_NOACCESS
        assert!(!is_link_dbf_type(0)); // DBF_STRING
    }
}

#[cfg(test)]
mod dbr_text_tests {
    use super::*;

    #[test]
    fn resolves_every_type_name_to_its_code() {
        // Round-trip the whole table: each name resolves to its index.
        for (code, name) in DBR_TEXT.iter().enumerate() {
            assert_eq!(
                dbr_text_to_type(name),
                Some(code as u16),
                "{name} should resolve to {code}"
            );
        }
        // Boundary names spot-check (the cited High-finding values).
        assert_eq!(dbr_text_to_type("DBR_STRING"), Some(DBR_STRING));
        assert_eq!(dbr_text_to_type("DBR_TIME_FLOAT"), Some(DBR_TIME_FLOAT));
        assert_eq!(
            dbr_text_to_type("DBR_STSACK_STRING"),
            Some(DBR_STSACK_STRING)
        );
        assert_eq!(dbr_text_to_type("DBR_CLASS_NAME"), Some(DBR_CLASS_NAME));
    }

    #[test]
    fn is_case_sensitive_like_c_strcmp() {
        // C `dbr_text_to_type` uses `strcmp`, so lowercase does not
        // match — the C tools then revert to their plain request.
        assert_eq!(dbr_text_to_type("dbr_time_float"), None);
        assert_eq!(dbr_text_to_type("DBR_Time_Float"), None);
    }

    #[test]
    fn unknown_and_bare_family_names_do_not_match() {
        // Bare family names need the caller's `DBR_` retry — the raw
        // search rejects them, matching C's first-pass `strcmp`.
        assert_eq!(dbr_text_to_type("TIME_FLOAT"), None);
        assert_eq!(dbr_text_to_type("DOUBLE"), None);
        assert_eq!(dbr_text_to_type("DBR_NONSENSE"), None);
        assert_eq!(dbr_text_to_type(""), None);
    }
}

#[cfg(test)]
mod dbf_code_tests {
    use super::*;

    /// C's `dbfType` in declaration order (`dbFldTypes.h:24-43` @R7.0.10),
    /// paired with `pamapdbfType`'s strings (`:51-70`) as `dbTest.c:76-81`
    /// carries them, prefix-stripped.
    ///
    /// The WHOLE table, not a sample. A nineteenth type inserted in the
    /// middle has to fail on its own row here rather than silently shift
    /// every code after it — the codes are what `dbDumpField` prints and
    /// what a `.dbd` interchange file means by `DBF_MENU`.
    const C_DBF_TYPE: [(DbfCode, i16, &str); 18] = [
        (DbfCode::String, 0, "STRING"),
        (DbfCode::Char, 1, "CHAR"),
        (DbfCode::UChar, 2, "UCHAR"),
        (DbfCode::Short, 3, "SHORT"),
        (DbfCode::UShort, 4, "USHORT"),
        (DbfCode::Long, 5, "LONG"),
        (DbfCode::ULong, 6, "ULONG"),
        (DbfCode::Int64, 7, "INT64"),
        (DbfCode::UInt64, 8, "UINT64"),
        (DbfCode::Float, 9, "FLOAT"),
        (DbfCode::Double, 10, "DOUBLE"),
        (DbfCode::Enum, 11, "ENUM"),
        (DbfCode::Menu, 12, "MENU"),
        (DbfCode::Device, 13, "DEVICE"),
        (DbfCode::Inlink, 14, "INLINK"),
        (DbfCode::Outlink, 15, "OUTLINK"),
        (DbfCode::Fwdlink, 16, "FWDLINK"),
        (DbfCode::NoAccess, 17, "NOACCESS"),
    ];

    /// The table above lists every variant: this match is exhaustive, so a
    /// nineteenth variant stops the build, and the pairwise-distinct check
    /// below then makes 18 rows a proof of coverage rather than of length.
    fn _the_enum_has_no_variant_outside_the_table(c: DbfCode) {
        match c {
            DbfCode::String
            | DbfCode::Char
            | DbfCode::UChar
            | DbfCode::Short
            | DbfCode::UShort
            | DbfCode::Long
            | DbfCode::ULong
            | DbfCode::Int64
            | DbfCode::UInt64
            | DbfCode::Float
            | DbfCode::Double
            | DbfCode::Enum
            | DbfCode::Menu
            | DbfCode::Device
            | DbfCode::Inlink
            | DbfCode::Outlink
            | DbfCode::Fwdlink
            | DbfCode::NoAccess => (),
        }
    }

    #[test]
    fn every_dbf_code_carries_cs_number_and_name() {
        for (i, (code, n, name)) in C_DBF_TYPE.iter().enumerate() {
            assert_eq!(*code as i16, *n, "DBF_{name}: wrong dbfType code");
            assert_eq!(code.name(), *name, "DBF_{name}: wrong name");
            // C's dbfType is dense 0..DBF_NTYPES-1 and `pamapdbfType` is
            // indexed by it, so position and code are the same fact.
            assert_eq!(i as i16, *n, "DBF_{name}: table position != code");
        }
        // DBF_NTYPES (`dbFldTypes.h:45`).
        assert_eq!(C_DBF_TYPE.len(), 18);
        for (a, (x, ..)) in C_DBF_TYPE.iter().enumerate() {
            for (y, ..) in C_DBF_TYPE.iter().skip(a + 1) {
                assert_ne!(x, y, "{x:?} listed twice");
            }
        }
    }

    #[test]
    fn every_served_type_maps_to_its_dbstatic_code() {
        // All twelve CA wire types, not a sample. The mapping only ever
        // answers a scalar code, because that is all a SERVED type can be —
        // `DbfCode::Menu`, `Device` and the three link codes are declarations
        // and are unreachable from here by construction.
        let table = [
            (DbFieldType::String, DbfCode::String),
            (DbFieldType::Short, DbfCode::Short),
            (DbFieldType::Float, DbfCode::Float),
            (DbFieldType::Enum, DbfCode::Enum),
            (DbFieldType::Char, DbfCode::Char),
            (DbFieldType::Long, DbfCode::Long),
            (DbFieldType::Double, DbfCode::Double),
            (DbFieldType::Int64, DbfCode::Int64),
            (DbFieldType::UInt64, DbfCode::UInt64),
            (DbFieldType::UShort, DbfCode::UShort),
            (DbFieldType::ULong, DbfCode::ULong),
            (DbFieldType::UChar, DbfCode::UChar),
        ];
        for (ft, code) in table {
            assert_eq!(ft.dbf_code(), code, "{ft:?}");
        }
        assert_eq!(table.len(), 12);
        // The CA wire codes are the OTHER numbering and must not have moved:
        // they are `DbFieldType`'s discriminants and are load-bearing on the
        // protocol, so the two tables disagreeing from the second entry on is
        // the expected state, not a defect.
        for (i, (ft, _)) in table.iter().enumerate() {
            assert_eq!(*ft as u16, i as u16, "{ft:?}: CA wire code moved");
        }
    }

    /// C's `mapDBFToDBR` (`dbAccess.c:76-95`), all eighteen rows in C's
    /// order, written as the DBR *names* C's initialiser comments give so a
    /// silent renumbering of either enum shows up as a name mismatch.
    #[test]
    fn the_dbr_map_is_cs_whole_table() {
        const C_MAP: [(DbfCode, DbfCode); 18] = [
            (DbfCode::String, DbfCode::String),
            (DbfCode::Char, DbfCode::Char),
            (DbfCode::UChar, DbfCode::UChar),
            (DbfCode::Short, DbfCode::Short),
            (DbfCode::UShort, DbfCode::UShort),
            (DbfCode::Long, DbfCode::Long),
            (DbfCode::ULong, DbfCode::ULong),
            (DbfCode::Int64, DbfCode::Int64),
            (DbfCode::UInt64, DbfCode::UInt64),
            (DbfCode::Float, DbfCode::Float),
            (DbfCode::Double, DbfCode::Double),
            (DbfCode::Enum, DbfCode::Enum),
            (DbfCode::Menu, DbfCode::Enum),
            (DbfCode::Device, DbfCode::Enum),
            (DbfCode::Inlink, DbfCode::String),
            (DbfCode::Outlink, DbfCode::String),
            (DbfCode::Fwdlink, DbfCode::String),
            (DbfCode::NoAccess, DbfCode::NoAccess),
        ];
        for (dbf, dbr) in C_MAP {
            assert_eq!(dbf.dbr_code(), dbr, "mapDBFToDBR[DBF_{}]", dbf.name());
        }
        assert_eq!(C_MAP.len(), 18);
        // C indexes `mapDBFToDBR` by the dbfType, so the table's position
        // and the code it maps are one fact — the same density check the
        // name table gets.
        for (i, (dbf, _)) in C_MAP.iter().enumerate() {
            assert_eq!(
                *dbf as i16,
                i as i16,
                "DBF_{}: row out of order",
                dbf.name()
            );
        }
        // `DBR_NOACCESS` is `DBF_NOACCESS` = 17 (`dbFldTypes.h:90`), NOT the
        // 12 that `printDbAddr`'s `dbr[]` index suggests: C remaps the index
        // to `DBR_ENUM+1` only to reach its shorter name array, and still
        // prints the unremapped 17 as the number (`dbTest.c:812-817`). Both
        // halves fall out of one code here, so the port cannot print the
        // pair inconsistently.
        assert_eq!(DbfCode::NoAccess.dbr_code() as i16, 17);
        assert_eq!(DbfCode::NoAccess.dbr_code().name(), "NOACCESS");
    }
}
