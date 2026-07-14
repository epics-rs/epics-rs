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

use crate::error::{CaError, CaResult};
use crate::server::snapshot::EnumStringForm;
use crate::types::{DbFieldType, EpicsValue, PvString};

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
        // Scan mechanism and its simulation-mode twin, both `menu(menuScan)`
        // (`dbCommon.dbd.pod:66`, `SSCN` on the 21 records that carry it).
        // Their values live in `CommonFields`, not in a record's field list,
        // but the menu is the same shared table every other menu field uses —
        // for the string→index put converter AND for the DBR_ENUM choice
        // labels served with a GET/MONITOR of `.SCAN`.
        "SCAN" | "SSCN" => Some(MENU_SCAN),
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

/// C `strtoul(str, &end, 0)` — the parse `epicsParseULong` runs
/// (`epicsStdlib.c:57-79`) with `dbConvertBase`, which is 0 unless
/// `EPICS_DB_CONVERT_DECIMAL_ONLY=YES` (`epicsConvert.c:37`, `iocInit.c:140`).
/// Base 0 means: leading whitespace, optional sign, `0x`/`0X` hex, leading-`0`
/// octal, else decimal. A `-` negates modulo 2^64, exactly as C's `strtoul`
/// does — `"-65534"` is a *valid* parse producing 0xFFFF…0002.
///
/// Returns `(value, rest)`; `None` is C's `S_stdlib_noConversion` /
/// `S_stdlib_overflow`.
fn c_strtoul_base0(s: &str) -> Option<(u64, &str)> {
    fn c_isspace(b: u8) -> bool {
        matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
    }

    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && c_isspace(b[i]) {
        i += 1;
    }
    let mut negate = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        negate = b[i] == b'-';
        i += 1;
    }

    let mut base: u64 = 10;
    if i < b.len() && b[i] == b'0' {
        if i + 1 < b.len() && (b[i + 1] | 0x20) == b'x' {
            if i + 2 < b.len() && b[i + 2].is_ascii_hexdigit() {
                base = 16;
                i += 2;
            } else {
                // C strtoul consumes the leading "0" and stops at the 'x'.
                return Some((0, &s[i + 1..]));
            }
        } else {
            base = 8;
        }
    }

    let start = i;
    let mut acc: u64 = 0;
    while i < b.len() {
        let digit = match b[i] {
            c @ b'0'..=b'9' => u64::from(c - b'0'),
            c @ b'a'..=b'f' => u64::from(c - b'a') + 10,
            c @ b'A'..=b'F' => u64::from(c - b'A') + 10,
            _ => break,
        };
        if digit >= base {
            break;
        }
        // C reports ERANGE, which epicsParseULong turns into an error return.
        acc = acc.checked_mul(base)?.checked_add(digit)?;
        i += 1;
    }
    if i == start {
        return None; // S_stdlib_noConversion
    }
    let value = if negate { acc.wrapping_neg() } else { acc };
    Some((value, &s[i..]))
}

