//! Choice-string tables for the EPICS shared `menu(...)` definitions, and
//! the field-name registry that maps a globally-consistent menu field to
//! its table.
//!
//! In EPICS dbStaticLib a `DBF_MENU` field is served to clients as
//! `DBR_ENUM`: the value is the menu index and the field carries its
//! `menu()` choice strings, so `caget`/`pvget` present the labels rather
//! than a bare number (`dbStaticLib.c` `dbGetMenuChoices`; `dbAccess.c`
//! `get_enum_str`). A handful of menus are *shared* — the same `menu()`
//! is referenced by the same field name across every record type
//! (`HHSV`/`HSV`/`LSV`/`LLSV`/... are always `menuAlarmSevr`, `OMSL` is
//! always `menuOmsl`, and so on). Those tables are defined once here and
//! keyed by field name in [`shared_menu_choices`], so a record never
//! restates the mapping. Record-*specific* menus (`sel.SELM`,
//! `compress.ALG`, ...) stay with their record via
//! [`crate::server::record::Record::menu_field_choices`].
//!
//! A name only belongs in [`shared_menu_choices`] when it maps to the
//! *same* `menu()` — same membership *and* same value order — in every
//! record that declares it. Two names that look shared are not, and are
//! resolved per record instead:
//!
//! * `SIMM` is `menu(menuSimm)` (NO/YES/RAW) on the analog/binary/multibit
//!   records (`aiRecord.dbd.pod`), but `menu(menuYesNo)` (NO/YES) on the
//!   long/string/array records (`longinRecord.dbd.pod`,
//!   `waveformRecord.dbd.pod`). Its saved copy `OLDSIMM` *is* always
//!   `menuSimm`, so only `OLDSIMM` stays shared here.
//! * `MPST`/`APST` are `menu(menuPost)` (On Change, Always) on `lsi`/`lso`,
//!   but record-specific POST menus whose value order is *reversed*
//!   ("Always" first) on `aai`/`aao`/`waveform`
//!   (`aaiRecord.dbd.pod` `menu(aaiPOST)`). The order is wire-visible, so
//!   a single shared table would mislabel those records.
//!
//! The choice order MUST match the `menu()` value order in the upstream
//! `.dbd` exactly: the index↔string mapping is wire-visible to clients.
//! Each table cites its source.

use crate::types::{DbFieldType, EpicsValue};

/// `menu(menuAlarmSevr)` — `menuAlarmSevr.dbd.pod:21-24`.
pub const MENU_ALARM_SEVR: &[&str] = &["NO_ALARM", "MINOR", "MAJOR", "INVALID"];

/// `menu(menuAlarmStat)` — `menuAlarmStat.dbd.pod:87-109`. The index order is
/// `alarm.h`'s `epicsAlarmCondition`, which `recGblSetSevr` stores in
/// `STAT`/`NSTA`.
pub const MENU_ALARM_STAT: &[&str] = &[
    "NO_ALARM",
    "READ",
    "WRITE",
    "HIHI",
    "HIGH",
    "LOLO",
    "LOW",
    "STATE",
    "COS",
    "COMM",
    "TIMEOUT",
    "HWLIMIT",
    "CALC",
    "SCAN",
    "LINK",
    "SOFT",
    "BAD_SUB",
    "UDF",
    "DISABLE",
    "SIMM",
    "READ_ACCESS",
    "WRITE_ACCESS",
];

/// `menu(menuPini)` — `menuPini.dbd.pod:59-65`. See
/// [`PiniMode`](crate::server::record::PiniMode) for the lifecycle point each
/// choice selects.
pub const MENU_PINI: &[&str] = &["NO", "YES", "RUN", "RUNNING", "PAUSE", "PAUSED"];

/// `menu(menuSimm)` — `menuSimm.dbd.pod:20-22`.
pub const MENU_SIMM: &[&str] = &["NO", "YES", "RAW"];

/// `menu(menuScan)` — `menuScan.dbd.pod:47-57`.
pub const MENU_SCAN: &[&str] = &[
    "Passive",
    "Event",
    "I/O Intr",
    "10 second",
    "5 second",
    "2 second",
    "1 second",
    ".5 second",
    ".2 second",
    ".1 second",
];

/// `menu(menuOmsl)` — `menuOmsl.dbd.pod:23-24`.
pub const MENU_OMSL: &[&str] = &["supervisory", "closed_loop"];

