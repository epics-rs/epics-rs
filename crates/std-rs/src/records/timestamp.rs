use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::record::{
    EPICS_TIME_EVENT_DEVICE_TIME, FieldDesc, ProcessContext, ProcessOutcome, Record,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};

use chrono::{Local, TimeZone};

/// Choice labels for the timestamp string-format menu, in index order.
/// C `menu(timestampTST)` (std `timestampRecord.dbd`): `TST` selects which
/// of these `strftime`-style formats `VAL` is rendered with. The
/// index↔string mapping is wire-visible to clients.
const TIMESTAMP_TST_CHOICES: &[&str] = &[
    "YY/MM/DD HH:MM:SS",
    "MM/DD/YY HH:MM:SS",
    "Mon DD HH:MM:SS YY",
    "Mon DD HH:MM:SS",
    "HH:MM:SS",
    "HH:MM",
    "DD/MM/YY HH:MM:SS",
    "DD Mon HH:MM:SS YY",
    "DD-Mon-YYYY HH:MM:SS",
    "Mon DD, YYYY HH:MM:SS.ns",
    "MM/DD/YY HH:MM:SS.ns",
];

/// EPICS epoch: 1990-01-01 00:00:00 UTC
const EPICS_EPOCH_OFFSET: i64 = 631152000;

/// Maximum number of visible (non-NUL) bytes in the VAL/OVAL fields.
///
/// `timestampRecord.dbd` declares `VAL`/`OVAL` as `char val[40]`, and C
/// `timestampRecord.c:140` calls `epicsTimeToStrftime(val, sizeof(val), ...)`.
/// `epicsTimeToStrftime` wraps `strftime`, which writes at most
/// `sizeof(val)` bytes *including* the terminating NUL — so the buffer
/// holds at most 39 visible characters. A Rust `String` carries no NUL
/// terminator, so the visible-byte bound is 39, not 40.
const VAL_VISIBLE_MAX: usize = 39;

/// Timestamp format strings indexed by TST field value.
///
/// Mirrors the `switch(tst)` in `timestampRecord.c:100-138`. Any TST value
/// outside `0..=10` falls through C's `default:` branch to format 0
/// (`YY/MM/DD HH:MM:SS`).
const TIMESTAMP_FORMATS: &[&str] = &[
    "%y/%m/%d %H:%M:%S", // 0  timestampTST_YY_MM_DD_HH_MM_SS
    "%m/%d/%y %H:%M:%S", // 1  timestampTST_MM_DD_YY_HH_MM_SS
    "%b %d %H:%M:%S %y", // 2  timestampTST_MM_DD_HH_MM_SS_YY
    "%b %d %H:%M:%S",    // 3  timestampTST_MM_DD_HH_MM_SS
    "%H:%M:%S",          // 4  timestampTST_HH_MM_SS
    "%H:%M",             // 5  timestampTST_HH_MM
    "%d/%m/%y %H:%M:%S", // 6  timestampTST_DD_MM_YY_HH_MM_SS
    "%d %b %H:%M:%S %y", // 7  timestampTST_DD_MM_HH_MM_SS_YY
    "%d-%b-%Y %H:%M:%S", // 8  timestampTST_VMS
];

/// Timestamp record — generates formatted timestamp strings.
///
/// Ported from EPICS std module `timestampRecord.c`.
pub struct TimestampRecord {
    /// Current formatted timestamp string (VAL).
    pub val: String,
    /// Previous value for change detection (OVAL).
    pub oval: String,
    /// Seconds past EPICS epoch (RVAL). DBF_ULONG in C; the Rust value
    /// model has no unsigned-32 scalar, so this follows the project
    /// convention of mapping DBF_ULONG to `i32`/`EpicsValue::Long`.
    pub rval: i32,
    /// Timestamp format selector (TST), a DBF_MENU. Values `0..=10`
    /// select an explicit format; any other value is rendered with
    /// format 0 (C `switch` `default:` branch).
    pub tst: i16,
    /// Framework-owned `dbCommon.tse`, pushed via
    /// [`Record::set_process_context`] before `process()`. C
    /// `timestampRecord.c:90` branches on
    /// `tse == epicsTimeEventDeviceTime`: device-time takes the raw OS
    /// clock (`epicsTimeFromTime_t(&time, time(0))`, whole seconds, no
    /// fraction); any other value uses the EPICS time-stamp framework.
    tse: i16,
}

