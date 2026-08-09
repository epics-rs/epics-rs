use crate::types::{DbFieldType, EpicsValue, PvString, WallTime};

/// Alarm status and severity.
#[derive(Debug, Clone, Default)]
pub struct AlarmInfo {
    pub status: u16,
    pub severity: u16,
    /// Acknowledge transient (record ACKT field). Populated when
    /// callers want DBR_STSACK_STRING responses to carry it; otherwise
    /// `None` and the encoder substitutes 0.
    pub ackt: Option<u16>,
    /// Acknowledge severity (record ACKS field).
    pub acks: Option<u16>,
    /// Alarm message string (`DBR_AMSG` / dbCommon `AMSG`). The record's
    /// committed `common.amsg`, populated by the per-record snapshot
    /// builders that hold the record's `CommonFields`
    /// (`snapshot_for_field`, `make_monitor_snapshot`); empty on the
    /// minimal `Snapshot::new` path and for non-record channels. PVA's
    /// `build_alarm` prefers a non-empty amsg over the synthesized
    /// condition string, mirroring pvxs `iocsource.cpp:230-236`.
    pub amsg: String,
}

/// Display/graphic metadata for numeric types.
#[derive(Debug, Clone, Default)]
pub struct DisplayInfo {
    /// Engineering units (record EGU). Byte-preserving: CA `DBR_STRING`
    /// and PVA `display.units` carry raw, not-guaranteed-UTF-8 bytes, so a
    /// non-UTF-8 EGU must reach the wire unmangled (pvxs stores the wire
    /// string verbatim, `pvaproto.h:403`). A `String` here forced a lossy
    /// UTF-8 round-trip at this metadata boundary.
    pub units: PvString,
    pub precision: i16,
    pub upper_disp_limit: f64,
    pub lower_disp_limit: f64,
    pub upper_alarm_limit: f64,
    pub upper_warning_limit: f64,
    pub lower_warning_limit: f64,
    pub lower_alarm_limit: f64,
    /// Display format hint (0=Default, 1=String, 2=Binary, 3=Decimal,
    /// 4=Hex, 5=Exponential, 6=Engineering). From record's Q:form info tag.
    pub form: i16,
    /// Record description (DESC field). `PvString` for the same
    /// byte-preserving reason as `units`: a non-UTF-8 DESC must reach
    /// the wire unmangled.
    pub description: PvString,
}

/// Control limits (DRVH/DRVL for output records, or HOPR/LOPR).
#[derive(Debug, Clone, Default)]
pub struct ControlInfo {
    pub upper_ctrl_limit: f64,
    pub lower_ctrl_limit: f64,
}

/// How an enum-valued field renders as a `DBR_STRING` — C's `get_enum_str`
/// rset slot (`dbConvert.c::getEnumString`), the menu choice list
/// (`getMenuString`) or the device choice list (`getDeviceString`).
///
/// This is NOT the [`EnumInfo::strings`] label array. C keeps the two apart
/// and so must the port:
///
/// * `get_enum_strs` (plural) fills `DBR_GR_ENUM` and reports `no_str`, the
///   count of *meaningful* leading states. `mbbi` cuts it at the last
///   non-empty state (`mbbiRecord.c:257-271`), so an all-empty record has
///   `no_str == 0`.
/// * `get_enum_str` (singular) renders ONE value. It indexes the record's
///   state array *untrimmed* (`mbbiRecord.c:246-250` reads `zrst + val*size`
///   for any `val <= 15`), so an undefined state renders as the EMPTY string,
///   and only an index past the array yields the record's sentinel.
///
/// Indexing the trimmed label list for the singular form is what made the port
/// answer with a decimal index for an ENUM. Measured on the compiled C IOC
/// (`softIoc`, mbbi with ZRST/ONST set):
///
/// ```text
/// caput VAL 5  -> caget -t: []                 (slot 5 empty, still <= 15)
/// caput VAL 20 -> caget -t: [Illegal Value]    (past the 16 states)
/// blank mbbi   -> caget -t: []                 (no states at all)
/// ```
///
/// The out-of-range answer is NOT one rule — see [`EnumOverflow`].
#[derive(Debug, Clone, Default)]
pub struct EnumStringForm {
    /// The index-addressable state slots. An undefined state is an EMPTY slot,
    /// not a missing one — C `strncpy`s whatever is in the record.
    pub slots: Vec<PvString>,
    /// What an index past `slots` renders as. See [`EnumOverflow`]: the rule is
    /// the DBF class's, not one global fallback.
    pub overflow: EnumOverflow,
}