/// `menu(menuIvoa)` — `menuIvoa.dbd.pod:20-22`.
pub const MENU_IVOA: &[&str] = &[
    "Continue normally",
    "Don't drive outputs",
    "Set output to IVOV",
];

/// `menu(menuConvert)` — `menuConvert.dbd.pod:23-37`.
pub const MENU_CONVERT: &[&str] = &[
    "NO CONVERSION",
    "SLOPE",
    "LINEAR",
    "typeKdegF",
    "typeKdegC",
    "typeJdegF",
    "typeJdegC",
    "typeEdegF(ixe only)",
    "typeEdegC(ixe only)",
    "typeTdegF",
    "typeTdegC",
    "typeRdegF",
    "typeRdegC",
    "typeSdegF",
    "typeSdegC",
];

/// `menu(menuYesNo)` — `menuYesNo.dbd.pod:28-29`.
pub const MENU_YES_NO: &[&str] = &["NO", "YES"];

/// `menu(menuPost)` — `menuPost.dbd.pod:19-20`.
pub const MENU_POST: &[&str] = &["On Change", "Always"];

/// `menu(menuPriority)` — `menuPriority.dbd.pod:25-27`.
pub const MENU_PRIORITY: &[&str] = &["LOW", "MEDIUM", "HIGH"];

/// `menu(menuFtype)` — `menuFtype.dbd.pod:19-30`.
pub const MENU_FTYPE: &[&str] = &[
    "STRING", "CHAR", "UCHAR", "SHORT", "USHORT", "LONG", "ULONG", "INT64", "UINT64", "FLOAT",
    "DOUBLE", "ENUM",
];

/// Choice table for a *shared* menu field, keyed by its (uppercase) field
/// name, or `None` for a name that is not a shared menu field.
///
/// These names reference the same `menu()` — same membership and value
/// order — in every record type that declares them, so the mapping is
/// global rather than per-record. A record-specific menu (`SELM`, `ALG`,
/// `OOPT`, ...) is **not** listed here; nor is a name whose menu varies by
/// record (`SIMM`, `MPST`/`APST` — see the module docs). Those stay on the
/// record's
/// [`menu_field_choices`](crate::server::record::Record::menu_field_choices)
/// override.
pub fn shared_menu_choices(field: &str) -> Option<&'static [&'static str]> {
    match field {
        // Alarm-severity menus (`menuAlarmSevr`): the analog limit
        // severities, the bi/bo/mbbi/mbbo state severities, the change-of-
        // state severity, the sub/aSub bad-return severity, and the
        // simulation-mode alarm severity all share one menu.
        //
        // The dbCommon severities belong to the same menu and are therefore
        // served as `DBR_ENUM` with these labels, exactly like every other
        // `DBF_MENU`: `SEVR` (`dbCommon.dbd.pod:302`), `NSEV` (`:318`),
        // `ACKS` (`:329`), `DISS` (`:343`), `UDFS` (`:556`).
        "SEVR" | "NSEV" | "ACKS" | "DISS" | "UDFS" | "HHSV" | "HSV" | "LSV" | "LLSV" | "ZSV"
        | "OSV" | "COSV" | "UNSV" | "BRSV" | "ZRSV" | "ONSV" | "TWSV" | "THSV" | "FRSV"
        | "FVSV" | "SXSV" | "SVSV" | "EISV" | "NISV" | "TESV" | "ELSV" | "TVSV" | "TTSV"
        | "FTSV" | "FFSV" | "SIMS" => Some(MENU_ALARM_SEVR),
        // dbCommon alarm status (`menuAlarmStat`) — `dbCommon.dbd.pod:296`
        // (`STAT`) and `:312` (`NSTA`).
        "STAT" | "NSTA" => Some(MENU_ALARM_STAT),
        // dbCommon alarm-ack transient (`menuYesNo`) — `dbCommon.dbd.pod:335`.
        "ACKT" => Some(MENU_YES_NO),
        // dbCommon process-at-init (`menuPini`) — `dbCommon.dbd.pod:169`.
        "PINI" => Some(MENU_PINI),
        // Saved simulation mode (`menuSimm`, always). The live `SIMM` field
        // is *not* shared — `menuSimm` (NO/YES/RAW) on analog/binary/multibit
        // records but `menuYesNo` (NO/YES) elsewhere — so it is resolved by
        // each record's `menu_field_choices`, not here.
        "OLDSIMM" => Some(MENU_SIMM),
        // Simulation-mode scan rate (`menuScan`); `.SCAN` itself is served
        // by the common-field path.
        "SSCN" => Some(MENU_SCAN),
        // Output mode select (`menuOmsl`).
        "OMSL" => Some(MENU_OMSL),
        // Invalid-output action (`menuIvoa`).
        "IVOA" => Some(MENU_IVOA),
        // Linear-conversion select (`menuConvert`).
        "LINR" => Some(MENU_CONVERT),
        // Post-overflow / circular-buffer-full flag (`menuYesNo`).
        "PBUF" => Some(MENU_YES_NO),
        // `MPST`/`APST` are deliberately absent: `menu(menuPost)` on
        // `lsi`/`lso` but record-specific POST menus with a *reversed* value
        // order on `aai`/`aao`/`waveform`, so they are resolved per record.
        // Array element type (`menuFtype`).
        "FTVL" => Some(MENU_FTYPE),
        // Record scan priority (`menuPriority`).
        "PRIO" => Some(MENU_PRIORITY),
        _ => None,
    }
}