/// C `epicsParseUInt16(str, &val, dbConvertBase, NULL)` (`epicsStdlib.c:229-243`).
///
/// Whitespace is legal *around* the number (strtoul skips leading, the
/// `units == NULL` tail check skips trailing) but any other trailing character
/// is `S_stdlib_extraneous`. The range check is C's, verbatim: a value that
/// wrapped through the negative side (`> ~0xffff`) is NOT an overflow — it
/// truncates to its low 16 bits.
fn epics_parse_uint16(s: &str) -> Option<u16> {
    let (value, rest) = c_strtoul_base0(s)?;
    if !rest
        .bytes()
        .all(|b| matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
    {
        return None; // S_stdlib_extraneous
    }
    if value > 0xffff && value <= !0xffff_u64 {
        return None; // S_stdlib_overflow
    }
    Some(value as u16)
}

/// Resolve a `DBF_MENU` field's `DBR_STRING` write against THAT field's own
/// choice table — C `dbConvert.c::putStringMenu` (lines 1206-1229), the
/// converter every runtime `dbPut` of a string into a menu field goes through.
///
/// C's rule, exactly:
/// 1. `strcmp` against each choice — EXACT and case-sensitive. No trimming:
///    `"Passive "` is not `"Passive"`.
/// 2. Otherwise `epicsParseUInt16` (base 0 — hex/octal accepted, whitespace
///    around the digits accepted) and the index must be `< nChoice`.
/// 3. Otherwise `S_db_badChoice`.
///
/// The port used to `trim()` before the label match and to accept ANY parsable
/// index, so `caput fanout.SELM "99"` stored 99 — an enum index no branch of
/// the record handles — where C refuses the put.
///
/// Returns [`CaError::BadChoice`] for C's `S_db_badChoice`. The error is the
/// whole point of the signature: a menu field's string put has exactly one
/// resolver, and a miss must FAIL the put — the caller may not fall back to a
/// field-blind numeric coercion, which is how `"Bogus"` used to land as index
/// 0 (`EpicsValue::convert_to`: `to_f64().unwrap_or(0.0) as u16`).
///
/// A menu label is menu-*specific*, so resolution MUST use the field's own
/// `choices` (this argument), never a cross-menu global table: the same label
/// names different indices in different menus (e.g. "Specified" is index 1 of
/// `menuFanout` but index 0 of `selSELM`).
pub fn resolve_menu_field_string(
    field: &str,
    choices: &[&str],
    dbf_type: DbFieldType,
    s: &str,
) -> CaResult<EpicsValue> {
    resolve_menu_field_string_bounded(field, choices, dbf_type, s, MenuBound::DbPut)
}

/// C `S_db_badChoice` ("Illegal choice", `dbAccessDefs.h:183`) — the status
/// `putStringMenu`/`dbPutStringNum` hand back to `dbPut`, which aborts before
/// storing anything (`dbAccess.c:1362` `if (status) goto done`), so the field
/// keeps its previous value and the record is not processed.
fn bad_choice(field: &str, choices: &[&str], s: &str) -> CaError {
    CaError::BadChoice(format!(
        "{field}: '{s}' is not one of {choices:?} nor an index below {}",
        choices.len()
    ))
}

/// Resolve a `DBF_MENU` field's `.db` value — C `dbStaticRun.c::dbPutStringNum`
/// (lines 485-512), which is the *loader's* converter and does NOT share
/// `putStringMenu`'s bound.
///
/// Same exact-label / `epicsParseUInt16` front end, then C's own out-of-menu
/// test: `if (value > nChoice && nChoice > 0 && value < USHRT_MAX)` → reject.
/// So the loader accepts `value == nChoice` (one past the last choice) and
/// accepts `65535`, which is how a dbd `initial("65535")` sentinel (SSCN) can
/// load at all while `caput .SSCN "65535"` is refused at runtime. Keeping the
/// two bounds distinct is the only way both C behaviours hold.
pub fn resolve_menu_field_string_db_load(
    field: &str,
    choices: &[&str],
    dbf_type: DbFieldType,
    s: &str,
) -> CaResult<EpicsValue> {
    resolve_menu_field_string_bounded(field, choices, dbf_type, s, MenuBound::DbLoad)
}

/// C `get_enum_strs` for the two-state records (`bi`/`bo`/`busy`): ZNAM, ONAM.
///
/// C trims `no_str` to 1 when ZNAM is set and ONAM is empty
/// (`boRecord.c:342-352`), so a `bo` with only a ZNAM advertises — and accepts —
/// exactly one state.
pub fn binary_enum_states(znam: &PvString, onam: &PvString) -> Vec<PvString> {
    if !znam.is_empty() && onam.is_empty() {
        vec![znam.clone()]
    } else {
        vec![znam.clone(), onam.clone()]
    }
}

/// C `get_enum_strs` for the 16-state records (`mbbi`/`mbbo`): ZRST..FFST cut at
/// the last non-empty state — C's high-water `no_str` (`mbbiRecord.c:262-269`).
pub fn multibit_enum_states(states: [&PvString; 16]) -> Vec<PvString> {
    let no_str = states
        .iter()
        .rposition(|s| !s.is_empty())
        .map(|i| i + 1)
        .unwrap_or(0);
    states[..no_str].iter().map(|s| (*s).clone()).collect()
}

/// C `get_enum_str` for the two-state records (`bi`/`bo`/`busy`):
/// `VAL==0 -> ZNAM`, `VAL==1 -> ONAM`, anything else `"Illegal_Value"`
/// (`biRecord.c:173-192`, `boRecord.c:320-339`).
///
/// Both slots are indexed UNTRIMMED — unlike [`binary_enum_states`], an empty
/// `ONAM` behind a set `ZNAM` is still slot 1 and still renders empty. C's
/// `no_str` trimming belongs to the label list, not to this read.
pub fn binary_enum_string_form(znam: &PvString, onam: &PvString) -> EnumStringForm {
    EnumStringForm::states(
        vec![znam.clone(), onam.clone()],
        PvString::from("Illegal_Value"),
    )
}

/// C `get_enum_str` for the 16-state records (`mbbi`/`mbbo`): any `val <= 15`
/// reads its state slot, empty or not; past that, `"Illegal Value"` — with a
/// SPACE, where the two-state records use an underscore
/// (`mbbiRecord.c:235-255`, `mbboRecord.c:314-333`). The two spellings are C's,
/// and both are wire-visible, so neither is normalized here.
pub fn multibit_enum_string_form(states: [&PvString; 16]) -> EnumStringForm {
    EnumStringForm::states(
        states.iter().map(|s| (*s).clone()).collect(),
        PvString::from("Illegal Value"),
    )
}

/// C `dbConvert.c::putStringEnum` (lines 1149-1190) — a `DBR_STRING` write to a
/// `DBF_ENUM` field, i.e. `caput MY:VALVE Open`.
///
/// C routes it through the record's `put_enum_str` rset slot (an exact
/// `strncmp` against the state strings — `boRecord.c::put_enum_str` ZNAM/ONAM,
/// `mbboRecord.c:354-371` ZRST..FFST) and, when no state matched, falls back to
/// `epicsParseUInt16` + the `get_enum_strs` bound (`val < no_str`). Both slots
/// read the SAME state table, so the port takes it once, from
/// [`Record::enum_state_strings`](crate::server::record::Record::enum_state_strings) —
/// a name the record advertises to a client is exactly a name a client may put,
/// by construction.
///
/// A record whose `enum_state_strings` is `None` leaves both rset slots NULL in
/// C (`mbbiDirect`/`mbboDirect`), and `putStringEnum` fails the put with
/// `S_db_noRSET`. The caller passes `None` for that case; the put is REJECTED,
/// never coerced — C stores nothing on either failure path.
///
/// Deviation from C (Tier 2, `doc/strategy-2026-07-13.md`): C's `put_enum_str`
/// scans all 16 raw state slots, so on a record with states `["a","b"]` a put of
/// the EMPTY string matches the first undefined slot and stores `VAL=2` — a
/// state the record's own `get_enum_strs` does not advertise. The port matches
/// against the advertised (highwater-trimmed) table only, so an unmatched name
/// is `S_db_badChoice` in every case.
pub fn resolve_enum_state_string(
    field: &str,
    states: Option<&[PvString]>,
    s: &PvString,
) -> CaResult<EpicsValue> {
    let Some(states) = states else {
        return Err(CaError::TypeMismatch(format!(
            "{field}: DBF_ENUM field whose record supplies no enum-state table \
             (C put_enum_str == NULL: S_db_noRSET)"
        )));
    };
    // C `strncmp` is byte-exact and case-SENSITIVE.
    let index = states
        .iter()
        .position(|state| state.as_bytes() == s.as_bytes())
        .map(|i| i as u16)
        // C's `get_enum_strs` fallback: `epicsParseUInt16` + `val < no_str`.
        .or_else(|| {
            let value = epics_parse_uint16(&s.as_str_lossy())?;
            (value < states.len() as u16).then_some(value)
        })
        .ok_or_else(|| {
            CaError::BadChoice(format!(
                "{field}: '{}' is not one of {:?} nor an index below {}",
                s.as_str_lossy(),
                states
                    .iter()
                    .map(|st| st.as_str_lossy().into_owned())
                    .collect::<Vec<_>>(),
                states.len()
            ))
        })?;
    Ok(EpicsValue::Enum(index))
}

/// The one string→menu-index converter. Both public entry points are this
/// function with their C converter's bound; a caller that already knows which
/// converter it is (`RecordInstance::put_common_field` vs its `_db_load` twin)
/// passes the bound directly.
pub(crate) fn resolve_menu_field_string_bounded(
    field: &str,
    choices: &[&str],
    dbf_type: DbFieldType,
    s: &str,
    bound: MenuBound,
) -> CaResult<EpicsValue> {
    let index =
        menu_index_from_string(choices, s, bound).ok_or_else(|| bad_choice(field, choices, s))?;
    Ok(menu_index_value(dbf_type, index))
}

/// Which C converter's out-of-menu bound applies.
#[derive(Clone, Copy)]
pub(crate) enum MenuBound {
    /// `dbConvert.c::putStringMenu` — `val < nChoice`. Every runtime `dbPut`.
    DbPut,
    /// `dbStaticRun.c::dbPutStringNum` — `!(val > nChoice && nChoice > 0 && val < 65535)`.
    /// The `.db`/dbd loader only.
    DbLoad,
}

/// The shared front end of both C converters: exact label, else a base-0
/// `epicsParseUInt16` index, then the caller's bound.
fn menu_index_from_string(choices: &[&str], s: &str, bound: MenuBound) -> Option<i64> {
    if let Some(i) = choices.iter().position(|c| *c == s) {
        return Some(i as i64);
    }
    let value = epics_parse_uint16(s)?;
    let n_choice = choices.len() as u16;
    let in_range = match bound {
        MenuBound::DbPut => value < n_choice,
        MenuBound::DbLoad => !(value > n_choice && n_choice > 0 && value < u16::MAX),
    };
    in_range.then_some(i64::from(value))
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

    /// R8-3: the runtime converter's bound is `val < nChoice`
    /// (`dbConvert.c:1228`); the loader's is
    /// `!(val > nChoice && nChoice > 0 && val < USHRT_MAX)`
    /// (`dbStaticRun.c::dbPutStringNum`). They differ at exactly two points —
    /// `val == nChoice` and `val == 65535` — and both must hold: the 65535
    /// case is how a dbd `initial("65535")` menu sentinel loads at all.
    #[test]
    fn db_put_and_db_load_bounds_differ_where_c_says_they_do() {
        const N: usize = MENU_PRIORITY.len(); // 3
        assert_eq!(
            menu_index_from_string(MENU_PRIORITY, "2", MenuBound::DbPut),
            Some(2)
        );
        assert_eq!(
            menu_index_from_string(MENU_PRIORITY, &N.to_string(), MenuBound::DbPut),
            None
        );
        assert_eq!(
            menu_index_from_string(MENU_PRIORITY, &N.to_string(), MenuBound::DbLoad),
            Some(N as i64)
        );
        assert_eq!(
            menu_index_from_string(MENU_PRIORITY, "65535", MenuBound::DbPut),
            None
        );
        assert_eq!(
            menu_index_from_string(MENU_PRIORITY, "65535", MenuBound::DbLoad),
            Some(65535)
        );
        // Both refuse a value that is neither a label nor a number.
        assert_eq!(
            menu_index_from_string(MENU_PRIORITY, "HIGH ", MenuBound::DbLoad),
            None
        );
    }

    /// `epicsParseUInt16` = `strtoul(str, &end, 0)` + a tail check + C's own
    /// range test (`epicsStdlib.c:229-243`).
    #[test]
    fn epics_parse_uint16_is_c_strtoul_base_0() {
        assert_eq!(epics_parse_uint16("10"), Some(10));
        assert_eq!(epics_parse_uint16(" 0x1f\t"), Some(31));
        assert_eq!(epics_parse_uint16("010"), Some(8));
        assert_eq!(epics_parse_uint16("65535"), Some(65535));
        assert_eq!(epics_parse_uint16("65536"), None); // S_stdlib_overflow
        assert_eq!(epics_parse_uint16("12abc"), None); // S_stdlib_extraneous
        assert_eq!(epics_parse_uint16(""), None); // S_stdlib_noConversion
        assert_eq!(epics_parse_uint16("abc"), None);
        // C's range test spares the wrapped negatives: strtoul("-1") is
        // 0xFFFF_FFFF_FFFF_FFFF, which is `> ~0xffff` is false, so it
        // truncates to 0xffff instead of erroring.
        assert_eq!(epics_parse_uint16("-1"), Some(0xffff));
    }
}