/// What an index PAST the slots renders as — and it is not one rule, because C
/// reaches the out-of-range case through a different converter per DBF class
/// (`dbFastLinkConv.c`, the scalar table `dbGet` uses for a one-element read):
///
/// * `DBF_ENUM` → `cvt_e_st_get` → the record's `get_enum_str` rset, whose
///   out-of-range answer is a per-record-type SENTINEL (`mbbi`/`mbbo`:
///   `"Illegal Value"`; `bi`/`bo`: `"Illegal_Value"`).
/// * `DBF_MENU` → `cvt_menu_st` (:1590-1596) — *"Convert out-of-range values to
///   numeric strings"*, `epicsSnprintf(to, MAX_STRING_SIZE, "%u", *from)`. This
///   is C's ONE numeric enum rendering, and it is reachable: `SSCN`'s declared
///   initial is 65535, past every menu, and a C IOC serves `caget -t REC.SSCN`
///   as `65535`.
/// * `DBF_DEVICE` → `cvt_device_st` (:1616-1620) — a record type with NO device
///   support is *"Valid"* and renders the EMPTY string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnumOverflow {
    /// A fixed string: the record's `get_enum_str` sentinel, or the empty string
    /// where C has no answer to give.
    Text(PvString),
    /// The index itself, decimal — C `cvt_menu_st`'s out-of-range branch.
    Decimal,
}

impl Default for EnumOverflow {
    fn default() -> Self {
        Self::Text(PvString::new())
    }
}

impl EnumStringForm {
    /// C `get_enum_str` / `cvt_menu_st` / `cvt_device_st`: the slot, or the
    /// class's out-of-range answer.
    pub fn render(&self, index: u16) -> PvString {
        match self.slots.get(index as usize) {
            Some(slot) => slot.clone(),
            None => match &self.overflow {
                EnumOverflow::Text(text) => text.clone(),
                EnumOverflow::Decimal => PvString::from(index.to_string().as_str()),
            },
        }
    }

    /// A `DBF_MENU` field's form: its `menu()` choices, and C's numeric-string
    /// rendering for an index past them.
    pub fn menu(choices: impl IntoIterator<Item = PvString>) -> Self {
        Self {
            slots: choices.into_iter().collect(),
            overflow: EnumOverflow::Decimal,
        }
    }

    /// A `DBF_DEVICE` field's form: the record type's device menu, and the empty
    /// string where the type declares no device support.
    pub fn device(choices: Vec<PvString>) -> Self {
        Self {
            slots: choices,
            overflow: EnumOverflow::Text(PvString::new()),
        }
    }

    /// A `DBF_ENUM` `VAL`'s form: the record's untrimmed state slots and its
    /// out-of-range sentinel.
    pub fn states(slots: Vec<PvString>, sentinel: PvString) -> Self {
        Self {
            slots,
            overflow: EnumOverflow::Text(sentinel),
        }
    }
}

