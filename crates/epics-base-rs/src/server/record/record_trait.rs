use crate::error::CaResult;
use crate::types::c_parse::Converted;
use crate::types::{DbFieldType, DbfCode, EpicsValue, PvString, c_parse};

use super::scan::ScanType;

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
/// The discriminants ARE C's `SPC_*` numbers (`special.h:26-39` @R7.0.10),
/// so `special as i16` is the integer C prints and stores. They are written
/// out rather than left implicit because the table is not contiguous — C has
/// no code 4, and the record-specific half starts at 100 — so the obvious
/// cast on an implicitly-numbered enum would have answered 4 for
/// [`Self::AlarmAck`] and 7 for [`Self::Mod`]. Carrying the number on the
/// type is what makes every future cast right by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i16)]
pub enum Special {
    /// No `special()` declared — C leaves `pdbFldDes->special` 0.
    None = 0,
    /// `SPC_NOMOD` — the field must not be modified. Mirrored into
    /// [`FieldDesc::read_only`], which is the bit the put gate reads.
    NoMod = 1,
    /// `SPC_DBADDR` — the record's `cvt_dbaddr` supplies the field's type
    /// and element count. [`FieldDesc::dbf_type`] carries the type C serves at
    /// the selector field's default; a record whose type is state-dependent
    /// (`waveform.VAL` on `FTVL`, `mbbo.VAL` on `SDEF`) overrides it at runtime.
    DbAddr = 2,
    /// `SPC_SCAN` — a scan-related field; C re-registers the scan.
    Scan = 3,
    /// `SPC_ALARMACK` — an alarm acknowledgement. C skips 4.
    AlarmAck = 5,
    /// `SPC_AS` — access security; C re-computes the record's ASG.
    As = 6,
    /// `SPC_ATTRIBUTE` — a pseudo (attribute) field. Set internally; it
    /// is the one code `pamapspcType` omits, so no `.dbd` declares it.
    Attribute = 7,
    /// `SPC_MOD` — the record's own `special()` runs on the put. C's
    /// record-specific range starts here.
    Mod = 100,
    /// `SPC_RESET` — the `RES` field is being modified.
    Reset = 101,
    /// `SPC_LINCONV` — a linear-conversion field changed; C calls the
    /// device support's `special_linconv`.
    LinConv = 102,
    /// `SPC_CALC` — the `CALC` expression changed; C recompiles it.
    Calc = 103,
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
}

impl ConstantInitLink {
    /// A seed that does not touch UDF — the INPA..L / SELL / NVL / DOLn form.
    pub const fn new(link_field: &'static str, target_field: &'static str) -> Self {
        Self {
            link_field,
            target_field,
            clears_udf: false,
            normalize_bool: false,
        }
    }

