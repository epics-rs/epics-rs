//! The NT shape QSRV2 projects a channel as — **derived from the `.dbd`**,
//! so a wrong shape is attributable instead of being a bare text diff.
//!
//! # Why the shape is predictable at all
//!
//! QSRV2 does not project per record type. It picks the shape per channel from
//! that channel's *final* DBR type, and nothing else
//! (`pvxs/ioc/singlesource.cpp:189-205`, `getValuePrototype`):
//!
//! ```text
//! short dbrType(dbChannelFinalFieldType(chan));
//! auto valueType(IOCSource::getChannelValueType(chan));
//! if (dbrType == DBR_ENUM) valuePrototype = nt::NTEnum{}.create();
//! else valuePrototype = nt::NTScalar{ valueType, display, control, valueAlarm, true }.create();
//! ```
//!
//! So the shape is a function of the channel's *final* `DBADDR` — which, for
//! most fields, the declared `DBF_*` type in [`crate::dbd`] fixes outright. That
//! makes it derivable *before* either side is asked, which is the whole point: a
//! channel whose shape is wrong can then be blamed on a named side, rather than
//! showing up as an unattributable difference between two blobs of text.
//!
//! "For most fields" is load-bearing; see [the limit of the
//! derivation](#where-the-dbd-stops-determining-the-shape) below.
//!
//! # The step that is easy to get wrong
//!
//! `dbChannelFinalFieldType` does **not** return the field's `DBF_*` code. It
//! returns the *export* code, which base maps first:
//!
//! ```text
//! dbChannel.c:579   probe.field_type = dbChannelExportType(chan);
//! dbChannel.c:621   chan->final_type = probe.field_type;
//! dbChannel.h:424   #define dbChannelExportType(pChan) ((pChan)->addr.dbr_field_type)
//! dbAccess.c:639    paddr->dbr_field_type = mapDBFToDBR[dbfType];
//! ```
//!
//! and `mapDBFToDBR` (`dbAccess.c:76`) sends `DBF_MENU` and `DBF_DEVICE` to
//! `DBR_ENUM`, and all three link types to `DBR_STRING`. Read naively — as if
//! `final_type` were the raw `DBF_*` code — every `DBF_MENU` field would be
//! predicted `NTScalar{Null}`, because `DBF_MENU` is 12 and `DBR_ENUM` is 11,
//! and `fromDbrType` (`pvxs/ioc/typeutils.cpp:30`) returns `Null` for anything
//! it does not name. That would mis-predict most of `dbCommon` — `SCAN`,
//! `STAT`, `SEVR` and the rest are all `DBF_MENU`. The mapping through
//! `dbr_field_type` is what puts them on `NTEnum`, and [`NtShape::expected`]
//! encodes the mapped rule.
//!
//! # Where the `.dbd` stops determining the shape
//!
//! `mapDBFToDBR` sets a **default** that the record type may then overwrite. The
//! very next lines of `dbEntryToAddr` — which `dbNameToAddr` reaches at
//! `dbAccess.c:677` — hand the `DBADDR` to the record (`:639-648`):
//!
//! ```text
//! paddr->dbr_field_type = mapDBFToDBR[dbfType];   /* the rule encoded below */
//! if (paddr->special == SPC_DBADDR) {
//!     const rset *prset = dbGetRset(paddr);
//!     if (prset && prset->cvt_dbaddr)
//!         return prset->cvt_dbaddr(paddr);        /* ...which this may contradict */
//! }
//! ```
//!
//! Both inputs to the projection live in that `DBADDR` and both are fair game
//! for `cvt_dbaddr`: `dbChannelFinalFieldType` reads `dbr_field_type`, and
//! `dbChannelFinalElements` — which decides scalar vs array
//! (`iocsource.cpp:632`, `isArray = dbChannelFinalElements(chan) != 1`) — reads
//! `no_elements`. Two measured cases show both halves failing:
//!
//! - `mbbo.VAL` is `DBF_ENUM`, but its `cvt_dbaddr` rewrites the type to
//!   `DBF_USHORT` when the record has no state strings
//!   (`mbboRecord.c:308-311`). Both sides project it `NTScalar{uint16_t}`, not
//!   `NTEnum`. The condition is `!prec->sdef` — a *runtime record value* — so
//!   this is not a rule the `.dbd` withholds, it is one the `.dbd` cannot state.
//! - `asyn.BINP`/`BOUT` keep their declared `DBF_CHAR` but become arrays of
//!   `imax`/`omax` elements (`asynRecord.c:944-955`), so the true shape is
//!   `NTScalarArray` — a mode the scalar table below cannot express at all.
//!
//! `special(SPC_DBADDR)` is precisely the gate on that override, and it *is* in
//! the `.dbd`. So the `.dbd` states exactly where its own authority ends, and
//! [`NtShape::expected`] declines to predict there ([`FieldDef::rewrites_dbaddr`]).
//! Hard-coding the two observed answers instead would be worse than useless:
//! `mbbo.VAL`'s depends on the reproducer `.db` having no `ZRST`, so the
//! hard-coded row would silently become a lie the day that `.db` gains state
//! strings. Declining is the only answer that stays true.
//!
//! Those channels are still fully diffed C-against-Rust on both contracts
//! ([`crate::pvaread`]); it is only this harness's *own* prediction that stands
//! down, on the three fields of the enumerated surface that carry the token.
//!
//! # What is derived, and what is deliberately not
//!
//! Only the two things the `DBF_*` type actually determines: the **NT type id**
//! and the declared type of the **`value`** member. Not the rest of the
//! structure — whether `display` carries `limitLow`/`precision`/`form`, what
//! `control` contains, which `valueAlarm` members exist. Those come from pvxs's
//! NT builder rather than from the field, so predicting them here would be this
//! harness asserting its own reimplementation of `nt.cpp` and calling a
//! divergence in *that* a port defect. The two sides are still compared against
//! each other on the full `pvxinfo` text ([`crate::pvaread`]), so nothing goes
//! unmeasured — it is only the *expectation* that stops at what the `.dbd`
//! entails.
//!
//! # Provenance of the table
//!
//! Every row but two was confirmed against `softIocPVX` itself by reading a
//! witness field of that `DBF_*` type and parsing what it declared. The
//! exceptions are `DBF_CHAR` and `DBF_FLOAT`: the only fields carrying them in
//! the fat dbd are `asyn.BOUT`/`asyn.BINP` and `transform.VERS`, and
//! `softIocPVX` cannot load `asyn` or `transform` at all (measured: `ERROR:
//! Record type 'transform' for record 'T:X' not found`, exit 2). Those two rows
//! therefore come from `fromDbrType` (`typeutils.cpp:32,51`) alone and have no
//! live witness on this side. They are stated here rather than quietly omitted.