/// Enum state strings (up to 16 states, each max 26 chars on wire).
///
/// Byte-preserving like [`DisplayInfo::units`]: enum choice labels are
/// raw wire/record bytes with no UTF-8 guarantee, so they must reach the
/// CA `DBR_GR_ENUM` slots and the PVA `value.choices` array unmangled.
///
/// `#[non_exhaustive]`: the two C rset slots this carries — the `no_str`-trimmed
/// label array and the [`EnumStringForm`] a `DBR_STRING` read indexes — must be
/// assigned together, so the struct is built through [`EnumInfo::new`] (labels
/// are their own state table: a menu, a plain enum channel) or
/// [`EnumInfo::with_string_form`] (a record whose `get_enum_str` differs from
/// its `get_enum_strs`). A bare literal could set one and leave the other empty,
/// which is the exact desync that made an enum serve its index as its string.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct EnumInfo {
    /// C `get_enum_strs` — the `DBR_GR_ENUM` label array, trimmed to `no_str`.
    pub strings: Vec<PvString>,
    /// C `get_enum_str` — how a `DBR_STRING` read of the field renders.
    pub string_form: EnumStringForm,
}

impl EnumInfo {
    /// A channel whose labels ARE its state table: every menu and device choice
    /// list, and any enum channel with no record `get_enum_str` behind it. The
    /// two C slots coincide, so one list fills both.
    pub fn new(strings: Vec<PvString>) -> Self {
        Self {
            string_form: EnumStringForm {
                slots: strings.clone(),
                overflow: EnumOverflow::default(),
            },
            strings,
        }
    }

    /// A record whose `get_enum_str` is NOT its `get_enum_strs`: `mbbi`'s label
    /// list stops at the last non-empty state while its `DBR_STRING` form still
    /// indexes all 16 slots and has an `"Illegal Value"` sentinel beyond them.
    pub fn with_string_form(strings: Vec<PvString>, string_form: EnumStringForm) -> Self {
        Self {
            strings,
            string_form,
        }
    }
}

/// Unified internal state representation for a PV read.
///
/// `#[non_exhaustive]` so future field additions (e.g. another DBR
/// variant's metadata, a new pvxs-style annotation) don't break
/// external code that constructs `Snapshot` via struct literal.
/// Internal call sites use `Snapshot::new` + field assignment; that
/// pattern is forward-compatible.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Snapshot {
    pub value: EpicsValue,
    pub alarm: AlarmInfo,
    /// Wall-clock timestamp with full EPICS `nsec` precision on every platform.
    /// Held as [`WallTime`] rather than [`std::time::SystemTime`] because the latter is
    /// 100 ns-granular on Windows and truncated externally supplied `nsec`
    /// (wire decode, PVA PUT, `Q:time:tag` split). [`Snapshot::new`] still
    /// accepts a `SystemTime` (via `Into`), so "now"-style call sites are
    /// unchanged; precise integer sources construct it with
    /// [`WallTime::from_unix`].
    pub timestamp: WallTime,
    pub display: Option<DisplayInfo>,
    pub control: Option<ControlInfo>,
    pub enums: Option<EnumInfo>,
    /// Which of the six metadata properties THIS channel actually supplies
    /// — the record type's `rset` slots, narrowed to the addressed field
    /// ([`PropertySupport::narrowed_to_field`]). Read `Snapshot::units` and
    /// friends rather than reaching into `display` / `control` directly
    /// when the consumer must not invent a value.
    pub properties: PropertySupport,
    /// Timestamp user tag (from Q:time:tag info, nsec LSB splitting).
    pub user_tag: i32,
    /// IOC record-type class name. Populated by the server before
    /// encoding a `DBR_CLASS_NAME` (38) response so the client receives
    /// the actual recordType (`ai`, `bo`, `waveform`, …) rather than an
    /// empty string. `None` for non-record-backed channels.
    pub class_name: Option<String>,
}