    /// A DOL→VAL seed, which C follows with `prec->udf = FALSE`.
    pub const fn dol_to_val(link_field: &'static str, target_field: &'static str) -> Self {
        Self {
            link_field,
            target_field,
            clears_udf: true,
            normalize_bool: false,
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
///   (`dbCa.c:662-704`), which both return `-1` on a disconnected link and
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
/// ([`Record::input_link_read_as`]); the framework, which is the side that
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
    /// C `dbGetLink(..., DBR_DOUBLE, ...)`.
    Double,
    /// C `dbGetLink(..., DBF_CHAR|DBF_UCHAR, buf, 0, &n)` — up to `max_elements`
    /// bytes of the source's char array, taken as the string they spell
    /// (`sseqRecord.c:682-686`: `n_elements` clamped to the record's 40-byte
    /// `s` buffer, then `strcmp`/`atof` read it as a C string).
    CharArrayAsString { max_elements: usize },
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
    /// widened one ([`Record::take_secondary_value_mask`]) and the event
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
    /// step, [`Record::take_secondary_value_change`], never split across the
    /// code that computed the new value — C assigns `oraw` INSIDE the post it
    /// guards, so a cycle that changes RVAL without posting it must leave
    /// `oraw` stale for the next cycle to find.
    OnChangeForced,
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
}

/// The [`ValuePostGate`] a record declared for `field`, or `None` when `field`
/// is not one of its secondary value-mask fields.
///
/// The single lookup for [`Record::fields_posted_with_value_mask`], shared by
/// every monitor loop (both `process_record_*` paths, the deferred-completion
/// path, and `RecordInstance::process_local`) so they cannot drift apart on how
/// a secondary field is gated.
pub(crate) fn value_gate(
    value_masked: &'static [(&'static str, ValuePostGate)],
    field: &str,
) -> Option<ValuePostGate> {
    value_masked
        .iter()
        .find(|(name, _)| *name == field)
        .map(|(_, gate)| *gate)
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

    /// `alarm_bits` is this cycle's `recGblResetAlarms` result; `deadband_mask`
    /// is VAL's own monitor mask (those alarm bits, plus `DBE_VALUE` when MDEL
    /// crossed and `DBE_LOG` when ADEL crossed).
    pub(crate) fn mask_for(
        &self,
        field: &str,
        alarm_bits: crate::server::recgbl::EventMask,
        deadband_mask: crate::server::recgbl::EventMask,
    ) -> crate::server::recgbl::EventMask {
        use crate::server::recgbl::EventMask;
        if self.value_only.contains(&field) {
            alarm_bits | EventMask::VALUE
        } else if self.monitor_masked.contains(&field) {
            deadband_mask | EventMask::VALUE
        } else if self.no_alarm_bits.contains(&field) {
            EventMask::VALUE | EventMask::LOG
        } else {
            alarm_bits | EventMask::VALUE | EventMask::LOG
        }
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
            ..P::NONE
        },
        "bo" => P {
            units: true,
            precision: true,
            control_double: true,
            enum_strs: true,
            ..P::NONE
        },
        // busyRecord.c (synApps busy): units/graphic/control/alarm NULL,
        // get_precision and get_enum_strs present.
        "busy" => P {
            precision: true,
            enum_strs: true,
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
    /// Async pending, but notify these intermediate field changes immediately.
    /// Used by motor records to flush DMOV=0 before the move completes.
    AsyncPendingNotify(Vec<(String, EpicsValue)>),
    /// Completed synchronously (PACT cleared, unlike `AsyncPending`), but the
    /// record produced no new value to publish this cycle — the framework must
    /// skip the value-publication epilogue (UDF clear / timestamp / monitor /
    /// FLNK). C parity `compressRecord.c:365` `if (status != 1)`: a compress
    /// record still accumulating toward its next compressed sample runs none of
    /// `recGblGetTimeStamp` / `monitor` / `recGblFwdLink` on that cycle.
    CompleteNoEmit,
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
    /// Field stores this cycle owes the framework, to be applied only AFTER
    /// the cycle's queued [`ProcessAction::WriteDbLink`] have executed.
    ///
    /// C runs a record's `dbPutLink` calls and the completion-flag clear that
    /// follows them inside ONE `dbScanLock` — `sseqRecord.c::processCallback`
    /// puts `LNKn` (`:714-792`) and `asyncFinish` clears `busy` (`:498-505`)
    /// with the lock held throughout, and `scalerRecord.c::process` clears
    /// `cnt` (`:370`) and puts `COUT`/`COUTP` (`:457`, `:463`) in one cycle —
    /// so `dbGetField`, which takes the same lock, can never observe the flag
    /// clear ahead of the writes. The port cannot hold the record's data guard
    /// across the writes (a self/cyclic OUT link would dead-lock the
    /// non-reentrant guard), so a `process()` that both queues a write and
    /// clears its flag exposes a state C cannot produce: `BUSY == 0` with
    /// `LNKn` unwritten, `CNT == 0` with `COUT` unwritten.
    ///
    /// A record therefore does not store such a field in `process()`; it emits
    /// it here, and the framework applies it — store plus a `DBE_VALUE` monitor
    /// post — immediately after this cycle's link writes and before the
    /// cycle's snapshot notification. One owner for one transition.
    ///
    /// Applied in order, so a cycle that raises and clears the same field
    /// (`sseqRecord.c:302-305` `busy = 1` then an invalid `SELN` reaching
    /// `asyncFinish`) publishes both transitions exactly as C does.
    ///
    /// Only the STORE moves. A flag whose value is an input to a later
    /// decision in the same cycle keeps its `process()` store: motor's `DMOV`
    /// gates `recGblFwdLink` at `motorRecord.cc:1509-1510`, so it is read after
    /// it is set and does not belong here.
    pub post_write_fields: Vec<(String, EpicsValue)>,
}

impl ProcessOutcome {
    /// Shorthand for a simple Complete with no actions.
    pub fn complete() -> Self {
        Self {
            result: RecordProcessResult::Complete,
            actions: Vec::new(),
            device_did_compute: false,
            post_write_fields: Vec::new(),
        }
    }

    /// Shorthand for Complete with actions.
    pub fn complete_with(actions: Vec<ProcessAction>) -> Self {
        Self {
            result: RecordProcessResult::Complete,
            actions,
            device_did_compute: false,
            post_write_fields: Vec::new(),
        }
    }

    /// Completed synchronously, but no new value was emitted this cycle, so
    /// the framework skips the value-publication epilogue (UDF clear /
    /// timestamp / monitor / FLNK). See `RecordProcessResult::CompleteNoEmit`.
    pub fn complete_no_emit() -> Self {
        Self {
            result: RecordProcessResult::CompleteNoEmit,
            actions: Vec::new(),
            device_did_compute: false,
            post_write_fields: Vec::new(),
        }
    }

    /// Completed synchronously with the alarm epilogue only — no value posts,
    /// no output, no forward link. See `RecordProcessResult::CompleteAlarmOnly`.
    pub fn complete_alarm_only() -> Self {
        Self {
            result: RecordProcessResult::CompleteAlarmOnly,
            actions: Vec::new(),
            device_did_compute: false,
            post_write_fields: Vec::new(),
        }
    }

    /// Shorthand for AsyncPending with no actions.
    pub fn async_pending() -> Self {
        Self {
            result: RecordProcessResult::AsyncPending,
            actions: Vec::new(),
            device_did_compute: false,
            post_write_fields: Vec::new(),
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
    /// `dbCommon.udf` — value is undefined. C records check this at the
    /// top of `process()` (e.g. `epidRecord.c:195`).
    pub udf: bool,
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
    pub time: std::time::SystemTime,
    /// `dbCommon.tsel` — time-stamp event link string.
    pub tsel: String,
    /// `dbCommon.dtyp` — device-support type name. A record's
    /// `process()` / pre-process hooks can branch on the DTYP to mirror
    /// C device support that lives in a separate DSET (e.g. the epid
    /// record's `devEpidSoftCallback` callback DSET drives the TRIG
    /// readback link, whereas `devEpidSoft` does not).
    pub dtyp: String,
    /// The callback band `dbCommon.prio` selects for work this cycle defers —
    /// C `seqRecord.c:145-146`, which re-runs
    /// `callbackSetPriority(prec->prio, &pcb->callback)` at the top of every
    /// `process()` so a `PRIO` written between cycles takes effect on the next
    /// one. Already converted: a record stashes this and hands it to
    /// [`crate::runtime::task::spawn_background`] rather than re-deriving a
    /// band from a raw menu index.
    pub callback_priority: crate::runtime::task::CallbackPriority,
}

/// C `epicsTime.h`: `epicsTimeEventDeviceTime` — the `TSE` sentinel
/// meaning "device support provides the time stamp". `timestampRecord.c`
/// uses it to take the OS-clock branch instead of `recGblGetTimeStamp`.
pub const EPICS_TIME_EVENT_DEVICE_TIME: i16 = -2;

/// Snapshot of changes from a process cycle, used for notify outside lock.
pub struct ProcessSnapshot {
    /// `(field, value, mask)` — every posted field carries its own
    /// `DBE_*` posting mask, mirroring C's per-field
    /// `db_post_events(prec, &field, mask)`. One process cycle posts
    /// different classes per field: a deadband-gated readback narrows
    /// to the deadbands that actually crossed (MDEL → `DBE_VALUE`,
    /// ADEL → `DBE_LOG`; motorRecord.cc `monitor()` 3476-3507,
    /// aiRecord.c `monitor()`), while a change-detected auxiliary
    /// field posts `DBE_VALUE | DBE_LOG` (motorRecord.cc 3522-3645
    /// `DBE_VAL_LOG`; calcRecord.c:420). A single record-wide mask
    /// collapses that granularity — an archive-only deadband crossing
    /// would wrongly reach `DBE_VALUE` subscribers whenever any other
    /// field changed in the same pass.
    pub changed_fields: Vec<(String, EpicsValue, crate::server::recgbl::EventMask)>,
}

impl ProcessSnapshot {
    /// The union of every `DBE_*` class this cycle actually published — the
    /// port's answer to "what did `db_post_events` send for this record".
    ///
    /// A field carrying an empty mask is not posted at all
    /// (`RecordInstance::notify_from_snapshot` skips it), so an empty union
    /// means the cycle published nothing and no monitor of any class fired.
    pub fn published_mask(&self) -> crate::server::recgbl::EventMask {
        self.changed_fields
            .iter()
            .fold(crate::server::recgbl::EventMask::NONE, |acc, (_, _, m)| {
                acc | *m
            })
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFetchPolicy {
    /// Read every configured link; a failed read neither stops the loop nor
    /// gates the record body. C `transformRecord.c::process` (534-547) reads
    /// on through a failed `dbGetLink` and computes anyway.
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
    fn field_native_count(&self, field: &str) -> Option<u32>;

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
    /// That distinction is why this cannot be one lookup here either.
    /// [`FieldDesc::special`] carries the `cvt_dbaddr` special for a DBADDR
    /// field, not the declaration (`dbd/cvt_dbaddr.types` says so, and derives
    /// `read_only` from it) — which preserves `SPC_DBADDR` for every such field
    /// except the five long-string ones, whose `cvt_dbaddr` overwrites it. Those
    /// five are exactly [`Record::long_string_fields`] (`lsi`/`lso` VAL+OVAL,
    /// `printf` VAL), so the second half asks the record for them.
    fn field_is_dbaddr(&self, field: &str) -> bool;
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
        self.dbaddr_capacity(field)
    }

    fn field_is_dbaddr(&self, field: &str) -> bool {
        self.field_list()
            .iter()
            .any(|d| d.name.eq_ignore_ascii_case(field) && d.special == Special::DbAddr)
            || self
                .long_string_fields()
                .iter()
                .any(|f| f.eq_ignore_ascii_case(field))
    }
}

/// Trait that all EPICS record types must implement.
pub trait Record: Send + Sync + 'static {
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
    /// Applied on both the GET path (`snapshot_for_field`) and the
    /// monitor path (`make_monitor_snapshot`), AFTER the cached
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

    /// Field names this record serves as a *long string*: a `DBF_CHAR`
    /// array field that semantically holds a NUL-terminated string.
    ///
    /// In EPICS such a field is declared `DBF_NOACCESS` (or carries a `$`
    /// modifier) and is accessed through a `DBR_CHAR` array view whose
    /// `form` is `"String"`; pvxs maps that view to a scalar `pvString`
    /// rather than an `int8[]` (`ioc/channel.cpp:58-68`,
    /// `ioc/iocsource.cpp:619-643`). QSRV uses this list to serve those
    /// fields as scalar-string NTScalar values instead of byte scalars.
    ///
    /// The record keeps its `CharArray` storage; the QSRV boundary does
    /// the `CharArray <-> String` conversion. Default empty — only
    /// long-string record types (`lsi`/`lso` VAL/OVAL, `printf` VAL)
    /// override this. Names are matched case-insensitively.
    fn long_string_fields(&self) -> &'static [&'static str] {
        &[]
    }

    /// Field names declared `pp(TRUE)` in this record type's DBD (empty if
    /// none, e.g. `event`/`histogram`, or if the type is unmodeled).
    ///
    /// Drives the `dbPutField` processing gate: C `dbAccess.c:1263`
    /// re-processes a record on a put only when the put field is `PROC` or it
    /// is `pp(TRUE)` **and** `SCAN == Passive`. The table is total and
    /// fail-safe — an unmodeled type returns `&[]` (and warns once), so its
    /// field puts never auto-process (only `PROC` does). The default consults
    /// the central DBD-sourced table keyed by [`Record::record_type`]; record
    /// types can override.
    fn process_passive_fields(&self) -> &'static [&'static str] {
        super::process_passive::pp_fields_for(self.record_type())
    }

    /// Whether a put to `field` should reprocess this Passive record.
    ///
    /// The default is pure `pp(TRUE)` membership — the put gate's
    /// `field in process_passive_fields()` test. A record type overrides this
    /// when its C `special()` conditionally returns ERROR to suppress the
    /// reprocess for a `pp(TRUE)` field on certain values (e.g. motor STUP:
    /// only a `STUP == ON` put runs the status-update process; any other value
    /// is clamped to OFF and C returns ERROR so no process runs). Modeling that
    /// here keeps the suppression at the same gate as the pp test, with no
    /// per-put one-shot state — the post-clamp field value is deterministic.
    fn processes_after_put(&self, field: &str) -> bool {
        self.process_passive_fields()
            .iter()
            .any(|f| f.eq_ignore_ascii_case(field))
    }

    /// The record's `DBF_ENUM` state strings — the C rset slot pair
    /// `get_enum_strs` / `put_enum_str`, which in C read the same fields and
    /// are therefore ONE table here.
    ///
    /// It is what a client reads as the `DBR_ENUM` choice list AND the set of
    /// names a `DBR_STRING` put to the record's `DBF_ENUM` `VAL` may name
    /// (`dbConvert.c::putStringEnum` → [`crate::server::record::resolve_enum_state_string`]). Taking
    /// both from one table is the point: a name the record advertises is a name
    /// a client may put, by construction — the two cannot drift.
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
    /// `None` — the record leaves the rset slot NULL (`#define get_enum_str
    /// NULL`), which is every record but `bi`/`bo`/`mbbi`/`mbbo`. A field on
    /// such a record renders from its menu instead; see
    /// `RecordInstance::enum_string_form_for`,
    /// the single owner that picks between the two.
    fn enum_string_form(&self) -> Option<crate::server::snapshot::EnumStringForm> {
        None
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
        self.get_field(self.primary_field())
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
    fn soft_input_read(&mut self, value: Option<EpicsValue>) -> CaResult<()> {
        match value {
            Some(value) => self.set_val(value),
            None => Ok(()),
        }
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

    /// C parity: a record whose completion restamps `TIME` AFTER the VAL
    /// monitor post and the forward link, not before the value post like
    /// every standard record (`aoRecord.c:190` stamps ahead of `writeValue`).
    ///
    /// Only sseq's `asyncFinish` (`sseqRecord.c`) has this ordering: it posts
    /// VAL at `:474`, runs `recGblFwdLink` at `:499`, and only then calls
    /// `recGblGetTimeStamp` at `:501`. So the first VAL monitor event carries
    /// the record's pre-update timestamp, and `TIME` advances only for the
    /// following BUSY post and the next cycle — the VAL timestamp always lags
    /// one completion behind. The framework's synchronous `Complete` tail
    /// consults this to skip the pre-output restamp and apply it after the
    /// forward-link tail instead. Default false: every other record (including
    /// the base `seq`, whose `seqRecord.c:224` restamps BEFORE the `:229` VAL
    /// post) stamps before the value post.
    fn restamps_time_after_completion(&self) -> bool {
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
    /// `RecordInstance::value_include_classes`, which
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

    /// The value the MDEL/ADEL deadband is evaluated against.
    ///
    /// For most records C `monitor()` applies the value deadband to
    /// `VAL`, so the default is [`Self::val`]. A record whose monitored
    /// quantity is not its primary value must override this: the motor
    /// record, for instance, has `VAL` as the setpoint and applies
    /// MDEL/ADEL to `RBV` (the readback) — its C `monitor()` deadbands
    /// `RBV`, not `VAL`. Such a record returns its readback field here.
    ///
    /// Default is `val()`, so existing records are unaffected.
    fn monitor_deadband_value(&self) -> Option<EpicsValue> {
        self.val()
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
    fn alarm_cycle_monitored_fields(&self) -> &'static [&'static str] {
        &[]
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
    fn force_posted_fields(&self) -> &'static [&'static str] {
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
    /// Default: empty — and `Vec::new()` does not allocate.
    fn take_cycle_posted_fields(&mut self) -> Vec<(&'static str, CyclePostMask)> {
        Vec::new()
    }

    /// Fields this record posts on its FIRST monitor cycle whether or not they
    /// changed — C's `|| (prpvt->firstCalcPosted == 0)` term
    /// (`transformRecord.c:798`), which `monitor()` disarms unconditionally
    /// afterwards (`:807`) so it fires once per IOC lifetime.
    ///
    /// Deliberately NOT [`Self::take_cycle_posted_fields`], which carries marks
    /// the RUNNING cycle made and which iocInit drops wholesale
    /// (`seed_record_after_init`, so a seed put's mark cannot become a late
    /// event). This flag is initial record state, made by no put, so it has to
    /// survive the seed — putting it on the per-cycle channel would let the
    /// init drain consume it and the first cycle would post nothing.
    ///
    /// Same TAKE semantics and same masks: called once per process cycle, and
    /// the record clears what it answered from.
    ///
    /// Default: empty.
    fn take_first_monitor_cycle(&mut self) -> Vec<(&'static str, CyclePostMask)> {
        Vec::new()
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
    fn log_swept_fields(&self) -> &'static [&'static str] {
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

    /// Per-cycle widening of the guard [`ValuePostGate::OnChangeForced`]
    /// fields sit behind — C `if (prec->omod) monitor_mask |=
    /// (DBE_VALUE|DBE_LOG);` (aoRecord.c:535), which opens the secondary block
    /// on a cycle where VAL's own monitor mask is empty.
    ///
    /// ao needs it because `omod` tracks OVAL, not VAL: OROC can walk the
    /// output one step per cycle toward a VAL that has not moved since MLST,
    /// so C posts OVAL/RVAL on cycles that post no VAL at all.
    ///
    /// Clear-on-read, because C's is: `omod` can only ever be true on a cycle
    /// whose guard therefore fires, and the guard's first act is
    /// `prec->omod = FALSE` (`:537`).
    ///
    /// Default: empty — no widening, so the guard is VAL's own mask.
    fn take_secondary_value_mask(&mut self) -> crate::server::recgbl::EventMask {
        crate::server::recgbl::EventMask::NONE
    }

    /// C's whole `if (prec->oraw != prec->rval) { db_post_events(...);
    /// prec->oraw = prec->rval; }` (aoRecord.c:541-543) minus the post: TEST
    /// the [`ValuePostGate::OnChangeForced`] field against the record's own
    /// "old" copy and, when it moved, ADVANCE that copy — one indivisible
    /// step, because C's two statements are inseparable and splitting them is
    /// what the port got wrong.
    ///
    /// Called once per cycle per declared field, by the framework, only when
    /// the guard is open — and independently of whether anyone is subscribed,
    /// since C's `db_post_events` runs whether or not anyone is listening. A
    /// record must not advance the copy anywhere else: an eager `oraw = rval`
    /// in the compute step makes the next guarded cycle see no change and
    /// withhold an event C sends.
    ///
    /// Default: `false` — nothing declared, nothing to test.
    fn take_secondary_value_change(&mut self, field: &str) -> bool {
        let _ = field;
        false
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
    /// `check_deadband_ext` (the single owner of the VAL-mask decision).
    ///
    /// The hook is the VAL mask, not the MPST/APST rule specifically: any
    /// record whose C `monitor()` decides the mask by its own rule implements
    /// it. `histogram` is the other implementor — its rule is the MDEL COUNT
    /// deadband (`mcnt > mdel`, histogramRecord.c:287-291), and like waveform's
    /// it updates the state it keys on (there, `MCNT = 0`; here, `HASH`).
    fn array_monitor_post(&mut self) -> Option<ArrayMonitorPost> {
        None
    }

    /// Fields the record posts itself via an event-driven, individually
    /// masked path rather than the generic change-detection loop. The
    /// framework excludes these from that loop so they are neither
    /// double-posted nor spuriously posted on a cycle the event did not
    /// fire. C waveform/aai/aao `monitor()` posts `HASH` this way —
    /// `db_post_events(prec, &prec->hash, DBE_VALUE)` only when the content
    /// hash changed (waveformRecord.c:317-319), never via VAL's change.
    ///
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
    /// put and from [`Self::monitor_side_effect_fields`]. The list is a
    /// whitelist, not a blacklist, so a field added to the record later stays
    /// silent unless C posts it.
    ///
    /// Fields inside the list keep their normal treatment (change detection,
    /// [`Self::value_only_change_fields`] mask, deadband, `log_swept_fields`).
    fn process_posted_fields(&self) -> Option<&'static [&'static str]> {
        None
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

    /// Whether this record type's C `init_record` resets the record to a
    /// defined, no-alarm state — `prec->udf = 0; recGblResetAlarms(prec)` — so
    /// a freshly loaded, never-processed record reads `UDF=0`,
    /// `STAT=NO_ALARM`, `SEVR=NO_ALARM` instead of the born `UDF`/`INVALID`/`1`.
    ///
    /// Almost no record does this: a not-yet-processed record is normally left
    /// `UDF` so an `MS` consumer inherits `INVALID` at IOC startup (see
    /// `RecordInstance::run_init_passes`). The asyn record is the exception —
    /// `asynRecord.c` `init_record` pass 0 does `pasynRec->udf = 0;
    /// recGblResetAlarms(pasynRec)` unconditionally, because a device-config
    /// record is defined the moment it loads, even against a disconnected port.
    /// `UDF` is a common field `init_record` cannot reach, so the record
    /// declares the fact here and the init owner performs the reset. Default
    /// `false`.
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
    /// trackers) had to be the exception. Default: empty.
    fn init_record_tail(&mut self) {}

    /// Seed the monitor/archive/alarm deadband trackers (MLST/ALST/LALM)
    /// from the initial value at iocInit, called once by the builder after
    /// both `init_record` passes and `post_init_finalize_undef`.
    ///
    /// Most C value records end `init_record` with
    /// `prec->mlst = prec->alst = prec->lalm = prec->val`
    /// (`aiRecord.c:129-131`, `longinRecord.c:120-122`), so their first
    /// `monitor()` evaluates `DELTA(mlst, val) > mdel` with `mlst == val`
    /// and posts nothing for a value that has not moved since init. The
    /// default does that for whichever of MLST/ALST/LALM the record serves,
    /// so a record given a nonzero initial VAL (constant DOL, `field(VAL,..)`)
    /// does not post a spurious first-cycle update.
    ///
    /// It is NOT universal, which is why it stays overridable: `calcRecord.c`
    /// (:90-114), `dfanoutRecord.c` (:96-111), `selRecord.c` (:88-110),
    /// `sCalcoutRecord.c` (:203-322), `aCalcoutRecord.c` (:171-281) and
    /// `busyRecord.c` (:127-183) end `init_record` without touching the
    /// trackers, so those six leave them at 0 and DO post that first update.
    /// Each overrides this with an empty body. The output records override it
    /// only to name the subset their C tail actually writes (`mbboDirect`
    /// seeds MLST and no LALM, `boRecord.c:172-173` seeds MLST and LALM but no
    /// ALST); everything else their tail does is
    /// [`Self::init_record_tail`]'s.
    fn seed_deadband_tracking(&mut self) {
        let seed = match self.monitor_deadband_value().and_then(|v| v.to_f64()) {
            Some(v) if v.is_finite() => v,
            _ => return,
        };
        for field in ["MLST", "ALST", "LALM"] {
            // Coerce to the cell's own type first. `bi`/`mbbi` declare MLST
            // DBF_USHORT and reject a Double outright, so seeding them as a
            // Double left them at 0 with the error dropped on the floor —
            // silently the no-seed behaviour, for two records C does seed.
            let Some(current) = self.get_field(field) else {
                continue;
            };
            let coerced = EpicsValue::Double(seed).convert_to(current.db_field_type());
            let _ = self.put_field(field, coerced);
        }
    }

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
    fn set_resolved_input_links(&mut self, _resolved: &[&'static str]) {}

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
    fn input_link_failure_is_inert(&self, _link_field: &str) -> bool {
        false
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

    /// Called before/after a field put for side-effect processing.
    fn special(&mut self, _field: &str, _after: bool) -> CaResult<()> {
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
    /// never invoked twice for the same state. Default: ignore.
    fn set_io_intr_scan(&mut self, _active: bool) {}

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

    /// Other fields whose monitors must be posted because a put to
    /// `put_field` changed them as a side effect, without driving a full
    /// process cycle.
    ///
    /// Mirrors the explicit `db_post_events` calls a C `special()` makes:
    /// e.g. `compressRecord.c::reset` (invoked on a `SPC_RESET` write to
    /// `RES`) posts `NUSE` and `VAL` even though `RES` is not `pp(TRUE)`
    /// and so does not process. The framework posts a `VALUE|LOG` monitor
    /// for each returned field after the put. Default: none.
    fn monitor_side_effect_fields(&self, _put_field: &str) -> &'static [&'static str] {
        &[]
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
    /// The framework already posts the `db_post_events` half of that `monitor()`
    /// through [`Record::monitor_side_effect_fields`]; this hook is the
    /// `recGblResetAlarms` half. When it returns true, the put owner
    /// (`field_io`'s `dbPut` paths) runs `rec_gbl_reset_alarms` and posts the
    /// resulting STAT/SEVR/AMSG/ACKS transition. Default: false (a `special()`
    /// that does not run the record's `monitor()`).
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

    /// Downcast to concrete type for device support init injection.
    /// Override in record types that need device support to inject state (e.g., MotorRecord).
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        None
    }

    /// Whether processing this record should clear UDF.
    /// Override to return false for record types that don't produce a valid value every cycle.
    fn clears_udf(&self) -> bool {
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
    /// resolvable at all (`dbgf HI.INP` → `PV 'HI.INP' not found`). Both
    /// [`RecordInstance::get_common_field`] and
    /// [`RecordInstance::put_common_field`] consult this, so the field cannot
    /// be readable on one route while refused on the other.
    ///
    /// [`RecordInstance::get_common_field`]: crate::server::record::RecordInstance::get_common_field
    /// [`RecordInstance::put_common_field`]: crate::server::record::RecordInstance::put_common_field
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
    /// Called by the framework on a soft-DTYP cycle whose INP is constant, with
    /// `value = Some(constant)` for a non-empty constant and `None` for an
    /// empty/unset INP. Returns whether the record consumed the input stage.
    /// Default `false` — the load-once rule.
    fn read_constant_inp(&mut self, _value: Option<EpicsValue>) -> bool {
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
    fn udf_alarm_message(&self) -> &str {
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
    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        &[]
    }

    /// The `(link_field, value_field)` pairs whose CONSTANT value this record's
    /// C `special()` RE-SEEDS on a runtime put to the link field —
    /// `recGblInitConstantLink(plink, DBF_DOUBLE, pvalue)` +
    /// `db_post_events(prec, pvalue, DBE_VALUE)` + `INAV = CON`
    /// (`calcoutRecord.c:373-378`, `sCalcoutRecord.c:513-518`,
    /// `aCalcoutRecord.c:533-538`).
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
    /// calc — calcout, sCalcout, aCalcout, transform. Everywhere else (calc,
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
    ) -> Option<Vec<(&'static str, &'static str)>> {
        None
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
        self.get_field("OVAL").or_else(|| self.val())
    }

    /// Return multi-output link field pairs: (link_field, value_field).
    /// Override in transform to return OUTA..OUTP → A..P mappings.
    fn multi_output_links(&self) -> &[(&'static str, &'static str)] {
        &[]
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
    fn input_link_read_as(&self, link_field: &str, source: &OutTarget) -> Option<LinkReadAs> {
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
    fn output_event(&self) -> Option<String> {
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
    fn pre_process_actions(&mut self) -> Vec<ProcessAction> {
        Vec::new()
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
    fn pre_input_link_actions(&mut self) -> Vec<ProcessAction> {
        Vec::new()
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
    /// Records that hold PACT across their delay (calcout / scalcout /
    /// acalcout / swait / sseq ODLY) already disambiguate through PACT and do
    /// not need this. Additive, framework-set-hook pattern (same shape as
    /// [`Record::set_process_context`]). Default: ignore.
    fn set_process_continuation(&mut self, _continuation: bool) {}

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
    fn set_out_link_write_status(
        &mut self,
        _link_field: &'static str,
        _value: &EpicsValue,
        _failed: bool,
    ) {
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
    fn set_async_context(&mut self, _name: String, _db: crate::server::database::AsyncDbHandle) {}

    /// Framework init hook: called once at record load *after* the common
    /// link fields (`INP`/`OUT`/`FLNK`/...) have been resolved and the
    /// `init_record` passes have run, with the record's resolved
    /// [`CommonFields`](crate::server::record::CommonFields).
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
    // An array source into a SCALAR destination delivers element 0. C's link
    // layer asks for exactly one element (`dbGetLink(..., nRequest = NULL)`), so
    // `dbGet` converts the field at offset 0 and the record sees a scalar — a
    // waveform INP into an `ai.VAL` lands `wf[0]`, it is not dropped. Without the
    // reduction the array reached the record's typed `put_field` arm, which
    // rejected it and left the field at its stale value. Same clamp as
    // `field_io::dbput_request` (C `dbPut` `nRequest -> no_elements`), through the
    // same primitive; a `CharArray` into a `DBF_STRING` field is likewise exempt —
    // that shape is the dbChannel `$` char view of a string field, decoded by
    // `convert_to`.
    let dest_is_array = record.get_field(name).is_some_and(|v| v.is_array());
    let is_char_string_view =
        matches!(value, EpicsValue::CharArray(_)) && target_type == Some(DbFieldType::String);
    let value = if !dest_is_array && value.is_array() && !is_char_string_view {
        value.first_element().unwrap_or(value)
    } else {
        value
    };
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
        None if is_enum_carrier => value.convert_to(DbFieldType::Long),
        _ => value,
    };
    record.put_field(name, value)
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
/// An ARRAY destination is exempt HERE, not exempt from the rule: C reaches it
/// through the same `putString*` routine (`nRequest` elements, parsed one at a
/// time), and this port's array records carry their own element-type
/// conversion, so the row is theirs to run. `WaveformRecord::put_field("VAL")`
/// runs it, refusing `"hi"` into an `FTVL=CHAR` buffer exactly as the compiled
/// softIoc does. A string reaching a char buffer as its BYTES is the DBR_CHAR
/// row (`caput -S`), which arrives as a `CharArray` and needs no conversion.
pub fn coerce_put_value<R: Record + ?Sized>(
    record: &R,
    field: &str,
    target: DbFieldType,
    value: EpicsValue,
) -> CaResult<Converted> {
    if let EpicsValue::String(s) = &value {
        // DTYP (DBF_DEVICE) validates against the record type's FULL device
        // menu — static `device()` lines PLUS runtime-contributed device
        // support — the same set the read/announce path exposes via
        // `RecordInstance::device_choices`. `menu_choices_of`'s DTYP branch
        // returns only the static half, so a contributed device-support name
        // (asyn's `asynInt32`, scaler-rs's `Asyn Scaler`, ...) would wrongly
        // fail this put even though a client can read it in the DTYP choices.
        // Resolve DTYP against the merged menu to keep put and read symmetric.
        if field.eq_ignore_ascii_case("DTYP") {
            let choices = super::merged_device_menu(record.record_type());
            if !choices.is_empty() {
                return super::resolve_menu_field_string(
                    field,
                    &choices,
                    target,
                    &s.as_str_lossy(),
                )
                .map(Converted::Stored);
            }
            // No device menu declared or contributed for this record type: fall
            // through to the generic handling below (unchanged behavior).
        } else if let Some(choices) = super::record_instance::menu_choices_of(record, field) {
            return super::resolve_menu_field_string(field, choices, target, &s.as_str_lossy())
                .map(Converted::Stored);
        }
        if target == DbFieldType::Enum {
            return super::resolve_enum_state_string(
                field,
                record.enum_state_strings().as_deref(),
                s,
            )
            .map(Converted::Stored);
        }
        // An ARRAY destination runs the row ITSELF: C applies the same
        // `putString*` routine to each of the `nRequest` elements, and this
        // port's array records carry their own element-type conversion, so the
        // string is handed over untouched. Converting here would run a SECOND
        // and DIFFERENT rule ahead of theirs — `convert_to`'s string→`DBF_CHAR[]`
        // byte carry, which is the DBR_CHAR row (`caput -S`), not this one.
        if record.get_field(field).is_some_and(|v| v.is_array()) {
            return Ok(Converted::Stored(value));
        }
        if let Some(numeric) = c_parse::NumericField::of(target) {
            return c_parse::put_string(field, numeric, &s.as_str_lossy());
        }
        if target == DbFieldType::String {
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
    }
    // The float/double rows of C's DBR_STRING put column render through the
    // record's `get_precision`, seeded 6: `putFloatString`/`putDoubleString`
    // (`dbConvert.c:1558`/`:1600`) for the array path,
    // `cvt_f_st`/`cvt_d_st` (`dbFastLinkConv.c:1216`/`:1333`) for the scalar
    // one that `dbAccess.c:1391` actually takes. `convert_to` cannot express
    // it — it is field-blind by contract, and precision is the record's — so
    // the row belongs here, next to the menu and enum rows it also cannot
    // express. Rendering through the GET direction's own converter is what
    // keeps `dbtgf REC.DESC` and a `caput` of the same number agreeing.
    if target == DbFieldType::String
        && let Some(rendered) =
            crate::types::codec::dbr_string_at_precision(&value, put_string_precision(record))
    {
        return Ok(Converted::Stored(rendered));
    }
    Ok(Converted::Stored(value.convert_to(target)))
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
fn put_string_precision<R: Record + ?Sized>(record: &R) -> u16 {
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
fn cap_string_to_field_size<R: Record + ?Sized>(record: &R, field: &str, s: &PvString) -> PvString {
    match super::record_instance::field_desc_of(record, field) {
        Some(desc) if desc.size > 0 => {
            let cap = (desc.size as usize).saturating_sub(1);
            let bytes = s.as_bytes();
            if bytes.len() > cap {
                PvString::from_bytes(bytes[..cap].to_vec())
            } else {
                s.clone()
            }
        }
        _ => s.clone(),
    }
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
            | Special::Calc => (),
        }
    }

    #[test]
    fn every_special_carries_cs_spc_number() {
        for (spc, n, name) in C_SPECIAL {
            assert_eq!(spc as i16, n, "{name}: wrong SPC_ number");
        }
        for (a, (x, ..)) in C_SPECIAL.iter().enumerate() {
            for (y, ..) in C_SPECIAL.iter().skip(a + 1) {
                assert_ne!(x, y, "{x:?} listed twice");
            }
        }
        // The gap is the reason the discriminants are written out: an
        // implicitly-numbered enum would have answered 4 here and 7 for
        // `Mod`, and `special as i16` is the integer C prints.
        assert!(!C_SPECIAL.iter().any(|(_, n, _)| *n == 4), "C has no SPC 4");
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