impl Default for TimestampRecord {
    fn default() -> Self {
        Self {
            val: String::new(),
            oval: String::new(),
            rval: 0,
            tst: 0,
            tse: 0,
        }
    }
}

static FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OVAL",
        dbf_type: DbFieldType::String,
        read_only: true,
    },
    FieldDesc {
        name: "RVAL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "TST",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
];

impl TimestampRecord {
    fn format_timestamp(&self) -> (String, i32) {
        // C `timestampRecord.c:90-93`: `tse == epicsTimeEventDeviceTime`
        // takes the raw OS clock via `epicsTimeFromTime_t(&time, time(0))`
        // — whole seconds only, the nanosecond field is zero. Any other
        // TSE value goes through `recGblGetTimeStamp`, which carries
        // sub-second precision. The Rust port mirrors the observable
        // difference: device-time truncates `now` to whole seconds so
        // the `.%03f` formats (TST 9/10) render `.000`.
        let now = if self.tse == EPICS_TIME_EVENT_DEVICE_TIME {
            let secs = Local::now().timestamp();
            // `timestamp_opt(secs, 0)` is always `Single` for any
            // in-range Unix second; fall back to the un-truncated clock
            // on the impossible `None`/`Ambiguous` case rather than
            // panicking.
            Local
                .timestamp_opt(secs, 0)
                .single()
                .unwrap_or_else(Local::now)
        } else {
            Local::now()
        };
        let unix_secs = now.timestamp();
        let sec_past_epoch = (unix_secs - EPICS_EPOCH_OFFSET) as i32;

        // C `timestampRecord.c:96`: `if (time.secPastEpoch == 0)` — the
        // "-NULL-" sentinel is emitted only when the EPICS-epoch second
        // count is exactly zero (an uninitialised/unset time stamp), not
        // for any non-positive value.
        if sec_past_epoch == 0 {
            return ("-NULL-".to_string(), sec_past_epoch);
        }

        // C `timestampRecord.c:100-138`: any TST outside the valid menu
        // range falls through `default:` to format 0. The raw TST value
        // is preserved (the field is a plain menu); only the format
        // *selection* is bounded here.
        let tst = self.tst;

        let formatted = match tst {
            0..=8 => now.format(TIMESTAMP_FORMATS[tst as usize]).to_string(),
            // Formats 9 (timestampTST_MM_DD_YYYY) and 10
            // (timestampTST_MM_DD_YY) carry `.%03f` fractional seconds.
            // C `timestampRecord.c:130,133`. EPICS `%03f` is the
            // 3-digit fractional-seconds field derived from the time
            // stamp's nanoseconds; `subsec_millis()` is the equivalent
            // 3-digit truncation of the same fraction.
            9 | 10 => {
                let ms = now.timestamp_subsec_millis();
                let base = if tst == 9 {
                    now.format("%b %d %Y %H:%M:%S").to_string()
                } else {
                    now.format("%m/%d/%y %H:%M:%S").to_string()
                };
                format!("{base}.{ms:03}")
            }
            // C `default:` branch — format 0 (`YY/MM/DD HH:MM:SS`).
            _ => now.format(TIMESTAMP_FORMATS[0]).to_string(),
        };

        // C `timestampRecord.c:140` `epicsTimeToStrftime(val, sizeof(val), ...)`
        // bounds the result to the `char val[40]` buffer; `strftime` keeps
        // one byte for the NUL terminator, so at most 39 visible chars.
        (truncate_to(formatted, VAL_VISIBLE_MAX), sec_past_epoch)
    }
}

