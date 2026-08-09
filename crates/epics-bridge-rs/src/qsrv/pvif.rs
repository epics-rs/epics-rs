//! PVIF: PVData Interface — converts between EPICS record state and PVA structures.
//!
//! Corresponds to C++ QSRV's `pvif.h/pvif.cpp` (ScalarBuilder, etc.).

use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::snapshot::{ControlInfo, DisplayInfo, PropertySupport, Snapshot};
use epics_base_rs::types::{EpicsValue, PvString, WallTime};
use epics_pva_rs::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

use crate::convert::{epics_to_pv_field, epics_to_scalar};

/// Field mapping type, corresponding to C++ QSRV PVIFBuilder types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldMapping {
    /// NTScalar/NTScalarArray with full metadata (alarm, timestamp, display, control)
    Scalar,
    /// Value only, no metadata
    Plain,
    /// Alarm + timestamp only, no value
    Meta,
    /// Variant union wrapping
    Any,
    /// Process-only: put triggers record processing, no value transfer
    Proc,
    /// Nested structure placeholder — no channel, defines an intermediate
    /// node in the group schema (pvxs `MappingInfo::Structure`).
    Structure,
    /// Constant value — no channel, set once at subscription setup and
    /// never changes (pvxs `MappingInfo::Const`).
    Const,
}

impl FieldMapping {
    /// The lowercase mapping-type token pvxs prints for this mapping in
    /// `Group::show` / `pvxgl`, mirroring `MappingInfo::name`
    /// (`ioc/typeutils.cpp:65-77`). Used by the QSRV `pvxgl` diagnostic
    /// instead of Rust's `{:?}` (which would emit the capitalized variant
    /// names `Scalar`/`Plain`/… and break pvxs output compatibility).
    pub fn pvxs_name(self) -> &'static str {
        match self {
            FieldMapping::Scalar => "scalar",
            FieldMapping::Plain => "plain",
            FieldMapping::Any => "any",
            FieldMapping::Meta => "meta",
            FieldMapping::Proc => "proc",
            FieldMapping::Structure => "structure",
            FieldMapping::Const => "const",
        }
    }

    /// True iff a client PUT of this member carries a leaf the server
    /// writes to the database. pvxs `IOCSource::put` (iocsource.cpp:
    /// 576-598) writes for `Scalar` / `Plain` / `Any`, and returns without
    /// writing for `Meta`, `Proc`, `Structure` (explicitly "can't write")
    /// and `Const` (the value comes from the group config, never the
    /// client). `Proc` is separately routed to record processing by the
    /// group PUT path.
    pub fn is_client_writable(self) -> bool {
        matches!(
            self,
            FieldMapping::Scalar | FieldMapping::Plain | FieldMapping::Any
        )
    }
}

// ---------------------------------------------------------------------------
// DBE event class -> wire leaves (pvxs `IOCSource::get`)
// ---------------------------------------------------------------------------

/// The leaves a DBE_PROPERTY event marks on a Scalar mapping — exactly the
/// fields pvxs `getProperties` (`pvxs/ioc/iocsource.cpp:252-310`) assigns.
///
/// Delegates to [`epics_pva_rs::nt::property_leaves`], the single owner of
/// that rule. The NATIVE PVA server needs the same answer for the same
/// reason — it serves the same records over the same protocol — and two
/// copies of "which leaves does a QSRV read assign" is the shape that let
/// the native source's NT drift from `nt.cpp` on four counts at once. The
/// input is unchanged: the [`PropertySupport`] mask the DB layer already
/// narrowed to the addressed field.
fn property_leaves(props: PropertySupport) -> Vec<&'static str> {
    epics_pva_rs::nt::property_leaves(props)
}

/// The wire leaves one mapping contributes for one DBE change mask, as
/// field-path suffixes relative to the mapped node (`""` = the node
/// itself, for value-only mappings that carry no metadata sub-structure).
///
/// This is the single owner of pvxs `IOCSource::get`'s per-`UpdateType`
/// assignment (`ioc/iocsource.cpp:312-352`) — the ONLY place that decides
/// which leaves a DB event marks. Both QSRV monitor sources consume it:
/// the group monitor per triggered member, and the single-record monitor
/// at the root of its NT (empty prefix).
///
/// * properties iff `change & Property` — `getProperties`
///   (`iocsource.cpp:252-310`) is gated on `info.type == Scalar`, so only
///   Scalar mappings carry property leaves. It assigns exactly
///   `property_leaves`: the whole `display` / `control` / `valueAlarm`
///   structures are NOT assigned;
/// * `timeStamp` iff `change & (Value | Alarm)` — `getTimeAlarm` always
///   fills the timestamp when it runs, but its `alarm` leaves only under
///   `change & Alarm` (`iocsource.cpp:183-251`);
/// * `value` iff `change & Value`, never for `Meta`. The `value` leaf is
///   *semantic*: for an NTEnum node it is the structure `{index,
///   choices}`, and `marked_changed_bitset` expands a marked structure
///   path to its whole subtree — so a bare `value` mark would re-send the
///   property-only `value.choices`. [`narrow_enum_value_leaves`] resolves
///   that against the concrete value.
///
/// `timeStamp` and `alarm` stay as structure paths because `time_t` and
/// `alarm_t` have no leaf pvxs leaves unassigned on the base this port
/// targets. `getTimeAlarm` (`iocsource.cpp:160-251`) requests
/// `DBR_STATUS | DBR_AMSG | DBR_TIME | DBR_UTAG` and assigns each group under
/// the option the DB actually returned — "as of base 7.0.6 time/alarm
/// meta-data is always available" (`iocsource.cpp:181`), so all three of
/// `alarm.{status,severity,message}` and both of
/// `timeStamp.{secondsPastEpoch,nanoseconds}` land. `timeStamp.userTag` is the
/// one conditional leaf: it is written under `DBR_UTAG` only
/// (`iocsource.cpp:243-250`), a macro that compiles away on a pre-7.0.6 base —
/// which the port's own record layer does not target. So expanding either path
/// yields exactly pvxs's set. The property structures are different — they
/// carry leaves pvxs never touches (below).
pub fn change_leaves(
    mapping: FieldMapping,
    change: EventMask,
    props: PropertySupport,
) -> Vec<&'static str> {
    let mut leaves = Vec::new();
    match mapping {
        FieldMapping::Scalar => {
            if change.intersects(EventMask::PROPERTY) {
                leaves.extend(property_leaves(props));
            }
            if change.intersects(EventMask::VALUE | EventMask::ALARM) {
                leaves.push("timeStamp");
                if change.intersects(EventMask::ALARM) {
                    leaves.push("alarm");
                }
            }
            if change.intersects(EventMask::VALUE) {
                leaves.push("value");
            }
        }
        // `+type:meta`: `alarm` + `timeStamp` only — pvxs has no value leaf
        // for Meta and skips getProperties for it.
        FieldMapping::Meta => {
            if change.intersects(EventMask::VALUE | EventMask::ALARM) {
                leaves.push("timeStamp");
                if change.intersects(EventMask::ALARM) {
                    leaves.push("alarm");
                }
            }
        }
        // Value-only mappings: the mapped node IS the value (pvxs `value =
        // node`), marked whole; no metadata sub-tree exists, so there is
        // nothing to over-mark.
        FieldMapping::Plain | FieldMapping::Any => {
            if change.intersects(EventMask::VALUE) {
                leaves.push("");
            }
        }
        // Const/Structure/Proc carry no runtime event leaf.
        _ => {}
    }
    leaves
}

/// [`change_leaves`] expanded into absolute field paths under `prefix`.
///
/// `prefix` is the mapped node's path in the served structure: a group
/// member's `field_name`, or `""` for a single-record channel whose NT IS
/// the root. An empty prefix with an empty leaf (a `Plain`/`Any` mapping at
/// the root) yields no path; the only mapping pvxs allows at the struct top
/// is `+type:"meta"` (`groupconfigprocessor.cpp:224-231`), whose leaves are
/// the nameable root `alarm` / `timeStamp`.
///
/// A prefix carrying an array SUBSCRIPT (`"a[0].x"`) resolves to the
/// enclosing `StructureArray` field — here `"a"` — and to nothing below it.
/// That is what the mark means on the wire: pvxs assigns into the array
/// ELEMENT, and `Value::mark` (`data.cpp:256-270`) walks the element's
/// enclosing tops, so the only bit that lands in the parent store is the
/// array field's own (`store[a].valid = true`). One bit, and `to_wire_valid`
/// serializes the whole array field. Bits inside an element are not
/// addressable in the root bitset at all, which is why the mark cannot be
/// `"a[0].x.value"`: [`marked_changed_bitset`](epics_pva_rs::pvdata::encode::marked_changed_bitset)
/// builds its candidate paths from `FieldDesc` names and never descends a
/// `StructureArray`, so a subscripted path matched nothing, framed an empty
/// bitset, and every update for an array member was dropped by the enqueue
/// gate.
pub fn change_leaf_paths(
    prefix: &str,
    mapping: FieldMapping,
    change: EventMask,
    props: PropertySupport,
) -> Vec<String> {
    let leaves = change_leaves(mapping, change, props);
    // The member's change classes assign nothing (a Const/Structure/Proc
    // member, a property event on a non-Scalar mapping): no mark, no post.
    if leaves.is_empty() {
        return Vec::new();
    }
    // Subscripted member: the enclosing array field is the whole mark,
    // whatever the mapping would have contributed under it.
    if let Some(array_field) = enclosing_array_field(prefix) {
        return vec![array_field.to_string()];
    }
    leaves
        .into_iter()
        .filter_map(|suffix| match (prefix.is_empty(), suffix.is_empty()) {
            (true, true) => None,
            (true, false) => Some(suffix.to_string()),
            (false, true) => Some(prefix.to_string()),
            (false, false) => Some(format!("{prefix}.{suffix}")),
        })
        .collect()
}

/// The leaves a full READ of one mapping assigns — pvxs
/// `IOCSource::initialize` followed by `IOCSource::get(…, UpdateType::
/// Everything, …)`, the pair every QSRV read runs (`singlesource.cpp:283`,
/// `groupsource.cpp:454-460`). This is what a GET reply / monitor seed
/// frames, and it is a SUBSET of the request mask: pvxs reads into a
/// `cloneEmpty()`, so a leaf nobody assigned never reaches the wire.
///
/// Over `change_leaf_paths(prefix, mapping, VALUE|ALARM|PROPERTY)` it adds:
///
/// * `display.form.choices` — `IOCSource::initialize` (`iocsource.cpp:39-65`)
///   fills the form menu for a `Scalar` mapping. Present in pvxs's own pinned
///   delta (`testqsingle.cpp:129-149`) even though no DBE class assigns it;
/// * `display.form.index` — assigned by the SAME initialize, but ONLY when
///   the mapping's channel addresses the record's VAL field
///   (`if(dbIsValueField(dbChannelFldDes(chan)))`, `iocsource.cpp:53`), which
///   is what `is_value_field` carries here. A channel on `REC.RVAL` leaves the
///   index unassigned, so marking it shipped one changed bit and four bytes
///   pvxs never sends;
/// * a `Const` member's own node — `IOCSource::get` assigns `info.cval`
///   (`iocsource.cpp:319-322`) on a read, where no runtime event ever fires
///   for it.
///
/// `Structure` / `Proc` members assign nothing on a read either (pvxs skips
/// them: `iocsource.cpp:316-317`, and `groupsource.cpp:495` / `:513` `continue`
/// before even reading), so they stay unmarked.
///
/// The seven leaves the port's NT carries but `getProperties` never assigns —
/// `control.minStep`, `valueAlarm.active`, the four `valueAlarm.*Severity`,
/// `valueAlarm.hysteresis` — are absent here exactly as they are absent from
/// pvxs's delta.
pub fn read_leaf_paths(
    prefix: &str,
    mapping: FieldMapping,
    is_value_field: bool,
    props: PropertySupport,
) -> Vec<String> {
    let everything = EventMask::VALUE | EventMask::ALARM | EventMask::PROPERTY;
    let mut paths = change_leaf_paths(prefix, mapping, everything, props);
    match mapping {
        // `IOCSource::initialize` — Scalar only.
        FieldMapping::Scalar => {
            // A subscripted member already collapsed to its enclosing array
            // field, which marks the whole field; nothing to add under it.
            if enclosing_array_field(prefix).is_none() {
                let mut forms = vec!["display.form.choices"];
                if is_value_field {
                    forms.push("display.form.index");
                }
                for form in forms {
                    paths.push(if prefix.is_empty() {
                        form.to_string()
                    } else {
                        format!("{prefix}.{form}")
                    });
                }
            }
        }
        // A const member is assigned on every read, never posted.
        FieldMapping::Const => {
            if let Some(array_field) = enclosing_array_field(prefix) {
                paths.push(array_field.to_string());
            } else if !prefix.is_empty() {
                paths.push(prefix.to_string());
            }
        }
        _ => {}
    }
    paths
}

/// The `StructureArray` field a subscripted member path is nested in — the
/// text before its FIRST `[`. `"a[0].x"` → `"a"`, `"a[0].b[1].c"` → `"a"`
/// (marking propagates up through every enclosing top, and only the
/// outermost array field has a bit in the root store). `None` for a path
/// with no subscript.
fn enclosing_array_field(path: &str) -> Option<&str> {
    path.split_once('[').map(|(head, _)| head)
}

/// Resolve value-shape-dependent leaves in a marked set against the
/// concrete value about to be encoded (so the marks cannot drift from the
/// descriptor).
///
/// [`change_leaves`] emits the *semantic* leaf `value` for a Scalar
/// mapping. For an NTEnum node the `value` child is the structure
/// `{index, choices}`, and `marked_changed_bitset` expands a marked
/// structure path to its whole subtree — so a bare `value` mark would
/// re-send the property-only `value.choices` on every value update. pvxs
/// assigns only `value.index` on a value/alarm event
/// (`iocsource.cpp:107-109,331-351`) and fills `value.choices` solely from
/// `getProperties` on a property event (`iocsource.cpp:278-285`). Rewrite
/// each marked `<node>.value` (or the bare root `value`) whose concrete
/// node is an enum to `…value.index`; plain-scalar `value` leaves and
/// every non-`value` leaf are left untouched. Property-event sets carry
/// `value.choices`, never a bare `value`, so they are unaffected.
pub fn narrow_enum_value_leaves(paths: Vec<String>, root: &PvStructure) -> Vec<String> {
    paths
        .into_iter()
        .map(|path| {
            let base = if path == "value" {
                Some("")
            } else {
                path.strip_suffix(".value")
            };
            match base {
                Some(base) if value_node_is_enum(root, base) => format!("{path}.index"),
                _ => path,
            }
        })
        .collect()
}

