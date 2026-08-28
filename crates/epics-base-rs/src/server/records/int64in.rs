use epics_macros_rs::EpicsRecord;

use crate::server::record::MENU_YES_NO;
use crate::types::PvString;

/// `int64inRecord.c:225` `get_control_double` lists one field the shared
/// VAL-class set does not: `SVAL`, which takes the record's own `HOPR`/`LOPR`
/// like `VAL` does. Without this it falls to the `default:` arm and reports
/// the DBF_INT64 range of ±9223372036854775807.
///
/// The third and last member of the SVAL family — `aiRecord.c:280` and
/// `longinRecord.c:230` list it too, and this record type was the one left
/// out. `int64outRecord.c:251-277` does NOT list SVAL (an output record has
/// no simulation buffer to serve), so the family ends here.
fn int64in_metadata_override(
    rec: &Int64inRecord,
    field: &str,
) -> Option<crate::server::record::FieldMetadataOverride> {
    field
        .eq_ignore_ascii_case("SVAL")
        .then(|| crate::server::record::FieldMetadataOverride {
            ctrl_limits: Some((rec.hopr as f64, rec.lopr as f64)),
            ..Default::default()
        })
}

// int64in: 64-bit integer input.
// CA limitation: served as DBR_DOUBLE over Channel Access (f64, precision loss for |val|>2^53).
// Native i64 storage is lossless; precision is only lost at the CA wire boundary.
//
// Alarm threshold fields (HIHI/HIGH/LOW/LOLO/HHSV/HSV/LSV/LLSV) are intentionally absent
// from the field list so they route to RecordInstance::common.analog_alarm via
// put_common_field, matching the path used by longin/ao/ai.
#[derive(EpicsRecord)]
// `dset_owns_udf_on_computed`: C `int64inRecord.c:144` is
// `if (status==0) prec->udf = FALSE;` with no `status == 2` fold, the longin
// twin. `devI64inSoft.c::readLocked` likewise returns the `dbGetLink` status,
// never 2.
#[record(
    type = "int64in",
    metadata_override = int64in_metadata_override,
    dset_owns_udf_on_computed
)]
pub struct Int64inRecord {
    #[field(type = "Int64")]
    pub val: i64,
    #[field(type = "PvStr")]
    pub egu: PvString,
    // HOPR/LOPR/HYST/ADEL/MDEL are DBF_INT64 (int64inRecord.dbd.pod:140-268),
    // not DBF_DOUBLE. Modeling them as i64 keys the string→native put parse on
    // the integer row (`epicsParseInt64`), so a fractional or out-of-i64-range
    // caput is REFUSED, matching C; served over CA as DBR_DOUBLE via
    // `EpicsValue::Int64`'s wire mapping, the same as VAL.
    #[field(type = "Int64")]
    pub hopr: i64,
    #[field(type = "Int64")]
    pub lopr: i64,
    #[field(type = "Int64")]
    pub hyst: i64,
    // LALM is DBF_INT64 too, but `special(SPC_NOMOD)` (dbd:233-236): read-only,
    // so no client put reaches the parse. It is the alarm ladder's own
    // `epicsInt64 lalm` (`int64inRecord.c:262`) — the value the hysteresis
    // arm compares a threshold against — so it holds that threshold exactly
    // rather than as a rounded double.
    #[field(type = "Int64")]
    pub lalm: i64,
    #[field(type = "Int64")]
    pub adel: i64,
    #[field(type = "Int64")]
    pub mdel: i64,
    // Alarm-range time-constant filter (int64inRecord.c::checkAlarms:303-349).
    // AFTC > 0 low-pass-filters the integer alarmRange so transient
    // excursions don't immediately alarm; AFVL is the accumulator.
    #[field(type = "Double")]
    pub aftc: f64,
    #[field(type = "Double")]
    pub afvl: f64,
    #[field(type = "Double")]
    pub alst: f64,
    #[field(type = "Double")]
    pub mlst: f64,
    // SIMM is `DBF_MENU menu(menuYesNo)` (int64inRecord.dbd.pod:279-283):
    // the two-choice NO/YES simulation menu, served as DBR_ENUM.
    #[field(type = "Short", menu_choices = MENU_YES_NO)]
    pub simm: i16,
    #[field(type = "String")]
    pub siml: String,
    #[field(type = "String")]
    pub siol: String,
    // SVAL is `DBF_INT64` (int64inRecord.dbd.pod:270-272) — the BUFFER C's
    // `readValue` reads SIOL into (`dbGetLink(&prec->siol, DBR_INT64,
    // &prec->sval)`, int64inRecord.c:409) before publishing `val = sval`.
    #[field(type = "Int64")]
    pub sval: i64,
    #[field(type = "Short")]
    pub sims: i16,
    // SDLY — "Sim. Mode Async Delay" (`DBF_DOUBLE`, `initial("-1.0")`,
    // int64inRecord.dbd.pod:301-307). A non-negative SDLY makes the simulated SIOL read asynchronous:
    // C's `readValue` arms `callbackRequestProcessCallbackDelayed(..., sdly)`
    // and holds PACT across the delay (int64inRecord.c:398-405). The framework reads the delay
    // via `resolve_field("SDLY")`, which falls back to the dbd `initial`, so
    // this field is the record's own store for it rather than a requirement.
    #[field(type = "Double")]
    pub sdly: f64,
}

impl Default for Int64inRecord {
    fn default() -> Self {
        Self {
            val: 0,
            egu: PvString::new(),
            hopr: 0,
            lopr: 0,
            hyst: 0,
            lalm: 0,
            adel: 0,
            mdel: 0,
            aftc: 0.0,
            afvl: 0.0,
            alst: 0.0,
            mlst: 0.0,
            simm: 0,
            siml: String::new(),
            siol: String::new(),
            sval: 0,
            sims: 0,
            // C `field(SDLY,DBF_DOUBLE) { initial("-1.0") }` — negative means
            // "synchronous simulation".
            sdly: -1.0,
        }
    }
}

impl Int64inRecord {
    pub fn new(val: i64) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }
}
