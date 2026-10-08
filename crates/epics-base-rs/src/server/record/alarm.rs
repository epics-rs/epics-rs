use crate::types::EpicsValue;

/// Alarm severity levels matching EPICS base.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
#[repr(u16)]
pub enum AlarmSeverity {
    #[default]
    NoAlarm = 0,
    Minor = 1,
    Major = 2,
    Invalid = 3,
}

impl AlarmSeverity {
    pub fn from_u16(v: u16) -> Self {
        match v {
            0 => Self::NoAlarm,
            1 => Self::Minor,
            2 => Self::Major,
            3 => Self::Invalid,
            _ => Self::Invalid,
        }
    }
}

/// Analog alarm configuration — only for ai/ao/longin/longout.
///
/// The four severity selectors (`HHSV`/`HSV`/`LSV`/`LLSV`, each
/// `DBF_MENU menu(menuAlarmSevr)`) hold the **raw stored ordinal**, not a
/// clamped [`AlarmSeverity`]. C stores whatever `(epicsEnum16)` a numeric put
/// truncates to (`dbConvert.c::putDoubleEnum` = `*pfield = (epicsEnum16)val`),
/// so `caput REC.HSV 4` keeps `4` and `caput REC.HSV -1` keeps `65535` — both
/// wire-visible and both used verbatim to derive the alarm (C's `if (prec->hsv)`
/// is a nonzero test, and `recGblResetAlarms` clamps the resulting `nsev` to
/// `INVALID_ALARM`, not the field). Modeled as `i16` so the 16-bit pattern
/// round-trips, matching sel/dfanout's already-raw `hhsv`/… fields; read the
/// alarm meaning with `AlarmSeverity::from_u16(field as u16)` and the C nonzero
/// enable with `field != 0`.
///
/// The four limits are held as one array in the record's own domain, in the
/// ladder's order, so the cycle compares them straight off the slot: C reads
/// `prec->hihi` as a struct member, and a per-limit `AlarmLimit` cost a match
/// and a conversion for each of the four on every cycle.
#[derive(Clone, Copy, Debug)]
pub struct AnalogAlarmConfig {
    limits: AlarmLadder,
    pub hhsv: i16,
    pub hsv: i16,
    pub lsv: i16,
    pub llsv: i16,
}

/// `[HIHI, LOLO, HIGH, LOW]` — the order [`analog_alarm_range`] tests them —
/// in the one type the record's `.dbd` declares all four with.
#[derive(Clone, Copy, Debug)]
enum AlarmLadder {
    Double([f64; 4]),
    Long([i32; 4]),
    Int64([i64; 4]),
}

/// One of the four analog-alarm limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlarmLimitField {
    Hihi = 0,
    Lolo = 1,
    High = 2,
    Low = 3,
}

impl AnalogAlarmConfig {
    /// A slot for a record whose limits are `DBF_DOUBLE`, every limit and
    /// severity zero as a fresh C record has them.
    pub fn double() -> Self {
        Self::of(AlarmLadder::Double([0.0; 4]))
    }

    /// A slot for a record whose limits are `DBF_LONG` (longin/longout).
    pub fn long() -> Self {
        Self::of(AlarmLadder::Long([0; 4]))
    }

    /// A slot for a record whose limits are `DBF_INT64` (int64in/int64out).
    pub fn int64() -> Self {
        Self::of(AlarmLadder::Int64([0; 4]))
    }

    fn of(limits: AlarmLadder) -> Self {
        Self {
            limits,
            hhsv: 0,
            hsv: 0,
            lsv: 0,
            llsv: 0,
        }
    }

    /// The limit as stored, in the type its `.dbd` row declares.
    pub fn limit(&self, field: AlarmLimitField) -> AlarmLimit {
        let i = field as usize;
        match self.limits {
            AlarmLadder::Double(l) => AlarmLimit::Double(l[i]),
            AlarmLadder::Long(l) => AlarmLimit::Long(l[i]),
            AlarmLadder::Int64(l) => AlarmLimit::Int64(l[i]),
        }
    }