/// Split `snap.timestamp.nanoseconds` across `nanoseconds` / `user_tag`
/// along `nsec_mask` — the mask a record's `info(Q:time:tag,
/// "nsec:lsb:N")` resolves to (pvxs `MappingInfo::updateNsecMask`,
/// `typeutils.cpp:79-88`).
///
/// Mirrors pvxs `iocsource.cpp:239-248` byte for byte:
///
/// ```text
/// node["timeStamp.nanoseconds"] = meta.time.nsec & ~info.nsecMask;
/// if(info.nsecMask)
///     utag = meta.time.nsec & info.nsecMask;
/// ```
///
/// The mask — not a bit count — is the parameter, so "the feature is
/// off" has exactly one representation: `nsec_mask == 0`. pvxs gates the
/// `userTag` override on the same test, and `nsec & ~0` is the identity,
/// so a zero mask leaves both fields untouched. With the typical
/// `nsec:lsb:20` mask (`0x000F_FFFF`), `nanoseconds` keeps wall-clock
/// precision down to ~1 µs while the userTag carries a 20-bit event id.
pub fn apply_nsec_mask(snap: &mut Snapshot, nsec_mask: u64) {
    if nsec_mask == 0 {
        return;
    }
    // pvxs holds `nsecMask` as `uint64_t` and applies it to the
    // `epicsUInt32` `nsec`, so only the low 32 bits can ever take effect.
    let mask = nsec_mask as u32;
    let secs = snap.timestamp.unix_secs();
    let nanos = snap.timestamp.subsec_nanos();
    snap.user_tag = (nanos & mask) as i32;
    snap.timestamp = WallTime::from_unix(secs, nanos & !mask);
}

impl Snapshot {
    /// Create a new snapshot with minimal metadata (no display/control/enum info).
    ///
    /// `timestamp` accepts anything convertible into [`WallTime`] — a plain
    /// [`std::time::SystemTime`] (for "now"-style clock reads, which lose no precision the
    /// OS clock already lacks) or a [`WallTime::from_unix`] built from an exact
    /// integer `(secs, nsec)` taken off the wire.
    pub fn new(
        value: EpicsValue,
        status: u16,
        severity: u16,
        timestamp: impl Into<WallTime>,
    ) -> Self {
        Self {
            value,
            alarm: AlarmInfo {
                status,
                severity,
                ackt: None,
                acks: None,
                amsg: String::new(),
            },
            timestamp: timestamp.into(),
            display: None,
            control: None,
            enums: None,
            properties: PropertySupport::NONE,
            user_tag: 0,
            class_name: None,
        }
    }

    /// Engineering units, or `None` when the record type has no
    /// `get_units` slot. See [`PropertySupport`].
    pub fn units(&self) -> Option<&PvString> {
        let d = self.display.as_ref()?;
        self.properties.units.then_some(&d.units)
    }

    /// Display precision, or `None` when this channel does not supply it —
    /// the record type has no `get_precision` slot, or the addressed field
    /// is not a float/double ([`PropertySupport::narrowed_to_field`]).
    pub fn precision(&self) -> Option<i16> {
        let d = self.display.as_ref()?;
        self.properties.precision.then_some(d.precision)
    }

    /// `(lower, upper)` display limits, or `None` when the record type has
    /// no `get_graphic_double` slot.
    pub fn graphic_limits(&self) -> Option<(f64, f64)> {
        let d = self.display.as_ref()?;
        self.properties
            .graphic_double
            .then_some((d.lower_disp_limit, d.upper_disp_limit))
    }

    /// `(lower, upper)` control limits, or `None` when the record type has
    /// no `get_control_double` slot.
    pub fn control_limits(&self) -> Option<(f64, f64)> {
        let c = self.control.as_ref()?;
        self.properties
            .control_double
            .then_some((c.lower_ctrl_limit, c.upper_ctrl_limit))
    }

    /// `(lower, upper)` display limits, or `None` when the record type has no
    /// `get_graphic_double` slot — the counterpart of [`Self::control_limits`],
    /// gated on its own property bit because the two slots are independently
    /// nullable in C.
    pub fn display_limits(&self) -> Option<(f64, f64)> {
        let d = self.display.as_ref()?;
        self.properties
            .graphic_double
            .then_some((d.lower_disp_limit, d.upper_disp_limit))
    }