use crate::dbd::{DbfType, FieldDef};

/// `epics:nt/NTScalar:1.0`.
pub const NT_SCALAR: &str = "epics:nt/NTScalar:1.0";
/// `epics:nt/NTEnum:1.0`.
pub const NT_ENUM: &str = "epics:nt/NTEnum:1.0";
/// What `NTEnum`'s struct-valued `value` member is called on the wire.
pub const ENUM_T: &str = "enum_t";

/// The part of a channel's NT projection that the `.dbd` entails.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NtShape {
    /// The NT type id, e.g. `epics:nt/NTScalar:1.0`.
    pub type_id: String,
    /// The declared type of the `value` member: a pvxs scalar rendering such as
    /// `double`/`int16_t`/`string`, or [`ENUM_T`] for `NTEnum`'s struct.
    pub value: String,
}

impl NtShape {
    fn scalar(value: &str) -> Self {
        Self {
            type_id: NT_SCALAR.to_string(),
            value: value.to_string(),
        }
    }

    fn enumeration() -> Self {
        Self {
            type_id: NT_ENUM.to_string(),
            value: ENUM_T.to_string(),
        }
    }

    /// The shape QSRV2 must project this field's channel as, or `None` where the
    /// `.dbd` does not entail one.
    ///
    /// `None` in exactly two cases, both of which are the `.dbd` declining to
    /// answer rather than this function giving up:
    ///
    /// - **`special(SPC_DBADDR)`** — the record type's `cvt_dbaddr` owns this
    ///   field's type and element count, and the declared `DBF_*` is only the
    ///   default it overrides ([`FieldDef::rewrites_dbaddr`]). Predicting from
    ///   the declared type here is how `mbbo.VAL` got called a `DEFECT` against
    ///   two sides that were both right.
    /// - **`DBF_NOACCESS`** — `fromDbrType(DBR_NOACCESS)` is `TypeCode::Null`,
    ///   pvxs cannot build an `NTScalar` of `Null`, and `SingleSource::onCreate`
    ///   refuses the channel outright rather than serving a shape. Those fields
    ///   are already outside the denominator ([`crate::surface`]).
    ///
    /// Everything that survives both gates is a scalar
    /// (`dbChannelFinalElements == 1`): `no_elements` is 1 unless `cvt_dbaddr`
    /// says otherwise, and the fields whose `cvt_dbaddr` makes them arrays are
    /// precisely the ones the first gate just removed.
    pub fn expected(field: &FieldDef) -> Option<Self> {
        if field.rewrites_dbaddr() {
            return None;
        }
        Self::projected(field.dbf)
    }