    /// Store a limit. The put has already been coerced to the field's
    /// declared type, so the casts are reached only by a value that arrives
    /// in another domain, and take C's `dbPut` conversion.
    pub fn set_limit(&mut self, field: AlarmLimitField, value: AlarmLimit) {
        let i = field as usize;
        match &mut self.limits {
            AlarmLadder::Double(l) => l[i] = value.as_f64(),
            AlarmLadder::Long(l) => {
                l[i] = match value {
                    AlarmLimit::Long(v) => v,
                    AlarmLimit::Int64(v) => v as i32,
                    AlarmLimit::Double(v) => v as i32,
                }
            }
            AlarmLadder::Int64(l) => {
                l[i] = match value {
                    AlarmLimit::Int64(v) => v,
                    AlarmLimit::Long(v) => v as i64,
                    AlarmLimit::Double(v) => v as i64,
                }
            }
        }
    }

    /// C `checkAlarms`' `alarmRange` for `val`, compared in VAL's own domain:
    /// `f64` for a `DBF_DOUBLE` VAL, `i128` for `DBF_LONG`/`DBF_INT64` so
    /// `alev - hyst` cannot overflow. `hyst` `None` is a type that keeps HYST
    /// in `CommonFields`, passed as `common_hyst`.
    pub(crate) fn range(
        &self,
        val: AlarmLimit,
        hyst: Option<AlarmLimit>,
        lalm: AlarmLimit,
        common_hyst: f64,
    ) -> u16 {
        let sevs = [self.hhsv, self.llsv, self.hsv, self.lsv];
        match val {
            AlarmLimit::Double(v) => {
                let limits = match self.limits {
                    AlarmLadder::Double(l) => l,
                    AlarmLadder::Long(l) => l.map(f64::from),
                    AlarmLadder::Int64(l) => l.map(|x| x as f64),
                };
                analog_alarm_range(
                    v,
                    hyst.map_or(common_hyst, AlarmLimit::as_f64),
                    lalm.as_f64(),
                    limits,
                    sevs,
                )
            }
            AlarmLimit::Long(_) | AlarmLimit::Int64(_) => {
                let v = val.as_i128();
                let limits = match self.limits {
                    AlarmLadder::Double(l) => l.map(|x| x as i128),
                    AlarmLadder::Long(l) => l.map(i128::from),
                    AlarmLadder::Int64(l) => l.map(i128::from),
                };
                analog_alarm_range(
                    v,
                    hyst.map_or(common_hyst as i128, AlarmLimit::as_i128),
                    lalm.as_i128(),
                    limits,
                    sevs,
                )
            }
        }
    }

    /// C `range_stat[]` plus the severity and `alev` each range selects. C
    /// reaches this mapping twice — out of the ladder and out of the AFTC
    /// filter's `switch (alarmRange)` — so it is one table.
    pub(crate) fn resolve(&self, range: u16) -> (AlarmSeverity, u16, Option<AlarmLimit>) {
        use crate::server::recgbl::alarm_status;
        let (sev, stat, field) = match range {
            5 => (self.hhsv, alarm_status::HIHI_ALARM, AlarmLimitField::Hihi),
            4 => (self.hsv, alarm_status::HIGH_ALARM, AlarmLimitField::High),
            2 => (self.lsv, alarm_status::LOW_ALARM, AlarmLimitField::Low),
            1 => (self.llsv, alarm_status::LOLO_ALARM, AlarmLimitField::Lolo),
            _ => return (AlarmSeverity::NoAlarm, alarm_status::NO_ALARM, None),
        };
        (
            AlarmSeverity::from_u16(sev as u16),
            stat,
            Some(self.limit(field)),
        )
    }
}

/// One analog-alarm limit — `HIHI`/`HIGH`/`LOW`/`LOLO` — in the record's own
/// numeric domain.
///
/// C declares the four limits with the record's VAL type and compares them
/// against VAL with no conversion anywhere: `epicsFloat64` on
/// ai/ao/calc/calcout/sub/scalcout, `epicsInt32` on longin/longout
/// (`longinRecord.dbd.pod`), `epicsInt64` on int64in/int64out
/// (`int64inRecord.dbd.pod:152-176`, and `int64inRecord.c:262-264`
/// `epicsInt64 val, hyst, lalm; epicsInt64 alev;`). Holding every limit as
/// `f64` rounded an `epicsInt64` limit above 2^53, so
/// `field(HIHI,"9007199254740993")` on an `int64in` stored 9007199254740992
/// and alarmed one count early — at exactly the nanosecond-timestamp
/// magnitudes `int64` records exist for.
///
/// One variant per C declaration, so a limit is served back in the type its
/// `.dbd` row names as well as compared in it. Which variant a limit holds is
/// decided once, from the record's own `.dbd` row, by the common-field
/// coercion owner — never from the field's name.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AlarmLimit {
    Double(f64),
    Long(i32),
    Int64(i64),
}

