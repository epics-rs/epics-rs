use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

use chrono::Local;

/// EPICS epoch: 1990-01-01 00:00:00 UTC
const EPICS_EPOCH_OFFSET: i64 = 631152000;

/// Timestamp format strings indexed by TST field value.
const TIMESTAMP_FORMATS: &[&str] = &[
    "%y/%m/%d %H:%M:%S", // 0
    "%m/%d/%y %H:%M:%S", // 1
    "%b %d %H:%M:%S %y", // 2
    "%b %d %H:%M:%S",    // 3
    "%H:%M:%S",          // 4
    "%H:%M",             // 5
    "%d/%m/%y %H:%M:%S", // 6
    "%d %b %H:%M:%S %y", // 7
    "%d-%b-%Y %H:%M:%S", // 8: VMS format
];

/// Timestamp record — generates formatted timestamp strings.
///
/// Ported from EPICS std module `timestampRecord.c`.
pub struct TimestampRecord {
    /// Current formatted timestamp string (VAL).
    pub val: String,
    /// Previous value for change detection (OVAL).
    pub oval: String,
    /// Seconds past EPICS epoch (RVAL).
    pub rval: i32,
    /// Timestamp format selector 0–10 (TST).
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

        if sec_past_epoch <= 0 {
            return ("-NULL-".to_string(), 0);
        }

        let tst = self.tst.clamp(0, 10) as usize;

        let formatted = if tst <= 8 {
            now.format(TIMESTAMP_FORMATS[tst]).to_string()
        } else {
            // Formats 9 and 10 include milliseconds
            let ms = now.timestamp_subsec_millis();
            let base = if tst == 9 {
                now.format("%b %d %Y %H:%M:%S").to_string()
            } else {
                // tst == 10
                now.format("%m/%d/%y %H:%M:%S").to_string()
            };
            format!("{}.{:03}", base, ms)
        };

        (formatted, sec_past_epoch)
    }
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
                    self.val = v;
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