/// True iff the node addressed by the dot-separated `base` path (`""` =
/// the root) carries an NTEnum `value` — its `value` child is itself a
/// structure with an `index` leaf.
fn value_node_is_enum(root: &PvStructure, base: &str) -> bool {
    let mut node = root;
    if !base.is_empty() {
        for seg in base.split('.') {
            match node.get_field(seg) {
                Some(PvField::Structure(s)) => node = s,
                _ => return false,
            }
        }
    }
    matches!(
        node.get_field("value"),
        Some(PvField::Structure(v)) if v.get_field("index").is_some()
    )
}

/// NormativeType classification derived from record type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NtType {
    /// ai, ao, longin, longout, stringin, stringout, calc, calcout
    Scalar,
    /// bi, bo, mbbi, mbbo
    Enum,
    /// waveform, compress, histogram
    ScalarArray,
    /// A long-string field (`lsi`/`lso` VAL/OVAL, `printf` VAL): a
    /// `DBF_CHAR` array that semantically holds a NUL-terminated string.
    /// Served as a scalar-string NTScalar — the `CharArray` storage is
    /// decoded to a `pvString` value at the QSRV boundary, matching
    /// pvxs's `form = "String"` view (`ioc/iocsource.cpp:619-643`). This
    /// removes the dual meaning of a `CharArray` snapshot (byte array vs.
    /// string) by making the channel's intent explicit.
    LongString,
}

impl NtType {
    /// Determine NtType from EPICS record type name.
    pub fn from_record_type(rtyp: &str) -> Self {
        match rtyp {
            "bi" | "bo" | "mbbi" | "mbbo" => NtType::Enum,
            "waveform" | "compress" | "histogram" => NtType::ScalarArray,
            _ => NtType::Scalar,
        }
    }
}

/// Choose the NormativeType for a channel bound to a field, the single
/// owner of NT selection for both the single-record path
/// ([`super::channel::BridgeChannel`]) and the QSRV group scalar-member
/// path ([`super::group::GroupChannel`]).
///
/// The NT is chosen from the field's *resolved value's* final DBR/DBF
/// type and element count, mirroring pvxs, which builds every channel
/// prototype from `dbChannelFinalFieldType(chan)`
/// (singlesource.cpp:189-205; group members via `getTypeDefForChannel` /
/// `IOCSource::getChannelValueType`, groupconfigprocessor.cpp:867-974):
/// `DBF_ENUM`/`DBF_MENU`/`DBF_DEVICE` resolve to `DBR_ENUM`
/// (db/dbAccess.c:88-90) and select `NTEnum`, an array field selects
/// `NTScalarArray`, and everything else is `NTScalar`.
///
/// This applies uniformly to `VAL` and every other field — there is no
/// record-type-name short-circuit for `VAL`. A `bi`/`bo`/`mbbi`/`mbbo`
/// **and** `busy` `VAL` all resolve to an enum value and become `NTEnum`;
/// an `aai`/`waveform`/`compress` `VAL` resolves to an array and becomes
/// `NTScalarArray`; a `REC.SCAN` member becomes `NTEnum`; a `BI.DESC`
/// member stays `NTScalar` string. Deriving NT from the resolved value —
/// not from `Record::record_type()` — keeps the advertised NT in lockstep
/// with pvxs `dbChannelFinalFieldType` for every record, including the
/// `DBF_ENUM` records (e.g. `busy`) a record-type name list would omit.
pub(crate) fn nt_type_for_field(value: Option<&EpicsValue>) -> NtType {
    match value {
        // DBF_ENUM/MENU/DEVICE → DBR_ENUM → NTEnum (scalar index +
        // choices). An enum *array* has no scalar-index NTEnum shape,
        // so it falls through to NTScalarArray below.
        Some(EpicsValue::Enum(_)) => NtType::Enum,
        Some(
            EpicsValue::ShortArray(_)
            | EpicsValue::FloatArray(_)
            | EpicsValue::EnumArray(_)
            | EpicsValue::DoubleArray(_)
            | EpicsValue::LongArray(_)
            | EpicsValue::CharArray(_)
            | EpicsValue::StringArray(_)
            | EpicsValue::Int64Array(_)
            | EpicsValue::UInt64Array(_)
            | EpicsValue::UShortArray(_)
            | EpicsValue::ULongArray(_)
            | EpicsValue::UCharArray(_),
        ) => NtType::ScalarArray,
        _ => NtType::Scalar,
    }
}

/// The port's `IOCSource::getChannelValueType` (pvxs
/// `ioc/iocsource.cpp:619-643`): the NT a channel bound to
/// `record.field` serves, INCLUDING the long-string collapse. Single
/// owner — the single-record channel ([`super::channel::BridgeChannel`])
/// and the group scalar-member path ([`super::group::GroupChannel`]) both
/// resolve their NT here, so a long-string field cannot be a string on one
/// surface and a byte array on the other.
///
/// Two ways a channel is a long string, both of which pvxs decides as
/// "final field type is `DBR_CHAR`, the field is an array, and the
/// channel's format is `String`":
///
/// - The record type declares the field one (`Record::long_string_fields`
///   — `lsi`/`lso` VAL/OVAL, `printf` VAL). These are C's `SPC_DBADDR`
///   fields whose `cvt_dbaddr` reports a `DBF_STRING` with `field_size >
///   MAX_STRING_SIZE+1`; pvxs re-views them as a `DBR_CHAR` array and
///   forces `form = "String"` (`ioc/channel.cpp:52-74`).
/// - The record carries `info(Q:form, "String")` and the field is the VAL
///   of a `DBF_CHAR` array — the QSRV long-string idiom for a plain
///   `waveform`/`aai` of CHAR. The info tag applies to VAL only
///   (`dbIsValueField`, `ioc/channel.cpp:43-47`), so another field of the
///   same record stays a byte array.
pub(crate) fn nt_type_for_channel(
    instance: &epics_base_rs::server::record::RecordInstance,
    field_upper: &str,
    resolved: Option<&EpicsValue>,
) -> NtType {
    if instance
        .record
        .long_string_fields()
        .iter()
        .any(|f| f.eq_ignore_ascii_case(field_upper))
    {
        return NtType::LongString;
    }
    // `DBR_CHAR` (not `DBR_UCHAR` — pvxs tests `final_field_type ==
    // DBR_CHAR`) array VAL with the `String` form tag.
    if field_upper.eq_ignore_ascii_case("VAL")
        && matches!(resolved, Some(EpicsValue::CharArray(_)))
        && instance.get_info("Q:form") == Some("String")
    {
        return NtType::LongString;
    }
    nt_type_for_field(resolved)
}

/// The bare leaf a `+type:"plain"` group member serves — **descriptor and
/// value from one decision**, so they cannot disagree on the wire (R18-26).
///
/// pvxs builds a plain member's leaf `Member` straight from the channel's
/// value type — `addMembersForPlainType` (`ioc/groupconfigprocessor.cpp:886-895`)
/// is `TypeDef leaf(IOCSource::getChannelValueType(chan, true))` — and the read
/// path then fills *that same* `Value`: `getArrayValue`
/// (`ioc/iocsource.cpp:132-137`) collapses a `DBR_CHAR` buffer at the NUL
/// exactly when the leaf it is filling is `TypeCode::String`. One type code,
/// two renderings.
///
/// The port had them derived independently — a `match nt_type` in the group's
/// introspection and `epics_to_pv_field` on the read path — so a long-string
/// member advertised `Scalar(Byte)` and shipped `ScalarArray(bytes)`. Here the
/// classification happens once, in [`BareLeaf::of_channel`]; [`BareLeaf::desc`]
/// and [`BareLeaf::value`] are the two renderings of it.
///
/// Note this is NOT `build_field_desc_for_nt`: that emits the full NT wrapper
/// (`NTScalar` with `alarm`/`timeStamp`/…), which is the `+type:"scalar"`
/// shape. A plain member is the bare leaf, no wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BareLeaf {
    /// A `DBR_CHAR` array whose channel format is `String` — pvxs
    /// `TypeCode::String` (`getChannelValueType`, `iocsource.cpp:634-636`).
    LongString,
    /// The value is stored as an `*Array` variant ([`nt_type_for_field`])
    /// — pvxs `valueType.arrayOf()`. Deliberate divergence: pvxs keys
    /// array-ness on `dbChannelFinalElements() != 1`, so C QSRV serves a
    /// `NELM=1` waveform as a scalar; the port pins the shape to the
    /// FTVL storage variant and keeps it NTScalarArray.
    Array(ScalarType),
    /// A scalar-stored value — pvxs `fromDbrType(final_field_type)`.
    Scalar(ScalarType),
}

impl BareLeaf {
    /// Classify the channel bound to `record.field`. `resolved` is the
    /// field's resolved value (record → common → virtual); `field_dbf` is the
    /// fallback DBF type when the field has no resolved value.
    pub(crate) fn of_channel(
        instance: &epics_base_rs::server::record::RecordInstance,
        field_upper: &str,
        resolved: Option<&EpicsValue>,
        field_dbf: epics_base_rs::types::DbFieldType,
    ) -> Self {
        let scalar = crate::convert::dbf_to_scalar_type(
            resolved.map(|v| v.db_field_type()).unwrap_or(field_dbf),
        );
        Self::from_nt(nt_type_for_channel(instance, field_upper, resolved), scalar)
    }

    /// The same classification, from an NT already resolved through
    /// [`nt_type_for_channel`] (the group's introspection pass, which needs
    /// the NT for its `+type:"scalar"` members anyway).
    pub(crate) fn from_nt(nt_type: NtType, scalar: ScalarType) -> Self {
        match nt_type {
            NtType::LongString => BareLeaf::LongString,
            NtType::ScalarArray => BareLeaf::Array(scalar),
            NtType::Scalar | NtType::Enum => BareLeaf::Scalar(scalar),
        }
    }

    /// The leaf's introspection.
    pub(crate) fn desc(self) -> FieldDesc {
        match self {
            BareLeaf::LongString => FieldDesc::Scalar(ScalarType::String),
            BareLeaf::Array(t) => FieldDesc::ScalarArray(t),
            BareLeaf::Scalar(t) => FieldDesc::Scalar(t),
        }
    }

    /// The leaf's value, rendered for the descriptor [`Self::desc`] emits.
    pub(crate) fn value(self, value: &EpicsValue) -> PvField {
        match self {
            BareLeaf::LongString => PvField::Scalar(ScalarValue::String(long_string_value(value))),
            BareLeaf::Array(_) | BareLeaf::Scalar(_) => epics_to_pv_field(value),
        }
    }
}

/// The port's `putLongString` (pvxs `ioc/iocsource.cpp:513-519`): the
/// image a string PUT writes into a long-string channel's CHAR-array
/// storage — the string's bytes plus the NUL terminator, which is what
/// `doDbPut(pDbChannel, DBR_CHAR, str.c_str(), strlen+1)` puts. The
/// terminator counts: it is why C's `NORD` / `LEN` after a long-string
/// PUT is `strlen + 1`.
pub(crate) fn long_string_put_image(s: &PvString) -> EpicsValue {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    EpicsValue::CharArray(bytes)
}

// ---------------------------------------------------------------------------
// Scalar-type classification
// ---------------------------------------------------------------------------

/// `display.form.choices` — the fixed seven-entry menu pvxs publishes for
/// every numeric NTScalar / NTScalarArray (`Q:form` info-tag menu).
///
/// Re-export of [`epics_pva_rs::nt::FORM_CHOICES`]: the native PVA server
/// fills the same menu from the same `IOCSource::initialize` rule, so the
/// list has one owner.
use epics_pva_rs::nt::FORM_CHOICES;

/// Derive the PVA scalar type of an `EpicsValue`, taking the element type
/// for array variants. This is the Rust equivalent of pvxs
/// `TypeCode::scalarOf()` applied to the NTScalar `value` member: metadata
/// limit fields (`display`, `control`, `valueAlarm`) are typed with this
/// scalar type, not hard-coded `double`.
fn value_scalar_type(value: &EpicsValue) -> ScalarType {
    match value {
        EpicsValue::String(_) | EpicsValue::StringArray(_) => ScalarType::String,
        EpicsValue::Short(_) | EpicsValue::ShortArray(_) => ScalarType::Short,
        EpicsValue::Float(_) | EpicsValue::FloatArray(_) => ScalarType::Float,
        EpicsValue::Enum(_) | EpicsValue::EnumArray(_) | EpicsValue::EnumWithChoices { .. } => {
            ScalarType::UShort
        }
        // DBF_CHAR ↔ pvByte (signed), matching `convert::dbf_to_scalar_type`.
        EpicsValue::Char(_) | EpicsValue::CharArray(_) => ScalarType::Byte,
        // DBF_UCHAR ↔ pvUByte (unsigned), the unsigned twin of Char/Byte
        // (pvxs `ioc/typeutils.cpp:34-35` DBR_UCHAR→UInt8).
        EpicsValue::UChar(_) | EpicsValue::UCharArray(_) => ScalarType::UByte,
        EpicsValue::Long(_) | EpicsValue::LongArray(_) => ScalarType::Int,
        EpicsValue::Double(_) | EpicsValue::DoubleArray(_) => ScalarType::Double,
        EpicsValue::Int64(_) | EpicsValue::Int64Array(_) => ScalarType::Long,
        // C `DBF_UINT64` → PVA `ulong`.
        EpicsValue::UInt64(_) | EpicsValue::UInt64Array(_) => ScalarType::ULong,
        // C `DBF_USHORT` → PVA `ushort` / `DBF_ULONG` → PVA `uint`
        // (pvxs `ioc/typeutils.cpp:38-44`).
        EpicsValue::UShort(_) | EpicsValue::UShortArray(_) => ScalarType::UShort,
        EpicsValue::ULong(_) | EpicsValue::ULongArray(_) => ScalarType::UInt,
    }
}