impl Default for AlarmLimit {
    fn default() -> Self {
        Self::Double(0.0)
    }
}

impl AlarmLimit {
    /// The stored value, in the `f64` alarm domain.
    pub fn as_f64(self) -> f64 {
        match self {
            Self::Double(v) => v,
            Self::Long(v) => v as f64,
            Self::Int64(v) => v as f64,
        }
    }

    /// The stored value, in the integer alarm domain, widened to `i128` so
    /// `alev - hyst` cannot overflow the way C's `epicsInt64` arithmetic does.
    pub fn as_i128(self) -> i128 {
        match self {
            Self::Double(v) => v as i128,
            Self::Long(v) => v as i128,
            Self::Int64(v) => v as i128,
        }
    }

    /// The value a `dbGet` on the limit serves, in its stored type.
    pub fn to_epics_value(self) -> EpicsValue {
        match self {
            Self::Double(v) => EpicsValue::Double(v),
            Self::Long(v) => EpicsValue::Long(v),
            Self::Int64(v) => EpicsValue::Int64(v),
        }
    }

    /// The VAL the ladder compares, or `None` for a stored variant the ladder
    /// has no arm for: C's `checkAlarms` exists only on the records whose VAL
    /// is `DBF_DOUBLE`, `DBF_LONG` or `DBF_INT64`, and the variant is what
    /// picks the comparison domain, so it is kept rather than flattened.
    pub fn from_ladder_value(value: &EpicsValue) -> Option<Self> {
        match value {
            EpicsValue::Double(v) => Some(Self::Double(*v)),
            EpicsValue::Long(v) => Some(Self::Long(*v)),
            EpicsValue::Int64(v) => Some(Self::Int64(*v)),
            _ => None,
        }
    }

    /// The limit a coerced put carries. The put has already been projected
    /// onto the field's declared `.dbd` type by the common-field coercion
    /// owner, so the integer arms are reached exactly for the record types
    /// whose `.dbd` says `DBF_LONG`/`DBF_INT64`.
    pub fn from_stored(value: &EpicsValue) -> Option<Self> {
        match value {
            EpicsValue::Int64(v) => Some(Self::Int64(*v)),
            EpicsValue::Long(v) => Some(Self::Long(*v)),
            other => other.to_f64().map(Self::Double),
        }
    }
}

impl std::fmt::Display for AlarmLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Double(v) => write!(f, "{v}"),
            Self::Long(v) => write!(f, "{v}"),
            Self::Int64(v) => write!(f, "{v}"),
        }
    }
}

/// C `checkAlarms`' four-arm limit ladder, returning C's integer `alarmRange`
/// — 5 = Hihi, 4 = High, 3 = Normal, 2 = Low, 1 = Lolo
/// (`int64inRecord.c:248-259`, `:273-301`).
///
/// C compiles this ladder once per record type, in that record's VAL type, so
/// the port instantiates it once per domain: `f64` for the `DBF_DOUBLE`
/// records and `i128` for the `DBF_LONG`/`DBF_INT64` ones. Running every
/// record through the `f64` instantiation is what let an `epicsInt64`
/// comparison round — see [`AlarmLimit`].
pub(crate) fn analog_alarm_range<T>(
    val: T,
    hyst: T,
    lalm: T,
    [hihi, lolo, high, low]: [T; 4],
    [hhsv, llsv, hsv, lsv]: [i16; 4],
) -> u16
where
    T: Copy + PartialOrd + std::ops::Add<Output = T> + std::ops::Sub<Output = T>,
{
    if hhsv != 0 && (val >= hihi || (lalm == hihi && val >= hihi - hyst)) {
        5
    } else if llsv != 0 && (val <= lolo || (lalm == lolo && val <= lolo + hyst)) {
        1
    } else if hsv != 0 && (val >= high || (lalm == high && val >= high - hyst)) {
        4
    } else if lsv != 0 && (val <= low || (lalm == low && val <= low + hyst)) {
        2
    } else {
        3
    }
}