    /// `(lolo, low, high, hihi)` alarm limits, or `None` when the record
    /// type has no `get_alarm_double` slot — the `waveform` case that made
    /// a GUI draw alarm bands at zero.
    pub fn alarm_limits(&self) -> Option<(f64, f64, f64, f64)> {
        let d = self.display.as_ref()?;
        self.properties.alarm_double.then_some((
            d.lower_alarm_limit,
            d.lower_warning_limit,
            d.upper_warning_limit,
            d.upper_alarm_limit,
        ))
    }
}

/// Which metadata properties a record TYPE supplies — the six nullable
/// `get_*` slots of C's `rset`.
///
/// This is the primitive the port was missing. `dbGet` asks for every
/// property, then **clears** the `DBR_*` option bit of each slot the record
/// type left NULL (`dbAccess.c:336-430`: `*options ^= DBR_UNITS`,
/// `^= DBR_PRECISION`, `^= DBR_GR_DOUBLE`, `^= DBR_CTRL_DOUBLE`,
/// `^= DBR_AL_DOUBLE`, `^= DBR_ENUM_STRS`). QSRV then assigns each NT leaf
/// only under the surviving bit (pvxs `ioc/iocsource.cpp:263-305`), so a
/// record type with a NULL slot leaves that leaf **unmarked**.
///
/// Marking a fabricated default is worse than omitting it: the mark tells
/// the client the value is authoritative. A GUI reading alarm limits off a
/// `waveform` — whose C rset has no `get_alarm_double` — must see "not
/// provided", not bands drawn at zero.
///
/// The CA wire is deliberately unaffected: C also `memset`s the property
/// struct to zero and sends it, because the DBR layout is fixed-size. Only
/// consumers that inspect the narrowed `options` — i.e. QSRV/PVA — can see
/// the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PropertySupport {
    /// `rset.get_units`
    pub units: bool,
    /// `rset.get_precision`
    pub precision: bool,
    /// `rset.get_graphic_double`
    pub graphic_double: bool,
    /// `rset.get_control_double`
    pub control_double: bool,
    /// `rset.get_alarm_double`
    pub alarm_double: bool,
    /// `rset.get_enum_strs`
    pub enum_strs: bool,
}

impl PropertySupport {
    /// No slot implemented — a record type that supplies no metadata at all
    /// (C `stringin`, `stringout`, `lsi`, `lso`, `event`, `printf`, …).
    pub const NONE: Self = Self {
        units: false,
        precision: false,
        graphic_double: false,
        control_double: false,
        alarm_double: false,
        enum_strs: false,
    };

    /// Every numeric slot, no enum strings — the `ai`/`ao`/`calc` shape.
    pub const NUMERIC: Self = Self {
        units: true,
        precision: true,
        graphic_double: true,
        control_double: true,
        alarm_double: true,
        enum_strs: false,
    };

    /// The record type's slots narrowed to ONE addressed field — C's second
    /// gate, applied by the same `getOptions`:
    ///
    /// * `DBR_PRECISION` survives only for `DBF_FLOAT`/`DBF_DOUBLE`
    ///   (`dbAccess.c:386-395`), so an `ai`'s `.RVAL` (`DBF_LONG`) supplies
    ///   no precision even though the `ai` rset has `get_precision`;
    /// * `DBR_ENUM_STRS` is supplied by `DBF_MENU`/`DBF_DEVICE` fields from
    ///   the menu itself (no rset slot needed), by a `DBF_ENUM` field only
    ///   when the rset has `get_enum_strs`, and by nothing else
    ///   (`get_enum_strs`, `dbAccess.c:196-248`).
    ///
    /// A [`Snapshot`]'s `properties` is always the narrowed mask, so a
    /// consumer never has to re-apply either gate — "which leaves does THIS
    /// channel supply" has exactly one answer.
    pub fn narrowed_to_field(self, field_type: DbFieldType, is_menu_field: bool) -> Self {
        Self {
            precision: self.precision
                && matches!(field_type, DbFieldType::Float | DbFieldType::Double),
            enum_strs: matches!(field_type, DbFieldType::Enum) && (self.enum_strs || is_menu_field),
            ..self
        }
    }
}

