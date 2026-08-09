use crate::error::CaResult;
use crate::types::{DbFieldType, EpicsValue, PvString, c_parse};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Special {
    /// No `special()` declared.
    None,
    /// `SPC_NOMOD` (1) — the field must not be modified. Mirrored into
    /// [`FieldDesc::read_only`], which is the bit the put gate reads.
    NoMod,
    /// `SPC_DBADDR` (2) — the record's `cvt_dbaddr` supplies the field's type
    /// and element count. [`FieldDesc::dbf_type`] carries the type C serves at
    /// the selector field's default; a record whose type is state-dependent
    /// (`waveform.VAL` on `FTVL`, `mbbo.VAL` on `SDEF`) overrides it at runtime.
    DbAddr,
    /// `SPC_SCAN` (3) — a scan-related field; C re-registers the scan.
    Scan,
    /// `SPC_ALARMACK` (5) — an alarm acknowledgement.
    AlarmAck,
    /// `SPC_AS` (6) — access security; C re-computes the record's ASG.
    As,
    /// `SPC_ATTRIBUTE` (7) — a pseudo (attribute) field.
    Attribute,
    /// `SPC_MOD` (100) — the record's own `special()` runs on the put.
    Mod,
    /// `SPC_RESET` (101) — the `RES` field is being modified.
    Reset,
    /// `SPC_LINCONV` (102) — a linear-conversion field changed; C calls the
    /// device support's `special_linconv`.
    LinConv,
    /// `SPC_CALC` (103) — the `CALC` expression changed; C recompiles it.
    Calc,
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
    /// The `DBF_*` type. This is the *field* type; the CA wire type is derived
    /// from it by [`DbFieldType::ca_wire_type`], which owns the promotions CA
    /// has no type for (`ULong`/`Int64`/`UInt64` -> `DBR_DOUBLE`, `UShort` ->
    /// `DBR_LONG`, `UChar` -> `DBR_CHAR`). Do not pre-promote here: PVA serves
    /// the native width.
    ///
    /// This is what the field is SERVED as, on every delivery path — see
    /// [`RecordInstance::project_to_declared_type`](super::RecordInstance::project_to_declared_type),
    /// which projects the stored value onto it. The one exception is a
    /// [`Self::runtime_typed`] field, where C's `cvt_dbaddr` overrides.
    pub dbf_type: DbFieldType,
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
    /// The full `special()` code, of which [`Self::read_only`] is one case.
    pub special: Special,
    /// `pp(TRUE)` — a put to this field processes the record.
    pub pp: bool,
    /// `asl(...)` — the access-security level.
    pub asl: Asl,
    /// `size(N)` — the declared byte size of a `DBF_STRING` field, or 0.
    pub size: u16,
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
            // A hand-written table declares a plain field: the type it names is
            // the type it is served as. The `cvt_dbaddr` records are all on the
            // generated table, which sets this from `cvt_dbaddr.types`.
            runtime_typed: false,
            read_only,
            special: if read_only {
                Special::NoMod
            } else {
                Special::None
            },
            pp: false,
            asl: Asl::Asl1,
            size: 0,
            menu: None,
            initial: None,
            interest: 0,
            prop: false,
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
/// (`dbAccess.c:336-430`), and therefore what decides whether QSRV marks
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
        // aCalcoutRecord.c:793-822, sCalcoutRecord.c:653 — the switch lists
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
/// (`aCalcoutRecord.c:1046-1068`, `sCalcoutRecord.c:906-928`).
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
        // `get_enum_strs`. `boRecord.c:59-61` keeps `get_units`,
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
    /// The framework spawns `tokio::spawn(sleep(d) + process_record(name))`.
    /// The current cycle's OUT/FLNK/notify proceed normally.
    ///
    /// Equivalent to C EPICS `callbackRequestDelayed()` + `scanOnce()`.
    ReprocessAfter(std::time::Duration),

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
}

impl ProcessOutcome {
    /// Shorthand for a simple Complete with no actions.
    pub fn complete() -> Self {
        Self {
            result: RecordProcessResult::Complete,
            actions: Vec::new(),
            device_did_compute: false,
        }
    }