/// True for PVA scalar types that pvxs treats as numeric
/// (`Kind::Integer || Kind::Real`). pvxs only emits `display` limit
/// fields, `control`, and `valueAlarm` for numeric NTScalar values; a
/// string `value` carries `display = {description, units}` only.
///
/// Mirrors pvxs `src/nt.cpp:55` (`const bool isnumeric = ...`).
fn is_numeric_scalar(t: ScalarType) -> bool {
    !matches!(t, ScalarType::String)
}

/// Build a metadata limit `ScalarValue` of the value's scalar type from a
/// stored `f64` limit. pvxs types `display`/`control`/`valueAlarm` limits
/// with `value.scalarOf()`, so an `int32_t[]` waveform gets `int32_t`
/// limits and a `uint64_t` field gets `uint64_t` limits.
///
/// Mirrors pvxs `src/nt.cpp:61-104` (`Member(scalar, "limit*")`).
fn limit_scalar(t: ScalarType, v: f64) -> ScalarValue {
    match t {
        ScalarType::Boolean => ScalarValue::Boolean(v != 0.0),
        ScalarType::Byte => ScalarValue::Byte(v as i8),
        ScalarType::Short => ScalarValue::Short(v as i16),
        ScalarType::Int => ScalarValue::Int(v as i32),
        ScalarType::Long => ScalarValue::Long(v as i64),
        ScalarType::UByte => ScalarValue::UByte(v as u8),
        ScalarType::UShort => ScalarValue::UShort(v as u16),
        ScalarType::UInt => ScalarValue::UInt(v as u32),
        ScalarType::ULong => ScalarValue::ULong(v as u64),
        ScalarType::Float => ScalarValue::Float(v as f32),
        ScalarType::Double => ScalarValue::Double(v),
        // String values are non-numeric and never reach the limit
        // builders; fall back to the textual rendering for completeness.
        ScalarType::String => ScalarValue::String(v.to_string().into()),
    }
}

// ---------------------------------------------------------------------------
// Snapshot → PvStructure conversion
// ---------------------------------------------------------------------------

/// Convert a Snapshot into an NTScalar PvStructure.
///
/// Structure ID: `epics:nt/NTScalar:1.0`
/// Fields: value, alarm, timeStamp, display (optional), control (optional)
pub fn snapshot_to_nt_scalar(snapshot: &Snapshot) -> PvStructure {
    let mut pv = PvStructure::new("epics:nt/NTScalar:1.0");

    // an empty array landing in NTScalar conversion can't yield a real
    // scalar — `epics_to_scalar` will fall back to 0/0.0/"". Surface that as
    // INVALID/UDF so clients don't treat the placeholder as a valid reading.
    let empty_array = is_empty_array(&snapshot.value);

    // metadata limit fields take the value's scalar type, and the
    // metadata field *set* depends on whether that type is numeric.
    let scalar_type = value_scalar_type(&snapshot.value);
    let numeric = is_numeric_scalar(scalar_type);

    // value
    pv.fields.push((
        "value".into(),
        PvField::Scalar(epics_to_scalar(&snapshot.value)),
    ));

    // alarm
    let alarm_struct = if empty_array {
        build_alarm_overlay(snapshot, /*severity*/ 3, /*status (UDF)*/ 17)
    } else {
        build_alarm(snapshot)
    };
    pv.fields
        .push(("alarm".into(), PvField::Structure(alarm_struct)));

    // timeStamp
    pv.fields.push((
        "timeStamp".into(),
        PvField::Structure(build_timestamp(snapshot.timestamp, snapshot.user_tag)),
    ));

    // The NTScalar descriptor advertises `display` for every value plus
    // `control`/`valueAlarm` for numeric ones (`nt.cpp:58-112`), and the
    // port derives its descriptor FROM this value — so every leaf must be
    // built here, whether or not the record type supplies it. Building a
    // leaf is not claiming it: pvxs reads into a `cloneEmpty()` whose
    // descriptor carries the same leaves, and simply never ASSIGNS the ones
    // `dbChannelGet` reported unsupplied, so they reach no client
    // (`iocsource.cpp:263-305`). The port says the same thing by leaving
    // them out of the changed-bitset — see `property_leaves`, which is
    // the one gate, keyed off the snapshot's [`PropertySupport`].
    let disp = snapshot.display.clone().unwrap_or_default();
    pv.fields.push((
        "display".into(),
        PvField::Structure(build_display(&disp, scalar_type, numeric)),
    ));
    if numeric {
        let ctrl = snapshot.control.clone().unwrap_or_default();
        pv.fields.push((
            "control".into(),
            PvField::Structure(build_control(&ctrl, scalar_type)),
        ));
        pv.fields.push((
            "valueAlarm".into(),
            PvField::Structure(build_value_alarm(&disp, scalar_type)),
        ));
    }

    pv
}

/// Convert a Snapshot into an NTEnum PvStructure.
///
/// Structure ID: `epics:nt/NTEnum:1.0`
/// Fields: value{index, choices}, alarm, timeStamp, display{description}
///
/// pvxs's QSRV NTEnum (testqsingle.cpp:174) uses
/// `value.index int32_t` (not ushort) and includes a trailing
/// `display.description` field. Aligning the runtime shape and
/// descriptor with that prevents pvxs clients from seeing
/// a wrong-type index and missing the description field.
pub fn snapshot_to_nt_enum(snapshot: &Snapshot) -> PvStructure {
    let mut pv = PvStructure::new("epics:nt/NTEnum:1.0");

    // value sub-structure with index + choices
    let index = match &snapshot.value {
        EpicsValue::Enum(v) => *v as i32,
        EpicsValue::Short(v) => *v as i32,
        other => other.to_f64().map(|f| f as i32).unwrap_or(0),
    };

    let choices: Vec<ScalarValue> = snapshot
        .enums
        .as_ref()
        .map(|e| {
            e.strings
                .iter()
                .map(|s| ScalarValue::String(s.clone()))
                .collect()
        })
        .unwrap_or_default();

    let mut value_struct = PvStructure::new("enum_t");
    value_struct
        .fields
        .push(("index".into(), PvField::Scalar(ScalarValue::Int(index))));
    value_struct
        .fields
        .push(("choices".into(), PvField::ScalarArray(choices)));

    pv.fields
        .push(("value".into(), PvField::Structure(value_struct)));
    pv.fields
        .push(("alarm".into(), PvField::Structure(build_alarm(snapshot))));
    pv.fields.push((
        "timeStamp".into(),
        PvField::Structure(build_timestamp(snapshot.timestamp, snapshot.user_tag)),
    ));
    // trailing `display.description` is part of pvxs's
    // QSRV NTEnum shape; populate from the DESC field when
    // available, otherwise emit an empty string so the field
    // is present (pvxs always emits the leaf).
    let mut display = PvStructure::new("");
    display.fields.push((
        "description".into(),
        PvField::Scalar(ScalarValue::String(
            snapshot
                .display
                .as_ref()
                .map(|d| d.description.clone())
                .unwrap_or_default(),
        )),
    ));
    pv.fields
        .push(("display".into(), PvField::Structure(display)));

    pv
}

/// Convert a Snapshot into an NTScalarArray PvStructure.
///
/// Structure ID: `epics:nt/NTScalarArray:1.0`
/// Fields: value[], alarm, timeStamp, display, control, valueAlarm.
///
/// pvxs builds NTScalarArray with the *same* `NTScalar` builder as
/// the scalar case — `value.isarray()` only flips the struct id — so a
/// numeric array carries `control` and `valueAlarm` just like a scalar
/// (pvxs `src/nt.cpp:44-112`, confirmed by `test/testqsingle.cpp:354-397`).
pub fn snapshot_to_nt_scalar_array(snapshot: &Snapshot) -> PvStructure {
    let mut pv = PvStructure::new("epics:nt/NTScalarArray:1.0");

    // array metadata limits take the element scalar type.
    let scalar_type = value_scalar_type(&snapshot.value);
    let numeric = is_numeric_scalar(scalar_type);

    // value (array)
    pv.fields
        .push(("value".into(), epics_to_pv_field(&snapshot.value)));

    // alarm
    pv.fields
        .push(("alarm".into(), PvField::Structure(build_alarm(snapshot))));

    // timeStamp
    pv.fields.push((
        "timeStamp".into(),
        PvField::Structure(build_timestamp(snapshot.timestamp, snapshot.user_tag)),
    ));

    // Same descriptor/value consistency rule as the scalar builder:
    // pvxs reuses the NTScalar builder for arrays, so a numeric array
    // value carries `display` plus `control`/`valueAlarm` unconditionally
    // (with descriptor defaults when the record has no metadata, e.g.
    // histogram), and a string array carries `display` only. Emitting
    // them only when the snapshot had metadata diverged from the
    // `getField` descriptor (src/nt.cpp:44-112, iocsource.cpp:254-309).
    let disp = snapshot.display.clone().unwrap_or_default();
    pv.fields.push((
        "display".into(),
        PvField::Structure(build_display(&disp, scalar_type, numeric)),
    ));
    if numeric {
        let ctrl = snapshot.control.clone().unwrap_or_default();
        pv.fields.push((
            "control".into(),
            PvField::Structure(build_control(&ctrl, scalar_type)),
        ));
        pv.fields.push((
            "valueAlarm".into(),
            PvField::Structure(build_value_alarm(&disp, scalar_type)),
        ));
    }

    pv
}

/// Decode a long-string field's stored value into a byte-preserving
/// [`PvString`].
///
/// The record keeps the string as a `DBF_CHAR` `CharArray` (its native CA
/// representation); the QSRV boundary reads it back as the NUL-terminated
/// byte run verbatim — pvxs serves the `DBF_CHAR` `form="String"` view as a
/// `pvString` of the raw bytes up to NUL with no UTF-8 validation
/// (`singlesource.cpp:189-205`), so the bytes pass through unmodified.
pub(crate) fn long_string_value(value: &EpicsValue) -> PvString {
    match value {
        EpicsValue::CharArray(bytes) => {
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            PvString::from_bytes(&bytes[..end])
        }
        EpicsValue::String(s) => s.clone(),
        // Any other shape is unexpected for a long-string channel; fall
        // back to the scalar rendering so a value is still produced.
        other => match epics_to_scalar(other) {
            ScalarValue::String(s) => s,
            sv => sv.to_string().into(),
        },
    }
}

/// Convert a long-string Snapshot into a scalar-string NTScalar.
///
/// pvxs serves a `form = "String"` `DBF_CHAR` view as a `pvString`
/// NTScalar (`ioc/iocsource.cpp:123-137`, `singlesource.cpp:189-205`).
/// We decode the `CharArray` to a `String` and reuse the standard
/// NTScalar builder, which emits the string-scalar metadata set
/// (`display = {description, units}`, no numeric `control`/`valueAlarm`).
fn snapshot_to_nt_long_string(snapshot: &Snapshot) -> PvStructure {
    let mut snap = snapshot.clone();
    snap.value = EpicsValue::String(long_string_value(&snapshot.value));
    snapshot_to_nt_scalar(&snap)
}

/// Convert a Snapshot to the appropriate NormativeType based on NtType.
pub fn snapshot_to_pv_structure(snapshot: &Snapshot, nt_type: NtType) -> PvStructure {
    match nt_type {
        NtType::Scalar => snapshot_to_nt_scalar(snapshot),
        NtType::Enum => snapshot_to_nt_enum(snapshot),
        NtType::ScalarArray => snapshot_to_nt_scalar_array(snapshot),
        NtType::LongString => snapshot_to_nt_long_string(snapshot),
    }
}

// ---------------------------------------------------------------------------
// PvStructure → EpicsValue extraction (for put path)
// ---------------------------------------------------------------------------

/// Extract the primary value from a PvStructure (for put operations).
///
/// For NTScalar: extracts "value" scalar field.
/// For NTEnum: extracts "value.index" as Enum.
/// For NTScalarArray: extracts "value" array.
pub fn pv_structure_to_epics(pv: &PvStructure) -> Option<EpicsValue> {
    let field = pv.get_field("value")?;
    match field {
        PvField::Scalar(sv) => Some(crate::convert::scalar_to_epics(sv)),
        PvField::ScalarArray(_) | PvField::ScalarArrayTyped(_) => {
            crate::convert::pv_field_to_epics(field)
        }
        PvField::Structure(s) => {
            // NTEnum: value is a sub-structure with "index" field
            if let Some(PvField::Scalar(sv)) = s.get_field("index") {
                let idx = crate::convert::scalar_to_epics(sv);
                match idx {
                    EpicsValue::Enum(v) => Some(EpicsValue::Enum(v)),
                    other => Some(EpicsValue::Enum(
                        other.to_f64().map(|f| f as u16).unwrap_or(0),
                    )),
                }
            } else {
                None
            }
        }
        // Other composite shapes are not (yet) represented as a single
        // EpicsValue. Handled out-of-line by the qsrv group/native source.
        PvField::StructureArray(_)
        | PvField::Union { .. }
        | PvField::UnionArray(_)
        | PvField::Variant(_)
        | PvField::VariantArray(_)
        | PvField::Null => None,
    }
}

// ---------------------------------------------------------------------------
// pvRequest field selection
// ---------------------------------------------------------------------------

/// Filter a PvStructure to only include fields requested in pvRequest.
///
/// pvRequest is a PvStructure describing which fields the client wants.
/// If pvRequest has a "field" sub-structure, only those named fields are kept.
/// If pvRequest is empty or has no "field", return the full structure.
///
/// **Nested filtering**: when a requested field is itself a non-empty
/// structure in the request, it acts as a sub-spec that recursively
/// filters the corresponding PvStructure field. An empty structure
/// in the request means "include this field entirely".
///
/// Example:
/// ```text
/// request: { field: { value: {}, alarm: { severity: {} } } }
/// pv:      { value: 42, alarm: {severity: 0, status: 0, message: ""}, timeStamp: {...} }
/// result:  { value: 42, alarm: { severity: 0 } }
/// ```
///
/// Corresponds to C++ QSRV's pvRequest mask handling.
pub fn filter_by_request(pv: &PvStructure, request: &PvStructure) -> PvStructure {
    // Look for "field" sub-structure in request
    let field_spec = match request.get_field("field") {
        Some(PvField::Structure(s)) => s,
        _ => return pv.clone(), // No field filter, return everything
    };

    filter_by_spec(pv, field_spec)
}

