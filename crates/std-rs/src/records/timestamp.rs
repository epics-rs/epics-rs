use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::record::{
    EPICS_TIME_EVENT_DEVICE_TIME, FieldDesc, ProcessContext, ProcessOutcome, Record, ValuePostGate,
};
use epics_base_rs::types::{EpicsValue, PvString};

use super::dbd_generated;
use chrono::{Local, TimeZone};

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
    pub val: PvString,
    /// Previous value for change detection (OVAL).
    pub oval: PvString,
    /// Seconds past EPICS epoch (RVAL). DBF_ULONG in C; the Rust value
    /// model has no unsigned-32 scalar, so this follows the project
    /// convention of mapping DBF_ULONG to `i32`/`EpicsValue::Long`.
    /// `field(RVAL,DBF_ULONG)` (`timestampRecord.dbd:28`) — C
    /// `ptimestamp->rval = ptimestamp->time.secPastEpoch` (`timestampRecord.c:94`),
    /// and `secPastEpoch` is an `epicsUInt32`. Stored `i32` and served
    /// `EpicsValue::Long` while the port hand-wrote its own field table.
    pub rval: u32,
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
            val: PvString::new(),
            oval: PvString::new(),
            rval: 0,
            tst: 0,
            tse: 0,
        }
    }
}