/// Resolve a `DBF_MENU` field's textual value — a `.db` field directive or a
/// `DBR_STRING` write — against THAT field's own choice table, exactly like C
/// `dbStaticLib` `dbPutStringNum` and `dbConvert` `putStringMenu`: an exact,
/// case-sensitive label match first (`dbGetMenuIndexFromString`'s `strcmp`),
/// then a bare numeric index (`epicsParseUInt16`). The resolved index is typed
/// to the field's stored `dbf_type` so it lands in the record's `put_field`
/// without a second coercion.
///
/// Returns `None` when the text is neither a known label nor a numeric index —
/// the caller then reports the C `S_db_badChoice` error rather than guessing.
/// A menu label is menu-*specific*, so resolution MUST use the field's own
/// `choices` (this function's argument), never a cross-menu global table: the
/// same label can name different indices in different menus (e.g. "Specified"
/// is index 1 of `menuFanout` but index 0 of `selSELM`).
pub fn resolve_menu_field_string(
    choices: &[&str],
    dbf_type: DbFieldType,
    s: &str,
) -> Option<EpicsValue> {
    let trimmed = s.trim();
    let index = match choices.iter().position(|c| *c == trimmed) {
        Some(i) => i as i64,
        None => trimmed.parse::<i64>().ok()?,
    };
    Some(menu_index_value(dbf_type, index))
}