/// Recursively filter `pv` by the given field spec.
///
/// The spec is a PvStructure where each child indicates which sub-field
/// to keep. An empty child structure means "include this field entirely".
/// A non-empty child structure recursively filters that sub-field.
fn filter_by_spec(pv: &PvStructure, spec: &PvStructure) -> PvStructure {
    // Empty spec → return everything (passthrough)
    if spec.fields.is_empty() {
        return pv.clone();
    }

    let mut result = PvStructure::new(&pv.struct_id);
    for (name, value) in &pv.fields {
        let sub_spec = match spec.get_field(name) {
            Some(s) => s,
            None => continue, // Field not in spec, skip
        };

        match (sub_spec, value) {
            // Both are structures: recurse
            (PvField::Structure(s_spec), PvField::Structure(s_val)) => {
                result.fields.push((
                    name.clone(),
                    PvField::Structure(filter_by_spec(s_val, s_spec)),
                ));
            }
            // Spec is structure but value is scalar/array: include as-is
            // (the spec just selects the field, doesn't restructure it)
            (_, _) => {
                result.fields.push((name.clone(), value.clone()));
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// FieldDesc builders (type introspection, no values)
// ---------------------------------------------------------------------------

/// Build a PVA FieldDesc for an NTScalar with the given scalar type.
///
/// `display`/`control`/`valueAlarm` limits take `scalar_type`, and
/// `control`/`valueAlarm` are omitted for non-numeric (string) values —
/// matching pvxs `NTScalar::build` (`src/nt.cpp:37-114`).
pub fn build_nt_scalar_desc(scalar_type: ScalarType) -> FieldDesc {
    let numeric = is_numeric_scalar(scalar_type);
    let mut fields = vec![
        ("value".into(), FieldDesc::Scalar(scalar_type)),
        ("alarm".into(), alarm_desc()),
        ("timeStamp".into(), timestamp_desc()),
        ("display".into(), display_desc(scalar_type, numeric)),
    ];
    if numeric {
        fields.push(("control".into(), control_desc(scalar_type)));
        fields.push(("valueAlarm".into(), value_alarm_desc(scalar_type)));
    }
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalar:1.0".into(),
        fields,
    }
}

/// Build a PVA FieldDesc for an NTEnum.
///
/// `value.index` is `Int` (matches pvxs QSRV
/// `testqsingle.cpp:174` `value.index int32_t`); the shape
/// includes a trailing `display.description` field.
pub fn build_nt_enum_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "epics:nt/NTEnum:1.0".into(),
        fields: vec![
            (
                "value".into(),
                FieldDesc::Structure {
                    struct_id: "enum_t".into(),
                    fields: vec![
                        ("index".into(), FieldDesc::Scalar(ScalarType::Int)),
                        ("choices".into(), FieldDesc::ScalarArray(ScalarType::String)),
                    ],
                },
            ),
            ("alarm".into(), alarm_desc()),
            ("timeStamp".into(), timestamp_desc()),
            (
                "display".into(),
                FieldDesc::Structure {
                    struct_id: String::new(),
                    fields: vec![("description".into(), FieldDesc::Scalar(ScalarType::String))],
                },
            ),
        ],
    }
}

/// Build a PVA FieldDesc for an NTScalarArray with the given element type.
///
/// pvxs reuses the `NTScalar` builder for arrays, so a numeric
/// array descriptor carries `control` and `valueAlarm` with element-typed
/// limits (pvxs `src/nt.cpp:44-112`, `test/testqsingle.cpp:354-397`).
pub fn build_nt_scalar_array_desc(element_type: ScalarType) -> FieldDesc {
    let numeric = is_numeric_scalar(element_type);
    let mut fields = vec![
        ("value".into(), FieldDesc::ScalarArray(element_type)),
        ("alarm".into(), alarm_desc()),
        ("timeStamp".into(), timestamp_desc()),
        ("display".into(), display_desc(element_type, numeric)),
    ];
    if numeric {
        fields.push(("control".into(), control_desc(element_type)));
        fields.push(("valueAlarm".into(), value_alarm_desc(element_type)));
    }
    FieldDesc::Structure {
        struct_id: "epics:nt/NTScalarArray:1.0".into(),
        fields,
    }
}

/// Build the appropriate FieldDesc based on NtType and scalar type.
pub fn build_field_desc_for_nt(nt_type: NtType, scalar_type: ScalarType) -> FieldDesc {
    match nt_type {
        NtType::Scalar => build_nt_scalar_desc(scalar_type),
        NtType::Enum => build_nt_enum_desc(),
        NtType::ScalarArray => build_nt_scalar_array_desc(scalar_type),
        // A long-string field is advertised as a scalar-string NTScalar,
        // independent of the bound field's `DBF_CHAR` storage type, so
        // the descriptor matches the `pvString` value the GET path emits.
        NtType::LongString => build_nt_scalar_desc(ScalarType::String),
    }
}

// ---------------------------------------------------------------------------
// Helper builders
// ---------------------------------------------------------------------------

fn build_alarm(snapshot: &Snapshot) -> PvStructure {
    let mut alarm = PvStructure::new("alarm_t");
    alarm.fields.push((
        "severity".into(),
        PvField::Scalar(ScalarValue::Int(snapshot.alarm.severity as i32)),
    ));
    // PVA alarm.status is the status CLASS, and alarm.message is
    // the condition string (pvxs iocsource.cpp:187-236) — not the raw
    // condition code / severity name.
    alarm.fields.push((
        "status".into(),
        PvField::Scalar(ScalarValue::Int(alarm_status_class(snapshot.alarm.status))),
    ));
    alarm.fields.push((
        "message".into(),
        PvField::Scalar(ScalarValue::String(
            alarm_message(snapshot, snapshot.alarm.status).into(),
        )),
    ));
    alarm
}

/// pvxs `iocsource.cpp:230-236`, exactly: a non-empty carried amsg (the
/// record's `common.amsg`) wins; else the alarm condition string for a
/// non-zero `status`; else "". Only mbboDirect sets a UDF amsg ("UDFS",
/// `mbboDirectRecord.c:191`); every other record raises UDF via plain
/// `recGblSetSevr` (empty namsg), so its empty amsg falls through here to
/// the "UDF" condition string — the same rule as
/// `epics_pva_rs::server::native_source::build_alarm`.
///
/// `status` is passed separately so the overlay builder can supply its
/// escalated `eff_status` for the condition-string fallback while still
/// preferring the record's own amsg (the amsg belongs to the record
/// regardless of the severity overlay). `alarm_condition_string` already
/// maps NO_ALARM / out-of-range to "".
fn alarm_message(snapshot: &Snapshot, status: u16) -> String {
    if !snapshot.alarm.amsg.is_empty() {
        snapshot.alarm.amsg.clone()
    } else {
        alarm_condition_string(status).to_string()
    }
}

/// Build an alarm overlay that escalates severity/status without losing the
/// underlying record context. Used when a structural mismatch (e.g. empty
/// array fed to NTScalar) makes the value field unreliable.
fn build_alarm_overlay(snapshot: &Snapshot, severity: u16, status: u16) -> PvStructure {
    let eff_severity = snapshot.alarm.severity.max(severity);
    let eff_status = if snapshot.alarm.status == 0 {
        status
    } else {
        snapshot.alarm.status
    };
    let mut alarm = PvStructure::new("alarm_t");
    alarm.fields.push((
        "severity".into(),
        PvField::Scalar(ScalarValue::Int(eff_severity as i32)),
    ));
    // status CLASS + amsg-or-condition-string message (pvxs
    // iocsource.cpp:187-236), using the escalated `eff_status` for the
    // fallback but still preferring the record's own amsg.
    alarm.fields.push((
        "status".into(),
        PvField::Scalar(ScalarValue::Int(alarm_status_class(eff_status))),
    ));
    alarm.fields.push((
        "message".into(),
        PvField::Scalar(ScalarValue::String(
            alarm_message(snapshot, eff_status).into(),
        )),
    ));
    alarm
}

/// True when `value` is an array variant containing zero elements.
fn is_empty_array(value: &EpicsValue) -> bool {
    matches!(
        value,
        EpicsValue::ShortArray(a) if a.is_empty()
    ) || matches!(
        value,
        EpicsValue::FloatArray(a) if a.is_empty()
    ) || matches!(
        value,
        EpicsValue::EnumArray(a) if a.is_empty()
    ) || matches!(
        value,
        EpicsValue::DoubleArray(a) if a.is_empty()
    ) || matches!(
        value,
        EpicsValue::LongArray(a) if a.is_empty()
    ) || matches!(
        value,
        EpicsValue::CharArray(a) if a.is_empty()
    ) || matches!(
        value,
        EpicsValue::UCharArray(a) if a.is_empty()
    ) || matches!(
        value,
        EpicsValue::UShortArray(a) if a.is_empty()
    ) || matches!(
        value,
        EpicsValue::ULongArray(a) if a.is_empty()
    ) || matches!(
        value,
        EpicsValue::StringArray(a) if a.is_empty()
    )
}

fn build_timestamp(time: WallTime, user_tag: i32) -> PvStructure {
    let mut ts = PvStructure::new("time_t");
    let dur = time.since_unix_epoch();
    let (secs, nanos) = (dur.as_secs() as i64, dur.subsec_nanos() as i32);
    // PVA Normative Types define secondsPastEpoch as POSIX/UNIX epoch
    // (pvxs iocsource.cpp:240 adds POSIX_TIME_AT_EPICS_EPOCH to convert
    // from internal EPICS epoch). `WallTime` is already UNIX-based,
    // so no conversion is needed here.
    ts.fields.push((
        "secondsPastEpoch".into(),
        PvField::Scalar(ScalarValue::Long(secs)),
    ));
    ts.fields.push((
        "nanoseconds".into(),
        PvField::Scalar(ScalarValue::Int(nanos)),
    ));
    ts.fields.push((
        "userTag".into(),
        PvField::Scalar(ScalarValue::Int(user_tag)),
    ));
    ts
}

/// Build the `enum_t` sub-structure for `display.form`.
///
/// pvxs models `display.form` as an `enum_t` (`{int32 index,
/// string[] choices}`), not a scalar `int`. `index` is the `Q:form` info
/// tag value; `choices` is the fixed seven-entry menu.
/// Mirrors pvxs `src/nt.cpp:71-74` and `ioc/iocsource.cpp:42-62`.
fn build_form(form: i16) -> PvStructure {
    let mut f = PvStructure::new("enum_t");
    f.fields.push((
        "index".into(),
        PvField::Scalar(ScalarValue::Int(form as i32)),
    ));
    f.fields.push((
        "choices".into(),
        PvField::ScalarArray(
            FORM_CHOICES
                .iter()
                .map(|s| ScalarValue::String((*s).to_string().into()))
                .collect(),
        ),
    ));
    f
}

/// Build the `display` sub-structure.
///
/// for numeric values pvxs emits `{limitLow, limitHigh,
/// description, units, precision, form}` with limits typed as the value's
/// scalar type and `form` as an `enum_t`; for non-numeric (string) values
/// only `{description, units}` is emitted (pvxs `src/nt.cpp:58-85`).
fn build_display(disp: &DisplayInfo, scalar_type: ScalarType, numeric: bool) -> PvStructure {
    // pvxs builds `display`/`control`/`valueAlarm` with the 2-arg
    // `members::Struct(name, children)` form (`src/nt.cpp:60`/`:89`/`:99`),
    // which leaves `id = std::string()` (empty) — only `alarm`/`timeStamp`
    // (and NTEnum's `value`) use the 3-arg form with an explicit id. An
    // `id`-carrying struct serializes the id as a length-prefixed string
    // (`encode_structure_body`), so a non-empty `display_t` diverged from
    // pvxs byte-for-byte on every NTScalar/NTScalarArray introspection.
    let mut d = PvStructure::new("");
    if numeric {
        d.fields.push((
            "limitLow".into(),
            PvField::Scalar(limit_scalar(scalar_type, disp.lower_disp_limit)),
        ));
        d.fields.push((
            "limitHigh".into(),
            PvField::Scalar(limit_scalar(scalar_type, disp.upper_disp_limit)),
        ));
    }
    d.fields.push((
        "description".into(),
        PvField::Scalar(ScalarValue::String(disp.description.clone())),
    ));
    d.fields.push((
        "units".into(),
        PvField::Scalar(ScalarValue::String(disp.units.clone())),
    ));
    if numeric {
        d.fields.push((
            "precision".into(),
            PvField::Scalar(ScalarValue::Int(disp.precision as i32)),
        ));
        d.fields
            .push(("form".into(), PvField::Structure(build_form(disp.form))));
    }
    d
}

fn build_control(ctrl: &ControlInfo, scalar_type: ScalarType) -> PvStructure {
    // Anonymous id — pvxs `Struct("control", {…})` (`src/nt.cpp:89`), see
    // `build_display`.
    let mut c = PvStructure::new("");
    c.fields.push((
        "limitLow".into(),
        PvField::Scalar(limit_scalar(scalar_type, ctrl.lower_ctrl_limit)),
    ));
    c.fields.push((
        "limitHigh".into(),
        PvField::Scalar(limit_scalar(scalar_type, ctrl.upper_ctrl_limit)),
    ));
    c.fields.push((
        "minStep".into(),
        PvField::Scalar(limit_scalar(scalar_type, 0.0)),
    ));
    c
}

fn alarm_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "alarm_t".into(),
        fields: vec![
            ("severity".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("status".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("message".into(), FieldDesc::Scalar(ScalarType::String)),
        ],
    }
}

fn timestamp_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "time_t".into(),
        fields: vec![
            (
                "secondsPastEpoch".into(),
                FieldDesc::Scalar(ScalarType::Long),
            ),
            ("nanoseconds".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("userTag".into(), FieldDesc::Scalar(ScalarType::Int)),
        ],
    }
}