impl TimestampRecord {
    fn format_timestamp(&self) -> (PvString, u32) {
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
        let sec_past_epoch = (unix_secs - EPICS_EPOCH_OFFSET) as u32;

        // C `timestampRecord.c:96`: `if (time.secPastEpoch == 0)` — the
        // "-NULL-" sentinel is emitted only when the EPICS-epoch second
        // count is exactly zero (an uninitialised/unset time stamp), not
        // for any non-positive value.
        if sec_past_epoch == 0 {
            return (PvString::from("-NULL-"), sec_past_epoch);
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
                // C `epicsTime.cpp:234-239`: the `%03f` fractional field
                // ROUNDS to the nearest millisecond (see
                // `round_subsec_to_millis`). `timestamp_subsec_millis()`
                // (= nsec / 1e6) truncates instead, shifting every value
                // on a half-ms boundary down by one.
                let ms = round_subsec_to_millis(now.timestamp_subsec_nanos());
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
        (
            truncate_to(PvString::from(formatted), VAL_VISIBLE_MAX),
            sec_past_epoch,
        )
    }
}

/// Truncate `s` to at most `max` bytes.
///
/// C stores VAL/OVAL in a fixed `char[40]` buffer whose last byte is the
/// NUL terminator, so at most 39 visible bytes survive. C `strftime`
/// truncates the buffer byte for byte, so this cut is on a raw byte
/// boundary and a non-UTF-8 VAL written by a client round-trips verbatim.
fn truncate_to(s: PvString, max: usize) -> PvString {
    if s.len() > max {
        PvString::from_bytes(s.as_bytes()[..max].to_vec())
    } else {
        s
    }
}

/// Round a sub-second nanosecond count to a 3-digit millisecond field.
///
/// C `epicsTime.cpp:234-239` renders the `%03f` fractional field by
/// ROUNDING the nanoseconds to the nearest millisecond, with a clamp
/// that prevents the rounded value from carrying into whole seconds:
/// ```text
/// frac = nsec + div[fracWid]/2;            // div[3] = 1e6, so +5e5
/// if (frac >= 1000000000) frac = 1000000000 - 1;
/// frac /= div[fracWid];                    // /1e6 -> 0..=999
/// ```
/// A naive `nsec / 1_000_000` truncates, biasing every value on a
/// half-millisecond boundary down by one (e.g. 1.7 ms → `.001` instead
/// of `.002`). The `min` clamp keeps a near-`1e9` nanosecond count from
/// rounding up to `1000` ms (which would need a carry into the seconds
/// field C never performs here).
fn round_subsec_to_millis(nsec: u32) -> u32 {
    let frac = (nsec + 500_000).min(1_000_000_000 - 1);
    frac / 1_000_000
}

impl Record for TimestampRecord {
    fn record_type(&self) -> &'static str {
        "timestamp"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let (formatted, sec_past_epoch) = self.format_timestamp();
        // RVAL is refreshed on every process (`timestampRecord.c:94`) — a caget
        // of RVAL between posts reads the current second — so only the
        // *posting* is gated, never the value. The OVAL compare and copy are
        // C's `monitor()`'s, and live in `monitor_value_changed`.
        self.val = formatted;
        self.rval = sec_past_epoch;
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::String(self.val.clone())),
            "OVAL" => Some(EpicsValue::String(self.oval.clone())),
            "RVAL" => Some(EpicsValue::ULong(self.rval)),
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
                    self.val = truncate_to(v, VAL_VISIBLE_MAX);
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "RVAL" => match value {
                EpicsValue::ULong(v) => {
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

    fn declared_fields(&self) -> &'static [FieldDesc] {
        dbd_generated::TIMESTAMP_FIELDS
    }

    /// C `timestampRecord.c:90` reads `ptimestamp->tse`. The framework
    /// owns `dbCommon.tse`; this hook captures it so `process()` can
    /// take the device-time branch.
    fn set_process_context(&mut self, ctx: &ProcessContext) {
        self.tse = ctx.tse;
    }

    /// The timestamp record has NO value deadband: its monitored
    /// quantity is the formatted `VAL` string, and `timestampRecord.dbd`
    /// declares no MDEL/ADEL. C `monitor()` (`timestampRecord.c:152-163`)
    /// posts `VAL` (and `RVAL`) only inside
    /// `if (strncmp(oval, val, sizeof(val)))` — i.e. exactly when the
    /// formatted string changed since the previous process — then copies
    /// `val` into `oval`. That is plain change-detection, not a deadband.
    ///
    /// The framework's snapshot builders force-post the deadband field on
    /// every cycle the deadband gate fires (and the gate always fires for
    /// a non-numeric value — see [`epics_base_rs::server::record::RecordInstance::check_deadband_ext`],
    /// whose `to_f64()` returns `None` for a string `VAL`). Returning the
    /// default `"VAL"` here would therefore re-post `VAL` on every scan,
    /// even when the rendered string is unchanged — diverging from C's
    /// `strncmp` gate. Returning `""` (a name no field resolves to)
    /// suppresses that force-post (`resolve_field("")` is `None`, so the
    /// `if let Some(val) = dval` push is skipped) and routes `VAL`
    /// through the generic change-detection loop, which posts it only
    /// when it differs from the last posted value — matching C exactly.
    fn monitor_deadband_field(&self) -> &'static str {
        ""
    }

    /// C `monitor()`'s single gate: the `strncmp(oval, val)` string change
    /// (`timestampRecord.c:158`). Reported here so the framework's VAL monitor
    /// mask is live only on a cycle that re-rendered a different string — which
    /// is also the gate `RVAL` hangs off (see
    /// [`Self::fields_posted_with_value_mask`]).
    /// C `timestampRecord.c:158-162` `monitor()`: the `strncmp(oval, val)`
    /// mismatch posts VAL *and* RVAL and then copies VAL into OVAL. Compared
    /// and committed HERE, at C's position, never captured during `process()`.
    fn monitor_value_changed(&mut self) -> Option<bool> {
        let changed = self.oval != self.val;
        if changed {
            self.oval = self.val.clone();
        }
        Some(changed)
    }

