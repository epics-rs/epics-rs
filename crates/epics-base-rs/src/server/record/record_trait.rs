use crate::error::CaResult;
use crate::types::c_parse::Converted;
use crate::types::{DbFieldType, DbfCode, EpicsValue, PvString, Scalar, c_parse};

use super::CommonFields;
use super::alarm::{AlarmLimit, AlarmSeverity};
use super::record_instance::FieldIx;
use super::scan::ScanType;
use crate::server::device_support::DsetInit;

/// A `DBF_ENUM` `VAL`'s `get_enum_str` slots, borrowed from the record. The
/// variant carries C's out-of-range sentinel, whose spelling differs by class
/// (`boRecord.c:320-339` vs `mbbiRecord.c:235-255`).
#[derive(Debug, Clone, Copy)]
pub enum EnumSlots<'a> {
    /// `ZNAM`, `ONAM`: bi, bo, busy.
    Binary([&'a PvString; 2]),
    /// `ZRST`..`FFST`: mbbi, mbbo.
    Multibit([&'a PvString; 16]),
}

impl<'a> EnumSlots<'a> {
    fn parts(&self) -> (&[&'a PvString], &'static str) {
        match self {
            Self::Binary(slots) => (slots, "Illegal_Value"),
            Self::Multibit(slots) => (slots, "Illegal Value"),
        }
    }

    /// The slot at `idx`, empty or not; past the slots, the sentinel.
    pub fn label(self, idx: u16) -> &'a [u8] {
        let (slots, sentinel) = self.parts();
        match slots.get(idx as usize) {
            Some(slot) => slot.as_bytes(),
            None => sentinel.as_bytes(),
        }
    }

    pub fn form(self) -> crate::server::snapshot::EnumStringForm {
        let (slots, sentinel) = self.parts();
        crate::server::snapshot::EnumStringForm::states(
            slots.iter().map(|slot| (*slot).clone()).collect(),
            PvString::from(sentinel),
        )
    }
}

/// Which of a `devXxxSoftRaw` dset's two entry points is delivering a value to
/// [`Record::raw_soft_input`]. They are not the same function in C, and they do
/// not agree about `MASK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawSoftEntry {
    /// `devXxxSoftRaw::init_record` — `recGblInitConstantLink(&prec->inp,
    /// DBF_x, &prec->rval)` (`devAiSoftRaw.c:41`, `devBiSoftRaw.c:42`,
    /// `devMbbiSoftRaw.c:42`, `devMbbiDirectSoftRaw.c:42`). A CONSTANT `INP`
    /// (`field(INP,"12")`) is loaded ONCE, at iocInit, straight into `RVAL`.
    ///
    /// `recGblInitConstantLink` is a plain typed store — **no MASK**. The mask
    /// lives in `read_xxx`, which a constant INP never reaches (a constant link
    /// delivers nothing at process).
    InitConstant,
    /// `devXxxSoftRaw::read_xxx` — the per-cycle `dbGetLink(&prec->inp, ...)`
    /// followed by the dset's own masking (`devBiSoftRaw.c:56-57` `if
    /// (prec->mask) prec->rval &= prec->mask;`, `devMbbiSoftRaw.c:78-79`
    /// unconditionally).
    Read,
}

/// The `special(SPC_*)` dispatch code a field declares — C `special.h`.
///
/// C hands this to the record's `special(DBADDR *, int after)` on every put, and
/// `dbAccess.c` acts on three of them itself before the record ever sees the
/// write: `NoMod` refuses it (`S_db_noMod`), `DbAddr` means the field's type and
/// element count come from the record's `cvt_dbaddr` rather than the `.dbd`, and
/// `As` re-evaluates access security.
///
/// [`Self::code`] is C's `SPC_*` number (`special.h:26-39` @R7.0.10), the
/// integer C prints and stores. It is a table, not a cast, because the
/// numbering is not contiguous — C has no code 4, and the record-specific half
/// starts at 100 — and because a `.dbd` may also declare a code by number
/// (`special(111)`, which `dbLexRoutines.c:631` reads with `%hd`), which
/// [`Self::Record`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Special {
    /// No `special()` declared — C leaves `pdbFldDes->special` 0.
    None,
    /// `SPC_NOMOD` — the field must not be modified. Mirrored into
    /// [`FieldDesc::read_only`], which is the bit the put gate reads.
    NoMod,
    /// `SPC_DBADDR` — the record's `cvt_dbaddr` supplies the field's type
    /// and element count. [`FieldDesc::dbf_type`] carries the type C serves at
    /// the selector field's default; a record whose type is state-dependent
    /// (`waveform.VAL` on `FTVL`, `mbbo.VAL` on `SDEF`) overrides it at runtime.
    DbAddr,
    /// `SPC_SCAN` — a scan-related field; C re-registers the scan.
    Scan,
    /// `SPC_ALARMACK` — an alarm acknowledgement. C skips 4.
    AlarmAck,
    /// `SPC_AS` — access security; C re-computes the record's ASG.
    As,
    /// `SPC_ATTRIBUTE` — a pseudo (attribute) field. Set internally; it
    /// is the one code `pamapspcType` omits, so no `.dbd` declares it.
    Attribute,
    /// `SPC_MOD` — the record's own `special()` runs on the put. C's
    /// record-specific range starts here.
    Mod,
    /// `SPC_RESET` — the `RES` field is being modified.
    Reset,
    /// `SPC_LINCONV` — a linear-conversion field changed; C calls the
    /// device support's `special_linconv`.
    LinConv,
    /// `SPC_CALC` — the `CALC` expression changed; C recompiles it.
    Calc,
    /// A record-specific code declared by number, above the ones `special.h`
    /// names — sscan's `special(111)` (`SPC_SC_S`) .. `special(120)`. C
    /// routes it to the record's `special()` as it does `SPC_MOD`.
    Record(i16),
}

impl Special {
    /// The `.dbd` numeric form: a named code when C names it, else
    /// [`Self::Record`]. `None` for a number below `SPC_MOD` that C does not
    /// name, which no `.dbd` declares.
    pub const fn from_code(code: i16) -> Option<Self> {
        Some(match code {
            0 => Self::None,
            1 => Self::NoMod,
            2 => Self::DbAddr,
            3 => Self::Scan,
            5 => Self::AlarmAck,
            6 => Self::As,
            7 => Self::Attribute,
            100 => Self::Mod,
            101 => Self::Reset,
            102 => Self::LinConv,
            103 => Self::Calc,
            104.. => Self::Record(code),
            _ => return None,
        })
    }

    /// C's `pdbFldDes->special` for this code.
    pub const fn code(self) -> i16 {
        match self {
            Self::None => 0,
            Self::NoMod => 1,
            Self::DbAddr => 2,
            Self::Scan => 3,
            Self::AlarmAck => 5,
            Self::As => 6,
            Self::Attribute => 7,
            Self::Mod => 100,
            Self::Reset => 101,
            Self::LinConv => 102,
            Self::Calc => 103,
            Self::Record(code) => code,
        }
    }
}

/// A field's access-security level — C `.dbd` `asl(ASL0|ASL1)`.
///
/// `ASL1` is the `.dbd` default (`dbLexRoutines.c:570`); `asl(ASL0)` lowers a
/// field to the level an operator may write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asl {
    Asl0,
    Asl1,
}

/// The numeric base `dbGetString` renders an integer field in — C's
/// `ctType` (`dbBase.h:63`), set by the `.dbd` `base(DECIMAL|HEX)` item
/// (`dbLexRoutines.c:652-661`).
///
/// It selects the renderer inside `dbGetStringNum` (`dbStaticLib.c:2074-2124`),
/// so it changes what `dbpr`, `dbDumpRecord` and the `.db` writer print and
/// nothing else: `dbgf` reads through `dbConvert`, which has no base. In
/// EPICS base the whole `HEX` population is `mbbi`/`mbbo`'s sixteen `*VL`
/// fields, which C prints as `ZRVL: 0xa` where the port printed `ZRVL: 10`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base {
    /// `CT_DECIMAL` — the `.dbd` default.
    Decimal,
    /// `CT_HEX`.
    Hex,
}

/// A field's index in its record type's generated field table
/// ([`FieldDeclaration::field_list`]) — C's `dbFldDes::offset` as a handle rather than
/// a byte count. `dbd_generated::slots::<rtype>::<FIELD>` names each one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FieldSlot(pub u16);

/// Declares the typed-slot pair: `SlotRef`/`SlotMut` with one variant per
/// Rust storage type, and a `From<&T>`/`From<&mut T>` per variant so the
/// variant always follows the member's type.
macro_rules! typed_slots {
    ($($variant:ident($ty:ty)),* $(,)?) => {
        /// A borrowed field at a [`FieldSlot`] ([`Record::slot_ref`]), keyed by
        /// the Rust type the record stores it as. A `DBF_ENUM` field is a
        /// `U16`; the declaration ([`FieldDesc::dbf_type`]) tells the two apart.
        #[derive(Debug)]
        pub enum SlotRef<'a> {
            $($variant(&'a $ty),)*
        }

        /// The writable twin of [`SlotRef`] ([`Record::slot_mut`]).
        #[derive(Debug)]
        pub enum SlotMut<'a> {
            $($variant(&'a mut $ty),)*
        }

        $(
            impl<'a> From<&'a $ty> for SlotRef<'a> {
                fn from(v: &'a $ty) -> Self {
                    SlotRef::$variant(v)
                }
            }

            impl<'a> From<&'a mut $ty> for SlotMut<'a> {
                fn from(v: &'a mut $ty) -> Self {
                    SlotMut::$variant(v)
                }
            }
        )*
    };
}

typed_slots! {
    F64(f64),
    F32(f32),
    I8(i8),
    U8(u8),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    Str(PvString),
}

impl SlotMut<'_> {
    /// Store `value` raw, as `dbPutString` writes record memory: it must
    /// already be the variant the slot holds ([`SlotRef::to_value`]'s).
    pub(crate) fn store(self, value: EpicsValue) -> CaResult<()> {
        match (self, value) {
            (SlotMut::F64(s), EpicsValue::Double(v)) => *s = v,
            (SlotMut::F32(s), EpicsValue::Float(v)) => *s = v,
            (SlotMut::I8(s), EpicsValue::Char(v)) => *s = v as i8,
            (SlotMut::U8(s), EpicsValue::UChar(v)) => *s = v,
            (SlotMut::I16(s), EpicsValue::Short(v)) => *s = v,
            (SlotMut::U16(s), EpicsValue::UShort(v) | EpicsValue::Enum(v)) => *s = v,
            (SlotMut::I32(s), EpicsValue::Long(v)) => *s = v,
            (SlotMut::U32(s), EpicsValue::ULong(v)) => *s = v,
            (SlotMut::I64(s), EpicsValue::Int64(v)) => *s = v,
            (SlotMut::U64(s), EpicsValue::UInt64(v)) => *s = v,
            (SlotMut::Str(s), EpicsValue::String(v)) => *s = v,
            (slot, value) => {
                return Err(crate::error::CaError::InvalidValue(format!(
                    "{slot:?} cannot hold {value:?}"
                )));
            }
        }
        Ok(())
    }
}

impl SlotRef<'_> {
    /// The field as [`Record::get_field`] answers it; `dbf` is its declared
    /// type, which tells a `DBF_ENUM` apart from a `DBF_USHORT`.
    pub(crate) fn to_value(&self, dbf: DbFieldType) -> EpicsValue {
        match self {
            SlotRef::F64(v) => EpicsValue::Double(**v),
            SlotRef::F32(v) => EpicsValue::Float(**v),
            SlotRef::I8(v) => EpicsValue::Char(**v as u8),
            SlotRef::U8(v) => EpicsValue::UChar(**v),
            SlotRef::I16(v) => EpicsValue::Short(**v),
            SlotRef::U16(v) if dbf == DbFieldType::Enum => EpicsValue::Enum(**v),
            SlotRef::U16(v) => EpicsValue::UShort(**v),
            SlotRef::I32(v) => EpicsValue::Long(**v),
            SlotRef::U32(v) => EpicsValue::ULong(**v),
            SlotRef::I64(v) => EpicsValue::Int64(**v),
            SlotRef::U64(v) => EpicsValue::UInt64(**v),
            SlotRef::Str(s) => EpicsValue::String((*s).clone()),
        }
    }

    /// The field as `dbGet(DBR_DOUBLE)` delivers it ([`Record::get_slot_f64`]).
    pub(crate) fn to_f64(&self) -> Option<f64> {
        Some(match self {
            SlotRef::F64(v) => **v,
            SlotRef::F32(v) => f64::from(**v),
            SlotRef::I8(v) => f64::from(**v),
            SlotRef::U8(v) => f64::from(**v),
            SlotRef::I16(v) => f64::from(**v),
            SlotRef::U16(v) => f64::from(**v),
            SlotRef::I32(v) => f64::from(**v),
            SlotRef::U32(v) => f64::from(**v),
            SlotRef::I64(v) => **v as f64,
            SlotRef::U64(v) => **v as f64,
            // `getStringDouble` (`dbConvert.c:392-414`).
            SlotRef::Str(s) => c_parse::get_string(
                c_parse::NumericField::of(DbFieldType::Double)?,
                &s.as_str_lossy(),
            )
            .ok()?
            .into_double()
            .ok()?,
        })
    }
}

/// Where a `.db` `field()` of a record type's own declared field is stored at
/// load ([`Record::declared_store`]). C's `dbPutString` writes record memory
/// at the field's offset and runs no record code, so a record type answers
/// for every field it declares; none can be "not found".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeclaredStore {
    /// A plain member: stored raw through [`Record::slot_mut`].
    Member,
    /// A field the record stores itself, through [`Record::load_field`].
    Custom,
    /// A field the record keeps no storage for: the framework holds it, in
    /// the common fields it models (`INP`, `OUT`, the alarm limits, `SSCN`)
    /// or else its declared-field store.
    Spill,
}

/// [`Record::slot_ref`], [`Record::slot_mut`] and [`Record::declared_store`]
/// over a record type's generated slot constants:
///
/// ```ignore
/// slot_map!(crate::server::record::dbd_generated::slots::calc;
///     fields: VAL => val, A => vars[0], LA => prev[0];
///     custom: CALC, INPA;
///     spill: VERS);
/// ```
///
/// Each arm matches `slots::<rtype>::<FIELD>`, so a misspelt field does not
/// compile, and the variant comes from the member's type. The three sections
/// must name every field the type declares exactly once, against the
/// generated `COUNT` ([`declared_store!`](crate::declared_store)). Every
/// `slot_mut` arm must store what `put_field(name, v)` stores
/// (`every_slot_store_equals_the_name_store`); `SPC_NOMOD` is the runtime
/// put gate's, not the record's.
///
/// The form without `fields:` maps slots only, for a type not yet migrated:
/// it has no `declared_store`, and a `read_only:` field gets no `slot_mut`.
#[macro_export]
macro_rules! slot_map {
    // No member: the trait's `slot_ref`/`slot_mut` stand.
    ($($seg:ident)::+;
     fields:
     $(; custom: $($cfield:ident),* $(,)?)?
     $(; spill: $($sfield:ident),* $(,)?)?) => {
        fn declared_store(
            &self,
            slot: $crate::server::record::FieldSlot,
        ) -> ::core::option::Option<$crate::server::record::DeclaredStore> {
            $crate::declared_store!(slot; $($seg)::+;
                member: ;
                custom: $($($cfield),*)?;
                spill: $($($sfield),*)?)
        }
    };
    ($($seg:ident)::+;
     fields: $($field:ident => $member:ident $([$i:expr])?),* $(,)?
     $(; custom: $($cfield:ident),* $(,)?)?
     $(; spill: $($sfield:ident),* $(,)?)?) => {
        fn slot_ref(
            &self,
            slot: $crate::server::record::FieldSlot,
        ) -> ::core::option::Option<$crate::server::record::SlotRef<'_>> {
            use $($seg)::+ as __slots;
            match slot {
                $(__slots::$field => Some((&self.$member $([$i])?).into()),)*
                _ => None,
            }
        }

        fn slot_mut(
            &mut self,
            slot: $crate::server::record::FieldSlot,
        ) -> ::core::option::Option<$crate::server::record::SlotMut<'_>> {
            use $($seg)::+ as __slots;
            match slot {
                $(__slots::$field => Some((&mut self.$member $([$i])?).into()),)*
                _ => None,
            }
        }

        fn declared_store(
            &self,
            slot: $crate::server::record::FieldSlot,
        ) -> ::core::option::Option<$crate::server::record::DeclaredStore> {
            $crate::declared_store!(slot; $($seg)::+;
                member: $($field),*;
                custom: $($($cfield),*)?;
                spill: $($($sfield),*)?)
        }
    };
    ($($seg:ident)::+;
     $($field:ident => $member:ident $([$i:expr])?),* $(,)?
     $(; read_only: $($rfield:ident => $rmember:ident $([$ri:expr])?),* $(,)?)?) => {
        fn slot_ref(
            &self,
            slot: $crate::server::record::FieldSlot,
        ) -> ::core::option::Option<$crate::server::record::SlotRef<'_>> {
            use $($seg)::+ as __slots;
            match slot {
                $(__slots::$field => Some((&self.$member $([$i])?).into()),)*
                $($(__slots::$rfield => Some((&self.$rmember $([$ri])?).into()),)*)?
                _ => None,
            }
        }

        fn slot_mut(
            &mut self,
            slot: $crate::server::record::FieldSlot,
        ) -> ::core::option::Option<$crate::server::record::SlotMut<'_>> {
            use $($seg)::+ as __slots;
            match slot {
                $(__slots::$field => Some((&mut self.$member $([$i])?).into()),)*
                _ => None,
            }
        }
    };
}

/// Whether `slots` names each of a type's `N` fields exactly once: the check
/// [`declared_store!`](crate::declared_store) makes at compile time.
#[doc(hidden)]
pub const fn covers_every_slot<const N: usize>(slots: &[FieldSlot]) -> bool {
    if slots.len() != N {
        return false;
    }
    let mut seen = [false; N];
    let mut i = 0;
    while i < slots.len() {
        let at = slots[i].0 as usize;
        if at >= N || seen[at] {
            return false;
        }
        seen[at] = true;
        i += 1;
    }
    true
}

/// [`Record::declared_store`]'s answer for `slot` of one record type, as an
/// expression: the classification [`slot_map!`](crate::slot_map) writes, for a
/// type that serves several record types and answers per kind. The sections
/// must name every field of `slots::<rtype>` exactly once, or it does not
/// compile:
///
/// ```
/// use epics_base_rs::server::record::{DeclaredStore, FieldSlot};
/// fn store(slot: FieldSlot) -> Option<DeclaredStore> {
///     epics_base_rs::declared_store!(slot;
///         epics_base_rs::server::record::dbd_generated::slots::state;
///         member: ; custom: VAL; spill: OVAL)
/// }
/// assert_eq!(store(FieldSlot(1)), Some(DeclaredStore::Spill));
/// ```
///
/// ```compile_fail
/// use epics_base_rs::server::record::{DeclaredStore, FieldSlot};
/// fn store(slot: FieldSlot) -> Option<DeclaredStore> {
///     epics_base_rs::declared_store!(slot;
///         epics_base_rs::server::record::dbd_generated::slots::state;
///         member: ; custom: VAL; spill: VAL)
/// }
/// ```
///
/// ```compile_fail
/// use epics_base_rs::server::record::{DeclaredStore, FieldSlot};
/// fn store(slot: FieldSlot) -> Option<DeclaredStore> {
///     epics_base_rs::declared_store!(slot;
///         epics_base_rs::server::record::dbd_generated::slots::state;
///         member: ; custom: VAL; spill: )
/// }
/// ```
#[macro_export]
macro_rules! declared_store {
    ($slot:expr; $($seg:ident)::+;
     member: $($field:ident),* $(,)?;
     custom: $($cfield:ident),* $(,)?;
     spill: $($sfield:ident),* $(,)?) => {{
        use $($seg)::+ as __slots;
        use $crate::server::record::DeclaredStore as __S;
        const _: () = assert!(
            $crate::server::record::covers_every_slot::<{ __slots::COUNT }>(&[
                $(__slots::$field,)* $(__slots::$cfield,)* $(__slots::$sfield,)*
            ]),
            "the sections do not name every declared field exactly once"
        );
        Some(match $slot {
            $(__slots::$field => __S::Member,)*
            $(__slots::$cfield => __S::Custom,)*
            $(__slots::$sfield => __S::Spill,)*
            _ => return None,
        })
    }};
}

/// The `.dbd` declaration of a single record field.
///
/// Every one of these is **generated** from the vendored EPICS `.dbd` by
/// `tools/dbd-codegen` — see [`dbd_generated`](super::dbd_generated). They used
/// to be hand-copied, which is what made a wrong `dbf_type` or a missed
/// `special(SPC_NOMOD)` a recurring finding rather than an impossible state.
///
/// The struct carries the *whole* declaration, not just the three attributes the
/// runtime consumes today: dropping the rest at the parser is how the port ended
/// up unable to answer questions like "is this field `pp(TRUE)`?" without
/// re-reading the `.dbd`.
#[derive(Debug, Clone)]
pub struct FieldDesc {
    /// The field name, upper-case as declared.
    pub name: &'static str,
    /// The type the field is **SERVED** as, on every delivery path — see
    /// [`RecordInstance::project_to_declared_type`](super::RecordInstance::project_to_declared_type),
    /// which projects the stored value onto it. The one exception is a
    /// [`Self::runtime_typed`] field, where C's `cvt_dbaddr` overrides.
    ///
    /// It is NOT the `DBF_*` token the `.dbd` declared — that is
    /// [`Self::declared_dbf`], and the two differ for every link and menu
    /// field. The CA wire type is derived from this one by
    /// [`DbFieldType::ca_wire_type`], which owns the promotions CA has no type
    /// for (`ULong`/`Int64`/`UInt64` -> `DBR_DOUBLE`, `UShort` -> `DBR_LONG`,
    /// `UChar` -> `DBR_CHAR`). Do not pre-promote here: PVA serves the native
    /// width.
    pub dbf_type: DbFieldType,
    /// The `DBF_*` token the `.dbd` declared, carried verbatim from the
    /// generator's input — C's `pdbFldDes->field_type`, which is what
    /// `dbDumpField` prints and `dbGetFieldType`/`dbGetFieldTypeString`
    /// (`dbStaticLib.c:964-977`) answer for a `DBENTRY`.
    ///
    /// C's `dba` prints `paddr->field_type` instead, which is this token for
    /// every field EXCEPT the 83 `special(SPC_DBADDR)` rows, where
    /// `dbNameToAddr` lets the record's `cvt_dbaddr` overwrite it; on those
    /// rows [`Self::dbf_type`] carries the overwritten type. 80 of the 83
    /// declare `DBF_NOACCESS` here, which is precisely why [`Self::no_access`]
    /// reads this field and not `dbf_type`.
    ///
    /// A SECOND fact, not a spelling of [`Self::dbf_type`]. The generator maps
    /// six C tokens onto two served types (`DBF_ENUM|DBF_MENU|DBF_DEVICE` all
    /// serve as [`DbFieldType::Enum`], `DBF_INLINK|DBF_OUTLINK|DBF_FWDLINK` all
    /// as [`DbFieldType::String`]), and that collapse is one-way: no mapping
    /// out of `dbf_type` can recover the token, because `ai.INP` and
    /// `dbCommon.FLNK` are the same served variant. Nor can the field NAME —
    /// `LNK1` is `DBF_OUTLINK` on `sseq` and `DBF_FWDLINK` on `fanout`. So the
    /// declaration is carried, not derived.
    pub declared_dbf: DbfCode,
    /// C's `cvt_dbaddr` re-types this field at name-resolution time from the
    /// record's own state — `waveform.VAL` from `FTVL`, `aSub.A` from `FTA`,
    /// `mbbo.VAL` from `SDEF` — so the `.dbd` declaration is a placeholder
    /// carrying only the selector's default. [`Self::dbf_type`] is therefore
    /// NOT what such a field is served as; the record's stored variant is this
    /// port's `cvt_dbaddr` answer, and it wins.
    ///
    /// A `special(SPC_DBADDR)` field whose type is nevertheless FIXED
    /// (`compress.VAL` is always a double array, `histogram.VAL` always
    /// `epicsUInt32`) is *not* runtime-typed: its row in `cvt_dbaddr.types`
    /// carries no selector, so the declared type is the true one and it is
    /// projected like any other field.
    pub runtime_typed: bool,
    /// A `DBF_STRING` whose `field_size` is the record's `SIZV` (`lsi`/`lso`
    /// VAL+OVAL, `printf` VAL): the `size` column of `cvt_dbaddr.types`. It is
    /// pvxs's long string (`field_size > MAX_STRING_SIZE+1`,
    /// `ioc/channel.cpp:58-67`); the record stores its `LEN` elements as a
    /// `CharArray`, and every server asks this, not the record.
    pub long_string: bool,
    /// `special(SPC_NOMOD)` — the field is immutable for this record type. The
    /// static half of the no-modify declaration; see [`Record::field_no_mod`]
    /// for the half a record decides at runtime.
    pub read_only: bool,
    /// The full `special()` code AS RESOLVED, of which [`Self::read_only`] is
    /// one case: the `.dbd` declaration for most fields, but for a
    /// `special(SPC_DBADDR)` field the code its `cvt_dbaddr` raises instead —
    /// `SPC_MOD` on `lsi.VAL`, `SPC_NOMOD` on `lsi.OVAL`
    /// (`lsiRecord.c:127-134`). That is the code the put gate and the CA
    /// write-access answer must read, because it is what C's `DBADDR` carries
    /// after name resolution.
    ///
    /// It is NOT what the `.dbd` said — see [`Self::declared_special`], which
    /// is. The pair splits the same way [`Self::dbf_type`] and
    /// [`Self::declared_dbf`] do, and for the same reason: one cell holding
    /// both answers is a cell that is wrong for one of its two readers.
    pub special: Special,
    /// The `special()` code the `.dbd` DECLARED — C's `pdbFldDes->special`,
    /// which `cvt_dbaddr` never touches because it writes the per-resolution
    /// `DBADDR` and not the shared field descriptor.
    ///
    /// Every reader that asks about the DECLARATION rather than about a
    /// resolved address wants this one. The db loader's field-name suggestion
    /// is the case in hand: it skips `SPC_NOMOD` and `SPC_DBADDR` candidates
    /// (`dbLexRoutines.c:1283-1285`), and reading [`Self::special`] there would
    /// let `lsi.VAL` and `lso.VAL` — declared `SPC_DBADDR`, resolved `SPC_MOD`
    /// — be proposed where C does not propose them.
    pub declared_special: Special,
    /// `pp(TRUE)` — a put to this field processes the record.
    pub pp: bool,
    /// `asl(...)` — the access-security level.
    pub asl: Asl,
    /// The byte width C's `pdbFldDes->size` carries.
    ///
    /// Two sources, one meaning. A `DBF_STRING` field takes it from `size(N)`,
    /// which C's loader REQUIRES on every such row
    /// (`dbLexRoutines.c:755-758`). A `DBF_NOACCESS` internal takes it from
    /// the width of the C struct member [`Self::extra`] declares, which is
    /// what a C IOC's generated `<rec>RecordSizeOffset` writes into the same
    /// cell — the `.dbd` never spells it.
    ///
    /// Not string-only, and not decoration on the second kind: `dbpr`'s
    /// `DBF_NOACCESS` arm dispatches on it (`dbTest.c:1235`, `:1241`) and then
    /// prints exactly this many bytes as hex (`:1249-1262`), so a width of 0
    /// prints an empty row where C prints the field.
    ///
    /// 0 for every other declaration, where C carries `sizeof` the member and
    /// no reader in this port asks.
    ///
    /// This is `dbFldDes.size` only. The served element's width, which is what
    /// a put caps at and `dba` prints, is [`Self::element_size`].
    pub size: u16,
    /// `extra("...")` — the C struct member a `DBF_NOACCESS` field stands for,
    /// verbatim, and `None` for a field declaring none.
    ///
    /// C's loader requires it on exactly the rows [`Self::size`]'s second
    /// source covers (`dbLexRoutines.c:759-762`), and it is not documentation:
    /// `dbpr` reads the declaration TEXT to choose a renderer — a `*` in it
    /// means print a pointer, a leading `ELLLIST` means print a list header
    /// (`dbTest.c:1235-1247`). The type name is the only thing that says how
    /// wide the member is, since no `.dbd` states it.
    pub extra: Option<&'static str>,
    /// `menu(...)` choice strings, in index order, for a `DBF_MENU` field. The
    /// index is the stored value and the strings are what `get_enum_strs` serves,
    /// so a client sees `"NO CONVERSION"` rather than `0`.
    pub menu: Option<&'static [&'static str]>,
    /// The `menu(...)` name itself, C's `pdbMenu->name` — set for `menuScan`
    /// too, whose [`Self::menu`] is `None`. Menus that list the same choices
    /// share one table, so the table cannot name its menu.
    pub menu_name: Option<&'static str>,
    /// `initial("...")` — the value C's dbd loader seeds the field with.
    pub initial: Option<&'static str>,
    /// `interest(N)` — the `dbpr` verbosity level at which C prints the field.
    pub interest: u8,
    /// `prop(YES)` — the field is a property: a change to it posts a
    /// `DBE_PROPERTY` event.
    pub prop: bool,
    /// `prompt("...")` — the operator-facing label. C prints it in the
    /// parenthesised half of the db loader's field suggestion
    /// (`dbLexRoutines.c:1380-1384`), which is the only reason it is carried;
    /// a field that declares none makes C print the bare `Did you mean` line.
    pub prompt: Option<&'static str>,
    /// `promptgroup("...")` — the DCT group the field belongs to, or `None`
    /// when the `.dbd` declares none. C stores it as a 1-based key into
    /// `guiGroupList` (`findOrAddGuiGroup`, `dbLexRoutines.c:1176-1189`), so
    /// zero means absent and this is `Option`, not the group's index. The
    /// suggestion halves a groupless field's score (`:1288-1289`): a field no
    /// DCT screen offers is an unlikely thing to have misspelled.
    pub promptgroup: Option<&'static str>,
    /// `base(DECIMAL|HEX)` — the base `db_get_string` renders this field in.
    pub base: Base,
}

impl FieldDesc {
    /// A hand-written descriptor carrying only the three attributes the port
    /// used to model.
    ///
    /// **Transitional.** Every record type is migrating to the generated table
    /// in [`dbd_generated`](super::dbd_generated), which carries the whole `.dbd`
    /// declaration; this constructor exists only so the not-yet-migrated records
    /// keep compiling, and it goes away with the last of them. It does NOT know
    /// the field's `pp`/`asl`/`size`/`menu`/`initial`, so it reports the neutral
    /// value for each — a record still on this constructor answers "no menu"
    /// here and resolves its choices through the
    /// [`Record::menu_field_choices`] fallback instead.
    pub const fn new(name: &'static str, dbf_type: DbFieldType, read_only: bool) -> Self {
        Self {
            name,
            dbf_type,
            // A hand-written table names one type and means both facts by it:
            // it has no `.dbd` behind it to disagree with, so the declaration
            // is the served type's own code. That also makes `no_access()`
            // false for every such row, which is right — a hand-written table
            // never declares a C internal.
            declared_dbf: dbf_type.dbf_code(),
            // A plain field: the type it names is the type it is served as.
            // The `cvt_dbaddr` records are all on the generated table, which
            // sets this from `cvt_dbaddr.types`.
            runtime_typed: false,
            long_string: false,
            read_only,
            special: if read_only {
                Special::NoMod
            } else {
                Special::None
            },
            // A hand-written row has no `.dbd` behind it to disagree with, so
            // the declaration is the resolved code.
            declared_special: if read_only {
                Special::NoMod
            } else {
                Special::None
            },
            pp: false,
            asl: Asl::Asl1,
            size: 0,
            extra: None,
            menu: None,
            menu_name: None,
            initial: None,
            interest: 0,
            prop: false,
            prompt: None,
            promptgroup: None,
            base: Base::Decimal,
        }
    }

    /// The field is declared `DBF_NOACCESS` — a C internal with no dbStatic
    /// representation. C's `dbPutString` switches on exactly this and answers
    /// `S_dbLib_badField` with "Can't set array field before iocInit()"
    /// (`dbStaticLib.c:2646-2650`), so a `.db` file cannot assign one.
    ///
    /// Derived rather than stored, so the two cannot disagree. It is NOT
    /// implied by [`Self::special`] — `mbbo.VAL` is `DBF_ENUM` + `SPC_DBADDR`
    /// and is perfectly settable, while `waveform.VAL` is `DBF_NOACCESS` +
    /// `SPC_DBADDR` and is not — nor by [`Self::dbf_type`], which for these
    /// rows carries the type C SERVES rather than the one declared.
    pub const fn no_access(&self) -> bool {
        matches!(self.declared_dbf, DbfCode::NoAccess)
    }

    /// The field resolves as a NAME but has no readable value — C `dbGet`'s
    /// validity gate, `field_type > DBF_DEVICE` (`dbAccess.c:667-675`
    /// @R7.0.10).
    ///
    /// Not the same question as [`Self::no_access`], and the difference is
    /// the whole point. `dbEntryToAddr` seeds `paddr->field_type` from the
    /// DECLARATION and then overwrites it from the record's `cvt_dbaddr` —
    /// but only for a field whose `special` is `SPC_DBADDR`
    /// (`dbAccess.c:640-647`). So a `DBF_NOACCESS` row carrying
    /// `SPC_DBADDR` — `waveform.VAL` — arrives at the gate re-typed to what
    /// it serves and PASSES, while one that does not — `dbCommon`'s `BKPT`
    /// and `TIME` — arrives still holding 17 and FAILS. Both facts are
    /// needed; neither alone answers it.
    ///
    /// Derived rather than stored, for the same reason `no_access` is: a
    /// stored copy is a second answer that can drift from the declaration.
    pub const fn unreadable(&self) -> bool {
        self.no_access() && !matches!(self.declared_special, Special::DbAddr)
    }

    /// The width of one element as C's `paddr->field_size` holds it, for a
    /// field served as `served`.
    ///
    /// [`Self::size`] is `sizeof(prec->member)` on every row — a string's
    /// `size(N)`, a pointer's 8 for an aSub `A`..`U`. `dbNameToAddr` copies it
    /// into `field_size`, except that `cvt_dbaddr` of a `SPC_DBADDR` field
    /// overwrites it with `dbValueSize(field_type)` (`dbAccess.c:640-647`):
    /// the width of the element it serves, 40 for a string. A put's string
    /// cap and `dba`'s `Field Size` read this, never `size` directly.
    pub fn element_size(&self, served: DbfCode) -> u16 {
        if !matches!(self.declared_special, Special::DbAddr) {
            return self.size;
        }
        match served {
            DbfCode::String => 40,
            DbfCode::Char | DbfCode::UChar => 1,
            DbfCode::Short | DbfCode::UShort | DbfCode::Enum | DbfCode::Menu | DbfCode::Device => 2,
            DbfCode::Long | DbfCode::ULong | DbfCode::Float => 4,
            DbfCode::Double | DbfCode::Int64 | DbfCode::UInt64 => 8,
            _ => self.size,
        }
    }
}

/// One `recGblInitConstantLink(&prec->LINK, DBF_x, &prec->TARGET)` call from a
/// record's C `init_record` — the seed of a CONSTANT input link.
///
/// Declared by [`Record::constant_init_links`] and applied by the single owner
/// `crate::server::database::PvDatabase::rec_gbl_init_constant_links`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConstantInitLink {
    /// The link field holding the constant (`INPA`, `NVL`, `SELL`, `DOL1`,
    /// `SUBL`, `DOL`, ...).
    pub link_field: &'static str,
    /// The value field the constant is loaded into (`A`, `SELN`, `DO1`,
    /// `SNAM`, `VAL`, ...).
    pub target_field: &'static str,
    /// Whether a successful seed clears UDF — C's
    /// `if (recGblInitConstantLink(&prec->dol, ...)) prec->udf = FALSE;`
    /// (`aoRecord.c:112-113`, `longoutRecord.c:113`, `mbboRecord.c:133`,
    /// `int64outRecord.c:110`, `dfanoutRecord.c:105`). The multi-input seeders
    /// (calc/sub/sel/aSub/seq/fanout) do NOT clear UDF: they seed A..L, not
    /// VAL.
    pub clears_udf: bool,
    /// Whether the loaded value is stored as its BOOLEAN — C `boRecord.c:146-148`
    /// loads the constant into a temporary and stores `prec->val = !!ival`, so
    /// `field(DOL,"5")` leaves a bo at VAL=1, not 5. The only seed whose stored
    /// value differs from the loaded one.
    pub normalize_bool: bool,
    /// `Some((dbr, nRequest))` for a `dbLoadLinkArray(plink, dbr, pbuffer,
    /// &n)` seed instead of `recGblInitConstantLink` — aSub's INPA..U
    /// (`aSubRecord.c:128-137`), which load into the FTx buffer by NOx.
    pub array: Option<(crate::types::DbFieldType, usize)>,
}

impl ConstantInitLink {
    /// A seed that does not touch UDF — the INPA..L / SELL / NVL / DOLn form.
    pub const fn new(link_field: &'static str, target_field: &'static str) -> Self {
        Self {
            link_field,
            target_field,
            clears_udf: false,
            normalize_bool: false,
            array: None,
        }
    }

    /// A DOL→VAL seed, which C follows with `prec->udf = FALSE`.
    pub const fn dol_to_val(link_field: &'static str, target_field: &'static str) -> Self {
        Self {
            link_field,
            target_field,
            clears_udf: true,
            normalize_bool: false,
            array: None,
        }
    }

    /// aSub's INPx→x seed: `dbLoadLinkArray` of `nreq` `dbr` elements.
    pub const fn load_array(
        link_field: &'static str,
        target_field: &'static str,
        dbr: crate::types::DbFieldType,
        nreq: usize,
    ) -> Self {
        Self {
            link_field,
            target_field,
            clears_udf: false,
            normalize_bool: false,
            array: Some((dbr, nreq)),
        }
    }

    /// bo's DOL→VAL seed: `prec->val = !!ival; prec->udf = FALSE;`
    /// (`boRecord.c:146-149`).
    pub const fn dol_to_bool_val(link_field: &'static str, target_field: &'static str) -> Self {
        Self {
            link_field,
            target_field,
            clears_udf: true,
            normalize_bool: true,
            array: None,
        }
    }
}

/// The seed table for a record whose C seeds exactly the input links it
/// fetches — the `for (i = 0; i < N; i++) recGblInitConstantLink(plink++,
/// DBF_DOUBLE, pvalue++)` loop of calc / calcout / sub / sel / aSub /
/// scalcout / acalcout / transform, expressed over the record's own
/// [`Record::multi_input_links`] table.
pub fn seed_input_links(pairs: &[(&'static str, &'static str)]) -> Vec<ConstantInitLink> {
    pairs
        .iter()
        .map(|(link, value)| ConstantInitLink::new(link, value))
        .collect()
}

/// Resolved metadata of an OUT-link TARGET, as C's soft device support
/// obtains it before choosing its write buffer.
///
/// C's two sources, both mirrored by
/// [`PvDatabase::resolve_out_target`](crate::server::database::PvDatabase):
/// - `DB_LINK` — `dbNameToAddr` gives `field_type` and `no_elements`
///   (`devsCalcoutSoft.c:127-131`, `devaCalcoutSoft.c:78-79`); an
///   unresolvable name leaves the caller's initializers untouched.
/// - `CA_LINK` — `dbCaGetLinkDBFtype` / `dbCaGetNelements`
///   (`dbCa.c:633-675`), which both return `-1` on a disconnected link and
///   likewise leave the initializers untouched.
///
/// A record reproduces its C device support's buffer switch on this in
/// [`Record::multi_output_buffer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutTarget {
    /// Target field's DBF type. `None` = unresolved (disconnected CA link,
    /// or a name this IOC cannot resolve) — C's `field_type` initializer.
    pub field_type: Option<DbFieldType>,
    /// Target field's element capacity — C `no_elements` / `dbCaGetNelements`.
    /// `1` when unresolved, matching C's `n_elements = 1` initializer.
    pub element_count: i64,
    /// True when C would classify this link as a `CA_LINK`: an explicit
    /// `ca://`/`pva://` link, or a DB-style name that is not a record of
    /// this IOC (`dbInitLink` locality). Device support that splits its
    /// buffer choice on sync-vs-async (`devsCalcoutSoft.c:76`, gated on
    /// `plink->type == CA_LINK && pscalcout->wait`) reads this.
    pub is_ca_link: bool,
    /// True when the target field is one of the seven DBF classes C's soft
    /// device support puts as `DBR_STRING` — `DBF_STRING`, `DBF_ENUM`,
    /// `DBF_MENU`, `DBF_DEVICE`, `DBF_INLINK`, `DBF_OUTLINK`, `DBF_FWDLINK`
    /// (`devsCalcoutSoft.c:83-85`, `:128-130`).
    ///
    /// Carried here rather than re-derived from [`Self::field_type`] because
    /// [`DbFieldType`] is the DBR *wire* type: it has no `Menu` or `Device`
    /// variant, so a menu target (`PRIO`, `STAT`, `SEVR`, `DISS`, `ACKT`, …)
    /// or `DTYP` is indistinguishable from a plain numeric/string field by
    /// type alone. The classification is made once, at resolution, by the
    /// side that holds the target's field metadata
    /// (`RecordInstance::field_puts_as_string`); a record's
    /// [`Record::multi_output_buffer`] just reads the answer.
    pub puts_as_string: bool,
    /// C `plink->type == CONSTANT`, the test `tableRecord.c:2321` makes —
    /// not `dbLinkIsConstant`, which a removed link also answers: its type
    /// stays `PV_LINK` (`dbDbLink.c:132`) while it has no target.
    pub constant_type: bool,
}

impl OutTarget {
    /// C's initializer state: `field_type = 0` is never *used* as a type by
    /// the port (a `None` type routes to the device support's `default:`
    /// arm), and `n_elements = 1`. An unresolved target is not in the string
    /// class — C's `field_type = 0` matches no `case` and falls to `default:`.
    pub const UNRESOLVED: Self = Self {
        field_type: None,
        element_count: 1,
        is_ca_link: false,
        puts_as_string: false,
        constant_type: true,
    };
}

/// The `dbrType` a record asks an INPUT link for — the second argument of C
/// `dbGetLink(plink, dbrType, pbuffer, options, pnRequest)` (`dbLink.c:305`).
///
/// The READ twin of [`Record::typed_output_buffer`]'s destination switch. C's
/// input-side switch is on the SOURCE's DBF class (`dbGetLinkDBFtype(&dol)`,
/// `sseqRecord.c:640-705`) and each arm asks `dbGetLink` for a DIFFERENT
/// `dbrType`, so the value a record receives is not the source's native one:
/// a `DBF_ENUM`/`DBF_MENU` source read with `DBR_STRING` delivers its state
/// LABEL, and a `DBF_CHAR` array read with `DBF_CHAR` delivers bytes, not a
/// number. The record declares the request
/// ([`Record::input_link_request`]); the framework, which is the side that
/// can address the source, performs the conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkReadAs {
    /// The source's NATIVE value, coerced (or preserved) by the target field's
    /// own `put_field_internal`. The framework default: every record whose C
    /// `dbGetLink` request does not switch on the source class (compress `INP`,
    /// waveform `INP`, sseq `SELL`, epid, motor, table) reads this way.
    Native,
    /// C `dbGetLink(..., DBR_STRING, ...)`. An `ENUM`/`MENU` source delivers its
    /// state LABEL (`dbConvert.c` `getEnumString` → the record's
    /// `get_enum_str`), never the index; a link/`DTYP` field delivers its text.
    String,
    /// C `dbGetLink(..., DBR_CHAR, ...)`.
    Char,
    /// C `dbGetLink(..., DBR_UCHAR, ...)`.
    UChar,
    /// C `dbGetLink(..., DBR_SHORT, ...)`.
    Short,
    /// C `dbGetLink(..., DBR_FLOAT, ...)`.
    Float,
    /// C `dbGetLink(..., DBR_DOUBLE, ...)`.
    Double,
    /// C `dbGetLink(..., DBR_LONG, ...)`.
    Long,
    /// C `dbGetLink(..., DBR_ULONG, ...)`.
    ULong,
    /// C `dbGetLink(..., DBR_USHORT, ...)`.
    UShort,
    /// C `dbGetLink(..., DBR_INT64, ...)`.
    Int64,
    /// C `dbGetLink(..., DBR_UINT64, ...)`.
    UInt64,
    /// C `dbGetLink(..., DBR_ENUM, ...)` into the reader's buffer. Numeric
    /// sources convert as [`Self::UShort`] does. A one-element `DBF_STRING`
    /// source goes to the SOURCE record's `put_enum_str` (`cvt_st_e`), which on
    /// a name match stores the source's VAL and leaves the buffer as it was; a
    /// longer one parses element 0 (`getStringUshort`). Where C writes nothing
    /// the read delivers an empty `EnumArray`, so the buffer stays the
    /// reader's and this request can be cached in the plan.
    Enum,
    /// C `dbGetLink(..., DBF_CHAR|DBF_UCHAR, buf, 0, &n)` — up to `max_elements`
    /// bytes of the source's char array, taken as the string they spell
    /// (`sseqRecord.c:682-686`: `n_elements` clamped to the record's 40-byte
    /// `s` buffer, then `strcmp`/`atof` read it as a C string).
    CharArrayAsString { max_elements: usize },
    /// C `dbGetLink(..., dbr, buf, 0, &n)` with `nRequest = n_request > 1`
    /// (`aSubRecord.c:283-284`): an array source delivers its first
    /// `n_request` elements converted to `dbr`, a scalar source its one value
    /// converted at `dbr` (no `f64` step), so the request settles without
    /// the source's element count.
    Array {
        dbr: crate::types::DbFieldType,
        n_request: usize,
    },
}

impl LinkReadAs {
    /// Does this request render a `DBR_STRING` at the source, so the read
    /// owes the conversion the source's
    /// [`SourceRender`](super::record_instance::SourceRender)?
    #[inline(always)]
    pub(crate) fn renders_at_source(self) -> bool {
        matches!(
            self,
            Self::String
                | Self::Array {
                    dbr: crate::types::DbFieldType::String,
                    ..
                }
        )
    }

    /// The scalar `dbrType` this request names; `None` for the native read and
    /// the char-array one, which name none.
    pub(crate) fn dbr_type(self) -> Option<crate::types::DbFieldType> {
        use crate::types::DbFieldType as T;
        Some(match self {
            Self::String => T::String,
            Self::Char => T::Char,
            Self::UChar => T::UChar,
            Self::Short => T::Short,
            Self::Float => T::Float,
            Self::Double => T::Double,
            Self::Long => T::Long,
            Self::ULong => T::ULong,
            Self::UShort => T::UShort,
            Self::Int64 => T::Int64,
            Self::UInt64 => T::UInt64,
            Self::Enum { .. } => T::Enum,
            Self::Native | Self::CharArrayAsString { .. } | Self::Array { .. } => return None,
        })
    }
}

/// The scalar requests a soft dset's INP read can be taken at without an
/// [`EpicsValue`], each the [`LinkReadAs`] of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SoftRead {
    Double,
    Long,
    UShort,
    ULong,
    Int64,
}

impl SoftRead {
    pub(crate) fn of(read_as: LinkReadAs) -> Option<Self> {
        match read_as {
            LinkReadAs::Double => Some(Self::Double),
            LinkReadAs::Long => Some(Self::Long),
            LinkReadAs::UShort => Some(Self::UShort),
            LinkReadAs::ULong => Some(Self::ULong),
            LinkReadAs::Int64 => Some(Self::Int64),
            _ => None,
        }
    }

    pub(crate) fn read_as(self) -> LinkReadAs {
        match self {
            Self::Double => LinkReadAs::Double,
            Self::Long => LinkReadAs::Long,
            Self::UShort => LinkReadAs::UShort,
            Self::ULong => LinkReadAs::ULong,
            Self::Int64 => LinkReadAs::Int64,
        }
    }

    /// The reading of a `DBF_DOUBLE` source: C's integer cast, the row
    /// [`EpicsValue::get_convert`] takes for a non-integer source.
    pub(crate) fn reading_of_f64(self, v: f64) -> SoftReading {
        match self {
            Self::Double => SoftReading::Double(v),
            Self::Long => SoftReading::Long(crate::types::c_cast::f64_to_i32(v)),
            Self::UShort => SoftReading::UShort(crate::types::c_cast::f64_to_u16(v)),
            Self::ULong => SoftReading::ULong(crate::types::c_cast::f64_to_u32(v)),
            Self::Int64 => SoftReading::Int64(crate::types::c_cast::f64_to_i64(v)),
        }
    }

    /// The reading of a value already converted at [`Self::read_as`]; `None`
    /// for a value of any other shape.
    pub(crate) fn reading_of(self, value: &EpicsValue) -> Option<SoftReading> {
        match (self, value) {
            (Self::Double, EpicsValue::Double(v)) => Some(SoftReading::Double(*v)),
            (Self::Long, EpicsValue::Long(v)) => Some(SoftReading::Long(*v)),
            (Self::UShort, EpicsValue::UShort(v)) => Some(SoftReading::UShort(*v)),
            (Self::ULong, EpicsValue::ULong(v)) => Some(SoftReading::ULong(*v)),
            (Self::Int64, EpicsValue::Int64(v)) => Some(SoftReading::Int64(*v)),
            _ => None,
        }
    }
}

/// A soft dset's INP read at one of the [`SoftRead`] requests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SoftReading {
    /// `DBR_DOUBLE`.
    Double(f64),
    /// `DBR_LONG`.
    Long(i32),
    /// `DBR_USHORT`.
    UShort(u16),
    /// `DBR_ULONG`.
    ULong(u32),
    /// `DBR_INT64`.
    Int64(i64),
}

impl SoftReading {
    /// The same reading as the link layer delivers it to
    /// [`Record::soft_input_read`].
    pub(crate) fn into_value(self) -> EpicsValue {
        match self {
            Self::Double(v) => EpicsValue::Double(v),
            Self::Long(v) => EpicsValue::Long(v),
            Self::UShort(v) => EpicsValue::UShort(v),
            Self::ULong(v) => EpicsValue::ULong(v),
            Self::Int64(v) => EpicsValue::Int64(v),
        }
    }

    /// The record's soft dset entry for this request.
    #[inline(always)]
    pub(crate) fn apply(
        self,
        record: &mut dyn Record,
        dpvt: &mut crate::server::device_support::Dpvt,
    ) -> CaResult<()> {
        match self {
            Self::Double(v) => record.soft_input_read_f64(v, dpvt),
            Self::Long(v) => record.soft_input_read_i32(v, dpvt),
            Self::UShort(v) => record.soft_input_read_u16(v, dpvt),
            Self::ULong(v) => record.soft_input_read_u32(v, dpvt),
            Self::Int64(v) => record.soft_input_read_i64(v, dpvt),
        }
    }
}

/// A record's per-link `dbGetLink` request, asked BEFORE the link's source is
/// resolved.
///
/// Resolving the source ([`PvDatabase::resolve_out_target`](crate::server::database::PvDatabase))
/// costs a records-map lookup and the TARGET record's read lock, and it has to
/// happen with no reader lock held, so it cannot be deferred into
/// [`Record::input_link_read_as_from_source`]. Only four record types let the
/// source decide (`sseq` `DOLn`, `aSub`'s STRING channels, `lsi` `INP`/`SIOL`,
/// `lso` `DOL`) — every other type's C switch is on the LINK FIELD alone. This
/// enum is what lets those four ask for the walk and everyone else skip it,
/// measured at 0.10 us per link per cycle.
///
/// [`Self::FromSource`] is the only way to reach
/// [`Record::input_link_read_as_from_source`], so the two answers cannot
/// disagree: a record that never asks for the source is never handed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputLinkRequest {
    /// The request, settled by the link field alone.
    As(LinkReadAs),
    /// C's `default: break` — this record does not read that link at all.
    NotRead,
    /// The request depends on the source's DBF class / element count: resolve
    /// the target and ask [`Record::input_link_read_as_from_source`].
    FromSource,
}

impl InputLinkRequest {
    /// [`LinkReadAs::renders_at_source`] for a request that may still be
    /// the source's to settle.
    #[inline(always)]
    pub(crate) fn renders_at_source(self) -> bool {
        match self {
            Self::As(read_as) => read_as.renders_at_source(),
            Self::NotRead => false,
            Self::FromSource => true,
        }
    }

    /// INP's request under the record's DTYP, the one answer both INP reads of
    /// a cycle ask: a soft DTYP's dset decides it
    /// ([`Record::soft_input_request`]), any other leaves it to the record.
    #[inline]
    pub(crate) fn for_inp(
        record: &dyn Record,
        soft: Option<crate::server::device_support::SoftDtyp>,
    ) -> Self {
        match soft {
            Some(soft) => record.soft_input_request(soft),
            None => record.input_link_request("INP"),
        }
    }
}

/// How C gates a secondary field named by
/// [`Record::fields_posted_with_value_mask`] *inside* the guard that decides
/// whether VAL posts at all.
///
/// Both variants share the outer guard (the field posts only on a cycle where
/// VAL's own monitor mask is live, and carries that same mask); they differ in
/// whether C re-tests the secondary field's own value once inside it. Folding
/// the two into one rule is what over- or under-posts the field: gating
/// `timestamp`'s RVAL on its own change silences it (see [`Self::WithValue`]),
/// and NOT gating `ai`'s RVAL on its own change posts a raw count that never
/// moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValuePostGate {
    /// C re-tests the field's own previous value inside the guard, and posts
    /// only if it moved: `ai` `RVAL` — `if (prec->oraw != prec->rval) {
    /// db_post_events(&prec->rval, monitor_mask); prec->oraw = prec->rval; }`
    /// (aiRecord.c:460-465).
    OnChange,
    /// C posts the field whenever the guard fires, with no test of its own
    /// value: `timestamp` `RVAL` — `if (strncmp(oval, val, ...)) {
    /// db_post_events(&val[0], mask); db_post_events(&rval, mask); }`
    /// (timestampRecord.c:158-162). The VAL-string change is the *only* gate,
    /// so a cycle that re-renders the same seconds count still re-posts RVAL.
    WithValue,
    /// [`Self::OnChange`]'s own-value test, but the guard is the record's
    /// widened one ([`Record::take_secondary_value_changes`]) and the event
    /// carries C's forced `monitor_mask | DBE_VALUE | DBE_LOG`: ao `RVAL` /
    /// `RBV` — `if(prec->oraw != prec->rval) { db_post_events(&prec->rval,
    /// monitor_mask|DBE_VALUE|DBE_LOG); prec->oraw = prec->rval; }`
    /// (aoRecord.c:539-548), all inside `if(monitor_mask)` (`:536`).
    ///
    /// Distinct from [`Self::OnChange`] in BOTH halves, which is why it is a
    /// third variant and not a flag: ai's RVAL rides VAL's own mask and VAL's
    /// own guard, while ao's rides a forced mask and a guard `omod` can open
    /// when VAL's is shut.
    ///
    /// The own-value test AND the advance of the record's "old" copy are one
    /// step, [`Record::take_secondary_value_changes`], never split across the
    /// code that computed the new value — C assigns `oraw` INSIDE the post it
    /// guards, so a cycle that changes RVAL without posting it must leave
    /// `oraw` stale for the next cycle to find.
    OnChangeForced,
    /// No own-value test: C posts the field whenever the widened guard fires,
    /// with that guard as its mask: ao `OVAL` — `if(prec->omod) monitor_mask
    /// |= (DBE_VALUE|DBE_LOG); if(monitor_mask) { prec->omod = FALSE;
    /// db_post_events(&prec->oval, monitor_mask);` (aoRecord.c:535-538).
    Guarded,
}

/// What one cycle's secondary block decided
/// ([`Record::take_secondary_value_changes`]): bit `i` fired
/// [`Record::fields_posted_with_value_mask`]`[i]`, and the top bit says the
/// record widened VAL's mask into the block's guard. C's only widening is
/// `monitor_mask |= (DBE_VALUE|DBE_LOG)` (ao `omod`, aoRecord.c:535), so a
/// flag carries it, and the whole answer returns in one register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct SecondaryTake(u64);

impl SecondaryTake {
    const WIDENED: u64 = 1 << 63;

    /// Nothing fired, nothing widened.
    pub const NONE: Self = Self(0);

    /// `fired` (below the top bit), with the guard widened or not.
    /// A widened guard always fires something (ao's OVAL is `Guarded`), so
    /// [`Self::is_none`] tests the whole word.
    pub const fn new(fired: u64, widened: bool) -> Self {
        debug_assert!(fired & Self::WIDENED == 0 && (fired != 0 || !widened));
        Self(fired | if widened { Self::WIDENED } else { 0 })
    }

    /// Nothing fired.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// A block outside C's `if (monitor_mask)` (bo `RVAL`/`RBV`): what fired
    /// opens its own guard, with the `DBE_VALUE | DBE_LOG` it posts with.
    pub const fn unguarded(fired: u64) -> Self {
        Self::new(fired, fired != 0)
    }

    /// The fired bits.
    pub const fn fired(self) -> u64 {
        self.0 & !Self::WIDENED
    }

    /// What the record ORs into VAL's mask to form the block's guard.
    pub fn widen(self) -> crate::server::recgbl::EventMask {
        if self.0 & Self::WIDENED != 0 {
            crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::LOG
        } else {
            crate::server::recgbl::EventMask::NONE
        }
    }

    /// C's `if (prec->oraw != prec->rval) { ...; prec->oraw = prec->rval; }`
    /// minus the post: whether `new` moved off `old`, advancing `old`.
    pub fn advance<T: PartialEq + Copy>(old: &mut T, new: T) -> bool {
        let moved = *old != new;
        if moved {
            *old = new;
        }
        moved
    }
}

/// The event mask ONE per-cycle mark posts with
/// ([`Record::take_cycle_posted_fields`]).
///
/// A record can mark the same field from two different C `db_post_events` call
/// sites in one cycle, and the two need not agree on the mask. aCalcout does
/// exactly that with its arrays: `afterCalc` posts the AMASK-flagged ones with a
/// LITERAL `DBE_VALUE|DBE_LOG` (`aCalcoutRecord.c:296`) while `monitor()` posts
/// the NEWM-flagged ones with `monitor_mask|DBE_VALUE|DBE_LOG` (`:1034`). An
/// array in BOTH masks gets BOTH events — the record marks it twice, and the
/// variant carried with each mark is what keeps them distinguishable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CyclePostMask {
    /// A literal `DBE_VALUE` — no LOG bit, no alarm bits. C's shape for a
    /// field the record re-DERIVED from the one it was given: sseq re-renders
    /// `STRn` after a `DOn` write and posts it with a bare `DBE_VALUE`
    /// (`sseqRecord.c:679`, `:1115`), while the view actually written carries
    /// `DBE_VALUE|DBE_LOG`.
    Value,
    /// A literal `DBE_VALUE | DBE_LOG` — the alarm-transition bits are NOT
    /// folded in, because this C call site does not have `monitor_mask` in
    /// scope (aCalcout `afterCalc`, `aCalcoutRecord.c:296`).
    ValueLog,
    /// `monitor_mask | DBE_VALUE | DBE_LOG` — C's usual `monitor()` shape
    /// (aCalcout `monitor()`, `aCalcoutRecord.c:1034`).
    MonitorValueLog,
    /// `monitor_mask | DBE_VALUE` — an array whose own MDEL crossing fired and
    /// whose ADEL did not (aCalcout AVAL/OAV, `aCalcoutRecord.c:996-1000`).
    /// `monitor_mask` is VAL's, its deadband classes included.
    MonitorValue,
    /// `monitor_mask | DBE_LOG` — the ADEL-only counterpart.
    MonitorLog,
    /// A `monitor()` that `special()` runs (compress SPC_RESET,
    /// `compressRecord.c:101-111`): the put's drain commits the alarm
    /// (`recGblResetAlarms`) before the first such post and ORs its
    /// `DBE_ALARM` into `DBE_VALUE | DBE_LOG`. A cycle has already committed
    /// its alarm, so there it is [`Self::MonitorValueLog`].
    ResetValueLog,
    /// [`Self::ResetValueLog`], posted only when that commit moved the alarm:
    /// the `alarm_mask ||` half of compress's NUSE condition (`:105`), marked
    /// when the record's own half did not fire.
    ResetValueLogOnAlarm,
    /// The record's own `recGblResetAlarms`, committed as for
    /// [`Self::ResetValueLog`], whose `DBE_ALARM` alone is posted when that
    /// commit moved the alarm: throttle `valuePut` on a SENT that did not
    /// move (`throttleRecord.c:607-614`). In a cycle whose alarm is already
    /// committed it is that commit's `DBE_ALARM`.
    ResetAlarm,
}

/// One take's marks: a bit per entry of the record type's static post table,
/// so a take under the record hold allocates nothing. Iterates in table
/// order, which is the order two marks of one field are posted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CycleMarks {
    table: PostTable,
    bits: u128,
}

impl CycleMarks {
    pub const NONE: Self = Self {
        table: &[],
        bits: 0,
    };

    /// Bit `i` of `bits` marks `table[i]`.
    pub fn of(table: PostTable, bits: u128) -> Self {
        debug_assert!(
            table.len() >= u128::BITS as usize || bits >> table.len() == 0,
            "a mark past the end of its post table"
        );
        Self { table, bits }
    }

    pub fn is_empty(self) -> bool {
        self.bits == 0
    }

    /// The marks ahead of the first marked `Reset*` entry, and the rest.
    pub fn split_at_reset(self) -> (Self, Self) {
        let mut bits = self.bits;
        while bits != 0 {
            let i = bits.trailing_zeros();
            if matches!(
                self.table[i as usize].1,
                CyclePostMask::ResetValueLog
                    | CyclePostMask::ResetValueLogOnAlarm
                    | CyclePostMask::ResetAlarm
            ) {
                let head = self.bits & ((1 << i) - 1);
                return (
                    Self { bits: head, ..self },
                    Self {
                        bits: self.bits & !head,
                        ..self
                    },
                );
            }
            bits &= bits - 1;
        }
        (self, Self::NONE)
    }

    /// The post table the bits index.
    pub(crate) fn table(self) -> PostTable {
        self.table
    }

    /// The masks of the marks among `of`, in table order.
    pub(crate) fn masks_among(self, of: u128) -> impl Iterator<Item = CyclePostMask> {
        Self {
            bits: self.bits & of,
            ..self
        }
        .map(|(_, mask)| mask)
    }

    /// Whether a mark is among `of`.
    pub(crate) fn any_among(self, of: u128) -> bool {
        self.bits & of != 0
    }
}

impl Iterator for CycleMarks {
    type Item = (FieldIx, CyclePostMask);

    fn next(&mut self) -> Option<Self::Item> {
        if self.bits == 0 {
            return None;
        }
        let i = self.bits.trailing_zeros() as usize;
        self.bits &= self.bits - 1;
        Some(self.table[i])
    }
}

/// A record's set of its own declared fields by [`FieldIx`], resolved when
/// the type is compiled: membership is an index compare, never a name read.
pub type DeclaredFields = &'static [FieldIx];

/// A [`CycleMarks`] post table: each entry a declared field and its mask.
pub type PostTable = &'static [(FieldIx, CyclePostMask)];

const fn same_name(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// `name`'s [`FieldIx`] in a type whose own table is `own`, found as
/// `field_desc_in` finds it: the own table, then `dbCommon`. An undeclared
/// name fails the build.
///
/// An index, not a `&FieldDesc`: a reference built here points at a copy of
/// the table, never at the table a record serves.
pub const fn declared_field_ix(own: &'static [FieldDesc], name: &str) -> FieldIx {
    let common: &'static [FieldDesc] = super::dbd_generated::DB_COMMON_FIELDS;
    let mut i = 0;
    while i < own.len() {
        if same_name(own[i].name, name) {
            return FieldIx((common.len() + i) as u16);
        }
        i += 1;
    }
    let mut i = 0;
    while i < common.len() {
        if same_name(common[i].name, name) {
            return FieldIx(i as u16);
        }
        i += 1;
    }
    panic!("a declared-field set names a field its table does not declare")
}

/// The declaration at `ix` in a type whose own table is `own`.
pub fn field_at(own: &'static [FieldDesc], ix: FieldIx) -> Option<&'static FieldDesc> {
    let common = super::dbd_generated::DB_COMMON_FIELDS;
    let i = usize::from(ix.0);
    common
        .get(i)
        .or_else(|| own.get(i.checked_sub(common.len())?))
}

/// `names` resolved against `own` at compile time, for a [`DeclaredFields`].
pub const fn declared_set<const N: usize>(
    own: &'static [FieldDesc],
    names: [&str; N],
) -> [FieldIx; N] {
    let mut out = [FieldIx::UNDECLARED; N];
    let mut i = 0;
    while i < N {
        out[i] = declared_field_ix(own, names[i]);
        i += 1;
    }
    out
}

/// A [`PostTable`] from `entries`, resolved against `own` at compile time.
pub const fn declared_post_table<const N: usize>(
    own: &'static [FieldDesc],
    entries: [(&str, CyclePostMask); N],
) -> [(FieldIx, CyclePostMask); N] {
    assert!(N <= u128::BITS as usize);
    let mut out = [(FieldIx::UNDECLARED, CyclePostMask::Value); N];
    let mut i = 0;
    while i < N {
        out[i] = (declared_field_ix(own, entries[i].0), entries[i].1);
        i += 1;
    }
    out
}

/// A [`PostTable`] posting each of `names` with `mask`.
pub const fn uniform_post_table<const N: usize>(
    own: &'static [FieldDesc],
    names: &[&str],
    mask: CyclePostMask,
) -> [(FieldIx, CyclePostMask); N] {
    assert!(names.len() == N && N <= u128::BITS as usize);
    let mut table = [(FieldIx::UNDECLARED, mask); N];
    let mut i = 0;
    while i < N {
        table[i] = (declared_field_ix(own, names[i]), mask);
        i += 1;
    }
    table
}

/// [`Record::set_input_link_slots`] answered off the record's own link texts,
/// in [`Record::multi_input_links`] order.
///
/// Every slot is lent a text, so the "unknown" half of the pair is empty by
/// construction: a type that can answer this way knows all of its own links.
pub fn input_link_slots_of<S: AsRef<[u8]>>(texts: &[S]) -> Option<(u64, u64)> {
    debug_assert!(texts.len() <= u64::BITS as usize);
    let mut set = 0u64;
    for (slot, text) in texts.iter().enumerate() {
        if !text.as_ref().is_empty() {
            set |= 1 << slot;
        }
    }
    Some((set, 0))
}

/// The event mask a change-detected AUXILIARY field posts with — the single
/// owner of that decision, built once per cycle from the record's declarations
/// and shared by every monitor loop (both `process_record_*` paths, the
/// deferred-completion path, and `RecordInstance::process_local`), so they
/// cannot drift apart on what mask a field carries.
///
/// C's usual shape for the "post every input/aux field that changed" loop is
/// `monitor_mask | DBE_VALUE | DBE_LOG` (calcRecord.c:420, subRecord.c:400,
/// motor `DBE_VAL_LOG`) — that is the default. Three record-declared exceptions
/// narrow it, and no two are the same narrowing:
///
/// * [`Record::value_only_change_fields`] — a literal `DBE_VALUE`
///   (tableRecord.c:659, scaler `Sn`): alarm bits + `DBE_VALUE`, never `LOG`.
/// * [`Record::fields_posted_with_monitor_mask`] — `monitor_mask | DBE_VALUE`
///   (swaitRecord.c:650): VAL's own monitor mask, so `DBE_LOG` rides along
///   exactly when VAL's ADEL deadband crossed.
/// * [`Record::fields_posted_without_alarm_bits`] — a literal
///   `DBE_VALUE | DBE_LOG` (epidRecord.c:376): both value classes, alarm bits
///   discarded.
#[derive(Clone, Copy)]
pub(crate) struct AuxPostMask {
    value_only: &'static [&'static str],
    monitor_masked: &'static [&'static str],
    no_alarm_bits: &'static [&'static str],
}

impl AuxPostMask {
    /// Read the record's three declarations once, outside the per-field loop.
    pub(crate) fn of(record: &dyn Record) -> Self {
        Self {
            value_only: record.value_only_change_fields(),
            monitor_masked: record.fields_posted_with_monitor_mask(),
            no_alarm_bits: record.fields_posted_without_alarm_bits(),
        }
    }

    /// The three declarations, in class order: a field's class is 1 + the
    /// index of the first that holds it, 0 for none.
    pub(crate) fn lists(&self) -> [&'static [&'static str]; 3] {
        [self.value_only, self.monitor_masked, self.no_alarm_bits]
    }

    /// The mask a change-detected post of a field of `class` carries.
    /// `alarm_bits` is this cycle's `recGblResetAlarms` result; `deadband_mask`
    /// is VAL's own monitor mask (those alarm bits, plus `DBE_VALUE` when MDEL
    /// crossed and `DBE_LOG` when ADEL crossed).
    pub(crate) fn mask_of_class(
        class: u8,
        alarm_bits: crate::server::recgbl::EventMask,
        deadband_mask: crate::server::recgbl::EventMask,
    ) -> crate::server::recgbl::EventMask {
        use crate::server::recgbl::EventMask;
        match class {
            1 => alarm_bits | EventMask::VALUE,
            2 => deadband_mask | EventMask::VALUE,
            3 => EventMask::VALUE | EventMask::LOG,
            _ => alarm_bits | EventMask::VALUE | EventMask::LOG,
        }
    }
}

/// The per-type monitor-plan bits [`Record::monitor_value_classes`] reads.
#[derive(Debug, Clone, Copy)]
pub struct MonitorValueGate {
    /// [`Record::uses_monitor_deadband`].
    pub uses_deadband: bool,
    /// [`Record::has_array_monitor_post`].
    pub array_monitor_post: bool,
}

/// [`Record::monitor_value_classes`]' answer: the classes the deadband
/// field's post carries, and whether an array record's `HASH` moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorValueClasses {
    /// `DBE_VALUE`.
    pub value: bool,
    /// `DBE_LOG`.
    pub archive: bool,
    /// See [`ArrayMonitorPost::hash_changed`].
    pub hash_changed: bool,
    /// The record marked fields for [`Record::take_cycle_posted_fields`] in
    /// this call, so this cycle takes them whatever its static hooks say.
    pub marked: bool,
}

impl MonitorValueClasses {
    /// A type with no change gate and no deadband: binary records post always.
    pub const ALWAYS: Self = Self {
        value: true,
        archive: true,
        hash_changed: false,
        marked: false,
    };
}

/// The deadband half of [`Record::monitor_value_classes`]: the array rule
/// where the type has one, else MDEL/ADEL against MLST/ALST.
#[inline]
pub fn monitor_deadband_classes<R: Record + ?Sized>(
    r: &mut R,
    array_monitor_post: bool,
    last_posted: &mut [f64; 2],
) -> MonitorValueClasses {
    if array_monitor_post && let Some(post) = r.array_monitor_post() {
        return MonitorValueClasses {
            value: post.post_value,
            archive: post.post_archive,
            hash_changed: post.hash_changed,
            marked: r.cycle_marked_fields(),
        };
    }
    // A value the deadband cannot compare posts unconditionally.
    let Some(cells) = r.monitor_deadband_cells(*last_posted) else {
        return MonitorValueClasses {
            marked: r.cycle_marked_fields(),
            ..MonitorValueClasses::ALWAYS
        };
    };
    let (value, archive) = cells.crossed();
    if value || archive {
        return commit_last_posted(r, cells.val(), value, archive, last_posted);
    }
    MonitorValueClasses {
        value,
        archive,
        hash_changed: false,
        marked: r.cycle_marked_fields(),
    }
}

/// C `checkAlarms`' analog ladder — see [`Record::check_analog_alarms`].
#[inline]
pub(crate) fn analog_alarm_ladder<R: Record + ?Sized>(r: &mut R, common: &mut CommonFields) {
    use crate::server::recgbl;

    // Borrowed, not copied: the slot is disjoint from the record the
    // filter below writes, and its last use is before the alarm commit.
    let Some(cfg) = &common.analog_alarm else {
        return;
    };
    // VAL goes down in the variant the record stores it in, not
    // flattened to `f64`: it is what picks the ladder's comparison
    // domain, and `Int64(v) as f64` had already rounded the value
    // before the first comparison ran.
    let Some(input) = r.analog_alarm_input() else {
        return;
    };

    // C `checkAlarms` returns immediately on a UDF cycle: it raises
    // `UDF_ALARM`/`UDFS` (already done by `rec_gbl_check_udf` in
    // `evaluate_alarms`), zeroes `AFVL` on the AFTC-capable records, and
    // returns BEFORE the range check — so `LALM` is left untouched and
    // `AFVL` is not filtered this cycle. The identical guard appears in
    // every record that shares this arm (`aiRecord.c:319-323`,
    // `aoRecord.c:383-386`, `longinRecord.c:274-278`,
    // `longoutRecord.c:317-320`, `int64inRecord.c:267-271`,
    // `int64outRecord.c:298-301`, `calcRecord.c:300-304`,
    // `calcoutRecord.c:563-566`). AFTC-capable records (ai/longin/
    // int64in/calc) carry `AFVL` and zero it (`prec->afvl = 0`); the
    // out records (ao/longout/int64out/calcout) have no `AFVL` and just
    // return. Running the range check here would drift `LALM` to `val`
    // (NaN on an undefined cycle) and filter `AFVL` — both observable.
    let AnalogAlarmInput {
        val,
        hyst,
        lalm,
        aftc,
        afvl,
    } = input;
    if common.udf != 0 {
        if afvl != 0.0 {
            r.store_alarm_filter_value(0.0);
        }
        return;
    }

    // One rule for every ladder input: a record that DECLARES the field
    // owns it, because `Record::put_field` absorbs the client's put before
    // `put_common_field` ever runs, and only an undeclared field falls
    // through to `CommonFields`. MDEL/ADEL/MLST/ALST in
    // `check_monitor_deadbands` already read this way; HYST did not, so
    // `int64in`/`int64out`'s `pub hyst` swallowed every put while the
    // hysteresis compared against a permanent 0.0 — with `caget .HYST`
    // reading the value back, which is what made it silent.
    //
    // `common.hyst` stays an `f64` and stays exact: after the limits moved
    // to the declared type its only remaining readers are the
    // `DBF_DOUBLE` records and longin/longout, and every `epicsInt32` is
    // an `f64` exactly.

    // C-style per-level hysteresis: alarm fires if val passes the level,
    // OR if we were already at that alarm level (lalm == alev) and val
    // hasn't retreated past the hysteresis margin.
    //
    // `alarm_range` is the C-style integer level: 1=Lolo, 2=Low,
    // 3=Normal, 4=High, 5=Hihi. Required for the calc-record AFTC
    // filter (`calcRecord.c::checkAlarms:339-381`) which filters
    // on the range level (not on severity) and re-maps back.
    // C's `checkAlarms` enables each level with a NONZERO test on the raw
    // severity ordinal (`if (prec->hhsv && …)`) and passes that raw ordinal
    // to `recGblSetSevr`; `recGblResetAlarms` then clamps the resulting
    // *severity* to `INVALID_ALARM` while the *status* keeps the level. So an
    // out-of-range selector (`HHSV = 4`) still fires HIHI and lands
    // SEVR=INVALID/STAT=HIHI — reproduced by testing `!= 0` and mapping the
    // ordinal through [`AlarmSeverity::from_u16`] (which clamps `>= 3` to
    // `Invalid`).
    let mut alarm_range = cfg.range(val, hyst, lalm, common.hyst);

    // C parity: the alarm-range AFTC low-pass filter
    // (`{ai,longin,int64in,calc}Record.c::checkAlarms`) smooths the
    // integer `alarmRange` and re-maps. Only records that carry the
    // AFTC/AFVL fields run it; the rest answer AFTC = AFVL = 0 and so
    // take neither arm's store.
    if aftc > 0.0 {
        let (filtered_range, new_afvl) = crate::server::records::alarm_filter::aftc_filter(
            alarm_range,
            aftc,
            afvl,
            common.time_last.get(),
            common.time,
        );
        r.store_alarm_filter_value(new_afvl);
        // C re-maps through the SAME `switch (alarmRange)` the ladder
        // fell out of, so the filter changes only the range and
        // `resolve` below answers for both.
        alarm_range = filtered_range;
    } else {
        // aftc <= 0 disables the filter. C `checkAlarms`
        // (e.g. aiRecord.c:356,401) initialises the local
        // `afvl = 0` and unconditionally stores `prec->afvl =
        // afvl` at the end, so a disabled filter drives AFVL to
        // 0. Mirror that here so a stale accumulator from a prior
        // `aftc > 0` run cannot mis-seed the filter if AFTC is
        // re-enabled later.
        if afvl != 0.0 {
            r.store_alarm_filter_value(0.0);
        }
    }
    // C re-maps the filtered range through the same table the ladder used.
    let (new_sevr, new_stat, alev) = cfg.resolve(alarm_range);

    if new_sevr != AlarmSeverity::NoAlarm {
        // C `aiRecord.c:405-406` — the latch is armed to the THRESHOLD, and
        // only when `recGblSetSevr` returns TRUE. A level that fires while a
        // higher-or-equal severity is already pending (an MS input link, a
        // SIMM alarm, a device INVALID) raises nothing, so C leaves LALM
        // where it was; arming it there would let the next cycle's
        // `lalm == alev && val >= alev - hyst` clause hold an alarm C has
        // already cleared.
        if recgbl::rec_gbl_set_sevr(common, new_stat, new_sevr) {
            r.store_analog_lalm(alev.unwrap_or(val));
        }
    } else {
        // No alarm condition: reset LALM to current value. C `aiRecord.c:409`
        // does this unconditionally — only the alarm arm is gated.
        r.store_analog_lalm(val);
    }
}

/// C `monitor()`'s `prec->mlst = prec->val` / `prec->alst = prec->val`,
/// out of line so the no-crossing cycle runs without a frame. It builds the
/// whole answer, so the caller tail-calls it and holds nothing across it.
#[inline(never)]
fn commit_last_posted<R: Record + ?Sized>(
    r: &mut R,
    val: AlarmLimit,
    value: bool,
    archive: bool,
    last_posted: &mut [f64; 2],
) -> MonitorValueClasses {
    r.store_monitor_last_posted(val, value, archive);
    if archive {
        last_posted[1] = val.as_f64();
    }
    if value {
        last_posted[0] = val.as_f64();
    }
    MonitorValueClasses {
        value,
        archive,
        hash_changed: false,
        marked: r.cycle_marked_fields(),
    }
}

/// Outcome of a record's array-style monitor decision, returned by
/// [`Record::array_monitor_post`] (C waveform/aai/aao `monitor()`,
/// waveformRecord.c:291-326).
#[derive(Debug, Clone, Copy)]
pub struct ArrayMonitorPost {
    /// Include `DBE_VALUE` on the VAL post this cycle (MPST = Always, or
    /// MPST = On Change with a changed hash).
    pub post_value: bool,
    /// Include `DBE_LOG` on the VAL post this cycle (APST = Always, or
    /// APST = On Change with a changed hash).
    pub post_archive: bool,
    /// The content hash changed this cycle (On Change mode) — the owner
    /// posts `HASH` with a literal `DBE_VALUE`.
    pub hash_changed: bool,
}

/// The record type's RSET metadata slots — which of C's six nullable
/// `get_*` property functions the record type implements.
///
/// This is the port's `rset` property table, transcribed slot by slot
/// from the `#define get_xxx NULL` lines of each C record's `.c`. It is
/// what `dbGet` consults to *narrow* the caller's `options` mask
/// (`dbAccess.c:336-427`), and therefore what decides whether QSRV marks
/// an NT leaf at all (pvxs `ioc/iocsource.cpp:263-305`). Without it the
/// port fabricated every leaf it could name and marked it as supplied —
/// telling the client a made-up `display.precision = 0` on a `longout`,
/// or `valueAlarm` bands at zero on a `waveform`, were authoritative.
///
/// A slot counts as supplied when the C function pointer is non-NULL,
/// even if the function writes nothing for the field in question — C
/// leaves the option bit set either way (e.g. `boRecord.c:294-299`,
/// whose `get_units` writes `"s"` only for `HIGH`, yet `DBR_UNITS`
/// survives for every `bo` field).
/// What a record type's C `get_control_double` writes for a field its switch
/// does not list — the slot's LAST arm.
///
/// Independent of [`crate::server::snapshot::PropertySupport::control_double`],
/// which says whether the slot EXISTS at all (a NULL slot makes
/// `dbAccess.c:257` fail and clears the option bit, so no leaf is served).
/// This says what a slot that DOES exist answers when it falls through. The
/// two genuinely differ: `acalcout` supplies the slot and still writes nothing
/// for an unlisted field.
///
/// Slot-neutral: the two arm SHAPES are the same for `get_control_double` and
/// `get_graphic_double`, but which one a record type takes is asked per slot —
/// [`control_default_arm`] and [`graphic_default_arm`] are separate answers.
/// `aSub` is the type that proves they must be: its `get_control_double` is a
/// bare `recGblGetControlDouble` (`aSubRecord.c:372-376`) while its
/// `get_graphic_double` (`:350-368`) has no recGbl call at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RsetDefaultArm {
    /// The slot ends in `recGblGetControlDouble` / `recGblGetGraphicDouble` —
    /// the field TYPE's numeric range (`recGbl.c:146-171`, table at
    /// `:372-419`).
    RecGblRange,
    /// The slot returns without writing, so the `dbAccess.c:256` / `:216`
    /// `(0.0, 0.0)` seed stands.
    Seed,
}

/// The last arm of `rtype`'s C `get_control_double`.
///
/// Audited by reading each ported record type's own C rset. Every record in
/// EPICS base delegates (`aiRecord.c:267`, `aoRecord.c:341`, `calcRecord.c:235`,
/// `calcoutRecord.c:506`, `aSubRecord.c:372`, `subRecord.c:272`,
/// `selRecord.c:203`, `seqRecord.c:342`, `dfanoutRecord.c:197`,
/// `longinRecord.c:217`, `longoutRecord.c:268`, `int64inRecord.c:212`,
/// `int64outRecord.c:251`, `boRecord.c:310`, `waveformRecord.c:268`,
/// `aaiRecord.c:293`, `aaoRecord.c:296`, `subArrayRecord.c:262`,
/// `compressRecord.c:487`, `histogramRecord.c:458`), as do the downstream
/// types that supply the slot (`motorRecord.cc:3303`, `epidRecord.c:285`,
/// `tableRecord.c:806`). The rest NULL it outright and never reach here
/// (`biRecord.c`, `mbbiRecord.c`, `mbboRecord.c`, `mbbiDirectRecord.c`,
/// `mbboDirectRecord.c`, `stringinRecord.c`, `stringoutRecord.c`,
/// `lsiRecord.c`, `lsoRecord.c`, `eventRecord.c`, `fanoutRecord.c`,
/// `permissiveRecord.c`, `printfRecord.c`, `stateRecord.c`,
/// `sseqRecord.c:141`, `swaitRecord.c`, `transformRecord.c`, `busyRecord.c`,
/// `asynRecord.c`, `throttleRecord.c:70`, `scalerRecord.c:157`).
///
/// Only the synApps calc pair writes nothing: both end `get_control_double`
/// with a bare `return(0)` after their listed cases, so C serves the seed
/// where a delegating record serves the type range. Measured on the
/// differential oracle as 42 `acalcout`/`scalcout` fields.
pub fn control_default_arm(rtype: &str) -> RsetDefaultArm {
    match rtype {
        // aCalcoutRecord.c:793-822, sCalcoutRecord.c:653-682 — the switch lists
        // VAL/HIHI/HIGH/LOW/LOLO and the A-L / PA-PL ranges, then falls off
        // the end into `return(0)` with no recGbl delegation.
        "acalcout" | "scalcout" => RsetDefaultArm::Seed,
        _ => RsetDefaultArm::RecGblRange,
    }
}

/// The last arm of `rtype`'s C `get_graphic_double` — the twin of
/// [`control_default_arm`], and NOT the same answer for every type.
///
/// Read from each ported type's own rset. `aSubRecord.c:350-368` is the one
/// that separates the two slots: it tries `get_inlinkNumber` then
/// `get_outlinkNumber` and, for a field that is neither, falls out of the
/// function having written nothing — no `default:`, no recGbl call — so the
/// `dbAccess.c:216` seed stands. Its `get_control_double` (`:372-376`) is a
/// bare `recGblGetControlDouble` in the same file, which is why one shared bit
/// could not answer both. Measured: `ASUB.PHAS` serves display 0/0 where
/// `CALC.PHAS` serves the DBF_SHORT range ±32767.
///
/// The synApps calc pair ends the same way — the listed cases return early and
/// the function ends `return(0)` with no delegation
/// (`aCalcoutRecord.c:762-791`, `sCalcoutRecord.c:622-651`).
///
/// Every other ported type that supplies the slot delegates
/// (`aiRecord.c:244`, `aoRecord.c:316`, `calcRecord.c:187`,
/// `calcoutRecord.c:452`, `subRecord.c:222`, `selRecord.c:181`,
/// `seqRecord.c:322`, `dfanoutRecord.c:181`, `longinRecord.c:190`,
/// `int64inRecord.c:196`, `int64outRecord.c:235`, `longoutRecord.c:252`,
/// `waveformRecord.c:251`, `aaiRecord.c:276`, `aaoRecord.c:279`,
/// `subArrayRecord.c:231`, `compressRecord.c:471`, `histogramRecord.c:442`).
pub fn graphic_default_arm(rtype: &str) -> RsetDefaultArm {
    match rtype {
        "aSub" | "acalcout" | "scalcout" => RsetDefaultArm::Seed,
        _ => RsetDefaultArm::RecGblRange,
    }
}

/// How `rtype`'s C `get_alarm_double` answers the fields it lists explicitly
/// (see [`alarm_explicit_fields`]).
///
/// The severity gate is NOT universal, so this bit cannot be inferred from the
/// base analog shape — it is read from each ported type's own rset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmValArm {
    /// `pad->upper_alarm_limit = prec->hhsv ? prec->hihi : epicsNAN` — each
    /// limit is served only when its severity is enabled. The base analog
    /// shape (`aiRecord.c:290-301`, and ao/longin/longout/calc/calcout/sel/
    /// sub/dfanout alike).
    Gated,
    /// `pad->upper_alarm_limit = prec->hihi` — the four limits verbatim, with
    /// no severity test at all, so an unset record serves 0 rather than NaN
    /// (`int64inRecord.c:235-246`, `int64outRecord.c:279-290`,
    /// `sCalcoutRecord.c:683-696`, `aCalcoutRecord.c:823-836`,
    /// `motorRecord.cc:3344-3361`, `epidRecord.c:289-301`).
    Unconditional,
}

/// The fields `rtype`'s C `get_alarm_double` lists BEFORE its
/// `recGblGetAlarmDouble` fall-through — the ones that take
/// [`alarm_val_arm`]'s answer instead of the four NaN.
///
/// Empty means the rset lists nothing: even VAL falls to the default arm.
/// `seqRecord.c:355-367` routes only its `DOn` fields (through their `DOLn`
/// link), `aSubRecord.c:378-404` only its `INPn`/`OUTn` links, and
/// `swaitRecord.c:608-612` is a bare `recGblGetAlarmDouble(paddr,pad)` with no
/// field test whatsoever. A constant link supplies no alarm limits, so the link
/// arm lands on the same four NaN — which is why only the listed set needs a
/// per-type answer and the link fields do not.
///
/// `motorRecord.cc:3344-3361` is the one type listing a second field: its case
/// is `fieldIndex == motorRecordVAL || fieldIndex == motorRecordDVAL`, so the
/// dial-coordinate readback carries the same limits as VAL.
pub fn alarm_explicit_fields(rtype: &str) -> &'static [&'static str] {
    match rtype {
        "seq" | "aSub" | "swait" => &[],
        "motor" => &["VAL", "DVAL"],
        _ => &["VAL"],
    }
}

/// See [`AlarmValArm`]. Types whose rset lists nothing
/// ([`alarm_explicit_fields`] empty) never consult this.
pub fn alarm_val_arm(rtype: &str) -> AlarmValArm {
    match rtype {
        "int64in" | "int64out" | "scalcout" | "acalcout" | "motor" | "epid" => {
            AlarmValArm::Unconditional
        }
        _ => AlarmValArm::Gated,
    }
}

/// The record fields `rtype`'s C `get_graphic_double` reads for the fields its
/// rset lists — the source of the record-level display limits.
///
/// `HOPR`/`LOPR` in every ported type but one: `motorRecord.cc:3221-3225`
/// answers the soft travel limits `HLM`/`LLM` and never reads its own
/// HOPR/LOPR.
///
/// The `"motor"` arm decides nothing today and is kept deliberately.
/// `MotorRecord::field_metadata_override` (`motor-rs/src/record/mod.rs:201-205`)
/// answers `Some(..)` for EVERY field, and the override layer runs after
/// `route_field_metadata` (pinned by
/// `epics-base-rs/tests/the_override_layer_runs_last.rs`), so every served
/// motor field takes its window from `motor-rs` and never from this table.
/// The arm states what C's rset does for the day a motor field reaches the
/// routing layer without an override.
pub fn graphic_limit_fields(rtype: &str) -> (&'static str, &'static str) {
    match rtype {
        "motor" => ("HLM", "LLM"),
        _ => ("HOPR", "LOPR"),
    }
}

/// Which of the record's own fields `rtype`'s C `get_control_double` answers
/// with for the fields its rset lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlLimitSource {
    /// `DRVH`/`DRVL` unconditionally (`aoRecord.c:356-357`).
    Drive,
    /// `DRVH`/`DRVL` when `DRVH > DRVL`, else `HOPR`/`LOPR`
    /// (`longoutRecord.c:282-287`, `int64outRecord.c:265-270`).
    DriveWhenSet,
    /// The soft travel limits `HLM`/`LLM` (`motorRecord.cc:3272-3276`).
    SoftLimits,
    /// `HOPR`/`LOPR` — every other ported type that supplies the slot.
    Operator,
}

/// See [`ControlLimitSource`]. Read from each ported type's own rset; the
/// three named types are the only ones in base or the ported modules whose
/// control limits are not the operator range.
///
/// The `"motor"` arm is dead for the same reason, and kept for the same
/// reason, as the `"motor"` arm of [`graphic_limit_fields`].
pub fn control_limit_source(rtype: &str) -> ControlLimitSource {
    match rtype {
        "ao" => ControlLimitSource::Drive,
        "longout" | "int64out" => ControlLimitSource::DriveWhenSet,
        "motor" => ControlLimitSource::SoftLimits,
        _ => ControlLimitSource::Operator,
    }
}

pub fn default_property_support(rtype: &str) -> crate::server::snapshot::PropertySupport {
    use crate::server::snapshot::PropertySupport as P;
    match rtype {
        // Every numeric slot, no enum strings.
        // aiRecord.c:68-87, aoRecord.c:67-86, calcRecord.c:63-82,
        // calcoutRecord.c:67-86, selRecord.c:58-77, subRecord.c:62-81,
        // dfanoutRecord.c:66-85, seqRecord.c:56-75.
        "ai" | "ao" | "calc" | "calcout" | "sel" | "sub" | "dfanout" | "seq" => P::NUMERIC,

        // Integer scalars: `#define get_precision NULL`
        // (longinRecord.c, longoutRecord.c, int64inRecord.c,
        // int64outRecord.c). This is the measured `longout` case —
        // pvxs leaves `display.precision` absent, the port sent 0.
        "longin" | "longout" | "int64in" | "int64out" => P {
            precision: false,
            ..P::NUMERIC
        },

        // Arrays and compress: `#define get_alarm_double NULL`
        // (waveformRecord.c, aaiRecord.c, aaoRecord.c,
        // subArrayRecord.c, compressRecord.c, histogramRecord.c).
        // This is the measured `waveform` case — pvxs leaves all four
        // `valueAlarm.*Limit` absent, the port sent four zeros.
        "waveform" | "aai" | "aao" | "subArray" | "compress" | "histogram" => P {
            alarm_double: false,
            ..P::NUMERIC
        },

        // No property slots at all: every `get_*` is `#define`d NULL.
        // stringinRecord.c:62-81, stringoutRecord.c:64-83,
        // lsiRecord.c:287-306, lsoRecord.c:328-347,
        // eventRecord.c:62-81, permissiveRecord.c:56-75,
        // stateRecord.c:58-77, printfRecord.c:456-475,
        // fanoutRecord.c:60-79, timestampRecord (std-rs).
        // This is the measured `stringout` case — pvxs leaves
        // `display.units` absent, the port sent "".
        "stringin" | "stringout" | "lsi" | "lso" | "event" | "permissive" | "state" | "printf"
        | "fanout" | "timestamp" => P::NONE,

        // Enum records. `biRecord.c:61-80` and `mbbiRecord.c:65-84` /
        // `mbboRecord.c:64-83` NULL every numeric slot and supply only
        // `get_enum_strs`. `boRecord.c:54-61` keeps `get_units`,
        // `get_precision` and `get_control_double` (they serve the
        // `HIGH` field) but NULLs `get_graphic_double` and
        // `get_alarm_double`.
        "bi" | "mbbi" | "mbbo" => P {
            enum_strs: true,
            put_enum_str: Some(true),
            ..P::NONE
        },
        "bo" => P {
            units: true,
            precision: true,
            control_double: true,
            enum_strs: true,
            put_enum_str: Some(true),
            ..P::NONE
        },
        // busyRecord.c (synApps busy): units/graphic/control/alarm NULL,
        // get_precision and get_enum_strs present.
        "busy" => P {
            precision: true,
            enum_strs: true,
            put_enum_str: Some(true),
            ..P::NONE
        },
        // mbbiDirectRecord.c:63-81 / mbboDirectRecord.c:63-81 — only
        // `get_precision` survives, and C's DBF_FLOAT/DOUBLE gate
        // (`dbAccess.c:388-395`) drops it again for their DBF_ENUM/LONG
        // value, so nothing is marked. `Snapshot::precision` applies
        // that gate.
        "mbbiDirect" | "mbboDirect" => P {
            precision: true,
            ..P::NONE
        },

        // synApps, transcribed the same way.
        // sCalcoutRecord.c / aCalcoutRecord.c / epidRecord.c /
        // motorRecord.cc:259-279 (the rset table: get_units, get_precision,
        // get_graphic_double, get_control_double and get_alarm_double all
        // supplied, get_enum_strs NULL) / aSubRecord.c: full numeric set, no
        // enum strings.
        "scalcout" | "acalcout" | "motor" | "epid" | "aSub" => P::NUMERIC,
        // scalerRecord.c:147-158 NULLs every property slot but one:
        // `#define get_units NULL` (:151), `get_enum_strs` (:154),
        // `get_graphic_double` (:156), `get_control_double` (:157) and
        // `get_alarm_double` (:158). Only `get_precision` (:152) survives.
        //
        // Grouping scaler with the full-numeric synApps types claimed TEN
        // leaves QSRV2 never serves: `display.units`, the two `display.limit*`,
        // the two `control.limit*`, the four `valueAlarm.*Limit` — and
        // `display.precision`, which pvxs assigns only inside its
        // `DBR_GR_DOUBLE` branch (`iocsource.cpp:288-291`). That nesting is
        // also why `precision` stays true here and yet marks nothing: it
        // records what the rset supplies, exactly as `transform`/`sseq` do.
        "scaler" => P {
            precision: true,
            ..P::NONE
        },
        // tableRecord.cc (optics): `#define get_alarm_double NULL`.
        "table" => P {
            alarm_double: false,
            ..P::NUMERIC
        },
        // swaitRecord.c: get_units and get_control_double are NULL.
        "swait" => P {
            units: false,
            control_double: false,
            ..P::NUMERIC
        },
        // Only `get_precision` survives; the rset NULLs the other five.
        // transformRecord.c; sseqRecord.c:124-144 (the rset table itself —
        // `NULL, /* get_units */ get_precision, /* get_precision */ ...
        // NULL, /* get_graphic_double */ NULL, /* get_control_double */
        // NULL /* get_alarm_double */`). `sseq` was previously grouped with
        // the full-numeric synApps types, which marked six leaves per field
        // that QSRV2 omits entirely.
        //
        // `asyn` is NOT here: asyn-rs owns that record and declares its own
        // row (asynRecord.c:84-91, the same shape) — a downstream crate
        // cannot reach this table, which is why `Record::property_support`
        // is the hook and this is only its default.
        "transform" | "sseq" => P {
            precision: true,
            ..P::NONE
        },
        "throttle" => P {
            precision: true,
            graphic_double: true,
            ..P::NONE
        },

        // A record type whose C rset the port has not transcribed keeps
        // the pre-existing "supplies what it populated" behaviour rather
        // than silently losing metadata. Add an arm above — with the C
        // file and line — when porting a new record type.
        _ => P::NUMERIC,
    }
}

/// Per-field metadata deltas returned by
/// [`Record::field_metadata_override`].
///
/// Each `Some` member replaces the corresponding member of the
/// snapshot's record-level display/control metadata; `None` members
/// keep the record-level value.
#[derive(Debug, Clone, Default)]
pub struct FieldMetadataOverride {
    /// `display.units` — C RSET `get_units`.
    pub units: Option<crate::types::PvString>,
    /// `display.precision` — C RSET `get_precision`.
    pub precision: Option<i16>,
    /// `(upper, lower)` display limits — C RSET `get_graphic_double`.
    pub disp_limits: Option<(f64, f64)>,
    /// `(upper, lower)` control limits — C RSET `get_control_double`.
    pub ctrl_limits: Option<(f64, f64)>,
    /// `(hihi, high, low, lolo)` — C RSET `get_alarm_double`.
    pub alarm_limits: Option<(f64, f64, f64, f64)>,
}

/// Work a record's `special(_, true)` owes once the put's gate is down, and
/// may not do under the hold — a spawned link-status classification, say.
/// See [`Record::take_after_gate`].
pub struct AfterGate(AfterWork);

/// Shared work a record builds once, so leaving it from `special()` under the
/// hold allocates nothing.
pub type SharedAfterWork =
    std::sync::Arc<dyn Fn(&crate::server::database::PvDatabase) + Send + Sync>;

enum AfterWork {
    Once(Box<dyn FnOnce(&crate::server::database::PvDatabase) + Send + Sync>),
    Shared(SharedAfterWork),
}

impl AfterGate {
    pub fn new(
        work: impl FnOnce(&crate::server::database::PvDatabase) + Send + Sync + 'static,
    ) -> Self {
        Self(AfterWork::Once(Box::new(work)))
    }

    /// `work`, preallocated by the record; the gate holds a clone.
    pub fn shared(work: &SharedAfterWork) -> Self {
        Self(AfterWork::Shared(std::sync::Arc::clone(work)))
    }

    pub(crate) fn run(self, db: &crate::server::database::PvDatabase) {
        match self.0 {
            AfterWork::Once(work) => work(db),
            AfterWork::Shared(work) => work(db),
        }
    }
}

impl std::fmt::Debug for AfterGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AfterGate")
    }
}

/// Side-effect actions that a record requests from the processing framework.
///
/// Records return these from `process()` via `ProcessOutcome::actions`.
/// The framework executes them at the appropriate point in the processing
/// cycle, keeping records as pure state machines without direct DB access.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessAction {
    /// Write a value to a DB link. The framework reads `link_field` from the
    /// record to get the target PV name, then writes `value` to that PV.
    ///
    /// Executed after alarm/snapshot, before FLNK.
    /// Example: scaler writes CNT to COUT/COUTP links.
    WriteDbLink {
        link_field: &'static str,
        value: EpicsValue,
    },

    /// A [`Self::WriteDbLink`] whose failure is C's `if (status) return
    /// status;`: `process()` returns the put's status there, and the cycle
    /// runs no further action, alarm commit, monitor or forward link. `pact`
    /// is what the C `process()` had done to PACT by then.
    WriteDbLinkOrReturn {
        link_field: &'static str,
        value: EpicsValue,
        pact: ReturnPact,
    },

    /// Resolve an OUT link's TARGET ([`OutTarget`]) and hand it to the record
    /// through [`Record::set_resolved_out_target`], BEFORE `process()` runs.
    ///
    /// **Pre-process action** — the OUT-link twin of [`Self::ReadDbLink`],
    /// and C's `checkLinks`-cached `lnk_field_type`: a record whose fire-time
    /// branch depends on the target's DBF class (sseq decides the wire buffer
    /// AND whether a `WAITn` put-callback is issued from the one switch,
    /// `sseqRecord.c:714-792`) must have the class in hand when it decides, not
    /// after the framework's put path has already been entered.
    ResolveOutTarget { link_field: &'static str },

    /// Read a value from a DB link into a record field. The framework reads
    /// `link_field` from the record to get the source PV name, reads that PV,
    /// and writes the result into `target_field` via an internal put that
    /// bypasses read-only checks.
    ///
    /// The value delivered is the link target's **native** [`EpicsValue`] — it
    /// is NOT coerced to a numeric type on the way in. The record coerces (or
    /// preserves) it at its own `put_field`/`put_field_internal` boundary, so a
    /// string-class source can reach a string field byte-exact (the `sseq`
    /// `DOLn`→`STRn` path, C `sseqRecord.c:643-705`). Records whose
    /// `target_field` is numeric simply convert there, exactly as before.
    ///
    /// **Pre-process action**: executed BEFORE the next process() cycle so
    /// the value is immediately available. This matches C EPICS `dbGetLink()`
    /// which is synchronous/immediate.
    ///
    /// Example: throttle reads SINP into VAL when SYNC is triggered.
    ReadDbLink {
        link_field: &'static str,
        target_field: &'static str,
    },

    /// C `dbGetLink` into a buffer the record owns rather than one of its
    /// fields — throttle's `valueSync` reads `SINP` into `rpvt->sival` and
    /// decides `VAL` and `STS` from the status (`throttleRecord.c:628-647`).
    /// The framework reads `link_field` at the record's
    /// [`Record::input_link_request`] class, raises the link alarm a failed
    /// read raises, and hands the outcome to [`Record::link_read_result`].
    /// From `process()` it runs with the cycle's link writes, ahead of the
    /// alarm reset and the monitor post, where a C `process()` reads.
    GetLink { link_field: &'static str },

    /// Schedule a re-process of this record after the given duration.
    /// The framework defers it through
    /// `database/processing.rs` (`schedule_delayed_reprocess`), which uses
    /// `spawn_background` + `sleep_background` — *not* the ambient
    /// `tokio::spawn`/`tokio::time::sleep`, which a record-processing thread
    /// has no runtime for. The current cycle's OUT/FLNK/notify proceed
    /// normally.
    ///
    /// Equivalent to C EPICS `callbackRequestDelayed()` + `scanOnce()`
    /// (`db/callback.c:410` (`callbackRequestDelayed`), `db/dbScan.c:660`
    /// (`scanOnce`); epics-base R7.0.10).
    ReprocessAfter(std::time::Duration),

    /// C `callbackRequestDelayed()` armed with a handler that mutates the
    /// record BEFORE it calls `dbProcess` — `boRecord.c::myCallbackFunc`
    /// (:105-118) and its `busyRecord.c:107-124` twin, the HIGH one-shot.
    ///
    /// Distinct from [`Self::ReprocessAfter`], whose timer re-enters
    /// `process()` unchanged. Here the fire runs
    /// [`Record::delayed_callback_fire`] under the record gate first, and that
    /// hook — not `process()` — performs the timer's own mutation. The record
    /// therefore keeps no "a timer is pending" flag for `process()` to consume,
    /// so a foreign scan, a caput or a FLNK arriving inside the delay window
    /// cannot take the one-shot the timer owns.
    DelayedCallbackAfter(std::time::Duration),

    /// C throttle's `callbackRequestDelayed(&delayFuncCb, ..)`
    /// (`throttleRecord.c:593`), whose handler runs `valuePut` under
    /// `dbScanLock` and never calls `dbProcess` (`:530-538`). The fire runs
    /// [`Record::delayed_value_put`] and the actions it returns, then posts
    /// the record's marks and fires FLNK if
    /// [`Record::should_fire_forward_link`]; it is not a process cycle.
    ValuePutAfter(std::time::Duration),

    /// C `scanOnce(precord)` — queue ONE process of this record, now.
    ///
    /// A record's `special()` emits this when a put changed state the record
    /// must act on but the put itself will not process the record. C guards
    /// every such call with `if (precord->scan)` — scaler `special()`
    /// (scalerRecord.c:655 CNT, :667 CONT), whose comment is exactly the
    /// contract: *"Scan record if it's not Passive. (If it's Passive, it'll
    /// get scanned automatically, since .cnt is a Process-Passive field.)"*
    ///
    /// The FRAMEWORK owns that gate, not the record: the framework owns SCAN
    /// and owns the `pp(TRUE)` reprocess decision (`dbPutField`,
    /// dbAccess.c:1265-1268), and a record's `special()` cannot see either. So
    /// a record emits `ScanOnce` unconditionally wherever C calls `scanOnce`,
    /// and the executor drops it for a Passive record — where the put's own
    /// process already covers it and a second one would double-process.
    ///
    /// Queued, not inline: C's `scanOnce` hands the record to the scan-once
    /// thread, which takes `dbScanLock` — so the process lands after the
    /// putting thread leaves `dbPutField`.
    ScanOnce,

    /// Send a named command to the device support driver.
    /// The framework calls `DeviceSupport::handle_command()` with this data.
    /// Used by scaler to request reset/arm/write_preset operations
    /// without the record holding a direct driver reference.
    DeviceCommand {
        command: &'static str,
        args: Vec<EpicsValue>,
    },

    /// Write a value to a DB link as a put-*with-completion*, then re-enter
    /// THIS record's `process()` when the downstream operation completes.
    ///
    /// The framework arms a put-notify wait-set (C `dbProcessNotify`),
    /// writes `link_field`'s target through it, releases the initiator's
    /// own count, and wires the completion to an async re-entry of this
    /// record (`mint_async_token` + `reprocess_on_notify`). The record
    /// returns [`RecordProcessResult::AsyncPending`] alongside this action
    /// and is re-entered once the downstream record (and its FLNK/OUT
    /// chain) finishes — the synApps `sseq` `WAITn` "wait for the put
    /// callback" dependency (`sseqRecord.c::processNextLink`,
    /// `dbCaPutLinkCallback`). Built on the same `new_put_notify` +
    /// `reprocess_on_notify` primitive an out-of-band
    /// [`crate::server::database::AsyncDbHandle`] caller uses.
    ///
    /// Executed before FLNK, like [`Self::WriteDbLink`].
    WriteDbLinkNotify {
        link_field: &'static str,
        value: EpicsValue,
    },

    /// (Re)arm this record's monitor watchdog — C `histogramRecord.c::wdogInit`
    /// (:126-152), whose `callbackRequestDelayed(&pcallback->callback,
    /// prec->sdel)` starts (or restarts) the periodic
    /// [`Record::watchdog_fire`] tick.
    ///
    /// Emitted from a record's `special()` when the put changed the watchdog's
    /// period (histogram SDEL is `special(SPC_RESET)` precisely so it can
    /// re-arm, `histogramRecord.c:266-268`). The framework also arms every
    /// record's watchdog once at `iocInit`, which is C's other `wdogInit` call
    /// site (`init_record` pass 1, `:168`).
    ///
    /// Arming supersedes any tick already pending for the record, exactly as
    /// C's `callbackRequestDelayed` replaces an outstanding delayed callback.
    ArmWatchdog,

    /// Cancel this record's outstanding async re-entry (C
    /// `callbackCancelDelayed`): the framework advances the record's
    /// re-entry generation so any pending `ReprocessAfter` timer or
    /// `WriteDbLinkNotify` completion re-entry becomes a structural no-op
    /// (the `AsyncToken` gate), with no runtime "is-aborted" check on the
    /// re-entry path. Used by `sseq` `ABORT` to drop a pending `DLYn`
    /// delay or `WAITn` wait; the record resets its own sequence state in
    /// the same `process()` cycle that emits this.
    CancelReprocess,

    /// C `checkLink` called from `process()` itself rather than at its top
    /// (throttle, under `proc_flag`, `throttleRecord.c:289-293`):
    /// re-read the [`Record::link_check`] links now, among the cycle's link
    /// actions, and arm the recheck timer if one stays disconnected.
    CheckLinks,

    /// Posts C's `process()` makes after its epilogue — after `monitor()`'s
    /// alarm commit and `recGblFwdLink`, or on a cycle that skipped both
    /// (scaler's autocount FREQ, `scalerRecord.c:530`). Run with the cycle's
    /// other deferred actions, past the FLNK tail on a completing cycle and
    /// with the actions of a [`RecordProcessResult::CompleteNoEmit`] one. The
    /// marks name a static table, so the action owns no allocation; a
    /// `Reset*` entry has no meaning here.
    PostAfterEpilogue(CycleMarks),

    /// Store `field` and post it `DBE_VALUE` plus the bit the cycle's
    /// `recGblResetAlarms` returned — C `asyncFinish`'s `busy = 0` and its
    /// `MonitorMask` post, after `recGblFwdLink` and the closing stamp
    /// (`sseqRecord.c:471`, `:499-505`), so a forward-link target still reads
    /// the field set. Run with the cycle's other deferred actions. A
    /// [`CycleValue`], so the variant adds no drop to the link stage that
    /// carries the actions.
    PutAfterEpilogue {
        field: &'static str,
        value: CycleValue,
    },

    /// Store `field` at this place among the cycle's link writes, posted as
    /// `post` says: the puts ahead of it see the old value and the puts after
    /// it the new one (scanparm's `go = 0` between `dbPutLink(OGO)` and
    /// `dbPutLink(OLOAD)`, `scanparmRecord.c:248-262`).
    ///
    /// Last in the list, it is a flag C clears under the `dbScanLock` its
    /// puts ran in (`sseqRecord.c:714-792` then `:498-505`). The port
    /// releases the data guard across the writes, so a store made in
    /// `process()` would let `dbGetField` read `BUSY == 0` with `LNKn`
    /// unwritten. A clear C makes after `recGblFwdLink` is a
    /// [`Self::PutAfterEpilogue`]; a flag a later decision of the same cycle
    /// reads keeps its `process()` store (motor's `DMOV`,
    /// `motorRecord.cc:1509-1510`).
    PutInLinkStage {
        field: &'static str,
        value: CycleValue,
        post: PostStage,
    },

    /// Print `prefix`, the record's name and `suffix` as one stdout line at
    /// this place among the cycle's link writes: a C debug `printf` between
    /// two `dbPutLink` groups, which a group that returns ahead of it never
    /// reaches (scanparm's "starting scan", `scanparmRecord.c:206-211`).
    Trace {
        prefix: &'static str,
        suffix: &'static str,
    },
}

/// PACT where a C `process()` returns a failed put's status
/// ([`ProcessAction::WriteDbLinkOrReturn`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReturnPact {
    /// Set, and nothing clears it (`tableRecord.c:574-576`, `:1001-1004`).
    Held,
    /// Never set: the return comes ahead of `pact = TRUE`
    /// (`scanparmRecord.c:206`, `:219`).
    Clear,
}

/// Where a C `process()` calls `recGblGetTimeStamp` for its cycle
/// ([`Record::stamp_point`]). The cycle reads TSEL at this point too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StampPoint {
    /// Before `checkAlarms` and the outputs (`aiRecord.c:156/164`,
    /// `aoRecord.c:190`).
    #[default]
    BeforeAlarms,
    /// After `checkAlarms`, before the outputs: calcout, scalcout and
    /// acalcout (`calcoutRecord.c:244/249`), so a failed TSEL read's LINK
    /// alarm loses an equal-severity tie to the record's own limit alarm.
    AfterAlarms,
    /// After every link put of `process()`, before `monitor()`
    /// (`tableRecord.c:594`, `scanparmRecord.c:263`). A cycle that returns a
    /// failed put's status leaves before it and does not move TIME.
    AfterLinkWrites,
    /// After `monitor()` and `recGblFwdLink`: sseq's `asyncFinish`
    /// (`sseqRecord.c:474`, `:499`, `:501`), so the VAL event carries the
    /// previous completion's time.
    AfterCompletion,
}

impl StampPoint {
    /// Stamps after the record's link writes, with its own TSEL read there.
    pub const fn is_late(self) -> bool {
        matches!(self, Self::AfterLinkWrites | Self::AfterCompletion)
    }
}

/// A list a record hands the framework for one cycle — the actions of
/// [`Record::pre_input_link_actions`] and [`Record::pre_process_actions`],
/// and the shape a `process()` outcome's lists are carried in.
///
/// An empty list is the answer of every record type that never fills it,
/// on every cycle, so discarding one must cost nothing: the list owns no
/// allocation while its capacity is zero, and the drop is a single inline
/// test then. A bare `Vec` pays an out-of-line drop glue call for the same
/// nothing, once per list per cycle.
#[derive(Debug)]
pub struct CycleList<T>(std::mem::ManuallyDrop<Vec<T>>);

/// The actions a record hands the framework for one cycle.
pub type ProcessActions = CycleList<ProcessAction>;

/// What one link read came back with — [`Record::link_read_result`]. C's
/// `dbGetLink` hands back a status and fills the caller's buffer, so the value
/// travels here only when the read named no field to fill.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkRead {
    /// Status 0, and a [`ProcessAction::GetLink`] read delivered this value.
    Value(EpicsValue),
    /// Status 0 with no value in this report: a [`ProcessAction::ReadDbLink`]
    /// (its `target_field` holds whatever the link delivered), or a `GetLink`
    /// whose link wrote nothing — a CONSTANT link (`dbConstGetValue`,
    /// `dbConstLink.c:219-225`) or one the record reads at no class.
    Succeeded,
    /// Non-zero status. The LINK/INVALID alarm is already raised.
    Failed,
}

/// `prec->pact` as [`Record::set_process_continuation`] sees it: a view the
/// record may read and never write — PACT has one owner. Passed as a view, not
/// a loaded `bool`, so a record that never asks costs the cycle no load.
#[derive(Clone, Copy)]
pub struct Pact<'a>(&'a std::sync::atomic::AtomicBool);

impl<'a> Pact<'a> {
    pub fn new(flag: &'a std::sync::atomic::AtomicBool) -> Self {
        Self(flag)
    }

    /// PACT is held.
    pub fn held(self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Which of the per-cycle hooks a type can answer with something other than
/// the trait default. C has no such hooks (`dbCommon` is read as a struct
/// member), so the cycle settles this once, in the plan, and a type that
/// claims a bit not set here is never asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CycleHooks(u16);

impl Default for CycleHooks {
    fn default() -> Self {
        Self::ALL
    }
}

impl CycleHooks {
    /// [`Record::pre_input_link_actions`].
    pub const PRE_INPUT: Self = Self(1);
    /// [`Record::take_cycle_posted_fields`].
    pub const CYCLE_POSTED: Self = Self(2);
    /// [`Record::set_process_context`].
    pub const CONTEXT: Self = Self(4);
    /// [`Record::take_cycle_posted_fields`] outside `monitor()`: a put's
    /// `special()` or a link read marks posts the write's owner drains.
    pub const WRITE_POSTED: Self = Self(8);
    /// [`Record::take_process_posted_fields`].
    pub const PROCESS_POSTED: Self = Self(16);
    /// [`Record::take_put_report`] after a `special(_, true)` that returned
    /// `Ok`: a put-only hook, outside [`Self::ALL`], declared by the type
    /// whose `special()` reports and still accepts the put.
    pub const PUT_REPORT: Self = Self(32);
    /// [`Record::take_cycle_posted_fields`] drained in the cycle where the
    /// marks are made, never by `monitor()`: a C record that posts by hand
    /// from `process()` (throttle `valuePut`, `throttleRecord.c:540-615`).
    /// Outside [`Self::ALL`].
    pub const HAND_POSTED: Self = Self(128);
    /// [`Self::HAND_POSTED`], and `recGblGetTimeStamp` runs only ahead of the
    /// record's own `recGblResetAlarms`, posted from
    /// [`Record::take_value_put_tail`]: throttle stamps in `valuePut` alone
    /// (`throttleRecord.c:602`), so a rejected or queued cycle keeps the
    /// previous TIME. The stamp bit has no constant of its own: only the
    /// tail's owners stamp.
    pub const HAND_POSTED_OWN_STAMP: Self = Self(Self::HAND_POSTED.0 | Self::OWN_STAMP);
    const OWN_STAMP: u16 = 256;
    /// [`Record::gate_after_pre_input_reads`]: outside [`Self::ALL`],
    /// declared by the type whose cycle hangs on a pre-input read.
    pub const PRE_INPUT_GATE: Self = Self(512);
    /// [`Record::set_resolved_input_links`], [`Record::set_fetch_gate_failed`]
    /// and [`Record::read_constant_inp`]: what the input stage tells the type.
    pub const INPUT_FETCH: Self = Self(64);
    /// [`Record::prev_compared_inputs`], [`Record::force_posted_fields`] and
    /// [`Record::log_swept_fields`]: `monitor()`'s per-type post lists.
    pub const MONITOR_SETS: Self = Self(1024);
    /// [`Record::should_output`] and [`Record::output_event`]: the output
    /// stage's questions.
    pub const OUTPUT_HOOKS: Self = Self(2048);
    /// [`Record::set_process_continuation`].
    pub const CONTINUATION: Self = Self(4096);
    /// [`Record::take_time_stamp_request`]: outside [`Self::ALL`], declared by
    /// a type whose C `process()` calls `recGblGetTimeStamp` only on some
    /// passes (`mcaRecord.c:762`, `:811`). The cycle stamps such a pass only
    /// when its `process()` asked, and reads no `TSEL` otherwise.
    pub const STAMP_REQUEST: Self = Self(8192);
    /// [`Record::monitor_resets_alarms`]: outside [`Self::ALL`], declared by a
    /// type whose C `monitor()` calls `recGblResetAlarms` only on some cycles
    /// (`scanparmRecord.c:280`).
    pub const MONITOR_RESET: Self = Self(16384);
    /// [`Record::link_check`]: C `checkLinks` at the top of `process()`.
    /// Outside [`Self::ALL`]: a type declares it. A type that checks later
    /// in `process()` returns [`ProcessAction::CheckLinks`] instead.
    pub const LINK_CHECK: Self = Self(32768);
    /// No hook acts: the stock record.
    pub const NONE: Self = Self(0);
    /// Every per-cycle hook is asked — the default, so a type that never says
    /// otherwise pays the call and never loses an action.
    pub const ALL: Self = Self(31 | 64 | 1024 | 2048 | 4096);

    /// Both sets together.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether `hook` is among these.
    #[inline(always)]
    pub const fn has(self, hook: Self) -> bool {
        self.0 & hook.0 != 0
    }

    /// Whether these are [`Self::HAND_POSTED_OWN_STAMP`].
    #[inline(always)]
    pub const fn stamps_at_own_reset(self) -> bool {
        self.0 & Self::OWN_STAMP != 0
    }

    /// Whether the cycle's own stamp may be withheld: [`Self::STAMP_REQUEST`]
    /// or the own-stamp bit of [`Self::HAND_POSTED_OWN_STAMP`].
    #[inline(always)]
    pub const fn withholds_cycle_stamp(self) -> bool {
        self.0 & (Self::OWN_STAMP | Self::STAMP_REQUEST.0) != 0
    }
}

// The stamp bit never stands without the drain that stamps.
const _: () = assert!(CycleHooks::HAND_POSTED_OWN_STAMP.has(CycleHooks::HAND_POSTED));
// Every single-bit hook is one bit, and no two share it.
const _: () = {
    let bits = [
        CycleHooks::PRE_INPUT.0,
        CycleHooks::CYCLE_POSTED.0,
        CycleHooks::CONTEXT.0,
        CycleHooks::WRITE_POSTED.0,
        CycleHooks::PROCESS_POSTED.0,
        CycleHooks::PUT_REPORT.0,
        CycleHooks::HAND_POSTED.0,
        CycleHooks::OWN_STAMP,
        CycleHooks::PRE_INPUT_GATE.0,
        CycleHooks::INPUT_FETCH.0,
        CycleHooks::MONITOR_SETS.0,
        CycleHooks::OUTPUT_HOOKS.0,
        CycleHooks::CONTINUATION.0,
        CycleHooks::STAMP_REQUEST.0,
        CycleHooks::MONITOR_RESET.0,
        CycleHooks::LINK_CHECK.0,
    ];
    let mut seen = 0;
    let mut i = 0;
    while i < bits.len() {
        assert!(bits[i].count_ones() == 1 && seen & bits[i] == 0);
        seen |= bits[i];
        i += 1;
    }
};

impl<T> CycleList<T> {
    /// An empty list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one entry, after those already listed.
    pub fn push(&mut self, entry: T) {
        self.0.push(entry);
    }

    /// The list as the `Vec` it is, for the consumers that take it apart.
    pub fn into_vec(mut self) -> Vec<T> {
        // The take leaves a capacity-0 `Vec` behind, which the drop below
        // then has nothing to release.
        std::mem::take(&mut *self.0)
    }

    #[cold]
    #[inline(never)]
    fn release(entries: &mut std::mem::ManuallyDrop<Vec<T>>) {
        // SAFETY: called once, from `drop`, on a field nothing else drops —
        // it is `ManuallyDrop`, and `into_vec` leaves a fresh `Vec` in it.
        unsafe { std::mem::ManuallyDrop::drop(entries) }
    }
}

impl<T> Default for CycleList<T> {
    fn default() -> Self {
        Self(std::mem::ManuallyDrop::new(Vec::new()))
    }
}

impl<T> From<Vec<T>> for CycleList<T> {
    fn from(entries: Vec<T>) -> Self {
        Self(std::mem::ManuallyDrop::new(entries))
    }
}

impl<T> Drop for CycleList<T> {
    #[inline]
    fn drop(&mut self) {
        // A capacity-0 `Vec` holds no elements and owns no allocation, so
        // there is nothing to run for it. The release is a frame of its own
        // so that this test is all the drop glue holds, and is inlined.
        if self.0.capacity() != 0 {
            Self::release(&mut self.0);
        }
    }
}

impl<T> std::ops::Deref for CycleList<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T> IntoIterator for CycleList<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.into_vec().into_iter()
    }
}

/// What the [`ProcessAction::DelayedCallbackAfter`] timer does once the
/// record's [`Record::delayed_callback_fire`] handler has run — the three arms
/// of C `boRecord.c::myCallbackFunc` (:105-118).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelayedCallbackOutcome {
    /// C's `else` branch (`boRecord.c:115-117`): the handler made its change,
    /// so re-enter `process()` now.
    Reprocess,
    /// C's `if (prec->pact)` branch (`boRecord.c:107-114`): the record is
    /// mid-async-cycle, so the handler changed nothing and the timer is re-armed
    /// for this long instead.
    Rearm(std::time::Duration),
    /// C's `if (prec->pact)` branch with its inner test false: the timer expires
    /// with nothing left to do, neither mutating nor processing.
    Drop,
}

/// Result of a record's process() call.
///
/// Determines how the framework handles the current processing cycle.
/// Side-effect actions (link writes, delayed reprocess, etc.) are expressed
/// separately in `ProcessOutcome::actions`.
#[derive(Clone, Debug, PartialEq)]
pub enum RecordProcessResult {
    /// Processing completed synchronously this cycle.
    /// Framework proceeds with alarm/timestamp/snapshot/OUT/FLNK.
    Complete,
    /// Processing started but not yet complete (PACT stays set).
    /// Current cycle skips alarm/timestamp/snapshot/OUT/FLNK.
    /// ProcessActions (if any) are still executed.
    AsyncPending,
    /// Async pending, but notify these intermediate field changes immediately,
    /// each read from the record. Used by motor records to flush DMOV=0
    /// before the move completes.
    AsyncPendingNotify(Vec<FieldIx>),
    /// Completed synchronously (PACT cleared, unlike `AsyncPending`), but the
    /// record produced no new value to publish this cycle — the framework must
    /// skip the value-publication epilogue (UDF clear / timestamp / monitor /
    /// FLNK). C parity `compressRecord.c:365` `if (status != 1)`: a compress
    /// record still accumulating toward its next compressed sample runs none of
    /// `recGblGetTimeStamp` / `monitor` / `recGblFwdLink` on that cycle.
    /// ProcessActions are still executed: the `dbPutLink`s C makes inside
    /// `process()` before that return (a scaler's COUT on a non-idle cycle,
    /// `scalerRecord.c:456-466`) have already happened.
    ///
    /// `raise` is a `recGblSetSevr` made before that return (`epidRecord.c:199`
    /// ahead of `:201`): the framework puts it in `nsta`/`nsev` on either
    /// dispatch path, and the next cycle's `recGblResetAlarms` commits it.
    CompleteNoEmit {
        raise: Option<(u16, crate::server::record::AlarmSeverity)>,
    },
    /// Ran the value-publication epilogue NOW (UDF clear / timestamp / monitor —
    /// VAL and the alarm fields are posted this cycle), but the OUTPUT side (OUT
    /// link write / OEVT / forward link) is deferred to a scheduled
    /// reprocess, with PACT held across the wait. C parity `swaitRecord.c::process`
    /// (lines 425-481): when `schedOutput` arms the ODLY watchdog it sets
    /// `async=TRUE`, so `process` still runs `monitor()` (line 475) — posting the
    /// value side at the START of the delay — but skips the `if(!async)
    /// {recGblFwdLink; pact=FALSE;}` tail; the deferred `execOutput` (watchdog,
    /// at delay-END) does the OUT write + OEVT + forward link and posts no
    /// monitors. Unlike the calcout/scalcout/acalcout family, whose C `process`
    /// `return`s BEFORE `monitor()` (calcoutRecord.c:282, only `dlya` posted), so
    /// they defer the value side too and use `AsyncPendingNotify`. The deferral
    /// must carry a [`ProcessAction::ReprocessAfter`] — that scheduled reprocess
    /// is the continuation that releases the held PACT (same by-construction
    /// invariant as the `AsyncPendingNotify` ODLY defer).
    CompleteDeferOutput,
    /// Completed synchronously (PACT cleared), and the framework runs the ALARM
    /// epilogue ONLY: the UDF update, `check_alarms`, `recGblResetAlarms`
    /// (committing SEVR/STAT/AMSG and posting those fields with their C masks)
    /// and the timestamp. The VALUE side is skipped entirely — no `monitor()`
    /// value posts (so the last-posted trackers stay put and the next publishing
    /// cycle re-detects the change, exactly as C leaves `LA..LP` un-updated), no
    /// OUT / OEVT write, no process actions, no forward link.
    ///
    /// C parity `transformRecord.c:554-560`: an INVALID input severity with
    /// `IVLA == transformIVLA_DO_NOTHING` makes `process()` run
    /// `recGblGetTimeStamp` + `checkAlarms` + `recGblResetAlarms`, clear `pact`
    /// and `return` — skipping the calc loop, all 16 OUTx `dbPutLink` writes,
    /// `monitor()` and `recGblFwdLink()`.
    ///
    /// Distinct from [`RecordProcessResult::CompleteNoEmit`], which skips the
    /// alarm commit and the timestamp too (C `compressRecord.c:365` returns
    /// before `checkAlarms`).
    CompleteAlarmOnly,
}

/// Which statuses of the framework's cycle a type's C `process()` returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessStatusRule {
    /// `readValue`/`writeValue`'s status: a failed device read or write, the
    /// SIOL link's under simulation, and an illegal SIMM's -1.
    pub io: bool,
    /// The Soft Channel dsets whose `read_xxx`/`write_xxx` return the INP or
    /// OUT link's status.
    pub soft: SoftStatus,
    /// The soft dset is `dbGetLinkLS`/`dbPutLinkLS`'s, which return 0 for an
    /// unconnected link (`dbLink.c:504-505`).
    pub ls: bool,
    pub dol: DolStatus,
    /// The links of the [`ProcessAction::WriteDbLink`]s that assign
    /// `process()`'s returned `status`: the last of them to be put wins, a
    /// success over an earlier failure (`motorRecord.cc:1495` RLNK,
    /// `scanparmRecord.c:250-258` OGO then OLOAD).
    pub status_links: &'static [&'static str],
}

impl ProcessStatusRule {
    /// A `process()` that returns 0 whatever its links did.
    pub const ZERO: Self = Self {
        io: false,
        soft: SoftStatus::NONE,
        ls: false,
        dol: DolStatus::Dropped,
        status_links: &[],
    };

    /// `readValue`/`writeValue`'s status, with `soft`'s dsets passing the
    /// link's up.
    pub const fn io(soft: SoftStatus) -> Self {
        Self {
            io: true,
            soft,
            ..Self::ZERO
        }
    }

    pub const fn with_dol(self, dol: DolStatus) -> Self {
        Self { dol, ..self }
    }

    pub const fn ls(self) -> Self {
        Self { ls: true, ..self }
    }

    pub const fn with_status_links(self, links: &'static [&'static str]) -> Self {
        Self {
            status_links: links,
            ..self
        }
    }
}

/// [`ProcessStatusRule::soft`], one flag per [`SoftDtyp`](crate::server::device_support::SoftDtyp).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoftStatus {
    pub plain: bool,
    pub raw: bool,
    pub async_: bool,
}

impl SoftStatus {
    pub const NONE: Self = Self {
        plain: false,
        raw: false,
        async_: false,
    };

    pub const fn returns(self, dtyp: crate::server::device_support::SoftDtyp) -> bool {
        use crate::server::device_support::SoftDtyp;
        match dtyp {
            SoftDtyp::Plain => self.plain,
            SoftDtyp::Raw => self.raw,
            SoftDtyp::Async => self.async_,
        }
    }
}

/// What a failed closed-loop DOL read does to `process()`'s status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DolStatus {
    /// Overwritten or never kept (`mbboRecord.c`, `lsoRecord.c`).
    Dropped,
    /// Kept unless `writeValue` runs and replaces it — only IVOA "Don't
    /// drive outputs" keeps it (`aoRecord.c:196-223`).
    UntilWrite,
    /// Returned whatever the outputs did (`dfanoutRecord.c:119`).
    Kept,
}

/// Complete outcome of a record's process() call.
///
/// Contains the processing result (Complete, AsyncPending, etc.) and a list
/// of side-effect actions for the framework to execute.
#[derive(Clone, Debug)]
pub struct ProcessOutcome {
    pub result: RecordProcessResult,
    pub actions: Vec<ProcessAction>,
    /// Set by the framework when device support's read() returned
    /// `did_compute: true`. The record's process() can check this to
    /// skip its built-in computation (e.g., PID). Replaces the `pid_done`
    /// flag pattern.
    pub device_did_compute: bool,
    /// The status the record's own `process()` returns: epid's `do_pid`
    /// status (`epidRecord.c:214`), aao's failed DOL (`aaoRecord.c:167-168`).
    /// The framework's statuses are [`Record::process_status_rule`]'s.
    pub status: crate::error::DbResult,
}

/// Who posts a [`ProcessAction::PutInLinkStage`] store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostStage {
    /// Posted there, `DBE_VALUE` (`sseqRecord.c:304`, `scanparmRecord.c:250`).
    Output,
    /// Posted by the cycle's `monitor()` change walk with the field's mask,
    /// after `recGblResetAlarms` (`asyncFinish`'s `abort`/`aborting`,
    /// `:471`, `:479-482`, which the type declares `DBE_VALUE` plus the alarm
    /// bit through [`Record::value_only_change_fields`]).
    Monitor,
}

impl ProcessOutcome {
    /// Shorthand for a simple Complete with no actions.
    pub fn complete() -> Self {
        Self {
            result: RecordProcessResult::Complete,
            actions: Vec::new(),
            device_did_compute: false,
            status: Ok(()),
        }
    }

    /// Shorthand for Complete with actions.
    pub fn complete_with(actions: Vec<ProcessAction>) -> Self {
        Self {
            result: RecordProcessResult::Complete,
            actions,
            device_did_compute: false,
            status: Ok(()),
        }
    }

    /// Completed synchronously, but no new value was emitted this cycle, so
    /// the framework skips the value-publication epilogue (UDF clear /
    /// timestamp / monitor / FLNK). See `RecordProcessResult::CompleteNoEmit`.
    pub fn complete_no_emit() -> Self {
        Self {
            result: RecordProcessResult::CompleteNoEmit { raise: None },
            actions: Vec::new(),
            device_did_compute: false,
            status: Ok(()),
        }
    }

    /// [`Self::complete_no_emit`] that leaves `stat`/`sevr` pending.
    pub fn complete_no_emit_raising(stat: u16, sevr: crate::server::record::AlarmSeverity) -> Self {
        Self {
            result: RecordProcessResult::CompleteNoEmit {
                raise: Some((stat, sevr)),
            },
            ..Self::complete_no_emit()
        }
    }

    /// [`Self::complete_no_emit`] with the actions `process()` queued.
    pub fn complete_no_emit_with(actions: Vec<ProcessAction>) -> Self {
        Self {
            actions,
            ..Self::complete_no_emit()
        }
    }

    /// Completed synchronously with the alarm epilogue only — no value posts,
    /// no output, no forward link. See `RecordProcessResult::CompleteAlarmOnly`.
    pub fn complete_alarm_only() -> Self {
        Self {
            result: RecordProcessResult::CompleteAlarmOnly,
            actions: Vec::new(),
            device_did_compute: false,
            status: Ok(()),
        }
    }

    /// Shorthand for AsyncPending with no actions.
    pub fn async_pending() -> Self {
        Self {
            result: RecordProcessResult::AsyncPending,
            actions: Vec::new(),
            device_did_compute: false,
            status: Ok(()),
        }
    }
}

impl Default for ProcessOutcome {
    fn default() -> Self {
        Self::complete()
    }
}

/// Result of setting a common field, indicating what scan index updates are needed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommonFieldPutResult {
    NoChange,
    ScanChanged {
        old_scan: ScanType,
        new_scan: ScanType,
        phas: i16,
    },
    PhasChanged {
        scan: ScanType,
        old_phas: i16,
        new_phas: i16,
    },
}

/// Read-only snapshot of framework-owned `CommonFields` state that a
/// record's `process()` or device support's `read()` needs to see
/// *during* the processing cycle.
///
/// The framework owns `RecordInstance.common`; a record `process()`
/// receives only `&mut self` (the concrete record) and device support
/// `read()` receives only `&mut dyn Record`. Neither can reach
/// `CommonFields`. C records, by contrast, see `dbCommon` directly —
/// e.g. `epidRecord.c:195` reads `pepid->udf`, `timestampRecord.c:90`
/// reads `ptimestamp->tse`, `devTimeOfDay.c:122` reads `psi->phas`.
///
/// The framework builds a `ProcessContext` from `common` and pushes it
/// onto the record (via [`Record::set_process_context`]) and onto the
/// device support (via
/// [`crate::server::device_support::DeviceSupport::set_process_context`])
/// immediately before the respective call. This mirrors the existing
/// `set_device_did_compute` framework-set-hook pattern: additive,
/// no `process()` / `read()` signature change.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessContext {
    /// `dbCommon.udf`, the raw `DBF_UCHAR` byte: `epidRecord.c:195` tests
    /// `udf == TRUE`, which a byte of 2 does not pass.
    pub udf: u8,
    /// `dbCommon.udfs` — alarm severity raised for a UDF record.
    pub udfs: crate::server::record::AlarmSeverity,
    /// `dbCommon.nsev` — the *pending* (new) alarm severity this cycle has
    /// accumulated so far, BEFORE the record body runs. C `dbGetLink` folds an
    /// `MS`-class input link's severity into `nsev` at fetch time, so a record
    /// body that branches on the input severity reads it here — e.g.
    /// `transformRecord.c:554` `if ((ptran->nsev >= INVALID_ALARM) && (ptran->ivla
    /// == transformIVLA_DO_NOTHING))`. The framework folds every input-link alarm
    /// into `common.nsev` before building this snapshot, so `nsev` is the single
    /// source of truth; the record never re-derives it from the links.
    pub nsev: crate::server::record::AlarmSeverity,
    /// `dbCommon.phas` — phase. Used by device support for format
    /// selection (`devTimeOfDay.c:122`).
    pub phas: i16,
    /// `dbCommon.tse` — time-stamp event. `timestampRecord.c:90`
    /// branches on `tse == epicsTimeEventDeviceTime`.
    pub tse: i16,
    /// `dbCommon.time` — the record's current resolved time stamp at the
    /// start of this cycle (the previous cycle's stamp, or `UNIX_EPOCH`
    /// before the first process). Device support that has to format the
    /// record's time during `read()` — the std module's `devTimeOfDay.c`
    /// `recGblGetTimeStamp(psi)` call, which runs *before* the framework's
    /// per-cycle timestamp application — resolves the stamp with
    /// [`crate::server::recgbl::get_time_stamp`]`(tse, time)`. The `time`
    /// member is the device-provided value that helper returns verbatim on
    /// the `TSE == epicsTimeEventDeviceTime (-2)` branch.
    pub time: crate::types::WallTime,
    /// The soft-channel dset the record was bound to at iocInit
    /// ([`crate::server::device_support::Dtyp::soft`]), not the current DTYP
    /// name: C keeps `prec->dset` from `initDevSup`, so a runtime DTYP put
    /// changes nothing a hook may branch on (the epid record's
    /// `devEpidSoftCallback` TRIG write).
    pub soft: Option<crate::server::device_support::SoftDtyp>,
    /// The callback band `dbCommon.prio` selects for work this cycle defers —
    /// C `seqRecord.c:145-146`, which re-runs
    /// `callbackSetPriority(prec->prio, &pcb->callback)` at the top of every
    /// `process()` so a `PRIO` written between cycles takes effect on the next
    /// one. Already converted: a record stashes this and hands it to
    /// [`crate::runtime::task::spawn_background`] rather than re-deriving a
    /// band from a raw menu index.
    pub callback_priority: crate::runtime::task::CallbackPriority,
    /// `dbCommon.scan`: StreamEpics.cc `Stream::process` answers an I/O
    /// Intr record's pass from the input it already has.
    pub scan: crate::server::record::ScanType,
    /// `dbCommon.proc`, the raw byte: StreamEpics.cc runs the `@init`
    /// handler on a `PROC` put of 2.
    pub proc_field: u8,
    /// `dbCommon.tpro`, the raw byte.
    pub tpro: u8,
}

/// C `epicsTime.h`: `epicsTimeEventDeviceTime` — the `TSE` sentinel
/// meaning "device support provides the time stamp". `timestampRecord.c`
/// uses it to take the OS-clock branch instead of `recGblGetTimeStamp`.
pub const EPICS_TIME_EVENT_DEVICE_TIME: i16 = -2;

/// One `db_post_events(prec, &field, mask)` a process cycle owes.
///
/// Every posted field carries its OWN `DBE_*` mask, mirroring C's per-field
/// call. One cycle posts different classes per field: a deadband-gated
/// readback narrows to the deadbands that actually crossed (MDEL → `DBE_VALUE`,
/// ADEL → `DBE_LOG`; motorRecord.cc `monitor()` 3476-3507, aiRecord.c
/// `monitor()`), while a change-detected auxiliary field posts
/// `DBE_VALUE | DBE_LOG` (motorRecord.cc 3522-3645 `DBE_VAL_LOG`;
/// calcRecord.c:420). A single record-wide mask collapses that granularity —
/// an archive-only deadband crossing would wrongly reach `DBE_VALUE`
/// subscribers whenever any other field changed in the same pass.
///
/// The name is `Cow` because most posts name a field the record type declared
/// — a `&'static str` already in the binary, which C posts as `&prec->val`, a
/// pointer. Owning it charged one malloc and one free per posted field per
/// cycle. Subscriber-driven posts, whose names come from the subscription map,
/// are the owned case.
/// What one post carries of its field's value, by the field's
/// [`LogKind`](crate::server::event_queue::LogKind): a value only for a
/// field whose log copies one. A 40-byte string or an array fails C's
/// `useValque` (`dbEvent.c:493-500`, `DB_EVENT_LOG_STRINGS` undefined), so
/// its log is a reference read at delivery and the post copies nothing.
#[derive(Debug, Clone, PartialEq)]
pub enum PostedValue {
    Val(EpicsValue),
    Ref,
}

impl PostedValue {
    /// The post of a walk's read: a copy of `read`, or a reference for a
    /// field the walk does not read (`None`). Out of line, so each post site
    /// in the hot walk is a call.
    #[inline(never)]
    pub(crate) fn of_read(read: &Option<EpicsValue>) -> Self {
        match read {
            Some(val) => Self::Val(val.clone()),
            None => Self::Ref,
        }
    }

    /// The value posted, `None` for a field read at delivery.
    pub fn as_value(&self) -> Option<&EpicsValue> {
        match self {
            Self::Val(v) => Some(v),
            Self::Ref => None,
        }
    }
}

pub type FieldPost = (PostName, PostedValue, crate::server::recgbl::EventMask);

/// The field a post names. `Declared` is the field's own [`FieldDesc`], so
/// two declared fields are one field exactly when they are one pointer — what
/// lets a post find its subscribers and its published entry without reading a
/// byte of either name.
#[derive(Debug, Clone)]
pub enum PostName {
    Declared(&'static FieldDesc),
    Named(std::borrow::Cow<'static, str>),
}

impl std::ops::Deref for PostName {
    type Target = str;

    fn deref(&self) -> &str {
        match self {
            Self::Declared(desc) => desc.name,
            Self::Named(name) => name,
        }
    }
}

impl AsRef<str> for PostName {
    fn as_ref(&self) -> &str {
        self
    }
}

impl PartialEq for PostName {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl PartialEq<str> for PostName {
    fn eq(&self, other: &str) -> bool {
        **self == *other
    }
}

impl PartialEq<&str> for PostName {
    fn eq(&self, other: &&str) -> bool {
        **self == **other
    }
}

impl PartialEq<std::borrow::Cow<'_, str>> for PostName {
    fn eq(&self, other: &std::borrow::Cow<'_, str>) -> bool {
        **self == **other
    }
}

impl From<&'static str> for PostName {
    fn from(name: &'static str) -> Self {
        Self::Named(name.into())
    }
}

impl From<String> for PostName {
    fn from(name: String) -> Self {
        Self::Named(name.into())
    }
}

/// Snapshot of changes from a process cycle, used for notify outside lock.
///
/// The first post is held inline and the rest spill to a `Vec`, the same shape
/// as [`ProcStack`](crate::server::database::ProcStack) and for the same
/// reason: the overwhelmingly common cycle posts exactly one field — the
/// record's deadband-tracked value, with nothing subscribed behind it — and a
/// `Vec` charged that cycle a 256-byte allocation and its free. It was the last
/// per-cycle allocation on the process path.
///
/// `head` and `rest` carry no meaning of their own: `head` is post 0 and `rest`
/// is posts 1.., so nothing may read them apart from [`Self::iter`].
///
/// A snapshot with nothing to free drops as one inline test of `owns_heap`,
/// as [`CycleList`] does; drop glue over the two fields was an out-of-line
/// call on every cycle, and decoding the head post's variants there cost a
/// monitored cycle more than the call.
#[derive(Default)]
pub struct ProcessSnapshot(std::mem::ManuallyDrop<SnapshotPosts>);

#[derive(Default)]
struct SnapshotPosts {
    head: Option<FieldPost>,
    rest: Vec<FieldPost>,
    /// The classes of the posts made with no subscriber to deliver to.
    unwatched: crate::server::recgbl::EventMask,
    /// A post was pushed that frees something on drop, or spilled to `rest`.
    owns_heap: bool,
}

impl Drop for ProcessSnapshot {
    #[inline]
    fn drop(&mut self) {
        if self.0.owns_heap {
            Self::release(&mut self.0);
        }
    }
}

/// Whether dropping `post` frees anything. A monitored cycle's one post is
/// a declared field's scalar, and its drop glue was a call per cycle.
#[inline]
fn owns_heap(post: &FieldPost) -> bool {
    use EpicsValue as V;
    !matches!(
        post,
        (
            PostName::Declared(_),
            PostedValue::Ref
                | PostedValue::Val(
                    V::Short(_)
                        | V::Float(_)
                        | V::Enum(_)
                        | V::Char(_)
                        | V::Long(_)
                        | V::Double(_)
                        | V::Int64(_)
                        | V::UInt64(_)
                        | V::UShort(_)
                        | V::ULong(_)
                        | V::UChar(_)
                ),
            _,
        )
    )
}

#[cfg(test)]
thread_local! {
    static RELEASES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl ProcessSnapshot {
    /// A cycle that posted nothing.
    pub fn new() -> Self {
        Self::default()
    }

    #[cold]
    #[inline(never)]
    fn release(posts: &mut std::mem::ManuallyDrop<SnapshotPosts>) {
        #[cfg(test)]
        RELEASES.with(|n| n.set(n.get() + 1));
        // SAFETY: called once, from `drop`, on a field nothing else drops.
        unsafe { std::mem::ManuallyDrop::drop(posts) }
    }

    /// Add one post, after every post already added.
    pub fn push(&mut self, post: FieldPost) {
        match self.0.head {
            None => {
                self.0.owns_heap |= owns_heap(&post);
                self.0.head = Some(post);
            }
            Some(_) => {
                self.0.owns_heap = true;
                self.0.rest.push(post);
            }
        }
    }

    /// A post nothing is subscribed to: it publishes `mask`, which a CP
    /// edge reads through [`Self::published_mask`], and has no value to
    /// deliver, so its value is never read.
    pub fn push_unwatched(&mut self, mask: crate::server::recgbl::EventMask) {
        self.0.unwatched |= mask;
    }

    /// Whether this cycle has no post to deliver.
    pub fn is_empty(&self) -> bool {
        self.0.head.is_none()
    }

    /// How many fields this cycle posts.
    pub fn len(&self) -> usize {
        usize::from(self.0.head.is_some()) + self.0.rest.len()
    }

    /// The cycle's posts, in the order they were added.
    pub fn iter(&self) -> impl Iterator<Item = &FieldPost> {
        self.0.head.iter().chain(self.0.rest.iter())
    }

    /// Keep the posts before `first`, and of the rest only those `keep`
    /// accepts; `keep` sees each of the rest once, in the order they were
    /// added. A cycle whose walk added nothing returns before any call.
    #[inline]
    pub fn retain_from(&mut self, first: usize, mut keep: impl FnMut(&FieldPost) -> bool) {
        if first >= self.len() {
            return;
        }
        let rest_from = match first {
            0 => {
                if self.0.head.as_ref().is_some_and(|post| !keep(post)) {
                    self.0.head = None;
                }
                0
            }
            _ => first - 1,
        };
        let mut kept = rest_from;
        for i in rest_from..self.0.rest.len() {
            if keep(&self.0.rest[i]) {
                self.0.rest.swap(kept, i);
                kept += 1;
            }
        }
        self.0.rest.truncate(kept);
        if self.0.head.is_none() && !self.0.rest.is_empty() {
            self.0.head = Some(self.0.rest.remove(0));
        }
    }

    /// The union of every `DBE_*` class this cycle actually published — the
    /// port's answer to "what did `db_post_events` send for this record".
    ///
    /// A field carrying an empty mask is not posted at all
    /// (`RecordInstance::notify_from_snapshot` skips it), so an empty union
    /// means the cycle published nothing and no monitor of any class fired.
    pub fn published_mask(&self) -> crate::server::recgbl::EventMask {
        self.iter()
            .fold(self.0.unwatched, |acc, (_, _, m)| acc | *m)
    }
}

impl Extend<FieldPost> for ProcessSnapshot {
    fn extend<I: IntoIterator<Item = FieldPost>>(&mut self, iter: I) {
        for post in iter {
            self.push(post);
        }
    }
}

impl FromIterator<FieldPost> for ProcessSnapshot {
    fn from_iter<I: IntoIterator<Item = FieldPost>>(iter: I) -> Self {
        let mut out = Self::new();
        out.extend(iter);
        out
    }
}

/// What C's `fetch_values()` does when one of the record's input links fails
/// to read, and whether that failure gates the record body.
///
/// Every C record with an INPA..INPx block has a `fetch_values()` helper, but
/// they do not share a failure shape, so the framework cannot pick one rule
/// for all of them — each record declares its own via
/// [`Record::input_fetch_policy`].
///
/// The two dimensions C varies are "does the loop stop at the first failure"
/// and "does a failure gate the record body", and it uses three of the four
/// combinations. Whichever variant a record picks, the framework reduces the
/// cycle to ONE outcome — C's `fetch_values()` return status, zero or not —
/// and delivers it through a single owner: [`Record::set_fetch_gate_failed`]
/// for records that compute in their own `process()`, and
/// `RecordInstance::suppress_subroutine_run` for the two whose body is a
/// framework-dispatched subroutine (sub/aSub).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InputFetchPolicy {
    /// Read every configured link; a failed read neither stops the loop nor
    /// gates the record body. C `transformRecord.c::process` (534-547) reads
    /// on through a failed `dbGetLink` and computes anyway.
    #[default]
    ReadAll,
    /// Read every configured link — a failure does NOT stop the loop, so the
    /// inputs behind it still refresh — but the body is skipped this cycle.
    ///
    /// C `calcRecord.c::fetch_values` (427-443) keeps the FIRST failing status
    /// while looping to the end (`if (status == 0) status = newStatus;`), and
    /// `calcRecord.c::process` (120) runs `calcPerform` only
    /// `if (fetch_values(prec) == 0)` — so VAL and UDF freeze, no CALC_ALARM is
    /// raised, and everything after the calc (timestamp, alarms, monitors,
    /// forward link) still runs. `calcoutRecord.c` (694-709 fetch, 237 gate) is
    /// the same shape, and its OOPT decision then runs against the frozen VAL.
    ReadAllGateOnFailure,
    /// Stop at the FIRST failed link and skip the record body this cycle.
    ///
    /// C `subRecord.c::fetch_values` (407-418) `return -1`s on the first
    /// failing `dbGetLink`, so the inputs behind it are never read and keep
    /// their previous values; `subRecord.c::process` (144 fetch, 147 gate)
    /// then runs `do_sub` only `if (status == 0)`, freezing VAL/UDF and raising
    /// none of the subroutine's alarms. `aSubRecord.c` (277-289 fetch, 216-218
    /// process), `sCalcoutRecord.c` (885-887 fetch, 356 gate),
    /// `aCalcoutRecord.c` (1068-1071 fetch, 399 gate) and
    /// `swaitRecord.c` (686-705 fetch, 408 gate) are the same shape.
    AbortOnFirstFailure,
    /// Read every configured link and gate the body on the LAST link's status.
    ///
    /// C `selRecord.c::fetch_values` (434-437) assigns `status` unguarded on
    /// every pass of its all-inputs loop and returns it, so what
    /// `selRecord.c::process` (114-116) gates `do_sel` on is INPL's status
    /// alone — a failed INPA is read, posted, and then ignored. In
    /// `Specified` mode the loop is one link (`:421-432`), so the same rule
    /// reads as "the selected input's status". Faithful to C, quirk included:
    /// see `sel`'s doc for why the quirk is C's and not ours.
    ReadAllGateOnLastFailure,
}

/// **The** field declaration of a record type, and the only way to obtain one.
///
/// Not implementable: the blanket `impl` below covers every [`Record`], so a
/// second `impl FieldDeclaration for MyRecord` is a coherence error. A record
/// type therefore cannot *supply* a field list — it can only be *asked* for
/// one, and the answer is resolved here:
///
/// * a record type **base's** vendored `.dbd` set covers is declared by the
///   table generated from that `.dbd` ([`crate::server::record::dbd_generated::record_fields`]);
/// * any other record type is asked for its own declaration,
///   [`Record::declared_fields`] — which for the downstream record types is the
///   table generated from the `.dbd` *their* crate vendors, and for a synthetic
///   record type (tests) is a hand-written table.
///
/// The two are mutually exclusive by construction, which is what closes the
/// invariant *one declaration per record type*. It used to be closed by luck:
/// both tables were live, `field_desc_of` merely happened to consult the
/// generated one first, and every consumer that reached for `field_list()`
/// directly (`dbpr`, the `dbpf` typo hint, `motor`'s field gate) read the
/// hand-written one — which is how `waveform.FTVL` was declared `DBF_SHORT`
/// with no menu while `waveformRecord.dbd` said `DBF_MENU`/`menu(menuFtype)`.
/// `field`'s offset into a contiguous argument block whose members are the
/// single letters `A`..`A+nargs-1` behind `prefix` — C's `get_linkNumber`
/// index test (`idx >= indexof(A) && idx < indexof(A) + NARGS`) written as a
/// letter test, which is the same set because the `.dbd` declares those
/// fields contiguously in letter order (`calcRecord.dbd.pod:792-980`).
///
/// `prefix` is `""` for the plain arg letters, `"L"` for calc's previous-value
/// twins `LA`..`LU`, `"VAL"` for aSub's output slots `VALA`..`VALU`.
pub fn arg_letter_offset(field: &str, prefix: &str, nargs: u8) -> Option<u8> {
    let rest = field.strip_prefix(prefix)?;
    let &[c] = rest.as_bytes() else { return None };
    if !c.is_ascii_uppercase() {
        return None;
    }
    let n = c - b'A';
    (n < nargs).then_some(n)
}

/// The `nargs`-wide link field named by [`arg_letter_offset`]'s answer:
/// offset 0 behind `"INP"` is `"INPA"`. The one place the port turns a C
/// `&prec->inpa + linkNumber` pointer walk into a field name.
pub fn arg_link_field(prefix: &str, offset: u8) -> String {
    format!("{prefix}{}", (b'A' + offset) as char)
}

/// C `CALCPERFORM_NARGS` (`postfix.h:29`) and `subRecord.c:89`'s
/// `INP_ARG_MAX` — the same 21, which is why calc, calcout and sub share one
/// arg-link mapping.
pub const CALC_CLASS_NARGS: u8 = 21;

/// The calc-class `get_linkNumber` mapping (`calcRecord.c:161-167`,
/// `calcoutRecord.c:417-423`, `subRecord.c:198-204`): the arg letters
/// `A`..`U` AND their previous-value twins `LA`..`LU` both index
/// `&prec->inpa + n`, so both answer their metadata from `INPn`.
///
/// `sel` names its args the same way and is deliberately NOT routed here: its
/// rset lists them explicitly on HOPR/LOPR and calls `dbGetGraphicLimits`
/// nowhere.
pub fn calc_class_link_backed_metadata_field(field: &str) -> Option<String> {
    arg_letter_offset(field, "", CALC_CLASS_NARGS)
        .or_else(|| arg_letter_offset(field, "L", CALC_CLASS_NARGS))
        .map(|n| arg_link_field("INP", n))
}

/// [`FieldDesc::long_string`] of `field` in `fields`. The flag is tested
/// first, so only a long-string row pays a name compare.
pub(crate) fn long_string_in(fields: &[FieldDesc], field: &str) -> bool {
    fields
        .iter()
        .any(|d| d.long_string && d.name.eq_ignore_ascii_case(field))
}

pub trait FieldDeclaration {
    /// The record type's field descriptors, in `.dbd` declaration order.
    fn field_list(&self) -> &'static [FieldDesc];

    /// The record-own `DBF_NOACCESS` internal names the generator dropped
    /// from [`FieldDeclaration::field_list`] (`BPTR`, `RPVT`, ...): names
    /// C's `dbNameToAddr` resolves — so a SEARCH is answered — but whose
    /// channel is refused at creation. Same base-table-then-own resolution
    /// as `field_list`.
    fn noaccess_names(&self) -> &'static [&'static str];

    /// The channel's native (maximum) element count for `field`, when it
    /// differs from the count of the field's current value — C's `cvt_dbaddr`
    /// `paddr->no_elements` against `get_array_info`'s current valid length, so
    /// a client's `ca_element_count` is the capacity even though a GET returns
    /// fewer elements.
    ///
    /// Only a `special(SPC_DBADDR)` field reaches `cvt_dbaddr` in C, and the
    /// `.dbd` is what says which fields those are. This reads that from
    /// [`FieldDeclaration::field_list`] and only then asks the record for the
    /// number ([`Record::dbaddr_capacity`]) — so the population comes from the
    /// one declaration every consumer already goes through, and a record cannot
    /// advertise a capacity for a field C would never have handed one.
    ///
    /// This is C's `cvt_dbaddr` RUNNING — a channel resolving on `field` — so
    /// it first gives the record its [`Record::cvt_dbaddr`]. A caller that
    /// only needs the number asks [`Self::dbaddr_count`].
    fn field_native_count(&self, field: &str) -> Option<u32>;

    /// [`Self::field_native_count`]'s number, with no resolve behind it: the
    /// load-time settlement of the lock-free `dbNameToAddr` counts asks every
    /// `SPC_DBADDR` field at once, which C never does.
    fn dbaddr_count(&self, field: &str) -> Option<u32>;

    /// C `paddr->pfldDes->special == SPC_DBADDR` — the field's STATIC `.dbd`
    /// declaration, and the single owner of "is this an ARRAY destination".
    ///
    /// `dbPut` reads exactly this to pick its array arm
    /// (`dbAccess.c:1350`, tag `R7.0.10`:
    /// `if (nRequest>1 || paddr->pfldDes->special == SPC_DBADDR)`), and it is
    /// `pfldDes` — the shared field descriptor — NOT the per-`DBADDR`
    /// `paddr->special` that `cvt_dbaddr` is free to overwrite. The two differ:
    /// `lsiRecord.c:127-134` raises `paddr->special` to `SPC_MOD` on VAL and
    /// `SPC_NOMOD` on OVAL, and the branch still takes the array arm.
    ///
    /// That distinction is why this reads [`FieldDesc::declared_special`]:
    /// [`FieldDesc::special`] carries the `cvt_dbaddr` special, which is no
    /// longer `SPC_DBADDR` on the five long-string fields.
    fn field_is_dbaddr(&self, field: &str) -> bool;

    /// [`FieldDesc::long_string`] of `field`.
    fn field_is_long_string(&self, field: &str) -> bool;

    /// Whether the port stores `field` as a buffer — what `get_field(field)`
    /// would answer with `is_array()`, without cloning the value to ask. Only
    /// a [`Self::field_is_dbaddr`] field holds one; among those a record
    /// names the scalar-stored ones in [`Record::dbaddr_stored_as_scalar`].
    /// A type without a generated `.dbd` still asks its value.
    fn field_stores_array(&self, field: &str) -> bool;
}

impl<R: Record + ?Sized> FieldDeclaration for R {
    fn field_list(&self) -> &'static [FieldDesc] {
        super::dbd_generated::record_fields(self.record_type())
            .unwrap_or_else(|| self.declared_fields())
    }

    fn noaccess_names(&self) -> &'static [&'static str] {
        super::dbd_generated::record_noaccess_fields(self.record_type())
            .unwrap_or_else(|| self.declared_noaccess_fields())
    }

    fn field_native_count(&self, field: &str) -> Option<u32> {
        if !self.field_is_dbaddr(field) {
            return None;
        }
        self.cvt_dbaddr(field);
        self.dbaddr_capacity(field)
    }

    fn dbaddr_count(&self, field: &str) -> Option<u32> {
        if !self.field_is_dbaddr(field) {
            return None;
        }
        self.dbaddr_capacity(field)
    }

    fn field_is_dbaddr(&self, field: &str) -> bool {
        // Every put asks this; a scan of the whole table cost ~690
        // instructions on a calc target.
        if let Some(names) = super::dbd_generated::record_dbaddr_fields(self.record_type())
            .or_else(|| self.declared_dbaddr_fields())
        {
            return names.iter().any(|n| n.eq_ignore_ascii_case(field));
        }
        self.field_list()
            .iter()
            .any(|d| d.name.eq_ignore_ascii_case(field) && d.declared_special == Special::DbAddr)
    }

    fn field_is_long_string(&self, field: &str) -> bool {
        long_string_in(self.field_list(), field)
    }

    fn field_stores_array(&self, field: &str) -> bool {
        if self.field_is_dbaddr(field) {
            return !self.dbaddr_stored_as_scalar(field);
        }
        if super::dbd_generated::record_fields(self.record_type()).is_some() {
            return false;
        }
        // A hand-written declaration need not mark its buffers `SPC_DBADDR`:
        // the stored value is the only witness.
        self.get_field(field).is_some_and(|v| v.is_array())
    }
}

/// Trait that all EPICS record types must implement.
/// The per-cycle report of which input links produced a value — C's
/// `RTN_SUCCESS(dbGetLink(..))` per link, as [`Record::set_resolved_input_links`]
/// is handed it. It is the bits the multi-input fetch set over the record's
/// own [`Record::multi_input_links`] and the names the pre-process reads
/// resolved, so making it costs a cycle nothing and asking it is one scan
/// of a static list; no name list is built for the types that ignore it.
#[derive(Clone, Copy)]
pub struct ResolvedInputLinks<'a> {
    /// The record's `multi_input_links`, the slots `mask` covers.
    multi: &'static [(&'static str, &'static str)],
    mask: u64,
    /// The pre-process `ReadDbLink` fields that resolved.
    pre: &'a [&'static str],
}

impl<'a> ResolvedInputLinks<'a> {
    pub(crate) fn new(
        multi: &'static [(&'static str, &'static str)],
        mask: u64,
        pre: &'a [&'static str],
    ) -> Self {
        ResolvedInputLinks { multi, mask, pre }
    }

    /// A report of these link fields alone — for a test that plays the
    /// framework.
    pub fn of_names(names: &'a [&'static str]) -> Self {
        ResolvedInputLinks {
            multi: &[],
            mask: 0,
            pre: names,
        }
    }

    /// Whether `link_field`'s read produced a value this cycle.
    pub fn contains(&self, link_field: &str) -> bool {
        self.multi_names().any(|name| name == link_field) || self.pre.contains(&link_field)
    }

    /// Every link field whose read produced a value this cycle.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + 'a {
        self.multi_names().chain(self.pre.iter().copied())
    }

    fn multi_names(&self) -> impl Iterator<Item = &'static str> + 'a {
        let multi = self.multi;
        let mut mask = self.mask;
        std::iter::from_fn(move || {
            if mask == 0 {
                return None;
            }
            let slot = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            Some(multi[slot].0)
        })
    }
}

/// The record's SIMM, SIMS and SDLY cells, read as C reads them off the
/// record struct: three members, no field-name lookup and no
/// [`EpicsValue`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimmCells {
    pub simm: i16,
    pub sims: i16,
    pub sdly: f64,
}

/// The `&mut dyn Any` of a sized record, which [`Record::as_any_mut`]'s
/// default reaches through the vtable; sealed in this module.
pub trait RecordAny: std::any::Any {
    fn record_any_mut(&mut self) -> &mut dyn std::any::Any;
}

impl<T: std::any::Any> RecordAny for T {
    fn record_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

pub trait Record: RecordAny + Send + Sync + 'static {
    /// Return the record type name (e.g., "ai", "ao", "bi").
    fn record_type(&self) -> &'static str;

    /// Process the record (scan/compute cycle).
    ///
    /// Returns a `ProcessOutcome` containing the processing result and any
    /// side-effect actions for the framework to execute.
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }

    /// Optional: report whether this record's last `process()` call
    /// mutated a metadata-class field (EGU/PREC/HOPR/LOPR/HLM/LLM/
    /// alarm limits / DRVH/DRVL / state strings).
    ///
    /// The framework checks this after every `process()` call and, if
    /// true, invalidates the record's metadata cache so the next
    /// snapshot rebuilds from the new values.
    ///
    /// Default: `false` — most records never touch metadata fields
    /// during processing. Override only when your record dynamically
    /// adjusts limits or unit strings (e.g., a motor that recomputes
    /// HLM/LLM after a hardware homing operation).
    ///
    /// Implementations should reset their internal flag after returning
    /// `true` so the next cycle starts clean.
    fn took_metadata_change(&mut self) -> bool {
        false
    }

    /// Get a field value by name.
    fn get_field(&self, name: &str) -> Option<EpicsValue>;

    /// Set a field value by name.
    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()>;

    /// A `.db` `field()` of a field the record stores itself: what C's
    /// `dbPutString` writes and nothing more, so a buffer sized by the field
    /// is sized in `init_record`, as C sizes it. The default is
    /// [`Self::put_field`], for a type whose arms only store.
    fn load_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        self.put_field(name, value)
    }

    /// The field declaration of a record type **base's** `.dbd` set does not
    /// cover — a record type that lives in another crate.
    ///
    /// C has exactly one declaration per record type: the `.dbd`, read at
    /// runtime. The port compiles the vendored `.dbd`s into a generated table,
    /// and [`FieldDeclaration::field_list`] — the single resolver every consumer
    /// goes through — serves base's
    /// [`dbd_generated`](crate::server::record::dbd_generated) table for every
    /// record type IT covers and **never falls through to here**. So for a
    /// base record type this method is unreachable: it cannot declare one of its
    /// fields a second time, whatever it writes here.
    ///
    /// A record type outside base declares itself here, and the answer is still
    /// its `.dbd`: the downstream Tier-3 record types (`motor`, `table`,
    /// `scaler`, `epid`, `throttle`, `timestamp`) vendor their upstream `.dbd`
    /// into their OWN crate, `tools/dbd-codegen` generates a table into that
    /// crate (`tools/dbd-codegen/src/targets.rs`), and this method returns it.
    /// Each such crate carries the same ratchet base does
    /// (`one_declaration_per_record_type`): the table returned here must BE the
    /// generated one, so the `.dbd` stays the single declaration across a crate
    /// boundary the generator's output cannot cross on its own.
    ///
    /// A hand-written table is what is left when a record type has no `.dbd`
    /// anywhere — the synthetic record types the tests define. There are no
    /// others; a shipped record type has a `.dbd`, and its declaration is that
    /// `.dbd`.
    ///
    /// The declaration is a *spec*: it says what each field's type, menu and
    /// `special(SPC_NOMOD)` are. It does NOT say who implements the field. Ask
    /// [`Record::implements_field`] for that — see its docs for why the two
    /// must not be the same question.
    fn declared_fields(&self) -> &'static [FieldDesc] {
        &[]
    }

    /// The analogue of [`Record::declared_fields`] for
    /// [`FieldDeclaration::noaccess_names`]: the record-own `DBF_NOACCESS`
    /// internal names of a record type no base `.dbd` declares. A downstream
    /// crate whose generated table reports dropped internals returns its
    /// `record_noaccess_fields` entry here, next to its `declared_fields`.
    fn declared_noaccess_fields(&self) -> &'static [&'static str] {
        &[]
    }

    /// The analogue of [`Record::declared_fields`] for
    /// [`FieldDeclaration::field_is_dbaddr`]: a downstream crate's generated
    /// `<T>_DBADDR`. `None` leaves a hand-written type to the field-table walk.
    fn declared_dbaddr_fields(&self) -> Option<&'static [&'static str]> {
        None
    }

    /// Does this record type implement `name` in its own `get_field` /
    /// `put_field`, as opposed to leaving it to the framework's dbCommon
    /// handling?
    ///
    /// This used to be answered by `field_list()` membership, which conflated
    /// two questions: "what is this field?" and "who owns it?". They give
    /// different answers for `INP`/`OUT`: every record type *declares* them in
    /// the `.dbd`, but only some drive the link themselves
    /// (`multi_output_links` for `acalcout`/`scalcout`, device support for
    /// `motorRecord`/`scalerRecord`); for the rest the framework arms
    /// `parsed_inp`/`parsed_out` and drives it. While `field_list()` was
    /// hand-written and incomplete the conflation was invisible, because the
    /// hand-written tables happened to omit exactly the fields the framework
    /// owns. A complete, spec-derived `field_list()` makes membership true for
    /// every record, so ownership needs its own predicate or the framework
    /// would stop arming any link at all.
    ///
    /// The default answers it truthfully — a record implements the fields its
    /// own `get_field` can produce. (Verified equivalent to the old
    /// `field_list()` membership on all 1,757 fields of all 40 record types at
    /// the time of the split, so the split changed no behaviour.)
    fn implements_field(&self, name: &str) -> bool {
        self.get_field(name).is_some()
    }

    /// `SPC_NOMOD` that a record's `cvt_dbaddr` decides **at runtime, from
    /// record state** — the dynamic half of the no-modify declaration.
    ///
    /// [`FieldDesc::read_only`] is the static half: it carries the `.dbd`
    /// `special(SPC_NOMOD)` of a field that is immutable for the record type,
    /// full stop. But C lets a record's `cvt_dbaddr` *raise* SPC_NOMOD per
    /// dbAddr, keyed on the record's own fields, and one record does:
    ///
    /// ```c
    /// /* compressRecord.c:395-407 */
    /// static long cvt_dbaddr(DBADDR *paddr) {
    ///     ...
    ///     if (prec->balg == bufferingALG_LIFO)
    ///         paddr->special = SPC_NOMOD;
    /// }
    /// ```
    ///
    /// A compress VAL is writable under BALG=FIFO and refused under BALG=LIFO —
    /// a per-record-state fact no static `FieldDesc` can express. The one gate
    /// that owns field immutability (`field_io::check_no_mod`) consults this
    /// hook alongside the static set, so the dynamic refusal reaches EVERY put
    /// route exactly as the static one does.
    ///
    /// (C caches `paddr->special` in the DBADDR at name-resolution time, so a
    /// CA channel opened while FIFO keeps writing after a switch to LIFO until
    /// it reconnects; `dbpf`, which resolves fresh, is refused immediately. The
    /// port evaluates live on every put — the invariant C's own `dbpf` path
    /// shows, without the stale-cache hole.)
    ///
    /// `field` is upper-case. Default: no dynamic NOMOD.
    fn field_no_mod(&self, _field: &str) -> bool {
        false
    }

    /// Choice strings for a record-specific `DBF_MENU` field served as
    /// `DBR_ENUM`, keyed by field name (uppercase, as declared).
    ///
    /// EPICS dbStaticLib serves a `DBF_MENU` field as `DBR_ENUM`: the value
    /// is the menu index and the field carries its `menu()` choice strings,
    /// so `caget`/`pvget` present the labels rather than a bare number
    /// (`dbStaticLib.c` `dbGetMenuChoices`; `dbAccess.c` `get_enum_str`).
    /// A record returns the label table (in index order) for each field it
    /// serves as [`DbFieldType::Enum`] from a `menu()`; the framework
    /// attaches it to the field snapshot's `EnumInfo` so the CA/PVA enum
    /// encoders present the labels — the same mechanism `bi`/`bo`/`mbbi`/
    /// `mbbo` already use for their `VAL` state strings, but per field
    /// rather than per record (a record can carry several distinct menus).
    ///
    /// This is the single owner of "menu field -> choice table": a record
    /// declares its menu fields here once, and `get_field` returns the menu
    /// index as [`EpicsValue::Enum`]. Default: no record-specific menu
    /// fields. The dbCommon menu fields (`SCAN`, etc.) are handled
    /// separately by the framework, not here.
    ///
    /// INVARIANT — answering here is a claim that the field is `DBF_MENU`, so
    /// [`FieldDeclaration::field_list`] MUST declare it [`DbFieldType::Enum`]: C's
    /// `mapDBFToDBR` serves every `DBF_MENU` as `DBR_ENUM`, and the DECLARED
    /// type is what goes on the wire
    /// ([`RecordInstance::project_to_declared_type`](super::RecordInstance::project_to_declared_type)).
    /// A field with choices but a `Short` declaration is a self-contradictory
    /// declaration: it would be served as a bare `DBR_SHORT` index while
    /// claiming to have labels for it. `menu_choices_are_served_as_dbr_enum`
    /// (`tests/menu_fields_serve_enum_choices.rs`) fails on any record type
    /// that breaks it — the storage may be a short, the declaration may not.
    fn menu_field_choices(&self, _field: &str) -> Option<&'static [&'static str]> {
        None
    }

    /// Per-field override of the record-level display/control metadata
    /// for a GET / monitor snapshot of `field`.
    ///
    /// C record support serves metadata PER FIELD: the RSET functions
    /// `get_units` / `get_precision` / `get_graphic_double` /
    /// `get_control_double` / `get_alarm_double` all key on
    /// `dbGetFieldIndex(paddr)` and fall back to the `recGbl*` defaults
    /// for unlisted fields. The framework's metadata cache is per
    /// record (built by `populate_display_info` /
    /// `populate_control_info` from the VAL-class fields); a record
    /// whose RSET serves different metadata for non-VAL fields
    /// overrides this hook to patch the cached values for that field
    /// (e.g. the motor record: VELO's display range is VMAX/VBAS, not
    /// HLM/LLM — `motorRecord.cc:3247-3250`).
    ///
    /// Applied to every served read (`snapshot_for_field`), a monitor
    /// update's delivery read included, AFTER the cached
    /// record-level metadata — and computed live on each call, so an
    /// override derived from non-cached fields can never go stale.
    /// `field` is uppercase, as declared in [`FieldDeclaration::field_list`].
    /// Default: `None` — record-level metadata serves every field.
    fn field_metadata_override(&self, _field: &str) -> Option<FieldMetadataOverride> {
        None
    }

    /// Which of THIS record's own link fields C's rset reads `field`'s
    /// `get_units` / `get_precision` / `get_graphic_double` /
    /// `get_alarm_double` from — C's `get_linkNumber` (`calcRecord.c:161-167`,
    /// `calcoutRecord.c:417-423`, `subRecord.c:198-204`), `get_dol`
    /// (`seqRecord.c:279-280`) and aSub's `get_inlinkNumber` /
    /// `get_outlinkNumber` pair (`aSubRecord.c:294-304`).
    ///
    /// Twenty-four call sites across five record types answer four of the six
    /// metadata slots from a LINK rather than from the record — the target's
    /// EGU, PREC, HOPR/LOPR and alarm ladder, not the source's. Control is
    /// deliberately absent: `dbGetControlLimits` has no caller in all of base,
    /// and `aSubRecord.c:372-376` is the type specimen calling
    /// `recGblGetControlDouble` with no link branch.
    ///
    /// The record type owns this answer for the same reason it owns
    /// [`Self::property_support`]: it is a transcription of the record's own C
    /// rset, and a central table keyed on the record-type *string* silently
    /// answers "no link" for every type nobody remembered to add. `aSub` was
    /// exactly that omission — its eight sites are the largest group of the
    /// twenty-four and it alone routes OUT links, a shape a
    /// `match rtype { "calc" | "calcout" | "sub" => ... }` cannot express at
    /// all.
    ///
    /// Returns the link field's name as declared (`"INPA"`, `"OUTC"`,
    /// `"DOL3"`), uppercase. Default: `None` — the record answers every field
    /// from itself.
    fn link_backed_metadata_field(&self, _field: &str) -> Option<String> {
        None
    }

    /// Which of C's six nullable `rset` `get_*` property slots THIS record
    /// type implements — the record's own `#define get_xxx NULL` lines.
    ///
    /// `dbGet` consults the rset to *narrow* the caller's options mask
    /// (`dbAccess.c:336-427`): a NULL slot clears the option bit, so the leaf
    /// never reaches the client. That is what decides whether QSRV marks an NT
    /// leaf at all (pvxs `ioc/iocsource.cpp:263-305`). A slot counts as
    /// supplied when the C function pointer is non-NULL **even if the function
    /// writes nothing for the field in question** — C leaves the option bit
    /// set either way (`boRecord.c:294-299` writes units only for `HIGH`, yet
    /// `DBR_UNITS` survives for every `bo` field).
    ///
    /// The record type owns this answer because the record type owns its C
    /// rset. A central table keyed on the record-type *string* cannot: a
    /// record implemented in a downstream crate (`asyn-rs`'s `asynRecord`,
    /// the motor/scaler/optics types) has no way to add a row to it, so it
    /// silently inherited a default that marked every leaf it could name —
    /// telling clients a fabricated `display.units` of `""` and
    /// `valueAlarm` bands of zero were authoritative.
    ///
    /// The default answers from `default_property_support`, the
    /// transcription of the record types epics-base-rs implements itself.
    /// Override it in the record's own file, citing the C rset lines.
    fn property_support(&self) -> crate::server::snapshot::PropertySupport {
        default_property_support(self.record_type())
    }

    /// Field names declared `pp(TRUE)` in this record type's DBD (empty if
    /// none, e.g. `event`/`histogram`, or if the type has no declaration).
    ///
    /// Drives the `dbPutField` processing gate: C `dbAccess.c:1263`
    /// re-processes a record on a put only when the put field is `PROC` or it
    /// is `pp(TRUE)` **and** `SCAN == Passive`. The default reads `pp` from
    /// [`FieldDeclaration::field_list`], so a downstream type's generated
    /// `declared_fields` carries it; only a synthetic type with no
    /// declaration overrides this.
    fn process_passive_fields(&self) -> &'static [&'static str] {
        super::process_passive::pp_fields_of(self.field_list())
    }

    /// Whether a put to `field` should reprocess this Passive record, given
    /// `pp`: whether `field` is in [`Self::process_passive_fields`], which the
    /// put gate reads from the set its cell cached at creation.
    ///
    /// The default is `pp` itself. A record type overrides this
    /// when its C `special()` conditionally returns ERROR to suppress the
    /// reprocess for a `pp(TRUE)` field on certain values (e.g. motor STUP:
    /// only a `STUP == ON` put runs the status-update process; any other value
    /// is clamped to OFF and C returns ERROR so no process runs). Modeling that
    /// here keeps the suppression at the same gate as the pp test, with no
    /// per-put one-shot state — the post-clamp field value is deterministic.
    fn processes_after_put(&self, field: &str, pp: bool) -> bool {
        let _ = field;
        pp
    }

    /// The record's `DBF_ENUM` state strings — the C rset slot
    /// `get_enum_strs`: what a client reads as the `DBR_ENUM` choice list, and
    /// the `no_str` bound a numeric `DBR_STRING` put is held to. The names a
    /// put may match are [`Self::enum_str_state`]'s, which C scans untrimmed.
    ///
    /// The table is already trimmed to C's `no_str`: `bi`/`bo`/`busy` drop
    /// `ONAM` when `ZNAM` is set and `ONAM` is empty (`boRecord.c:342-352`);
    /// `mbbi`/`mbbo` cut at the last non-empty state (`mbbiRecord.c:262-269`).
    ///
    /// `None` — the record type leaves both rset slots NULL. That is every
    /// record whose `VAL` is not `DBF_ENUM`, and `mbbiDirect`/`mbboDirect`
    /// (`mbbiDirectRecord.c:58` `#define put_enum_str NULL`), whose `VAL` is
    /// `DBF_LONG`. C then fails a `DBR_STRING` put with `S_db_noRSET`.
    fn enum_state_strings(&self) -> Option<Vec<PvString>> {
        None
    }

    /// The record's `put_enum_str` rset slot: the state `name` selects, or
    /// `None` (`S_db_badChoice`). Only called when [`Self::enum_state_strings`]
    /// is `Some`. Default: the label list, for a record with no slots of its
    /// own to scan.
    fn enum_str_state(&self, name: &PvString) -> Option<u16> {
        self.enum_state_strings()?
            .iter()
            .position(|state| state.as_bytes() == name.as_bytes())
            .map(|i| i as u16)
    }

    /// Whether that slot's store also clears UDF. C's bi and mbbi do
    /// (`biRecord.c:227`, `mbbiRecord.c:284`); bo, mbbo and busy store VAL
    /// alone (`boRecord.c:360-368`, `mbboRecord.c:354-371`,
    /// `busyRecord.c:321-329`).
    fn enum_str_clears_udf(&self) -> bool {
        false
    }

    /// The record's `get_enum_str` rset slot — how a `DBR_STRING` READ of its
    /// `DBF_ENUM` `VAL` renders. A THIRD slot, distinct from the pair above:
    /// C's `get_enum_str` (singular) is not `get_enum_strs` (plural), and
    /// serving the read from the plural table is what made an undefined `mbbi`
    /// state come out as its index.
    ///
    /// The difference is the trimming. `get_enum_strs` reports `no_str`, so the
    /// label list stops at the last non-empty state; `get_enum_str` indexes the
    /// state array *untrimmed* (`mbbiRecord.c:246-250`: any `val <= 15` reads
    /// `zrst + val * sizeof(zrst)`, empty or not) and only an index past the
    /// array reaches the sentinel. Verified on the compiled C `softIoc`: an
    /// `mbbi` with `ZRST`/`ONST` set answers `caget -t` with `""` at `VAL=5` and
    /// `"Illegal Value"` at `VAL=20`.
    ///
    /// The slots, borrowed, are the one overridable answer: the CA form
    /// ([`EnumSlots::form`]) and a link read's label ([`EnumSlots::label`]) are
    /// both derived from them, so the two cannot disagree.
    ///
    /// `None` — the record leaves the rset slot NULL (`#define get_enum_str
    /// NULL`), which is every record but `bi`/`bo`/`mbbi`/`mbbo`. A field on
    /// such a record renders from its menu instead; see
    /// `RecordInstance::enum_string_form_for`,
    /// the single owner that picks between the two.
    fn enum_slots(&self) -> Option<EnumSlots<'_>> {
        None
    }

    /// One of the cells the scan cycle reads on every pass, in the numeric
    /// carrier the record stores it in — see [`CycleRead`].
    ///
    /// The cycle's accessors below ([`Self::val`],
    /// [`Self::monitor_deadband_cells`], [`Self::analog_alarm_input`], ...)
    /// read through this rather than through `get_field`, so a type that
    /// stores its cells as members overrides this one method and every
    /// accessor reads them with no name match. The answer is `Copy` with no
    /// drop, so in a default body monomorphised per type the override's
    /// `match` folds to the member load.
    ///
    /// The default answers by name, except VAL and OVAL: those are
    /// [`CycleRead::ByName`], because [`Self::val`] and
    /// [`Self::output_link_value`] then read them by name once rather than
    /// cloning an array VAL here only to find it is not a number.
    fn cycle_cell(&self, cell: CycleCell) -> CycleRead {
        match cell {
            CycleCell::Val | CycleCell::Oval => CycleRead::ByName,
            cell => CycleRead::of_field(self.get_field(cell.name())),
        }
    }

    /// Store into a numeric cycle cell, exactly as [`Self::put_field`] stores
    /// `value` into `cell.name()`. The accessors hand over a value already
    /// converted to the cell's own carrier, so an override need only take
    /// that variant directly and pass anything else to `put_field`.
    fn put_cycle_cell(&mut self, cell: CycleCell, value: CycleValue) -> CaResult<()> {
        self.put_field(cell.name(), value.into())
    }

    /// Validate a put before it is applied. Return Err to reject.
    fn validate_put(&self, _field: &str, _value: &EpicsValue) -> CaResult<()> {
        Ok(())
    }

    /// Hook called after a successful put_field.
    fn on_put(&mut self, _field: &str) {}

    /// Whether a put to `field` names a subroutine that must resolve in the
    /// function registry — C `special(SPC_MOD)` → `registryFunctionFind`
    /// (`aSubRecord.c::special`, `subRecord.c::special`). C stores the name in
    /// `dbPut`, then `special(after)` looks it up and returns `S_db_BadSub`
    /// (→ rsrv `ECA_PUTFAIL`) when the name is non-empty and unregistered, so
    /// the client's write is REFUSED while the field keeps the value it was
    /// given. An empty name names no routine and is accepted.
    ///
    /// The record's `special()` has no database handle and so cannot reach the
    /// registry itself (the registry is the DB's single owner); this hook only
    /// says "a put to `field` is a subroutine name that must be resolved". The
    /// put owner — which holds the registry — extracts the name from the write,
    /// performs the lookup, and applies the `S_db_BadSub` refusal at the point
    /// C's `dbPut` returns the after-put `special()` status. Returns `false`
    /// for any field/mode C accepts without a lookup (a non-SNAM field, or an
    /// aSub in `LFLG=READ` where the name comes from the SUBL link at process
    /// time and a SNAM put is not validated).
    fn is_subroutine_name_field(&self, _field: &str) -> bool {
        false
    }

    /// Primary field name (default "VAL"). Override for waveform etc.
    fn primary_field(&self) -> &'static str {
        "VAL"
    }

    /// Whether a put to `field` directly defines the record — clears UDF
    /// exactly like a primary-value-field put. C `dbAccess.c::dbPut`
    /// (`:1409-1410`) clears `udf` synchronously only when the put target is
    /// `dbIsValueField` (i.e. `field == primary_field()`); this hook is where
    /// a record type says its own `special()` ALSO clears UDF for a
    /// non-value field, independent of `dbIsValueField`.
    ///
    /// `mbboDirect` is the case this exists for: `mbboDirectRecord.c::special`
    /// (`after==1`, B0..B1F, line 290) sets `prec->udf = FALSE` on a bit-field
    /// put — the bit write is a second value source alongside a VAL put and
    /// the closed-loop DOL fetch. Default: only the primary field.
    fn is_udf_defining_put(&self, field: &str) -> bool {
        field == self.primary_field()
    }

    /// Get the primary value.
    fn val(&self) -> Option<EpicsValue> {
        match self.primary_field() {
            "VAL" => match self.cycle_cell(CycleCell::Val) {
                CycleRead::Value(v) => Some(v.into()),
                CycleRead::Absent => None,
                CycleRead::ByName => self.get_field("VAL"),
            },
            field => self.get_field(field),
        }
    }

    /// Set the primary value.
    ///
    /// Matches C EPICS `dbPut` behavior: if the value type doesn't match
    /// the field type, it is automatically coerced (e.g., Long→Double for
    /// ai, Long→Enum for bi/mbbi). This prevents silent failures when
    /// asyn device support provides Int32 values to Enum-typed records.
    fn set_val(&mut self, value: EpicsValue) -> CaResult<()> {
        // Soft-channel INP/DOL delivery into the record's value field is
        // internal delivery, so it takes the same single owner every other
        // link target takes — `put_field_internal`. It was a parallel path
        // (put_field, then a `TypeMismatch`-triggered `convert_to` off the
        // *current* value's type), which silently dropped a shape the typed
        // arm rejected and `convert_to` could not fix: an array source into a
        // scalar VAL stayed an array and never landed. C's link layer asks for
        // one element (`dbGetLink(..., nRequest = NULL)`), so a waveform INP
        // into an `ai.VAL` delivers `wf[0]`.
        let field = self.primary_field();
        self.put_field_internal(field, value)
    }

    /// Whether this record's `INP` is read by DEVICE SUPPORT (a C `DSET`), as
    /// opposed to by the record body itself.
    ///
    /// The init-time load of a CONSTANT `INP` into the record's value field is
    /// soft device support's `init_record` (`devAiSoft.c`, `devLonginSoft.c`,
    /// … each call `recGblInitConstantLink(&prec->inp, …)`), so it exists only
    /// where a DSET exists. `compress` has no device support at all
    /// (`compressRecord.c` declares no `dset`; its `process` calls `dbGetLink`
    /// on `INP` itself), which is why C leaves a `field(INP,"5")` compress with
    /// an EMPTY circular buffer — the constant is loaded nowhere and delivers
    /// nothing at process (`dbConstGetValue`). Seeding it anyway put a phantom
    /// sample in the buffer before the first scan.
    fn input_read_by_device_support(&self) -> bool {
        true
    }

    /// The `(dbrType, *pnReq)` an ARRAY soft dset's `init_record` hands
    /// `dbLoadLinkArray` (`devWfSoft.c:43`, `devAaiSoft.c:59`,
    /// `devSASoft.c:61-67`). `None`, the default, is a scalar dset.
    fn soft_input_array_request(&self) -> Option<(crate::types::DbFieldType, usize)> {
        None
    }

    /// The rest of the soft INPUT device support's `init_record`, once the
    /// constant-INP load above has (or has not) landed.
    ///
    /// Most soft dsets are exactly `recGblInitConstantLink()` and stop there —
    /// a link they could not load leaves the record's own init state alone. The
    /// ARRAY dsets do not: `devWfSoft.c:39-51` runs `dbLoadLinkArray` on every
    /// waveform and sets `prec->nord = 0` when it fails (a real link, or none —
    /// `dbLoadLinkArray` has no `loadArray` lset outside a constant), which is
    /// why C serves `NORD = 0` on a `record(waveform,"X"){}` even though the
    /// record's own `init_record` seeded `nord = (nelm == 1)` a moment earlier.
    ///
    /// `loaded` is whether a constant INP reached the value field. Defaulted to
    /// a no-op: a dset that only seeds does not need this half.
    fn soft_input_dset_init(&mut self, loaded: bool) {
        let _ = loaded;
    }

    /// The soft dset's own `init_record` for a record whose DTYP names one —
    /// the state it leaves in the record's fields, input and output alike.
    /// Called once, from [`crate::server::device_support::init_device_support`],
    /// after the record's pass-0 init (C's order). Default: no such state.
    fn init_soft_dset(&mut self, soft: crate::server::device_support::SoftDtyp) {
        let _ = soft;
    }

    /// The plain `DTYP="Soft Channel"` INPUT dset body — C's `devXxxSoft.c`
    /// (and `devXxxSoftCallback.c`) `read_xxx`, handed the outcome of its own
    /// `dbGetLink(&prec->inp, ...)` once per process cycle.
    ///
    /// `Some(value)` is C's `if (!status)` arm, `None` its failure arm. Most
    /// soft dsets store what the link delivered and do nothing on failure,
    /// which is this default. `ai`'s two do not: `devAiSoft.c:81-93` and
    /// `devAiSoftCallback.c:180-194` blend the reading into the previous `VAL`
    /// through `SMOO` and keep their own "a read has completed" state, which
    /// they clear on a failed read so the next good one is unsmoothed.
    ///
    /// That filter belongs here and not in `process()`: `aiRecord.c:440-444`
    /// is the copy the RAW dset needs (`devAiSoftRaw::read_ai` returns 0, so
    /// `convert()` runs), while both soft dsets return 2 and `convert()` never
    /// runs for them at all.
    fn soft_input_read(
        &mut self,
        value: Option<EpicsValue>,
        _dpvt: &mut crate::server::device_support::Dpvt,
    ) -> CaResult<()> {
        match value {
            Some(value) => self.set_val(value),
            None => Ok(()),
        }
    }

    /// [`Self::soft_input_read`] for a `DBR_DOUBLE` reading, handed over
    /// without the [`EpicsValue`] wrapper. A record whose soft read filters
    /// its input overrides this with the direct path.
    fn soft_input_read_f64(
        &mut self,
        value: f64,
        dpvt: &mut crate::server::device_support::Dpvt,
    ) -> CaResult<()> {
        self.soft_input_read(Some(EpicsValue::Double(value)), dpvt)
    }

    /// [`Self::soft_input_read`] for a `DBR_LONG` reading. An override stores
    /// exactly what the wrapped read stores.
    fn soft_input_read_i32(
        &mut self,
        value: i32,
        dpvt: &mut crate::server::device_support::Dpvt,
    ) -> CaResult<()> {
        self.soft_input_read(Some(EpicsValue::Long(value)), dpvt)
    }

    /// [`Self::soft_input_read`] for a `DBR_USHORT` reading. An override
    /// stores exactly what the wrapped read stores.
    fn soft_input_read_u16(
        &mut self,
        value: u16,
        dpvt: &mut crate::server::device_support::Dpvt,
    ) -> CaResult<()> {
        self.soft_input_read(Some(EpicsValue::UShort(value)), dpvt)
    }

    /// [`Self::soft_input_read`] for a `DBR_ULONG` reading. An override
    /// stores exactly what the wrapped read stores.
    fn soft_input_read_u32(
        &mut self,
        value: u32,
        dpvt: &mut crate::server::device_support::Dpvt,
    ) -> CaResult<()> {
        self.soft_input_read(Some(EpicsValue::ULong(value)), dpvt)
    }

    /// [`Self::soft_input_read`] for a `DBR_INT64` reading. An override
    /// stores exactly what the wrapped read stores.
    fn soft_input_read_i64(
        &mut self,
        value: i64,
        dpvt: &mut crate::server::device_support::Dpvt,
    ) -> CaResult<()> {
        self.soft_input_read(Some(EpicsValue::Int64(value)), dpvt)
    }

    /// The `dbrType` the record type's `soft` INPUT dset hands its INP
    /// `dbGetLink` — `devAiSoft` asks `DBR_DOUBLE`, `devAiSoftRaw` `DBR_LONG`.
    /// The request is the dset's, not the record's: one record type reads INP
    /// at a different type under each of its soft DTYPs.
    ///
    /// Default: the record's own [`Self::input_link_request`] for `INP`.
    fn soft_input_request(
        &self,
        soft: crate::server::device_support::SoftDtyp,
    ) -> InputLinkRequest {
        let _ = soft;
        self.input_link_request("INP")
    }

    /// The `dbrType` the same soft dset's `init_record` loads a constant INP
    /// at — `recGblInitConstantLink(&prec->inp, dbrType, …)`, a call of its own
    /// in C. `None` loads nothing.
    ///
    /// Default: the type [`Self::soft_input_request`] reads at, which is the
    /// init type of every soft dset but `devMbbiSoftRaw`; a request that names
    /// no type falls back to VAL's, the type `devLiSoft` and `devI64inSoft`
    /// load at (`DBF_LONG`, `DBF_INT64`).
    fn soft_input_load_type(
        &self,
        soft: crate::server::device_support::SoftDtyp,
    ) -> Option<crate::types::DbFieldType> {
        match self.soft_input_request(soft) {
            InputLinkRequest::As(read_as) => read_as.dbr_type(),
            InputLinkRequest::NotRead | InputLinkRequest::FromSource => None,
        }
        .or_else(|| crate::server::record::record_instance::declared_field_type_of(self, "VAL"))
    }

    /// The `dbftype` `init_record` loads a constant SIOL into SVAL at —
    /// `recGblInitConstantLink(&prec->siol, DBF_x, &prec->sval)`, which names
    /// its own type rather than SVAL's.
    ///
    /// Default: SVAL's declared type.
    fn siol_load_type(&self) -> Option<crate::types::DbFieldType> {
        crate::server::record::record_instance::declared_field_type_of(self, "SVAL")
    }

    /// The `DTYP="Raw Soft Channel"` INPUT dset — C's four `devXxxSoftRaw.c`
    /// read supports (`devAiSoftRaw`, `devBiSoftRaw`, `devMbbiSoftRaw`,
    /// `devMbbiDirectSoftRaw`).
    ///
    /// The value read from the INP link goes to **`RVAL`**, not `VAL`: the
    /// record's own `RVAL → VAL` convert then runs (that is the whole
    /// difference from `"Soft Channel"`, whose `read_xxx` returns 2 and writes
    /// `VAL` directly).
    ///
    /// **`Some`/`None` IS the dset table.** A record type that implements this
    /// is one for which C ships a SoftRaw input dset; a record type that does
    /// not, C has no such dset for, so `DTYP="Raw Soft Channel"` on it is a
    /// configuration C rejects at iocInit. There is no separate boolean saying
    /// whether the record "accepts" raw input — that boolean existed, defaulted
    /// to `false`, had ONE override in the workspace, and silently sent the
    /// other three input records' raw values into `VAL`, where their own convert
    /// then overwrote them from an unseeded `RVAL=0` (R19-66). A capability
    /// answer that can disagree with the implementation is the bug.
    fn raw_soft_input(&mut self, entry: RawSoftEntry, value: EpicsValue) -> Option<CaResult<()>> {
        let _ = (entry, value);
        None
    }

    /// Whether the `"Async Soft Channel"` dset's `read_xxx` on a link its
    /// `add_record` refused returns 2 — `devBiSoftCallback.c:169` and the
    /// mbbi/mbbiDirect twins — so `if (status == 0) prec->udf = ...` is not
    /// reached and UDF stands. ai returns 2 too but folds it to 0 first;
    /// longin, int64in and stringin return 0.
    fn refused_async_read_keeps_udf(&self) -> bool {
        false
    }

    /// Whether a failed INP read under `"Raw Soft Channel"` skips the
    /// record's RVAL -> VAL convert. `devAiSoftRaw`, `devBiSoftRaw` and
    /// `devMbbiSoftRaw` return the `dbGetLink` status, so `if (status == 0)
    /// convert()` is not reached; `devMbbiDirectSoftRaw` ignores it and
    /// returns 0, so its convert runs on the unchanged RVAL.
    fn raw_soft_read_failure_skips_convert(&self) -> bool {
        false
    }

    /// Which of the cycle's statuses C's `process()` returns; the record's
    /// own is [`ProcessOutcome::status`].
    fn process_status_rule(&self) -> ProcessStatusRule {
        ProcessStatusRule::ZERO
    }

    /// The `DTYP="Raw Soft Channel"` OUTPUT dset — C's four `devXxxSoftRaw.c`
    /// write supports: the value `write_xxx()` puts to the OUT link.
    ///
    /// `devAoSoftRaw.c` / `devBoSoftRaw.c` put `RVAL` (`dbPutLink(&prec->out,
    /// DBR_LONG, &prec->rval, 1)`); `devMbboSoftRaw.c` /
    /// `devMbboDirectSoftRaw.c` put `RVAL & MASK` as `DBR_ULONG`. All four
    /// write the RAW word — never the engineering `OVAL` that
    /// [`Record::output_link_value`] (the `"Soft Channel"` dset) puts.
    ///
    /// Same rule as [`Record::raw_soft_input`]: `Some`/`None` IS the dset
    /// table. Before this hook existed, a `DTYP="Raw Soft Channel"` output
    /// record matched neither the soft-OUT arm (which tests for `"Soft
    /// Channel"`) nor the device arm (it has no device), so it wrote **nothing
    /// at all** to OUT.
    fn raw_soft_output_value(&self) -> Option<EpicsValue> {
        None
    }

    /// [`Self::raw_soft_output_value`], borrowed: C's `(dbrType, pbuffer)`
    /// for the Raw Soft dset. `None` takes the materialised value.
    fn raw_soft_output_view(&self) -> Option<OutView<'_>> {
        None
    }

    /// Apply a raw device value read *back* from an output record's device
    /// support (the asyn init seed and driver readback callback), the output
    /// counterpart of an input record's raw path, where device support
    /// writes `RVAL` and the record's own conversion produces `VAL`
    /// (`device_support.rs:43-46`). An output record whose
    /// `convert()` is forward (engineering → raw) must invert it here — store
    /// the raw value into `RVAL` and compute the engineering `VAL` — because
    /// the framework's forward convert would otherwise recompute `RVAL` from
    /// the stale `VAL` and discard the readback (C `processAo`/`initAo` set
    /// `rval`/`val` directly, devAsynInt32.c:955-957/:973-994).
    ///
    /// Returns `true` when the record fully produced `VAL` from the raw value
    /// (the asyn store path then reports `computed` so the forward convert is
    /// skipped). The default returns `false`: records whose own `convert()` is
    /// already `raw → eng` (`ai`) or that need no conversion (`longout`,
    /// `mbbo`, whose `set_val` re-derives from the raw value) keep the legacy
    /// raw → `RVAL` / direct-`VAL` path.
    ///
    /// `raw` is the `RVAL` the dset stores. A MASK is the dset's to apply, as
    /// asyn's `processBo`/`processMbbi` do, never the record's.
    fn apply_raw_readback(&mut self, _raw: i32) -> bool {
        false
    }

    /// Apply a float64 device value read *back* from an output record's asyn
    /// device support — the `asynFloat64` analogue of
    /// [`Record::apply_raw_readback`]. A float64 output (`ao`) whose device
    /// value carries an `ASLO`/`AOFF` linear scaling must seed the engineering
    /// `VAL` here (`VAL = value * ASLO + AOFF`), because the asyn store path
    /// would otherwise write the raw device value straight into `VAL` and drop
    /// the scaling. Sets `VAL` only (a float64 `ao` carries no `RVAL`); the
    /// reverse scaling `(OVAL - AOFF) / ASLO` is applied on the device-write
    /// side. Mirrors C `initAo`/`processAo` (devAsynFloat64.c:628-630/:647-649).
    ///
    /// Returns `true` when the record produced `VAL` from the raw value (the
    /// asyn store path then reports `computed`, skipping the forward convert).
    /// The default returns `false`: records with no float64 readback scaling
    /// keep the raw `set_val` path.
    fn apply_float64_readback(&mut self, _raw: f64) -> bool {
        false
    }

    /// Hand the record the database's breakpoint-table registry so an `ai`/`ao`
    /// with `LINR >= 3` can resolve and cache the table its `LINR` selects.
    /// Called once at iocInit, before the first `process`/`convert`. The record
    /// resolves the table lazily on the first conversion (and re-resolves when
    /// `LINR` changes at runtime), mirroring C `cvtRawToEngBpt`'s
    /// `init || *ppbrk == NULL` cache. The default is a no-op: only `ai`/`ao`
    /// carry `LINR`.
    fn install_breaktable_registry(
        &mut self,
        _registry: std::sync::Arc<crate::server::cvt_bpt::BreakTableRegistry>,
    ) {
    }

    /// Apply IVOA=2 ("set outputs to IVOV") semantics. C line numbers below
    /// resolve at epics-base `R7.0.10`; `busy` is module `busy` at
    /// `R1-7-4-6-g2dfe92d`.
    ///
    /// **The rule for every record that has a raw/staged output word is
    /// `VAL = IVOV`, then the record's OWN conversion — never `RVAL = IVOV`
    /// or `OVAL = IVOV`.** IVOV is a value in VAL's engineering units, so it
    /// has to travel the same VAL->raw path an ordinary cycle uses:
    ///
    /// - `ao` (`aoRecord.c:207-212`): `val = ivov; value = ivov;
    ///   convert(prec, value);` — OROC, linearisation and raw rounding all
    ///   run, so OVAL *and* RVAL are the converted IVOV.
    /// - `bo` (`boRecord.c:230-238`), `busy` (`busyRecord.c:235-243`):
    ///   `val = ivov;` then the inline `/* convert val to rval */` MASK rule.
    /// - `mbbo` (`mbboRecord.c:232-236`), `mbboDirect`
    ///   (`mbboDirectRecord.c:210-214`): `val = ivov; convert(prec);` — the
    ///   ZRVL..FFVL state-value lookup (mbbo only) and the SHFT shift.
    ///
    /// Records with no raw word assign directly and there is nothing to
    /// convert:
    ///
    /// - `calcout` (`calcoutRecord.c:646-647`), `scalcout`, `acalcout`:
    ///   `oval = ivov` — and inside the `doOutput`-gated `execOutput`, so a
    ///   non-output INVALID cycle must not touch OVAL at all. VAL is the calc
    ///   result, not the output.
    /// - `lso` (`lsoRecord.c:131-138`): `strncpy(val, ivov, sizv-1)` and
    ///   `len = strlen(val)+1`. OVAL is `monitor()`'s tracker here, not
    ///   output staging, and seeding it suppresses the write's own post.
    /// - `dfanout` (`dfanoutRecord.c:137-139`): `val = ivov;` then
    ///   `push_values(prec)`.
    ///
    /// Default uses [`Record::set_val`], which is the `dfanout` shape and is
    /// correct for any record whose OUT path reads VAL only.
    fn apply_invalid_output_value(&mut self, ivov: EpicsValue) -> CaResult<()> {
        self.set_val(ivov)
    }

    /// Whether this record type supports device write (output records only).
    /// `aao` is included here even though it's served by the same
    /// concrete struct as `waveform`/`aai`/`subArray` — the
    /// WaveformRecord's `can_device_write` override picks the right
    /// answer per [`crate::server::records::waveform::ArrayKind`], but this default matters for code that
    /// only has the record-type string.
    fn can_device_write(&self) -> bool {
        matches!(
            self.record_type(),
            "ao" | "bo"
                | "longout"
                | "int64out"
                | "mbbo"
                | "mbboDirect"
                | "stringout"
                | "lso"
                | "printf"
                | "aao"
        )
    }

    /// Has this cycle REACHED the record's `recGblFwdLink` line?
    ///
    /// C `if (!pact && prec->pact) return(0)` — device support took the write
    /// asynchronously, so `process()` returns before the tail and the client's
    /// `ca_put_callback` is still owed. A record that returns
    /// `AsyncPendingNotify` answers `false` until its device round-trip is
    /// done. Default: true, the synchronous record that runs to its own tail.
    ///
    /// One of the two inputs to `complete_put_notify`; see
    /// [`Self::should_fire_forward_link`] for the other and for the invariant
    /// they share.
    fn is_put_complete(&self) -> bool {
        true
    }

    /// Does this cycle TAKE the record's `recGblFwdLink` line, having reached
    /// it?
    ///
    /// # Invariant (CONTRACT)
    ///
    /// A cycle completes an outstanding put-notify if and only if it runs
    /// `recGblFwdLink`. C `recGbl.c:290-303` is why: `if (pdbc->ppn)
    /// dbNotifyCompletion(pdbc)` sits INSIDE the function a record type
    /// chooses whether to call, so declining the forward link withholds the
    /// client's `ca_put_callback` by construction. The two are one decision in
    /// C and must stay one here — mca's own source says so where it makes the
    /// choice: "Process forward-linked record. Tell EPICS dbPutNotify
    /// mechanism that processing is finished." (`mcaRecord.c:820-826`).
    ///
    /// The single owner of that decision is
    /// `database::processing::complete_put_notify`, which reads this method
    /// and [`Self::is_put_complete`] together. A record type states its C gate
    /// HERE and nowhere else; it must not carry a second, separately-drifting
    /// `is_put_complete` override to say the same thing.
    ///
    /// Six types gate the call, each transcribing one C line:
    ///
    /// - `busy` (`busyRecord.c:271`): `if ((prec->val == 0) || (prec->oval ==
    ///   0)) recGblFwdLink(prec);`
    /// - `motor` (`motorRecord.cc:1509`): `if (pmr->dmov != 0)`
    /// - `mca` (`mcaRecord.c:824`): `if (!pmca->acqg)`
    /// - `scaler` (`scalerRecord.c:475`): `if ((pscal->pcnt==0) && (pscal->us
    ///   == USER_STATE_IDLE))` inside the `ss == SCALER_STATE_IDLE` arm
    /// - `epid` (`epidRecord.c:201`): the UDF-gated `return(0)` above the
    ///   `:212` call
    /// - `throttle` (`throttleRecord.c:308`): the call is commented out in
    ///   `process()` and fired only from `valuePut`'s real-OUT-write branch
    ///   (`:582`)
    ///
    /// Default: true — the standard record, which calls it unconditionally.
    fn should_fire_forward_link(&self) -> bool {
        true
    }

    /// Where this type's cycle stamps TIME; see [`StampPoint`].
    fn stamp_point(&self) -> StampPoint {
        StampPoint::BeforeAlarms
    }

    /// Whether this output's `recGblGetTimeStampSimm(prec, prec->simm, NULL)`
    /// runs AFTER `writeValue` — only `lsoRecord.c:154`; every other output
    /// stamps before it (`aoRecord.c:192`, ...). `writeValue` is where
    /// `recGblGetSimm` reads SIML, so the stamp sees the previous cycle's
    /// SIMM unless this is true, and this cycle's when it is.
    fn stamps_after_write(&self) -> bool {
        false
    }

    /// Whether this record's OUT link should be written after processing.
    /// Defaults to true. Override in calcout / longout to implement OOPT
    /// conditional output (epics-base 7.0.8).
    fn should_output(&self) -> bool {
        true
    }

    /// The epilogue of C's `conditional_write` — where the record advances
    /// the reference the NEXT cycle's [`Self::should_output`] compares
    /// against (`longout.pval`, and the `outpvt` first-cycle bit with it).
    ///
    /// `longoutRecord.c:489-493` puts `prec->pval = prec->val;
    /// prec->outpvt = DONT_EXEC_OUTPUT;` OUTSIDE `if (doDevSupWrite)`, so a
    /// cycle that OOPT suppressed still advances. That unconditionality is
    /// the whole transition mechanism: `Transition_To_Zero` fires on the
    /// cycle where `val` reaches 0 and `pval` does not, which can only
    /// happen if the earlier nonzero cycle — which wrote nothing — latched.
    ///
    /// The framework therefore calls this once per cycle that reaches
    /// `conditional_write`, not once per write: C skips it only where
    /// `writeValue` returns before the switch (SIMM simulation, a failed
    /// SIML read) or where IVOA vetoes the call outright. Default: no-op —
    /// `calcout`/`sCalcout`/`aCalcout`/`swait` latch inside their own
    /// `process()`, as their C does.
    fn after_output_decision(&mut self) {}

    /// Whether [`Record::after_output_decision`] does anything for this type —
    /// whether its C `process()` has a `conditional_write` epilogue outside
    /// `if (doDevSupWrite)` at all. Answered so the cycle can settle it once
    /// (`ProcessPlan::redecides_after_output`) instead of taking the record's
    /// write lock every cycle to call an empty body; 42 of the 43 types have
    /// one. Override it in the same impl block as the epilogue — a type that
    /// forgets is caught by `a_type_that_redecides_after_output_says_so`.
    fn redecides_after_output(&self) -> bool {
        false
    }

    /// Whether this record uses MDEL/ADEL deadband for monitor posting.
    /// Binary records (bi, bo, busy, mbbi, mbbo) return false because
    /// C EPICS always posts monitors for these record types regardless
    /// of whether the value changed.
    fn uses_monitor_deadband(&self) -> bool {
        true
    }

    /// Whether this record's process cycle posts its primary value (`VAL`)
    /// as a value monitor (`DBE_VALUE` / `DBE_LOG`).
    ///
    /// Default `true`: for most records C `monitor()` posts `VAL` whenever the
    /// value moved (deadband, change-gate, or always).
    ///
    /// `false` for the "trigger" records `fanout` and `seq`. Their `VAL` is
    /// `field(VAL,DBF_LONG){ pp(TRUE) }` — "Used to trigger" — and their C
    /// `process()` posts `VAL` ONLY with the alarm events `recGblResetAlarms`
    /// returns: `if (events) db_post_events(prec, &prec->val, events)`
    /// (fanoutRecord.c:148-150, seqRecord.c:227-229), never `DBE_VALUE` /
    /// `DBE_LOG`. Writing `VAL` fans out the forward links / sequences the
    /// `DOn`→`LNKn` writes; the value itself is not a monitored quantity, so a
    /// run of `caput VAL` posts no per-put value event (only the initial
    /// subscription snapshot fires). The alarm bits still reach `VAL` through
    /// the deadband post's `alarm_bits`, so an alarm transition posts `VAL`
    /// with `DBE_ALARM` exactly as C's `if (events)` does.
    fn process_posts_value_monitor(&self) -> bool {
        true
    }

    /// Whether an alarm transition alone posts the deadband field. C
    /// `monitor()` posts VAL inside `if (monitor_mask)` for every type but
    /// aSub, state and permissive, whose guard is the VAL/OVAL change
    /// (`aSubRecord.c:414`, `stateRecord.c:115`, `permissiveRecord.c:107`):
    /// there the alarm bits ride a changed VAL and never post it alone.
    fn alarm_alone_posts_value(&self) -> bool {
        true
    }

    /// Per-record VALUE/LOG monitor gate for record types that post a
    /// monitor *only when the value actually changed* — and have no
    /// MDEL/ADEL deadband to express that.
    ///
    /// `Some(changed)` makes the framework post the VALUE and LOG
    /// monitors iff `changed`; `None` (the default) leaves the decision
    /// to the deadband / always-post path.
    ///
    /// C `lsiRecord.c`/`lsoRecord.c` `monitor()` raise `DBE_VALUE |
    /// DBE_LOG` only when `len != olen || memcmp(oval, val, len)`. Those
    /// records return [`Self::uses_monitor_deadband`]`== false`, which
    /// otherwise routes them to the unconditional always-post path
    /// (correct for binary records, wrong for lsi/lso).
    ///
    /// **THIS HOOK IS C's `monitor()`.** It takes `&mut self` because that is
    /// where C both compares and commits: `if (prec->mlst != prec->val) {
    /// events |= DBE_VALUE | DBE_LOG; prec->mlst = prec->val; }`
    /// (`boRecord.c:395-400`) and its `oval`/`olen` twin
    /// (`lsoRecord.c:248-252`). The implementation compares LIVE and commits
    /// its own previous-value tracker here — it must NOT capture a flag during
    /// `process()`.
    ///
    /// The distinction is not cosmetic. A record whose value can still change
    /// between `process()` returning and the monitors being posted — every
    /// record with an `IVOA = Set_output_to_IVOV` arm, since the port hoists
    /// that decision into the framework's single IVOA owner where C keeps it
    /// inside `process()` — reports a flag captured against the PRE-arm value
    /// if it captures early, so the post for the IVOV write lands a cycle late
    /// or, if the arm also seeds the tracker, never. Comparing here is the
    /// uniform rule that removes the ordering from each record's concern.
    ///
    /// Called exactly once per completed cycle, from
    /// [`Self::monitor_value_classes`], which
    /// the three cycle owners — the sync epilogue, the async completion, and
    /// the SIMM tail — reach on mutually exclusive paths.
    fn monitor_value_changed(&mut self) -> Option<bool> {
        None
    }

    /// `menuPost` "Always" override for the VALUE / LOG monitor masks.
    ///
    /// Returns `(post_value_always, post_archive_always)`. The framework
    /// ORs these into the change-gated mask from
    /// [`Self::monitor_value_changed`], so an *unchanged* process cycle
    /// still posts `DBE_VALUE` (resp. `DBE_LOG`) when the record's MPST
    /// (resp. APST) menu field is set to `Always`.
    ///
    /// C `lsiRecord.c`/`lsoRecord.c` `monitor()` compute the VAL post
    /// mask from three independent inputs:
    ///
    /// * the change test `len != olen || memcmp(oval, val, len)` →
    ///   `DBE_VALUE | DBE_LOG`,
    /// * `if (mpst == menuPost_Always) events |= DBE_VALUE;`,
    /// * `if (apst == menuPost_Always) events |= DBE_LOG;`.
    ///
    /// [`Self::monitor_value_changed`] carries the first input; this hook
    /// carries the other two. Records without a `menuPost` field keep the
    /// default `(false, false)`, which leaves the change gate unchanged.
    fn monitor_always_post(&self) -> (bool, bool) {
        (false, false)
    }

    /// Whether this cycle marked fields for [`Self::take_cycle_posted_fields`]
    /// — the `marked` of the default [`Self::monitor_value_classes`], so a
    /// type that marks keeps that default body instead of wrapping it.
    fn cycle_marked_fields(&self) -> bool {
        false
    }

    /// C `monitor()`'s VALUE / LOG decision for the deadband field, asked
    /// once per cycle: [`Self::monitor_value_changed`], else the
    /// [`Self::array_monitor_post`] rule, else the MDEL/ADEL deadband, with
    /// MLST/ALST committed where it crossed. `last_posted` is the instance's
    /// own `[MLST, ALST]`, read for a type without the cells and always
    /// written on a crossing.
    ///
    /// Not overridden: it is the default body that matters, compiled per
    /// type so the hooks a type leaves at their default fold away and the
    /// cells are read as struct members, as C reads them.
    fn monitor_value_classes(
        &mut self,
        gate: MonitorValueGate,
        last_posted: &mut [f64; 2],
    ) -> MonitorValueClasses {
        if let Some(changed) = self.monitor_value_changed() {
            let (val_always, archive_always) = self.monitor_always_post();
            return MonitorValueClasses {
                value: changed || val_always,
                archive: changed || archive_always,
                hash_changed: false,
                marked: self.cycle_marked_fields(),
            };
        }
        if !gate.uses_deadband {
            return MonitorValueClasses {
                marked: self.cycle_marked_fields(),
                ..MonitorValueClasses::ALWAYS
            };
        }
        monitor_deadband_classes(self, gate.array_monitor_post, last_posted)
    }

    /// The value the MDEL/ADEL deadband is evaluated against.
    ///
    /// For most records C `monitor()` applies the value deadband to
    /// `VAL`, so the default is [`Self::val`]. A record whose monitored
    /// quantity is not its primary value must override this: the motor
    /// record, for instance, has `VAL` as the setpoint and applies
    /// MDEL/ADEL to `RBV` (the readback) — its C `monitor()` deadbands
    /// `RBV`, not `VAL`. Such a record returns its readback field here.
    ///
    /// In the deadband's own domain, which VAL's type picks: C compares
    /// `double`s (`recGblCheckDeadband`) except on longin/longout and
    /// int64in/int64out, whose `DELTA` is integer (`int64inRecord.c:363`) —
    /// through a double, two `epicsInt64` values past 2^53 apart by one
    /// compared equal. `None` is a value the deadband cannot compare, which
    /// posts unconditionally.
    ///
    /// Default is `val()`, so existing records are unaffected.
    fn monitor_deadband_value(&self) -> Option<AlarmLimit> {
        match val_cell(self) {
            CycleRead::Value(v) => Some(v.alarm_limit()),
            CycleRead::Absent => None,
            // An array has no deadband; lent, it answers that without a copy.
            CycleRead::ByName => match self.monitor_value_view() {
                Some(ValueView::Scalar(v)) => Some(v.alarm_limit()),
                Some(_) => None,
                None => self.val().as_ref().and_then(AlarmLimit::from_stored),
            },
        }
    }

    /// The FIELD whose VALUE/LOG monitor delivery the MDEL/ADEL
    /// deadband gates — the field [`Self::monitor_deadband_value`]
    /// reads. A record overriding one must override both consistently.
    ///
    /// For most records the deadband gates the primary value itself,
    /// so the default returns [`Self::primary_field`] and nothing
    /// changes. The motor record deadbands RBV: C `monitor()`
    /// (motorRecord.cc:3468-3507) throttles the RBV post with
    /// MDEL/ADEL, while VAL is posted only when an actual setpoint
    /// change marked it (M_VAL). When this returns a non-primary
    /// field, the framework's snapshot builders:
    ///
    /// * deliver THIS field on the deadband triggers (instead of raw
    ///   change-detection), and
    /// * route the primary field through generic change-detection, so
    ///   an unchanged setpoint is not re-posted on every readback
    ///   poll.
    fn monitor_deadband_field(&self) -> &'static str {
        self.primary_field()
    }

    /// Fields the record's C `monitor()` posts on every cycle whose
    /// alarm transition fired, even when their value did not change.
    ///
    /// C motorRecord.cc `monitor()` (3456-3646) computes
    /// `local_mask = monitor_mask | (MARKED(x) ? DBE_VAL_LOG : 0)`
    /// for each field in its posting list — when the alarm moved
    /// (`monitor_mask != 0`), `local_mask` is non-zero for UNMARKED
    /// fields too, so every listed field posts with `DBE_ALARM` and a
    /// `DBE_ALARM`-only subscriber observes the alarm moment on any of
    /// them. The framework's change-detection loop posts a listed,
    /// subscribed, unchanged field with the cycle's alarm bits when
    /// this list names it.
    ///
    /// Default: empty — most C record types post only their value
    /// field(s) on an alarm transition (aiRecord.c `monitor()` posts
    /// VAL with `monitor_mask` and RVAL only when it changed), which
    /// the deadband-field post already covers.
    fn alarm_cycle_monitored_fields(&self) -> DeclaredFields {
        &[]
    }

    /// Whether [`Self::alarm_cycle_monitored_fields`] post with
    /// `monitor_mask | DBE_VALUE | DBE_LOG` rather than `monitor_mask` alone.
    /// C `mbboDirectRecord.c:323-326` posts every bit with
    /// `events|DBE_VALUE|DBE_LOG` once `events & ~(DBE_VALUE|DBE_LOG)`, so a
    /// `DBE_VALUE` subscriber sees the alarm moment too.
    fn alarm_cycle_posts_value_log(&self) -> bool {
        false
    }

    /// Fields whose C `monitor()` change test reads the record's own previous
    /// copy rather than what was last posted, and which of them this cycle
    /// found changed (bit i for `names[i]`). calc/calcout compare `A..U`
    /// against `LA..LU` (calcRecord.c:418, calcoutRecord.c:681), which a put
    /// does not move, so a put-then-process posts the input twice in C.
    ///
    /// Default: none — every field is compared against its last post.
    fn prev_compared_inputs(&self) -> (&'static [&'static str], u32) {
        (&[], 0)
    }

    /// Fields the record's C `monitor()` re-posts with `DBE_VAL_LOG` on
    /// every cycle that recomputed them, even when the value did not
    /// change — the analogue of an unconditional `MARK(field)` in C.
    ///
    /// Unlike [`Self::alarm_cycle_monitored_fields`] (which posts unchanged
    /// fields only on a cycle whose alarm transition fired), these post on
    /// any cycle the record names them, with `DBE_VALUE | DBE_LOG` (plus the
    /// cycle's alarm bits when one fired). The framework's change-detection
    /// loop posts a listed, subscribed, unchanged field with that mask.
    ///
    /// C motorRecord `process_motor_info` (motorRecord.cc:3764-3767)
    /// `MARK`s `M_DIFF`/`M_RDIF` unconditionally on every `CALLBACK_DATA`
    /// pass, and `monitor()` (3522-3531) posts them with `monitor_mask |
    /// DBE_VAL_LOG`; a `camonitor DIFF` on an axis parked at a constant
    /// non-zero following error thus gets an event every poll. The record
    /// returns the fields ONLY on the cycles it actually re-marked them (it
    /// reads its own per-cycle state), so a pass that did not recompute them
    /// does not over-post.
    ///
    /// Default: empty — most record types post a field only when it
    /// changed (or on an alarm transition), which the existing gates cover.
    fn force_posted_fields(&self) -> DeclaredFields {
        &[]
    }

    /// Fields this cycle's C `monitor()` posted UNCONDITIONALLY, chosen per
    /// cycle from record state — the DYNAMIC sibling of
    /// [`Self::force_posted_fields`].
    ///
    /// Some records decide which fields to post from a per-cycle BIT MASK
    /// rather than from a fixed list. aCalcout has two of them, and neither
    /// consults the value: `afterCalc` posts exactly the AMASK-flagged array
    /// fields — the ones the expression STORED into
    /// (`aCalcoutRecord.c:294-298`) — and `monitor()` posts exactly the
    /// NEWM-flagged ones — the input arrays whose link delivered a CHANGED
    /// value (`:1031-1036`). `AA := AA` therefore still posts AA.
    ///
    /// Neither existing gate can express that. The change-detection loop posts
    /// only what moved, so it drops the store-the-same-value case;
    /// [`Self::force_posted_fields`] is `&'static`, so it cannot name a set
    /// that varies per cycle (twelve arrays, 2^12 combinations) without
    /// over-posting every one of them every cycle.
    ///
    /// Each entry is ONE C `db_post_events` call, with the mask THAT call site
    /// uses ([`CyclePostMask`]) — not one entry per field. The two aCalcout call
    /// sites disagree on the mask (`afterCalc` posts a literal `DBE_VALUE|
    /// DBE_LOG`, `monitor()` posts `monitor_mask|DBE_VALUE|DBE_LOG`), and an
    /// array in BOTH masks is posted TWICE by C, once from each. So a field may
    /// legitimately appear twice, and the framework emits an event per entry.
    ///
    /// TAKE semantics: called exactly once per process cycle, and the record
    /// clears whatever state it answered from — C's `pcalc->newm = 0` (`:1036`)
    /// is part of the same step.
    ///
    /// Default: empty.
    fn take_cycle_posted_fields(&mut self) -> CycleMarks {
        CycleMarks::NONE
    }

    /// Whether marks remain after the last [`Self::take_cycle_posted_fields`]:
    /// a cycle owing more posts than one post table holds (sscan's
    /// end-of-scan arrays). The `WRITE_POSTED` / `HAND_POSTED` drain takes
    /// until this is false; the monitor walk takes once, so a
    /// `CYCLE_POSTED` type leaves it false.
    ///
    /// Default: false.
    fn cycle_posts_remain(&self) -> bool {
        false
    }

    /// The next post whose value was read when it was made: C
    /// `db_post_events` copies the field into the event at the call, so two
    /// posts of one field in a cycle deliver both values (sscan's `CPT`,
    /// `DATA` and `BUSY` transitions). The `WRITE_POSTED` / `HAND_POSTED`
    /// drain takes these until `None`, after the marks, and posts each stored
    /// value without reading the record.
    ///
    /// Default: `None`.
    fn take_cycle_posted_value(
        &mut self,
    ) -> Option<(FieldIx, crate::server::recgbl::EventMask, EpicsValue)> {
        None
    }

    /// Posts the record's C `process()` makes itself, ahead of
    /// `recGblGetTimeStamp` and `monitor()` — aCalcout's `afterCalc` array
    /// posts (`aCalcoutRecord.c:294-298`). Drained as soon as `process()`
    /// returns, so they carry the previous cycle's stamp and the alarm
    /// `recGblResetAlarms` has not yet committed. TAKE semantics.
    ///
    /// Default: empty.
    fn take_process_posted_fields(&mut self) -> CycleMarks {
        CycleMarks::NONE
    }

    /// Fields whose ONLY post path is the record's own per-cycle mark — C never
    /// change-detects them, so a value change alone must post nothing.
    ///
    /// aCalcout's arrays AA..LL are the case. C's `monitor()` compares scalar
    /// A..L against their PA..PL previous values (`aCalcoutRecord.c:1024-1029`)
    /// and OVAL against POVL (`:1039`), but it keeps NO previous copy of an
    /// array and runs no array comparison anywhere: an array posts if and only
    /// if the expression stored into it (AMASK, `afterCalc` `:294-298`) or its
    /// input link delivered a changed value (NEWM, `:1031-1036`) — both reported
    /// by [`Self::take_cycle_posted_fields`].
    ///
    /// So the change-detection arm must not see these fields at all. It is not
    /// merely redundant with the marks: a client `caput` to AA posts the put's
    /// value without advancing the subscriber's `last_posted`, and the next
    /// process — which stored nothing into AA and fetched nothing into it —
    /// then found AA "changed" and emitted a post C has no counterpart for.
    ///
    /// Default: empty — every other record's auxiliary fields post on change.
    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &[]
    }

    /// Fields the record's C `monitor()` re-posts with `DBE_LOG` ONLY on
    /// every cycle it names them, regardless of change — the analogue of
    /// an unconditional `db_post_events(field, DBE_LOG)` sweep.
    ///
    /// Distinct from [`Self::force_posted_fields`], which posts with
    /// `DBE_VALUE | DBE_LOG`: these post with `DBE_LOG` alone, so only a
    /// `DBE_LOG` (archiver) subscriber receives the event.
    ///
    /// The sweep is an INDEPENDENT post, NOT an alternative to the
    /// change-detected post. C's `db_post_events` calls compose: on the
    /// scaler's count-completion cycle `updateCounts()` posts each changed
    /// `Sn` with `DBE_VALUE` (scalerRecord.c:582) and then `monitor()` —
    /// reached because the done-interrupt set `ss = IDLE` (`:367`,
    /// `:510`) — posts the SAME `Sn` again with a literal `DBE_LOG`
    /// (`:757-773`). Two events, one field, one cycle. So the framework
    /// emits the sweep post in addition to whatever the change detection
    /// produced; gating it on "did not change" would silently drop the
    /// `DBE_LOG` half on exactly the cycle that carries the final counts.
    ///
    /// For a field that is ALSO a [`Self::value_only_change_fields`]
    /// member (scaler `Sn`) the change post carries `DBE_VALUE` only, so
    /// this sweep is the sole source of its `DBE_LOG` events, matching C.
    ///
    /// The record returns the names ONLY on the cycles whose C `monitor()`
    /// runs — the scaler reads its own `ss` state and returns `S1..Snch`
    /// while idle, empty while counting (a counting cycle never reaches C
    /// `monitor()`).
    ///
    /// The sweep post carries `DBE_LOG` plus the cycle's ALARM-transition bits
    /// (`recGblResetAlarms`). C's scaler computes that mask and then posts with a
    /// literal `DBE_LOG`, dropping the alarm bit — CBUG-B19, a deliberate
    /// deviation; see the post site in `collect_subscriber_posts`.
    ///
    /// Default: empty — most record types have no LOG-only sweep.
    fn log_swept_fields(&self) -> DeclaredFields {
        &[]
    }

    /// Fields whose change-detected monitor post must carry `DBE_VALUE`
    /// only — the LOG bit is stripped — instead of the framework default
    /// `DBE_VALUE | DBE_LOG`.
    ///
    /// The generic change-detection post (and the deadband post for a
    /// deadband field named here) normally bundles `DBE_LOG` so an
    /// archiver subscribed `DBE_LOG` sees every value change. A record
    /// whose C `db_post_events` calls pass a literal `DBE_VALUE` for
    /// these fields names them here so the framework drops the LOG bit;
    /// the cycle's alarm bits are still OR'd in (alarm posting is a
    /// separate per-field contract, unaffected by this hook).
    ///
    /// C `scalerRecord.c` posts CNT/T/VAL/PR1/TP/FREQ and each active
    /// channel `S1..Snch` with a literal `DBE_VALUE` on a value change
    /// (scalerRecord.c:372,478,582,588 et al.); `DBE_LOG` appears ONLY in
    /// the idle `monitor()` sweep ([`Self::log_swept_fields`],
    /// scalerRecord.c:771). The two hooks are complementary: a `DBE_LOG`
    /// subscriber on `Sn` is served by the idle sweep, never by a
    /// counting-cycle value change — matching C.
    ///
    /// Default: empty — most record types post changes with
    /// `DBE_VALUE | DBE_LOG` (C `monitor_mask | DBE_VALUE | DBE_LOG`,
    /// calcRecord.c:420, subRecord.c:400).
    fn value_only_change_fields(&self) -> &'static [&'static str] {
        &[]
    }

    /// Secondary value fields a record posts with the *primary VAL
    /// monitor mask*, from INSIDE the same guard C wraps its VAL post in —
    /// never with a forced `DBE_VALUE | DBE_LOG` on every change.
    ///
    /// Mirrors C records that drive a raw secondary field with the shared
    /// `monitor_mask` rather than `monitor_mask | DBE_VALUE | DBE_LOG`. Each
    /// entry pairs the field with the gate C applies to it *inside* that
    /// guard — see [`ValuePostGate`], which is the whole reason this is a
    /// pair and not a bare name: `ai` re-tests the raw value
    /// (`if (prec->oraw != prec->rval)`) while `timestamp` does not.
    ///
    /// Distinct from the default change-detected aux post (which carries
    /// `DBE_VALUE | DBE_LOG` unconditionally): ao `RVAL`/`RBV`, mbbo/
    /// mbboDirect/mbbiDirect `RVAL`/`RBV`, sel `SELN` and compress `NUSE`
    /// are all posted by C with the `DBE_VALUE | DBE_LOG`-forced mask, so
    /// they stay on the default path and must NOT be named here.
    ///
    /// Default: empty.
    fn fields_posted_with_value_mask(&self) -> &'static [(&'static str, ValuePostGate)] {
        &[]
    }

    /// C `monitor()`'s whole secondary block minus the posts, once per cycle:
    /// widen VAL's mask `value_mask` into the block's guard (ao `if
    /// (prec->omod) monitor_mask |= (DBE_VALUE|DBE_LOG);`, aoRecord.c:535,
    /// clear-on-read as C's `prec->omod = FALSE`), then TEST each
    /// [`Self::fields_posted_with_value_mask`] field its [`ValuePostGate`]
    /// lets in against the record's own "old" copy and, when it moved,
    /// ADVANCE that copy — C's `if (prec->oraw != prec->rval) {
    /// db_post_events(...); prec->oraw = prec->rval; }` (aoRecord.c:541-543)
    /// is one indivisible step.
    ///
    /// Called by the framework, independently of whether anyone is
    /// subscribed, since C's `db_post_events` runs whether or not anyone is
    /// listening. A record must not advance a copy anywhere else: an eager
    /// `oraw = rval` in the compute step makes the next cycle see no change
    /// and withhold an event C sends. Fired bits are positions in
    /// [`Self::fields_posted_with_value_mask`], so the cycle names no field by
    /// its bytes.
    ///
    /// Default: [`SecondaryTake::NONE`] — nothing declared, nothing to test.
    fn take_secondary_value_changes(
        &mut self,
        value_mask: crate::server::recgbl::EventMask,
    ) -> SecondaryTake {
        let _ = value_mask;
        SecondaryTake::NONE
    }

    /// Change-detected auxiliary fields this record posts with C's
    /// `monitor_mask | DBE_VALUE` — VAL's monitor mask ORed with `DBE_VALUE`,
    /// and NOT the framework default `monitor_mask | DBE_VALUE | DBE_LOG`.
    ///
    /// The difference is the forced `DBE_LOG`. For a field named here the LOG
    /// bit is present only when it is already in VAL's monitor mask — i.e. only
    /// when VAL's own ADEL deadband crossed this cycle — so a `DBE_LOG`
    /// subscriber (an archiver) receives the field exactly on the cycles C
    /// sends it, instead of on every change.
    ///
    /// `swaitRecord.c::monitor` (647-654) is this shape for its A..L inputs:
    ///
    /// ```c
    /// if (*pnew != *pprev)
    ///     db_post_events(pwait, pnew, monitor_mask | DBE_VALUE);
    /// ```
    ///
    /// while `calcRecord.c:420` — the same loop, one module over — writes
    /// `monitor_mask | DBE_VALUE | DBE_LOG`. The two records genuinely differ,
    /// so the mask is a per-record property, not a framework-wide rule.
    ///
    /// Distinct from [`Self::value_only_change_fields`] (a literal `DBE_VALUE`,
    /// which drops the ADEL LOG bit as well) and from
    /// [`Self::fields_posted_with_value_mask`] (posted from INSIDE C's
    /// `if (monitor_mask)` guard, so they do not post at all on a cycle where
    /// VAL itself does not). The fields named here post on every change,
    /// guard or no guard.
    ///
    /// Default: empty.
    fn fields_posted_with_monitor_mask(&self) -> &'static [&'static str] {
        &[]
    }

    /// Fields whose change post carries a LITERAL `DBE_VALUE | DBE_LOG` — this
    /// cycle's alarm bits DISCARDED.
    ///
    /// C's fourth mask shape, and the only one that *drops* information the
    /// record already computed. `epidRecord.c::monitor` builds VAL's mask from
    /// `recGblResetAlarms` (`:351`) and posts VAL with it, then REASSIGNS
    /// (not `|=`) before the secondaries:
    ///
    /// ```c
    /// monitor_mask = DBE_LOG|DBE_VALUE;          /* :376 */
    /// if (pepid->ovlp != pepid->oval) db_post_events(pepid, &pepid->oval, monitor_mask);
    /// ...                                        /* P, I, D, CT, DT, ERR, CVAL */
    /// ```
    ///
    /// so on an alarm-transition cycle a `DBE_ALARM`-only subscriber to one of
    /// those fields is sent NOTHING, while the generic aux mask
    /// (`alarm_bits | DBE_VALUE | DBE_LOG`) would send it an event.
    ///
    /// Distinct from the three narrower shapes: [`Self::value_only_change_fields`]
    /// (literal `DBE_VALUE`), [`Self::fields_posted_with_monitor_mask`]
    /// (`monitor_mask | DBE_VALUE` — keeps the alarm bits AND VAL's ADEL LOG bit)
    /// and [`Self::fields_posted_with_value_mask`] (VAL's mask, posted from inside
    /// C's `if (monitor_mask)` guard). A field named here posts on every change,
    /// with both value classes and no alarm class, whatever the cycle's alarms did.
    ///
    /// Resolved for every change-detected field by `AuxPostMask::mask_for`, the single
    /// owner of the aux-post mask.
    ///
    /// Default: empty.
    fn fields_posted_without_alarm_bits(&self) -> &'static [&'static str] {
        &[]
    }

    /// The array-style monitor decision (C waveform/aai/aao `monitor()`,
    /// waveformRecord.c:291-326). `None` (the default) means the record has
    /// no MPST/APST/HASH mechanism and the generic MDEL/ADEL deadband
    /// decision applies. `Some(_)` lets the record replace that with its
    /// "Always vs On Change" rule: it hashes the array content, compares to
    /// the stored `HASH`, updates it, and reports whether `DBE_VALUE` /
    /// `DBE_LOG` should be on the VAL post this cycle and whether the hash
    /// changed (so the owner posts `HASH` with `DBE_VALUE`). Called by
    /// [`monitor_deadband_classes`] (the single owner of the VAL-mask decision).
    ///
    /// The hook is the VAL mask, not the MPST/APST rule specifically: any
    /// record whose C `monitor()` decides the mask by its own rule implements
    /// it. `histogram` is the other implementor — its rule is the MDEL COUNT
    /// deadband (`mcnt > mdel`, histogramRecord.c:287-291), and like waveform's
    /// it updates the state it keys on (there, `MCNT = 0`; here, `HASH`).
    fn array_monitor_post(&mut self) -> Option<ArrayMonitorPost> {
        None
    }

    /// Whether [`Self::array_monitor_post`] can answer `Some` for this type,
    /// settled once in the monitor plan so a deadband cycle does not call an
    /// empty hook. `true` by default: a type that forgets pays the call
    /// rather than losing its mask rule.
    fn has_array_monitor_post(&self) -> bool {
        true
    }

    /// The software event a process of this record posts — C `event`'s
    /// `postEvent(prec->epvt)` (`eventRecord.c:120`), the handle settled at
    /// the VAL write. `None` posts nothing.
    fn software_event(&self) -> Option<&crate::server::records::event::EventHandle> {
        None
    }

    /// The CLOSED set of fields a process cycle of this record may post —
    /// the record's C `process()` + `monitor()` `db_post_events` calls,
    /// enumerated.
    ///
    /// `None` (the default) leaves the framework's generic rule in force:
    /// every subscribed field that changed since its last post is posted.
    /// That rule is right for a record whose C `monitor()` walks its fields
    /// and posts whatever moved (calc, sub, ai …). It is WRONG for a record
    /// whose C `monitor()` posts a fixed list and leaves every other field it
    /// wrote silent — the framework then invents events C never sends:
    ///
    /// * a field the record WRITES during `process()` but C never posts
    ///   (scaler's gate→direction copy, `scalerRecord.c:413-414`: `pdir[i] =
    ///   pgate[i]` with no `db_post_events` — C posts `Dn` only from
    ///   `special()`).
    ///
    /// (A field a PUT already posted is NOT in that category: the put's own
    /// post advances `last_posted` — see the `RecordInstance::last_posted`
    /// contract — so the next process cycle does not change-detect it. This
    /// hook must not be used to paper over a framework double post.)
    ///
    /// `Some(list)` closes it by construction: a field outside the list is
    /// never posted by a process cycle — its only monitors come from its own
    /// put and from the posts its `special()` marks. The list is a
    /// whitelist, not a blacklist, so a field added to the record later stays
    /// silent unless C posts it.
    ///
    /// Fields inside the list keep their normal treatment (change detection,
    /// [`Self::value_only_change_fields`] mask, deadband, `log_swept_fields`).
    ///
    /// The list may depend on the record's state, but `Some` versus `None`
    /// is fixed per type: the monitor plan reads it once.
    fn process_posted_fields(&self) -> Option<DeclaredFields> {
        None
    }

    /// Whether every post a cycle makes past the deadband field is decided
    /// from the record's own fields, as C `monitor()` decides it: each
    /// [`Self::process_posted_fields`] field is the deadband field, a
    /// `dbCommon` field or a [`Self::fields_posted_only_when_marked`] one, no
    /// secondary is [`ValuePostGate::WithValue`], and the type names no forced,
    /// alarm-fan-out or log-swept field. The cycle then walks only on a mark
    /// or a fired secondary, whoever is subscribed.
    ///
    /// The plan's `own_posts_hold` check is necessary, not sufficient: post
    /// sets that turn over with record state (aSub's `EFLG`) pass it and still
    /// lose posts, so do not declare this on a new type; a derived post-set
    /// delta replaces the declaration.
    fn answers_own_posts(&self) -> bool {
        false
    }

    /// Fields the record posts itself via an event-driven, individually
    /// masked path rather than the generic change-detection loop. The
    /// framework excludes these from that loop so they are neither
    /// double-posted nor spuriously posted on a cycle the event did not
    /// fire. C waveform/aai/aao `monitor()` posts `HASH` this way —
    /// `db_post_events(prec, &prec->hash, DBE_VALUE)` only when the content
    /// hash changed (waveformRecord.c:317-319), never via VAL's change.
    ///
    /// Default: empty.
    fn event_posted_fields(&self) -> &'static [&'static str] {
        &[]
    }

    /// Initialize record (pass 0: field defaults; pass 1: dependent init).
    fn init_record(&mut self, _pass: u8) -> CaResult<()> {
        Ok(())
    }

    /// Did `init_record` leave the record PERMANENTLY ACTIVE — C's
    /// `prec->pact = TRUE` inside `init_record`, which is how a record type
    /// disables itself when it cannot possibly process?
    ///
    /// `subRecord.c:119-123` is the live case: an empty `SNAM` has no
    /// subroutine to call, so C prints `"%s.SNAM is empty"`, sets `pact = TRUE`
    /// and returns 0. `dbProcess` then takes the PACT-active branch on every
    /// scan — the record serves its fields but never runs record support.
    /// `caget X.PACT` on a bare `record(sub,"X"){}` reads 1 on a C IOC.
    ///
    /// This is a STATE PREDICATE over the record's current fields, not a
    /// one-time init verdict: C re-asks it on every put to a field in
    /// [`Self::pact_park_fields`], through the two passes of `special()`
    /// (`subRecord.c:170-194`). Pass 0 releases the park, pass 1 re-takes it if
    /// the value just stored still leaves the record unable to run, so
    /// `caput X.SNAM mySub` revives a parked record and `caput X.SNAM ""` parks
    /// a running one.
    ///
    /// PACT is a `dbCommon` field with a single owner
    /// ([`crate::server::record::RecordInstance::enter_pact`] / [`leave_pact`]), so a record cannot
    /// park it itself. It answers here and the owner performs the transition —
    /// at the end of the init passes, and either side of a park-field put.
    ///
    /// [`leave_pact`]: crate::server::record::RecordInstance::leave_pact
    fn parks_pact(&self) -> bool {
        false
    }

    /// The fields whose put can change [`Self::parks_pact`]'s answer — C's
    /// `special(SPC_MOD)` set for the PACT park (`subRecord.dbd.pod` marks only
    /// `SNAM`). Nothing else re-asks the question, so a put to an unrelated
    /// field of a parked record cannot disturb the park the way an
    /// every-put re-assertion would. Default: none, so the whole mechanism is
    /// inert for every record type that does not use PACT as a self-disable.
    fn pact_park_fields(&self) -> &'static [&'static str] {
        &[]
    }

    /// The status this record type's soft channel dset `init_record` returns.
    /// Default `0`; `devBoSoft`, `devMbboSoft`, `devMbboDirectSoftRaw` and
    /// `devAoSoft` return `2` (or have no `init_record`, which their record
    /// treats the same).
    fn soft_dset_init(&self) -> crate::server::device_support::DsetInit {
        crate::server::device_support::DsetInit::Ok
    }

    /// The `pmessage` of the record's `recGblRecordError(S_dev_badInitRet)`
    /// for a dset `init_record` status other than 0 or 2. `None` for the
    /// record types that pass the status on unreported (default).
    fn bad_init_ret_message(&self) -> Option<&'static str> {
        None
    }

    /// The record's `init_record` arm for a dset `init_record` that returned
    /// 0: convert the RVAL the dset left into VAL, returning the UDF it sets.
    /// `None` for the record types with no such arm (default).
    fn dset_init_convert(&mut self) -> Option<bool> {
        None
    }

    /// ai/ao `init_record` just before `pdset->common.init_record`
    /// (`aiRecord.c:115-119`): the legacy EOFF rebase, returning EOFF/ESLO as
    /// they were before it. `None` for the record types with no such step.
    fn eoff_eslo_before_dset_init(&mut self) -> Option<(f64, f64)> {
        None
    }

    /// ai/ao `init_record` right after a dset `init_record` returns, whatever
    /// its status (`aiRecord.c:121-126`): a SLOPE record takes back the
    /// EOFF/ESLO [`Self::eoff_eslo_before_dset_init`] saved. A dset with no
    /// `init_record` never reaches it.
    fn eoff_eslo_after_dset_init(&mut self, _saved: (f64, f64)) {}

    /// Whether the record calls its dset's `init_record` in pass 0 rather
    /// than pass 1: aai and aao do (`aaiRecord.c:115-127`,
    /// `aaoRecord.c:124-131`), so the driver's init runs before
    /// `doResolveLinks` and every record's `add_record`. Default `false`.
    fn dset_init_in_pass0(&self) -> bool {
        false
    }

    /// The record's `init_record` right after the dset's `init_record` call,
    /// or where a dset without one would make it, whatever its status:
    /// `init_common` in mbbi (`mbbiRecord.c:135`) and mbbo
    /// (`mbboRecord.c:141`, `:173`). Before [`Self::dset_init_convert`].
    fn after_dset_init(&mut self) {}

    /// A device rewrote the state table in place, outside any put, as
    /// devOpcua's `updateEnumInfos` does (it sets `sdef` itself,
    /// devOpcua.cpp:714-716): re-derive what `init_common` caches. No
    /// `special()` runs.
    fn state_table_written(&mut self) {}

    /// Post-init finalisation hook with mutable access to the
    /// framework's UDF flag. Called once after both `init_record`
    /// passes complete. Default implementation is a no-op.
    ///
    /// epics-base PR `dabcf89` (mbboDirect): when VAL is undefined
    /// at init time but the user populated B0..B1F bits, the bits
    /// should be folded into VAL and UDF cleared. The framework
    /// owns `common.udf`, so the record cannot mutate it from
    /// `init_record` alone — this hook is the controlled point of
    /// access.
    fn post_init_finalize_undef(&mut self, _udf: &mut bool) -> CaResult<()> {
        Ok(())
    }

    /// Whether this record type's C `init_record` calls `recGblResetAlarms`,
    /// so a freshly loaded, never-processed record reads `STAT=NO_ALARM`,
    /// `SEVR=NO_ALARM` instead of the born `UDF`/`INVALID`.
    ///
    /// Almost no record does this: a not-yet-processed record is normally left
    /// in alarm so an `MS` consumer inherits `INVALID` at IOC startup (see
    /// `RecordInstance::run_init_pass0`). asyn does (`asynRecord.c:303-304`,
    /// after `udf = 0`), and so does table, whose init-time `monitor()`
    /// (`tableRecord.c:405`) runs `recGblResetAlarms` (`:882`) with UDF left
    /// at 1. This hook is the alarm reset only; a record that also clears UDF
    /// says so through [`Record::post_init_finalize_undef`]. Default `false`.
    fn init_resets_alarms(&self) -> bool {
        false
    }

    /// C `cvt_dbaddr`'s `paddr->no_elements` for one of this record type's
    /// `special(SPC_DBADDR)` fields — the CHANNEL's capacity, which is not the
    /// count `get_array_info` serves.
    ///
    /// Answer the number only. WHICH fields reach `cvt_dbaddr` is not a record's
    /// question: it is the `.dbd`'s, and
    /// [`FieldDeclaration::field_native_count`] reads it from there before
    /// asking. A record that hand-lists its own array fields here is copying a
    /// declaration it already has, which is how the two drift apart.
    ///
    /// Returning `None` (the default, and the answer for a record type with no
    /// `SPC_DBADDR` field) means the channel count is the value's own count.
    ///
    /// - waveform `VAL` → `NELM` (buffer capacity; the value serves `NORD`).
    /// - asyn `BOUT` → `OMAX`, `BINP` → `IMAX` (the `SPC_DBADDR` octet buffers;
    ///   the value serves the transferred byte count `NOWT`/`NORD`).
    /// - acalcout `AVAL`/`AA..LL`/`OAV` → `NELM`, or the `NUSE` window under
    ///   `SIZE=NUSE` (`aCalcoutRecord.c:627-631`).
    /// - mca `VAL`/`BG` → `NMAX` (`mcaRecord.c:857`).
    fn dbaddr_capacity(&self, _field: &str) -> Option<u32> {
        None
    }

    /// What C's `cvt_dbaddr` DOES to the record before answering — acalcout
    /// `calloc`s and charges the buffer it hands out (`aCalcoutRecord.c:589-617`).
    /// Run only when a channel resolves on the `SPC_DBADDR` field `field`
    /// ([`FieldDeclaration::field_native_count`]), never by a capacity query.
    fn cvt_dbaddr(&self, _field: &str) {}

    /// The `paddr->field_size` this record's `cvt_dbaddr` stores for the
    /// `SPC_DBADDR` field `field`, when it is not the declaration's own
    /// ([`FieldDesc::element_size`]). `None` (the default) is every record
    /// whose `cvt_dbaddr` sets `dbValueSize(field_type)` or `sizeof` — aSub
    /// `A`..`U`, waveform/aai/aao/subArray `VAL`, compress, histogram — which
    /// the declaration and the served type already answer.
    ///
    /// lso/lsi `VAL`/`OVAL` and printf `VAL` answer `SIZV`
    /// (`lsoRecord.c:186`, `lsiRecord.c:144`, `printfRecord.c:420`): the width
    /// is the record's own and varies per instance.
    fn dbaddr_element_size(&self, _field: &str) -> Option<u16> {
        None
    }

    /// Whether this record stores the `SPC_DBADDR` field `field` as a single
    /// element rather than a buffer (`mbbo.VAL`, an `aSub` channel at count 1).
    /// [`FieldDeclaration::field_stores_array`] reads it so a put can learn
    /// the destination's shape without cloning the value under the lock.
    fn dbaddr_stored_as_scalar(&self, _field: &str) -> bool {
        false
    }

    /// The part of C's `init_record` tail that is NOT tracker seeding: an
    /// output record's closing VAL→RVAL conversion, run at the same point
    /// [`Self::seed_deadband_tracking`] is and immediately before it, which is
    /// the order C writes the two in (`boRecord.c:167-175` converts, then
    /// seeds).
    ///
    /// It cannot live in [`Self::init_record`]: that runs before the
    /// `recGblInitConstantLink` table, so a record whose VAL arrives from a
    /// constant DOL would convert the pre-seed VAL. C has no such split —
    /// `busyRecord.c:151-179` is one function with the constant load at the
    /// top and the conversion at the bottom.
    ///
    /// Every record whose C init tail does non-tracker work implements it here
    /// — the conversion and the output-tracking stores C writes around the
    /// `mlst`/`alst`/`lalm` lines:
    ///
    /// - busy `:176-179` — the convert alone; it seeds no tracker at all.
    /// - bo `:166-170,174-175` — convert through MASK, then `oraw`/`orbv`.
    /// - mbbo `:176-177,181-182` — `convert()`, then `oraw`/`orbv`.
    /// - mbboDirect `:142-143,161-162` — `bitsFromVAL`, then `oraw`/`orbv`.
    /// - ao `:156,160-161` — `oval`/`pval`, then `oraw`/`orbv`.
    ///
    /// The split is by MEANING, not by convenience: this hook is the tail's
    /// derived state (RVAL and the bit cells from VAL, ORAW/ORBV from RVAL/RBV,
    /// ao's OVAL/PVAL), and [`Self::seed_deadband_tracking`] is the tail's
    /// deadband trackers and nothing else. Four records used to carry both
    /// halves in `seed_deadband_tracking`, which made "the trackers are seeded"
    /// and "the record is converted" the same call — so a caller that wanted
    /// one got the other, and busy (which needs the convert without the
    /// trackers) had to be the exception.
    ///
    /// `common` is there for an alarm the tail raises: mbbo's `convert()`
    /// raises SOFT/INVALID into NSEV at `mbboRecord.c:177`. Default: empty.
    fn init_record_tail(&mut self, _common: &mut CommonFields) {}

    /// Seed the monitor/archive/alarm deadband trackers (MLST/ALST/LALM)
    /// from the initial value at iocInit, called once by the builder after
    /// both `init_record` passes and `post_init_finalize_undef`.
    ///
    /// The C value records whose `init_record` ends
    /// `prec->mlst = prec->alst = prec->lalm = prec->val` (`aiRecord.c:129-131`,
    /// `longinRecord.c:120-122`) post nothing on a first `monitor()` for a
    /// value that has not moved since init; each overrides this to seed the
    /// subset its C tail writes, after the dset outcome its C checks
    /// (`biRecord.c:112-114`'s `if (status) return status;`, ai only without
    /// a dset `init_record`, mbbi/calcout regardless), and only that.
    /// [`Self::init_record_tail`] owns the rest of the tail.
    ///
    /// The default seeds nothing: most record types (calc, sel, dfanout,
    /// busy, the synApps calcs, motor, epid, ...) end `init_record` without
    /// touching the trackers and so post that first update.
    fn seed_deadband_tracking(&mut self, _dset_init: DsetInit) {}

    /// Called by the framework immediately after applying this cycle's
    /// [`Record::multi_input_links`] fetches, before `process()`.
    ///
    /// `resolved` lists the `link_field` names (the first element of
    /// each `multi_input_links` pair) whose fetch SUCCEEDED this cycle —
    /// C's `RTN_SUCCESS(dbGetLink(...))`, i.e. status 0. That includes a
    /// CONSTANT link, which returns success having delivered nothing
    /// (`dbConstGetValue`, `dbConstLink.c:219-225`) — `epidRecord.c:191`
    /// clears UDF on exactly that. A link field absent from the slice
    /// either had no link configured or its DB/CA fetch FAILED.
    ///
    /// This is the framework analogue of C device support inspecting
    /// `RTN_SUCCESS(dbGetLink(...))` — e.g. `epidRecord.c:191-193`
    /// clears `udf` only when `dbGetLink(&prec->stpl, ...)` returns
    /// success. A record's `process()` cannot otherwise observe whether
    /// an input link's fetch succeeded, because a failed fetch simply
    /// leaves the target field unwritten.
    ///
    /// Additive, framework-set-hook pattern (same shape as
    /// [`Record::set_process_context`]). Default: ignore.
    fn set_resolved_input_links(&mut self, _resolved: ResolvedInputLinks<'_>) {}

    /// Report this cycle's `fetch_values()` outcome: `failed == true` means C's
    /// helper would have returned a non-zero status, so the record body — the
    /// `calcPerform` / `do_sel` the C `process()` wraps in
    /// `if (fetch_values(prec) == 0)` — must NOT run, and VAL/UDF freeze.
    ///
    /// This is the single delivery point for that outcome, whichever
    /// [`InputFetchPolicy`] produced it (a failed link read) and whichever
    /// record-specific rule did (sel's `Specified`-mode selected-input read).
    /// Records that gate: calc (calcRecord.c:120), calcout (:237), sCalcout
    /// (sCalcoutRecord.c:356), aCalcout (aCalcoutRecord.c:399), swait
    /// (swaitRecord.c:408 — which additionally raises READ_ALARM/INVALID on the
    /// failure) and sel (selRecord.c:114). sub/aSub gate the same outcome, but
    /// their body is the framework-dispatched subroutine, so they consume it
    /// through `RecordInstance::suppress_subroutine_run` instead.
    ///
    /// Default: ignore (records with no fetch gate). Same framework-set hook
    /// pattern as [`Record::set_resolved_input_links`].
    fn set_fetch_gate_failed(&mut self, _failed: bool) {}

    /// Whether this record's multi-input fetch ([`Self::multi_input_links`]) is
    /// C `dbGetLink` — which raises `setLinkAlarm` (LINK/INVALID, AMSG
    /// `field <NAME>`) on every failed read — or a `recDynLink` CA-style get,
    /// which raises nothing.
    ///
    /// `true` for every base record and for sCalcout/aCalcout/transform
    /// (`calcRecord.c:439`, `calcoutRecord.c:705`, `subRecord.c:414`,
    /// `aSubRecord.c:282`, `selRecord.c:430`, `printfRecord.c:54`,
    /// `sCalcoutRecord.c:886`, `aCalcoutRecord.c:1070`,
    /// `transformRecord.c:537`). `false` only for swait, whose `fetch_values`
    /// (`swaitRecord.c:702`) reads INAA..INPL with `recDynLinkGet` and answers a
    /// failure with `recGblSetSevr(READ_ALARM, INVALID_ALARM)` at
    /// `swaitRecord.c:413` instead. Both alarms are INVALID and
    /// `rec_gbl_set_sevr*` is strict-greater, so raising the wrong one first
    /// wins the tie and publishes the wrong STAT.
    fn multi_input_fetch_is_db_get_link(&self) -> bool {
        true
    }

    /// The generation of this record's [`Self::multi_input_links`]: a count
    /// that moves on EVERY write of any of their texts, and on every write
    /// that can change what [`Self::input_link_request`] or
    /// [`Self::input_link_failure_is_inert`] answers for any of them. While
    /// the record answers `g`, a parse validated at `g` is still the parse of
    /// the text, and those two answers read at `g` are still the answers.
    /// A type answers `Some` only when all of those fields have ONE writer
    /// that counts, behind private fields — `calc`'s `set_inp_link`, aSub's
    /// `put_field` of `INPx`/`FTx` — and the count is then what C's
    /// `dbPutSpecial` on a `DBF_INLINK` gives `dbDbInitLink`: the moment the
    /// link changed. `None` (the default) validates every parse against the
    /// text itself, byte for byte, and asks both answers again, on every read.
    fn input_links_generation(&self) -> Option<u64> {
        None
    }

    /// Whether a FAILED read of this multi-input link leaves the cycle
    /// untouched — no `setLinkAlarm`, no [`Self::input_fetch_policy`] gate,
    /// no value stored.
    ///
    /// One record needs it: acalcout guards its ARRAY half with the link's own
    /// connection status (`aCalcoutRecord.c:1078`)
    ///
    /// ```c
    /// if ((*plinkValid==acalcoutINAV_EXT) || (*plinkValid==acalcoutINAV_LOC))
    /// ```
    ///
    /// so an array link C believes cannot deliver is never read at all, and
    /// `fetch_values` returns 0 with the calc still running. C moves a link
    /// between `EXT_NC` and `EXT` from a `dbCaIsLinkConnected` watchdog
    /// (`aCalcoutRecord.c:1155`, `:1187`); this port has no CA client, so it
    /// publishes `EXT_NC` for EVERY external link
    /// ([`crate::server::records::link_status::classify_link`]) and cannot make
    /// that test at select time. Reading and then discarding the failure is the
    /// same observable behaviour for every state the port can reach — a link
    /// that delivers is C's `EXT`/`LOC`, a link that does not is C's `EXT_NC` —
    /// and, unlike gating on the published `IAAV`, it does not stop reading the
    /// external array links whose values the resolver really does serve.
    ///
    /// The SCALAR half is deliberately NOT inert: its loop (`:1068-1071`) has
    /// no status test and `return`s at the first failing `dbGetLink`.
    /// The text of a link field as the record stores it, lent rather than
    /// copied.
    ///
    /// C reads a link off `dbCommon` as a `struct link` and copies nothing; a
    /// scan cycle that asks [`Self::get_field`] instead pays a `String` clone,
    /// a [`PvString`] wrap, an
    /// [`EpicsValue`] construction and a drop for
    /// every link the type declares — 21 of them per `calc` cycle, whether or
    /// not a single one is wired.
    ///
    /// The text is the field's bytes, as C's `struct link` holds them: a
    /// link may name a record whose name is not UTF-8.
    ///
    /// `Some(b"")` is a declared link that is unset, which is what an unwired
    /// `INPA` holds. `None` is this type declining to lend the field, and the
    /// caller falls back to the owned [`Self::get_field`] path, so a type that
    /// does not override this loses nothing.
    fn link_text_ref(&self, _link_field: &str) -> Option<&[u8]> {
        None
    }

    /// SIMM/SIMS/SDLY as stored, lent without an owned field value. Every
    /// base record type that declares SIMM overrides this (a type whose C
    /// dbd has no SDLY answers the synchronous `-1.0`). `None` is for
    /// record crates outside this one, which keep the three fields behind
    /// [`Self::get_field`]; the callers read them there.
    fn simm_cells(&self) -> Option<SimmCells> {
        None
    }

    /// seq link group `group`'s `(DLYn, DOn)` as stored, for the dispatch
    /// that reads them every cycle. `None` keeps them behind
    /// [`Self::get_field`], where the caller reads them.
    fn seq_group_cells(&self, _group: usize) -> Option<(f64, f64)> {
        None
    }

    /// SIMM as the `i16` the recGbl helpers compare, through
    /// [`Self::simm_cells`] where the type offers it.
    fn simm_value(&self) -> Option<i16> {
        match self.simm_cells() {
            Some(cells) => Some(cells.simm),
            None => match self.get_field("SIMM") {
                Some(EpicsValue::Short(simm)) => Some(simm),
                _ => None,
            },
        }
    }

    fn input_link_failure_is_inert(&self, _link_field: &str) -> bool {
        false
    }

    /// C `monitor()`'s deadband operands, read in one question: the value
    /// ([`Self::monitor_deadband_value`]), the MDEL / ADEL thresholds and the
    /// MLST / ALST values the record last posted at, all in the value's
    /// domain. A deadband this type does not carry is 0; an MLST/ALST it does
    /// not carry is the instance's own `[MLST, ALST]`, passed in as
    /// `last_posted`. `None` is no comparable value.
    ///
    /// Asked as one question because the cycle otherwise asks
    /// [`Self::get_field`] four times through the vtable and pays an
    /// [`EpicsValue`] per answer. Each cell is read through
    /// [`Self::cycle_cell`]; by name, that was measured at ~970 instructions
    /// of an `ai` cycle across the cycle's cell reads.
    fn monitor_deadband_cells(&self, last_posted: [f64; 2]) -> Option<MonitorDeadbandCells> {
        Some(match self.monitor_deadband_value()? {
            AlarmLimit::Double(val) => MonitorDeadbandCells::Double(deadband_cells(
                self,
                val,
                last_posted,
                CycleValue::to_f64,
            )),
            AlarmLimit::Long(val) => MonitorDeadbandCells::Long(deadband_cells(
                self,
                val,
                last_posted,
                CycleValue::to_i32,
            )),
            AlarmLimit::Int64(val) => MonitorDeadbandCells::Int64(deadband_cells(
                self,
                val,
                last_posted,
                CycleValue::to_i64,
            )),
        })
    }

    /// Store the value this cycle posted at into the cells whose deadband it
    /// crossed — C `monitor()`'s `prec->mlst = prec->val` /
    /// `prec->alst = prec->val`.
    ///
    /// C declares MLST/ALST with the record's VAL type, so the value is
    /// converted to whatever the cell holds. Same one-question reason as
    /// [`Self::monitor_deadband_cells`] — this is four more vtable calls
    /// otherwise.
    #[inline(always)]
    fn store_monitor_last_posted(&mut self, val: AlarmLimit, mlst: bool, alst: bool) {
        for (cell, store) in [(CycleCell::Alst, alst), (CycleCell::Mlst, mlst)] {
            if store {
                store_cycle_cell(self, cell, CycleValue::from(val));
            }
        }
    }

    /// The cells C `checkAlarms` reads straight off the record — VAL, HYST,
    /// LALM, AFTC and AFVL — in one question, in the ladder's own domain.
    /// `None` is a VAL the ladder has no arm for; a `None` HYST is one this
    /// type keeps in `CommonFields`, and a type with no LALM cell latches on
    /// VAL. A type with no AFTC field (`ao`, `longout`, `int64out`, `calcout`,
    /// per their `.dbd.pod`) has no filter block in C and answers AFTC = AFVL
    /// = 0: a disabled filter with nothing to clear.
    ///
    /// Same reason as [`Self::monitor_deadband_cells`]: the ladder runs on
    /// every cycle of every analog record, and asking [`Self::get_field`] by
    /// name is a full name match per cell, each answered in an `EpicsValue`
    /// the ladder then converts back to the number C reads as a struct
    /// member.
    #[inline(always)]
    fn analog_alarm_input(&self) -> Option<AnalogAlarmInput> {
        let val = match val_cell(self) {
            CycleRead::Value(v) => v.ladder()?,
            CycleRead::Absent => return None,
            CycleRead::ByName => AlarmLimit::from_ladder_value(&self.val()?)?,
        };
        let limit = |cell: CycleCell| match self.cycle_cell(cell) {
            CycleRead::Value(v) => Some(v.alarm_limit()),
            CycleRead::Absent => None,
            CycleRead::ByName => self
                .get_field(cell.name())
                .and_then(|v| AlarmLimit::from_stored(&v)),
        };
        let f64_of = |cell: CycleCell| match self.cycle_cell(cell) {
            CycleRead::Value(v) => v.to_f64(),
            CycleRead::Absent => 0.0,
            CycleRead::ByName => self
                .get_field(cell.name())
                .and_then(|v| v.to_f64())
                .unwrap_or(0.0),
        };
        Some(AnalogAlarmInput {
            val,
            hyst: limit(CycleCell::Hyst),
            lalm: limit(CycleCell::Lalm).unwrap_or(val),
            aftc: f64_of(CycleCell::Aftc),
            afvl: f64_of(CycleCell::Afvl),
        })
    }

    /// C `checkAlarms`' analog ladder — HIHI/HIGH/LOW/LOLO with HYST, the
    /// AFTC filter and the LALM latch — for a record with an analog alarm
    /// config. Not overridden, for the reason [`Self::monitor_value_classes`]
    /// gives: compiled per type, the cells are read and stored as members.
    fn check_analog_alarms(&mut self, common: &mut CommonFields) {
        analog_alarm_ladder(self, common);
    }

    /// Store the AFTC filter accumulator — C's `prec->afvl = afvl`.
    fn store_alarm_filter_value(&mut self, afvl: f64) {
        let _ = self.put_cycle_cell(CycleCell::Afvl, CycleValue::Double(afvl));
    }

    /// Arm C's `prec->lalm` latch, coerced to the type the cell is declared
    /// with: C declares LALM with the record's VAL type, so an `epicsInt64`
    /// latch must not round through a double.
    #[inline(always)]
    fn store_analog_lalm(&mut self, lalm: AlarmLimit) {
        store_cycle_cell(self, CycleCell::Lalm, lalm.into());
    }

    /// Which slots of [`Self::multi_input_links`] are wired, as two bitmasks
    /// over that list: `.0` the slots [`Self::link_text_ref`] lends a
    /// non-empty text for, `.1` the slots it declines to lend at all, which
    /// the caller must resolve through the instance instead.
    ///
    /// Asked as ONE question because the cycle otherwise asks one per declared
    /// link — 21 trips through the vtable for a `calc`, to learn that a stock
    /// database wires none of them. A trait default body is codegen'd per
    /// implementing type, so `multi_input_links` and `link_text_ref` inside
    /// this one are static calls the compiler can inline.
    ///
    /// Inlined is still one name match per declared link on every cycle of
    /// every record of the type — the cycle's most-repeated question, measured
    /// at 232 of a `calc` cycle's ~2570 TSC. A type whose link texts sit in one
    /// array answers off that array through [`input_link_slots_of`] and skips
    /// the walk; a type whose texts are separate fields hands the same helper
    /// an array of references to them. Either way the answer must be the one
    /// this body would give, which is what
    /// `a_record_that_reads_its_own_link_slots_answers_what_the_name_walk_would`
    /// pins for every declared type.
    ///
    /// `None` for a type declaring more links than a mask holds; the caller
    /// then walks the list link by link, as it did before this existed.
    fn set_input_link_slots(&self) -> Option<(u64, u64)> {
        let links = self.multi_input_links();
        if links.len() > u64::BITS as usize {
            return None;
        }
        let (mut set, mut unknown) = (0u64, 0u64);
        for (slot, (link_field, _)) in links.iter().enumerate() {
            match self.link_text_ref(link_field) {
                Some([]) => {}
                Some(_) => set |= 1 << slot,
                None => unknown |= 1 << slot,
            }
        }
        Some((set, unknown))
    }

    /// Whether this record's C `process()` performs the SCALAR closed-loop DOL
    /// fetch — `dbGetLink(&prec->dol, <dbr>, &prec->val, 0, 0)` guarded by
    /// `prec->dol.type != CONSTANT && prec->omsl == menuOmslclosed_loop`
    /// (`boRecord.c:191-205`, `busyRecord.c:196-208`, `dfanoutRecord.c:116-122`,
    /// and the same block in ao/longout/int64out/mbbo/mbboDirect/stringout/lso).
    /// The framework then owns the fetch, its LINK/INVALID failure arm
    /// ([`Self::closed_loop_dol_read_failed`]) and the UDF clear.
    ///
    /// Declaring both `menu(menuOmsl)` and `field(DOL,DBF_INLINK)` is NOT the
    /// same question and cannot stand in for this one: `aao` declares both and
    /// copies DOL as an ARRAY (`aaoRecord.c::fetchValue`), `motor` declares both
    /// and drives DOL through its own C code. Both source DOL record-locally and
    /// answer `false` here. `epid` has `menuOmsl` and no DOL at all — it fetches
    /// from `STPL`.
    ///
    /// This was a record-name match inside the process cycle. A list the
    /// compiler cannot check, in a file no record author reads, had already
    /// been wrong twice — `dfanout` once and `busy` again — so the fact moved
    /// to the record that owns it. Default: false.
    fn fetches_dol_closed_loop(&self) -> bool {
        false
    }

    /// C's failure arm for THIS cycle's closed-loop DOL read: the framework
    /// calls it when `dbGetLink(&prec->dol, ...)` returned a non-zero status
    /// with `OMSL = closed_loop` and a non-constant DOL.
    ///
    /// The LINK/INVALID alarm is not this hook's business — `db_get_link` owns
    /// it, exactly as C's `dbGetLink` does. What is left is the per-record body
    /// gate, and C writes a different one in each record:
    ///
    /// * ao — `fetch_value` (aoRecord.c:442) sets `prec->val = prec->pval`
    ///   BEFORE the read, and `if(!status) convert(prec, value)` (:188) then
    ///   skips the convert, so VAL falls back to the last actual output and
    ///   OVAL/PVAL/RVAL freeze (an OROC ramp stops advancing).
    /// * longout (:155) / int64out (:146) — same `if (!status) convert(...)`,
    ///   whose convert is the DRVH/DRVL clamp.
    /// * mbbo (:206) / mbboDirect (:186) — `goto CONTINUE` past `udf = FALSE`,
    ///   `bitsFromVAL`, `convert` and the pre-output timestamp.
    /// * bo (:200-204), stringout, lso, dfanout — nothing: bo converts VAL to
    ///   RVAL whether the read succeeded or not, and the other three have no
    ///   convert step at all. They keep the default no-op.
    ///
    /// Additive, framework-set-hook pattern, same shape as
    /// [`Self::set_fetch_gate_failed`]. Default: ignore.
    fn closed_loop_dol_read_failed(&mut self) {}

    /// The INP counterpart of [`Self::closed_loop_dol_read_failed`]: the
    /// framework calls it when a soft-channel `read_xxx`'s `dbGetLink(&prec->inp,
    /// ...)` returned a non-zero status, so no value was sourced this cycle.
    ///
    /// The LINK/INVALID alarm is not this hook's business — `db_get_link` owns
    /// it, as C's `dbGetLink` does. What is left is what the record makes of a
    /// cycle that read nothing, and C writes that per record:
    ///
    /// * subArray — `read_sa` skips `subset()` entirely on a non-zero status
    ///   (`devSASoft.c:118-120`), `readValue` returns it, and `process` does
    ///   `prec->udf = !!status` (`subArrayRecord.c:148`). An UNDEFINED subArray
    ///   then serves ZERO elements, not the stale slice
    ///   (`get_array_info`, `:181-184`) — the only array record with that rule.
    /// * waveform/aai/aao — nothing: they clear UDF on the line after
    ///   `readValue` whatever it returned ([`Self::clears_udf_unconditionally`]).
    /// * the scalar soft records — nothing here either; C leaves `prec->udf`
    ///   untouched on a failed read (`if (status == 0) … prec->udf = FALSE`),
    ///   which is the framework's per-cycle re-derive, not a record hook.
    ///
    /// Additive, framework-set-hook pattern. Default: ignore.
    fn soft_input_read_failed(&mut self) {}

    /// Report this cycle's subroutine status — C `process`'s `status` variable
    /// for `sub`/`aSub`:
    ///
    /// ```c
    /// status = fetch_values(prec);
    /// if (!status) { status = do_sub(prec); prec->val = status; }
    /// ...
    /// if (!status)                      /* aSubRecord.c:234-240 */
    ///     for (i = 0; i < NUM_ARGS; i++)
    ///         dbPutLink(&(&prec->outa)[i], (&prec->ftva)[i], (&prec->vala)[i],
    ///             (&prec->neva)[i]);
    /// ```
    ///
    /// so `0` — and only `0` — means the input fetch succeeded AND `do_sub` ran
    /// and returned success. It is the gate on aSub's OUT-link pushes.
    ///
    /// Delivered by `RecordInstance::run_registered_subroutine`, the single
    /// owner of the `do_sub` call, on every one of its exit paths (the
    /// suppressed cycle, no bound routine, the routine's own return).
    ///
    /// Default: ignore (records with no subroutine).
    fn set_subroutine_status(&mut self, _status: i64) {}

    /// Called before/after a field put for side-effect processing. C's
    /// `special(paddr, after)` reaches the whole record through the dbAddr;
    /// `common` is the record's common half (DISA, DISV, SCAN, ...).
    fn special(
        &mut self,
        _field: &str,
        _after: bool,
        _common: &crate::server::record::CommonFields,
    ) -> CaResult<()> {
        Ok(())
    }

    /// The period of this record's monitor watchdog, or `None` when it has
    /// none — C `histogramRecord.c::wdogInit` (:126-152):
    ///
    /// ```c
    /// static void wdogInit(histogramRecord *prec) {
    ///     if (prec->sdel > 0) { ... callbackRequestDelayed(&pcallback->callback, prec->sdel); }
    /// }
    /// ```
    ///
    /// A watchdog is NOT a process cycle: it posts monitors for a record whose
    /// value is changing but whose deadband (histogram MDEL) is holding the
    /// posts back, so a slow accumulation still reaches a display. The
    /// framework re-reads this on every tick, so clearing SDEL stops the
    /// watchdog at the next fire without any separate cancel.
    ///
    /// histogram is the only base record with one. Default: no watchdog.
    fn watchdog_interval(&self) -> Option<std::time::Duration> {
        None
    }

    /// One watchdog tick — C `histogramRecord.c::wdogCallback` (:102-124):
    ///
    /// ```c
    /// if (prec->mcnt > 0) {
    ///     dbScanLock(prec);
    ///     recGblGetTimeStamp(prec);
    ///     db_post_events(prec, &prec->val, DBE_VALUE | DBE_LOG);
    ///     prec->mcnt = 0;
    ///     dbScanUnlock(prec);
    /// }
    /// ```
    ///
    /// The record performs its own state change (histogram: zero MCNT) and
    /// returns the fields whose monitors the framework must post — the
    /// `db_post_events` half, which a record cannot do itself. An empty slice
    /// means "nothing changed since the last tick": no timestamp, no post. The
    /// framework holds the record lock across the call (C `dbScanLock`) and
    /// re-arms afterwards from [`Self::watchdog_interval`].
    ///
    /// Default: nothing to post.
    fn watchdog_fire(&mut self) -> &'static [&'static str] {
        &[]
    }

    /// The body of the delayed callback armed by
    /// [`ProcessAction::DelayedCallbackAfter`] — C `boRecord.c::myCallbackFunc`
    /// (:105-118):
    ///
    /// ```c
    /// if (prec->pact) {
    ///     if ((prec->val == 1) && (prec->high > 0)) { callbackRequestDelayed(cb, prec->high); }
    /// } else {
    ///     prec->val = 0;
    ///     dbProcess((struct dbCommon *)prec);
    /// }
    /// ```
    ///
    /// Run under the record gate (C `dbScanLock`) when the timer fires, and
    /// before any `process()` re-entry. `pact` is the record's PACT at fire
    /// time, which the record cannot read for itself.
    ///
    /// This hook is the ONLY place the one-shot's mutation may happen: keeping
    /// it out of `process()` is what stops an unrelated process cycle from
    /// consuming it. Default: re-enter `process()` and change nothing, so a
    /// record that emits the action without overriding still behaves as a plain
    /// [`ProcessAction::ReprocessAfter`].
    fn delayed_callback_fire(&mut self, _pact: bool) -> DelayedCallbackOutcome {
        DelayedCallbackOutcome::Reprocess
    }

    /// The [`ProcessAction::ValuePutAfter`] handler, run under the record
    /// gate: C throttle's `valuePut` (`throttleRecord.c:540-613`). Its posts
    /// go out as [`Self::take_cycle_posted_fields`] marks.
    fn delayed_value_put(&mut self) -> ProcessActions {
        ProcessActions::new()
    }

    /// C `valuePut`'s tail (`throttleRecord.c:592-612`): the STS post, then
    /// what `recGblResetAlarms` and the SENT post mark, taken by the owner
    /// only after `recGblFwdLink` and stamped just ahead of the first `Reset*`
    /// entry. Never part
    /// of [`Self::take_cycle_posted_fields`], which a link read inside
    /// `valuePut` drains. Empty unless a `valuePut` ran since the last take.
    fn take_value_put_tail(&mut self) -> CycleMarks {
        CycleMarks::NONE
    }

    /// Whether this cycle's `monitor()` runs `recGblResetAlarms`. Only a
    /// [`CycleHooks::MONITOR_RESET`] type is asked, once, on a cycle that
    /// reaches `monitor()`, so it may advance the state C's `monitor()` keeps
    /// (scanparm `lstp = step`, `scanparmRecord.c:282`). A cycle that answers
    /// `false` leaves the pending NSTA/NSEV for the next reset.
    fn monitor_resets_alarms(&mut self) -> bool {
        true
    }

    /// Whether the `process()` that just ran called C `recGblGetTimeStamp`,
    /// asked once per pass at the pass's stamp point, so an answer of `true`
    /// is cleared by the asking. Only a [`CycleHooks::STAMP_REQUEST`] type is
    /// asked; its pass that answers `false` leaves TIME as it was. The stamp
    /// lands after `process()` returns and reaches the record through
    /// [`Self::time_stamped`].
    fn take_time_stamp_request(&mut self) -> bool {
        false
    }

    /// The TIME the stamp point just stored for a pass whose
    /// [`Self::take_time_stamp_request`] answered `true`, TSEL applied, before
    /// the pass posts: C's code after `recGblGetTimeStamp` reads the fresh
    /// `prec->time` (mcaRecord.c:762-764 RTIM, :811-814 STIM).
    fn time_stamped(&mut self, _time: crate::types::WallTime) {}

    /// Whether this record type's support reads SIML through the `recGbl`
    /// simulation helpers (`recGblGetSimm`/`recGblInitSimm`, and therefore
    /// `recGblSaveSimm`/`recGblCheckSimm`) rather than a bare `dbGetLink`.
    ///
    /// ONE C fact, two consequences — which is why it is one predicate:
    ///
    /// - **The SCAN swap.** The helpers take `&prec->sscn` and `&prec->oldsimm`,
    ///   so only a record that declares those fields can call them, and only
    ///   such a record swaps SCAN with SSCN on a SIMM transition
    ///   (`recGblCheckSimm`, `recGbl.c:427-437`).
    /// - **The alarm on a failed SIML read.** `recGblGetSimm` reads SIML with
    ///   `dbTryGetLink` — which does NOT call `setLinkAlarm` — and then raises
    ///   the alarm itself, by writing `nsta` DIRECTLY:
    ///   `if (status && !pcommon->nsev) pcommon->nsta = LINK_ALARM;`
    ///   (`recGbl.c:454`). SEVR is left alone. A record reading SIML with a
    ///   plain `dbGetLink` (`busyRecord.c:399`, `swaitRecord.c:402`) instead
    ///   gets `setLinkAlarm` (`dbLink.c:319-323`) →
    ///   `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM)`, a full severity raise.
    ///
    /// The 21 base records that declare SSCN answer `true`; `busy` and `swait`
    /// carry SIMM/SIML/SIOL but neither SSCN nor OLDSIMM. Records with no
    /// simulation block answer `false` trivially.
    ///
    /// The default consults [`record_type_has_sscn`](crate::server::recgbl::simm::record_type_has_sscn),
    /// which is enumerated from the C dbd files, so no record has to restate it.
    fn uses_recgbl_simm_helpers(&self) -> bool {
        crate::server::recgbl::simm::record_type_has_sscn(self.record_type())
    }

    /// The record's `readValue`/`writeValue` ABORTS when the SIML read fails —
    /// it returns before performing any I/O, so the cycle does no device write,
    /// no SIOL redirect, and raises no SIMM_ALARM.
    ///
    /// `busy` is the only record that does this (busyRecord.c:399-401):
    ///
    /// ```c
    /// status = dbGetLink(&prec->siml, DBR_USHORT, &prec->simm, 0, 0);
    /// if (status)
    ///     return(status);          /* <-- before write_busy AND before dbPutLink */
    /// ```
    ///
    /// The LINK_ALARM that `dbGetLink`'s `setLinkAlarm` already raised is the
    /// cycle's only simulation alarm.
    ///
    /// The other two families do NOT abort, for different reasons:
    ///
    /// - The 21 [`Self::uses_recgbl_simm_helpers`] records look like they do —
    ///   `readValue` has `status = recGblGetSimm(...); if (status) return status;`
    ///   (longinRecord.c:403-405) — but `recGblGetSimm` ends with an
    ///   unconditional `return 0` (recGbl.c:456), so that branch is DEAD and the
    ///   record always proceeds to its `switch (prec->simm)`.
    /// - `swait` reads SIML with a plain `dbGetLink` and simply never tests the
    ///   status (swaitRecord.c:402), so it proceeds too.
    ///
    /// Default: `false`.
    fn aborts_on_failed_siml_read(&self) -> bool {
        false
    }

    /// The record joined (`true`) or left (`false`) the `SCAN="I/O Intr"` list.
    ///
    /// C parity: `dbScan.c::scanAdd` calls the record's device support
    /// `get_ioint_info(0, precord, &iopvt)` when SCAN becomes `I/O Intr`, and
    /// `scanDelete` calls `get_ioint_info(1, ...)` when it leaves. Device
    /// support that registers driver interrupt callbacks does so there —
    /// `asynRecord.c:582-597` registers/cancels its per-interface interrupt
    /// users in exactly those two calls, and clears its `gotValue` cell on the
    /// register.
    ///
    /// The port's device supports own their subscription through
    /// [`crate::server::device_support::DeviceSupport::io_intr_receiver`],
    /// which the framework asks for once at `iocInit`. This hook is the
    /// *runtime* half: a record whose own state decides what to subscribe to
    /// (asynRecord's PORT/IFACE/UI32MASK/REASON) must (re)register when the
    /// operator moves SCAN in or out of `I/O Intr` after `iocInit`.
    ///
    /// Called from the single owner of the SCAN transition
    /// (`RecordInstance::put_common_field*`, and the `scanAdd`-failure demotion
    /// in `ioc_app`) only when I/O Intr membership actually changes, so it is
    /// never invoked twice for the same state.
    ///
    /// Returns `false` when joining is refused — C `get_ioint_info(0)`
    /// returning non-zero, on which `scanAdd` leaves the record Passive
    /// (dbScan.c:286-288). Default: accept.
    fn set_io_intr_scan(&mut self, _active: bool) -> bool {
        true
    }

    /// iocInit's `scanAdd` (`buildScanLists`, dbScan.c:286) for a record that
    /// loaded with `SCAN="I/O Intr"`: the registration
    /// [`Self::set_io_intr_scan`] deferred at load may exist now. `false` is C's
    /// `get_ioint_info(0)` refusal, and the framework demotes the record to
    /// Passive. Default: accept.
    fn io_intr_scan_add(&mut self) -> bool {
        true
    }

    /// The link writes a C `special()` performs *itself*, inside `dbPut`.
    ///
    /// `special()` takes the record alone — it has no database handle — so a
    /// record whose C `special()` calls `dbPutLink` cannot make that write from
    /// `special()`. It queues the write here instead, and the put owner
    /// (`field_io`'s `dbPut` paths) drains the queue immediately after
    /// `special(field, true)` returns and executes the actions BEFORE the put's
    /// `pp(TRUE)` process cycle. That is C's order: `dbPutField` → `dbPut` →
    /// `dbPutSpecial(paddr, 1)` — which runs the `dbPutLink` to completion,
    /// target processing included — → `dbProcess`.
    ///
    /// The scaler is the case this exists for: `scalerRecord.c:623-624` puts
    /// `CNT` to `COUTP` inside `special()`, so a record wired to `.COUTP` is
    /// processed while the scaler is still IDLE, before the count is armed. The
    /// port deferred that write to the head of the CNT-triggered process cycle,
    /// where the target saw an already-COUNTING scaler.
    ///
    /// This is neither the record's "should the link fire" state nor part of the
    /// process cycle's action list: a `special()` put and a `process()` put to
    /// the same link (scaler `COUTP` again, `:463`) are independent writes.
    ///
    /// The drain is unconditional — it runs even when `special()` returned an
    /// error — so a queued action can never survive the put that queued it and
    /// fire against a later, unrelated put.
    ///
    /// Default: none (a `special()` that writes no link).
    fn take_special_actions(&mut self) -> Vec<ProcessAction> {
        Vec::new()
    }

    /// The [`AfterGate`] work the last `special(_, true)` left, taken by the
    /// put path beside [`Self::take_special_actions`] and run once the put's
    /// gate drops.
    ///
    /// Default: none.
    fn take_after_gate(&mut self) -> Option<AfterGate> {
        None
    }

    /// The [`PutReport`](crate::server::recgbl::PutReport) the last
    /// `special(_, true)` made in place of printing under the hold, taken by
    /// the put path and printed once the put's gate drops. Asked after a
    /// refused `special()`, and after an accepted one only for a type that
    /// declares [`CycleHooks::PUT_REPORT`].
    ///
    /// Default: none.
    fn take_put_report(&mut self) -> Option<crate::server::recgbl::PutReport> {
        None
    }

    /// A `special()`/`put_field` pass that ended on C's `prec->udf = FALSE`.
    ///
    /// UDF is a common field, so a record method cannot write it — the same
    /// wall [`Self::post_init_finalize_undef`] exists for at init time. This is
    /// the runtime half: `histogramRecord.c:354-364`'s `clear_histogram` ends
    /// on `prec->udf = FALSE` and is reached from two puts (the `CMD <= 1`
    /// SPC_CALC arm `:246-259` and the ULIM/LLIM SPC_RESET arm `:266-273`), so
    /// latching inside the clear rather than at each caller is what makes the
    /// two agree by construction.
    ///
    /// Drained unconditionally by the after-put owner, for
    /// [`Self::take_special_actions`]'s reason: C performs this assignment
    /// inside `special()`, before any status can divert the caller.
    ///
    /// Default: none (the record's puts never clear UDF).
    fn take_udf_clear(&mut self) -> bool {
        false
    }

    /// A `special(_, true)` pass that stored the common `SCAN` field itself,
    /// as `sscanRecord.c:1571` does with `psscan->scan = 0`: a store, not a
    /// `dbPut`, so no `scanDelete`/`scanAdd` runs. The record stays on the scan
    /// list it was on and goes on being processed there, because C's `scanList`
    /// never reads `SCAN` (`dbScan.c:1014-1019`). A later `SCAN` put deletes
    /// from the list the stored value names (none for Passive, `dbScan.c:318`)
    /// and adds to the new one, so the record is then on both; C's `addToList`
    /// re-links its one `scan_element` there and corrupts the old list
    /// (`:1083-1095`), which the port's per-list index does not reproduce.
    ///
    /// Drained beside [`Self::take_udf_clear`], ahead of the pass's posts, so a
    /// mark on `SCAN` posts the stored value.
    ///
    /// Default: none.
    fn take_scan_store(&mut self) -> Option<super::ScanType> {
        None
    }

    /// True iff the C `special()` for a put to `put_field` runs the record's
    /// own `monitor()` — whose first act is `recGblResetAlarms(prec)`, which
    /// commits `nsta`/`nsev` into `stat`/`sevr` and clears the born-UDF alarm.
    ///
    /// `compress` is the case this exists for: `compressRecord.c::special`
    /// (:385-388) calls `reset(prec); monitor(prec);` on any SPC_RESET write
    /// (RES/ALG/PBUF/BALG/N), and `compressRecord.c::monitor` (:103) opens with
    /// `recGblResetAlarms`. A born-UDF compress record therefore transitions to
    /// NO_ALARM the moment one of those fields is put, with no process cycle.
    ///
    /// `special()` marks that `monitor()`'s posts as
    /// [`CyclePostMask::ResetValueLog`], whose drain commits the alarm; this
    /// hook only makes the CA route run `special()` on a rejected conversion
    /// too (`dbAccess.c:1398`). Default: false.
    fn special_commits_alarms(&self, _put_field: &str) -> bool {
        false
    }

    /// True iff the C `special()` for a put to `put_field` runs code that
    /// writes `stat`/`sevr` DIRECTLY (not `nsta`/`nsev`) with NO `monitor()` /
    /// `recGblResetAlarms` after it — so the write STICKS and a later `caget`
    /// observes it, unlike the process path where `recGblResetAlarms` erases it.
    ///
    /// `histogram` is the case this exists for: a `.SGNL` caput is C's SPC_MOD
    /// `special()` → `add_count`, which writes `prec->stat = SOFT_ALARM` on
    /// inverted limits (histogramRecord.c:329-334) and returns with no monitor.
    /// When true, the put owner (`field_io`) runs
    /// [`Record::check_alarms`] after the store — the same direct write the
    /// process path makes, but here it persists because no process cycle
    /// follows. Default: false (no direct special-path alarm write).
    fn special_checks_alarms(&self, _put_field: &str) -> bool {
        false
    }

    /// True iff the C `special()` for a put to `put_field` calls the
    /// record's `checkLink` before it acts on the link statuses — throttle's
    /// SYNC arm (`throttleRecord.c:381-386`). The put owner (`field_io`)
    /// runs [`Record::link_check`]'s check under the record lock ahead of
    /// `special()`, which then reads the statuses it left. Default: false.
    fn special_checks_links(&self, _put_field: &str) -> bool {
        false
    }

    /// Downcast to the concrete type, for a device support that reaches
    /// its record's C members.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self.record_any_mut())
    }

    /// C's `*(void **)pfield` for a `DBF_NOACCESS` pointer member that only
    /// this record type's own state can answer (`dbpr`'s `PTR %p` arm,
    /// `dbTest.c:1239-1242`): the address of the object the record holds for
    /// `field`, `None` where it holds none. Everything a record holds in the
    /// framework (`RSET`, `DSET`, `LSET`, ...) is answered by `dbpr` itself.
    fn held_pointer(&self, _field: &str) -> Option<usize> {
        None
    }

    /// Whether processing this record should clear UDF.
    /// Override to return false for record types that don't produce a valid value every cycle.
    fn clears_udf(&self) -> bool {
        true
    }

    /// Whether a soft-channel INP read that landed a value clears UDF. This
    /// is the soft dset's behaviour, separate from [`Self::clears_udf`]:
    /// `devSiSoft.c:72` and `devEventSoft.c:84` clear it, `devLsiSoft.c`
    /// does not.
    fn soft_read_clears_udf(&self) -> bool {
        true
    }

    /// Does this record's C `process()` re-derive `udf` after a DEVICE read
    /// that wrote VAL directly (C `return 2`)?
    ///
    /// True where the `else if (status == 2) status = 0;` fold happens BEFORE
    /// the UDF assignment, so a `2` arrives at it as a `0` and the record
    /// re-derives whatever the dset already wrote: `aiRecord.c:158-161` is the
    /// shape, and it is the majority.
    ///
    /// False for the five records that keep the assignment INSIDE
    /// `if (status == 0)` with the fold after it — `biRecord.c:55-60`,
    /// `mbbiRecord.c:61-86`, `mbbiDirectRecord.c:58-69` — or that have no fold
    /// at all — `longinRecord.c:58`, `int64inRecord.c:58`. There `udf` belongs
    /// to the dset, which is exactly where C puts it: `devBiSoft.c::readLocked`
    /// writes `prec->udf = FALSE` and returns 2, and `devBiDbState.c:67` does
    /// the same. The port's equivalent is
    /// [`DeviceUdf`](crate::server::device_support::DeviceUdf), applied before
    /// this rule runs.
    ///
    /// Not consulted for a soft-channel INP read: there the framework IS the
    /// dset, so it owns the clear the way `readLocked` does.
    fn rederives_udf_on_computed_read(&self) -> bool {
        true
    }

    /// Whether this record's C `process()` clears UDF regardless of the read's
    /// status — i.e. even when `readValue` failed.
    ///
    /// Most records gate the clear on the read: `if (status == 0) prec->udf =
    /// FALSE;` inside `readValue`'s SIOL branch (`longinRecord.c:418`), so a
    /// failed simulation read leaves the record undefined. The array records do
    /// NOT: their `process()` clears UDF itself, unconditionally, on the line
    /// after `readValue` returns — `prec->pact = TRUE; prec->udf = FALSE;`
    /// (`waveformRecord.c:143-144`, `aaiRecord.c:173-174`) and
    /// `if (!pact) { prec->udf = FALSE; ... }` (`aaoRecord.c:164-165`) — whatever
    /// status came back, including the `-1` of an illegal SIMM. (waveform's
    /// readValue also clears UDF on a good SIOL read, `:353`, but the
    /// process-level clear runs after it and dominates.)
    ///
    /// Consulted by the simulation tail, which otherwise gates the clear on the
    /// SIOL fetch status. Default `false` — the status-gated majority.
    fn clears_udf_unconditionally(&self) -> bool {
        false
    }

    /// Does this record's C `process()` assign `udf` on a cycle whose INP read
    /// FAILED?
    ///
    /// The scalar input records do not — the assignment sits inside
    /// `if (status == 0)`: `aiRecord.c:161`, `biRecord.c:136-140`,
    /// `mbbiRecord.c:168-174`, `mbbiDirectRecord.c:155-164`,
    /// `longinRecord.c:148`, `int64inRecord.c:144`. A broken link therefore
    /// leaves the record at whatever UDF it already had, and that is what makes
    /// the `if (prec->udf) recGblSetSevr(prec, UDF_ALARM, ...)` on the next line
    /// reachable at all (`mbbiDirectRecord.c:168-169`).
    ///
    /// The array records and `compress` do — `prec->udf = FALSE` runs after
    /// `readValue` whatever it returned (`waveformRecord.c:144`,
    /// `aaiRecord.c:174`, `aaoRecord.c:165`); `compressRecord.c:342-366` folds a
    /// failed `dbGetLink` into `status = 0` and clears anyway; and subArray's
    /// `prec->udf = !!status` (`subArrayRecord.c:148`) IS the status.
    ///
    /// Default `false` — the status-gated majority. Records whose
    /// [`Self::clears_udf`] is false never reach the question: their gate is
    /// already "a value was sourced this cycle", which a failed read fails.
    fn derives_udf_on_read_failure(&self) -> bool {
        false
    }

    /// Whether this record type's `.dbd` declares an `INP` field at all.
    ///
    /// The port keeps INP on `CommonFields` for every record, which is right for
    /// the input records (`aiRecord.dbd.pod` … all declare it) but wrong for the
    /// ones whose C `.dbd` has no INP: C's dbd is the gate there, and
    /// `field(INP,...)` on such a record is a load error ("field not found"),
    /// leaving the record inert.
    ///
    /// `histogram` is the case this exists for: `histogramRecord.dbd.pod`
    /// declares NO INP — its DBF_INLINK is `SVL` (:212), read into SGNL by
    /// `devHistogramSoft.c` — so a histogram driven from INP is a database that
    /// no C IOC can load. Default `true`.
    ///
    /// This is the whole namespace gate, not just the loader's: in C the dbd
    /// also decides which `.FIELD` channels exist, so a histogram's INP is not
    /// resolvable at all (`dbgf HI.INP` → `PV 'HI.INP' not found`).
    /// [`RecordInstance::get_common_field`] consults this; the common-field
    /// store asks the type's declaration, which must agree with it.
    ///
    /// [`RecordInstance::get_common_field`]: crate::server::record::RecordInstance::get_common_field
    fn declares_inp_link(&self) -> bool {
        true
    }

    /// Process-time INP read when the INP link is CONSTANT (or unset) — the
    /// per-record exception to the load-once rule.
    ///
    /// Nearly every soft input device support skips a constant INP at process:
    /// `devWfSoft.c::read_wf` and `devAaiSoft.c::read_aai` open with
    /// `if (dbLinkIsConstant(pinp)) return 0;`, and the scalar ones call
    /// `dbGetLink`, whose `dbConstGetValue` (`dbConstLink.c:219-225`) writes
    /// nothing. The constant reaches such a record ONCE, at init, through
    /// `recGblInitConstantLink` / `dbLoadLinkArray`
    /// (`PvDatabase::rec_gbl_init_constant_inp`), so a client's caput to VAL is
    /// never clobbered by a re-delivered constant.
    ///
    /// `devSASoft.c::read_sa` (92-123) is the documented exception: it re-runs
    /// `dbLoadLinkArray` on a constant INP EVERY process, and on an EMPTY INP
    /// (`S_db_badField`) it sets `nRequest = prec->nord` and still subsets — so
    /// the record re-slices the client-written VAL by INDX each cycle. C draws
    /// that line at the device-support layer (`devSASoft` vs `devAaiSoft`), and
    /// so does this hook.
    ///
    /// Called by the framework on a soft-DTYP cycle whose INP is constant
    /// (`ParsedLink::Constant`) or unset (`ParsedLink::None`). The link, not its
    /// value, is passed so the load-once records copy nothing. Returns whether
    /// the record consumed the input stage. Default `false` — the load-once rule.
    fn read_constant_inp(&mut self, _inp: &crate::server::record::ParsedLink) -> bool {
        false
    }

    /// Whether this record type raises UDF_ALARM at all.
    ///
    /// C has no framework-level UDF alarm: every record that reports one does
    /// it itself, with the guard at the top of its own `checkAlarms` —
    /// `if (prec->udf) { recGblSetSevr(prec, UDF_ALARM, prec->udfs); return; }`
    /// (`aiRecord.c:319-323`, `calcRecord.c:300-304`, …). A record whose
    /// support has no such guard NEVER reports UDF_ALARM, no matter what its
    /// `UDF` field says — `swaitRecord.c` is the case in point: its two only
    /// `udf` statements are `udf = FALSE` (`:411`, `:419`); it has no
    /// `checkAlarms` and never names UDF_ALARM.
    ///
    /// The framework raises UDF_ALARM centrally (`rec_gbl_check_udf`), so this
    /// hook is where a record type says C does not. Default `true` — the base
    /// analog/binary/string records all carry the guard.
    fn raises_udf_alarm(&self) -> bool {
        true
    }

    /// Whether this record's C `checkAlarms` tests the UDF byte with
    /// `if (prec->udf == TRUE)` (EXACT-ONE) rather than `if (prec->udf)`
    /// (truthy). See [`crate::server::recgbl::udf_alarm_active`]: exact-one
    /// records (`boRecord.c:371`, `stringoutRecord.c:146`, `biRecord.c:225`,
    /// `busyRecord.c:337`) do NOT raise UDF_ALARM for a `udf` byte that is
    /// neither 0 nor 1.
    ///
    /// This only changes behavior for a record whose `udf` byte can actually
    /// hold such a value at `checkAlarms` time — one that does NOT re-derive
    /// `udf` every cycle ([`Record::clears_udf`] `== false`), reached via a
    /// direct `caput .UDF 255` (or `-1`, stored as `255` in the `DBF_UCHAR`
    /// field). For the re-deriving records (`clears_udf() == true`, e.g.
    /// `bi`/`busy`/the calc family) the byte is always 0/1 here, so exact-one
    /// and truthy agree and the flag is left at its default. Default `false`.
    fn udf_alarm_on_exact_one(&self) -> bool {
        false
    }

    /// The severity C raises `UDF_ALARM` at, or `None` to use the record's
    /// live `UDFS` field.
    ///
    /// 19 of the 21 `UDF_ALARM` raise sites in `std/rec` pass `prec->udfs`;
    /// exactly two pass the literal `INVALID_ALARM` — `lsoRecord.c:117-118`
    /// and `mbbiDirectRecord.c:168-169`. Nothing derives that split: the
    /// Direct pair disagrees with itself (`mbboDirectRecord.c:191` passes
    /// `prec->udfs`), and so does the long-string pair (`lsi` raises no UDF
    /// alarm at all). It is a per-record fact, so it belongs on the record and
    /// not in a two-name branch inside `rec_gbl_check_udf`.
    ///
    /// Overriding this makes `UDFS` inert for that record, which is the point:
    /// `rec_gbl_set_sevr_msg` is strict-greater, so `UDFS=NO_ALARM` on an `lso`
    /// otherwise raised nothing at all where C reports INVALID/UDF.
    fn udf_alarm_severity(&self) -> Option<crate::server::record::AlarmSeverity> {
        None
    }

    /// The alarm message C attaches when raising `UDF_ALARM`.
    ///
    /// Almost every base record raises UDF with plain
    /// `recGblSetSevr(prec, UDF_ALARM, prec->udfs)` — and `recGblSetSevr`
    /// forwards a NULL message to `recGblSetSevrMsg`, which sets
    /// `namsg[0] = '\0'` (`recGbl.c:249-251,258-261`). So the C amsg for a
    /// UDF record is EMPTY, and pvxs then serves the `"UDF"` condition
    /// string for `alarm.message` (`iocsource.cpp:230-236`). Default `""`
    /// models exactly that.
    ///
    /// The sole exception in base is `mbboDirectRecord.c:191`, which raises
    /// `recGblSetSevrMsg(prec, UDF_ALARM, prec->udfs, "UDFS")` — a bespoke
    /// literal. That record overrides this to `"UDFS"`.
    fn udf_alarm_message(&self) -> &'static str {
        ""
    }

    /// Whether the record's current `VAL` is undefined (UDF must
    /// stay set).
    ///
    /// C parity: `aiRecord.c:285` / `calcRecord.c::checkAlarms` /
    /// `int64inRecord.c:144` clear `UDF` **only** when the computed /
    /// read value is valid — `if (status == 0)` and, for floating
    /// records, only when `VAL` is not NaN. The framework owns
    /// `common.udf`; it calls `clears_udf()` to decide whether this
    /// record type clears UDF at all, then this method to decide
    /// whether the *value produced this cycle* is actually defined.
    ///
    /// Default: a floating `VAL` that is NaN (e.g. a calc
    /// divide-by-zero, or a soft input whose link read failed and
    /// left VAL un-updated) is undefined; everything else is defined.
    /// A record whose `val()` yields `None` (no primary value) is
    /// also treated as undefined.
    fn value_is_undefined(&self) -> bool {
        match val_cell(self) {
            CycleRead::Value(v) => return v.is_nan(),
            CycleRead::Absent => return true,
            CycleRead::ByName => {}
        }
        match self.monitor_value_view() {
            Some(ValueView::Scalar(v)) => return v.is_nan(),
            Some(_) => return false,
            None => {}
        }
        match self.val() {
            Some(EpicsValue::Double(v)) => v.is_nan(),
            Some(EpicsValue::Float(v)) => v.is_nan(),
            Some(_) => false,
            None => true,
        }
    }

    /// Per-record alarm hook — evaluate record-type-specific alarms
    /// (STATE / COS / analog limit / SOFT) and accumulate them into
    /// `nsta`/`nsev` via `recGblSetSevr`.
    ///
    /// The framework centralises the generic alarm machinery (UDF
    /// check, `recGblResetAlarms` transfer, MS/MSI/MSS link-alarm
    /// inheritance). The record-type-specific severity logic that C
    /// puts in each record's `checkAlarms()` belongs here so a record
    /// can raise its own alarms without the framework hardcoding a
    /// per-type `match` on `record_type()`.
    ///
    /// `common` is the record's [`crate::server::record::CommonFields`]; implementations
    /// raise alarms with [`crate::server::recgbl::rec_gbl_set_sevr`]
    /// / [`crate::server::recgbl::rec_gbl_set_sevr_msg`].
    ///
    /// Default: no-op — records that have not yet migrated their
    /// `checkAlarms` logic here are still covered by the framework's
    /// legacy centralised `evaluate_alarms` match.
    fn check_alarms(&mut self, _common: &mut crate::server::record::CommonFields) {}

    /// Return multi-input link field pairs: (link_field, value_field).
    /// Override in calc, calcout, sel, sub to return INPA..INPL → A..L mappings.
    fn multi_input_links(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }

    /// Deliver a multi-input link's scalar read to `field`, a value field of
    /// [`Self::multi_input_links`] — C's `dbGetLink(plink, DBR_DOUBLE,
    /// pvalue, 0, 0)` landing in `&prec->a` (`calcRecord.c:434`). The
    /// default routes through [`Self::put_field_internal`]; a record whose
    /// value fields are a plain `f64` block stores directly, since the
    /// by-name path resolves the field three times to reach the same store.
    fn put_multi_input_f64(&mut self, field: &'static str, value: f64) -> CaResult<()> {
        self.put_field_internal(field, EpicsValue::Double(value))
    }

    /// The field at `slot`, borrowed as the Rust type the record stores it
    /// as. `None` (the default) for a field the type does not map; every
    /// read of it then goes through [`Self::get_field`] by name.
    /// [`slot_map!`](crate::slot_map) writes this and [`Self::slot_mut`].
    fn slot_ref(&self, _slot: FieldSlot) -> Option<SlotRef<'_>> {
        None
    }

    /// The field at `slot`, writable. A store through it MUST equal what
    /// [`Self::put_field`] stores for the same value; a field `put_field`
    /// refuses has no `slot_mut`.
    fn slot_mut(&mut self, _slot: FieldSlot) -> Option<SlotMut<'_>> {
        None
    }

    /// Where a `.db` `field()` of the declared field at `slot` is stored at
    /// load. `None` (the default) for a type [`slot_map!`](crate::slot_map)
    /// does not yet classify; the load then asks [`Self::implements_field`].
    fn declared_store(&self, _slot: FieldSlot) -> Option<DeclaredStore> {
        None
    }

    /// C `dbFldDes::offset` for `field` (upper-case): its index in
    /// [`FieldDeclaration::field_list`], settled once per link target as `dbDbInitLink`
    /// settles a `dbAddr`. A type hands out a slot ONLY for a field
    /// [`Self::slot_ref`] answers; `None` sends every read of the field
    /// through [`Self::get_field`] by name.
    fn field_slot(&self, field: &str) -> Option<FieldSlot> {
        let at = self.field_list().iter().position(|f| f.name == field)?;
        let slot = FieldSlot(u16::try_from(at).ok()?);
        self.slot_ref(slot).is_some().then_some(slot)
    }

    /// The field at `slot` as `dbGet(DBR_DOUBLE)` delivers it: what
    /// [`Self::get_field`] followed by the numeric funnel
    /// ([`EpicsValue::into_double`] / `get_convert_f64`) produces, without
    /// the value built and taken apart on the way. Defined for every slot
    /// [`Self::field_slot`] hands out; `None` for any other.
    fn get_slot_f64(&self, slot: FieldSlot) -> Option<f64> {
        self.slot_ref(slot)?.to_f64()
    }

    /// Store a multi-input link's scalar read at `slot`, the
    /// [`Self::field_slot`] of one of [`Self::multi_input_links`]' value
    /// fields — C's `dbGetLink(plink, DBR_DOUBLE, &prec->a + i, ...)` store
    /// through the address instead of the name. `true` when stored, which
    /// the default does for an `f64` [`Self::slot_mut`] only; otherwise the
    /// fetch falls back to [`Self::put_multi_input_f64`]. A type that
    /// overrides this must store exactly what `put_multi_input_f64` stores
    /// for that field.
    fn put_slot_f64(&mut self, slot: FieldSlot, value: f64) -> bool {
        match self.slot_mut(slot) {
            Some(SlotMut::F64(v)) => {
                *v = value;
                true
            }
            _ => false,
        }
    }

    /// The `(link_field, value_field)` pairs whose CONSTANT value this record's
    /// C `special()` RE-SEEDS on a runtime put to the link field —
    /// `recGblInitConstantLink(plink, DBF_DOUBLE, pvalue)` +
    /// `db_post_events(prec, pvalue, DBE_VALUE)` + `INAV = CON`
    /// (`sCalcoutRecord.c:513-518`, `aCalcoutRecord.c:533-538`).
    ///
    /// Without it a constant link is load-once dead state: `caput CO.INPB 7`
    /// stores the link text, the link layer then delivers nothing at process
    /// time (a constant link is not read), and `B` keeps its `.db`-load value
    /// forever.
    ///
    /// Declaring the pair is all a record does — the put path
    /// (`database::field_io::special_after_put`, the one `special(field, true)`
    /// owner) runs the load through
    /// [`crate::server::record::rec_gbl_init_constant_link`], the same owner the
    /// init seed uses, and posts the value field. A record cannot declare the
    /// pair and forget to implement the re-seed.
    ///
    /// Default: EMPTY — and that is the correct answer for every record whose C
    /// `special()` does NOT re-seed. `recGblInitConstantLink` appears inside a
    /// `special()` body in exactly FOUR record types across base and synApps
    /// calc — calcout, sCalcout, aCalcout, transform. calcout keeps the default:
    /// `dbPutFieldLink` runs its `special()` before `dbAddLink`
    /// (`dbAccess.c:1181-1208`), so the load finds no `lset` and seeds
    /// nothing, and calcout posts the value field and `INxV` from its own
    /// `special`. Everywhere else (calc,
    /// sub, sel, aSub, seq, fanout, dfanout, swait, sseq, ao/bo/longout/…) it is
    /// called only from `init_record`, so those records seed once and never
    /// again, and they inherit this empty default.
    ///
    /// The overriding records list only the inputs C actually re-seeds:
    /// sCalcout/aCalcout guard with `fieldIndex <= INPL` (their string/array
    /// inputs are init-load only) and transform with
    /// `fieldIndex < transformRecordOUTA` (its OUT half is not an input).
    fn special_reseed_input_links(&self) -> &[(&'static str, &'static str)] {
        &[]
    }

    /// The event mask of the `db_post_events` call in that record's `special()`
    /// re-seed arm — the C call sites do not agree:
    ///
    ///  * calcout (`calcoutRecord.c:377`), sCalcout (`sCalcoutRecord.c:516`),
    ///    aCalcout (`aCalcoutRecord.c:536`)
    ///    post a literal `DBE_VALUE`.
    ///  * transform (`transformRecord.c:719`) posts `DBE_VALUE | DBE_LOG`.
    ///
    /// Default: `DBE_VALUE`, the majority shape. Only consulted for the fields
    /// named by [`Self::special_reseed_input_links`].
    fn special_reseed_post_mask(&self) -> crate::server::recgbl::EventMask {
        crate::server::recgbl::EventMask::VALUE
    }

    /// Every CONSTANT input link this record seeds ONCE, at `init_record` —
    /// the record's own `recGblInitConstantLink` / `dbLoadLinkArray` table,
    /// transcribed from its C.
    ///
    /// This is the OTHER half of the one rule the link layer enforces: a
    /// constant link delivers NOTHING at process time (`dbConstGetValue`,
    /// `dbConstLink.c:219-225`), so the ONLY way a `field(INPA,"5")` ever
    /// reaches `A` is this table, applied by the single init-seed owner
    /// `crate::server::database::PvDatabase::rec_gbl_init_constant_links`.
    /// A record that fetches an input link but declares no seed for it (swait,
    /// whose C uses `recDynLink` and seeds nothing) simply never sees the
    /// constant — which is what its C does.
    ///
    /// Default: none.
    fn constant_init_links(&self) -> Vec<ConstantInitLink> {
        Vec::new()
    }

    /// The link field this record loads into its long-string VAL at init through
    /// C's `dbLoadLinkLS` — `"DOL"` for `lso` (`lsoRecord.c:82`), `"INP"` for
    /// `lsi` (its soft device support, `devLsiSoft.c:24`). `loadLS` is a lset
    /// entry of its own, so this is a SEPARATE table from
    /// [`Self::constant_init_links`], not a variant of it; the same init-seed
    /// owner runs both.
    ///
    /// Default: none — a record with no long-string VAL has no `loadLS` seed.
    fn constant_ls_link(&self) -> Option<&'static str> {
        None
    }

    /// Apply the [`Self::constant_ls_link`] load, and return the resulting LEN.
    /// The record clamps the text at its own `SIZV` and runs C's init tail
    /// (`if (prec->len) { strcpy(prec->oval, prec->val); prec->olen = prec->len; }`,
    /// lsoRecord.c:92-95 / lsiRecord.c:85-88); the owner turns a non-zero LEN
    /// into `udf = FALSE`.
    fn apply_ls_load(&mut self, _load: crate::server::record::LsLoad) -> u32 {
        0
    }

    /// Whether this record's CONSTANT input links deliver their value on
    /// EVERY process cycle instead of only at init.
    ///
    /// `false` for every record that fetches with a plain `dbGetLink`. `printf`
    /// is the one exception in the whole database: its `GET_PRINT` macro
    /// (`printfRecord.c:49-52`) tests `dbLinkIsConstant` and re-runs
    /// `recGblInitConstantLink` on every `doPrintf`, so a constant INP0..9
    /// really is re-read each cycle.
    fn constant_inputs_deliver_at_process(&self) -> bool {
        false
    }

    /// The subset of [`Self::multi_input_links`] the framework should
    /// actually fetch this cycle, given an optional externally-resolved
    /// selector index (sel's NVL→SELN value, or `None` when no NVL link
    /// drove it). Default `None` = fetch every input link.
    ///
    /// C `selRecord.c::fetch_values` (lines 421-432) fetches ONLY `INP[SELN]`
    /// in `Specified` mode and all inputs otherwise; sel returns
    /// `Some(vec![INP[SELN]])` so the non-selected inputs are never read and
    /// raise no monitors or link-alarm SEVR.
    fn select_input_links(
        &self,
        _selector: Option<u16>,
    ) -> Option<&'static [(&'static str, &'static str)]> {
        None
    }

    /// Whether [`Self::select_input_links`] can EVER narrow this type's input
    /// list — `sel` in `Specified` mode, `swait` while simulating. The answer
    /// is a property of the type, not of its state, and the cycle carries it
    /// in its [`ProcessPlan`](crate::server::record::RecordInstance), so the
    /// other forty-odd types no longer take the record lock to be told `None`.
    fn narrows_input_links(&self) -> bool {
        false
    }

    /// A `SIMM != NO` cycle substitutes only this record's INPUT STAGE — the
    /// rest of its `process()` still runs.
    ///
    /// C's SIML/SIMM/SIOL group has three shapes, and this hook names the third:
    ///
    /// * `readValue` (ai, bi, longin, …): the simulated read replaces the device
    ///   read, which is the whole of the record's input; the framework performs
    ///   the SIOL read and completes the cycle itself.
    /// * `writeValue` (ao, bo, …): the simulated write replaces the device write
    ///   at the END of the body, so the body runs and only the output is
    ///   redirected to SIOL.
    /// * swait (`swaitRecord.c:402-422`): the simulated read replaces
    ///   `fetch_values()` **and** `calcPerform()` and nothing else — VAL comes
    ///   from SIOL through SVAL, and the OOPT switch, `execOutput`, the monitors
    ///   and the forward link all still run from the record's own `process()`.
    ///
    /// A record that returns `true` gets, on a simulated cycle: SIMM resolved
    /// from SIML, SIOL read into SVAL, `VAL = SVAL` and `UDF = FALSE` when that
    /// read succeeded (C `:417-420` — a failed read changes neither), SIMM_ALARM
    /// raised at SIMS *before* the body so it maximizes against whatever the
    /// body raises (C `:421`), no input-link fetch, and
    /// [`Self::set_simulation_active`] pushed before `process()`.
    fn simulation_substitutes_input_stage(&self) -> bool {
        false
    }

    /// Land the scalar a simulated cycle read from SIOL (through SVAL, where
    /// the record has one) — C `readValue`'s assignment plus whatever the
    /// record's `process()` body then does with it, under the same
    /// `status == 0` gate the framework applies before calling this.
    ///
    /// The base records assign the value straight to VAL — `longinRecord.c:417`
    /// `prec->val = prec->sval;` — which is the default (`set_val`).
    ///
    /// `histogram` does NOT: `histogramRecord.c:385-386` lands it in SGNL
    /// (`prec->sgnl = prec->sval;`), and `process()` (`:218-219`,
    /// `if (status == 0) add_count(prec);`) bins that signal into the VAL
    /// bin-count array. Its VAL is the array, so a `set_val` of the scalar
    /// no-ops and the simulated record is frozen. It overrides.
    fn land_simulated_value(&mut self, value: EpicsValue) -> CaResult<()> {
        self.set_val(value)
    }

    /// Whether the record has an SVAL, the SIOL read buffer — the field's
    /// existence, asked without copying its value.
    fn has_sval(&self) -> bool {
        self.get_field("SVAL").is_some()
    }

    /// C `readValue`'s `prec->val = prec->sval` (`longinRecord.c:417`): land
    /// the SVAL buffer where a simulated read lands, in place. The default
    /// reads the field and goes through [`Self::land_simulated_value`]; a
    /// record whose value is not a scalar copy overrides it.
    fn land_sval(&mut self) -> CaResult<()> {
        match self.get_field("SVAL") {
            Some(sval) => self.land_simulated_value(sval),
            None => Ok(()),
        }
    }

    /// The RVAL a `SIMM=RAW` cycle lands from SVAL — C `readValue`'s
    /// `prec->rval = prec->sval` (`mbbiRecord.c:394`). The default copies SVAL;
    /// a record whose C arm casts it overrides.
    fn sval_as_rval(&self) -> Option<EpicsValue> {
        self.get_field("SVAL")
    }

    /// Whether the record's C `switch (prec->simm)` carries a `default:` arm
    /// that REFUSES a SIMM value outside its own menu:
    ///
    /// ```c
    /// default:
    ///     recGblSetSevr(prec, SOFT_ALARM, INVALID_ALARM);
    ///     status = -1;
    /// ```
    ///
    /// Every record in the framework has it — all 21 base records
    /// (`longinRecord.c:436-438` and its twins; `aaiRecord.c:381-384` writes the
    /// same arm as an `else if (prec->simm != menuYesNoNO)`) and `busy`
    /// (`busyRecord.c:409-413`, the `else` of its YES test).
    ///
    /// `swait` is the sole exception: `swaitRecord.c:407-422` is a plain
    /// `if (pwait->simm == menuYesNoNO) { … } else { /* SIMULATION MODE */ … }`,
    /// so every non-NO value — legal or not — simulates. It overrides to
    /// `false`.
    ///
    /// Consumed by [`resolve_sim_mode`](crate::server::recgbl::simm::resolve_sim_mode),
    /// the single owner of the SIMM dispatch.
    fn rejects_illegal_sim_mode(&self) -> bool {
        true
    }

    /// Whether this record's `readValue` raises `SIMM_ALARM` AFTER its SIOL
    /// read instead of before it.
    ///
    /// `recGblSetSevr` is strict-greater, so the ORDER of the SIMM raise
    /// against the LINK_ALARM/INVALID that a failed `dbGetLink` raises inside
    /// itself (`dbLink.c:319` `setLinkAlarm`, reached from `:336`) decides an
    /// equal-severity tie. With `SIMS = INVALID` and a broken SIOL the record
    /// publishes whichever of the two was raised FIRST.
    ///
    /// The base records raise it first, at the top of the `case menuYesNoYES:`
    /// arm — `longinRecord.c:414` then `:416` — so they publish
    /// `STAT = SIMM_ALARM`. That is the default, and it is what the generic
    /// raise in `check_simulation_mode` performs.
    ///
    /// `mca` is the other order: `mcaRecord.c:1118` reads SIOL and only then
    /// `:1129` runs `recGblSetSevr(pmca, SIMM_ALARM, pmca->sims)`, past the
    /// `if/else` on SIMM, so the LINK_ALARM raised inside the read WINS the tie
    /// and C publishes `STAT = LINK_ALARM` with `AMSG = "field SIOL"`.
    /// (Read against `mca` at `687d563`; that tree records no pin.)
    ///
    /// `swait` is the same order but does not answer here — it has its own
    /// `input_stage` path in `check_simulation_mode`, which already reads
    /// before it raises (`swaitRecord.c:416` then `:421`).
    ///
    /// Declared per record rather than special-cased at the raise site so a
    /// record type carries its own C ordering; overriding to `true` is the
    /// whole opt-in.
    fn raises_simm_after_read(&self) -> bool {
        false
    }

    /// This cycle's simulation state, pushed by the framework before
    /// `process()` — the twin of [`Self::set_fetch_gate_failed`], and only for a
    /// record that declares [`Self::simulation_substitutes_input_stage`].
    ///
    /// It is pushed on EVERY cycle of such a record (`false` included), so the
    /// flag cannot survive the cycle it belongs to. The record uses it to skip
    /// exactly what C's simulation branch skips — for swait, `fetch_values()`
    /// (through [`Self::select_input_links`]) and `calcPerform()`.
    fn set_simulation_active(&mut self, _active: bool) {}

    /// How C's `fetch_values()` for this record type reacts to a link read
    /// that fails. Drives the framework's [`Self::multi_input_links`] fetch
    /// loop; see [`InputFetchPolicy`]. Default: [`InputFetchPolicy::ReadAll`].
    ///
    /// It governs the [`Self::multi_input_links`] loop ONLY.
    /// [`Self::string_input_links`] is C's *second*, separately-gated fetch
    /// loop and never participates in this policy.
    fn input_fetch_policy(&self) -> InputFetchPolicy {
        InputFetchPolicy::ReadAll
    }

    /// String-valued input links: `(link_field, value_field)` pairs read as
    /// DBR_STRING, C `sCalcoutRecord.c::fetch_values` (890-941) — the SECOND
    /// loop of that function, over `INAA`..`INLL` → `AA`..`LL`:
    ///
    /// ```c
    /// for (i=0, plink=&pcalc->inaa, psvalue=pcalc->strs; i<STRING_MAX_FIELDS; ...) {
    ///     ...
    ///     if (((field_type==DBR_CHAR) || (field_type==DBR_UCHAR)) && nelm>1) {
    ///         status = dbGetLink(plink, field_type, tmpstr, 0, &nelm);
    ///         epicsStrSnPrintEscaped(*psvalue, STRING_SIZE-1, tmpstr, strlen(tmpstr));
    ///     } else {
    ///         status = dbGetLink(plink, DBR_STRING, *psvalue, 0, 0);
    ///     }
    ///     if (!RTN_SUCCESS(status))
    ///         epicsSnprintf(*psvalue, STRING_SIZE-1, "%s:fetch(%s) failed", pcalc->name, sFldnames[i]);
    /// }
    /// return(0);
    /// ```
    ///
    /// Three properties this loop does NOT share with [`Self::multi_input_links`],
    /// which is why it is a separate list rather than more entries in that one:
    ///
    /// 1. **Ungated.** It ends in `return(0)` — a failing string link never
    ///    makes `fetch_values` non-zero, so it cannot suppress the record body.
    ///    The record's single [`Self::input_fetch_policy`] describes the numeric
    ///    loop (`AbortOnFirstFailure` for scalcout) and cannot also describe this
    ///    one.
    /// 2. **A failed read still writes the field** — with the diagnostic text
    ///    `"<record>:fetch(<FIELD>) failed"`, not with the previous value.
    /// 3. **A `DBF_CHAR`/`DBF_UCHAR` array source is read as text**, C-escaped
    ///    (`epicsStrSnPrintEscaped`), which is how a >40-char string reaches a
    ///    string calc; every other source type converts as DBR_STRING.
    ///
    /// The value is delivered through [`Self::put_field_internal`], so the
    /// target field's declared `DbFieldType` performs the final coercion.
    fn string_input_links(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }

    /// Input links this record reads at OUTPUT time instead of during the
    /// input-fetch phase: `(link_name_field, value_field)` pairs. The framework
    /// reads each configured link immediately before the OUT write, and ONLY on
    /// a cycle where the output actually fires ([`Self::should_output`] and no
    /// IVOA veto), then writes the value into `value_field` via
    /// [`Self::put_field`]; a failed read leaves the field alone.
    ///
    /// C `swaitRecord.c::execOutput` (763-772) does exactly this for `DOL`:
    ///
    /// ```c
    /// if (pwait->dopt) {                    /* DOPT = "Use DOL" */
    ///     if (!pwait->dolv) {               /* DOL PV connected */
    ///         oldDold = pwait->dold;
    ///         recDynLinkGet(&pcbst->caLinkStruct[DOL_INDEX], &(pwait->dold), ...);
    ///         if (pwait->dold != oldDold)
    ///             db_post_events(pwait, &pcbst->pwait->dold, DBE_VALUE);
    ///     }
    ///     outValue = pwait->dold;
    /// }
    /// ```
    ///
    /// The timing is the point: the value written out is the one the link holds
    /// at output time (ODLY delay-end included), and a cycle whose output does
    /// not fire never refreshes — or posts — the field. Fetching such a link in
    /// the normal input phase would do both. Default: none.
    fn output_time_input_links(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }

    /// Whether [`Self::output_time_input_links`] can ever be non-empty for
    /// this type. Read once into the process plan; the cycle never asks
    /// `output_time_input_links` of a type that answers `false`.
    fn declares_output_time_input_links(&self) -> bool {
        false
    }

    /// The value the framework writes to the OUT link. The single owner of
    /// "what goes out", shared by the soft-OUT write, the async-completion
    /// write and the simulated SIOL redirect.
    ///
    /// The default is the C staging convention: the record computed the output
    /// into `OVAL` during `process()` (`calcout`/`ao`/`bo`/...), falling back to
    /// `VAL` for records that have no `OVAL`. Override when the record's C
    /// composes the output value at *output* time rather than staging it — e.g.
    /// swait, whose `execOutput` (`swaitRecord.c:763-772`) picks between `VAL`
    /// and the just-fetched `DOLD` and whose `OVAL` field is C's "Old Value"
    /// (the previous VAL, used only by the OOPT test), not an output stage.
    fn output_link_value(&self) -> Option<EpicsValue> {
        match self.cycle_cell(CycleCell::Oval) {
            CycleRead::Value(v) => Some(v.into()),
            CycleRead::Absent => self.val(),
            CycleRead::ByName => self.get_field("OVAL").or_else(|| self.val()),
        }
    }

    /// [`Self::output_link_value`], borrowed: C's `dbPutLink(&prec->out,
    /// dbrType, pbuffer, nRequest)` copies nothing. `None` means no borrowed
    /// view, and the caller takes `output_link_value`; a `Some` view is that
    /// value. The default answers only a numeric OVAL (else VAL) cell.
    fn output_view(&self) -> Option<OutView<'_>> {
        let cell = match self.cycle_cell(CycleCell::Oval) {
            CycleRead::Absent => self.cycle_cell(CycleCell::Val),
            oval => oval,
        };
        match cell {
            CycleRead::Value(v) => Some(OutView::Num(v)),
            CycleRead::Absent | CycleRead::ByName => None,
        }
    }

    /// Return multi-output link field pairs: (link_field, value_field).
    /// Override in transform to return OUTA..OUTP → A..P mappings.
    fn multi_output_links(&self) -> &[(&'static str, &'static str)] {
        &[]
    }

    /// Does this TYPE declare a [`Self::multi_output_links`] table at all,
    /// whether or not this cycle drives it?
    ///
    /// `multi_output_links` is a per-CYCLE answer: `scalcout` withholds its
    /// pair while its own put-callback is in flight, `aSub` while the
    /// subroutine returned non-zero, `acalcout` and `epid` while the cycle
    /// computed nothing. Every one of them therefore answers `&[]` at
    /// construction and on most cycles, so the engine cannot settle "does
    /// this type have multi-outputs" by asking it. This is that settled
    /// answer, carried by `ProcessPlan`, so a record type with no such table
    /// never takes its own lock once a cycle to be told so.
    ///
    /// MUST be `true` for every type whose `multi_output_links` can ever
    /// return a non-empty slice. `dispatch_multi_output_values` debug-asserts
    /// the implication on every cycle it declines, so a type that adds the
    /// table and forgets this fails the test suite rather than losing its
    /// outputs in release.
    fn declares_multi_output_links(&self) -> bool {
        false
    }

    /// The record's C soft device support write-buffer switch for a
    /// multi-output pair: given the pair's staged value (the value field
    /// named by [`Self::multi_output_links`]) and the RESOLVED TARGET
    /// metadata, return the buffer C would actually put.
    ///
    /// C's soft device supports do not blindly write one field: they read
    /// the target's DBF type and element count and pick a buffer from them —
    /// `devaCalcoutSoft.c::write_acalcout` (75-87) picks
    /// `nelm == 1 ? &scalar : array`, `devsCalcoutSoft.c::write_scalcout`
    /// (66-144) routes a string-class target to the computed string, a
    /// `CHAR`/`UCHAR` array to the string's bytes, and everything else to
    /// the numeric. The framework resolves the target
    /// ([`PvDatabase::resolve_out_target`](crate::server::database::PvDatabase))
    /// and hands it here; the record reproduces its device support's switch.
    ///
    /// Default: write the staged value unchanged (no device-support switch).
    fn multi_output_buffer(
        &self,
        link_field: &str,
        staged: EpicsValue,
        target: &OutTarget,
    ) -> EpicsValue {
        let _ = (link_field, target);
        staged
    }

    /// The buffer to put on an OUT link the record drives itself, chosen from
    /// the RESOLVED TARGET — the no-staged-value sibling of
    /// [`Self::multi_output_buffer`].
    ///
    /// C `sseqRecord.c::processCallback` (708-795) is the case: the value a
    /// step forwards is not one field but a *switch on the destination*
    /// (`dbGetLinkDBFtype(&lnk)` / `dbGetNelements(&lnk)`), taken at fire
    /// time — the string view `s` for a string-class target, the double view
    /// `dov` for a numeric one, `s`'s bytes for a `CHAR`/`UCHAR` array, and
    /// **no put at all** for a target whose type does not resolve (C's
    /// `default: break`). `None` is that no-put: the caller issues no write.
    ///
    /// The record calls this ITSELF, on the target
    /// [`ProcessAction::ResolveOutTarget`] handed it before `process()` — one
    /// decision, made before anything is issued, because the same switch
    /// decides more than the buffer (sseq: whether a `WAITn` put-callback goes
    /// out, and hence whether `WTGn` is raised). See
    /// [`Self::set_resolved_out_target`].
    ///
    /// Default: `None`. Only a record that resolves its own OUT target reaches
    /// this, and it must override.
    fn typed_output_buffer(&self, link_field: &str, target: &OutTarget) -> Option<EpicsValue> {
        let _ = (link_field, target);
        None
    }

    /// Receive the RESOLVED target of an OUT link the record asked to have
    /// resolved before this cycle's `process()`
    /// ([`ProcessAction::ResolveOutTarget`]).
    ///
    /// C resolves an OUT link's DBF class OUTSIDE the put — `checkLinks` caches
    /// it in the record (`sseqRecord.c:203-240`) — so `processCallback` can make
    /// ONE decision from it: which view goes on the wire, AND whether a
    /// put-callback is issued (hence whether `waiting` is raised). Its
    /// `default:` arm (`:790`) does neither. A record that learns the class only
    /// from inside the framework's put path cannot keep those two halves
    /// together: it has to raise `waiting` first and find out afterwards that no
    /// put was made. This hook is that cached class — the record decides, then
    /// acts.
    ///
    /// Default: no-op. Only a record that emits the action reaches this.
    fn set_resolved_out_target(&mut self, link_field: &str, target: OutTarget) {
        let _ = (link_field, target);
    }

    /// The `dbrType` this record's framework-run input read on `link_field`
    /// asks the SOURCE for — the READ twin of [`Self::typed_output_buffer`],
    /// and C's `dbGetLink` second argument. Consulted by every framework
    /// fetch path: the pre-input [`ProcessAction::ReadDbLink`] stage, the
    /// single-INP soft fetch, the closed-loop DOL fetch, the multi-input
    /// loop and the SIOL simulation read.
    ///
    /// `source` is the far end of the input link as C's
    /// `dbGetLinkDBFtype`/`dbGetNelements` report it (the same lset accessors
    /// the OUT side uses — sseq asks them of `dol` at `sseqRecord.c:641` and of
    /// `lnk` at `:709`), resolved by the framework
    /// ([`PvDatabase::resolve_out_target`](crate::server::database::PvDatabase)).
    ///
    /// `None` means C's `default: break` — **no read at all**, the arm a source
    /// class the record's switch does not name falls to (an unresolvable /
    /// constant / disconnected source, and sseq's un-cased `DBF_INT64`). The
    /// link is left untouched and no LINK alarm is raised, exactly as C's
    /// skipped `dbGetLink` call leaves `status` alone.
    ///
    /// Default: [`LinkReadAs::Native`] — the source's native value, coerced at
    /// the target field's own put boundary.
    fn input_link_request(&self, link_field: &str) -> InputLinkRequest {
        let _ = link_field;
        InputLinkRequest::As(LinkReadAs::Native)
    }

    /// Whether [`Self::input_link_request`] and
    /// [`Self::input_link_failure_is_inert`] are settled by the link field
    /// alone, so the process plan may ask them once for every declared link
    /// when the record is built. `false` for the two types whose answer reads
    /// instance state — aSub (`FTA..FTU`) and printf (`FMT`) — which the fetch
    /// loop then asks per link, per cycle, through the vtable.
    fn input_link_answers_fixed_at_type(&self) -> bool {
        true
    }

    /// The same request for a record whose [`Self::input_link_request`]
    /// answered [`InputLinkRequest::FromSource`] — C's `dbGetLinkLS` switch on
    /// `dbGetLinkDBFtype`, and sseq's on the same accessor. Reached ONLY
    /// through that answer, so a record that never gives it needs no
    /// implementation.
    fn input_link_read_as_from_source(
        &self,
        link_field: &str,
        source: &OutTarget,
    ) -> Option<LinkReadAs> {
        let _ = (link_field, source);
        Some(LinkReadAs::Native)
    }

    /// Return the name of the output event (`OEVT`) to post this cycle, or
    /// `None`. The event-subsystem twin of the OUT write: a downstream
    /// `SCAN="Event"` / `EVNT="<name>"` record is woken each time the record
    /// drives output. Mirrors C `calcout`/`sCalcout`/`aCalcout` `execOutput`,
    /// which calls `postEvent(epvt)` / `post_event(oevt)` immediately after
    /// `writeValue` in every OUT-driving branch.
    ///
    /// The override MUST fold in the record's own output-fire decision
    /// (`should_output()` for `calcout`; the cached OOPT/calc-fail/ODLY
    /// decision for `sCalcout`/`aCalcout`) and return `None` when output did
    /// not fire or when `OEVT` is unset. The framework adds the only gate the
    /// record cannot see — the IVOA `Don't_drive` veto on an INVALID cycle —
    /// so the post fires on exactly the cycles the OUT write does. Numeric
    /// `OEVT` (DBF_USHORT) stringifies to match the `EVNT` ingest; a string
    /// `OEVT` (DBF_STRING) is the event name verbatim.
    fn output_event(&self) -> Option<crate::types::PvString> {
        None
    }

    /// Internal field write that bypasses read-only checks.
    /// Used by the framework to write values from ReadDbLink actions
    /// into fields that are normally read-only (e.g., epid.CVAL).
    /// Default implementation delegates to put_field().
    ///
    /// On the `ReadDbLink` path this is also where a pvalink NTEnum
    /// carrier ([`EpicsValue::EnumWithChoices`]) is resolved. The
    /// dbrType-blind link resolver produces it for an NTEnum source;
    /// pvxs `pvaGetValue` (`pvxs/ioc/pvalink_lset.cpp:330-360`) picks
    /// label-vs-index by the TARGET field's dbrType — only a DBR_STRING
    /// target gets the `choices[index]` label, every other type takes
    /// the numeric index. Route it through [`EpicsValue::convert_to`]
    /// (the single value-coercion owner) against the target field's
    /// `db_field_type`, so the transient carrier is consumed before any
    /// record `put_field` / storage / wire path can see it. The
    /// single-INP→VAL apply path reaches the same `convert_to` via
    /// `set_val`'s `TypeMismatch` auto-coerce.
    fn put_field_internal(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        put_field_internal_default(self, name, value)
    }

    /// Return pre-process actions (ReadDbLink) that the framework should
    /// execute BEFORE calling process(). This is called once per cycle.
    /// Default returns empty. Override in records that need link reads
    /// to be available during process().
    fn pre_process_actions(&mut self) -> ProcessActions {
        ProcessActions::new()
    }

    /// The per-cycle hooks this type overrides with a body that can act —
    /// see [`CycleHooks`]. `ALL` by default; the derive and every base type
    /// that keeps the empty defaults say `NONE`, and the drift test holds
    /// them to it.
    fn cycle_hooks(&self) -> CycleHooks {
        CycleHooks::ALL
    }

    /// Whether [`Self::pre_process_actions`] can return anything for this
    /// type. C has no such hook, so the cycle settles this once
    /// (`ProcessPlan::acts_before_process`) rather than call an empty body
    /// through the vtable every cycle. `true` by default: a type that keeps
    /// the empty hook says `false`, and one that forgets pays the call
    /// rather than losing an action.
    fn acts_before_process(&self) -> bool {
        true
    }

    /// Return actions the framework must execute BEFORE the input-link
    /// (`multi_input_links`, INP -> value-field) fetch for this cycle.
    ///
    /// This is strictly earlier than [`Self::pre_process_actions`]: the
    /// framework resolves input links *before* it calls
    /// `pre_process_actions`, so an action that must affect what an
    /// input link reads cannot be expressed there.
    ///
    /// The motivating case is the epid record's `devEpidSoftCallback`
    /// DB-type TRIG link: C `devEpidSoftCallback.c:120-132` writes the
    /// readback-trigger link with `dbPutLink` — which synchronously
    /// processes the triggered source chain — and only *then*
    /// (`devEpidSoftCallback.c:151`) does `dbGetLink(&pepid->inp, ...)`
    /// read `CVAL`. The trigger write therefore has to land before the
    /// `INP -> CVAL` fetch, in the same process pass.
    ///
    /// Called once per cycle, while a record write lock is held; the
    /// framework executes the returned actions (currently `WriteDbLink`
    /// and `ReadDbLink`) and then performs the input-link fetch.
    /// Default returns empty.
    fn pre_input_link_actions(
        &mut self,
        _dol_is_constant: bool,
        _dpvt: &mut crate::server::device_support::Dpvt,
    ) -> ProcessActions {
        ProcessActions::new()
    }

    /// Asked between this cycle's [`Self::pre_input_link_actions`] reads
    /// (`resolved`: the links that read successfully) and everything after
    /// them, by a type that declares [`CycleHooks::PRE_INPUT_GATE`]. `Some`
    /// carries writes to make before the input fetch; `None` drops the
    /// cycle's remaining pre-input writes. C `epidRecord.c:189-201` reads STPL
    /// and returns ahead of `do_pid`'s TRIG put and INP read while UDF holds.
    fn gate_after_pre_input_reads(
        &mut self,
        _resolved: &[&'static str],
        _dpvt: &mut crate::server::device_support::Dpvt,
    ) -> Option<ProcessActions> {
        Some(ProcessActions::new())
    }

    /// Called by the framework immediately before `process()` to push a
    /// read-only snapshot of framework-owned [`crate::server::record::CommonFields`] state
    /// ([`ProcessContext`]) that the record's `process()` needs to see.
    ///
    /// The framework owns `RecordInstance.common`; a record `process()`
    /// only gets `&mut self`. C records read `dbCommon` directly — e.g.
    /// `epidRecord.c:195` checks `pepid->udf` at the top of `process()`,
    /// `timestampRecord.c:90` branches on `ptimestamp->tse`. This hook
    /// is the controlled equivalent: a record that needs `udf`/`phas`/
    /// `tse`/`tsel` during `process()` overrides this to stash the
    /// values into its own fields.
    ///
    /// Additive, framework-set-hook pattern (same shape as
    /// [`Record::set_device_did_compute`]). Default: ignore — most
    /// records never need common state during `process()`.
    fn set_process_context(&mut self, _ctx: &ProcessContext) {}

    /// C `checkLinks`' view of this record — the links it re-reads, their
    /// status fields, and `caLinkStat`/`cbScheduled` — for a type whose
    /// link-status fields follow their links' connection.
    fn link_check(&mut self) -> Option<crate::server::records::link_status::LinkCheck<'_>> {
        None
    }

    /// Called by the framework immediately before `process()` to say whether
    /// this cycle is the record's OWN scheduled re-entry — the
    /// [`ProcessAction::ReprocessAfter`] timer firing, or a put-notify
    /// completion — rather than a fresh put / scan / forward-link process.
    ///
    /// C hands records this distinction for free: `callbackRequestDelayed`
    /// dispatches to a callback function of the record's own, never to
    /// `process()`. A port that models the timer with `ReprocessAfter` has
    /// both events arriving at the same entry point, so a record whose C
    /// original keeps the two paths separate — throttle's `delayFuncCallback`
    /// -> `valuePut` versus `process()` -> `enterValue`
    /// (`throttleRecord.c:518-613`) — cannot route the cycle without knowing
    /// which one it is. The framework already knows; this hook is what it
    /// tells the record, so the record never has to infer it from a clock
    /// reading (which a mid-flight interval change silently falsifies).
    ///
    /// `pact` is `prec->pact` at entry, which is what a record whose C
    /// `process()` branches on PACT (calcout's ODLY arm, `calcoutRecord.c:231`)
    /// must read instead: a device-link put clears it (`dbAccess.c:1194`) and
    /// the scheduled re-entry then runs a fresh cycle. Additive,
    /// framework-set-hook pattern (same shape as
    /// [`Record::set_process_context`]). Default: ignore.
    fn set_process_continuation(&mut self, _continuation: bool, _pact: Pact<'_>) {}

    /// Called by the framework once for every [`ProcessAction::WriteDbLink`]
    /// this record emitted, reporting the value it carried and whether the
    /// put failed — the port's stand-in for the `dbPutLink` return C reads
    /// inline.
    ///
    /// A record whose C original derives a field from that return needs it:
    /// throttle's `STS` is `throttleSTS_SUC` only on success and its `SENT`
    /// advances only then (`throttleRecord.c:564-575`). Without this the
    /// record can only commit its own intent, which reports Success for a put
    /// that never landed. The framework raises the LINK/INVALID alarm either
    /// way; this is the record-owned half.
    ///
    /// Called after the put, while the cycle's monitor snapshot is still
    /// ahead, so a field set here is posted by this cycle. Every emitted
    /// action reports exactly once, an unresolvable link included (reported
    /// as failed). Additive, framework-set-hook pattern (same shape as
    /// [`Record::set_process_context`]). Default: ignore — most records
    /// derive nothing from the put's result, as their C originals do not.
    ///
    /// The actions returned run next, as [`Self::link_read_result`]'s do: C
    /// throttle's `valuePut` calls `valueSync` straight after a landed
    /// `dbPutLink` (`throttleRecord.c:571-572`).
    fn set_out_link_write_status(
        &mut self,
        _link_field: &'static str,
        _value: &EpicsValue,
        _failed: bool,
    ) -> ProcessActions {
        ProcessActions::new()
    }

    /// Called by the framework once for every [`ProcessAction::GetLink`] this
    /// record emitted, and for every [`ProcessAction::ReadDbLink`] it emitted
    /// outside [`Self::pre_input_link_actions`] and
    /// [`Self::pre_process_actions`] (those report through
    /// [`Self::set_resolved_input_links`]): C's `dbGetLink` status, with the
    /// value when the read named no field to store it in.
    ///
    /// The actions returned run next, before the rest of the cycle's.
    /// Default: ignore.
    fn link_read_result(&mut self, _link_field: &'static str, _read: LinkRead) -> ProcessActions {
        ProcessActions::new()
    }

    /// Called once by the framework when the record is registered
    /// (`add_record`), delivering the record its own canonical name plus a
    /// cycle-free [`crate::server::database::AsyncDbHandle`] for driving
    /// async-side updates from OUTSIDE a `process()` cycle.
    ///
    /// The handle wraps a `Weak` reference to the database, so a record
    /// that stashes it creates no ownership cycle (the database owns the
    /// record; a stored strong handle would leak it). It is the controlled
    /// equivalent of C device support capturing `precord` plus the
    /// dbCommon scan lock for an out-of-band `db_post_events` /
    /// `callbackRequest`: e.g. the asyn TRACE/exception callback posts
    /// trace-flag fields immediately from the driver thread, and AQR
    /// cancels a queued I/O re-entry — neither happens inside `process()`.
    ///
    /// The in-band counterpart for a record's *own* process cycle is the
    /// completion-driven [`ProcessAction`] family
    /// ([`ProcessAction::WriteDbLinkNotify`],
    /// [`ProcessAction::CancelReprocess`],
    /// [`ProcessAction::ReprocessAfter`]); this hook exists for the
    /// out-of-band path that has no `process()` return to ride on.
    ///
    /// Additive, framework-set-hook pattern (same shape as
    /// [`Self::set_process_context`]). Default: ignore — most records do
    /// no out-of-band async posting.
    fn set_async_context(
        &mut self,
        _name: crate::types::RecordName,
        _db: crate::server::database::AsyncDbHandle,
    ) {
    }

    /// Framework init hook: called once at record load *after* the common
    /// link fields (`INP`/`OUT`/`FLNK`/...) have been resolved and the
    /// `init_record` passes have run, with the record's resolved
    /// [`CommonFields`].
    ///
    /// This is the seam for records that classify their links into status
    /// diagnostics at init the way C `init_record` does (e.g. calcout's
    /// `INAV..INUV`/`OUTV` `menu(calcoutINAV)` checkLinks loop): a record's
    /// *common* link strings (`OUT` is a common field, not a record field)
    /// are invisible to [`Self::set_async_context`] — which runs at
    /// `add_record`, *before* the common fields are applied — and to
    /// `init_record`, which carries no `CommonFields`. The record captures
    /// whichever common links it needs here so a passive, never-processed
    /// record already exposes its link status. Records whose links are all
    /// record-owned (e.g. sseq DOLn/LNKn) do not need this hook.
    ///
    /// Additive, framework-set-hook pattern. Default: ignore.
    fn init_links(&mut self, _common: &crate::server::record::CommonFields) {}

    /// The links C's `init_record(prec, 1)` reads with `dbGetLink`, each with
    /// the DBR type it requests. The framework reads them once at `iocInit`,
    /// after every record's pass 0, and hands the answers to
    /// [`Self::init_record_with_links`].
    fn init_link_reads(&self) -> &'static [(&'static str, LinkReadAs)] {
        &[]
    }

    /// Pass 1 of `init_record`. A type that declares
    /// [`Self::init_link_reads`] overrides this and runs its C loop over
    /// `reads`: store, gate, zero or stop on each answer as its C
    /// `init_record` does.
    fn init_record_with_links(&mut self, _reads: &mut InitLinkReads) -> CaResult<()> {
        self.init_record(1)
    }

    /// Called by the framework before process() to indicate whether device
    /// support's read() already performed the record's compute step.
    /// Override in records that have a built-in compute (e.g., epid PID)
    /// to skip it when device support already ran it.
    /// Default: ignore.
    fn set_device_did_compute(&mut self, _did_compute: bool) {}

    /// Whether this record has a raw-to-engineering (`RVAL → VAL`)
    /// `convert()` step that must be skipped on a `Soft Channel` input.
    ///
    /// C `devAiSoft.c:65` `read_ai` (and the other soft-channel input
    /// `read_xxx`) always returns 2 ("don't convert"), so `aiRecord.c`'s
    /// `if (status==0) convert(prec)` is bypassed for a `Soft Channel`
    /// input record. The framework expresses this by calling
    /// [`Record::set_device_did_compute`]`(true)` on the record before
    /// `process()`.
    ///
    /// This hook exists so the framework only suppresses `convert()` —
    /// NOT a record's entire built-in compute. Records like `epid` also
    /// override `set_device_did_compute` but interpret it as "skip the
    /// whole compute step" (the PID loop); those records have no
    /// `RVAL → VAL` convert and MUST keep the default `false` so a
    /// `Soft Channel` `epid` still runs `do_pid()` in `process()`.
    ///
    /// Default `false`: a record is only opted into the soft-channel
    /// convert-skip when it explicitly returns `true`.
    fn soft_channel_skips_convert(&self) -> bool {
        false
    }

    /// Whether this output record's forward `VAL → RVAL` `convert()` must be
    /// SKIPPED on a process cycle where VAL is still undefined (`UDF != 0`) and
    /// no value source ran.
    ///
    /// C's output records take an early `goto CONTINUE` before `convert()` when
    /// the record is undefined and no value was sourced this cycle:
    ///
    /// ```c
    /// /* mbboRecord.c:199-217 (and ao/bo/mbboDirect alike) */
    /// if (!pact) {
    ///     if (!dbLinkIsConstant(&prec->dol) && omsl == closed_loop) {
    ///         ... prec->val = <DOL>;          /* value sourced -> udf cleared */
    ///     }
    ///     else if (prec->udf) {
    ///         recGblSetSevr(prec, UDF_ALARM, prec->udfs);
    ///         goto CONTINUE;                  /* skip udf=FALSE AND convert() */
    ///     }
    ///     prec->udf = FALSE;
    ///     convert(prec);                      /* VAL -> RVAL */
    /// }
    /// ```
    ///
    /// So a `caput REC.RVAL 1` on a bare `record(mbbo,"M"){}` (UDF still 1, no
    /// VAL put, no closed-loop DOL) leaves RVAL at the client value: `convert`
    /// never runs to recompute `RVAL = VAL(=0)`. Verified on the compiled
    /// softIoc — bare RVAL put reads back the put value; after a VAL put clears
    /// UDF, the next RVAL put IS overwritten by `convert`.
    ///
    /// The framework consults this before `process()`: an opted-in output record
    /// with `UDF != 0` and no value source this cycle is told
    /// [`Self::set_device_did_compute`]`(true)` so its `process()` skips the
    /// forward convert. A VAL put (UDF cleared in `field_io`) or a closed-loop
    /// DOL fetch (UDF cleared at the DOL-apply site) leaves `UDF == 0`, so the
    /// convert runs exactly as C's fall-through does.
    ///
    /// Default `false`. The rest of C's output family (ao/bo/mbboDirect) shares
    /// the same `goto CONTINUE`; they are a separate change and stay opted out
    /// here.
    fn skips_forward_convert_when_undefined(&self) -> bool {
        false
    }

    /// C `mbboRecord.c:210-221` / `mbboDirectRecord.c:190-202` — the same
    /// `else if (prec->udf) goto CONTINUE` that skips the forward `convert()`
    /// ALSO jumps past the pre-output `recGblGetTimeStampSimm` call. So a soft
    /// (synchronous) mbbo/mbboDirect that is still UNDEFINED never stamps TIME
    /// on the first-pass output stage: the only other stamp after `CONTINUE:`
    /// is guarded by `if (pact)` (mbboRecord.c:256-258), which fires on
    /// ASYNCHRONOUS completion re-entry only. A sync UDF record therefore keeps
    /// TIME at the EPICS epoch ("never processed") until a VAL put clears UDF.
    ///
    /// Contrast ao/bo/longout/int64out/stringout: their `if (!pact)` block
    /// calls `recGblGetTimeStampSimm` UNCONDITIONALLY (aoRecord.c:192,
    /// boRecord.c:215), so they stamp even while undefined and do NOT opt in.
    ///
    /// The framework consults this at the synchronous output-stage stamp
    /// (`processing.rs` `process_record_with_links_inner`, the pre-output
    /// `apply_timestamp`): an opted-in record with `UDF != 0` skips that stamp,
    /// mirroring C's `goto CONTINUE`. The async-completion stamp
    /// (`complete_async_record_inner`) stays unconditional, matching C's
    /// `if (pact)` re-stamp on async devices.
    ///
    /// This is a SEPARATE hook from [`Self::skips_forward_convert_when_undefined`]:
    /// mbboDirect's VAL is bit-derived and does NOT opt into the convert-skip,
    /// yet it DOES share this timestamp-skip. Do not conflate the two.
    ///
    /// Default `false`. Only mbbo/mbboDirect carry the `goto CONTINUE`
    /// timestamp-skip in C; every other record stays opted out.
    fn skips_timestamp_when_undefined(&self) -> bool {
        false
    }

    /// The value [`Self::val`] answers, lent instead of cloned — what C
    /// `monitor()` reads in place. `None`: the type does not lend it, and the
    /// framework asks [`Self::val`].
    fn monitor_value_view(&self) -> Option<ValueView<'_>> {
        None
    }

    /// C `dbPut` of one element into the primary array field: the convert
    /// routine writes `pfield[0]` of the existing buffer and `put_array_info`
    /// sets NORD to 1 (`dbAccess.c:1350-1367`). `src` is an [`OutView::Str`]
    /// or [`OutView::Num`]. `false`: nothing was stored — the type has no such
    /// buffer, another holder shares it, or the request does not convert
    /// cleanly — and the general put takes it. The caller skips both
    /// `dbPutSpecial` passes, so only a type whose `special()` and `on_put`
    /// do nothing for its primary field may store here.
    fn put_primary_element0(&mut self, src: OutView<'_>) -> bool {
        let _ = src;
        false
    }
}

/// The body of [`Record::put_field_internal`] — the framework's internal write
/// path — as a free function, so a record that needs to observe an internal
/// write can WRAP it instead of re-implementing it.
///
/// A record overriding `put_field_internal` and ending in `self.put_field(..)`
/// silently drops the coercion below for every field it does not special-case.
/// Calling this instead keeps the one owner of that coercion.
///
/// Input-link / internal delivery coerces the source to the target field's
/// stored type before `put_field`, mirroring C `dbGetLink(DBF_<target>)`: the
/// link layer converts any numeric source to the requested type, so a record's
/// typed `put_field` arm never sees a mismatched type. This covers every
/// `ReadDbLink` target by construction (e.g. a `compress` INP from a `DBF_LONG`
/// record delivers a `Long`/`LongArray` that must become `Double`/`DoubleArray`
/// for the Double-only VAL arm, which otherwise drops it and never advances the
/// buffer). An `EnumWithChoices` carrier is always collapsed to a bare index by
/// `convert_to`, even when the target is already `Enum`.
/// Render a put request in the DESTINATION field's shape — C `dbPut`'s value
/// branch (`dbAccess.c:1350-1367`, tag `R7.0.10`) expressed as a value
/// transform, and the single owner of both of its directions.
///
/// C never asks a record to accept a shape. `dbPut` copies `nRequest` elements
/// through `dbPutConvertRoutine` into the field, so a ONE-element request over
/// a `special(SPC_DBADDR)` destination lands element 0 of a BUFFER (which is
/// how pvxs's `putScalar` — `doDbPut(chan, dbr, &value, 1)`,
/// `iocsource.cpp:599-601` — writes a one-element channel), and a
/// MULTI-element request over a scalar destination is clamped to `no_elements`
/// at `:1360` and lands element 0 of a SCALAR. This port hands the record one
/// `EpicsValue` whose variant its `put_field` arm matches on, so C's two arms
/// have to be a rendering, and one every client put shares: the two PVA
/// servers each carried the scalar-to-buffer half themselves, and a third
/// caller would have needed its own copy.
///
/// This is the `dbPut` contract only — [`dbput_coerce_value`] is the entry
/// that applies it. Internal delivery has [`link_value_in_field_shape`], and
/// the two are separate functions rather than one with a flag because they
/// disagree about the same request: one sample into a `compress` VAL is a
/// one-element buffer to `dbPut` and a `compress_scalar` sample to a link.
///
/// An `EnumWithChoices` carrier is left alone in both — it is a transient link
/// payload, not a stored value, and `convert_to` collapses it to a bare index.
/// `StoredShape` carries the rest of the shared rule.
pub fn put_value_in_field_shape<R: Record + ?Sized>(
    record: &R,
    field: &str,
    value: EpicsValue,
) -> EpicsValue {
    put_value_in_shape(record, field, StoredShape::of(record, field), value)
}

/// [`put_value_in_field_shape`] against a destination already read.
fn put_value_in_shape<R: Record + ?Sized>(
    record: &R,
    field: &str,
    shape: StoredShape,
    value: EpicsValue,
) -> EpicsValue {
    match (shape.is_array, value.is_array()) {
        // C's SCALAR arm: the clamp at `:1360` leaves one element.
        (false, true) => value.first_element().unwrap_or(value),
        // C's ARRAY arm at `nRequest == 1` — except into a long-string field.
        // C's `cvt_dbaddr` re-types those five to `DBF_STRING`
        // (`lsiRecord.c:127-134`), so their conversion row is
        // `putStringString`, a byte copy of the text, and the record's put
        // takes the `String` itself as that `DBR_STRING` put (all five store
        // their `LEN` elements as a `CharArray`). Wrapping a `String` request
        // into a one-element `StringArray` hands that row an array and the
        // record a variant no `put_field` arm takes.
        (true, false) if !record.field_is_long_string(field) => {
            one_element_buffer(&value).unwrap_or(value)
        }
        _ => value,
    }
}

/// The same question for INTERNAL DELIVERY — an input link or device support
/// handing a record a value — which has only C's scalar arm.
///
/// C's link layer asks for exactly ONE element (`dbGetLink(..., nRequest =
/// NULL)`), so `dbGet` converts the field at offset 0 and the record sees a
/// scalar: a waveform INP into an `ai.VAL` lands `wf[0]` rather than being
/// dropped by the typed `put_field` arm it does not match.
///
/// There is no array arm to render into, and that is the whole reason this is
/// a second entry rather than a flag on [`put_value_in_field_shape`]: internal
/// delivery is not a `dbPut`. A `compress` INP delivering ONE sample means
/// `compress_scalar` — the running `cvb` accumulator — and rendering it as a
/// one-element buffer runs `push_array`'s array algorithm instead, which
/// writes the N clamp back into the record.
pub fn link_value_in_field_shape<R: Record + ?Sized>(
    record: &R,
    field: &str,
    value: EpicsValue,
) -> EpicsValue {
    // The scalar arm reshapes only an array, so a scalar never asks the
    // destination's shape.
    if !value.is_array() {
        return value;
    }
    if StoredShape::of(record, field).is_array {
        return value;
    }
    value.first_element().unwrap_or(value)
}

/// What a put asks of the value its destination field holds: the type it is
/// stored as (the `.dbd` type for a field holding none) and whether it is a
/// buffer. Read once per put, since `get_field` copies the value out.
#[derive(Clone, Copy)]
pub(crate) struct StoredShape {
    ty: Option<DbFieldType>,
    /// The field's CURRENT VALUE SHAPE, not
    /// [`FieldDeclaration::field_is_dbaddr`]: `mbbo.VAL` is
    /// `special(SPC_DBADDR)` and stored as a scalar, and wrapping a request to
    /// it would put a one-element array in front of the menu row that owns it.
    is_array: bool,
}

impl StoredShape {
    pub(crate) fn of<R: Record + ?Sized>(record: &R, field: &str) -> Self {
        let current = record.get_field(field);
        StoredShape {
            ty: current
                .as_ref()
                .map(EpicsValue::db_field_type)
                .or_else(|| super::record_instance::declared_field_type_of(record, field)),
            is_array: current.is_some_and(|v| v.is_array()),
        }
    }

    pub(crate) fn ty(&self) -> Option<DbFieldType> {
        self.ty
    }

    pub(crate) fn is_array(&self) -> bool {
        self.is_array
    }
}

/// The client `dbPut` entry to the write-side converter: C `dbPut`'s value
/// branch in full — the destination-shape arm
/// ([`put_value_in_field_shape`]) and then the `dbPutConvertRoutine` row for
/// the request that arm produced ([`coerce_put_value`]).
///
/// Separate from `coerce_put_value` because the two ways a value enters a
/// record field have different contracts, and one function that renders the
/// shape for both means two things by context. Only a client `dbPut` gets the
/// array arm; internal delivery calls `coerce_put_value` with a request whose
/// shape [`link_value_in_field_shape`] already settled, and
/// `RecordInstance::put_declared_override` calls it for a destination that has
/// no shape at all (a shadow cell for a field the record does not serve).
///
/// It is UNCONDITIONAL. Its caller must not skip it when the request already
/// carries the destination's DBF: that skip is what let a scalar reach a
/// buffer field unrendered, so that `histogram.VAL` had to grow a scalar arm
/// of its own to accept what `dbPut` converts for every other array field.
pub fn dbput_coerce_value<R: Record + ?Sized>(
    record: &R,
    field: &str,
    target: DbFieldType,
    value: EpicsValue,
) -> CaResult<Converted> {
    let shape = StoredShape::of(record, field);
    dbput_coerce_value_in(
        record,
        field,
        shape,
        record.field_is_dbaddr(field),
        target,
        value,
    )
}

/// [`dbput_coerce_value`] against a destination already read: `shape` and
/// `dbaddr` ([`FieldDeclaration::field_is_dbaddr`]) are the field's.
pub(crate) fn dbput_coerce_value_in<R: Record + ?Sized>(
    record: &R,
    field: &str,
    shape: StoredShape,
    dbaddr: bool,
    target: DbFieldType,
    value: EpicsValue,
) -> CaResult<Converted> {
    // C's row test (`dbAccess.c:1355`), asked of the request before its shape
    // is settled: `nRequest > 1 || special == SPC_DBADDR`. A scalar-stored
    // `SPC_DBADDR` field (`mbbo.VAL`, an `aSub` channel at NOA 1) takes the
    // `putString*` row, which has no empty-string arm.
    let row = PutRow::DbPut {
        multi_request: value.count() > 1,
        dbaddr,
    };
    let value = put_value_in_shape(record, field, shape, value);
    coerce_put_value_in_row(record, field, target, value, row)
}

/// `value` as the one-element buffer of its OWN element type. The element type
/// is not this function's business — [`coerce_put_value`] runs C's
/// `dbPutConvertRoutine` row over the request afterwards, array to array.
fn one_element_buffer(value: &EpicsValue) -> Option<EpicsValue> {
    Some(match value {
        EpicsValue::Short(v) => EpicsValue::ShortArray(vec![*v].into()),
        EpicsValue::Float(v) => EpicsValue::FloatArray(vec![*v].into()),
        EpicsValue::Enum(v) => EpicsValue::EnumArray(vec![*v].into()),
        EpicsValue::Double(v) => EpicsValue::DoubleArray(vec![*v].into()),
        EpicsValue::Long(v) => EpicsValue::LongArray(vec![*v].into()),
        EpicsValue::Int64(v) => EpicsValue::Int64Array(vec![*v].into()),
        EpicsValue::UInt64(v) => EpicsValue::UInt64Array(vec![*v].into()),
        EpicsValue::UShort(v) => EpicsValue::UShortArray(vec![*v].into()),
        EpicsValue::ULong(v) => EpicsValue::ULongArray(vec![*v].into()),
        EpicsValue::UChar(v) => EpicsValue::UCharArray(vec![*v].into()),
        EpicsValue::Char(v) => EpicsValue::CharArray(vec![*v].into()),
        EpicsValue::String(v) => EpicsValue::StringArray(vec![v.clone()].into()),
        // Not a stored value; `convert_to` collapses it to a bare index.
        EpicsValue::EnumWithChoices { .. } => return None,
        // Already a buffer.
        _ => return None,
    })
}

pub fn put_field_internal_default<R: Record + ?Sized>(
    record: &mut R,
    name: &str,
    value: EpicsValue,
) -> CaResult<()> {
    // Coerce to the type the record STORES, not the type it SERVES. The two are
    // the same for most fields, but a `menu()` field is declared `DBF_MENU` and
    // served as `DBR_ENUM` with its choices (`promote_menu_value`) while the
    // record stores the bare choice index as a `Short` — and `put_field`'s arms
    // match on what is stored. Coercing to the served type would hand every
    // `put_field` an `Enum` its `Short` arm cannot match. This is the inverse of
    // `promote_menu_value`, and asking the record what it holds keeps the rule
    // uniform instead of special-casing menus here.
    //
    // The `.dbd` type is the fallback for a field the record cannot currently
    // produce a value for (an uninitialised array, a port-internal field).
    let target_type = record
        .get_field(name)
        .map(|v| v.db_field_type())
        .or_else(|| crate::server::record::record_instance::declared_field_type_of(record, name));
    let value = link_value_in_field_shape(&*record, name, value);
    let is_enum_carrier = matches!(value, EpicsValue::EnumWithChoices { .. });
    let value = match target_type {
        // A String target routes through the converter even on a type match: C's
        // `putStringString` truncates to `field_size - 1` (see `coerce_put_value`).
        Some(target)
            if is_enum_carrier
                || ((value.db_field_type() != target || target == DbFieldType::String)
                    && !value.is_empty_array()) =>
        {
            match coerce_put_value(record, name, target, value)? {
                Converted::Stored(v) => v,
                // C's converter returned success without storing: the field
                // keeps its old value and the put still succeeds.
                Converted::Unchanged => return Ok(()),
            }
        }
        // Carrier with no known target field: collapse to a bare index (the prior
        // fallback) rather than letting it reach storage.
        None if is_enum_carrier => value.into_type(DbFieldType::Long),
        _ => value,
    };
    record.put_field(name, value)
}

/// C's `putString*` row when the destination is an ARRAY — `Ok` when this row
/// owns the put, `Err` handing the request back when the scalar rows below do.
///
/// `dbPut` reaches it for every `special(SPC_DBADDR)` field regardless of
/// `nRequest` (`dbAccess.c:1350`), so a one-element string put is this row too;
/// keeping the two counts on one owner is what stops them drifting apart.
///
/// The two array destinations whose row is a byte copy rather than a parse — a
/// long-string field (`lsi`/`lso`/`printf` VAL, `DBF_STRING` in their
/// `cvt_dbaddr`) and an `FTVL=STRING` buffer, whose `NumericField::of` is
/// `None` — still belong to this row; it hands them the text unchanged for
/// their record to store.
///
/// A destination stored as a scalar gets its one element back as a scalar;
/// its enum row is `putStringEnum`, which parses the index with `&end` where
/// `cvt_st_e` passes NULL, so trailing text after the number is accepted.
fn put_string_array_row<R: Record + ?Sized>(
    record: &R,
    field: &str,
    target: DbFieldType,
    value: EpicsValue,
    row: PutRow,
) -> CaResult<Result<Converted, EpicsValue>> {
    let texts: &[crate::types::PvString] = match &value {
        EpicsValue::String(s) => std::slice::from_ref(s),
        EpicsValue::StringArray(a) => a,
        _ => return Ok(Err(value)),
    };
    let array_row = match row {
        PutRow::DbPut {
            multi_request,
            dbaddr,
        } => multi_request || dbaddr,
        PutRow::StoredShape => record.field_stores_array(field),
    };
    if !array_row {
        return Ok(Err(value));
    }
    // `putStringString` is a byte copy, and the record stores the text: hand the
    // value over unchanged rather than falling through to the SCALAR numeric
    // row below, which would parse `printf.VAL`'s `"7"` into the number 7.
    if record.field_is_long_string(field) {
        // `putStringString` into `field_size = SIZV`: the same cap, one owner.
        let value = match value {
            EpicsValue::String(s) => EpicsValue::String(cap_string_to_field_size(record, field, s)),
            other => other,
        };
        return Ok(Ok(Converted::Stored(value)));
    }
    // A scalar-held destination has `no_elements` 1, so `dbPut` clamps the
    // request to its first element (`dbAccess.c:1359`) and converts only that.
    let first = match texts {
        [first, ..] if !record.field_stores_array(field) => Some(first),
        _ => None,
    };
    if let Some(first) = first
        && target == DbFieldType::Enum
    {
        return super::resolve_put_string_enum(field, record, first)
            .map(|v| Ok(Converted::Stored(v)));
    }
    let Some(numeric) = c_parse::NumericField::of(target) else {
        // `putStringString` per element: each is cut at `field_size - 1`.
        if target == DbFieldType::String {
            let capped = match value {
                EpicsValue::String(s) => {
                    EpicsValue::String(cap_string_to_field_size(record, field, s))
                }
                EpicsValue::StringArray(a) => EpicsValue::StringArray(
                    a.iter()
                        .map(|s| cap_string_to_field_size(record, field, s.clone()))
                        .collect::<Vec<_>>()
                        .into(),
                ),
                other => other,
            };
            return Ok(Ok(Converted::Stored(capped)));
        }
        return Ok(Ok(Converted::Stored(value)));
    };
    match first {
        Some(first) => c_parse::put_string_element(field, numeric, &first.as_str_lossy()).map(Ok),
        None => c_parse::put_string_elements(field, numeric, texts).map(Ok),
    }
}

/// Coerce a written value to a field's stored type — the single owner of C
/// `dbConvert.c`'s `dbFastPutConvertRoutine[dbrType][field_type]` table, shared
/// by the two paths a value can enter a record's field through: a client
/// `dbPut` (`crate::server::database::field_io`) and an internal link /
/// device-support delivery ([`put_field_internal_default`]).
///
/// Every `DBR_STRING` row of C's put table is a converter that can FAIL, and
/// none of them is `EpicsValue::convert_to`:
///
/// * `DBF_MENU` → `putStringMenu` — exact label, else an index below `nChoice`
///   ([`crate::server::record::resolve_menu_field_string`]).
/// * `DBF_ENUM` → `putStringEnum` — the record's state strings, else an index
///   below `no_str` ([`crate::server::record::resolve_enum_state_string`]).
/// * `DBF_STRING` → `putStringString` — a byte copy, the one row that cannot
///   fail.
/// * every numeric width → `putStringChar` … `putStringDouble`, i.e.
///   `epicsParse*`, which refuses the put on overflow and on unparseable text
///   ([`c_parse::put_string`]).
///
/// `convert_to` cannot express any of the failures — it is field-blind and
/// total, mapping unparseable text to `0` and an out-of-range number to the
/// nearest representable one. That is how `caput MY:VALVE Open` became a silent
/// no-op that drove `VAL` to state 0, and how `caput REC.PREC 32768` — which the
/// compiled softIoc REFUSES — stored 32767.
///
/// This is the TYPE row alone. The request's shape is settled before it
/// arrives, by whichever entry the value came through
/// ([`dbput_coerce_value`] for a client `dbPut`,
/// [`link_value_in_field_shape`] for internal delivery), so nothing here
/// renders a destination shape that one of those two contracts would disagree
/// with.
///
/// An ARRAY destination takes the same rule through
/// [`c_parse::put_string_elements`], which runs `putString*` over every element
/// of the request. It is run HERE rather than delegated to the record, because
/// delegation made the row a convention nothing enforced: `waveform` ran it
/// while `compress.VAL` and `histogram.VAL` answered `TypeMismatch` to a put the
/// compiled softIoc accepts, and the `StringArray` half of the same row never
/// reached this function at all — it fell through to the total `convert_to`,
/// which stored `0.0` for text `epicsParseFloat64` refuses.
///
/// Two array destinations are outside the numeric row, and both are C's own
/// distinction rather than a carve-out here: a LONG-STRING field
/// ([`FieldDesc::long_string`]) is `DBF_STRING` in its `cvt_dbaddr`, so its
/// row is `putStringString`, a byte copy; and an `FTVL=STRING` buffer is the
/// same copy per element. Those keep the string and their records store it. A
/// string reaching a char buffer as its BYTES is the DBR_CHAR row (`caput -S`),
/// which arrives as a `CharArray` and needs no conversion.
pub fn coerce_put_value<R: Record + ?Sized>(
    record: &R,
    field: &str,
    target: DbFieldType,
    value: EpicsValue,
) -> CaResult<Converted> {
    coerce_put_value_in_row(record, field, target, value, PutRow::StoredShape)
}

/// How a put picks between `dbPutConvertRoutine` (`putString*`) and
/// `dbFastPutConvertRoutine`, asked only once the request is a string.
#[derive(Clone, Copy)]
enum PutRow {
    /// `dbPut`'s own test (`dbAccess.c:1355`): `nRequest > 1`, read before
    /// the request's shape was rendered, or `special == SPC_DBADDR`.
    DbPut { multi_request: bool, dbaddr: bool },
    /// An internal delivery: the destination's stored shape.
    StoredShape,
}

/// [`coerce_put_value`] with the row picked by `row`.
fn coerce_put_value_in_row<R: Record + ?Sized>(
    record: &R,
    field: &str,
    target: DbFieldType,
    value: EpicsValue,
    row: PutRow,
) -> CaResult<Converted> {
    // The ARRAY row first, so both counts of the SAME C routine have one owner:
    // a scalar `String` and a `StringArray` into an array destination are both
    // `dbPutConvertRoutine[DBR_STRING][target]` over `nRequest` elements
    // (`dbAccess.c:1350` takes that arm for every `special(SPC_DBADDR)` field,
    // whatever the count). Menu/DTYP/enum fields are never array destinations,
    // so nothing below is shadowed.
    let value = match put_string_array_row(record, field, target, value, row)? {
        Ok(converted) => return Ok(converted),
        Err(value) => value,
    };
    if let EpicsValue::String(s) = &value {
        // A DBF_DEVICE field validates against the record type's FULL device
        // menu — static `device()` lines PLUS runtime-contributed device
        // support — the same set the read/announce path exposes via
        // `RecordInstance::device_choices`. `menu_choices_with`'s device branch
        // returns only the static half, so a contributed device-support name
        // (asyn's `asynInt32`, scaler-rs's `Asyn Scaler`, ...) would wrongly
        // fail this put even though a client can read it in the DTYP choices.
        // An empty menu refuses every string, as `cvt_st_device` does.
        let desc = super::record_instance::field_desc_of(record, field);
        if super::record_instance::is_device(desc) {
            let choices = super::merged_device_menu(record.record_type());
            return super::menu_choices::resolve_device_field_string(
                field,
                &choices,
                target,
                &s.as_str_lossy(),
            )
            .map(Converted::Stored);
        }
        if let Some(choices) = super::record_instance::menu_choices_with(record, field, desc) {
            return super::resolve_menu_field_string(field, choices, target, &s.as_str_lossy())
                .map(Converted::Stored);
        }
        if target == DbFieldType::Enum {
            return super::resolve_enum_state_string(field, record, s).map(Converted::Stored);
        }
        if let Some(numeric) = c_parse::NumericField::of(target) {
            return c_parse::put_string(field, numeric, &s.as_str_lossy());
        }
    }
    if target == DbFieldType::String
        && let EpicsValue::String(s) = value
    {
        // C `putStringString` (dbConvert.c:916-925): `strncpy(pdst, psrc,
        // field_size); pdst[field_size-1] = 0` — the DBF_STRING put
        // truncates to `field_size - 1` bytes. The row is NOT a no-op even
        // for a String source, so it must run even when source and stored
        // type match (the two gates that call this converter skip it on a
        // type match; both route a String target here regardless). The CA
        // wire already caps a DBR_STRING at `MAX_STRING_SIZE - 1` (39), so
        // this only bites a field whose `.dbd` `size(N)` is under 40 —
        // dbCommon `ASG` `size(29)` → 28, and the like.
        return Ok(Converted::Stored(EpicsValue::String(
            cap_string_to_field_size(record, field, s),
        )));
    }
    // C's DBR_STRING put column renders a number at the record's
    // `get_precision`, seeded 6: `cvt_*_st` (`dbFastLinkConv.c:1216`/`:1333`)
    // for the scalar `dbAccess.c:1391` takes, through the one renderer the
    // lean `Str` arm also uses. A long-string field is `DBF_STRING` of
    // `field_size = SIZV` in its `cvt_dbaddr` (`lsoRecord.c:183-186`), so it
    // takes the same row and `putStringString`'s cap.
    if target == DbFieldType::String || record.field_is_long_string(field) {
        if let Some(n) = value.num() {
            let text = crate::server::database::render_for_string_target(record, n)
                .ok_or_else(|| crate::error::CaError::TypeMismatch(field.into()))?;
            return Ok(Converted::Stored(EpicsValue::String(
                cap_string_to_field_size(record, field, PvString::from(text.as_bytes())),
            )));
        }
        // `putFloatString`/`putDoubleString` (`dbConvert.c:1558`/`:1600`)
        // per element of an array request.
        if let Some(rendered) =
            crate::types::codec::dbr_string_at_precision(&value, put_string_precision(record))
        {
            return Ok(Converted::Stored(rendered));
        }
    }
    Ok(Converted::Stored(value.into_type(target)))
}

/// C's `prset->get_precision` answer for a **`DBF_STRING`** destination — the
/// precision `putFloatString`/`putDoubleString` render a numeric put with.
///
/// The seed is 6 and only `get_precision` overwrites it
/// (`dbConvert.c:1562-1568`, `dbFastLinkConv.c:1220-1228`). For a STRING field
/// the shared tail is a no-op: `recGblGetPrec`'s switch has no `DBF_STRING`
/// case (`recGbl.c:141-142`), so whatever the body seeded survives. And no
/// `get_precision` in base or in the ported modules names a `DBF_STRING` field
/// in its own switch, so the answer is per RECORD rather than per field: it is
/// `PREC` for every body that seeds `*precision = prec->prec` ahead of the tail
/// — ai, ao, aai, aao, aSub, calc, calcout, compress, dfanout, sel, seq, sub,
/// subArray, waveform, sCalcout, aCalcout, epid, mca, motor, scaler, sseq,
/// swait, throttle, transform. The two ported types that do not seed:
///
/// * `histogram` — a switch with no seed and no case a dbCommon field reaches,
///   so the caller's 6 arrives at `cvtDoubleToString`
///   (`histogramRecord.c:420-438`).
/// * `asyn` — `*precision = 0;` before the tail (`asynRecord.c::get_precision`).
///
/// `bo`, `busy`, `mbbiDirect` and `mbboDirect` are unseeded too and need no arm:
/// none of them declares `PREC`, so the fallback is already C's 6. No record
/// type declares `PREC` while NULLing `get_precision` (checked across base and
/// the ported modules), which is what lets "has a PREC field" stand in for
/// "supplies the slot" without a second table to keep in step.
///
/// A negative PREC is not clamped, for the reason
/// [`crate::types::codec`]'s GET side spells out: C hands the `long` to
/// `cvtDoubleToString`'s `epicsUInt16` parameter and the conversion
/// reinterprets it.
pub(crate) fn put_string_precision<R: Record + ?Sized>(record: &R) -> u16 {
    match record.record_type() {
        "histogram" => 6,
        "asyn" => 0,
        _ => record
            .get_field("PREC")
            .and_then(|v| v.as_int_i64())
            .map_or(6, |p| p as i16 as u16),
    }
}

/// C `putStringString`'s truncation: a `DBF_STRING` field stores at most
/// `field_size - 1` bytes (its `.dbd` `size(N)` less the forced NUL). A field
/// with no declared size (`0` — a Tier 3 hand table, or a field with no
/// declaration) is left uncapped beyond the wire's own `MAX_STRING_SIZE - 1`.
fn cap_string_to_field_size<R: Record + ?Sized>(record: &R, field: &str, s: PvString) -> PvString {
    match super::record_instance::field_desc_of(record, field) {
        // C caps at `paddr->field_size`: the member's `size(N)`, or for a
        // `cvt_dbaddr` field the served element's width (40 for FTx=STRING,
        // SIZV for lso/lsi/printf).
        Some(desc) if !desc.unreadable() => {
            let width = served_element_size(record, desc, DbfCode::String) as usize;
            if width > 0 && s.len() >= width {
                let mut bytes = s.into_bytes();
                bytes.truncate(width - 1);
                PvString::from_bytes(bytes)
            } else {
                s
            }
        }
        _ => s,
    }
}

/// The served element width of `desc` on `record` — the one owner of C's
/// `paddr->field_size` after `cvt_dbaddr`: the record's own answer
/// ([`Record::dbaddr_element_size`]), else the declaration's
/// ([`FieldDesc::element_size`]). A put's string cap, `dba` and the `$` view
/// all read this.
pub(crate) fn served_element_size<R: Record + ?Sized>(
    record: &R,
    desc: &FieldDesc,
    served: DbfCode,
) -> u16 {
    record
        .dbaddr_element_size(desc.name)
        .unwrap_or_else(|| desc.element_size(served))
}

/// Subroutine function type for `sub`/`aSub` records.
///
/// The return value is the subroutine's C `long` status
/// (`subRecord.c::do_sub` / `aSubRecord.c::do_sub`): `< 0` raises
/// `SOFT_ALARM` at the record's `BRSV` severity, and for `aSub` the status
/// is published as `VAL` (`aSubRecord.c:224`). Return `Ok(0)` for the
/// normal no-alarm path. `Err(..)` is reserved for an infrastructure
/// failure inside the closure (e.g. a field write error), which aborts
/// processing — it is distinct from a negative status.
pub type SubroutineFn = Box<dyn Fn(&mut dyn Record) -> CaResult<i64> + Send + Sync>;

/// A subroutine that also sees what C's reaches through `prec` besides the
/// fields: its record's name, `dpvt`, and the IOC it takes `dbScanLock` and
/// `scanOnce` against (autosave's `configMenuSub.c`).
pub type ContextSubroutineFn = Box<
    dyn Fn(&mut dyn Record, &mut crate::server::database::SubroutineContext<'_>) -> CaResult<i64>
        + Send
        + Sync,
>;

/// A registered `INAM`/`SNAM` function. The kind is fixed at registration,
/// so a record resolves it once and `do_sub` only dispatches.
pub enum Subroutine {
    Plain(SubroutineFn),
    Context(ContextSubroutineFn),
}

#[cfg(test)]
mod declaration_numbering_tests {
    use super::*;
    use crate::server::record::dbd_generated::{DB_COMMON_FIELDS, record_fields};

    /// C's `special.h:26-39` @R7.0.10 in full, plus the 0 C leaves in
    /// `pdbFldDes->special` for a field with no `special()`.
    ///
    /// The WHOLE table, not a sample, because the numbering is not
    /// contiguous: C has no 4, and the record-specific half jumps to 100. A
    /// twelfth variant inserted in the middle must fail on its own row here
    /// rather than shift every code after it.
    const C_SPECIAL: [(Special, i16, &str); 11] = [
        (Special::None, 0, "(none)"),
        (Special::NoMod, 1, "SPC_NOMOD"),
        (Special::DbAddr, 2, "SPC_DBADDR"),
        (Special::Scan, 3, "SPC_SCAN"),
        (Special::AlarmAck, 5, "SPC_ALARMACK"),
        (Special::As, 6, "SPC_AS"),
        (Special::Attribute, 7, "SPC_ATTRIBUTE"),
        (Special::Mod, 100, "SPC_MOD"),
        (Special::Reset, 101, "SPC_RESET"),
        (Special::LinConv, 102, "SPC_LINCONV"),
        (Special::Calc, 103, "SPC_CALC"),
    ];

    /// Exhaustive by construction: a twelfth variant stops the build here, so
    /// the distinctness check below turns 11 rows into a proof of coverage.
    fn _the_enum_has_no_variant_outside_the_table(s: Special) {
        match s {
            Special::None
            | Special::NoMod
            | Special::DbAddr
            | Special::Scan
            | Special::AlarmAck
            | Special::As
            | Special::Attribute
            | Special::Mod
            | Special::Reset
            | Special::LinConv
            | Special::Calc
            | Special::Record(_) => (),
        }
    }

    #[test]
    fn every_special_carries_cs_spc_number() {
        for (spc, n, name) in C_SPECIAL {
            assert_eq!(spc.code(), n, "{name}: wrong SPC_ number");
            assert_eq!(Special::from_code(n), Some(spc), "{name}: numeric form");
        }
        for (a, (x, ..)) in C_SPECIAL.iter().enumerate() {
            for (y, ..) in C_SPECIAL.iter().skip(a + 1) {
                assert_ne!(x, y, "{x:?} listed twice");
            }
        }
        assert!(!C_SPECIAL.iter().any(|(_, n, _)| *n == 4), "C has no SPC 4");
        assert_eq!(Special::from_code(4), None);
        assert_eq!(Special::from_code(111), Some(Special::Record(111)));
        assert_eq!(Special::Record(111).code(), 111);
        // `pamapspcType[SPC_NTYPES]` (`special.h:42,51-61`) is the `.dbd`
        // parser's table: nine entries, omitting SPC_ATTRIBUTE — which C sets
        // internally — and the 0 that means no `special()` at all.
        assert_eq!(
            C_SPECIAL
                .iter()
                .filter(|(s, ..)| !matches!(s, Special::None | Special::Attribute))
                .count(),
            9,
        );
    }

    #[test]
    fn the_declared_token_survives_the_generators_collapse() {
        // `dbf_to_rust` maps DBF_INLINK|DBF_OUTLINK|DBF_FWDLINK onto one
        // served type, so `dbf_type` cannot tell a forward link from an
        // output link — and the field NAME cannot either: `LNK1` is
        // DBF_OUTLINK on sseq and DBF_FWDLINK on fanout. Only the carried
        // declaration separates them, which is what `dba` and `dbDumpField`
        // have to print.
        let field = |rec: &str, name: &str| {
            record_fields(rec)
                .unwrap_or_else(|| panic!("no field table for {rec}"))
                .iter()
                .find(|f| f.name == name)
                .unwrap_or_else(|| panic!("{rec}.{name} not in the table"))
        };
        let sseq_lnk1 = field("sseq", "LNK1");
        let fanout_lnk1 = field("fanout", "LNK1");
        let ai_inp = field("ai", "INP");
        let flnk = DB_COMMON_FIELDS
            .iter()
            .find(|f| f.name == "FLNK")
            .expect("dbCommon.FLNK");

        for f in [sseq_lnk1, fanout_lnk1, ai_inp, flnk] {
            assert_eq!(f.dbf_type, DbFieldType::String, "{}: served type", f.name);
        }
        assert_eq!(sseq_lnk1.declared_dbf, DbfCode::Outlink);
        assert_eq!(fanout_lnk1.declared_dbf, DbfCode::Fwdlink);
        assert_eq!(ai_inp.declared_dbf, DbfCode::Inlink);
        assert_eq!(flnk.declared_dbf, DbfCode::Fwdlink);

        // The other half of the collapse: DBF_ENUM|DBF_MENU|DBF_DEVICE all
        // serve as Enum. `DTYP` is the one DBF_DEVICE field in all of base.
        let dtyp = DB_COMMON_FIELDS
            .iter()
            .find(|f| f.name == "DTYP")
            .expect("dbCommon.DTYP");
        let scan = DB_COMMON_FIELDS
            .iter()
            .find(|f| f.name == "SCAN")
            .expect("dbCommon.SCAN");
        assert_eq!(dtyp.dbf_type, DbFieldType::Enum);
        assert_eq!(dtyp.declared_dbf, DbfCode::Device);
        assert_eq!(scan.dbf_type, DbFieldType::Enum);
        assert_eq!(scan.declared_dbf, DbfCode::Menu);
    }

    #[test]
    fn no_access_reads_the_declaration_not_the_served_type() {
        // `waveform.VAL` is declared DBF_NOACCESS and re-typed by
        // `cvt_dbaddr`, so its served type says nothing; the declaration is
        // what `dbPutString` refuses a `.db` assignment on.
        let val = record_fields("waveform")
            .expect("waveform")
            .iter()
            .find(|f| f.name == "VAL")
            .expect("waveform.VAL");
        assert_eq!(val.declared_dbf, DbfCode::NoAccess);
        assert!(val.no_access());
        assert_ne!(val.dbf_type, DbFieldType::String, "re-typed by cvt_dbaddr");

        // `mbbo.VAL` is the counter-example the doc names: SPC_DBADDR too,
        // but declared DBF_ENUM, so it is settable.
        let mbbo_val = record_fields("mbbo")
            .expect("mbbo")
            .iter()
            .find(|f| f.name == "VAL")
            .expect("mbbo.VAL");
        assert!(!mbbo_val.no_access());

        // A hand-written descriptor has no `.dbd` behind it, so its
        // declaration is its served type's own code and it is never a C
        // internal.
        let hand = FieldDesc::new("VAL", DbFieldType::Double, false);
        assert_eq!(hand.declared_dbf, DbfCode::Double);
        assert!(!hand.no_access());
    }
}

/// `(record type, field)` pairs declared `Special::None` whose put still
/// runs a hook — a `special()` or `on_put` arm the `.dbd` cannot see. C
/// skips `dbPutSpecial` when `special == 0`; a lean put here would skip the
/// hook as well, so these take the general put.
/// `special_none_fields_have_no_put_hooks` holds the list.
pub const FORCE_GENERAL: &[(&str, &str)] = &[];

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::server::records::compress::CompressRecord;

    /// A snapshot whose posts own no heap drops without the out-of-line
    /// release; one that does still frees through it.
    #[test]
    fn a_snapshot_releases_only_what_owns_heap() {
        use crate::server::recgbl::EventMask;
        let ao_val = crate::server::record::declared_field_registry::declared_fields("ao")
            .find(|d| d.name == "VAL")
            .unwrap();
        let releases = |posts: Vec<FieldPost>| {
            let before = RELEASES.with(|n| n.get());
            let mut snap = ProcessSnapshot::new();
            for post in posts {
                snap.push(post);
            }
            drop(snap);
            RELEASES.with(|n| n.get()) - before
        };
        let val = |v| {
            (
                PostName::Declared(ao_val),
                PostedValue::Val(v),
                EventMask::VALUE,
            )
        };
        assert_eq!(releases(vec![]), 0);
        assert_eq!(releases(vec![val(EpicsValue::Double(1.0))]), 0);
        assert_eq!(
            releases(vec![(
                PostName::Declared(ao_val),
                PostedValue::Ref,
                EventMask::VALUE
            )]),
            0
        );
        assert_eq!(releases(vec![val(EpicsValue::String("s".into()))]), 1);
        assert_eq!(
            releases(vec![(
                PostName::Named(String::from("X").into()),
                PostedValue::Ref,
                EventMask::VALUE
            )]),
            1
        );
        let two = vec![val(EpicsValue::Double(1.0)), val(EpicsValue::Double(2.0))];
        assert_eq!(releases(two), 1, "a spilled post's Vec is freed");
    }

    /// `retain_from` keeps every post before `first` unasked, asks `keep`
    /// once of each later one, and keeps the survivors in order, at each
    /// boundary of the head/rest split.
    #[test]
    fn retain_from_asks_only_of_the_posts_from_first_on() {
        use crate::server::recgbl::EventMask;
        let ao_val = crate::server::record::declared_field_registry::declared_fields("ao")
            .find(|d| d.name == "VAL")
            .unwrap();
        let run = |len: usize, first: usize, drop: &[usize]| {
            let mut snap: ProcessSnapshot = (0..len)
                .map(|i| {
                    (
                        PostName::Declared(ao_val),
                        PostedValue::Val(EpicsValue::Long(i as i32)),
                        EventMask::VALUE,
                    )
                })
                .collect();
            let mut asked = Vec::new();
            snap.retain_from(first, |(_, val, _)| {
                let PostedValue::Val(EpicsValue::Long(i)) = val else {
                    unreachable!()
                };
                asked.push(*i as usize);
                !drop.contains(&(*i as usize))
            });
            let kept: Vec<usize> = snap
                .iter()
                .map(|(_, val, _)| match val {
                    PostedValue::Val(EpicsValue::Long(i)) => *i as usize,
                    _ => unreachable!(),
                })
                .collect();
            (asked, kept)
        };
        assert_eq!(run(0, 0, &[]), (vec![], vec![]));
        assert_eq!(run(1, 1, &[0]), (vec![], vec![0]), "walk added nothing");
        assert_eq!(run(3, 3, &[0, 1, 2]), (vec![], vec![0, 1, 2]));
        assert_eq!(run(1, 0, &[0]), (vec![0], vec![]));
        assert_eq!(
            run(3, 0, &[0]),
            (vec![0, 1, 2], vec![1, 2]),
            "rest refills head"
        );
        assert_eq!(run(3, 0, &[0, 1, 2]), (vec![0, 1, 2], vec![]));
        assert_eq!(run(3, 1, &[0, 1]), (vec![1, 2], vec![0, 2]));
        assert_eq!(run(4, 2, &[2]), (vec![2, 3], vec![0, 1, 3]));
        assert_eq!(run(5, 1, &[2, 4]), (vec![1, 2, 3, 4], vec![0, 1, 3]));
    }

    /// Every module-owned type too: the sweeps below cover `RECORD_TYPES`.
    pub(crate) fn any_record(record_type: &str) -> Box<dyn Record> {
        use crate::server::records::{
            acalcout::AcalcoutRecord, asyn_record::AsynRecord, busy::BusyRecord,
            scalcout::ScalcoutRecord, sseq::SseqRecord, swait::SwaitRecord,
            transform::TransformRecord,
        };
        fn f<R: Record + Default + 'static>() -> crate::server::RecordFactory {
            Box::new(|| Box::new(R::default()))
        }
        let factories = std::collections::HashMap::from([
            ("acalcout".to_string(), f::<AcalcoutRecord>()),
            ("asyn".to_string(), f::<AsynRecord>()),
            ("busy".to_string(), f::<BusyRecord>()),
            ("scalcout".to_string(), f::<ScalcoutRecord>()),
            ("sseq".to_string(), f::<SseqRecord>()),
            ("swait".to_string(), f::<SwaitRecord>()),
            ("transform".to_string(), f::<TransformRecord>()),
        ]);
        crate::server::db_loader::create_record_with_factories(record_type, &factories)
            .unwrap_or_else(|e| panic!("{record_type}: {e}"))
    }

    /// A DBF_FLOAT or unsigned field reads in its own variant: C's dbAddr
    /// carries the declared type, so transformRecord.dbd:17 VERS answers the
    /// float-rounded 5.800000190734863 on a DOUBLE get; likewise
    /// sCalcoutRecord.dbd:420 DLYA, busyRecord.dbd:64/117/122,
    /// aCalcoutRecord.dbd:813 and aSubRecord.dbd:749 for the unsigned types.
    #[test]
    fn a_float_or_unsigned_field_reads_in_its_declared_variant() {
        let mut checked = 0usize;
        for record_type in super::super::dbd_generated::RECORD_TYPES {
            let r = any_record(record_type);
            for d in r.field_list() {
                if d.runtime_typed
                    || d.unreadable()
                    || !matches!(
                        d.dbf_type,
                        DbFieldType::Float
                            | DbFieldType::UChar
                            | DbFieldType::UShort
                            | DbFieldType::ULong
                    )
                {
                    continue;
                }
                let Some(v) = r.get_field(d.name) else {
                    continue;
                };
                assert_eq!(v.db_field_type(), d.dbf_type, "{record_type}.{}", d.name);
                checked += 1;
            }
        }
        assert!(checked > 100, "{checked} float and unsigned fields");
    }

    /// Every field of `record` as `get_field` reads it, NaN-safe.
    fn image(record: &dyn Record) -> Vec<String> {
        record
            .field_list()
            .iter()
            .map(|f| format!("{}={:?}", f.name, record.get_field(f.name)))
            .collect()
    }

    /// The value a slot of this variant is stored with, as `put_field` takes it
    /// for a field declared `dbf`, for each boundary of the kind.
    fn boundary_puts(slot: &SlotMut<'_>, dbf: DbFieldType) -> Vec<EpicsValue> {
        macro_rules! ints {
            ($t:ty, $wrap:expr) => {
                [0 as $t, (-1i64) as $t, <$t>::MAX, <$t>::MIN]
                    .into_iter()
                    .map($wrap)
                    .collect()
            };
        }
        match slot {
            SlotMut::F64(_) => [0.0, -1.0, f64::MAX, f64::NAN]
                .into_iter()
                .map(EpicsValue::Double)
                .collect(),
            SlotMut::F32(_) => [0.0, -1.0, f32::MAX, f32::NAN]
                .into_iter()
                .map(EpicsValue::Float)
                .collect(),
            SlotMut::I8(_) => ints!(i8, |v| EpicsValue::Char(v as u8)),
            SlotMut::U8(_) => ints!(u8, EpicsValue::UChar),
            SlotMut::I16(_) => ints!(i16, EpicsValue::Short),
            SlotMut::U16(_) if dbf == DbFieldType::Enum => ints!(u16, EpicsValue::Enum),
            SlotMut::U16(_) => ints!(u16, EpicsValue::UShort),
            SlotMut::I32(_) => ints!(i32, EpicsValue::Long),
            SlotMut::U32(_) => ints!(u32, EpicsValue::ULong),
            SlotMut::I64(_) => ints!(i64, EpicsValue::Int64),
            SlotMut::U64(_) => ints!(u64, EpicsValue::UInt64),
            SlotMut::Str(_) => ["x".repeat(39), "y".repeat(40)]
                .into_iter()
                .map(|s| EpicsValue::String(s.into()))
                .collect(),
        }
    }

    /// The `DBF_*` a slot of this variant may be declared as.
    fn variant_agrees(slot: &SlotRef<'_>, dbf: DbFieldType) -> bool {
        use DbFieldType as T;
        matches!(
            (slot, dbf),
            (SlotRef::F64(_), T::Double)
                | (SlotRef::F32(_), T::Float)
                | (SlotRef::I8(_), T::Char)
                | (SlotRef::U8(_), T::UChar)
                | (SlotRef::I16(_), T::Short)
                | (SlotRef::U16(_), T::UShort | T::Enum)
                | (SlotRef::I32(_), T::Long)
                | (SlotRef::U32(_), T::ULong)
                | (SlotRef::I64(_), T::Int64)
                | (SlotRef::U64(_), T::UInt64)
                | (SlotRef::Str(_), T::String)
        )
    }

    /// `value` as a `dbPut` hands it to the record: `putStringString` keeps
    /// `field_size - 1` bytes of a `DBF_STRING` (`dbConvert.c:923-924`).
    fn as_put(value: EpicsValue, desc: &FieldDesc) -> EpicsValue {
        match value {
            EpicsValue::String(s) if desc.size > 0 => {
                let keep = s.len().min(usize::from(desc.size) - 1);
                EpicsValue::String(s.as_bytes()[..keep].into())
            }
            other => other,
        }
    }

    /// `slot_mut(slot)` stores exactly what `put_field(name, v)` stores, its
    /// variant is the declared `DBF_*`, and `field_slot` hands out exactly the
    /// slots `slot_ref` answers. A slot pointing at the wrong member, or one
    /// whose `put_field` arm does more than store, fails here.
    #[test]
    fn every_slot_store_equals_the_name_store() {
        use crate::server::record::dbd_generated::RECORD_TYPES;
        let mut checked = 0usize;
        for record_type in RECORD_TYPES {
            let probe = any_record(record_type);
            for (i, desc) in probe.field_list().iter().enumerate() {
                let slot = FieldSlot(i as u16);
                let at = format!("{record_type}.{}", desc.name);
                let answered = probe.slot_ref(slot);
                let read = probe.get_slot_f64(slot).map(f64::to_bits);
                let funnel = answered
                    .as_ref()
                    .and_then(SlotRef::to_f64)
                    .map(f64::to_bits);
                assert_eq!(read, funnel, "{at}: get_slot_f64 disagrees with slot_ref");
                assert_eq!(
                    probe.field_slot(desc.name),
                    answered.as_ref().map(|_| slot),
                    "{at}: field_slot disagrees with slot_ref"
                );
                if let Some(r) = &answered {
                    assert!(
                        variant_agrees(r, desc.dbf_type),
                        "{at}: {r:?} for {:?}",
                        desc.dbf_type
                    );
                    // A slot read stands in for the whole name read only for a
                    // readable non-link field.
                    assert_eq!(
                        Some(r.to_value(desc.dbf_type)),
                        probe.get_field(desc.name),
                        "{at}: slot read disagrees with get_field"
                    );
                    assert!(!desc.unreadable(), "{at}: unreadable slot");
                    assert!(
                        crate::types::link_class_of(desc).is_none(),
                        "{at}: link slot"
                    );
                }
                let mut shape = any_record(record_type);
                let Some(m) = shape.slot_mut(slot) else {
                    continue;
                };
                assert!(answered.is_some(), "{at}: slot_mut without slot_ref");
                for value in boundary_puts(&m, desc.dbf_type) {
                    let value = as_put(value, desc);
                    let mut by_slot = any_record(record_type);
                    by_slot
                        .slot_mut(slot)
                        .unwrap()
                        .store(value.clone())
                        .unwrap();
                    let mut by_name = any_record(record_type);
                    match by_name.put_field(desc.name, value.clone()) {
                        // A `SPC_NOMOD` member's writers are the load, through
                        // the slot, and the record itself.
                        Err(_) if desc.read_only => continue,
                        Err(e) => panic!("{at}: put_field({value:?}): {e}"),
                        Ok(()) => {}
                    }
                    assert_eq!(image(&*by_slot), image(&*by_name), "{at} <- {value:?}");
                    if let EpicsValue::Double(v) = value {
                        let mut by_put = any_record(record_type);
                        if by_put.put_slot_f64(slot, v) {
                            assert_eq!(image(&*by_put), image(&*by_name), "{at} put_slot_f64");
                        }
                    }
                    checked += 1;
                }
            }
        }
        assert!(checked > 0, "no type maps a slot");
    }

    /// A type that classifies its declared fields classifies all of them, and
    /// each as what the record does with it: a member is a slot, a custom
    /// field is one the record serves, a spilt one the record does not.
    #[test]
    fn every_declared_store_matches_the_record() {
        use crate::server::record::dbd_generated::RECORD_TYPES;
        let mut classified = 0usize;
        for record_type in RECORD_TYPES {
            let mut record = any_record(record_type);
            let fields = record.field_list();
            if (0..fields.len()).all(|i| record.declared_store(FieldSlot(i as u16)).is_none()) {
                continue;
            }
            classified += 1;
            for (i, desc) in fields.iter().enumerate() {
                let slot = FieldSlot(i as u16);
                let at = format!("{record_type}.{}", desc.name);
                let served = record.get_field(desc.name).is_some();
                match record.declared_store(slot) {
                    None => panic!("{at}: unclassified"),
                    Some(DeclaredStore::Member) => {
                        assert!(record.slot_ref(slot).is_some(), "{at}: member without slot");
                        assert!(record.slot_mut(slot).is_some(), "{at}: member not writable");
                    }
                    Some(DeclaredStore::Custom) => {
                        assert!(record.slot_ref(slot).is_none(), "{at}: custom with a slot");
                        assert!(served || desc.no_access(), "{at}: custom, not served");
                    }
                    Some(DeclaredStore::Spill) => assert!(!served, "{at}: spilt, but served"),
                }
            }
        }
        assert!(classified > 0, "no type classifies its fields");
    }

    /// A member slot is the member `get_field` serves under the same name: a
    /// value stored through the slot reads back by name. A `slot_map!` entry
    /// pointing at a sibling member passes every test that writes and reads
    /// through the slot alone, the load census included.
    #[test]
    fn every_member_slot_store_reads_back_by_name() {
        use crate::server::record::dbd_generated::RECORD_TYPES;
        let mut checked = 0usize;
        for record_type in RECORD_TYPES {
            let probe = any_record(record_type);
            for (i, desc) in probe.field_list().iter().enumerate() {
                let slot = FieldSlot(i as u16);
                if probe.declared_store(slot) != Some(DeclaredStore::Member) {
                    continue;
                }
                let at = format!("{record_type}.{}", desc.name);
                let mut shape = any_record(record_type);
                let values = boundary_puts(&shape.slot_mut(slot).unwrap(), desc.dbf_type);
                for value in values {
                    let value = as_put(value, desc);
                    let mut record = any_record(record_type);
                    record.slot_mut(slot).unwrap().store(value.clone()).unwrap();
                    // Debug text, so a stored NaN compares equal to itself.
                    let by_slot = record.slot_ref(slot).unwrap().to_value(desc.dbf_type);
                    assert_eq!(
                        format!("{:?}", record.get_field(desc.name)),
                        format!("{:?}", Some(by_slot)),
                        "{at} <- {value:?}"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 0, "no type maps a member");
    }

    /// C `dbPut` skips `dbPutSpecial` for a field whose `special` is 0, and a
    /// lean put does the same, so every `Special::None` field outside
    /// [`FORCE_GENERAL`] must leave every field unchanged through
    /// `special(f, false)`, `special(f, true)` and `on_put(f)`.
    #[test]
    fn special_none_fields_have_no_put_hooks() {
        use crate::server::record::dbd_generated::RECORD_TYPES;
        let mut hooked = Vec::new();
        for record_type in RECORD_TYPES {
            let fields = any_record(record_type).field_list();
            for desc in fields {
                if desc.special != Special::None
                    || FORCE_GENERAL.contains(&(*record_type, desc.name))
                {
                    continue;
                }
                let mut record = any_record(record_type);
                let before = image(&*record);
                let ran = record
                    .special(desc.name, false, &Default::default())
                    .and_then(|()| record.special(desc.name, true, &Default::default()));
                record.on_put(desc.name);
                if ran.is_err() || image(&*record) != before {
                    hooked.push((*record_type, desc.name));
                }
            }
        }
        assert!(hooked.is_empty(), "not on FORCE_GENERAL: {hooked:?}");
    }

    /// `prec->lalm = val` into a CHAR cell is `PUT(epicsFloat64, char)`
    /// (`dbConvert.c:1631`), a signed cast: -5.5 is 0xfb.
    #[test]
    fn a_double_into_a_char_cell_is_signed_dbconvert_c_1631() {
        assert_eq!(
            CycleValue::Double(-5.5).convert_as(CycleValue::Char(0)),
            CycleValue::Char(0xfb)
        );
    }

    /// Each scalar request names the `dbrType` of the same name.
    #[test]
    fn a_scalar_link_request_names_its_dbr_type() {
        use crate::types::DbFieldType as T;
        for (read_as, dbr) in [
            (LinkReadAs::String, T::String),
            (LinkReadAs::Char, T::Char),
            (LinkReadAs::UChar, T::UChar),
            (LinkReadAs::Short, T::Short),
            (LinkReadAs::UShort, T::UShort),
            (LinkReadAs::Long, T::Long),
            (LinkReadAs::ULong, T::ULong),
            (LinkReadAs::Int64, T::Int64),
            (LinkReadAs::UInt64, T::UInt64),
            (LinkReadAs::Float, T::Float),
            (LinkReadAs::Double, T::Double),
            (LinkReadAs::Enum, T::Enum),
        ] {
            assert_eq!(read_as.dbr_type(), Some(dbr), "{read_as:?}");
        }
    }

    /// The generated `SPC_DBADDR` list answers the table walk: C's declared
    /// special (asyn's `ERRS` is resolved to `NoMod` but declared `SPC_DBADDR`,
    /// `asynRecord.dbd:622-624`), which the long-string fields keep although
    /// their `cvt_dbaddr` overwrites the resolved one. A type only another
    /// crate can create is pinned on its generated table alone.
    #[test]
    fn the_dbaddr_list_is_what_the_field_table_walk_answers() {
        use crate::server::record::dbd_generated::{
            RECORD_TYPES, record_dbaddr_fields, record_fields,
        };

        let mut seen_long_string = 0;
        let mut table_only = Vec::new();
        for record_type in RECORD_TYPES {
            let listed = record_dbaddr_fields(record_type).unwrap();
            let Ok(probe) = crate::server::db_loader::create_record(record_type) else {
                for desc in record_fields(record_type).unwrap() {
                    assert_eq!(
                        listed.contains(&desc.name),
                        desc.declared_special == Special::DbAddr,
                        "{record_type}.{}",
                        desc.name
                    );
                }
                table_only.push(*record_type);
                continue;
            };
            let walk = |field: &str| {
                probe.field_list().iter().any(|d| {
                    d.name.eq_ignore_ascii_case(field) && d.declared_special == Special::DbAddr
                })
            };
            for desc in probe.field_list() {
                if desc.long_string {
                    seen_long_string += 1;
                    assert!(walk(desc.name), "{record_type}.{} is SPC_DBADDR", desc.name);
                }
                assert_eq!(
                    listed.contains(&desc.name),
                    walk(desc.name),
                    "{record_type}.{}",
                    desc.name
                );
                assert_eq!(
                    probe.field_is_dbaddr(&desc.name.to_ascii_lowercase()),
                    walk(desc.name),
                    "{record_type}.{} asked in lower case",
                    desc.name
                );
            }
        }
        assert_eq!(
            seen_long_string, 5,
            "lsi/lso VAL+OVAL and printf VAL reached"
        );
        assert!(table_only.contains(&"asyn"), "asyn pinned on its table");
        assert_eq!(
            record_dbaddr_fields("asyn").unwrap(),
            ["BOUT", "BINP", "ERRS"]
        );
    }

    /// A type base's `.dbd` set does not cover answers `field_is_dbaddr` from
    /// its own generated list, not the field-table walk.
    #[test]
    fn a_downstream_dbaddr_list_answers_field_is_dbaddr() {
        struct Downstream;
        impl Record for Downstream {
            fn record_type(&self) -> &'static str {
                "downstream"
            }
            fn declared_dbaddr_fields(&self) -> Option<&'static [&'static str]> {
                Some(&["BUF"])
            }
            fn get_field(&self, _: &str) -> Option<EpicsValue> {
                None
            }
            fn put_field(&mut self, _: &str, _: EpicsValue) -> CaResult<()> {
                Ok(())
            }
        }
        assert!(Downstream.field_is_dbaddr("buf"));
        assert!(!Downstream.field_is_dbaddr("VAL"));
    }

    /// A type that reads its own link texts to answer
    /// [`Record::set_input_link_slots`] must give the answer the default body's
    /// name walk would, or the cycle fetches the wrong set of input links —
    /// silently, since an unwired slot and a slot the override forgot look the
    /// same to it. Pinned against the default body's own algorithm, for every
    /// declared type and for each slot wired alone as well as none and all.
    #[test]
    fn a_record_that_reads_its_own_link_slots_answers_what_the_name_walk_would() {
        use crate::server::record::dbd_generated::RECORD_TYPES;

        for record_type in RECORD_TYPES {
            let Ok(probe) = crate::server::db_loader::create_record(record_type) else {
                continue;
            };
            let links = probe.multi_input_links();
            if links.is_empty() || links.len() > u64::BITS as usize {
                continue;
            }
            let wirings = std::iter::once(Vec::new())
                .chain((0..links.len()).map(|i| vec![i]))
                .chain(std::iter::once((0..links.len()).collect::<Vec<_>>()));
            for wired in wirings {
                let Ok(mut record) = crate::server::db_loader::create_record(record_type) else {
                    continue;
                };
                for &slot in &wired {
                    let _ = record.put_field(links[slot].0, EpicsValue::String("SRC".into()));
                }
                let walked = {
                    let (mut set, mut unknown) = (0u64, 0u64);
                    for (slot, (link_field, _)) in links.iter().enumerate() {
                        match record.link_text_ref(link_field) {
                            Some([]) => {}
                            Some(_) => set |= 1 << slot,
                            None => unknown |= 1 << slot,
                        }
                    }
                    Some((set, unknown))
                };
                assert_eq!(
                    record.set_input_link_slots(),
                    walked,
                    "{record_type}: set_input_link_slots() disagrees with the name walk \
                     with slots {wired:?} wired"
                );
            }
        }
    }

    /// The cycle skips [`Record::select_input_links`] — and the record read
    /// lock it needs — whenever [`Record::narrows_input_links`] is false. A
    /// type that answers `false` must never narrow, or the cycle skips a real
    /// restriction to save the lock that would have found it.
    #[test]
    fn a_type_that_never_narrows_its_inputs_says_so() {
        use crate::server::record::dbd_generated::RECORD_TYPES;
        for record_type in RECORD_TYPES {
            let Ok(record) = crate::server::db_loader::create_record(record_type) else {
                continue;
            };
            if record.narrows_input_links() {
                continue;
            }
            for selector in std::iter::once(None).chain((0..=20u16).map(Some)) {
                assert!(
                    record.select_input_links(selector).is_none(),
                    "{record_type}: narrows_input_links() is false but \
                     select_input_links({selector:?}) narrowed the list"
                );
            }
        }
    }

    /// The cycle skips [`Record::after_output_decision`] — and the record write
    /// lock it needs — whenever [`Record::redecides_after_output`] is false
    /// (`ProcessPlan::redecides_after_output`). A type that grows an epilogue
    /// and forgets the predicate would be silently skipped, so every type is
    /// driven through both and the two are required to agree.
    #[test]
    fn a_type_that_redecides_after_output_says_so() {
        use crate::server::record::dbd_generated::RECORD_TYPES;

        for record_type in RECORD_TYPES {
            let (Ok(mut ran), Ok(untouched)) = (
                crate::server::db_loader::create_record(record_type),
                crate::server::db_loader::create_record(record_type),
            ) else {
                continue;
            };
            // C `longoutRecord.c:492` latches VAL into PVAL, so a record left at
            // its initial VAL would hide the epilogue behind `pval == val`.
            let _ = ran.put_field("VAL", EpicsValue::Long(7));
            let before = field_dump(&*ran);
            ran.after_output_decision();
            let changed = field_dump(&*ran) != before;

            let diff: Vec<_> = field_dump(&*ran)
                .into_iter()
                .zip(before.iter())
                .filter(|(a, b)| a.1 != b.1)
                .map(|(a, b)| (a.0, b.1.clone(), a.1.clone()))
                .collect();
            assert_eq!(
                changed,
                untouched.redecides_after_output(),
                "{record_type}: after_output_decision changed {diff:?}, but \
                 redecides_after_output said {}",
                untouched.redecides_after_output()
            );
        }
    }

    /// `check_deadband_ext` skips [`Record::array_monitor_post`] for a type
    /// whose [`Record::has_array_monitor_post`] is false, so that type must
    /// keep the `None` default; and every deadband type without the hook says
    /// false. A type off the deadband path never asks, so its answer is free.
    #[test]
    fn a_type_with_an_array_monitor_post_says_so() {
        use crate::server::record::dbd_generated::RECORD_TYPES;
        const ARRAYS: &[&str] = &["aai", "aao", "histogram", "waveform"];
        let mut reached = Vec::new();
        let mut wrong = Vec::new();
        for record_type in RECORD_TYPES {
            let Some(mut r) = cycle_record(record_type) else {
                continue;
            };
            reached.push(*record_type);
            if r.uses_monitor_deadband()
                && r.has_array_monitor_post() != ARRAYS.contains(record_type)
            {
                wrong.push(format!("{record_type} has_array_monitor_post"));
            }
            if !r.has_array_monitor_post() && r.array_monitor_post().is_some() {
                wrong.push(format!("{record_type} answers but says false"));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
        for record_type in ARRAYS {
            assert!(reached.contains(record_type), "{record_type} not reached");
        }
    }

    /// The cycle skips [`Record::pre_process_actions`] for a type whose
    /// [`Record::acts_before_process`] is false, so that type must keep the
    /// empty default; and every base type without the hook says false.
    #[test]
    fn a_type_that_acts_before_process_says_so() {
        use crate::server::record::dbd_generated::RECORD_TYPES;
        const ACTS: &[&str] = &["scalcout", "sseq"];
        let mut reached = Vec::new();
        for record_type in RECORD_TYPES {
            let Some(mut r) = cycle_record(record_type) else {
                continue;
            };
            reached.push(*record_type);
            assert_eq!(
                r.acts_before_process(),
                ACTS.contains(record_type),
                "{record_type} acts_before_process"
            );
            if !r.acts_before_process() {
                assert!(
                    r.pre_process_actions().is_empty(),
                    "{record_type}: pre_process_actions acted but acts_before_process is false"
                );
            }
        }
        for record_type in ACTS {
            assert!(reached.contains(record_type), "{record_type} not reached");
        }
    }

    /// A hook the plan will not ask about must be the empty default on a fresh
    /// record, and a base type outside the listed ones claims no hook at all.
    #[test]
    fn a_type_that_overrides_a_cycle_hook_says_so() {
        use crate::server::record::dbd_generated::RECORD_TYPES;
        let expected = |t: &str| match t {
            "waveform" | "aai" | "aao" | "subArray" => {
                CycleHooks::PRE_INPUT.union(CycleHooks::INPUT_FETCH)
            }
            "compress" => CycleHooks::PRE_INPUT.union(CycleHooks::WRITE_POSTED),
            "acalcout" => CycleHooks::CYCLE_POSTED
                .union(CycleHooks::WRITE_POSTED)
                .union(CycleHooks::PROCESS_POSTED)
                .union(CycleHooks::PUT_REPORT)
                .union(CycleHooks::INPUT_FETCH)
                .union(CycleHooks::CONTINUATION)
                .union(CycleHooks::OUTPUT_HOOKS)
                .union(CycleHooks::LINK_CHECK),
            "mbbiDirect" | "mbboDirect" | "seq" => CycleHooks::CYCLE_POSTED,
            "calcout" => CycleHooks::WRITE_POSTED
                .union(CycleHooks::PUT_REPORT)
                .union(CycleHooks::PROCESS_POSTED)
                .union(CycleHooks::INPUT_FETCH)
                .union(CycleHooks::CONTINUATION)
                .union(CycleHooks::MONITOR_SETS)
                .union(CycleHooks::OUTPUT_HOOKS)
                .union(CycleHooks::LINK_CHECK),
            "swait" => CycleHooks::WRITE_POSTED
                .union(CycleHooks::PUT_REPORT)
                .union(CycleHooks::INPUT_FETCH)
                .union(CycleHooks::OUTPUT_HOOKS),
            "mbbi" | "mbbo" | "lso" | "lsi" => CycleHooks::WRITE_POSTED,
            "transform" => CycleHooks::CYCLE_POSTED
                .union(CycleHooks::WRITE_POSTED)
                .union(CycleHooks::CONTEXT)
                .union(CycleHooks::PUT_REPORT)
                .union(CycleHooks::INPUT_FETCH)
                .union(CycleHooks::LINK_CHECK),
            "printf" => CycleHooks::INPUT_FETCH
                .union(CycleHooks::CONTINUATION)
                .union(CycleHooks::STAMP_REQUEST),
            "calc" | "sel" => CycleHooks::INPUT_FETCH,
            "aSub" => CycleHooks::MONITOR_SETS,
            "longout" => CycleHooks::OUTPUT_HOOKS,
            "sseq" => CycleHooks::ALL,
            "scalcout" => CycleHooks::ALL
                .union(CycleHooks::PUT_REPORT)
                .union(CycleHooks::LINK_CHECK),
            _ => CycleHooks::NONE,
        };
        for record_type in RECORD_TYPES {
            let Some(mut r) = cycle_record(record_type) else {
                continue;
            };
            let hooks = r.cycle_hooks();
            assert_eq!(hooks, expected(record_type), "{record_type} cycle_hooks");
            if !hooks.has(CycleHooks::PRE_INPUT) {
                assert!(
                    r.pre_input_link_actions(true, &mut Default::default())
                        .is_empty(),
                    "{record_type}"
                );
            }
            if !hooks.has(
                CycleHooks::CYCLE_POSTED
                    .union(CycleHooks::WRITE_POSTED)
                    .union(CycleHooks::HAND_POSTED),
            ) {
                assert!(r.take_cycle_posted_fields().is_empty(), "{record_type}");
            }
            if !hooks.has(CycleHooks::PROCESS_POSTED) {
                assert!(r.take_process_posted_fields().is_empty(), "{record_type}");
            }
            if !hooks.has(CycleHooks::MONITOR_SETS) {
                assert!(r.prev_compared_inputs().0.is_empty(), "{record_type}");
                assert!(r.force_posted_fields().is_empty(), "{record_type}");
                assert!(r.log_swept_fields().is_empty(), "{record_type}");
            }
            if !hooks.has(CycleHooks::OUTPUT_HOOKS) {
                assert!(r.should_output(), "{record_type}");
                assert!(r.output_event().is_none(), "{record_type}");
            }
            assert_eq!(
                hooks.has(CycleHooks::LINK_CHECK),
                r.link_check().is_some_and(|check| check.spec.in_cycle),
                "{record_type}"
            );
        }
    }

    /// A field only the event path posts (waveform HASH from
    /// `array_monitor_post`) is skipped by the walk, so a declared set that
    /// names it costs a per-cycle entry and posts nothing.
    #[test]
    fn no_declared_set_names_an_event_posted_field() {
        let mut sets = 0usize;
        for record_type in super::super::dbd_generated::RECORD_TYPES {
            let r = any_record(record_type);
            let Some(set) = r.process_posted_fields() else {
                continue;
            };
            sets += 1;
            for &ix in set {
                let name = field_at(r.field_list(), ix).expect("declared").name;
                assert!(
                    !r.event_posted_fields().contains(&name),
                    "{record_type}.{name}"
                );
            }
        }
        assert!(sets > 4, "{sets} declared sets");
    }

    /// Every field the type declares, rendered — `sel` initialises A..L to
    /// `NaN`, which is never equal to itself, so the comparison is over the
    /// `Debug` form rather than the values.
    fn field_dump(r: &dyn Record) -> Vec<(&'static str, String)> {
        r.field_list()
            .iter()
            .map(|d| (d.name, format!("{:?}", r.get_field(d.name))))
            .collect()
    }

    /// A deadband cell the `.dbd` declares but the type does not serve still
    /// loads from `field(MDEL,"5")`, and the deadband then runs at 0: `sel`
    /// did that while `selRecord.c:316,319` deadbands both.
    #[test]
    fn every_declared_deadband_cell_is_served() {
        use crate::server::record::dbd_generated::RECORD_TYPES;

        for record_type in RECORD_TYPES {
            let Some(r) = cycle_record(record_type) else {
                continue;
            };
            for field in ["MDEL", "ADEL", "MLST", "ALST"] {
                assert_eq!(
                    r.field_list().iter().any(|d| d.name == field),
                    r.get_field(field).is_some(),
                    "{record_type}.{field}: declared and served must agree"
                );
            }
        }
    }

    /// Every one-question cycle accessor must answer exactly what the
    /// `get_field` path it replaces would answer, for every record type in
    /// the database — the overrides exist only to skip a name match, never to
    /// change what is read.
    ///
    /// The cells are seeded with DISTINCT values first, through the generic
    /// put path and in the type the cell is declared with, so an override
    /// that returns the neighbouring cell cannot pass on a default-valued
    /// record where every cell reads zero.
    #[test]
    fn every_record_answers_the_cycle_cells_as_get_field_does() {
        use crate::server::record::dbd_generated::RECORD_TYPES;

        let f64_of = |r: &dyn Record, field: &str| r.get_field(field).and_then(|v| v.to_f64());

        for record_type in RECORD_TYPES {
            let Some(mut r) = cycle_record(record_type) else {
                continue;
            };
            seed_cycle_cells(&mut *r);

            let r = &*r;
            // Distinct from every seeded cell, so a type that answers from
            // `last_posted` for a cell it carries is caught.
            let last_posted = [-71.5, -72.5];
            let cell = |field, absent| {
                r.get_field(field)
                    .as_ref()
                    .and_then(CycleValue::from_epics)
                    .unwrap_or(CycleValue::Double(absent))
            };
            let raw = [
                cell("MDEL", 0.0),
                cell("ADEL", 0.0),
                cell("MLST", last_posted[0]),
                cell("ALST", last_posted[1]),
            ];
            let want = r
                .val()
                .as_ref()
                .and_then(AlarmLimit::from_stored)
                .map(|val| match val {
                    AlarmLimit::Double(val) => {
                        let [mdel, adel, mlst, alst] = raw.map(CycleValue::to_f64);
                        MonitorDeadbandCells::Double(DeadbandCells {
                            val,
                            mdel,
                            adel,
                            mlst,
                            alst,
                        })
                    }
                    AlarmLimit::Long(val) => {
                        let [mdel, adel, mlst, alst] = raw.map(CycleValue::to_i32);
                        MonitorDeadbandCells::Long(DeadbandCells {
                            val,
                            mdel,
                            adel,
                            mlst,
                            alst,
                        })
                    }
                    AlarmLimit::Int64(val) => {
                        let [mdel, adel, mlst, alst] = raw.map(CycleValue::to_i64);
                        MonitorDeadbandCells::Int64(DeadbandCells {
                            val,
                            mdel,
                            adel,
                            mlst,
                            alst,
                        })
                    }
                });
            assert_eq!(
                r.monitor_deadband_cells(last_posted),
                want,
                "{record_type} deadband cells"
            );

            let ladder = r
                .val()
                .and_then(|v| AlarmLimit::from_ladder_value(&v))
                .map(|val| AnalogAlarmInput {
                    val,
                    hyst: r
                        .get_field("HYST")
                        .and_then(|v| AlarmLimit::from_stored(&v)),
                    lalm: r
                        .get_field("LALM")
                        .and_then(|v| AlarmLimit::from_stored(&v))
                        .unwrap_or(val),
                    aftc: f64_of(r, "AFTC").unwrap_or(0.0),
                    afvl: f64_of(r, "AFVL").unwrap_or(0.0),
                });
            assert_eq!(
                r.analog_alarm_input(),
                ladder,
                "{record_type} analog_alarm_input"
            );
        }
    }

    /// The write half, pinned against the path it replaces rather than
    /// against a restatement of it: two records of the same type, one driven
    /// through the store accessor and one through the `get_field` type probe
    /// and `put_field` the accessor stands in for. Their cells must end up
    /// identical, including where the put is refused — which is how the port
    /// behaved before the accessor existed.
    #[test]
    fn every_record_stores_the_cycle_cells_as_put_field_does() {
        use crate::server::record::dbd_generated::RECORD_TYPES;

        for record_type in RECORD_TYPES {
            let (Some(mut fast), Some(mut slow)) =
                (cycle_record(record_type), cycle_record(record_type))
            else {
                continue;
            };
            seed_cycle_cells(&mut *fast);
            seed_cycle_cells(&mut *slow);

            fast.store_monitor_last_posted(AlarmLimit::Double(23.0), true, true);
            for field in ["ALST", "MLST"] {
                let target = slow
                    .get_field(field)
                    .map(|v| v.db_field_type())
                    .unwrap_or(crate::types::DbFieldType::Double);
                let _ = slow.put_field(field, EpicsValue::Double(23.0).convert_to(target));
            }

            fast.store_alarm_filter_value(29.0);
            let _ = slow.put_field("AFVL", EpicsValue::Double(29.0));

            fast.store_analog_lalm(AlarmLimit::Double(31.0));
            let target = slow
                .get_field("LALM")
                .map(|v| v.db_field_type())
                .unwrap_or(crate::types::DbFieldType::Double);
            let _ = slow.put_field("LALM", EpicsValue::Double(31.0).convert_to(target));

            for field in ["MLST", "ALST", "AFVL", "LALM"] {
                assert_eq!(
                    fast.get_field(field),
                    slow.get_field(field),
                    "{record_type} {field}"
                );
            }
        }
    }

    /// [`Record::cycle_cell`] is the one read every cycle accessor goes
    /// through, so it is pinned per cell against the `get_field` it stands in
    /// for, and [`Record::put_cycle_cell`] against `put_field` — two records
    /// of each type, every cell, a value in the cell's own carrier and one in
    /// a foreign carrier, which an override must hand to `put_field`.
    #[test]
    fn every_record_answers_each_cycle_cell_as_its_field_does() {
        use crate::server::record::dbd_generated::RECORD_TYPES;
        use crate::server::record::{CycleCell, CycleRead, CycleValue};

        let mut answered = Vec::new();
        for record_type in RECORD_TYPES {
            let (Some(mut fast), Some(mut slow)) =
                (cycle_record(record_type), cycle_record(record_type))
            else {
                continue;
            };
            answered.push(*record_type);
            seed_cycle_cells(&mut *fast);
            seed_cycle_cells(&mut *slow);

            for (i, cell) in CycleCell::ALL.into_iter().enumerate() {
                let field = cell.name();
                let by_name = fast.get_field(field);
                let numeric = by_name.as_ref().and_then(CycleValue::from_epics);
                match fast.cycle_cell(cell) {
                    CycleRead::Value(v) => {
                        assert_eq!(Some(v.into()), by_name, "{record_type} cycle_cell({field})");
                        if cell == CycleCell::Val && fast.primary_field() == "VAL" {
                            assert_eq!(fast.val(), by_name, "{record_type} val");
                        }
                    }
                    CycleRead::Absent => {
                        assert_eq!(by_name, None, "{record_type} cycle_cell({field})")
                    }
                    // Only the trait default answers a numeric VAL/OVAL by
                    // name; a type that overrides the hook answers it typed.
                    // A number is answered typed; only the trait default
                    // leaves VAL/OVAL to `get_field`.
                    CycleRead::ByName => assert!(
                        numeric.is_none()
                            || matches!(cell, CycleCell::Val | CycleCell::Oval)
                                && !TYPED_CYCLE_CELLS.contains(record_type),
                        "{record_type} cycle_cell({field}) by name, get_field {by_name:?}"
                    ),
                }
                if let Some(v) = &by_name {
                    assert!(
                        numeric.is_some() || v.is_array() || matches!(v, EpicsValue::String(_)),
                        "{record_type} {field}: no CycleValue carrier for {v:?}"
                    );
                }

                let own = CycleValue::Double(51.0 + i as f64)
                    .convert_as(numeric.unwrap_or(CycleValue::Double(0.0)));
                let foreign = match own {
                    CycleValue::Double(_) => CycleValue::Long(7),
                    _ => CycleValue::Double(7.5),
                };
                for value in [own, foreign] {
                    let stored = fast.put_cycle_cell(cell, value).is_ok();
                    let put = slow.put_field(field, value.into()).is_ok();
                    assert_eq!(stored, put, "{record_type} put_cycle_cell({field}) result");
                    assert_eq!(
                        fast.get_field(field),
                        slow.get_field(field),
                        "{record_type} put_cycle_cell({field})"
                    );
                }
            }
        }
        // Every type that overrides the pair must be one the loop reached.
        for record_type in TYPED_CYCLE_CELLS {
            assert!(answered.contains(record_type), "{record_type} not reached");
        }
    }

    /// The types that answer [`Record::cycle_cell`] from their members.
    const TYPED_CYCLE_CELLS: &[&str] = &[
        "acalcout",
        "ai",
        "ao",
        "bi",
        "bo",
        "busy",
        "calc",
        "calcout",
        "dfanout",
        "event",
        "int64in",
        "int64out",
        "longin",
        "longout",
        "mbbi",
        "mbbiDirect",
        "mbbo",
        "mbboDirect",
        "permissive",
        "scalcout",
        "sel",
        "seq",
        "fanout",
        "sseq",
        "state",
        "stringin",
        "stringout",
        "sub",
        "swait",
    ];

    /// `Record::val` carries SEMANTICS that several types legitimately
    /// override, so it cannot be pinned database-wide. The types whose
    /// override exists only to skip the name match for a string VAL, which
    /// [`Record::cycle_cell`] answers [`CycleRead::ByName`], are listed here,
    /// and each must still answer what the default body would.
    #[test]
    fn a_speed_override_of_val_answers_what_the_default_would() {
        const SPEED_OVERRIDES: &[&str] = &["event", "state", "stringin", "stringout"];

        for record_type in SPEED_OVERRIDES {
            let mut r = crate::server::db_loader::create_record(record_type)
                .expect("a listed type is creatable");
            let _ = r.put_field("VAL", EpicsValue::String("41".into()));

            let r = &*r;
            assert_eq!(r.val(), r.get_field(r.primary_field()), "{record_type} val");
            assert_eq!(
                r.monitor_deadband_value(),
                r.val().as_ref().and_then(AlarmLimit::from_stored),
                "{record_type} monitor_deadband_value"
            );
            assert!(!r.value_is_undefined(), "{record_type} value_is_undefined");
        }
    }

    /// A type's direct multi-input store lands what the by-name default does,
    /// in every value field it declares.
    #[test]
    fn a_multi_input_store_is_what_put_field_internal_stores() {
        use crate::server::record::dbd_generated::RECORD_TYPES;
        // `swait` is module-owned, so base's registry does not build it.
        let make = |record_type: &str| -> Option<Box<dyn Record>> {
            match record_type {
                "swait" => Some(Box::new(
                    crate::server::records::swait::SwaitRecord::default(),
                )),
                _ => crate::server::db_loader::create_record(record_type).ok(),
            }
        };
        let mut reached = Vec::new();
        for record_type in RECORD_TYPES {
            let Some(probe) = make(record_type) else {
                continue;
            };
            for &(_, field) in probe.multi_input_links() {
                reached.push(*record_type);
                for value in [1.5, -0.0, f64::NAN, 1e300] {
                    let mut direct = make(record_type).unwrap();
                    let mut by_name = make(record_type).unwrap();
                    let direct_ok = direct.put_multi_input_f64(field, value).is_ok();
                    let by_name_ok = by_name
                        .put_field_internal(field, EpicsValue::Double(value))
                        .is_ok();
                    assert_eq!(direct_ok, by_name_ok, "{record_type}.{field}");
                    assert_eq!(
                        format!("{:?}", direct.get_field(field)),
                        format!("{:?}", by_name.get_field(field)),
                        "{record_type}.{field} <- {value}"
                    );
                }
            }
        }
        for record_type in ["sel", "calcout", "swait"] {
            assert!(reached.contains(&record_type), "{record_type} not reached");
        }
    }

    /// A borrowed value, copied out.
    fn owned(view: ValueView<'_>) -> EpicsValue {
        match view {
            ValueView::Scalar(v) => v.into(),
            ValueView::Double(a) => EpicsValue::DoubleArray(a.to_vec().into()),
            ValueView::Float(a) => EpicsValue::FloatArray(a.to_vec().into()),
            ValueView::Long(a) => EpicsValue::LongArray(a.to_vec().into()),
            ValueView::ULong(a) => EpicsValue::ULongArray(a.to_vec().into()),
            ValueView::Int64(a) => EpicsValue::Int64Array(a.to_vec().into()),
            ValueView::UInt64(a) => EpicsValue::UInt64Array(a.to_vec().into()),
            ValueView::Short(a) => EpicsValue::ShortArray(a.to_vec().into()),
            ValueView::UShort(a) => EpicsValue::UShortArray(a.to_vec().into()),
            ValueView::Char(a) => EpicsValue::CharArray(a.to_vec().into()),
            ValueView::UChar(a) => EpicsValue::UCharArray(a.to_vec().into()),
            ValueView::Enum(a) => EpicsValue::EnumArray(a.to_vec().into()),
        }
    }

    /// `view`, materialised, is `value`, its `DBR_*` included.
    fn view_is(view: OutView<'_>, value: &EpicsValue) -> bool {
        match view {
            OutView::Num(n) => {
                CycleValue::from_epics(value).is_some_and(|v| format!("{v:?}") == format!("{n:?}"))
            }
            OutView::Str(b) => matches!(value, EpicsValue::String(v) if v.as_bytes() == b),
            OutView::Chars(b) => matches!(value, EpicsValue::CharArray(v) if v[..] == *b),
            OutView::Array(a) => owned(a) == *value,
        }
    }

    /// The states the sweep below reads each type's output in: default, then
    /// VAL, then OVAL stored (numeric and string forms, whichever it takes),
    /// then the type's own boundaries.
    fn output_states(record_type: &str) -> Vec<Box<dyn Record>> {
        let common = [
            ("VAL", EpicsValue::Double(-3.25)),
            ("VAL", EpicsValue::String("41".into())),
            ("OVAL", EpicsValue::Double(6.5)),
            ("OVAL", EpicsValue::String("17".into())),
        ];
        let runs: Vec<&[(&str, EpicsValue)]> = (0..=common.len()).map(|n| &common[..n]).collect();
        let lso_full = EpicsValue::CharArray([&[b'y'; 40][..], &[0]].concat().into());
        let own: &[&[(&str, EpicsValue)]] = match record_type {
            "swait" => &[
                &[
                    ("DOPT", EpicsValue::Short(0)),
                    ("DOLD", EpicsValue::Double(9.5)),
                ],
                &[
                    ("DOPT", EpicsValue::Short(1)),
                    ("DOLD", EpicsValue::Double(9.5)),
                ],
            ],
            "ao" => &[
                &[("RVAL", EpicsValue::Long(i32::MIN))],
                &[("RVAL", EpicsValue::Long(-1))],
            ],
            "stringout" => &[
                &[("VAL", EpicsValue::String("z".repeat(39).into()))],
                &[("VAL", EpicsValue::String("".into()))],
            ],
            // LEN 0 is the default state; a full SIZV (41) is 40 bytes and the NUL.
            "lso" => &[&[("VAL", lso_full)]],
            // FTVL DOUBLE (the default STRING has no view): NORD = NELM, then 0.
            "aao" => &[
                &[
                    ("FTVL", EpicsValue::Short(10)),
                    ("NELM", EpicsValue::ULong(2)),
                    ("VAL", EpicsValue::DoubleArray(vec![1.5, 2.5].into())),
                ],
                &[
                    ("FTVL", EpicsValue::Short(10)),
                    ("NELM", EpicsValue::ULong(2)),
                    ("VAL", EpicsValue::DoubleArray(vec![1.5, 2.5].into())),
                    ("VAL", EpicsValue::DoubleArray(Vec::new().into())),
                ],
            ],
            "mbbo" => &[
                &[("RVAL", EpicsValue::ULong(u32::MAX))],
                &[
                    ("RVAL", EpicsValue::ULong(u32::MAX)),
                    ("MASK", EpicsValue::ULong(0x8000_0ff0)),
                ],
            ],
            "mbboDirect" => &[
                &[("RVAL", EpicsValue::ULong(u32::MAX))],
                &[
                    ("RVAL", EpicsValue::ULong(u32::MAX)),
                    ("MASK", EpicsValue::ULong(0x8000_0ff0)),
                ],
            ],
            "bo" => &[
                &[("RVAL", EpicsValue::ULong(u32::MAX))],
                &[("RVAL", EpicsValue::ULong(0x8000_0001))],
            ],
            _ => &[],
        };
        let mut states: Vec<Box<dyn Record>> = runs
            .iter()
            .map(|run| {
                let mut r = any_record(record_type);
                for (field, value) in *run {
                    let _ = r.put_field(field, value.clone());
                }
                r
            })
            .collect();
        // A type's own boundary must hold, or its case tests nothing.
        for run in own {
            let mut r = any_record(record_type);
            for (field, value) in *run {
                r.put_field(field, value.clone()).unwrap();
                assert_eq!(
                    r.get_field(field).as_ref(),
                    Some(value),
                    "{record_type}.{field}"
                );
            }
            states.push(r);
        }
        states
    }

    /// A `Some` output view is the value [`Record::output_link_value`] (and
    /// [`Record::raw_soft_output_value`]) materialises, for every type and
    /// state; `None` falls back to that value, so it is never wrong.
    #[test]
    fn output_view_matches_output_link_value() {
        use crate::server::record::dbd_generated::RECORD_TYPES;
        let mut viewed = 0usize;
        let mut wrong = Vec::new();
        for record_type in RECORD_TYPES {
            for (i, r) in output_states(record_type).iter().enumerate() {
                for (view, value, what) in [
                    (r.output_view(), r.output_link_value(), "output"),
                    (r.raw_soft_output_view(), r.raw_soft_output_value(), "raw"),
                ] {
                    let Some(view) = view else { continue };
                    if !value.as_ref().is_some_and(|v| view_is(view, v)) {
                        wrong.push(format!(
                            "{record_type} state {i} {what}: {view:?} vs {value:?}"
                        ));
                    }
                    viewed += 1;
                }
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
        assert!(viewed > 0, "no type lends its output");
    }

    /// A lent value is the one [`Record::val`] would copy, and the queries that
    /// read it answer as they would from the copy.
    #[test]
    fn a_lent_value_is_what_val_would_copy() {
        use crate::server::record::dbd_generated::RECORD_TYPES;
        // The last step lowers SIZV under a VAL already stored, so the lent
        // value has to apply the cap `val` applies.
        let steps = [
            ("VAL", EpicsValue::String("41".into())),
            ("VAL", EpicsValue::CharArray(vec![b'x'; 100].into())),
            ("SIZV", EpicsValue::UShort(16)),
        ];
        let mut lent = Vec::new();
        for record_type in RECORD_TYPES {
            let Ok(mut r) = crate::server::db_loader::create_record(record_type) else {
                continue;
            };
            for (field, value) in &steps {
                let _ = r.put_field(field, value.clone());
                let r = &*r;
                let Some(view) = r.monitor_value_view() else {
                    continue;
                };
                lent.push(*record_type);
                let val = r.val();
                assert_eq!(Some(owned(view)), val, "{record_type} after {field}");
                let undefined = match &val {
                    Some(EpicsValue::Double(v)) => v.is_nan(),
                    Some(EpicsValue::Float(v)) => v.is_nan(),
                    Some(_) => false,
                    None => true,
                };
                assert_eq!(r.value_is_undefined(), undefined, "{record_type}");
            }
        }
        assert!(lent.contains(&"lsi"), "lsi lends VAL");
    }

    /// [`CycleValue`] stands in for the scalar `EpicsValue` arms, so each of
    /// its answers is pinned against the `EpicsValue` one at the boundaries
    /// those arms branch on: sign, NaN, range, the signed `Char` byte and
    /// 64-bit values past a double's mantissa.
    #[test]
    fn a_cycle_value_converts_as_its_epics_value_does() {
        use crate::server::record::CycleValue as V;
        let values = [
            V::Double(-1.5),
            V::Double(f64::NAN),
            V::Double(1e300),
            V::Double(65536.7),
            V::Float(-2.5),
            V::Float(f32::NAN),
            V::Long(-1),
            V::Long(i32::MAX),
            V::ULong(u32::MAX),
            V::Int64(i64::MIN),
            V::Int64((1 << 53) + 1),
            V::UInt64(u64::MAX),
            V::Short(-300),
            V::UShort(u16::MAX),
            V::Char(0xFF),
            V::UChar(0xFF),
            V::Enum(7),
        ];
        let show = |v: EpicsValue| format!("{v:?}");
        for source in values {
            let wide = EpicsValue::from(source);
            assert_eq!(
                format!("{:?}", Some(source)),
                format!("{:?}", V::from_epics(&wide))
            );
            assert_eq!(
                format!("{:?}", Some(source.to_f64())),
                format!("{:?}", wide.to_f64()),
                "{source:?} to_f64"
            );
            assert_eq!(
                format!("{:?}", source.ladder()),
                format!("{:?}", AlarmLimit::from_ladder_value(&wide)),
                "{source:?} ladder"
            );
            assert_eq!(
                format!("{:?}", Some(source.alarm_limit())),
                format!("{:?}", AlarmLimit::from_stored(&wide)),
                "{source:?} alarm_limit"
            );
            let n = Scalar::from(source);
            assert_eq!(
                format!(
                    "{:?}",
                    (
                        source.to_f32(),
                        source.to_i8(),
                        source.to_u8(),
                        source.to_i16(),
                        source.to_u16(),
                        source.to_i32(),
                        source.to_u32(),
                        source.to_i64(),
                        source.to_u64()
                    )
                ),
                format!(
                    "{:?}",
                    (
                        n.to_f32(),
                        n.to_i8(),
                        n.to_u8(),
                        n.to_i16(),
                        n.to_u16(),
                        n.to_i32(),
                        n.to_u32(),
                        n.to_i64(),
                        n.to_u64()
                    )
                ),
                "{source:?} lean store"
            );
            for carrier in values {
                let target = EpicsValue::from(carrier).db_field_type();
                assert_eq!(
                    show(source.convert_as(carrier).into()),
                    show(wide.convert_to(target)),
                    "{source:?} as {target:?}"
                );
            }
        }
    }

    /// A record of `record_type` with its `.dbd` initials applied, including
    /// the module-owned types (calc, busy, asyn) base vendors but does not
    /// register, so the cycle-cell pins reach every type that overrides them.
    fn cycle_record(record_type: &str) -> Option<Box<dyn Record>> {
        use crate::server::records::*;
        let factory: fn() -> Box<dyn Record> = match record_type {
            "acalcout" => || Box::new(acalcout::AcalcoutRecord::default()),
            "asyn" => || Box::new(asyn_record::AsynRecord::default()),
            "busy" => || Box::new(busy::BusyRecord::default()),
            "scalcout" => || Box::new(scalcout::ScalcoutRecord::default()),
            "sseq" => || Box::new(sseq::SseqRecord::default()),
            "swait" => || Box::new(swait::SwaitRecord::default()),
            "transform" => || Box::new(transform::TransformRecord::default()),
            _ => return crate::server::db_loader::create_record(record_type).ok(),
        };
        let factories = std::collections::HashMap::from([(
            record_type.to_string(),
            Box::new(factory) as crate::server::db_loader::RecordFactory,
        )]);
        crate::server::db_loader::create_record_with_factories(record_type, &factories).ok()
    }

    /// Distinct values in every cell the cycle accessors read, each in the
    /// type its record declares it with.
    fn seed_cycle_cells(r: &mut dyn Record) {
        let cells = [
            "MDEL", "ADEL", "MLST", "ALST", "HYST", "LALM", "AFTC", "AFVL", "OVAL",
        ];
        for (i, field) in cells.iter().enumerate() {
            let Some(target) = r.get_field(field).map(|v| v.db_field_type()) else {
                continue;
            };
            let _ = r.put_field(
                field,
                EpicsValue::Double(11.0 + i as f64).convert_to(target),
            );
        }
    }

    /// Internal delivery is not a `dbPut`, and the two entries have to stay
    /// two. A `compress` INP reading a `DBF_LONG` source delivers ONE sample
    /// as an `EpicsValue::Long`; C's link layer converts it to the target's
    /// type and the record folds it into `compress_scalar`'s running `cvb`.
    /// Rendering it as a one-element buffer sends it to `push_array` instead,
    /// whose array algorithm writes the N clamp back into the record
    /// (`compressRecord.c:171-172`) — a field the scalar algorithm never
    /// touches.
    #[test]
    fn a_one_sample_link_delivery_is_not_a_one_element_buffer() {
        let mut rec = CompressRecord::new(4, 0);
        rec.put_field("N", EpicsValue::Long(0)).unwrap();
        assert_eq!(rec.get_field("N"), Some(EpicsValue::ULong(0)));

        // The source's DBF differs from VAL's, so this delivery reaches the
        // converter rather than being handed over unchanged.
        put_field_internal_default(&mut rec, "VAL", EpicsValue::Long(5)).unwrap();

        assert_eq!(
            rec.get_field("N"),
            Some(EpicsValue::ULong(0)),
            "compressRecord.c:273-304 never touches prec->n"
        );
    }
}

/// The three cells C `checkAlarms` compares — see
/// [`Record::analog_alarm_input`], which is the only producer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnalogAlarmInput {
    /// VAL, in the variant that picks the ladder's arm.
    pub val: AlarmLimit,
    /// HYST, or `None` for a type that keeps it in `CommonFields`.
    pub hyst: Option<AlarmLimit>,
    /// LALM, the armed latch; VAL for a type with no cell for it.
    pub lalm: AlarmLimit,
    /// AFTC, the alarm filter time constant; 0 for a type with no filter.
    pub aftc: f64,
    /// AFVL, the alarm filter accumulator; 0 for a type with no filter.
    pub afvl: f64,
}

/// The cells the scan cycle reads or writes on every pass — see
/// [`Record::cycle_cell`], the one accessor every cycle read goes through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CycleCell {
    Val,
    Oval,
    Mdel,
    Adel,
    Mlst,
    Alst,
    Hyst,
    Lalm,
    Aftc,
    Afvl,
}

impl CycleCell {
    pub const ALL: [CycleCell; 10] = [
        Self::Val,
        Self::Oval,
        Self::Mdel,
        Self::Adel,
        Self::Mlst,
        Self::Alst,
        Self::Hyst,
        Self::Lalm,
        Self::Aftc,
        Self::Afvl,
    ];

    /// The field name the cell answers for.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Val => "VAL",
            Self::Oval => "OVAL",
            Self::Mdel => "MDEL",
            Self::Adel => "ADEL",
            Self::Mlst => "MLST",
            Self::Alst => "ALST",
            Self::Hyst => "HYST",
            Self::Lalm => "LALM",
            Self::Aftc => "AFTC",
            Self::Afvl => "AFVL",
        }
    }
}

/// VAL through [`Record::cycle_cell`] where VAL is the primary field, and
/// [`CycleRead::ByName`] — ask [`Record::val`] — where it is not.
fn val_cell<R: Record + ?Sized>(r: &R) -> CycleRead {
    match r.primary_field() {
        "VAL" => r.cycle_cell(CycleCell::Val),
        _ => CycleRead::ByName,
    }
}

/// C's `prec->cell = value`, in the carrier the cell is stored in.
#[inline(always)]
fn store_cycle_cell<R: Record + ?Sized>(r: &mut R, cell: CycleCell, value: CycleValue) {
    let _ = match r.cycle_cell(cell) {
        CycleRead::Value(held) => r.put_cycle_cell(cell, value.convert_as(held)),
        CycleRead::Absent => r.put_cycle_cell(cell, CycleValue::Double(value.to_f64())),
        CycleRead::ByName => {
            let target = r
                .get_field(cell.name())
                .map(|v| v.db_field_type())
                .unwrap_or(crate::types::DbFieldType::Double);
            r.put_field(cell.name(), EpicsValue::from(value).convert_to(target))
        }
    };
}

/// How a record answers [`Record::cycle_cell`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CycleRead {
    /// The cell, held in this numeric carrier.
    Value(CycleValue),
    /// The type has no field by that name.
    Absent,
    /// The cell is a string or array (VAL/OVAL), or the type keeps the trait
    /// default, which does not read VAL/OVAL itself: ask `get_field`.
    ByName,
}

impl CycleRead {
    /// The answer `get_field` gives, classified.
    pub fn of_field(field: Option<EpicsValue>) -> Self {
        match field {
            None => Self::Absent,
            Some(v) => CycleValue::from_epics(&v).map_or(Self::ByName, Self::Value),
        }
    }
}

/// A numeric cycle cell in the carrier its record stores it in — every
/// scalar DBF number, so [`CycleRead::ByName`] is left for a string or array
/// cell and the trait default. `Copy` with no drop, so a read through
/// [`Record::cycle_cell`] costs a member load.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CycleValue {
    Double(f64),
    Float(f32),
    Long(i32),
    ULong(u32),
    Int64(i64),
    UInt64(u64),
    Short(i16),
    UShort(u16),
    Char(u8),
    UChar(u8),
    Enum(u16),
}

impl CycleValue {
    /// The carrier `value` is in, or `None` for a string or array.
    #[inline]
    pub fn from_epics(value: &EpicsValue) -> Option<Self> {
        Some(match *value {
            EpicsValue::Double(v) => Self::Double(v),
            EpicsValue::Float(v) => Self::Float(v),
            EpicsValue::Long(v) => Self::Long(v),
            EpicsValue::ULong(v) => Self::ULong(v),
            EpicsValue::Int64(v) => Self::Int64(v),
            EpicsValue::UInt64(v) => Self::UInt64(v),
            EpicsValue::Short(v) => Self::Short(v),
            EpicsValue::UShort(v) => Self::UShort(v),
            EpicsValue::Char(v) => Self::Char(v),
            EpicsValue::UChar(v) => Self::UChar(v),
            EpicsValue::Enum(v) => Self::Enum(v),
            _ => return None,
        })
    }

    /// As [`EpicsValue::to_f64`] answers the same carrier.
    #[inline]
    pub fn to_f64(self) -> f64 {
        match self {
            Self::Double(v) => v,
            Self::Float(v) => v as f64,
            Self::Long(v) => v as f64,
            Self::ULong(v) => v as f64,
            Self::Int64(v) => v as f64,
            Self::UInt64(v) => v as f64,
            Self::Short(v) => v as f64,
            Self::UShort(v) | Self::Enum(v) => v as f64,
            // DBF_CHAR is epicsInt8.
            Self::Char(v) => (v as i8) as f64,
            Self::UChar(v) => v as f64,
        }
    }

    /// As `EpicsValue::as_int_i64` answers the same carrier.
    #[inline]
    fn as_int_i64(self) -> Option<i64> {
        match self {
            Self::Double(_) | Self::Float(_) => None,
            Self::Char(v) => Some(v as i8 as i64),
            Self::Long(v) => Some(v as i64),
            Self::ULong(v) => Some(v as i64),
            Self::Int64(v) => Some(v),
            Self::UInt64(v) => Some(v as i64),
            Self::Short(v) => Some(v as i64),
            Self::UShort(v) | Self::Enum(v) => Some(v as i64),
            Self::UChar(v) => Some(v as i64),
        }
    }

    /// `self` stored into an `epicsInt32` cell.
    #[inline]
    pub fn to_i32(self) -> i32 {
        self.as_int_i64().map_or_else(
            || crate::types::c_cast::f64_to_i32(self.to_f64()),
            |i| i as i32,
        )
    }

    /// `self` stored into an `epicsInt64` cell.
    #[inline]
    pub fn to_i64(self) -> i64 {
        self.as_int_i64()
            .unwrap_or_else(|| crate::types::c_cast::f64_to_i64(self.to_f64()))
    }

    /// `self` stored into an `epicsFloat32` cell.
    #[inline]
    pub(crate) fn to_f32(self) -> f32 {
        match self {
            Self::Float(v) => v,
            _ => crate::types::c_cast::f64_to_f32(self.to_f64()),
        }
    }
}

/// `self` stored into a cell of each remaining integer type, as
/// [`CycleValue::to_i32`] is.
macro_rules! cycle_value_to_int {
    ($($to:ident: $t:ty = $cast:ident),* $(,)?) => {
        impl CycleValue {
            $(
                #[inline]
                pub(crate) fn $to(self) -> $t {
                    self.as_int_i64().map_or_else(
                        || crate::types::c_cast::$cast(self.to_f64()),
                        |i| i as $t,
                    )
                }
            )*
        }
    };
}

cycle_value_to_int!(
    to_i8: i8 = f64_to_i8,
    to_u8: u8 = f64_to_u8,
    to_i16: i16 = f64_to_i16,
    to_u16: u16 = f64_to_u16,
    to_u32: u32 = f64_to_u32,
    to_u64: u64 = f64_to_u64,
);

impl CycleValue {
    /// `self` stored into a cell that holds `carrier`'s variant — the
    /// scalar arms of [`EpicsValue::convert_to`], so `prec->lalm = val`
    /// lands in the cell's own type.
    #[inline]
    pub fn convert_as(self, carrier: Self) -> Self {
        use crate::types::c_cast;
        // `convert_to` hands a value already in the target type back as is;
        // through the double, a `Char` 0xFF would saturate to 0.
        if std::mem::discriminant(&self) == std::mem::discriminant(&carrier) {
            return self;
        }
        let int = self.as_int_i64();
        let f = self.to_f64();
        match carrier {
            Self::Double(_) => Self::Double(f),
            Self::Float(_) => Self::Float(c_cast::f64_to_f32(f)),
            Self::Long(_) => Self::Long(self.to_i32()),
            Self::ULong(_) => Self::ULong(int.map_or_else(|| c_cast::f64_to_u32(f), |i| i as u32)),
            Self::Int64(_) => Self::Int64(self.to_i64()),
            Self::UInt64(_) => {
                Self::UInt64(int.map_or_else(|| c_cast::f64_to_u64(f), |i| i as u64))
            }
            Self::Short(_) => Self::Short(int.map_or_else(|| c_cast::f64_to_i16(f), |i| i as i16)),
            Self::UShort(_) => {
                Self::UShort(int.map_or_else(|| c_cast::f64_to_u16(f), |i| i as u16))
            }
            Self::Char(_) => {
                Self::Char(int.map_or_else(|| c_cast::f64_to_i8(f) as u8, |i| i as u8))
            }
            Self::UChar(_) => Self::UChar(int.map_or_else(|| c_cast::f64_to_u8(f), |i| i as u8)),
            Self::Enum(_) => Self::Enum(int.map_or_else(|| c_cast::f64_to_u16(f), |i| i as u16)),
        }
    }

    /// `self` as the `&EpicsValue` a hook such as [`Record::validate_put`]
    /// takes. A numeric value owns nothing, so it is never dropped: the drop
    /// of a temporary `EpicsValue` is an out-of-line `drop_glue` call even
    /// when its variant is known.
    #[inline]
    pub fn lend(self) -> std::mem::ManuallyDrop<EpicsValue> {
        std::mem::ManuallyDrop::new(self.into())
    }

    /// Whether the cell holds C's undefined double.
    #[inline]
    pub fn is_nan(self) -> bool {
        match self {
            Self::Double(v) => v.is_nan(),
            Self::Float(v) => v.is_nan(),
            _ => false,
        }
    }

    /// As [`AlarmLimit::from_ladder_value`] answers the same carrier.
    #[inline]
    pub fn ladder(self) -> Option<AlarmLimit> {
        match self {
            Self::Double(v) => Some(AlarmLimit::Double(v)),
            Self::Long(v) => Some(AlarmLimit::Long(v)),
            Self::Int64(v) => Some(AlarmLimit::Int64(v)),
            _ => None,
        }
    }

    /// As [`AlarmLimit::from_stored`] answers the same carrier.
    #[inline]
    pub fn alarm_limit(self) -> AlarmLimit {
        match self {
            Self::Long(v) => AlarmLimit::Long(v),
            Self::Int64(v) => AlarmLimit::Int64(v),
            other => AlarmLimit::Double(other.to_f64()),
        }
    }
}

/// A stored value, borrowed: a scalar by value, an array by slice. See
/// [`Record::monitor_value_view`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ValueView<'a> {
    Scalar(CycleValue),
    Double(&'a [f64]),
    Float(&'a [f32]),
    Long(&'a [i32]),
    ULong(&'a [u32]),
    Int64(&'a [i64]),
    UInt64(&'a [u64]),
    Short(&'a [i16]),
    UShort(&'a [u16]),
    Char(&'a [u8]),
    UChar(&'a [u8]),
    Enum(&'a [u16]),
}

/// An output value, borrowed from the record that holds it
/// ([`Record::output_view`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OutView<'a> {
    /// A numeric field (OVAL, VAL, RVAL ...), in the `DBR_*` it goes out as.
    Num(CycleValue),
    /// A `DBR_STRING` value, up to its NUL.
    Str(&'a [u8]),
    /// A `DBR_CHAR` array of `nRequest` elements (a long string).
    Chars(&'a [u8]),
    /// A numeric array, `nRequest` = its length.
    Array(ValueView<'a>),
}

impl From<CycleValue> for Scalar {
    /// As `EpicsValue` classifies the same carrier for narrowing.
    #[inline]
    fn from(value: CycleValue) -> Self {
        match value {
            CycleValue::Double(v) => Self::Float(v),
            CycleValue::Float(v) => Self::F32(v),
            CycleValue::Long(v) => Self::Int(v.into()),
            CycleValue::ULong(v) => Self::Int(v.into()),
            CycleValue::Int64(v) => Self::Int(v),
            CycleValue::UInt64(v) => Self::U64(v),
            CycleValue::Short(v) => Self::Int(v.into()),
            CycleValue::UShort(v) | CycleValue::Enum(v) => Self::Int(v.into()),
            CycleValue::Char(v) => Self::Int((v as i8).into()),
            CycleValue::UChar(v) => Self::Int(v.into()),
        }
    }
}

impl From<CycleValue> for EpicsValue {
    #[inline]
    fn from(value: CycleValue) -> Self {
        match value {
            CycleValue::Double(v) => Self::Double(v),
            CycleValue::Float(v) => Self::Float(v),
            CycleValue::Long(v) => Self::Long(v),
            CycleValue::ULong(v) => Self::ULong(v),
            CycleValue::Int64(v) => Self::Int64(v),
            CycleValue::UInt64(v) => Self::UInt64(v),
            CycleValue::Short(v) => Self::Short(v),
            CycleValue::UShort(v) => Self::UShort(v),
            CycleValue::Char(v) => Self::Char(v),
            CycleValue::UChar(v) => Self::UChar(v),
            CycleValue::Enum(v) => Self::Enum(v),
        }
    }
}

impl From<AlarmLimit> for CycleValue {
    #[inline]
    fn from(limit: AlarmLimit) -> Self {
        match limit {
            AlarmLimit::Double(v) => Self::Double(v),
            AlarmLimit::Long(v) => Self::Long(v),
            AlarmLimit::Int64(v) => Self::Int64(v),
        }
    }
}

/// The operands of C `monitor()`'s deadband test, in one domain.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeadbandCells<T> {
    /// The value the test is for ([`Record::monitor_deadband_value`]).
    pub val: T,
    /// MDEL, the monitor deadband.
    pub mdel: T,
    /// ADEL, the archive deadband.
    pub adel: T,
    /// MLST, the value the last `DBE_VALUE` post carried.
    pub mlst: T,
    /// ALST, the value the last `DBE_LOG` post carried.
    pub alst: T,
}

/// The four cells beside `val`, each converted by `to` into its domain.
#[inline]
fn deadband_cells<R: Record + ?Sized, T>(
    r: &R,
    val: T,
    last_posted: [f64; 2],
    to: fn(CycleValue) -> T,
) -> DeadbandCells<T> {
    let cell = |cell: CycleCell, absent: f64| {
        to(match r.cycle_cell(cell) {
            CycleRead::Value(v) => v,
            CycleRead::Absent => CycleValue::Double(absent),
            CycleRead::ByName => r
                .get_field(cell.name())
                .as_ref()
                .and_then(CycleValue::from_epics)
                .unwrap_or(CycleValue::Double(absent)),
        })
    };
    DeadbandCells {
        val,
        mdel: cell(CycleCell::Mdel, 0.0),
        adel: cell(CycleCell::Adel, 0.0),
        mlst: cell(CycleCell::Mlst, last_posted[0]),
        alst: cell(CycleCell::Alst, last_posted[1]),
    }
}

/// [`DeadbandCells`] in the domain VAL's type picks — see
/// [`Record::monitor_deadband_cells`], which is the only producer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MonitorDeadbandCells {
    /// `recGblCheckDeadband`.
    Double(DeadbandCells<f64>),
    /// longin/longout `DELTA` (`longinRecord.c:370`).
    Long(DeadbandCells<i32>),
    /// int64in/int64out `DELTA` (`int64inRecord.c:363`).
    Int64(DeadbandCells<i64>),
}

impl MonitorDeadbandCells {
    #[inline]
    pub fn val(&self) -> AlarmLimit {
        match self {
            Self::Double(c) => AlarmLimit::Double(c.val),
            Self::Long(c) => AlarmLimit::Long(c.val),
            Self::Int64(c) => AlarmLimit::Int64(c.val),
        }
    }

    /// Whether VAL crossed `(MDEL, ADEL)`. The integer arms are C's
    /// `mdel < 0 || DELTA(mlst, val) > (epicsUInt64) mdel`.
    #[inline(always)]
    pub fn crossed(&self) -> (bool, bool) {
        fn delta_crossed(delta: u64, deadband: i64) -> bool {
            deadband < 0 || delta > deadband as u64
        }
        match self {
            Self::Double(c) => (
                super::record_instance::check_deadband(c.val, c.mlst, c.mdel),
                super::record_instance::check_deadband(c.val, c.alst, c.adel),
            ),
            Self::Long(c) => (
                delta_crossed(c.val.abs_diff(c.mlst).into(), c.mdel.into()),
                delta_crossed(c.val.abs_diff(c.alst).into(), c.adel.into()),
            ),
            Self::Int64(c) => (
                delta_crossed(c.val.abs_diff(c.mlst), c.mdel),
                delta_crossed(c.val.abs_diff(c.alst), c.adel),
            ),
        }
    }
}

/// `prec->mlst = prec->alst = prec->lalm = prec->val` for whichever of the
/// three cells the record serves.
pub(crate) fn seed_trackers_from_val<R: Record + ?Sized>(rec: &mut R) {
    let seed = match rec.monitor_deadband_value() {
        Some(AlarmLimit::Double(v)) if !v.is_finite() => return,
        Some(v) => v,
        None => return,
    };
    for field in ["MLST", "ALST", "LALM"] {
        // Coerce to the cell's own type first. `bi`/`mbbi` declare MLST
        // DBF_USHORT and reject a Double outright, so seeding them as a
        // Double left them at 0 with the error dropped on the floor —
        // silently the no-seed behaviour, for two records C does seed.
        let Some(current) = rec.get_field(field) else {
            continue;
        };
        let coerced = seed.to_epics_value().into_type(current.db_field_type());
        let _ = rec.put_field(field, coerced);
    }
}

/// The answers to a record's [`Record::init_link_reads`], as C's
/// `dbGetLink` would have given them inside `init_record(prec, 1)`.
pub struct InitLinkReads {
    reads: Vec<(&'static str, InitRead)>,
    /// What the reads the record took did to its pending alarm, in the order
    /// it took them: `recGblSetSevr` keeps the first status of the highest
    /// severity, so order decides which one wins a tie.
    effects: Vec<InitReadEffect>,
}

/// One link's answer and what taking it does to the reader.
pub(crate) struct InitRead {
    pub(crate) fetch: crate::server::recgbl::simm::LinkFetch,
    /// `recGblInheritSevrMsg` on a successful read (`dbDbLink.c:228-232`).
    pub(crate) inherit: Option<(
        crate::server::record::MonitorSwitch,
        crate::server::database::LinkAlarm,
    )>,
    /// What `fetch` raises on the reader when it is `Failed`.
    pub(crate) failure: InitGetFailure,
}

/// What a failed `dbGetLink` raises (`dbLink.c:335-339`).
#[derive(Clone, Copy)]
pub(crate) enum InitGetFailure {
    /// `S_db_noLSET`: -1 with no `setLinkAlarm`.
    NoLset,
    /// `setLinkAlarm`'s `"field %s"`.
    LinkAlarm,
    /// The lset's own `"%s Disconn"` first, which `setLinkAlarm` then leaves
    /// in place (`pvalink_lset.cpp:263-268`).
    Disconn,
}

enum InitReadEffect {
    Disconn(&'static str),
    LinkAlarm(&'static str),
    Inherit(
        crate::server::record::MonitorSwitch,
        crate::server::database::LinkAlarm,
    ),
}

impl InitLinkReads {
    /// The answers, by link field. A link absent from them reads as an
    /// unset link: success, nothing written.
    pub(crate) fn new(reads: Vec<(&'static str, InitRead)>) -> Self {
        Self {
            reads,
            effects: Vec::new(),
        }
    }

    /// `dbGetLink(&prec->link, ...)`. A failed read raises LINK/INVALID on the
    /// record, as `dbGetLink`'s `setLinkAlarm` does (`dbLink.c:338-339`), and a
    /// good one folds the source's alarm by the link's MS flag, but only for a
    /// link the record actually reads.
    pub fn get(&mut self, link: &str) -> crate::server::recgbl::simm::LinkFetch {
        use crate::server::recgbl::simm::LinkFetch;
        let Some((name, read)) = self.reads.iter().find(|(name, _)| *name == link) else {
            return LinkFetch::NoData;
        };
        match (&read.fetch, &read.inherit) {
            (LinkFetch::Failed, _) => match read.failure {
                InitGetFailure::NoLset => {}
                InitGetFailure::LinkAlarm => self.effects.push(InitReadEffect::LinkAlarm(name)),
                InitGetFailure::Disconn => {
                    self.effects.push(InitReadEffect::Disconn(name));
                    self.effects.push(InitReadEffect::LinkAlarm(name));
                }
            },
            (LinkFetch::Value(_), Some((ms, alarm))) => {
                self.effects
                    .push(InitReadEffect::Inherit(*ms, alarm.clone()));
            }
            _ => {}
        }
        read.fetch.clone()
    }

    /// Fold what the taken reads did into the record's pending alarm.
    pub(crate) fn apply(&self, common: &mut crate::server::record::CommonFields) {
        for effect in &self.effects {
            match effect {
                InitReadEffect::Disconn(field) => {
                    crate::server::recgbl::rec_gbl_set_sevr_msg(
                        common,
                        crate::server::recgbl::alarm_status::LINK_ALARM,
                        crate::server::record::AlarmSeverity::Invalid,
                        crate::server::record::SparseText::link_disconn(field),
                    );
                }
                InitReadEffect::LinkAlarm(field) => {
                    crate::server::recgbl::rec_gbl_set_link_alarm(common, field);
                }
                InitReadEffect::Inherit(ms, alarm) => {
                    crate::server::database::inherit_sevr_msg(common, *ms, alarm);
                }
            }
        }
    }
}