/// FieldDesc for `display.form` — an `enum_t` (`{int32 index, string[]
/// choices}`). Mirrors pvxs `src/nt.cpp:71-74`.
fn form_desc() -> FieldDesc {
    FieldDesc::Structure {
        struct_id: "enum_t".into(),
        fields: vec![
            ("index".into(), FieldDesc::Scalar(ScalarType::Int)),
            ("choices".into(), FieldDesc::ScalarArray(ScalarType::String)),
        ],
    }
}

fn display_desc(scalar_type: ScalarType, numeric: bool) -> FieldDesc {
    let mut fields: Vec<(String, FieldDesc)> = Vec::new();
    if numeric {
        fields.push(("limitLow".into(), FieldDesc::Scalar(scalar_type)));
        fields.push(("limitHigh".into(), FieldDesc::Scalar(scalar_type)));
    }
    fields.push(("description".into(), FieldDesc::Scalar(ScalarType::String)));
    fields.push(("units".into(), FieldDesc::Scalar(ScalarType::String)));
    if numeric {
        fields.push(("precision".into(), FieldDesc::Scalar(ScalarType::Int)));
        fields.push(("form".into(), form_desc()));
    }
    FieldDesc::Structure {
        // Anonymous id to match the value builder / pvxs (see `build_display`).
        struct_id: String::new(),
        fields,
    }
}

fn control_desc(scalar_type: ScalarType) -> FieldDesc {
    FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![
            ("limitLow".into(), FieldDesc::Scalar(scalar_type)),
            ("limitHigh".into(), FieldDesc::Scalar(scalar_type)),
            ("minStep".into(), FieldDesc::Scalar(scalar_type)),
        ],
    }
}

/// Build the `valueAlarm` sub-structure.
///
/// pvxs `valueAlarm` carries the full field set — `active` (bool),
/// the four alarm/warning limits typed as the value's scalar type, the
/// four `*Severity` fields (int32), and `hysteresis` (float64). The four
/// `*Severity` fields and `active`/`hysteresis` are not represented in the
/// EPICS `DisplayInfo` metadata, so they default to 0 / false / 0.0 — the
/// same values pvxs emits when QSRV does not populate them
/// (`test/testqsingle.cpp:116-127`). Mirrors pvxs `src/nt.cpp:97-112`.
fn build_value_alarm(disp: &DisplayInfo, scalar_type: ScalarType) -> PvStructure {
    // Anonymous id — pvxs `Struct("valueAlarm", {…})` (`src/nt.cpp:99`), see
    // `build_display`.
    let mut va = PvStructure::new("");
    va.fields.push((
        "active".into(),
        PvField::Scalar(ScalarValue::Boolean(false)),
    ));
    va.fields.push((
        "lowAlarmLimit".into(),
        PvField::Scalar(limit_scalar(scalar_type, disp.lower_alarm_limit)),
    ));
    va.fields.push((
        "lowWarningLimit".into(),
        PvField::Scalar(limit_scalar(scalar_type, disp.lower_warning_limit)),
    ));
    va.fields.push((
        "highWarningLimit".into(),
        PvField::Scalar(limit_scalar(scalar_type, disp.upper_warning_limit)),
    ));
    va.fields.push((
        "highAlarmLimit".into(),
        PvField::Scalar(limit_scalar(scalar_type, disp.upper_alarm_limit)),
    ));
    va.fields.push((
        "lowAlarmSeverity".into(),
        PvField::Scalar(ScalarValue::Int(0)),
    ));
    va.fields.push((
        "lowWarningSeverity".into(),
        PvField::Scalar(ScalarValue::Int(0)),
    ));
    va.fields.push((
        "highWarningSeverity".into(),
        PvField::Scalar(ScalarValue::Int(0)),
    ));
    va.fields.push((
        "highAlarmSeverity".into(),
        PvField::Scalar(ScalarValue::Int(0)),
    ));
    va.fields.push((
        "hysteresis".into(),
        PvField::Scalar(ScalarValue::Double(0.0)),
    ));
    va
}

fn value_alarm_desc(scalar_type: ScalarType) -> FieldDesc {
    FieldDesc::Structure {
        struct_id: String::new(),
        fields: vec![
            ("active".into(), FieldDesc::Scalar(ScalarType::Boolean)),
            ("lowAlarmLimit".into(), FieldDesc::Scalar(scalar_type)),
            ("lowWarningLimit".into(), FieldDesc::Scalar(scalar_type)),
            ("highWarningLimit".into(), FieldDesc::Scalar(scalar_type)),
            ("highAlarmLimit".into(), FieldDesc::Scalar(scalar_type)),
            (
                "lowAlarmSeverity".into(),
                FieldDesc::Scalar(ScalarType::Int),
            ),
            (
                "lowWarningSeverity".into(),
                FieldDesc::Scalar(ScalarType::Int),
            ),
            (
                "highWarningSeverity".into(),
                FieldDesc::Scalar(ScalarType::Int),
            ),
            (
                "highAlarmSeverity".into(),
                FieldDesc::Scalar(ScalarType::Int),
            ),
            ("hysteresis".into(), FieldDesc::Scalar(ScalarType::Double)),
        ],
    }
}

/// map a raw EPICS `epicsAlarmCondition` (0–21, `alarm.h`) to the
/// PVA `alarm_t.status` **status class**. PVA carries the alarm *class*
/// (NONE/DEVICE/DRIVER/RECORD/DB/UNDEFINED), not the raw DB condition
/// code — mirrors pvxs `ioc/iocsource.cpp:187-223`.
pub(crate) fn alarm_status_class(condition: u16) -> i32 {
    use epics_base_rs::server::recgbl::alarm_status as a;
    match condition {
        a::NO_ALARM => 0, // NONE
        a::READ_ALARM
        | a::WRITE_ALARM
        | a::HIHI_ALARM
        | a::HIGH_ALARM
        | a::LOLO_ALARM
        | a::LOW_ALARM
        | a::STATE_ALARM
        | a::COS_ALARM
        | a::HW_LIMIT_ALARM => 1, // DEVICE
        a::COMM_ALARM | a::TIMEOUT_ALARM | a::UDF_ALARM => 2, // DRIVER
        a::CALC_ALARM | a::SCAN_ALARM | a::LINK_ALARM | a::SOFT_ALARM | a::BAD_SUB_ALARM => 3, // RECORD
        a::DISABLE_ALARM | a::SIMM_ALARM | a::READ_ACCESS_ALARM | a::WRITE_ACCESS_ALARM => 4,  // DB
        _ => 6, // UNDEFINED
    }
}

