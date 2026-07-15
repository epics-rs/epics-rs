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
#[derive(Clone, Debug)]
pub struct AnalogAlarmConfig {
    pub hihi: f64,
    pub high: f64,
    pub low: f64,
    pub lolo: f64,
    pub hhsv: i16,
    pub hsv: i16,
    pub lsv: i16,
    pub llsv: i16,
}

impl Default for AnalogAlarmConfig {
    fn default() -> Self {
        Self {
            hihi: 0.0,
            high: 0.0,
            low: 0.0,
            lolo: 0.0,
            hhsv: 0,
            hsv: 0,
            lsv: 0,
            llsv: 0,
        }
    }
}