    /// `mapDBFToDBR` (`dbAccess.c:639`) composed with `fromDbrType`
    /// (`typeutils.cpp:30`) — the rule for a field whose `DBADDR` no record-type
    /// hook rewrites.
    ///
    /// Private on purpose: a caller holding only a `DbfType` has already lost the
    /// `special(SPC_DBADDR)` marker that says whether this rule applies, so
    /// [`Self::expected`] is the only way in.
    fn projected(dbf: DbfType) -> Option<Self> {
        Some(match dbf {
            // mapDBFToDBR: MENU and DEVICE both land on DBR_ENUM, alongside ENUM.
            DbfType::Enum | DbfType::Menu | DbfType::Device => Self::enumeration(),
            // mapDBFToDBR: every link exports as DBR_STRING.
            DbfType::String | DbfType::InLink | DbfType::OutLink | DbfType::FwdLink => {
                Self::scalar("string")
            }
            DbfType::Char => Self::scalar("int8_t"),
            DbfType::UChar => Self::scalar("uint8_t"),
            DbfType::Short => Self::scalar("int16_t"),
            DbfType::UShort => Self::scalar("uint16_t"),
            DbfType::Long => Self::scalar("int32_t"),
            DbfType::ULong => Self::scalar("uint32_t"),
            DbfType::Int64 => Self::scalar("int64_t"),
            DbfType::UInt64 => Self::scalar("uint64_t"),
            DbfType::Float => Self::scalar("float"),
            DbfType::Double => Self::scalar("double"),
            DbfType::NoAccess => return None,
        })
    }

    /// The shape a `pvxinfo` block actually declares.
    ///
    /// Parses only the outer struct's id and its `value` member, which is
    /// exactly what [`Self::expected`] predicts. Members of nested structs
    /// (`alarm.severity`, `display.form.choices`, ...) are skipped by brace
    /// depth, so a `value` nested inside `display` could never be mistaken for
    /// the top-level one.
    ///
    /// `None` when the block declares no outer struct or no `value` member —
    /// an unparseable block is reported as such, never silently treated as a
    /// shape that happens to match.
    pub fn observed(block: &str) -> Option<Self> {
        let mut depth: usize = 0;
        let mut type_id: Option<String> = None;
        // The id of the struct opened at each depth, so `} value` can report
        // what kind of struct it closed.
        let mut open: Vec<Option<String>> = Vec::new();
        let mut value: Option<String> = None;

        for line in block.lines() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            if t.ends_with('{') {
                let id = struct_id(t);
                if depth == 0 {
                    type_id = id.clone();
                }
                open.push(id);
                depth += 1;
                continue;
            }
            if t.starts_with('}') {
                let id = open.pop().flatten();
                depth = depth.saturating_sub(1);
                // A struct-valued member of the OUTER struct closes back to
                // depth 1 — that is NTEnum's `} value`.
                if depth == 1 && t.trim_start_matches('}').trim() == "value" {
                    value = Some(id.unwrap_or_else(|| "struct".to_string()));
                }
                continue;
            }
            // A plain `<type> <name>` member of the outer struct.
            if depth == 1
                && let Some((ty, name)) = t.rsplit_once(' ')
                && name == "value"
            {
                value = Some(ty.trim().to_string());
            }
        }

        Some(Self {
            type_id: type_id?,
            value: value?,
        })
    }

    /// A one-line rendering for the report, e.g. `epics:nt/NTScalar:1.0{value: double}`.
    pub fn render(&self) -> String {
        format!("{}{{value: {}}}", self.type_id, self.value)
    }
}