/// the EPICS alarm **condition string** for a raw
/// `epicsAlarmCondition` (`epicsAlarmConditionStrings`,
/// `libcom/src/misc/alarmString.c:27-50`), or `""` for `NO_ALARM` /
/// out-of-range. pvxs uses this for `alarm.message`
/// (`ioc/iocsource.cpp:225-236`).
pub(crate) fn alarm_condition_string(condition: u16) -> &'static str {
    use epics_base_rs::server::recgbl::alarm_status as a;
    match condition {
        a::NO_ALARM => "",
        a::READ_ALARM => "READ",
        a::WRITE_ALARM => "WRITE",
        a::HIHI_ALARM => "HIHI",
        a::HIGH_ALARM => "HIGH",
        a::LOLO_ALARM => "LOLO",
        a::LOW_ALARM => "LOW",
        a::STATE_ALARM => "STATE",
        a::COS_ALARM => "COS",
        a::COMM_ALARM => "COMM",
        a::TIMEOUT_ALARM => "TIMEOUT",
        a::HW_LIMIT_ALARM => "HWLIMIT",
        a::CALC_ALARM => "CALC",
        a::SCAN_ALARM => "SCAN",
        a::LINK_ALARM => "LINK",
        a::SOFT_ALARM => "SOFT",
        a::BAD_SUB_ALARM => "BAD_SUB",
        a::UDF_ALARM => "UDF",
        a::DISABLE_ALARM => "DISABLE",
        a::SIMM_ALARM => "SIMM",
        a::READ_ACCESS_ALARM => "READ_ACCESS",
        a::WRITE_ACCESS_ALARM => "WRITE_ACCESS",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_base_rs::server::snapshot::{EnumInfo, Snapshot};
    use std::time::UNIX_EPOCH;

    /// The mask an `ai`-class channel resolves to: every numeric property
    /// supplied, no enum strings. The cases below fix the CHANGE-CLASS
    /// narrowing (which leaves a value / property / read event assigns); the
    /// rset narrowing on top of it has its own boundary cases below.
    const NUMERIC_PROPS: PropertySupport = PropertySupport::NUMERIC;

    fn test_snapshot(value: EpicsValue) -> Snapshot {
        let mut snap = Snapshot::new(value, 0, 0, UNIX_EPOCH);
        snap.display = Some(DisplayInfo {
            units: "degC".into(),
            precision: 3,
            upper_disp_limit: 100.0,
            lower_disp_limit: 0.0,
            upper_alarm_limit: 90.0,
            upper_warning_limit: 80.0,
            lower_warning_limit: 10.0,
            lower_alarm_limit: 5.0,
            ..Default::default()
        });
        snap.control = Some(ControlInfo {
            upper_ctrl_limit: 100.0,
            lower_ctrl_limit: 0.0,
        });
        snap
    }

    /// A long-string `DBF_CHAR` field (`lsi`/`lso` VAL, `printf` VAL) is
    /// served as a scalar-string NTScalar: the `CharArray` storage is
    /// decoded to a `pvString` value, and the descriptor advertises
    /// `value` as `String`, not `pvByte`. Before the fix the byte-array
    /// snapshot was collapsed to a single `pvByte` (the first byte).
    #[test]
    fn long_string_field_builds_string_ntscalar() {
        let text = "abc"; // 3 bytes; byte-collapse would yield 97 ('a')
        let snap = Snapshot::new(
            EpicsValue::CharArray(text.as_bytes().to_vec()),
            0,
            0,
            UNIX_EPOCH,
        );
        let pv = snapshot_to_pv_structure(&snap, NtType::LongString);
        assert_eq!(pv.struct_id, "epics:nt/NTScalar:1.0");
        match pv.get_field("value") {
            Some(PvField::Scalar(ScalarValue::String(s))) => assert_eq!(s, text),
            other => panic!("expected scalar string value, got {other:?}"),
        }

        // Descriptor must advertise a string `value`, matching the GET
        // value shape (no pvByte, no numeric control/valueAlarm).
        let desc = build_field_desc_for_nt(NtType::LongString, ScalarType::Byte);
        match desc {
            FieldDesc::Structure { struct_id, fields } => {
                assert_eq!(struct_id, "epics:nt/NTScalar:1.0");
                let value_desc = fields.iter().find(|(n, _)| n == "value").map(|(_, d)| d);
                assert!(
                    matches!(value_desc, Some(FieldDesc::Scalar(ScalarType::String))),
                    "value must be advertised as pvString, got {value_desc:?}"
                );
                // String scalars carry no numeric control/valueAlarm.
                assert!(!fields.iter().any(|(n, _)| n == "control"));
                assert!(!fields.iter().any(|(n, _)| n == "valueAlarm"));
            }
            other => panic!("expected NTScalar structure descriptor, got {other:?}"),
        }
    }

    /// A NUL terminator inside the `CharArray` truncates the decoded
    /// string, mirroring the record's own `put_field` decode.
    #[test]
    fn long_string_value_stops_at_nul() {
        let snap = Snapshot::new(
            EpicsValue::CharArray(b"hi\0junk".to_vec()),
            0,
            0,
            UNIX_EPOCH,
        );
        let pv = snapshot_to_pv_structure(&snap, NtType::LongString);
        match pv.get_field("value") {
            Some(PvField::Scalar(ScalarValue::String(s))) => assert_eq!(s, "hi"),
            other => panic!("expected scalar string value, got {other:?}"),
        }
    }

    /// PVA-89: a long-string (`DBF_CHAR` `form="String"`) gateway
    /// pass-through must preserve non-UTF-8 bytes verbatim. pvxs reads the
    /// raw byte run up to NUL with no UTF-8 validation
    /// (`singlesource.cpp:189-205`); the port keeps the bytes in
    /// `PvString::from_bytes`, so a Latin-1 / binary CharArray reaches the
    /// PVA client unmangled rather than as U+FFFD replacements.
    #[test]
    fn long_string_value_preserves_non_utf8_bytes() {
        // 0xFF / 0x80 are invalid standalone UTF-8; the byte path keeps
        // each as one byte. A NUL still truncates (C buffer semantics).
        let snap = Snapshot::new(
            EpicsValue::CharArray(vec![0xff, 0x80, b'a', 0xc3, 0x28, 0x00, b'x']),
            0,
            0,
            UNIX_EPOCH,
        );
        let pv = snapshot_to_pv_structure(&snap, NtType::LongString);
        match pv.get_field("value") {
            Some(PvField::Scalar(ScalarValue::String(s))) => assert_eq!(
                s.as_bytes(),
                &[0xff, 0x80, b'a', 0xc3, 0x28],
                "non-UTF-8 long-string bytes must pass through verbatim, up to NUL"
            ),
            other => panic!("expected scalar string value, got {other:?}"),
        }
    }

    /// An `FTVL=UCHAR` waveform (`UCharArray`) is served as a PVA
    /// `ubyte[]` NTScalarArray — unsigned, unlike an `FTVL=CHAR` waveform's
    /// signed `byte[]` (pvxs `ioc/typeutils.cpp:34-35` DBR_UCHAR→UInt8).
    /// Element 200 must stay 200 (unsigned), not wrap to −56 as a signed
    /// byte would.
    #[test]
    fn q14_uchar_waveform_serves_as_ubyte_array() {
        let snap = Snapshot::new(
            EpicsValue::UCharArray(vec![0u8, 1, 200, 0xFF]),
            0,
            0,
            UNIX_EPOCH,
        );

        // The metadata scalar type is pvUByte (unsigned), so limits/control
        // are typed ubyte, not signed byte.
        assert_eq!(value_scalar_type(&snap.value), ScalarType::UByte);

        let pv = snapshot_to_pv_structure(&snap, NtType::ScalarArray);
        assert_eq!(pv.struct_id, "epics:nt/NTScalarArray:1.0");
        match pv.get_field("value") {
            Some(PvField::ScalarArray(arr)) => {
                assert!(
                    matches!(arr[2], ScalarValue::UByte(200)),
                    "element 200 must stay unsigned 200, got {:?}",
                    arr[2]
                );
                assert!(matches!(arr[3], ScalarValue::UByte(255)));
            }
            other => panic!("expected ubyte ScalarArray value, got {other:?}"),
        }

        // The descriptor advertises `value` as ubyte[], matching the GET.
        let desc = build_field_desc_for_nt(NtType::ScalarArray, ScalarType::UByte);
        if let FieldDesc::Structure { fields, .. } = desc {
            let value_desc = fields.iter().find(|(n, _)| n == "value").map(|(_, d)| d);
            assert!(
                matches!(value_desc, Some(FieldDesc::ScalarArray(ScalarType::UByte))),
                "value must be advertised as ubyte[], got {value_desc:?}"
            );
        } else {
            panic!("expected NTScalarArray structure descriptor");
        }
    }

    fn alarm_scalar_int(s: &PvStructure, name: &str) -> i32 {
        match s.fields.iter().find(|(n, _)| n == name).map(|(_, f)| f) {
            Some(PvField::Scalar(ScalarValue::Int(v))) => *v,
            other => panic!("expected Int field {name}, got {other:?}"),
        }
    }

    fn alarm_scalar_str(s: &PvStructure, name: &str) -> String {
        match s.fields.iter().find(|(n, _)| n == name).map(|(_, f)| f) {
            Some(PvField::Scalar(ScalarValue::String(v))) => v.as_str_lossy().into_owned(),
            other => panic!("expected String field {name}, got {other:?}"),
        }
    }

    #[test]
    fn br_r62_status_class_mapping() {
        // Raw epicsAlarmCondition -> PVA status class (pvxs iocsource.cpp).
        use epics_base_rs::server::recgbl::alarm_status as a;
        assert_eq!(alarm_status_class(a::NO_ALARM), 0); // NONE
        assert_eq!(alarm_status_class(a::HIGH_ALARM), 1); // DEVICE
        assert_eq!(alarm_status_class(a::HW_LIMIT_ALARM), 1); // DEVICE
        assert_eq!(alarm_status_class(a::COMM_ALARM), 2); // DRIVER
        assert_eq!(alarm_status_class(a::UDF_ALARM), 2); // DRIVER
        assert_eq!(alarm_status_class(a::LINK_ALARM), 3); // RECORD
        assert_eq!(alarm_status_class(a::SCAN_ALARM), 3); // RECORD
        assert_eq!(alarm_status_class(a::WRITE_ACCESS_ALARM), 4); // DB
        assert_eq!(alarm_status_class(a::SIMM_ALARM), 4); // DB
        assert_eq!(alarm_status_class(999), 6); // UNDEFINED (out of range)
    }

    #[test]
    fn br_r62_condition_string() {
        // alarm.message is the condition string, "" for NO_ALARM/out-of-range.
        use epics_base_rs::server::recgbl::alarm_status as a;
        assert_eq!(alarm_condition_string(a::NO_ALARM), "");
        assert_eq!(alarm_condition_string(a::HIGH_ALARM), "HIGH");
        assert_eq!(alarm_condition_string(a::LINK_ALARM), "LINK");
        assert_eq!(alarm_condition_string(a::HW_LIMIT_ALARM), "HWLIMIT");
        assert_eq!(alarm_condition_string(a::UDF_ALARM), "UDF");
        assert_eq!(alarm_condition_string(999), "");
    }

    #[test]
    fn br_r62_build_alarm_emits_class_and_condition_message() {
        // LINK_ALARM (raw 14) must emit status class RECORD (3) and the
        // condition string "LINK" — not raw 14 / the severity name.
        use epics_base_rs::server::recgbl::alarm_status as a;
        let snap = Snapshot::new(EpicsValue::Double(1.0), a::LINK_ALARM, 2, UNIX_EPOCH);
        let alarm = build_alarm(&snap);
        assert_eq!(alarm_scalar_int(&alarm, "severity"), 2); // raw severity
        assert_eq!(alarm_scalar_int(&alarm, "status"), 3); // RECORD, not 14
        assert_eq!(alarm_scalar_str(&alarm, "message"), "LINK");
    }

    #[test]
    fn br_r62_build_alarm_no_alarm_has_empty_message() {
        let snap = Snapshot::new(EpicsValue::Double(1.0), 0, 0, UNIX_EPOCH);
        let alarm = build_alarm(&snap);
        assert_eq!(alarm_scalar_int(&alarm, "status"), 0); // NONE
        assert_eq!(alarm_scalar_str(&alarm, "message"), "");
    }

    #[test]
    fn br_build_alarm_prefers_carried_amsg_over_condition_string() {
        // pvxs iocsource.cpp:230-236: a non-empty carried amsg wins over the
        // synthesized condition string. mbboDirect raises UDF with the
        // bespoke "UDFS" (mbboDirectRecord.c:191), so an undefined mbboDirect
        // served over QSRV shows alarm.message = "UDFS", NOT "UDF".
        use epics_base_rs::server::recgbl::alarm_status as a;
        let mut snap = Snapshot::new(EpicsValue::Enum(0), a::UDF_ALARM, 3, UNIX_EPOCH);
        snap.alarm.amsg = "UDFS".to_string();
        let alarm = build_alarm(&snap);
        assert_eq!(alarm_scalar_str(&alarm, "message"), "UDFS");

        // Empty amsg with a non-zero status falls back to the condition
        // string — how every non-mbboDirect UDF record (empty namsg from
        // plain recGblSetSevr) serves "UDF".
        let generic = Snapshot::new(EpicsValue::Double(0.0), a::UDF_ALARM, 3, UNIX_EPOCH);
        assert!(generic.alarm.amsg.is_empty());
        let alarm = build_alarm(&generic);
        assert_eq!(alarm_scalar_str(&alarm, "message"), "UDF");

        // Empty amsg with NO_ALARM stays "".
        let ok = Snapshot::new(EpicsValue::Double(1.0), 0, 0, UNIX_EPOCH);
        assert_eq!(alarm_scalar_str(&build_alarm(&ok), "message"), "");

        // The overlay uses eff_status for the fallback but still prefers the
        // record's own amsg.
        let overlay = build_alarm_overlay(&snap, 2, a::LINK_ALARM);
        assert_eq!(alarm_scalar_str(&overlay, "message"), "UDFS");
        let overlay_generic = build_alarm_overlay(&generic, 2, a::LINK_ALARM);
        assert_eq!(alarm_scalar_str(&overlay_generic, "message"), "UDF");
    }

    #[test]
    fn nt_scalar_structure() {
        let snap = test_snapshot(EpicsValue::Double(42.5));
        let pv = snapshot_to_nt_scalar(&snap);

        assert_eq!(pv.struct_id, "epics:nt/NTScalar:1.0");
        assert_eq!(pv.get_value(), Some(&ScalarValue::Double(42.5)));
        assert!(pv.get_alarm().is_some());
        assert!(pv.get_timestamp().is_some());
        assert!(pv.get_field("display").is_some());
        assert!(pv.get_field("control").is_some());
        // valueAlarm with alarm thresholds
        let va = pv.get_field("valueAlarm");
        assert!(va.is_some());
        if let Some(PvField::Structure(va_struct)) = va {
            assert!(va_struct.get_field("lowAlarmLimit").is_some());
            assert!(va_struct.get_field("highAlarmLimit").is_some());
            assert!(va_struct.get_field("lowWarningLimit").is_some());
            assert!(va_struct.get_field("highWarningLimit").is_some());
        } else {
            panic!("expected valueAlarm structure");
        }
    }

    #[test]
    fn nt_scalar_empty_array_marks_invalid_udf() {
        // when an empty array reaches NTScalar conversion, value cannot
        // be recovered — alarm must escalate to INVALID severity / UDF status
        // so clients don't read the placeholder zero as a valid sample.
        // alarm.status is the PVA status CLASS — UDF maps to DRIVER
        // (2) — and alarm.message is the condition string "UDF".
        let snap = test_snapshot(EpicsValue::DoubleArray(vec![]));
        let pv = snapshot_to_nt_scalar(&snap);

        if let Some(PvField::Structure(alarm)) = pv.get_field("alarm") {
            let sev = alarm.get_field("severity");
            let st = alarm.get_field("status");
            let msg = alarm.get_field("message");
            assert!(matches!(sev, Some(PvField::Scalar(ScalarValue::Int(3)))));
            assert!(matches!(st, Some(PvField::Scalar(ScalarValue::Int(2)))));
            assert!(matches!(msg, Some(PvField::Scalar(ScalarValue::String(s))) if s == "UDF"));
        } else {
            panic!("expected alarm structure");
        }
    }

    #[test]
    fn nt_scalar_non_empty_keeps_original_alarm() {
        // Sanity: non-empty array (or scalar) does NOT trigger the overlay.
        let snap = test_snapshot(EpicsValue::Double(1.0));
        let pv = snapshot_to_nt_scalar(&snap);
        if let Some(PvField::Structure(alarm)) = pv.get_field("alarm") {
            let sev = alarm.get_field("severity");
            assert!(matches!(sev, Some(PvField::Scalar(ScalarValue::Int(0)))));
        } else {
            panic!("expected alarm structure");
        }
    }

    /// pvxs QSRV NTEnum uses int32_t index +
    /// `display.description` (testqsingle.cpp:174).
    #[test]
    fn nt_enum_structure() {
        let mut snap = Snapshot::new(EpicsValue::Enum(1), 0, 0, UNIX_EPOCH);
        snap.enums = Some(EnumInfo::new(vec!["Off".into(), "On".into()]));
        let pv = snapshot_to_nt_enum(&snap);

        assert_eq!(pv.struct_id, "epics:nt/NTEnum:1.0");
        if let Some(PvField::Structure(val)) = pv.get_field("value") {
            if let Some(PvField::Scalar(ScalarValue::Int(idx))) = val.get_field("index") {
                assert_eq!(*idx, 1);
            } else {
                panic!(
                    "expected int32_t index scalar, got {:?}",
                    val.get_field("index")
                );
            }
            if let Some(PvField::ScalarArray(choices)) = val.get_field("choices") {
                assert_eq!(choices.len(), 2);
            } else {
                panic!("expected choices array");
            }
        } else {
            panic!("expected value structure");
        }

        // display.description is part of pvxs's NTEnum shape.
        if let Some(PvField::Structure(d)) = pv.get_field("display") {
            assert!(
                matches!(
                    d.get_field("description"),
                    Some(PvField::Scalar(ScalarValue::String(_)))
                ),
                "expected display.description string"
            );
        } else {
            panic!("expected display structure");
        }
    }

    #[test]
    fn nt_scalar_array_structure() {
        let snap = test_snapshot(EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]));
        let pv = snapshot_to_nt_scalar_array(&snap);

        assert_eq!(pv.struct_id, "epics:nt/NTScalarArray:1.0");
        if let Some(PvField::ScalarArray(arr)) = pv.get_field("value") {
            assert_eq!(arr.len(), 3);
        } else {
            panic!("expected value array");
        }
    }

    #[test]
    fn put_roundtrip_scalar() {
        let snap = test_snapshot(EpicsValue::Double(99.0));
        let pv = snapshot_to_nt_scalar(&snap);
        let back = pv_structure_to_epics(&pv).unwrap();
        assert_eq!(back, EpicsValue::Double(99.0));
    }

    #[test]
    fn put_roundtrip_enum() {
        let mut snap = Snapshot::new(EpicsValue::Enum(2), 0, 0, UNIX_EPOCH);
        snap.enums = Some(EnumInfo::new(vec!["A".into(), "B".into(), "C".into()]));
        let pv = snapshot_to_nt_enum(&snap);
        let back = pv_structure_to_epics(&pv).unwrap();
        assert_eq!(back, EpicsValue::Enum(2));
    }

    #[test]
    fn nt_type_from_record_type() {
        assert_eq!(NtType::from_record_type("ai"), NtType::Scalar);
        assert_eq!(NtType::from_record_type("bi"), NtType::Enum);
        assert_eq!(NtType::from_record_type("waveform"), NtType::ScalarArray);
        assert_eq!(NtType::from_record_type("calc"), NtType::Scalar);
        assert_eq!(NtType::from_record_type("mbbi"), NtType::Enum);
    }

    /// `nt_type_for_field` derives the NT purely from the resolved value,
    /// with no record-type-name short-circuit for `VAL`. A `busy` VAL
    /// resolves to an enum value and must select NTEnum (was NTScalar
    /// under the old name-list short-circuit that omitted `busy`); arrays
    /// select NTScalarArray; scalars and an unresolved value select
    /// NTScalar — matching pvxs `dbChannelFinalFieldType` for every record.
    #[test]
    fn nt_type_for_field_derives_from_value() {
        // DBF_ENUM value (bi/bo/mbbi/mbbo AND busy) → NTEnum.
        assert_eq!(nt_type_for_field(Some(&EpicsValue::Enum(2))), NtType::Enum);
        // Array value → NTScalarArray (aai/waveform/compress VAL etc.).
        assert_eq!(
            nt_type_for_field(Some(&EpicsValue::DoubleArray(vec![1.0, 2.0]))),
            NtType::ScalarArray
        );
        assert_eq!(
            nt_type_for_field(Some(&EpicsValue::LongArray(vec![1, 2]))),
            NtType::ScalarArray
        );
        // Plain scalar → NTScalar.
        assert_eq!(
            nt_type_for_field(Some(&EpicsValue::Double(3.0))),
            NtType::Scalar
        );
        assert_eq!(
            nt_type_for_field(Some(&EpicsValue::Long(7))),
            NtType::Scalar
        );
        // Unresolved value → NTScalar (uniform with the non-VAL path).
        assert_eq!(nt_type_for_field(None), NtType::Scalar);
    }

    #[test]
    fn field_desc_nt_scalar() {
        let desc = build_nt_scalar_desc(ScalarType::Double);
        assert_eq!(desc.value_scalar_type(), Some(ScalarType::Double));
        assert_eq!(desc.field_count(), 6); // value, alarm, timeStamp, display, control, valueAlarm
    }

    #[test]
    fn filter_by_request_empty() {
        let snap = test_snapshot(EpicsValue::Double(1.0));
        let pv = snapshot_to_nt_scalar(&snap);

        // Empty request → return everything
        let req = PvStructure::new("");
        let filtered = filter_by_request(&pv, &req);
        assert_eq!(filtered.fields.len(), pv.fields.len());
    }

    #[test]
    fn filter_by_request_value_only() {
        let snap = test_snapshot(EpicsValue::Double(1.0));
        let pv = snapshot_to_nt_scalar(&snap);

        // Request only "value" field
        let mut field_spec = PvStructure::new("");
        field_spec
            .fields
            .push(("value".into(), PvField::Structure(PvStructure::new(""))));
        let mut req = PvStructure::new("");
        req.fields
            .push(("field".into(), PvField::Structure(field_spec)));

        let filtered = filter_by_request(&pv, &req);
        assert_eq!(filtered.fields.len(), 1);
        assert_eq!(filtered.fields[0].0, "value");
    }

    #[test]
    fn filter_by_request_multiple_fields() {
        let snap = test_snapshot(EpicsValue::Double(1.0));
        let pv = snapshot_to_nt_scalar(&snap);

        let mut field_spec = PvStructure::new("");
        field_spec
            .fields
            .push(("value".into(), PvField::Structure(PvStructure::new(""))));
        field_spec
            .fields
            .push(("alarm".into(), PvField::Structure(PvStructure::new(""))));
        let mut req = PvStructure::new("");
        req.fields
            .push(("field".into(), PvField::Structure(field_spec)));

        let filtered = filter_by_request(&pv, &req);
        assert_eq!(filtered.fields.len(), 2);
    }

    #[test]
    fn filter_by_request_nested_subfield() {
        let snap = test_snapshot(EpicsValue::Double(1.0));
        let pv = snapshot_to_nt_scalar(&snap);

        // Build request: {field: {alarm: {severity: {}}}}
        // — only return alarm.severity, not other alarm fields
        let mut alarm_spec = PvStructure::new("");
        alarm_spec
            .fields
            .push(("severity".into(), PvField::Structure(PvStructure::new(""))));

        let mut field_spec = PvStructure::new("");
        field_spec
            .fields
            .push(("alarm".into(), PvField::Structure(alarm_spec)));

        let mut req = PvStructure::new("");
        req.fields
            .push(("field".into(), PvField::Structure(field_spec)));

        let filtered = filter_by_request(&pv, &req);
        assert_eq!(filtered.fields.len(), 1);
        assert_eq!(filtered.fields[0].0, "alarm");

        // Verify alarm only has "severity" sub-field, not "status" or "message"
        if let PvField::Structure(alarm) = &filtered.fields[0].1 {
            assert_eq!(alarm.fields.len(), 1);
            assert_eq!(alarm.fields[0].0, "severity");
        } else {
            panic!("expected alarm structure");
        }
    }

    #[test]
    fn br_r12_array_metadata_shape_matches_pvxs() {
        // an int32 NTScalarArray must carry `control` and
        // `valueAlarm` (pvxs reuses the NTScalar builder for arrays —
        // src/nt.cpp:44-112, test/testqsingle.cpp:354-397), the
        // display/control/valueAlarm limits must be typed as the element
        // scalar type (Int, not Double), and `display.form` must be an
        // `enum_t` sub-structure, not a scalar int.
        let snap = test_snapshot(EpicsValue::LongArray(vec![4, 5, 6, 7]));
        let pv = snapshot_to_nt_scalar_array(&snap);

        // control + valueAlarm present on the array
        let control = pv.get_field("control").expect("array must carry control");
        let value_alarm = pv
            .get_field("valueAlarm")
            .expect("array must carry valueAlarm");

        // display.limitLow typed as Int (element scalar type), not Double
        if let Some(PvField::Structure(disp)) = pv.get_field("display") {
            assert!(
                matches!(
                    disp.get_field("limitLow"),
                    Some(PvField::Scalar(ScalarValue::Int(_)))
                ),
                "display.limitLow must be Int for an int32 array"
            );
            // display.form must be an enum_t structure with index + choices
            match disp.get_field("form") {
                Some(PvField::Structure(form)) => {
                    assert_eq!(form.struct_id, "enum_t");
                    assert!(matches!(
                        form.get_field("index"),
                        Some(PvField::Scalar(ScalarValue::Int(_)))
                    ));
                    if let Some(PvField::ScalarArray(choices)) = form.get_field("choices") {
                        assert_eq!(choices.len(), 7, "form.choices is the fixed 7-entry menu");
                    } else {
                        panic!("display.form.choices must be a string array");
                    }
                }
                _ => panic!("display.form must be an enum_t structure"),
            }
        } else {
            panic!("expected display structure");
        }

        // control.limitLow typed as Int
        if let PvField::Structure(c) = control {
            assert!(matches!(
                c.get_field("limitLow"),
                Some(PvField::Scalar(ScalarValue::Int(_)))
            ));
        } else {
            panic!("expected control structure");
        }

        // valueAlarm full field set: active, 4 limits (Int), 4 severities, hysteresis
        if let PvField::Structure(va) = value_alarm {
            assert!(matches!(
                va.get_field("active"),
                Some(PvField::Scalar(ScalarValue::Boolean(false)))
            ));
            assert!(matches!(
                va.get_field("lowAlarmLimit"),
                Some(PvField::Scalar(ScalarValue::Int(_)))
            ));
            assert!(va.get_field("lowAlarmSeverity").is_some());
            assert!(va.get_field("highAlarmSeverity").is_some());
            assert!(matches!(
                va.get_field("hysteresis"),
                Some(PvField::Scalar(ScalarValue::Double(_)))
            ));
        } else {
            panic!("expected valueAlarm structure");
        }

        // Descriptor must mirror the same shape.
        let desc = build_nt_scalar_array_desc(ScalarType::Int);
        if let FieldDesc::Structure { fields, .. } = &desc {
            let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
            assert!(names.contains(&"control"), "array desc must carry control");
            assert!(
                names.contains(&"valueAlarm"),
                "array desc must carry valueAlarm"
            );
        } else {
            panic!("expected structure descriptor");
        }
    }

    #[test]
    fn br_r12_string_value_omits_numeric_metadata() {
        // a non-numeric (string) NTScalar carries only
        // `display = {description, units}` — no limits, no form, no
        // control, no valueAlarm (pvxs src/nt.cpp:78-85).
        let snap = test_snapshot(EpicsValue::String("Analog input".into()));
        let pv = snapshot_to_nt_scalar(&snap);

        assert!(
            pv.get_field("control").is_none(),
            "string value must not carry control"
        );
        assert!(
            pv.get_field("valueAlarm").is_none(),
            "string value must not carry valueAlarm"
        );
        if let Some(PvField::Structure(disp)) = pv.get_field("display") {
            assert!(disp.get_field("limitLow").is_none());
            assert!(disp.get_field("form").is_none());
            assert!(disp.get_field("description").is_some());
            assert!(disp.get_field("units").is_some());
        } else {
            panic!("expected display structure");
        }

        let desc = build_nt_scalar_desc(ScalarType::String);
        if let FieldDesc::Structure { fields, .. } = &desc {
            let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
            assert!(!names.contains(&"control"));
            assert!(!names.contains(&"valueAlarm"));
        } else {
            panic!("expected structure descriptor");
        }
    }

    /// BR-112: a record whose metadata cache was never populated
    /// (`display`/`control` are `None`, as for stringin/stringout and
    /// histogram) must still emit exactly the metadata members its
    /// `getField` descriptor advertises — built from descriptor defaults
    /// — so the runtime GET value shape equals the descriptor shape.
    #[test]
    fn metadata_absent_value_member_set_matches_descriptor() {
        let names = |pv: &PvStructure| -> Vec<String> {
            pv.fields.iter().map(|(n, _)| n.clone()).collect()
        };
        let desc_names = |d: &FieldDesc| -> Vec<String> {
            match d {
                FieldDesc::Structure { fields, .. } => {
                    fields.iter().map(|(n, _)| n.clone()).collect()
                }
                other => panic!("expected structure descriptor, got {other:?}"),
            }
        };

        // Numeric scalar, no display/control metadata.
        let snap = Snapshot::new(EpicsValue::Double(1.0), 0, 0, UNIX_EPOCH);
        assert!(snap.display.is_none() && snap.control.is_none());
        let pv = snapshot_to_nt_scalar(&snap);
        assert_eq!(
            names(&pv),
            desc_names(&build_field_desc_for_nt(NtType::Scalar, ScalarType::Double)),
            "numeric scalar value member set must equal descriptor"
        );

        // Numeric array (histogram-like), no metadata.
        let snap_arr = Snapshot::new(EpicsValue::LongArray(vec![1, 2, 3]), 0, 0, UNIX_EPOCH);
        let pv_arr = snapshot_to_nt_scalar_array(&snap_arr);
        assert_eq!(
            names(&pv_arr),
            desc_names(&build_field_desc_for_nt(
                NtType::ScalarArray,
                ScalarType::Int
            )),
            "numeric array value member set must equal descriptor"
        );

        // String scalar (stringin-like), no metadata: `display` present,
        // no numeric `control`/`valueAlarm`.
        let snap_s = Snapshot::new(EpicsValue::String("x".into()), 0, 0, UNIX_EPOCH);
        let pv_s = snapshot_to_nt_scalar(&snap_s);
        assert_eq!(
            names(&pv_s),
            desc_names(&build_field_desc_for_nt(NtType::Scalar, ScalarType::String)),
            "string scalar value member set must equal descriptor"
        );
    }

    #[test]
    fn br_r13_uint64_array_qsrv_descriptor_uses_ulong() {
        // a `waveform` with `FTVL = UINT64` must be advertised
        // through QSRV as `ulong[]` with `uint64_t`-typed metadata limits,
        // matching pvxs test/testqsingle.cpp:530-546. On main there was no
        // `EpicsValue::UInt64Array`, so the value collapsed into
        // `DoubleArray` and the descriptor advertised `double[]`.
        let snap = test_snapshot(EpicsValue::UInt64Array(vec![
            11111111111111111,
            222222222222222,
        ]));
        let pv = snapshot_to_nt_scalar_array(&snap);

        // value is a ulong array
        match pv.get_field("value") {
            Some(PvField::ScalarArray(vs)) => {
                assert!(matches!(vs[0], ScalarValue::ULong(11111111111111111)));
            }
            other => panic!("expected ulong ScalarArray, got {other:?}"),
        }

        // display.limitLow typed as ULong, not Double
        if let Some(PvField::Structure(disp)) = pv.get_field("display") {
            assert!(
                matches!(
                    disp.get_field("limitLow"),
                    Some(PvField::Scalar(ScalarValue::ULong(_)))
                ),
                "uint64 array display.limitLow must be ulong"
            );
        } else {
            panic!("expected display structure");
        }

        // descriptor advertises ulong[] value and ulong limits
        let desc = build_nt_scalar_array_desc(ScalarType::ULong);
        if let FieldDesc::Structure { fields, .. } = &desc {
            let value_desc = fields.iter().find(|(n, _)| n == "value").map(|(_, d)| d);
            assert!(matches!(
                value_desc,
                Some(FieldDesc::ScalarArray(ScalarType::ULong))
            ));
            let control = fields.iter().find(|(n, _)| n == "control").map(|(_, d)| d);
            if let Some(FieldDesc::Structure { fields: cf, .. }) = control {
                assert!(matches!(
                    cf.iter().find(|(n, _)| n == "limitLow").map(|(_, d)| d),
                    Some(FieldDesc::Scalar(ScalarType::ULong))
                ));
            } else {
                panic!("expected control descriptor");
            }
        } else {
            panic!("expected structure descriptor");
        }
    }

    /// descriptor uses Int (not UShort) for `value.index`
    /// and includes a trailing `display.description` leaf.
    #[test]
    fn field_desc_nt_enum_index_int() {
        let desc = build_nt_enum_desc();
        if let FieldDesc::Structure { fields, .. } = &desc {
            // value.index = Int32
            if let Some((
                _,
                FieldDesc::Structure {
                    fields: val_fields, ..
                },
            )) = fields.iter().find(|(n, _)| n == "value")
            {
                let index_field = val_fields.iter().find(|(n, _)| n == "index");
                assert!(
                    matches!(index_field, Some((_, FieldDesc::Scalar(ScalarType::Int)))),
                    "expected NTEnum value.index Int32, got {index_field:?}"
                );
            } else {
                panic!("expected value structure");
            }
            // display.description = String
            if let Some((
                _,
                FieldDesc::Structure {
                    fields: disp_fields,
                    ..
                },
            )) = fields.iter().find(|(n, _)| n == "display")
            {
                let desc_field = disp_fields.iter().find(|(n, _)| n == "description");
                assert!(
                    matches!(desc_field, Some((_, FieldDesc::Scalar(ScalarType::String)))),
                    "expected display.description String"
                );
            } else {
                panic!("expected display sub-structure");
            }
        } else {
            panic!("expected NTEnum top-level structure");
        }
    }

    /// NTEnum node must narrow its `value` leaf to `value.index`,
    /// not mark the whole `value` subtree. pvxs assigns only `value.index`
    /// on a value event (`iocsource.cpp:107-109,331-351`) and fills
    /// `value.choices` solely via getProperties (`iocsource.cpp:278-285`).
    /// A bare `value` mark would, through `marked_changed_bitset`'s
    /// whole-subtree expansion, re-send the property-only `choices` array
    /// on every value update. Plain-scalar `value` leaves and non-value
    /// leaves stay untouched, and a nested (dot-pathed) enum member is
    /// handled too.
    #[test]
    fn enum_value_event_narrows_value_leaf_to_index() {
        use epics_pva_rs::pvdata::ScalarValue;

        fn nt_enum_member() -> PvField {
            let mut value = PvStructure::new("enum_t");
            value
                .fields
                .push(("index".into(), PvField::Scalar(ScalarValue::Int(1))));
            value.fields.push((
                "choices".into(),
                PvField::ScalarArray(vec![
                    ScalarValue::String("OFF".into()),
                    ScalarValue::String("ON".into()),
                ]),
            ));
            let mut m = PvStructure::new("epics:nt/NTEnum:1.0");
            m.fields.push(("value".into(), PvField::Structure(value)));
            m.fields.push((
                "alarm".into(),
                PvField::Structure(PvStructure::new("alarm_t")),
            ));
            PvField::Structure(m)
        }

        fn nt_scalar_member() -> PvField {
            let mut m = PvStructure::new("epics:nt/NTScalar:1.0");
            m.fields
                .push(("value".into(), PvField::Scalar(ScalarValue::Double(3.5))));
            m.fields.push((
                "alarm".into(),
                PvField::Structure(PvStructure::new("alarm_t")),
            ));
            PvField::Structure(m)
        }

        // composed group value: one NTEnum member, one plain NTScalar
        // member, and a nested NTEnum member under an intermediate node.
        let mut nested = PvStructure::new("");
        nested.fields.push(("mode".into(), nt_enum_member()));

        let mut root = PvStructure::new("");
        root.fields.push(("state".into(), nt_enum_member()));
        root.fields.push(("temp".into(), nt_scalar_member()));
        root.fields.push(("grp".into(), PvField::Structure(nested)));

        // member_value_is_enum: enum members resolve true (incl. nested),
        // plain scalar resolves false, and an unknown path resolves false.
        assert!(value_node_is_enum(&root, "state"));
        assert!(value_node_is_enum(&root, "grp.mode"));
        assert!(!value_node_is_enum(&root, "temp"));
        assert!(!value_node_is_enum(&root, "missing"));

        let marked = vec![
            "state.value".to_string(),
            "state.alarm".to_string(),
            "temp.value".to_string(),
            "grp.mode.value".to_string(),
        ];
        let narrowed = narrow_enum_value_leaves(marked, &root);
        assert_eq!(
            narrowed,
            vec![
                "state.value.index".to_string(),
                "state.alarm".to_string(),
                "temp.value".to_string(),
                "grp.mode.value.index".to_string(),
            ],
            "enum value leaves narrow to value.index; plain value and non-value leaves unchanged"
        );
        // The over-marking path that re-sent choices must be gone.
        assert!(
            !narrowed.contains(&"state.value".to_string()),
            "bare enum `value` leaf (expands to choices) must not survive: {narrowed:?}"
        );
    }

    /// R15-33 — a READ (GET reply, PUT_GET readback, monitor seed) frames
    /// what the source ASSIGNED, and QSRV's source assigns a strict subset
    /// of the NT.
    ///
    /// pvxs reads into a `cloneEmpty()` through `IOCSource::initialize` +
    /// `IOCSource::get(…, Everything, …)` (`singlesource.cpp:283`,
    /// `groupsource.cpp:454-460`), then frames it with
    /// `to_wire_valid(R, value, &pvMask)` (`serverget.cpp:104`). So the wire
    /// carries only assigned leaves, and `getProperties`
    /// (`iocsource.cpp:252-310`) assigns neither `control.minStep`, nor
    /// `valueAlarm.active`, nor the four `valueAlarm.*Severity` leaves, nor
    /// `valueAlarm.hysteresis` — pinned by pvxs's own delta,
    /// `testqsingle.cpp:129-149`, where those seven are absent while
    /// `display.form.index` / `.choices` (from `initialize`, which no DBE
    /// class ever posts) are present.
    ///
    /// Tested per boundary: the never-assigned seven, the initialize-only
    /// form pair, the mappings `initialize` skips (Meta / Plain), and the
    /// read-only-assigned `Const` node.
    /// R19-41: a property leaf the record type does not SUPPLY is never
    /// marked — on a read or on a property event alike. One case per gate
    /// boundary of `dbChannelGet`'s option narrowing (`dbAccess.c:336-430`),
    /// which is what pvxs's `if(options & DBR_*)` reads
    /// (`iocsource.cpp:263-305`):
    ///
    /// * `units` off — the measured `stringout`;
    /// * `alarm_double` off — the measured `waveform`;
    /// * `precision` off — the measured `longout` (and any non-float field);
    /// * `graphic_double` off — precision goes with it, pvxs nests the
    ///   assignment (`iocsource.cpp:288-291`);
    /// * `control_double` off; `enum_strs` on/off;
    /// * the whole mask off — `display.description` survives alone, the one
    ///   leaf pvxs assigns with no option gate at all.
    #[test]
    fn an_unsupplied_property_leaf_is_never_marked() {
        let marks = |props| change_leaf_paths("", FieldMapping::Scalar, EventMask::PROPERTY, props);

        let full = marks(PropertySupport {
            enum_strs: true,
            ..PropertySupport::NUMERIC
        });
        for leaf in [
            "display.units",
            "display.limitLow",
            "display.limitHigh",
            "display.precision",
            "control.limitLow",
            "control.limitHigh",
            "valueAlarm.lowAlarmLimit",
            "valueAlarm.lowWarningLimit",
            "valueAlarm.highWarningLimit",
            "valueAlarm.highAlarmLimit",
            "value.choices",
            "display.description",
        ] {
            assert!(
                full.contains(&leaf.to_string()),
                "a record type supplying every slot marks {leaf}: {full:?}"
            );
        }

        // stringout: no rset slot at all.
        assert_eq!(
            marks(PropertySupport::NONE),
            vec!["display.description".to_string()],
            "a record type with no property slot marks only DESC, which pvxs \
             assigns with no option gate (`iocsource.cpp:307-310`)"
        );

        // waveform: `#define get_alarm_double NULL`.
        let wf = marks(PropertySupport {
            alarm_double: false,
            ..PropertySupport::NUMERIC
        });
        assert!(
            !wf.iter().any(|p| p.starts_with("valueAlarm.")),
            "a waveform supplies no alarm limits — bands at zero must not be \
             marked authoritative: {wf:?}"
        );
        assert!(wf.contains(&"display.limitLow".to_string()));

        // longout: `#define get_precision NULL`.
        let lo = marks(PropertySupport {
            precision: false,
            ..PropertySupport::NUMERIC
        });
        assert!(
            !lo.contains(&"display.precision".to_string()),
            "a longout supplies no precision: {lo:?}"
        );
        assert!(lo.contains(&"display.limitLow".to_string()));

        // pvxs nests precision INSIDE the graphic-limits branch, dropping it
        // for a field that supplies get_precision but NULLs get_graphic_double
        // (CBUG-G1). The port declines to reproduce that: precision gates on its
        // own DBR_PRECISION slot, so a graphic_double-less numeric marks
        // display.precision (its independent slot) but not display.limitLow.
        let no_gr = marks(PropertySupport {
            graphic_double: false,
            ..PropertySupport::NUMERIC
        });
        assert!(
            no_gr.contains(&"display.precision".to_string())
                && !no_gr.contains(&"display.limitLow".to_string()),
            "precision is its own DBR_PRECISION slot, independent of the \
             DBR_GR_DOUBLE limits (CBUG-G1 deviation): {no_gr:?}"
        );

        let no_ctrl = marks(PropertySupport {
            control_double: false,
            ..PropertySupport::NUMERIC
        });
        assert!(
            !no_ctrl.iter().any(|p| p.starts_with("control.")),
            "a record type with no get_control_double marks no control limits: {no_ctrl:?}"
        );

        assert!(
            !marks(PropertySupport::NUMERIC).contains(&"value.choices".to_string()),
            "a numeric record supplies no enum strings"
        );

        // The gate applies to the READ mark set too — a GET reply must not
        // carry the fabricated leaf either.
        let read = read_leaf_paths("", FieldMapping::Scalar, true, PropertySupport::NONE);
        for absent in ["display.units", "display.precision", "control.limitLow"] {
            assert!(
                !read.contains(&absent.to_string()),
                "a read must not frame the unsupplied {absent}: {read:?}"
            );
        }
        assert!(
            read.contains(&"value".to_string()) && read.contains(&"timeStamp".to_string()),
            "the value/alarm/timeStamp set is not property-gated: {read:?}"
        );
    }

    #[test]
    fn read_leaf_paths_frames_pvxs_initialize_plus_get_everything() {
        let scalar = read_leaf_paths("", FieldMapping::Scalar, true, NUMERIC_PROPS);

        // The seven leaves the port's NT carries and `getProperties` never
        // assigns. Absent from the read → absent from the wire.
        for never in [
            "control.minStep",
            "valueAlarm.active",
            "valueAlarm.lowAlarmSeverity",
            "valueAlarm.lowWarningSeverity",
            "valueAlarm.highWarningSeverity",
            "valueAlarm.highAlarmSeverity",
            "valueAlarm.hysteresis",
        ] {
            assert!(
                !scalar.contains(&never.to_string()),
                "`getProperties` never assigns {never} — it must not be framed: {scalar:?}"
            );
        }

        // `IOCSource::initialize` assigns the display form for a Scalar, so a
        // read carries it even though no DBE class posts it.
        for form in ["display.form.choices", "display.form.index"] {
            assert!(
                scalar.contains(&form.to_string()),
                "`IOCSource::initialize` assigns {form} on a read: {scalar:?}"
            );
        }

        // A read is `get(Everything)`: the value, the alarm/timeStamp pair and
        // every property leaf `getProperties` does assign.
        for assigned in ["value", "alarm", "timeStamp", "display.units"] {
            assert!(
                scalar.contains(&assigned.to_string()),
                "get(Everything) assigns {assigned}: {scalar:?}"
            );
        }

        // `initialize` is `info.type == Scalar`-only: a Meta member carries
        // alarm + timeStamp and no form, and a value-only mapping is marked
        // whole with nothing under it.
        let meta = read_leaf_paths("m", FieldMapping::Meta, true, NUMERIC_PROPS);
        assert_eq!(
            meta,
            vec!["m.timeStamp".to_string(), "m.alarm".to_string()],
            "a `+type:meta` member assigns only the alarm/timeStamp pair"
        );
        assert_eq!(
            read_leaf_paths("p", FieldMapping::Plain, true, NUMERIC_PROPS),
            vec!["p".to_string()],
            "a value-only mapping is the value: marked whole, no metadata under it"
        );

        // `IOCSource::get` assigns `info.cval` for a Const member
        // (`iocsource.cpp:319-322`) — a read frames it, no event ever posts
        // it. Structure/Proc members are skipped on read as on post.
        assert_eq!(
            read_leaf_paths("k", FieldMapping::Const, true, NUMERIC_PROPS),
            vec!["k".to_string()],
            "a const member is assigned on every read"
        );
        assert!(
            read_leaf_paths("s", FieldMapping::Structure, true, NUMERIC_PROPS).is_empty(),
            "pvxs skips a Structure member on a read (`iocsource.cpp:316-317`)"
        );

        // A subscripted member collapses to its enclosing array field on a
        // read exactly as on a post — that field is the only markable bit.
        assert_eq!(
            read_leaf_paths("a[0].x", FieldMapping::Scalar, true, NUMERIC_PROPS),
            vec!["a".to_string()],
            "a subscripted member marks the enclosing array field, whole"
        );
    }

    /// R16-31: `IOCSource::initialize` assigns `display.form.index` only when
    /// the channel addresses the record's VAL field (`dbIsValueField`,
    /// `iocsource.cpp:53`); `display.form.choices` is assigned for every
    /// field. A non-VAL channel (`REC.RVAL`) that marked the index shipped a
    /// changed bit and four bytes pvxs never sends.
    #[test]
    fn form_index_marked_for_val_channels_only() {
        let val = read_leaf_paths("", FieldMapping::Scalar, true, NUMERIC_PROPS);
        assert!(val.contains(&"display.form.choices".to_string()));
        assert!(val.contains(&"display.form.index".to_string()));

        let non_val = read_leaf_paths("", FieldMapping::Scalar, false, NUMERIC_PROPS);
        assert!(
            non_val.contains(&"display.form.choices".to_string()),
            "the form menu is assigned for every field: {non_val:?}"
        );
        assert!(
            !non_val.contains(&"display.form.index".to_string()),
            "Q:form applies to form.index for VAL only: {non_val:?}"
        );

        // Same rule for a group member bound to a non-VAL field.
        let member = read_leaf_paths("rval", FieldMapping::Scalar, false, NUMERIC_PROPS);
        assert!(member.contains(&"rval.display.form.choices".to_string()));
        assert!(
            !member.contains(&"rval.display.form.index".to_string()),
            "a group member on REC.RVAL must not mark its form index: {member:?}"
        );
    }

    /// R16-31: the value side of the same rule — `build_display` writes the
    /// index straight from `DisplayInfo::form`, which the snapshot producer
    /// now narrows to the served field (0 = Default for a non-VAL field), so
    /// a `Q:form` record never reports Hex on `REC.RVAL`.
    #[test]
    fn build_display_publishes_the_snapshots_form_index() {
        let disp = DisplayInfo {
            form: 4, // Hex — as the VAL channel of an info(Q:form,"Hex") record
            ..Default::default()
        };
        let d = build_display(&disp, ScalarType::Int, true);
        let PvField::Structure(form) = &d.fields.iter().find(|(n, _)| n == "form").unwrap().1
        else {
            panic!("display.form must be an enum_t structure");
        };
        assert_eq!(
            form.fields.iter().find(|(n, _)| n == "index").unwrap().1,
            PvField::Scalar(ScalarValue::Int(4))
        );
        let PvField::ScalarArray(choices) =
            &form.fields.iter().find(|(n, _)| n == "choices").unwrap().1
        else {
            panic!("display.form.choices must be a string array");
        };
        assert_eq!(choices.len(), 7, "the menu is published regardless");

        // The non-VAL snapshot carries form = 0 (Default).
        let d = build_display(&DisplayInfo::default(), ScalarType::Int, true);
        let PvField::Structure(form) = &d.fields.iter().find(|(n, _)| n == "form").unwrap().1
        else {
            panic!("display.form must be an enum_t structure");
        };
        assert_eq!(
            form.fields.iter().find(|(n, _)| n == "index").unwrap().1,
            PvField::Scalar(ScalarValue::Int(0))
        );
    }
}