/// Classification of DBR type ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbrClass {
    Plain,
    Sts,
    Time,
    Gr,
    Ctrl,
}

impl DbrClass {
    /// Classify a DBR type code into its range.
    pub fn from_dbr_type(dbr_type: u16) -> Option<Self> {
        match dbr_type {
            0..=6 => Some(DbrClass::Plain),
            7..=13 => Some(DbrClass::Sts),
            14..=20 => Some(DbrClass::Time),
            21..=27 => Some(DbrClass::Gr),
            28..=34 => Some(DbrClass::Ctrl),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    /// With the `nsec:lsb:20` mask, the low 20 nanosecond bits land in
    /// userTag and are cleared from the timestamp. Use a known nanosecond
    /// value so the bit math is easy to verify.
    #[test]
    fn nsec_mask_extracts_user_tag() {
        let nanos: u32 = 123_456_789; // 0x075BCD15 — sub-second
        // Inject the exact integer (secs, nsec); a `SystemTime` would truncate
        // these low nanosecond digits to 100 ns on Windows before the split.
        let ts = WallTime::from_unix(42, nanos);
        let mut snap = Snapshot::new(EpicsValue::Double(0.0), 0, 0, ts);
        apply_nsec_mask(&mut snap, (1 << 20) - 1);
        let mask: u32 = (1 << 20) - 1;
        let expected_user_tag = (nanos & mask) as i32;
        let expected_nanos = nanos & !mask;
        assert_eq!(snap.user_tag, expected_user_tag);
        assert_eq!(snap.timestamp.unix_secs(), 42);
        assert_eq!(snap.timestamp.subsec_nanos(), expected_nanos);
    }

    /// The `nsec:lsb:1` mask splits the single LSB into the userTag.
    #[test]
    fn nsec_mask_n1_keeps_high_bits() {
        let ts = WallTime::from_unix(0, 7); // ...0111
        let mut snap = Snapshot::new(EpicsValue::Double(0.0), 0, 0, ts);
        apply_nsec_mask(&mut snap, 1);
        assert_eq!(snap.user_tag, 1);
        assert_eq!(snap.timestamp.subsec_nanos(), 6);
    }

    /// Mask 0 (pvxs's `nsecMask` initialiser — no `Q:time:tag`, or one that
    /// does not parse) is the off state: `nsec & ~0` is the identity and
    /// pvxs's `if(info.nsecMask)` gate leaves `userTag` at the record's own
    /// UTAG. Neither field may be touched.
    #[test]
    fn nsec_mask_zero_is_a_no_op() {
        let ts = WallTime::from_unix(9, 123_456_789);
        let mut snap = Snapshot::new(EpicsValue::Double(0.0), 0, 0, ts);
        snap.user_tag = -7; // record's own UTAG
        apply_nsec_mask(&mut snap, 0);
        assert_eq!(snap.user_tag, -7);
        assert_eq!(snap.timestamp.unix_secs(), 9);
        assert_eq!(snap.timestamp.subsec_nanos(), 123_456_789);
    }

    /// `nsec:lsb:31` — the mask pvxs builds one past the old Rust `1..=30`
    /// clamp. `nanoseconds` is always < 1e9 < 2^30, so a 31-bit mask moves
    /// the whole nanosecond field into `userTag` and publishes 0 nanoseconds
    /// (pvxs `iocsource.cpp:239-248`).
    #[test]
    fn nsec_mask_31_moves_all_nanoseconds_to_user_tag() {
        let nanos: u32 = 999_999_999;
        let ts = WallTime::from_unix(1, nanos);
        let mut snap = Snapshot::new(EpicsValue::Double(0.0), 0, 0, ts);
        apply_nsec_mask(&mut snap, (1u64 << 31) - 1);
        assert_eq!(snap.user_tag, nanos as i32);
        assert_eq!(snap.timestamp.subsec_nanos(), 0);
        assert_eq!(snap.timestamp.unix_secs(), 1);
    }

    /// pvxs holds `nsecMask` as a `uint64_t` but applies it to the 32-bit
    /// `nsec`, so a mask wider than 32 bits behaves exactly like the
    /// all-ones 32-bit mask.
    #[test]
    fn nsec_mask_wider_than_32_bits_clears_all_nanoseconds() {
        let nanos: u32 = 123_456_789;
        let ts = WallTime::from_unix(3, nanos);
        let mut snap = Snapshot::new(EpicsValue::Double(0.0), 0, 0, ts);
        apply_nsec_mask(&mut snap, (1u64 << 40) - 1);
        assert_eq!(snap.user_tag, nanos as i32);
        assert_eq!(snap.timestamp.subsec_nanos(), 0);
    }

    #[test]
    fn test_snapshot_construction() {
        let snap = Snapshot::new(EpicsValue::Double(42.0), 0, 0, SystemTime::UNIX_EPOCH);
        assert_eq!(snap.alarm.status, 0);
        assert_eq!(snap.alarm.severity, 0);
        assert!(snap.display.is_none());
        assert!(snap.control.is_none());
        assert!(snap.enums.is_none());
    }

    #[test]
    fn test_snapshot_with_metadata() {
        let mut snap = Snapshot::new(EpicsValue::Double(3.14), 1, 2, SystemTime::UNIX_EPOCH);
        snap.display = Some(DisplayInfo {
            units: "degC".into(),
            precision: 3,
            upper_disp_limit: 100.0,
            lower_disp_limit: -50.0,
            upper_alarm_limit: 90.0,
            upper_warning_limit: 80.0,
            lower_warning_limit: -20.0,
            lower_alarm_limit: -40.0,
            ..Default::default()
        });
        snap.control = Some(ControlInfo {
            upper_ctrl_limit: 100.0,
            lower_ctrl_limit: -50.0,
        });
        let disp = snap.display.as_ref().unwrap();
        assert_eq!(disp.units, "degC");
        assert_eq!(disp.precision, 3);
        assert_eq!(snap.control.as_ref().unwrap().upper_ctrl_limit, 100.0);
    }

    #[test]
    fn test_dbr_class_plain() {
        for t in 0..=6 {
            assert_eq!(DbrClass::from_dbr_type(t), Some(DbrClass::Plain));
        }
    }

    #[test]
    fn test_dbr_class_all_ranges() {
        // STS: 7-13
        for t in 7..=13 {
            assert_eq!(DbrClass::from_dbr_type(t), Some(DbrClass::Sts));
        }
        // TIME: 14-20
        for t in 14..=20 {
            assert_eq!(DbrClass::from_dbr_type(t), Some(DbrClass::Time));
        }
        // GR: 21-27
        for t in 21..=27 {
            assert_eq!(DbrClass::from_dbr_type(t), Some(DbrClass::Gr));
        }
        // CTRL: 28-34
        for t in 28..=34 {
            assert_eq!(DbrClass::from_dbr_type(t), Some(DbrClass::Ctrl));
        }
    }

    #[test]
    fn test_dbr_class_invalid() {
        assert_eq!(DbrClass::from_dbr_type(35), None);
        assert_eq!(DbrClass::from_dbr_type(100), None);
        assert_eq!(DbrClass::from_dbr_type(u16::MAX), None);
    }
}