/// The id out of `struct "epics:nt/NTScalar:1.0" {`, or `None` for the
/// anonymous `struct {` pvxs emits for `display`/`control`/`valueAlarm`.
fn struct_id(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("struct")?.trim_start();
    let rest = rest.strip_prefix('"')?;
    rest.split('"').next().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain field: the `.dbd` alone fixes its type.
    fn field(dbf: DbfType) -> FieldDef {
        FieldDef {
            name: "VAL".into(),
            dbf,
            size: None,
            special: None,
            menu: None,
            initial: None,
            pp: false,
            asl: None,
        }
    }

    /// A field whose record type rewrites the `DBADDR` behind the `.dbd`'s back.
    fn dbaddr_field(dbf: DbfType) -> FieldDef {
        FieldDef {
            special: Some("SPC_DBADDR".into()),
            ..field(dbf)
        }
    }

    /// A real `pvxinfo` body for `ORACLE:AI` (DBF_DOUBLE), verbatim from
    /// `softIocPVX`. Trimmed of the header line, which the batch splitter
    /// consumes as the record separator.
    const AI_VAL: &str = r#"struct "epics:nt/NTScalar:1.0" {
    double value
    struct "alarm_t" {
        int32_t severity
        int32_t status
        string message
    } alarm
    struct "time_t" {
        int64_t secondsPastEpoch
        int32_t nanoseconds
        int32_t userTag
    } timeStamp
    struct {
        double limitLow
        double limitHigh
        string description
        string units
        int32_t precision
        struct "enum_t" {
            int32_t index
            string[] choices
        } form
    } display
    struct {
        double limitLow
        double limitHigh
        double minStep
    } control
    struct {
        bool active
        double lowAlarmLimit
        double lowWarningLimit
        double highWarningLimit
        double highAlarmLimit
        int32_t lowAlarmSeverity
        int32_t lowWarningSeverity
        int32_t highWarningSeverity
        int32_t highAlarmSeverity
        double hysteresis
    } valueAlarm
}"#;

    /// A real `pvxinfo` body for `ORACLE:AI.SCAN` (DBF_MENU) — an NTEnum, whose
    /// `value` is a struct rather than a plain member.
    const AI_SCAN: &str = r#"struct "epics:nt/NTEnum:1.0" {
    struct "enum_t" {
        int32_t index
        string[] choices
    } value
    struct "alarm_t" {
        int32_t severity
        int32_t status
        string message
    } alarm
    struct "time_t" {
        int64_t secondsPastEpoch
        int32_t nanoseconds
        int32_t userTag
    } timeStamp
    struct {
        string description
    } display
}"#;

    #[test]
    fn a_scalar_block_parses_to_its_type_id_and_value_type() {
        let s = NtShape::observed(AI_VAL).expect("parses");
        assert_eq!(s.type_id, NT_SCALAR);
        assert_eq!(s.value, "double");
    }

    /// The trap the depth tracking exists for: `display.form` is an `enum_t`
    /// struct and `alarm` has members too, but only the OUTER struct's `value`
    /// may be read.
    #[test]
    fn nested_members_never_masquerade_as_the_top_level_value() {
        let s = NtShape::observed(AI_VAL).expect("parses");
        assert_eq!(s.value, "double", "display.form must not become the value");
    }

    #[test]
    fn an_ntenum_block_reports_its_struct_valued_value_member() {
        let s = NtShape::observed(AI_SCAN).expect("parses");
        assert_eq!(s.type_id, NT_ENUM);
        assert_eq!(s.value, ENUM_T);
    }

    /// The measured C-side ground truth, one row per DBF type that has a
    /// witness in a record type `softIocPVX` can actually load. These pairs
    /// were read off `softIocPVX` (see the module docs), so this test pins the
    /// derivation against the real projection rather than against itself.
    #[test]
    fn the_derived_shape_matches_what_softiocpvx_was_measured_to_declare() {
        let cases: &[(DbfType, &str, &str)] = &[
            // Witness: aai.HOPR
            (DbfType::Double, NT_SCALAR, "double"),
            // Witness: ai.RVAL
            (DbfType::Long, NT_SCALAR, "int32_t"),
            // Witness: aai.PHAS
            (DbfType::Short, NT_SCALAR, "int16_t"),
            // Witness: bi.LALM
            (DbfType::UShort, NT_SCALAR, "uint16_t"),
            // Witness: aai.NELM
            (DbfType::ULong, NT_SCALAR, "uint32_t"),
            // Witness: int64in.VAL
            (DbfType::Int64, NT_SCALAR, "int64_t"),
            // Witness: aai.UTAG
            (DbfType::UInt64, NT_SCALAR, "uint64_t"),
            // Witness: aai.DISP
            (DbfType::UChar, NT_SCALAR, "uint8_t"),
            // Witness: aai.NAME
            (DbfType::String, NT_SCALAR, "string"),
            // Witnesses: aai.TSEL / aao.OUT / aai.FLNK — every link exports as
            // DBR_STRING, which is the mapDBFToDBR row that is easy to miss.
            (DbfType::InLink, NT_SCALAR, "string"),
            (DbfType::OutLink, NT_SCALAR, "string"),
            (DbfType::FwdLink, NT_SCALAR, "string"),
            // Witness: bi.VAL
            (DbfType::Enum, NT_ENUM, ENUM_T),
            // Witness: aai.SCAN — DBF_MENU, the row a naive reading gets wrong.
            (DbfType::Menu, NT_ENUM, ENUM_T),
            // Witness: aai.DTYP
            (DbfType::Device, NT_ENUM, ENUM_T),
        ];
        for (dbf, id, value) in cases {
            let s = NtShape::expected(&field(*dbf))
                .unwrap_or_else(|| panic!("{dbf:?} must have a shape"));
            assert_eq!(&s.type_id, id, "{dbf:?} type id");
            assert_eq!(&s.value, value, "{dbf:?} value type");
        }
    }

    /// The measured case that proves the table is not the whole rule.
    ///
    /// `mbbo.VAL` is `DBF_ENUM` and both `softIocPVX` and the port project it
    /// `NTScalar{uint16_t}` — its `cvt_dbaddr` rewrites the type to `DBF_USHORT`
    /// when the record has no state strings (`mbboRecord.c:308-311`). Predicting
    /// `NTEnum` from the declared type indicted two sides that agreed with each
    /// other and with base. The `.dbd` flags the field `special(SPC_DBADDR)`, so
    /// the honest answer is to make no prediction.
    #[test]
    fn a_field_whose_record_rewrites_the_dbaddr_gets_no_prediction() {
        assert_eq!(
            NtShape::expected(&field(DbfType::Enum)),
            Some(NtShape::enumeration()),
            "an ordinary DBF_ENUM field is still NTEnum",
        );
        assert!(
            NtShape::expected(&dbaddr_field(DbfType::Enum)).is_none(),
            "mbbo.VAL: the record type overrides the declared type in C",
        );
    }

    /// `asyn.BINP`/`BOUT` keep their declared `DBF_CHAR` yet `cvt_dbaddr` makes
    /// them arrays (`asynRecord.c:944-955`), so the true shape is an
    /// `NTScalarArray` that this scalar table cannot express. The `SPC_DBADDR`
    /// gate covers the element-count half of the override, not just the type
    /// half — the same token guards both.
    #[test]
    fn the_gate_also_covers_a_rewrite_that_changes_only_the_element_count() {
        assert!(
            NtShape::expected(&dbaddr_field(DbfType::Char)).is_none(),
            "asyn.BINP is DBF_CHAR but is served as an array",
        );
    }

    /// `DBF_CHAR`/`DBF_FLOAT` have no witness on this side (their only fields
    /// live in `asyn`/`transform`, which `softIocPVX` cannot load), so these two
    /// rows come from `fromDbrType` alone. Pinned so the unwitnessed claim is at
    /// least stated in one place rather than assumed.
    #[test]
    fn the_two_unwitnessed_rows_follow_from_dbr_type() {
        assert_eq!(
            NtShape::expected(&field(DbfType::Char)).unwrap().value,
            "int8_t"
        );
        assert_eq!(
            NtShape::expected(&field(DbfType::Float)).unwrap().value,
            "float"
        );
    }

    /// No shape is claimed for a field pvxs refuses to serve at all.
    #[test]
    fn noaccess_has_no_expected_shape() {
        assert!(NtShape::expected(&field(DbfType::NoAccess)).is_none());
    }

    /// An unparseable block must not resolve to a shape — otherwise a garbled
    /// reply could agree with an expectation by accident.
    #[test]
    fn an_unparseable_block_yields_no_shape_rather_than_a_wrong_one() {
        assert!(NtShape::observed("").is_none());
        assert!(NtShape::observed(" Error: field introspection unavailable").is_none());
        // A struct with no `value` member is not a shape either.
        assert!(NtShape::observed("struct \"x\" {\n    double other\n}").is_none());
    }

    #[test]
    fn render_states_both_halves_of_the_shape() {
        assert_eq!(
            NtShape::expected(&field(DbfType::Double)).unwrap().render(),
            "epics:nt/NTScalar:1.0{value: double}"
        );
        assert_eq!(
            NtShape::expected(&field(DbfType::Menu)).unwrap().render(),
            "epics:nt/NTEnum:1.0{value: enum_t}"
        );
    }
}