/// Type a resolved menu index to the field's stored `dbf_type`. A `DBF_MENU`
/// field stores its `epicsEnum16` index as `Enum` (e.g. `sel.SELM`) or `Short`
/// (e.g. `ai.LINR`); the other integer widths are covered defensively.
fn menu_index_value(dbf_type: DbFieldType, index: i64) -> EpicsValue {
    match dbf_type {
        DbFieldType::Enum => EpicsValue::Enum(index as u16),
        DbFieldType::Short => EpicsValue::Short(index as i16),
        DbFieldType::UShort => EpicsValue::UShort(index as u16),
        DbFieldType::Char => EpicsValue::Char(index as u8),
        DbFieldType::UChar => EpicsValue::UChar(index as u8),
        DbFieldType::Long => EpicsValue::Long(index as i32),
        DbFieldType::ULong => EpicsValue::ULong(index as u32),
        DbFieldType::Int64 => EpicsValue::Int64(index),
        DbFieldType::UInt64 => EpicsValue::UInt64(index as u64),
        // A menu field is always one of the integer widths above; an
        // unexpected type means the caller mis-classified the field, so emit
        // the index as `Short` and let `put_field` reject a true mismatch.
        _ => EpicsValue::Short(index as i16),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alarm_severity_order_matches_dbd() {
        // menuAlarmSevr value order is wire-visible (e.g. `caget rec.HHSV`
        // shows "MAJOR" for index 2).
        assert_eq!(MENU_ALARM_SEVR[0], "NO_ALARM");
        assert_eq!(MENU_ALARM_SEVR[2], "MAJOR");
        assert_eq!(shared_menu_choices("HHSV"), Some(MENU_ALARM_SEVR));
        assert_eq!(shared_menu_choices("COSV"), Some(MENU_ALARM_SEVR));
        assert_eq!(shared_menu_choices("SIMS"), Some(MENU_ALARM_SEVR));
    }

    #[test]
    fn simm_includes_raw_third_choice() {
        // menuSimm has a third "RAW" choice beyond NO/YES.
        assert_eq!(MENU_SIMM, &["NO", "YES", "RAW"]);
        // Only the saved copy `OLDSIMM` is always `menuSimm`. The live
        // `SIMM` field is per-record (menuSimm vs menuYesNo) and must not
        // be answered globally here.
        assert_eq!(shared_menu_choices("OLDSIMM"), Some(MENU_SIMM));
        assert_eq!(shared_menu_choices("SIMM"), None);
    }

    #[test]
    fn shared_names_map_to_their_menu() {
        assert_eq!(shared_menu_choices("OMSL"), Some(MENU_OMSL));
        assert_eq!(shared_menu_choices("IVOA"), Some(MENU_IVOA));
        assert_eq!(shared_menu_choices("LINR"), Some(MENU_CONVERT));
        assert_eq!(shared_menu_choices("SSCN"), Some(MENU_SCAN));
        assert_eq!(shared_menu_choices("FTVL"), Some(MENU_FTYPE));
        assert_eq!(shared_menu_choices("PRIO"), Some(MENU_PRIORITY));
        assert_eq!(shared_menu_choices("PBUF"), Some(MENU_YES_NO));
    }

    #[test]
    fn record_varying_menu_names_are_not_shared() {
        // SIMM and MPST/APST map to different menus (or different value
        // orders) across records, so they are resolved per record and must
        // return None from the global registry.
        assert_eq!(shared_menu_choices("SIMM"), None);
        assert_eq!(shared_menu_choices("MPST"), None);
        assert_eq!(shared_menu_choices("APST"), None);
    }

    #[test]
    fn dbcommon_menu_fields_resolve_to_their_menu() {
        // Every `DBF_MENU` field of dbCommon is served as `DBR_ENUM` with its
        // menu's choice strings (dbAccess.c:1074 `mapDBFToDBR`,
        // dbAccess.c:167-175 `get_enum_strs`) — not as a bare SHORT/CHAR.
        assert_eq!(shared_menu_choices("SEVR"), Some(MENU_ALARM_SEVR));
        assert_eq!(shared_menu_choices("NSEV"), Some(MENU_ALARM_SEVR));
        assert_eq!(shared_menu_choices("ACKS"), Some(MENU_ALARM_SEVR));
        assert_eq!(shared_menu_choices("DISS"), Some(MENU_ALARM_SEVR));
        assert_eq!(shared_menu_choices("UDFS"), Some(MENU_ALARM_SEVR));
        assert_eq!(shared_menu_choices("STAT"), Some(MENU_ALARM_STAT));
        assert_eq!(shared_menu_choices("NSTA"), Some(MENU_ALARM_STAT));
        assert_eq!(shared_menu_choices("ACKT"), Some(MENU_YES_NO));
        assert_eq!(shared_menu_choices("PINI"), Some(MENU_PINI));
    }

    #[test]
    fn alarm_stat_and_pini_orders_match_the_dbd() {
        // menuAlarmStat index order is `alarm.h` epicsAlarmCondition, which
        // `recGblSetSevr` stores into STAT/NSTA — wire-visible.
        assert_eq!(MENU_ALARM_STAT.len(), 22);
        assert_eq!(MENU_ALARM_STAT[0], "NO_ALARM");
        assert_eq!(MENU_ALARM_STAT[14], "LINK");
        assert_eq!(MENU_ALARM_STAT[17], "UDF");
        assert_eq!(MENU_ALARM_STAT[21], "WRITE_ACCESS");
        // menuPini has six choices; RUN is 2 and RUNNING is 3.
        assert_eq!(
            MENU_PINI,
            &["NO", "YES", "RUN", "RUNNING", "PAUSE", "PAUSED"]
        );
    }

    #[test]
    fn non_menu_name_is_none() {
        assert_eq!(shared_menu_choices("VAL"), None);
        assert_eq!(shared_menu_choices("DESC"), None);
        // OSV is menuAlarmSevr in bi/bo but a string field in scalcout;
        // the registry maps the name, the value-type gate at the snapshot
        // boundary protects the string case.
        assert_eq!(shared_menu_choices("OSV"), Some(MENU_ALARM_SEVR));
    }
}
