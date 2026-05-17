use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

use chrono::Local;

/// EPICS epoch: 1990-01-01 00:00:00 UTC
const EPICS_EPOCH_OFFSET: i64 = 631152000;

/// Maximum length of the VAL/OVAL string fields.
///
/// `timestampRecord.dbd` declares `VAL`/`OVAL` with `size(40)`, and C
/// `timestampRecord.c:140` calls `epicsTimeToStrftime(val, sizeof(val), ...)`
/// which truncates to that buffer (39 chars + NUL).
const VAL_SIZE: usize = 40;

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
}

impl Default for TimestampRecord {
    fn default() -> Self {
        Self {
            val: String::new(),
            oval: String::new(),
            rval: 0,
            tst: 0,
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
        let now = Local::now();
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

        // C `epicsTimeToStrftime(val, sizeof(val), ...)` bounds the
        // result to the 40-byte VAL buffer (`timestampRecord.dbd`
        // `size(40)`).
        (truncate_to(formatted, VAL_SIZE), sec_past_epoch)
    }
}

/// Truncate `s` to at most `max` bytes, respecting UTF-8 char boundaries.
///
/// C stores VAL/OVAL in a fixed `char[40]` buffer; the formatted output
/// must never exceed it. Timestamp format strings only ever emit ASCII,
/// so this is a plain byte truncation in practice.
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
            "VAL" => Some(EpicsValue::String(self.val.clone())),
            "OVAL" => Some(EpicsValue::String(self.oval.clone())),
            "RVAL" => Some(EpicsValue::Long(self.rval)),
            "TST" => Some(EpicsValue::Short(self.tst)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::String(v) => {
                    // VAL is a `char[40]` field in C; truncate to match.
                    self.val = truncate_to(v, VAL_SIZE);
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

    fn clears_udf(&self) -> bool {
        true
    }
}
