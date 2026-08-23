use epics_macros_rs::EpicsRecord;

use crate::server::record::MENU_YES_NO;
use crate::types::PvString;

/// `longinRecord.c:206-224` `get_control_double` lists one field the shared
/// VAL-class set does not: `SVAL`, which takes the record's own `HOPR`/`LOPR`
/// like `VAL` does. Without this it falls to the `default:` arm and reports
/// the DBF_LONG range of ±2147483647.
fn longin_metadata_override(
    rec: &LonginRecord,
    field: &str,
) -> Option<crate::server::record::FieldMetadataOverride> {
    field
        .eq_ignore_ascii_case("SVAL")
        .then(|| crate::server::record::FieldMetadataOverride {
            ctrl_limits: Some((rec.hopr as f64, rec.lopr as f64)),
            ..Default::default()
        })
}

#[derive(EpicsRecord)]
#[record(type = "longin", metadata_override = longin_metadata_override)]
pub struct LonginRecord {
    #[field(type = "Long")]
    pub val: i32,
    #[field(type = "PvStr")]
    pub egu: PvString,
    #[field(type = "Long")]
    pub hopr: i32,
    #[field(type = "Long")]
    pub lopr: i32,
    // Alarm thresholds (HIHI/HIGH/LOW/LOLO), severities (HHSV/HSV/LSV/LLSV)
    // and the alarm hysteresis (HYST) are NOT record-owned fields: they are
    // dbCommon-adjacent state owned by `CommonFields` — the limits/severities
    // in `CommonFields::analog_alarm` and HYST in `CommonFields::hyst`. The
    // record's `checkAlarms` ladder (`RecordInstance::evaluate_analog_alarm`)
    // reads them from there, so a record-local copy would be a second,
    // never-consulted owner: a `caput LI.HHSV INVALID` stored into a shadow
    // field left `analog_alarm.hhsv` at NO_ALARM and the alarm never fired.
    // Omitting the fields routes their get/put through the common-field path
    // (the single owner), exactly as `ai`/`int64in` do. LALM (last-alarmed
    // value) below IS record state — `evaluate_analog_alarm` reads it via
    // `self.record.get_field("LALM")` — so it stays. It is C's
    // `epicsInt32 lalm` (`longinRecord.c:269`), the type its `.dbd` declares
    // and the type the ladder compares in.
    #[field(type = "Long")]
    pub lalm: i32,
    // Deadband. ADEL/MDEL are `DBF_LONG` (longinRecord.dbd.pod) — the archive
    // and monitor deadbands are integer counts, not doubles. Stored as `i32`
    // (not `f64`) so the client-put target resolves to `DBF_LONG` and a string
    // put parses through the single `c_parse::put_string` owner's `Long` row,
    // which REFUSES an over-i32/under-i32 value exactly as C's `epicsParseInt32`
    // does — instead of the `DBF_DOUBLE` row accepting it and the read-path
    // projection saturating it to `i32::MAX`/`MIN`. Mirrors Cause A/B of the
    // put-conversion family (int64in/int64out DBF_INT64, commit 224d5ad5).
    #[field(type = "Long")]
    pub adel: i32,
    #[field(type = "Long")]
    pub mdel: i32,
    // Alarm-range time-constant filter (longinRecord.c::checkAlarms:310-356).
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
    // SIMM is `DBF_MENU menu(menuYesNo)` (longinRecord.dbd.pod:475-479):
    // the two-choice NO/YES simulation menu, served as DBR_ENUM.
    #[field(type = "Short", menu_choices = MENU_YES_NO)]
    pub simm: i16,
    #[field(type = "String")]
    pub siml: String,
    #[field(type = "String")]
    pub siol: String,
    // SVAL is `DBF_LONG` (longinRecord.dbd.pod:466-468) — the BUFFER C's
    // `readValue` reads SIOL into (`dbGetLink(&prec->siol, DBR_LONG,
    // &prec->sval)`, longinRecord.c:416) before publishing `val = sval`. An
    // unset (constant) SIOL reads status 0 without touching it, which is what
    // makes `caput REC.SIMM 1; caput REC.SVAL 42` simulate against a constant.
    #[field(type = "Long")]
    pub sval: i32,
    #[field(type = "Short")]
    pub sims: i16,
    // SDLY — "Sim. Mode Async Delay" (`DBF_DOUBLE`, `initial("-1.0")`,
    // longinRecord.dbd.pod:497-503). A non-negative SDLY makes the simulated SIOL read asynchronous:
    // C's `readValue` arms `callbackRequestProcessCallbackDelayed(..., sdly)`
    // and holds PACT across the delay (longinRecord.c:405-412). The framework reads the delay
    // via `get_field("SDLY")`, so the field must exist for a `.db` to set it.
    #[field(type = "Double")]
    pub sdly: f64,
}

impl Default for LonginRecord {
    fn default() -> Self {
        Self {
            val: 0,
            egu: PvString::new(),
            hopr: 0,
            lopr: 0,
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

impl LonginRecord {
    pub fn new(val: i32) -> Self {
        Self {
            val,
            ..Default::default()
        }
    }
}