    /// C posts `RVAL` from *inside* the VAL-string-change guard, with VAL's own
    /// monitor mask and with no test of RVAL's own value
    /// (`db_post_events(&ptimestamp->rval, monitor_mask)`,
    /// `timestampRecord.c:160`) — so the seconds count reaches monitors exactly
    /// when the rendered string moves, and no more often.
    ///
    /// Left to the generic change-detection loop instead, `RVAL` posts on every
    /// process that crosses a second — ~59 spurious `DBE_VALUE|DBE_LOG` events a
    /// minute per subscriber under a coarse TST such as `HH:MM`, whose VAL only
    /// changes once a minute.
    fn fields_posted_with_value_mask(&self) -> &'static [(&'static str, ValuePostGate)] {
        &[("RVAL", ValuePostGate::WithValue)]
    }

    fn clears_udf(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod subsec_round_tests {
    use super::round_subsec_to_millis;

    // C `epicsTime.cpp:234-239` rounds the `%03f` fractional field to
    // the nearest millisecond; the previous `nsec / 1e6` truncated.
    #[test]
    fn rounds_to_nearest_millisecond() {
        // Below the half-ms point: rounds down.
        assert_eq!(round_subsec_to_millis(0), 0);
        assert_eq!(round_subsec_to_millis(499_999), 0);
        assert_eq!(round_subsec_to_millis(1_400_000), 1);
        // Exactly half a millisecond: C's `+ div/2` rounds up.
        assert_eq!(round_subsec_to_millis(500_000), 1);
        assert_eq!(round_subsec_to_millis(1_500_000), 2);
        // Above the half-ms point: rounds up — the case truncation got
        // wrong (1.7 ms truncated to .001, now rounds to .002).
        assert_eq!(round_subsec_to_millis(1_700_000), 2);
    }

    // The clamp keeps a near-1e9 nanosecond count from rounding up to
    // 1000 ms (C `if (frac >= 1e9) frac = 1e9 - 1`), which would need a
    // carry into the seconds field the record never performs.
    #[test]
    fn clamps_instead_of_carrying_into_seconds() {
        assert_eq!(round_subsec_to_millis(999_500_000), 999);
        assert_eq!(round_subsec_to_millis(999_999_999), 999);
    }
}

#[cfg(test)]
mod menu_choice_tests {
    use super::{TimestampRecord, dbd_generated};
    use epics_base_rs::server::record::FieldDeclaration;
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

    /// The choices are the DECLARATION's, not a record hook's: `TST` is
    /// `DBF_MENU menu(timestampTST)` in `timestampRecord.dbd`, so its
    /// `FieldDesc` carries the choices and every consumer reads them from
    /// there. This used to assert a hand-written `TIMESTAMP_TST_CHOICES` that
    /// `menu_field_choices` returned — a second declaration of the same menu.
    #[test]
    fn timestamp_tst_choices_come_from_the_declaration() {
        let rec = TimestampRecord::default();
        let tst = rec
            .field_list()
            .iter()
            .find(|f| f.name == "TST")
            .expect("TST is declared");
        assert_eq!(tst.menu, Some(dbd_generated::MENU_TIMESTAMP_TST));
        let val = rec
            .field_list()
            .iter()
            .find(|f| f.name == "VAL")
            .expect("VAL is declared");
        assert_eq!(val.menu, None);
    }

    // C `timestampRecord.c:152-163`: `monitor()` posts VAL (and RVAL)
    // only inside `if (strncmp(oval, val))` — when the formatted string
    // changed. There is no value deadband. The record routes VAL through
    // the framework's generic change-detection loop by reporting an
    // empty deadband field; the framework's deadband force-post is then
    // skipped because that name resolves to nothing.
    #[test]
    fn timestamp_has_no_deadband_field_so_val_change_detects() {
        let rec = TimestampRecord::default();
        // No numeric value deadband — the sentinel routes VAL to the
        // change-detection loop (cf. motor's "RBV").
        assert_eq!(rec.monitor_deadband_field(), "");

        // The framework's deadband force-post fires only
        // `if let Some(val) = resolve_field(deadband_field)`. The "" name
        // must resolve to None so VAL is never force-posted on an
        // unchanged-string cycle.
        let inst = RecordInstance::new("TS:DB".into(), TimestampRecord::default());
        assert_eq!(inst.resolve_field(""), None);
    }
}