/// Truncate `s` to at most `max` bytes, respecting UTF-8 char boundaries.
///
/// C stores VAL/OVAL in a fixed `char[40]` buffer whose last byte is the
/// NUL terminator, so at most 39 visible bytes survive. Timestamp format
/// strings only ever emit ASCII, so this is a plain byte truncation in
/// practice.
fn truncate_to(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut cut = max;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

impl Record for TimestampRecord {
    fn record_type(&self) -> &'static str {
        "timestamp"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let (formatted, sec_past_epoch) = self.format_timestamp();
        self.oval = std::mem::replace(&mut self.val, formatted);
        self.rval = sec_past_epoch;
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::String(self.val.clone().into())),
            "OVAL" => Some(EpicsValue::String(self.oval.clone().into())),
            "RVAL" => Some(EpicsValue::Long(self.rval)),
            "TST" => Some(EpicsValue::Short(self.tst)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::String(v) => {
                    // VAL is a `char[40]` field in C; the last byte is the
                    // NUL terminator, so 39 visible bytes at most.
                    self.val = truncate_to(v.as_str_lossy().into_owned(), VAL_VISIBLE_MAX);
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "RVAL" => match value {
                EpicsValue::Long(v) => {
                    self.rval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "TST" => match value {
                EpicsValue::Short(v) => {
                    // TST is a plain DBF_MENU field — C stores whatever
                    // value is written and `format_timestamp` selects
                    // the format via a `switch` whose `default:` branch
                    // covers any out-of-range value. Do NOT clamp here:
                    // C `timestampRecord.dbd` declares no field range,
                    // and a read-back must reflect the raw value.
                    self.tst = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OVAL" => Err(CaError::ReadOnlyField(name.into())),
            _ => Err(CaError::FieldNotFound(name.into())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        FIELDS
    }

    /// `TST` is `DBF_MENU menu(timestampTST)` (std `timestampRecord.dbd`);
    /// served as `DBR_ENUM` with the format-string choice labels.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "TST" => Some(TIMESTAMP_TST_CHOICES),
            _ => None,
        }
    }

    /// C `timestampRecord.c:90` reads `ptimestamp->tse`. The framework
    /// owns `dbCommon.tse`; this hook captures it so `process()` can
    /// take the device-time branch.
    fn set_process_context(&mut self, ctx: &ProcessContext) {
        self.tse = ctx.tse;
    }

    fn clears_udf(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod menu_choice_tests {
    use super::{TIMESTAMP_TST_CHOICES, TimestampRecord};
    use epics_base_rs::server::record::{Record, RecordInstance};
    use epics_base_rs::types::EpicsValue;

    // TST is menu(timestampTST) served as Short; the base snapshot path
    // promotes it to DBR_ENUM and attaches the wire-visible format labels.
    #[test]
    fn timestamp_tst_snapshot_is_enum_with_labels() {
        let mut rec = TimestampRecord::default();
        rec.put_field("TST", EpicsValue::Short(4)).unwrap(); // HH:MM:SS
        let inst = RecordInstance::new("TS:TST".into(), rec);

        let snap = inst.snapshot_for_field("TST").unwrap();
        assert_eq!(snap.value, EpicsValue::Enum(4));
        let strings = &snap.enums.as_ref().unwrap().strings;
        assert_eq!(strings.len(), 11);
        assert_eq!(strings[4], "HH:MM:SS");
    }

    #[test]
    fn timestamp_menu_field_choices_match_dbd() {
        let rec = TimestampRecord::default();
        assert_eq!(rec.menu_field_choices("TST"), Some(TIMESTAMP_TST_CHOICES));
        assert_eq!(rec.menu_field_choices("VAL"), None);
    }
}