    /// Shorthand for Complete with actions.
    pub fn complete_with(actions: Vec<ProcessAction>) -> Self {
        Self {
            result: RecordProcessResult::Complete,
            actions,
            device_did_compute: false,
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
        }
    }

    /// Completed synchronously with the alarm epilogue only — no value posts,
    /// no output, no forward link. See `RecordProcessResult::CompleteAlarmOnly`.
    pub fn complete_alarm_only() -> Self {
        Self {
            result: RecordProcessResult::CompleteAlarmOnly,
            actions: Vec::new(),
            device_did_compute: false,
        }
    }

    /// Shorthand for AsyncPending with no actions.
    pub fn async_pending() -> Self {
        Self {
            result: RecordProcessResult::AsyncPending,
            actions: Vec::new(),
            device_did_compute: false,
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
    /// ADEL → `DBE_LOG`; motorRecord.cc `monitor()` 3477-3507,
    /// aiRecord.c `monitor()`), while a change-detected auxiliary
    /// field posts `DBE_VALUE | DBE_LOG` (motorRecord.cc 3522-3645
    /// `DBE_VAL_LOG`; calcRecord.c:420). A single record-wide mask
    /// collapses that granularity — an archive-only deadband crossing
    /// would wrongly reach `DBE_VALUE` subscribers whenever any other
    /// field changed in the same pass.
    pub changed_fields: Vec<(String, EpicsValue, crate::server::recgbl::EventMask)>,
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
    /// gates the record body. C `transformRecord.c::process` (531-545) reads
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
    /// their previous values; `subRecord.c::process` (145-146) then runs
    /// `do_sub` only `if (status == 0)`, freezing VAL/UDF and raising none of
    /// the subroutine's alarms. `aSubRecord.c` (277-289 fetch, 216-218
    /// process), `sCalcoutRecord.c` (885-887 fetch, 356 gate),
    /// `aCalcoutRecord.c` (1068-1071 fetch, 399 gate) and
    /// `swaitRecord.c` (686-705 fetch, 408 gate) are the same shape.
    AbortOnFirstFailure,
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
pub trait FieldDeclaration {
    /// The record type's field descriptors, in `.dbd` declaration order.
    fn field_list(&self) -> &'static [FieldDesc];

    /// The record-own `DBF_NOACCESS` internal names the generator dropped
    /// from [`FieldDeclaration::field_list`] (`BPTR`, `RPVT`, ...): names
    /// C's `dbNameToAddr` resolves — so a SEARCH is answered — but whose
    /// channel is refused at creation. Same base-table-then-own resolution
    /// as `field_list`.
    fn noaccess_names(&self) -> &'static [&'static str];
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
    /// /* compressRecord.c:398-407 */
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

    /// Which of C's six nullable `rset` `get_*` property slots THIS record
    /// type implements — the record's own `#define get_xxx NULL` lines.
    ///
    /// `dbGet` consults the rset to *narrow* the caller's options mask
    /// (`dbAccess.c:336-430`): a NULL slot clears the option bit, so the leaf
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
    /// (`:1409-1411`) clears `udf` synchronously only when the put target is
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
    /// side. Mirrors C `initAo`/`processAo` (devAsynFloat64.c:627-629/:646-649).
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

    /// Apply IVOA=2 ("set outputs to IVOV") semantics: copy the
    /// IVOV value into whatever output staging field the OUT
    /// writeback consumes for this record type. Mirrors the
    /// per-record C `recXxx.c` behaviour:
    ///
    /// - `ao`/`lso`: `OVAL = IVOV; VAL = OVAL`
    /// - `bo`/`busy`/`mbbo`/`mbboDirect`: `RVAL = IVOV; VAL = IVOV`
    /// - `calcout`/`scalcout`: `OVAL = IVOV` (VAL is calc input, not
    ///   touched on invalid-output)
    /// - `dfanout`: `VAL = IVOV` (the broadcast value)
    ///
    /// Default uses [`Record::set_val`] for records whose OUT path
    /// reads VAL only.
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

    /// Whether async processing has completed and put_notify can respond.
    /// Records that return AsyncPendingNotify should return false while
    /// async work is in progress, and true when done.
    /// Default: true (synchronous records are always complete).
    fn is_put_complete(&self) -> bool {
        true
    }

    /// Whether this record should fire its forward link after processing.
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

    /// Notify the record that the OUT-link / device write completed
    /// successfully on this cycle. The framework calls this right after
    /// the actual write so transition-detection state (e.g.
    /// `longout.pval`) can update for the next cycle's
    /// [`Self::should_output`] check. Default: no-op.
    fn on_output_complete(&mut self) {}

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
    /// (correct for binary records, wrong for lsi/lso). Because the
    /// framework posts monitors *after* `process()` — by which point the
    /// record has already committed `oval`/`olen` — the implementation
    /// captures the comparison result during `process()` and returns the
    /// captured flag here, not a live re-comparison.
    fn monitor_value_changed(&self) -> Option<bool> {
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
    /// C motorRecord.cc `monitor()` (3513-3645) computes
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
    /// (`aCalcoutRecord.c:293-297`) — and `monitor()` posts exactly the
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

    /// Fields whose ONLY post path is the record's own per-cycle mark — C never
    /// change-detects them, so a value change alone must post nothing.
    ///
    /// aCalcout's arrays AA..LL are the case. C's `monitor()` compares scalar
    /// A..L against their PA..PL previous values (`aCalcoutRecord.c:1023-1029`)
    /// and OVAL against POVL (`:1039`), but it keeps NO previous copy of an
    /// array and runs no array comparison anywhere: an array posts if and only
    /// if the expression stored into it (AMASK, `afterCalc` `:293-297`) or its
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
    /// `swaitRecord.c::monitor` (646-653) is this shape for its A..L inputs:
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
    /// `recGblResetAlarms` (`:350`) and posts VAL with it, then REASSIGNS
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
    /// and returns 0. The record exists and serves its fields, but `dbProcess`
    /// takes the PACT-active branch on every scan from then on — it never runs
    /// record support again. `caget X.PACT` on a bare `record(sub,"X"){}` reads
    /// 1 on a C IOC.
    ///
    /// PACT is a `dbCommon` field with a single owner
    /// ([`crate::server::record::RecordInstance::enter_pact`] / [`leave_pact`]), so a record cannot
    /// park it from `init_record`. It reports the fact here and the owner
    /// performs the transition, once, at the end of the init passes.
    ///
    /// [`leave_pact`]: crate::server::record::RecordInstance::leave_pact
    fn init_record_parks_pact(&self) -> bool {
        false
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

    /// The channel's native (maximum) element count for `field`, when it
    /// differs from the count of the field's current value.
    ///
    /// C's `cvt_dbaddr` fixes a channel's `no_elements` at the field's buffer
    /// capacity, while `get_array_info` reports the current valid length — so a
    /// client's `ca_element_count` is the capacity even though a GET returns
    /// fewer elements. Return `Some(capacity)` for such a field; `None`
    /// (default) means the channel count is the value's own count.
    ///
    /// - waveform `VAL` → `NELM` (buffer capacity; the value serves `NORD`).
    /// - asyn `BOUT` → `OMAX`, `BINP` → `IMAX` (the `SPC_DBADDR` octet buffers;
    ///   the value serves the transferred byte count `NOWT`/`NORD`).
    fn field_native_count(&self, _field: &str) -> Option<u32> {
        None
    }

    /// Seed the monitor/archive/alarm deadband trackers (MLST/ALST/LALM)
    /// from the initial value at iocInit, called once by the builder after
    /// both `init_record` passes and `post_init_finalize_undef`.
    ///
    /// Every C value record's `init_record` ends with
    /// `prec->mlst = prec->alst = prec->lalm = prec->val`
    /// (e.g. `longinRecord.c:120-122`, `aiRecord.c`), so the first
    /// `monitor()` evaluates `DELTA(mlst, val) > mdel` with `mlst == val`
    /// (= 0) and posts no DBE_VALUE/DBE_LOG event when the value is
    /// unchanged from its initial state. Records expose MLST/ALST/LALM as
    /// plain `f64` fields default-initialised to `0.0`; that default
    /// conflates "never published" with "published 0", so a record
    /// initialised to a *nonzero* value (constant DOL, initial VAL) used
    /// to post a spurious first-cycle update that C does not.
    ///
    /// The default seeds whichever of MLST/ALST/LALM the record actually
    /// serves from its monitor-deadband value (`val` for most records),
    /// making the invariant hold by construction for every record rather
    /// than per-type `init_record` code. It is idempotent for the record
    /// types that already seed inside `init_record`, and a no-op for
    /// records that serve none of these fields.
    fn seed_deadband_tracking(&mut self) {
        let seed = match self.monitor_deadband_value().and_then(|v| v.to_f64()) {
            Some(v) if v.is_finite() => v,
            _ => return,
        };
        for field in ["MLST", "ALST", "LALM"] {
            if self.get_field(field).is_some() {
                let _ = self.put_field(field, EpicsValue::Double(seed));
            }
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

    /// Report this cycle's subroutine status — C `process`'s `status` variable
    /// for `sub`/`aSub`:
    ///
    /// ```c
    /// status = fetch_values(prec);
    /// if (!status) { status = do_sub(prec); prec->val = status; }
    /// ...
    /// if (!status)                      /* aSubRecord.c:232-239 */
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
    /// `busy` is the only record that does this (busyRecord.c:397-400):
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
    /// (`calcoutRecord.c:367-378`, `sCalcoutRecord.c:512-517`,
    /// `aCalcoutRecord.c:534-540`).
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
    ///  * calcout (`calcoutRecord.c:376`), sCalcout (`:516`), aCalcout (`:537`)
    ///    post a literal `DBE_VALUE`.
    ///  * transform (`transformRecord.c:718`) posts `DBE_VALUE | DBE_LOG`.
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
    /// C `selRecord.c::fetch_values` (lines 421-431) fetches ONLY `INP[SELN]`
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
    /// * swait (`swaitRecord.c:401-421`): the simulated read replaces
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
    /// `swait` is the sole exception: `swaitRecord.c:407-421` is a plain
    /// `if (pwait->simm == menuYesNoNO) { … } else { /* SIMULATION MODE */ … }`,
    /// so every non-NO value — legal or not — simulates. It overrides to
    /// `false`.
    ///
    /// Consumed by [`resolve_sim_mode`](crate::server::recgbl::simm::resolve_sim_mode),
    /// the single owner of the SIMM dispatch.
    fn rejects_illegal_sim_mode(&self) -> bool {
        true
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
    /// C `sseqRecord.c::processCallback` (706-793) is the case: the value a
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
    /// it in the record (`sseqRecord.c:202-250`) — so `processCallback` can make
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
    /// pvxs `pvaGetValue` (`pvalink_lset.cpp:330-360`) picks
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
            coerce_put_value(record, name, target, value)?
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
/// An ARRAY destination keeps the coercion path. C reaches it through the same
/// `putString*` routine (`nRequest` elements, parsed one at a time), but this
/// port's array records carry their own element-type conversion, so the row is
/// theirs to own; routing a string here would break the string→`DBF_CHAR[]`
/// carry that `convert_to` provides for them.
pub fn coerce_put_value<R: Record + ?Sized>(
    record: &R,
    field: &str,
    target: DbFieldType,
    value: EpicsValue,
) -> CaResult<EpicsValue> {
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
                );
            }
            // No device menu declared or contributed for this record type: fall
            // through to the generic handling below (unchanged behavior).
        } else if let Some(choices) = super::record_instance::menu_choices_of(record, field) {
            return super::resolve_menu_field_string(field, choices, target, &s.as_str_lossy());
        }
        if target == DbFieldType::Enum {
            return super::resolve_enum_state_string(
                field,
                record.enum_state_strings().as_deref(),
                s,
            );
        }
        let dest_is_array = record.get_field(field).is_some_and(|v| v.is_array());
        if !dest_is_array {
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
                return Ok(EpicsValue::String(cap_string_to_field_size(
                    record, field, s,
                )));
            }
        }
    }
    Ok(value.convert_to(target))
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
/// is published as `VAL` (`aSubRecord.c:223`). Return `Ok(0)` for the
/// normal no-alarm path. `Err(..)` is reserved for an infrastructure
/// failure inside the closure (e.g. a field write error), which aborts
/// processing — it is distinct from a negative status.
pub type SubroutineFn = Box<dyn Fn(&mut dyn Record) -> CaResult<i64> + Send + Sync>;
