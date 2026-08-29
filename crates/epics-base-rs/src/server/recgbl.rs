use crate::server::record::{AlarmSeverity, CommonFields};
use crate::types::{DbFieldType, DbfCode};

pub mod simm;

/// Alarm status codes matching EPICS base's `menuAlarmStat.dbd` /
/// `epicsAlarmCondition` (libcom/src/misc/alarm.h) wire format. The
/// numeric values are baked into the CA wire protocol's `stat` byte,
/// so they MUST match C — a Rust IOC sending `LINK_ALARM = 13` would
/// be decoded by a C client as `SCAN` (which is 13 in C).
pub mod alarm_status {
    pub const NO_ALARM: u16 = 0;
    pub const READ_ALARM: u16 = 1;
    pub const WRITE_ALARM: u16 = 2;
    pub const HIHI_ALARM: u16 = 3;
    pub const HIGH_ALARM: u16 = 4;
    pub const LOLO_ALARM: u16 = 5;
    pub const LOW_ALARM: u16 = 6;
    pub const STATE_ALARM: u16 = 7;
    pub const COS_ALARM: u16 = 8;
    pub const COMM_ALARM: u16 = 9;
    pub const TIMEOUT_ALARM: u16 = 10;
    pub const HW_LIMIT_ALARM: u16 = 11;
    pub const CALC_ALARM: u16 = 12;
    pub const SCAN_ALARM: u16 = 13;
    pub const LINK_ALARM: u16 = 14;
    pub const SOFT_ALARM: u16 = 15;
    pub const BAD_SUB_ALARM: u16 = 16;
    pub const UDF_ALARM: u16 = 17;
    pub const DISABLE_ALARM: u16 = 18;
    pub const SIMM_ALARM: u16 = 19;
    pub const READ_ACCESS_ALARM: u16 = 20;
    pub const WRITE_ACCESS_ALARM: u16 = 21;
}

/// String name for an alarm status code, mirroring EPICS
/// `epicsAlarmConditionStrings` (libcom/src/misc/alarmString.c:27-50).
/// pvxs `iocsource.cpp:226,236` reports `alarm.message =
/// epicsAlarmConditionStrings[status]` when the status is non-zero.
/// The index is the [`alarm_status`] code; an out-of-range status
/// returns `""` (the C array is sized `ALARM_NSTATUS`, so indexing past
/// it would be undefined — we return empty instead).
pub fn alarm_condition_string(status: u16) -> &'static str {
    const STRINGS: [&str; 22] = [
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
    STRINGS.get(status as usize).copied().unwrap_or("")
}

/// Event mask bits for monitor posting (matches EPICS DBE_*).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventMask(u16);

impl EventMask {
    pub const NONE: Self = Self(0);
    pub const VALUE: Self = Self(0x01);
    pub const LOG: Self = Self(0x02);
    pub const ALARM: Self = Self(0x04);
    pub const PROPERTY: Self = Self(0x08);

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn intersects(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }

    pub const fn bits(self) -> u16 {
        self.0
    }

    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }
}

impl std::ops::BitOr for EventMask {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for EventMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl std::ops::BitAnd for EventMask {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

/// The record fields C `recGblResetAlarms` posts ITSELF (recGbl.c:202-222):
/// `db_post_events(&sevr, …)`, `db_post_events(&stat, …)`,
/// `db_post_events(&amsg, …)` and `db_post_events(&acks, DBE_VALUE)`.
///
/// Each carries its own C mask, assembled by the alarm-post owner
/// (`processing::alarm_field_posts` and its siblings) from
/// [`AlarmResetResult`]. The generic per-cycle change-detection loop
/// (`RecordInstance::collect_subscriber_posts`) must therefore SKIP every field
/// named here: emitting one from the record-wide snapshot as well would send a
/// subscriber two events for one C `db_post_events`, the second carrying the
/// framework's default `alarm_bits | DBE_VALUE | DBE_LOG` — a mask C never uses
/// for these fields.
///
/// The two sides are kept in step by construction: a field the alarm-post owner
/// emits belongs in this list, and the change-detection loop excludes exactly
/// this list. (`UDF` is NOT here — it is not a `recGblResetAlarms` post; the
/// framework posts it from its own undefined-value rule.)
pub const RECGBL_POSTED_ALARM_FIELDS: [&str; 4] = ["SEVR", "STAT", "AMSG", "ACKS"];

/// Result of rec_gbl_reset_alarms: whether alarm state changed.
pub struct AlarmResetResult {
    pub alarm_changed: bool,
    pub prev_sevr: AlarmSeverity,
    pub prev_stat: u16,
    /// True iff `amsg` value changed in this reset cycle (epics-base PR #568).
    pub amsg_changed: bool,
    /// True iff `recGblResetAlarms` POSTED `ACKS` this cycle — i.e. the
    /// alarm-acknowledge rule fired (recGbl.c:214-217):
    ///
    /// ```c
    /// if (stat_mask) {
    ///     ...
    ///     if (!pdbc->ackt || new_sevr >= pdbc->acks) {
    ///         pdbc->acks = new_sevr;
    ///         db_post_events(pdbc, &pdbc->acks, DBE_VALUE);
    ///     }
    /// }
    /// ```
    ///
    /// The post is UNCONDITIONAL inside the rule — there is no value-change
    /// test — so this flag says "the rule fired", not "the value moved". Those
    /// differ on any stat-only alarm transition at constant severity
    /// (LINK→CALC both INVALID; HIGH→HIHI with `HSV = HHSV = MAJOR`), where C
    /// re-posts an already-equal ACKS and a `DBE_VALUE`-only `.ACKS`
    /// subscriber receives the event.
    ///
    /// The `if (stat_mask)` guard is already folded in — a consumer posts
    /// `ACKS` on this flag alone and must NOT re-derive the guard.
    pub acks_posted: bool,
}

/// Set new alarm severity if it's higher than current nsta/nsev.
///
/// Returns C's `int` result (`recGbl.c:253`/`:255`): TRUE only when
/// `prec->nsev < new_sevr` actually raised the pending state. That return
/// is not decoration — it is the gate on every hysteresis latch in the
/// tree (`if (recGblSetSevr(...)) prec->lalm = alev;`, `aiRecord.c:405-406`
/// and its siblings). A level that fires but LOSES the severity compare
/// must not move `LALM`, or the next cycle's
/// `lalm == alev && val >= alev - hyst` clause re-fires an alarm C never
/// armed. Callers wanting only the side effect ignore it, as C's do.
///
/// Matches EPICS `recGblSetSevr` (recGbl.c:258-261): `recGblSetSevr`
/// delegates to `recGblSetSevrMsg(..., NULL)` → `recGblSetSevrVMsg`
/// with `msg == NULL`. When the new severity raises the pending
/// state, the `msg == NULL` branch (recGbl.c:249-251) executes
/// `prec->namsg[0] = '\0'` — i.e. it **clears** any pending alarm
/// message. A stale `namsg` written by an earlier MSS link (or
/// `evaluate_alarms`) must not survive when a later, higher-severity
/// no-message alarm raises the record severity; otherwise the
/// record's final `amsg` carries an unrelated upstream record's
/// alarm text.
pub fn rec_gbl_set_sevr(common: &mut CommonFields, stat: u16, sevr: AlarmSeverity) -> bool {
    if (sevr as u16) > (common.nsev as u16) {
        common.nsta = stat;
        common.nsev = sevr;
        // C `msg == NULL` branch clears the pending message.
        common.namsg.clear();
        return true;
    }
    false
}

/// `rec_gbl_set_sevr` for a severity that is a **raw menu ordinal**, comparing
/// it the way C's `recGblSetSevrVMsg` does — against the RAW `epicsEnum16`
/// (recGbl.c:242 `if (prec->nsev < new_sevr)`), BEFORE any clamp to
/// `INVALID_ALARM`.
///
/// A numeric `caput .ZSV 4` (or `-1` → `65535`) stores that raw ordinal in the
/// `DBF_MENU` field (`dbConvert.c::putDoubleEnum` = `*pfield = (epicsEnum16)val`,
/// the same raw-ordinal rule the analog `HHSV`/… selectors already follow, see
/// [`crate::server::record::AnalogAlarmConfig`]). C hands that raw ordinal
/// straight to `recGblSetSevr`, so a state alarm whose raw severity is
/// numerically greater than a prior UDF's in-range `INVALID` (3) — e.g. `ZSV=4`
/// or `65535` — DOES override it: `3 < 4` is true, STAT becomes STATE. Clamping
/// the ordinal to `Invalid` (3) BEFORE the compare (the [`rec_gbl_set_sevr`]
/// path via [`AlarmSeverity::from_u16`]) would tie the prior UDF and wrongly
/// leave STAT=UDF.
///
/// The DISPLAYED severity stays clamped: C clamps `nsev` to `INVALID_ALARM` in
/// `recGblResetAlarms` (recGbl.c:188-189), and this stores the clamped
/// [`AlarmSeverity`] so both sides show `INVALID`. `raw_sevr == 0` is a no-op
/// (NO_ALARM never raises), matching C's `recGblSetSevr(.., 0)`.
///
/// Returns the raise, same as [`rec_gbl_set_sevr`].
pub fn rec_gbl_set_sevr_raw(common: &mut CommonFields, stat: u16, raw_sevr: u16) -> bool {
    if raw_sevr > (common.nsev as u16) {
        common.nsta = stat;
        common.nsev = AlarmSeverity::from_u16(raw_sevr);
        // C `msg == NULL` branch clears the pending message.
        common.namsg.clear();
        return true;
    }
    false
}

/// Set new alarm severity AND attach an alarm message (epics-base PR
/// #568 `recGblSetSevrMsg`). Same "raise only" rule as `rec_gbl_set_sevr`;
/// when the new severity raises the pending state, both `nsta`/`nsev`
/// AND `namsg` are written together. Empty `msg` clears the pending
/// message — non-empty replaces it. Returns the raise, same as
/// [`rec_gbl_set_sevr`].
pub fn rec_gbl_set_sevr_msg(
    common: &mut CommonFields,
    stat: u16,
    sevr: AlarmSeverity,
    msg: impl Into<String>,
) -> bool {
    if (sevr as u16) > (common.nsev as u16) {
        common.nsta = stat;
        common.nsev = sevr;
        common.namsg = msg.into();
        return true;
    }
    false
}

/// C `setLinkAlarm` (dbLink.c:318-323) — **the single owner of "a link
/// operation failed"**, and the failure path of every `dbGetLink`,
/// `dbPutLink` and `dbPutLinkAsync`:
///
/// ```c
/// static void setLinkAlarm(struct link* plink)
/// {
///     recGblSetSevrMsg(plink->precord, LINK_ALARM, INVALID_ALARM,
///                      "field %s", dbLinkFieldName(plink));
/// }
/// ```
///
/// `field` is the LINK FIELD's name — `INP`, `SIOL`, `SIML`, `OUT` — and it
/// reaches the operator as the record's `AMSG`. Every failing link read must
/// go through here, or the record publishes a lower severity than C (and a
/// broken link goes silent when the record's other alarms are quiet).
///
/// The severity is a MAXIMIZE (`rec_gbl_set_sevr_msg` is strict-greater), so
/// WHERE this is called relative to the record's other `recGblSetSevr` calls
/// decides ties. Callers must reproduce C's ORDER, not just C's set of alarms:
/// a base record raises `SIMM_ALARM` BEFORE its SIOL read (longinRecord.c:414
/// then :416), so with `SIMS = INVALID` the equal-severity LINK_ALARM loses and
/// STAT stays SIMM_ALARM; swait raises it AFTER (swaitRecord.c:416 then :421),
/// so LINK_ALARM wins there.
///
/// NOT used for `dbTryGetLink`, which does not call `setLinkAlarm`:
/// `recGblGetSimm`'s SIML read (recGbl.c:453-454) instead writes
/// `nsta = LINK_ALARM` directly, leaving SEVR untouched.
pub fn rec_gbl_set_link_alarm(common: &mut CommonFields, field: &str) {
    rec_gbl_set_sevr_msg(
        common,
        alarm_status::LINK_ALARM,
        AlarmSeverity::Invalid,
        format!("field {field}"),
    );
}

/// Transfer nsta/nsev to stat/sevr, detect alarm change, reset nsta/nsev.
/// Matches EPICS recGblResetAlarms. Call at end of process cycle.
///
/// Mirrors epics-base PR #566 — the alarm-message string (`amsg`) is
/// transferred from `namsg` alongside the severity / status.
pub fn rec_gbl_reset_alarms(common: &mut CommonFields) -> AlarmResetResult {
    let prev_sevr = common.sevr;
    let prev_stat = common.stat;

    // C parity (recGbl.c:188-189): clamp pending severity at INVALID_ALARM.
    // Records that erroneously call `recGblSetSevr` with a severity > 3
    // (e.g. via field-typed values that round-trip through u16) would
    // otherwise corrupt `sevr` into an undefined alarm severity. Keep the
    // existing nsev variant if it's already valid — only re-encode when
    // an out-of-range value snuck in.
    if (common.nsev as u16) > (AlarmSeverity::Invalid as u16) {
        common.nsev = AlarmSeverity::Invalid;
    }

    // C parity (recGbl.c:191-195): the amsg copy AND the namsg clear are BOTH
    // inside `if (strcmp(namsg, amsg) != 0)`:
    //
    // ```c
    // if (strcmp(pdbc->namsg, pdbc->amsg) != 0) {
    //     strcpy(pdbc->amsg, pdbc->namsg);
    //     pdbc->namsg[0] = '\0';
    //     stat_mask = DBE_ALARM;
    // }
    // ```
    //
    // So an UNCHANGED message survives in `namsg` — C does not clear it — and
    // the next cycle that raises no alarm at all finds `namsg == amsg` again
    // and leaves `amsg` alone. AMSG therefore keeps the last alarm text after
    // the alarm clears. An unconditional take (the pre-fix shape) empties
    // `namsg` on the repeat cycle, so the clearing cycle transfers `""` and
    // publishes an empty AMSG — divergent for any message alarm that persists
    // for two or more cycles, which is every broken-then-recovered link
    // (`dbLink.c:320` raises `recGblSetSevrMsg(LINK_ALARM, INVALID, "field %s")`
    // on every failing read).
    let amsg_changed = common.namsg != common.amsg;
    if amsg_changed {
        common.amsg = std::mem::take(&mut common.namsg);
    }

    // Transfer new alarm state
    common.sevr = common.nsev;
    common.stat = common.nsta;

    // Reset for next cycle
    common.nsev = AlarmSeverity::NoAlarm;
    common.nsta = alarm_status::NO_ALARM;

    let alarm_changed = common.sevr != prev_sevr || common.stat != prev_stat;

    // C parity (recGbl.c:209-222): when an alarm-class field moved this cycle
    // (C's `if (stat_mask)`, which is exactly `alarm_changed || amsg_changed`),
    // update the alarm-acknowledge severity `acks`. If `ackt` is false (alarm
    // is transient — automatically resets when the condition clears) OR the new
    // severity is >= the currently remembered acks, raise `acks` to the new
    // severity. Operators clear `acks` back to NoAlarm via a CA put to ACKS
    // (handled in record_instance::put_common_field).
    //
    // ```c
    // if (!pdbc->ackt || new_sevr >= pdbc->acks) {
    //     pdbc->acks = new_sevr;
    //     db_post_events(pdbc, &pdbc->acks, DBE_VALUE);
    // }
    // ```
    //
    // The assignment and the post sit together inside the rule, with NO
    // value-change test around them: whenever the rule fires, C emits the
    // event. Gating the post on `acks != sevr` (the pre-fix shape) dropped it
    // on every stat-only transition at constant severity — LINK→CALC with both
    // INVALID, or HIGH→HIHI with `HSV = HHSV = MAJOR` — where the rule fires,
    // ACKS is already equal, and C still posts. `acks_posted` therefore reports
    // the RULE, not the value.
    let mut acks_posted = false;
    if (alarm_changed || amsg_changed)
        && (!common.ackt || (common.sevr as u16) >= (common.acks as u16))
    {
        common.acks = common.sevr;
        acks_posted = true;
    }

    AlarmResetResult {
        alarm_changed,
        prev_sevr,
        prev_stat,
        amsg_changed,
        acks_posted,
    }
}

/// The two ways C `checkAlarms` tests the `UDF` byte before raising
/// `UDF_ALARM`. Almost every record uses `if (prec->udf)` — TRUTHY, so any
/// non-zero byte counts as undefined (`aiRecord.c:319`, `mbboRecord.c:210`,
/// `longoutRecord.c:317`, …). A small set uses `if (prec->udf == TRUE)` —
/// EXACT-ONE, where `TRUE` is `1`, so a byte that is neither 0 nor 1 does NOT
/// raise the alarm (`boRecord.c:371`, `stringoutRecord.c:146`,
/// `biRecord.c:225`, `busyRecord.c:337`). The distinction is observable only
/// for a record whose `udf` byte can hold a value other than 0/1 at
/// `checkAlarms` time — i.e. one that does NOT re-derive `udf` every cycle
/// (`clears_udf() == false`) — after a direct `caput .UDF 255` (or `-1`, which
/// reaches the `DBF_UCHAR` field as `255`): C's exact-one records leave
/// STAT/SEVR at `NO_ALARM`, the truthy records raise `UDF_ALARM/INVALID`.
pub fn udf_alarm_active(udf: u8, exact_one: bool) -> bool {
    if exact_one { udf == 1 } else { udf != 0 }
}

/// Check UDF alarm: if record is still undefined, raise UDF_ALARM.
/// `exact_one` selects C's `udf == TRUE` semantics (see
/// [`udf_alarm_active`]); pass the record's [`crate::server::record::Record::udf_alarm_on_exact_one`].
///
/// `severity` is the record's fixed severity
/// ([`crate::server::record::Record::udf_alarm_severity`]); `None` — the case
/// for 19 of the 21 raise sites in `std/rec` — means the record's live `UDFS`
/// field, which stays readable here rather than being pre-baked by the caller.
/// This is the only place `UDFS` is consulted for a UDF raise.
///
/// `msg` is the record's alarm message ([`crate::server::record::Record::udf_alarm_message`]).
/// Almost every base record raises UDF via plain
/// `recGblSetSevr(prec, UDF_ALARM, prec->udfs)`, which forwards a NULL
/// message and leaves `namsg` EMPTY (`recGbl.c:249-251,258-261`) — so `msg` is
/// `""` for them, and PVA then serves the `"UDF"` condition string
/// (`iocsource.cpp:230-236`). Only `mbboDirectRecord.c:191` passes a
/// literal (`"UDFS"`).
pub fn rec_gbl_check_udf(
    common: &mut CommonFields,
    exact_one: bool,
    severity: Option<AlarmSeverity>,
    msg: &str,
) {
    if udf_alarm_active(common.udf, exact_one) {
        rec_gbl_set_sevr_msg(
            common,
            alarm_status::UDF_ALARM,
            severity.unwrap_or_else(|| AlarmSeverity::from_u16(common.udfs as u16)),
            msg,
        );
    }
}

/// `recGblGetTimeStamp` (recGbl.c): resolve a record's time stamp from
/// its `TSE` field.
///
/// `device_time` is the record's current `CommonFields.time` — the value
/// device support has already written this cycle. It is returned verbatim
/// only on the `TSE == epicsTimeEventDeviceTime (-2)` branch.
///
/// This is the single owner of "resolve TIME from TSE". Both the
/// framework's per-cycle timestamp application
/// (`database::apply_timestamp`) and device support that has to format
/// the record's resolved time *during* `read()` (the std module's
/// `devTimeOfDay.c` `recGblGetTimeStamp(psi)` call, before the
/// record-level application runs) route through here, so the two never
/// drift.
pub fn get_time_stamp(
    tse: i16,
    device_time: std::time::SystemTime,
) -> Option<std::time::SystemTime> {
    if tse == crate::server::record::EPICS_TIME_EVENT_DEVICE_TIME {
        // `epicsTimeEventDeviceTime` (epicsTime.h:104). Device support has
        // already written the time; C recGbl.c:324-342 makes this the only
        // TSE that does not go through `epicsTimeGetEvent`.
        return Some(device_time);
    }
    // Every other TSE is one C call — `epicsTimeGetEvent(&prec->time,
    // prec->tse)` — including 0 and -1, which it dispatches itself. `None`
    // is C's non-zero status: `prec->time` keeps its old value and
    // `recGblGetTimeStampSimm` errlogs (recGbl.c:325-327).
    crate::runtime::general_time::get_event(tse as i32)
}

/// C's `getMaxRangeValues` (`recGbl.c:372-419`) — the DBF-type numeric range a
/// record hands back for a field whose limits it does not supply itself.
///
/// Returns `(upper, lower)`, C's argument order (`pupper_limit`,
/// `plower_limit`).
///
/// This is the ONLY table of these constants in the workspace. Both
/// [`rec_gbl_get_graphic_double`] and [`rec_gbl_get_control_double`] route
/// here, exactly as C's `recGblGetGraphicDouble` (`recGbl.c:146-153`) and
/// `recGblGetControlDouble` (`recGbl.c:164-171`) both call it — the display
/// and control defaults are the same numbers by construction, not by two
/// copies that happen to agree.
///
/// # The discriminator is the declared DBF, and only that
///
/// C passes `pdbFldDes->field_type` (`recGbl.c:151`, `:169`): the `.dbd`
/// declaration, one value, no second opinion. [`DbfCode`] is that value, so
/// the two facts a caller used to have to supply separately fall out of it —
/// `DBF_MENU`/`DBF_DEVICE` are codes of their own with no case in the switch,
/// and a runtime-retyped field declares `DBF_NOACCESS`, which `cvt_dbaddr`
/// raises at address-build time but never here.
///
/// # `None` is a real answer, not "unknown"
///
/// C's switch has **no `default:` branch**. For `DBF_STRING`, `DBF_MENU`,
/// `DBF_DEVICE`, `DBF_NOACCESS` and the link types it writes NOTHING, so the
/// caller's pre-init survives onto the wire: `dbAccess.c:216` zeroes the
/// graphic struct and `dbAccess.c:256` the control struct *before* calling the
/// rset slot. `None` therefore means **leave the limit at 0.0** — a caller that
/// substitutes a range of its own is wrong, and a caller that treats `None` as
/// "drop the field" is also wrong (the option bit stays on; only a NULL rset
/// slot clears it, `dbAccess.c:217-220`).
fn get_max_range_values(declared_dbf: DbfCode) -> Option<(f64, f64)> {
    Some(match declared_dbf {
        // recGbl.c:376-379. C's `CHAR_MAX`/`CHAR_MIN` — plain `char`, which is
        // SIGNED on every target EPICS builds for, so 127/-128, NOT 255/0.
        DbfCode::Char => (f64::from(i8::MAX), f64::from(i8::MIN)),
        // recGbl.c:380-383
        DbfCode::UChar => (f64::from(u8::MAX), 0.0),
        // recGbl.c:384-387
        DbfCode::Short => (f64::from(i16::MAX), f64::from(i16::MIN)),
        // recGbl.c:388-392 — `DBF_ENUM` and `DBF_USHORT` share one case.
        DbfCode::Enum | DbfCode::UShort => (f64::from(u16::MAX), 0.0),
        // recGbl.c:393-396 — C writes the literals, not `INT_MAX`/`INT_MIN`.
        DbfCode::Long => (2147483647.0, -2147483648.0),
        // recGbl.c:397-400
        DbfCode::ULong => (4294967295.0, 0.0),
        // recGbl.c:401-404. NOTE the upper is 2^63, i.e. `INT64_MAX + 1` —
        // NOT `INT64_MAX`. C writes `9223372036854775808.0` verbatim. This is
        // not a typo to "fix": it is why QSRV2 puts INT64_MIN on BOTH ends of
        // an int64 field's limits (2^63 is out of int64 range, so the
        // double->int64 conversion lands on INT64_MIN, the same value the
        // exactly-representable lower limit converts to).
        DbfCode::Int64 => (9223372036854775808.0, -9223372036854775808.0),
        // recGbl.c:405-408
        DbfCode::UInt64 => (18446744073709551615.0, 0.0),
        // recGbl.c:409-412 — 1e30, not `FLT_MAX`.
        DbfCode::Float => (1e30, -1e30),
        // recGbl.c:413-416 — 1e300, not `DBL_MAX`.
        DbfCode::Double => (1e300, -1e300),
        // No case in C's switch: nothing written, caller's 0.0 pre-init wins.
        // `Menu`/`Device` are here because C's `DBF_MENU`/`DBF_DEVICE` are
        // codes the switch does not list, and `NoAccess` because that is what
        // a runtime-retyped field declares.
        DbfCode::String
        | DbfCode::Menu
        | DbfCode::Device
        | DbfCode::Inlink
        | DbfCode::Outlink
        | DbfCode::Fwdlink
        | DbfCode::NoAccess => return None,
    })
}

/// C's `recGblGetGraphicDouble` (`recGbl.c:146-153`) — the display limits for a
/// field the record's own `get_graphic_double` does not list.
///
/// See `get_max_range_values` for what the argument is and what `None` means.
pub fn rec_gbl_get_graphic_double(declared_dbf: DbfCode) -> Option<(f64, f64)> {
    get_max_range_values(declared_dbf)
}

/// C's `recGblGetControlDouble` (`recGbl.c:164-171`) — the control limits for a
/// field the record's own `get_control_double` does not list.
///
/// Identical to [`rec_gbl_get_graphic_double`] by construction, because C's two
/// helpers are identical bodies over the same `get_max_range_values` table.
///
/// See `get_max_range_values` for what the argument is and what `None` means.
pub fn rec_gbl_get_control_double(declared_dbf: DbfCode) -> Option<(f64, f64)> {
    get_max_range_values(declared_dbf)
}

/// C's `recGblGetAlarmDouble` (`recGbl.c:155-162`) — the valueAlarm limits for a
/// field the record's own `get_alarm_double` does not list.
///
/// All four are `epicsNAN`, in C's struct order: `(upper_alarm_limit,
/// upper_warning_limit, lower_warning_limit, lower_alarm_limit)`. Unlike the
/// graphic/control helpers there is no type table and no "writes nothing" case
/// — C fills all four unconditionally, and `dbAccess.c:294` pre-inits the same
/// four NaNs anyway, so the two agree.
///
/// Note this is NOT the same as a record with a NULL `get_alarm_double` slot:
/// there `dbAccess.c:295-298` leaves `no_data = TRUE` and the `DBR_AL_DOUBLE`
/// option bit is cleared (`:325-326`), so the client is sent no valueAlarm at
/// all rather than four NaNs. That distinction is owned by `PropertySupport`.
pub fn rec_gbl_get_alarm_double() -> (f64, f64, f64, f64) {
    (f64::NAN, f64::NAN, f64::NAN, f64::NAN)
}

/// C's `recGblGetPrec` (`recGbl.c:119-144`) — the precision for a field the
/// record's own `get_precision` does not list.
///
/// Unlike the limit helpers this one MUTATES the caller's seed rather than
/// producing a value: C takes `long *precision` already holding whatever the
/// record's `get_precision` stored (typically `prec->prec`, e.g.
/// `aiRecord.c:236`) and rewrites it only for the types its switch names.
/// `seed` is that value; the return is C's post-call buffer.
///
/// ```c
/// switch (pdbFldDes->field_type) {
/// case DBF_CHAR: ... case DBF_UINT64:  *precision = 0;  break;
/// case DBF_FLOAT: case DBF_DOUBLE:
///     if (*precision < 0 || *precision > 15) *precision = 15;
///     break;
/// default: break;
/// }
/// ```
///
/// `field_type` is the **static** dbd type (`pdbFldDes->field_type`), so a
/// runtime-retyped field passes `None` and takes the `default:` arm — the seed
/// survives untouched.
///
/// The integer arm is unobservable through `dbGet`: `dbAccess.c:387-394` gates
/// `DBR_PRECISION` on the field being `DBF_FLOAT`/`DBF_DOUBLE` *before* the
/// slot runs, so a `DBF_LONG` field's precision is never sent at all (the port
/// applies that gate in `PropertySupport::narrowed_to_field`). It is
/// transcribed anyway because this is `recGblGetPrec`, not `dbGet`'s view of
/// it, and a future caller must see C's rule, not the subset one caller can
/// observe.
pub fn rec_gbl_get_prec(field_type: Option<DbFieldType>, seed: i16) -> i16 {
    match field_type {
        Some(
            DbFieldType::Char
            | DbFieldType::UChar
            | DbFieldType::Short
            | DbFieldType::UShort
            | DbFieldType::Long
            | DbFieldType::ULong
            | DbFieldType::Int64
            | DbFieldType::UInt64,
        ) => 0,
        Some(DbFieldType::Float | DbFieldType::Double) => {
            if !(0..=15).contains(&seed) {
                15
            } else {
                seed
            }
        }
        // No case in C's switch: STRING, ENUM/MENU, DEVICE, NOACCESS, links.
        _ => seed,
    }
}

/// C `recGblRecordError` (`recGbl.c:59-71`) — the line a record writes when it
/// refuses one of its own fields.
///
/// This is why a refused `dbpf` still speaks even though `dbpf` itself prints
/// nothing but the read-back: the *record* is the one talking, from its
/// `special()`, and it talks through the errlog rather than through the shell's
/// stdout. A port that logs the same event through `tracing` says nothing at
/// all — no IOC binary installs a subscriber.
///
/// C looks the numeric status up with `errSymLookup` and leaves the slot empty
/// for `status <= 0`, which is where the doubled space in
/// `"recGblRecordError: scanAdd detected illegal SCAN value  PV: T:A"` comes
/// from. The port carries typed errors instead of `long` status codes, so the
/// caller passes the text that lookup would have produced — `""` for C's
/// non-positive statuses. C's `precord ? ... : "Unknown"` guard is for a null
/// record, which this signature cannot express.
pub fn rec_gbl_record_error(status_text: &str, record_name: &str, message: &str) {
    crate::runtime::log::errlog_printf(&format!(
        "recGblRecordError: {message} {status_text} PV: {record_name}\n"
    ));
}

/// C `recGblDbaddrError` (`recGbl.c:73-91` @R7.0.10) — the same report keyed
/// on a field address rather than a record, so the PV it names is
/// `record.FIELD`.
///
/// `status_text` is what `errSymLookup` would have produced, passed in for the
/// same reason [`rec_gbl_record_error`] takes it. `message` is the caller's
/// `sprintf`ed prefix and carries no newline of its own — C's format string
/// owns the only one (`recGbl.c:87-90`), so the report is one console line.
pub fn rec_gbl_dbaddr_error(status_text: &str, record_name: &str, field_name: &str, message: &str) {
    crate::runtime::log::errlog_printf(&format!(
        "recGblDbaddrError: {message} {status_text} PV: {record_name}.{field_name}\n"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every branch of C's `getMaxRangeValues` switch (`recGbl.c:372-419`),
    /// one case per DBF type, transcribed from the C literals rather than
    /// from Rust's `T::MAX` where C does not use the macro.
    #[test]
    fn get_max_range_values_matches_the_c_switch() {
        let r = get_max_range_values;
        // recGbl.c:376-379 — CHAR_MAX/CHAR_MIN, signed char.
        assert_eq!(r(DbfCode::Char), Some((127.0, -128.0)));
        // recGbl.c:380-383
        assert_eq!(r(DbfCode::UChar), Some((255.0, 0.0)));
        // recGbl.c:384-387
        assert_eq!(r(DbfCode::Short), Some((32767.0, -32768.0)));
        // recGbl.c:388-392 — one case, both types.
        assert_eq!(r(DbfCode::UShort), Some((65535.0, 0.0)));
        assert_eq!(r(DbfCode::Enum), Some((65535.0, 0.0)));
        // recGbl.c:393-396
        assert_eq!(r(DbfCode::Long), Some((2147483647.0, -2147483648.0)));
        // recGbl.c:397-400
        assert_eq!(r(DbfCode::ULong), Some((4294967295.0, 0.0)));
        // recGbl.c:409-412 / :413-416 — 1e30 and 1e300, not FLT_MAX/DBL_MAX.
        assert_eq!(r(DbfCode::Float), Some((1e30, -1e30)));
        assert_eq!(r(DbfCode::Double), Some((1e300, -1e300)));
    }

    /// Every branch of C's `recGblGetPrec` switch (`recGbl.c:119-144`).
    /// C mutates the caller's seed, so each case is pinned as
    /// `(seed) -> post-call buffer`.
    #[test]
    fn rec_gbl_get_prec_matches_the_c_switch() {
        // recGbl.c:124-133 — every integer type forces 0, whatever the seed.
        for t in [
            DbFieldType::Char,
            DbFieldType::UChar,
            DbFieldType::Short,
            DbFieldType::UShort,
            DbFieldType::Long,
            DbFieldType::ULong,
            DbFieldType::Int64,
            DbFieldType::UInt64,
        ] {
            assert_eq!(rec_gbl_get_prec(Some(t), 7), 0, "{t:?}");
            assert_eq!(rec_gbl_get_prec(Some(t), 0), 0, "{t:?}");
        }
        // recGbl.c:135-139 — FLOAT/DOUBLE clamp OUT-OF-RANGE to 15 and keep
        // an in-range seed. The boundaries are inclusive on both ends.
        for t in [DbFieldType::Float, DbFieldType::Double] {
            assert_eq!(rec_gbl_get_prec(Some(t), 7), 7, "{t:?}");
            assert_eq!(rec_gbl_get_prec(Some(t), 0), 0, "{t:?}");
            assert_eq!(rec_gbl_get_prec(Some(t), 15), 15, "{t:?}");
            assert_eq!(rec_gbl_get_prec(Some(t), 16), 15, "{t:?}");
            assert_eq!(rec_gbl_get_prec(Some(t), -1), 15, "{t:?}");
        }
    }

    /// C's `default: break` (`recGbl.c:141-142`) and the runtime-retyped
    /// field: the seed survives. `recGblGetPrec` reads the STATIC
    /// `pdbFldDes->field_type` (`recGbl.c:123`), so a `cvt_dbaddr` retype
    /// never reaches the switch — `None` is that field.
    #[test]
    fn rec_gbl_get_prec_leaves_the_seed_where_c_has_no_case() {
        for t in [
            Some(DbFieldType::String),
            Some(DbFieldType::Enum),
            // Runtime-typed (DBF_NOACCESS in the dbd).
            None,
        ] {
            assert_eq!(rec_gbl_get_prec(t, 7), 7, "{t:?}");
            assert_eq!(rec_gbl_get_prec(t, 99), 99, "{t:?}");
        }
    }

    /// `DBF_INT64`'s upper bound is 2^63 — `INT64_MAX + 1`, not `INT64_MAX`
    /// (recGbl.c:401-404). Pin it against the exact power of two so a future
    /// "fix" to `i64::MAX` fails here rather than on the wire, where it
    /// silently changes what QSRV2 sends for every int64 field.
    #[test]
    fn int64_upper_range_is_two_to_the_63_not_int64_max() {
        let (upper, lower) = get_max_range_values(DbfCode::Int64).unwrap();
        assert_eq!(upper, 9223372036854775808.0);
        assert_eq!(upper, 2.0f64.powi(63));
        assert!(upper > i64::MAX as f64 || upper == i64::MAX as f64 + 1.0);
        assert_eq!(lower, -9223372036854775808.0);
        assert_eq!(lower, i64::MIN as f64);

        // recGbl.c:405-408
        let (upper, lower) = get_max_range_values(DbfCode::UInt64).unwrap();
        assert_eq!(upper, 18446744073709551615.0);
        assert_eq!(lower, 0.0);
    }

    /// C's switch has no `default:`, so these write nothing and the caller's
    /// 0.0 pre-init (`dbAccess.c:216`, `:256`) reaches the wire. `None` is
    /// that answer.
    #[test]
    fn types_with_no_case_in_the_c_switch_get_no_range() {
        // DBF_STRING — no case.
        assert_eq!(get_max_range_values(DbfCode::String), None);
        // A `runtime_typed` field is DBF_NOACCESS in the .dbd, and C reads the
        // static `pdbFldDes->field_type` (recGbl.c:151/:169) — no case.
        assert_eq!(get_max_range_values(DbfCode::NoAccess), None);
        // DBF_MENU and DBF_DEVICE are distinct codes from DBF_ENUM in C and
        // have no case, even though this port SERVES all three as
        // `DbFieldType::Enum`. The declared code is what separates them: read
        // the served type instead and every menu field gains a bogus 65535/0,
        // which is what `SCAN` and `DTYP` used to report to `gft`.
        assert_eq!(get_max_range_values(DbfCode::Menu), None);
        assert_eq!(get_max_range_values(DbfCode::Device), None);
        assert_eq!(get_max_range_values(DbfCode::Enum), Some((65535.0, 0.0)));
        // The link codes have no case either.
        for t in [DbfCode::Inlink, DbfCode::Outlink, DbfCode::Fwdlink] {
            assert_eq!(get_max_range_values(t), None, "{t:?}");
        }
    }

    /// C's `recGblGetGraphicDouble` and `recGblGetControlDouble` are the same
    /// body over the same table (`recGbl.c:146-153` vs `:164-171`). Pin that
    /// they cannot drift apart.
    #[test]
    fn graphic_and_control_defaults_are_the_same_table() {
        for t in [
            DbfCode::Char,
            DbfCode::UChar,
            DbfCode::Short,
            DbfCode::UShort,
            DbfCode::Enum,
            DbfCode::Menu,
            DbfCode::Device,
            DbfCode::Long,
            DbfCode::ULong,
            DbfCode::Int64,
            DbfCode::UInt64,
            DbfCode::Float,
            DbfCode::Double,
            DbfCode::String,
            DbfCode::Inlink,
            DbfCode::Outlink,
            DbfCode::Fwdlink,
            DbfCode::NoAccess,
        ] {
            assert_eq!(
                rec_gbl_get_graphic_double(t),
                rec_gbl_get_control_double(t),
                "graphic/control default diverged for {t:?}",
            );
        }
    }

    /// `recGblGetAlarmDouble` (`recGbl.c:155-162`) fills all four with
    /// `epicsNAN`, unconditionally — no type table.
    #[test]
    fn alarm_default_is_four_nans() {
        let (hihi, high, low, lolo) = rec_gbl_get_alarm_double();
        assert!(hihi.is_nan() && high.is_nan() && low.is_nan() && lolo.is_nan());
    }

    #[test]
    fn alarm_condition_string_matches_c_table() {
        // Spot-check the EPICS `epicsAlarmConditionStrings` ordering
        // (alarmString.c:27-50) at the boundaries and a couple interior
        // codes; out-of-range returns "".
        assert_eq!(alarm_condition_string(alarm_status::NO_ALARM), "NO_ALARM");
        assert_eq!(alarm_condition_string(alarm_status::HIHI_ALARM), "HIHI");
        assert_eq!(
            alarm_condition_string(alarm_status::HW_LIMIT_ALARM),
            "HWLIMIT"
        );
        assert_eq!(alarm_condition_string(alarm_status::LINK_ALARM), "LINK");
        assert_eq!(
            alarm_condition_string(alarm_status::WRITE_ACCESS_ALARM),
            "WRITE_ACCESS"
        );
        assert_eq!(alarm_condition_string(22), "");
        assert_eq!(alarm_condition_string(9999), "");
    }

    #[test]
    fn test_set_sevr_raises() {
        let mut common = CommonFields::default();
        assert_eq!(common.nsev, AlarmSeverity::NoAlarm);

        rec_gbl_set_sevr(&mut common, alarm_status::HIGH_ALARM, AlarmSeverity::Minor);
        assert_eq!(common.nsev, AlarmSeverity::Minor);
        assert_eq!(common.nsta, alarm_status::HIGH_ALARM);
    }

    #[test]
    fn test_set_sevr_only_raises() {
        let mut common = CommonFields::default();
        rec_gbl_set_sevr(&mut common, alarm_status::HIHI_ALARM, AlarmSeverity::Major);
        rec_gbl_set_sevr(&mut common, alarm_status::HIGH_ALARM, AlarmSeverity::Minor);
        // Should keep the higher severity
        assert_eq!(common.nsev, AlarmSeverity::Major);
        assert_eq!(common.nsta, alarm_status::HIHI_ALARM);
    }

    #[test]
    fn test_reset_alarms_transfers() {
        let mut common = CommonFields::default();
        rec_gbl_set_sevr(&mut common, alarm_status::HIHI_ALARM, AlarmSeverity::Major);

        let result = rec_gbl_reset_alarms(&mut common);
        assert!(result.alarm_changed);
        assert_eq!(result.prev_sevr, AlarmSeverity::NoAlarm);
        assert_eq!(common.sevr, AlarmSeverity::Major);
        assert_eq!(common.stat, alarm_status::HIHI_ALARM);
        // nsta/nsev reset
        assert_eq!(common.nsev, AlarmSeverity::NoAlarm);
        assert_eq!(common.nsta, alarm_status::NO_ALARM);
    }

    #[test]
    fn test_reset_alarms_no_change() {
        let mut common = CommonFields::default();
        // A record is BORN `stat = UDF_ALARM` (dbCommon.dbd `initial("UDF")`),
        // so its FIRST reset is an alarm change: UDF -> NO_ALARM. C posts
        // DBE_ALARM there (recGbl.c:202-208).
        let first = rec_gbl_reset_alarms(&mut common);
        assert!(first.alarm_changed);
        assert_eq!(common.stat, alarm_status::NO_ALARM);

        // No alarm set since — the second reset shows no change.
        let result = rec_gbl_reset_alarms(&mut common);
        assert!(!result.alarm_changed);
    }

    #[test]
    fn test_reset_alarms_clears() {
        let mut common = CommonFields::default();
        // First: set alarm
        common.sevr = AlarmSeverity::Major;
        common.stat = alarm_status::HIHI_ALARM;
        // Don't set nsta/nsev (no alarm this cycle)
        let result = rec_gbl_reset_alarms(&mut common);
        assert!(result.alarm_changed);
        assert_eq!(result.prev_sevr, AlarmSeverity::Major);
        assert_eq!(common.sevr, AlarmSeverity::NoAlarm);
    }

    #[test]
    fn test_check_udf() {
        let mut common = CommonFields::default();
        assert!(common.udf != 0);
        rec_gbl_check_udf(&mut common, false, None, "");
        assert_eq!(common.nsev, AlarmSeverity::Invalid);
        assert_eq!(common.nsta, alarm_status::UDF_ALARM);
        // C's plain `recGblSetSevr(UDF_ALARM)` forwards a NULL message and
        // leaves `namsg` EMPTY (`recGbl.c:249-251,258-261`). The generic message is
        // "", not the old fabricated "UDF: record not initialized".
        assert_eq!(common.namsg, "");
    }

    #[test]
    fn test_check_udf_uses_udfs() {
        let mut common = CommonFields::default();
        assert!(common.udf != 0);
        common.udfs = AlarmSeverity::Minor as i16;
        rec_gbl_check_udf(&mut common, false, None, "");
        assert_eq!(common.nsev, AlarmSeverity::Minor);
        assert_eq!(common.nsta, alarm_status::UDF_ALARM);
    }

    /// C `udf == TRUE` records (exact-one): a `udf` byte of `255` — what a
    /// direct `caput .UDF 255` / `-1` leaves on a record that does not
    /// re-derive `udf` — does NOT raise UDF_ALARM, while the truthy records do.
    #[test]
    fn test_check_udf_exact_one_ignores_non_one_byte() {
        assert!(udf_alarm_active(255, false), "truthy: 255 raises");
        assert!(
            !udf_alarm_active(255, true),
            "exact-one: 255 does not raise"
        );
        assert!(udf_alarm_active(1, true), "exact-one: 1 still raises");
        assert!(!udf_alarm_active(0, true), "exact-one: 0 never raises");

        let mut common = CommonFields::default();
        common.udf = 255;
        rec_gbl_check_udf(&mut common, true, None, "");
        assert_eq!(common.nsev, AlarmSeverity::NoAlarm);
        assert_eq!(common.nsta, alarm_status::NO_ALARM);
    }

    #[test]
    fn test_check_udf_default_udfs_is_invalid() {
        let common = CommonFields::default();
        assert_eq!(common.udfs, AlarmSeverity::Invalid as i16);
    }

    // ----- AMSG / NAMSG (epics-base PR #568 / #566) -----

    #[test]
    fn set_sevr_msg_writes_namsg_when_raised() {
        let mut common = CommonFields::default();
        rec_gbl_set_sevr_msg(
            &mut common,
            alarm_status::HIGH_ALARM,
            AlarmSeverity::Minor,
            "above HIGH threshold",
        );
        assert_eq!(common.nsev, AlarmSeverity::Minor);
        assert_eq!(common.nsta, alarm_status::HIGH_ALARM);
        assert_eq!(common.namsg, "above HIGH threshold");
    }

    #[test]
    fn set_sevr_msg_keeps_higher_message() {
        let mut common = CommonFields::default();
        rec_gbl_set_sevr_msg(
            &mut common,
            alarm_status::HIHI_ALARM,
            AlarmSeverity::Major,
            "above HIHI",
        );
        // Lower-severity follow-up must NOT overwrite the message.
        rec_gbl_set_sevr_msg(
            &mut common,
            alarm_status::HIGH_ALARM,
            AlarmSeverity::Minor,
            "would-be lower message",
        );
        assert_eq!(common.nsev, AlarmSeverity::Major);
        assert_eq!(common.namsg, "above HIHI");
    }

    #[test]
    fn reset_alarms_transfers_amsg_and_clears_namsg() {
        let mut common = CommonFields::default();
        rec_gbl_set_sevr_msg(
            &mut common,
            alarm_status::HIHI_ALARM,
            AlarmSeverity::Major,
            "above HIHI",
        );
        let result = rec_gbl_reset_alarms(&mut common);
        assert!(result.alarm_changed);
        assert!(result.amsg_changed);
        assert_eq!(common.amsg, "above HIHI");
        assert_eq!(common.namsg, "");
    }

    #[test]
    fn reset_alarms_clears_amsg_when_no_new_message() {
        let mut common = CommonFields::default();
        // First cycle: raise an alarm with message.
        rec_gbl_set_sevr_msg(
            &mut common,
            alarm_status::HIGH_ALARM,
            AlarmSeverity::Minor,
            "first cycle",
        );
        rec_gbl_reset_alarms(&mut common);
        assert_eq!(common.amsg, "first cycle");
        // Second cycle: no new alarm, no new message — amsg must clear.
        let result = rec_gbl_reset_alarms(&mut common);
        assert!(result.amsg_changed);
        assert_eq!(common.amsg, "");
    }

    /// R13-61. C `recGbl.c:191-195` gates the `amsg = namsg` copy AND the
    /// `namsg[0] = '\0'` clear on `strcmp(namsg, amsg) != 0`, so a message that
    /// repeats across cycles is never cleared out of `namsg` — and AMSG then
    /// keeps the last alarm text after the alarm itself clears.
    ///
    /// Compiled C (`recGblResetAlarms` + `recGblSetSevrVMsg`, verbatim bodies
    /// over a minimal `dbCommon`), the calcout fail/fail/succeed scenario:
    ///
    /// ```text
    /// cycle1 (calc fails):      sevr=3 stat=12 amsg='calcPerform' namsg=''
    /// cycle2 (fails, same msg): sevr=3 stat=12 amsg='calcPerform' namsg='calcPerform'
    /// cycle3 (succeeds):        sevr=0 stat=0  amsg='calcPerform' namsg='calcPerform'
    /// ```
    ///
    /// Cycle 3 still POSTS amsg (sevr moved, so `stat_mask` carries DBE_ALARM)
    /// — it just carries the stale text, not `""`.
    #[test]
    fn reset_alarms_keeps_amsg_when_the_same_message_repeats() {
        let mut common = CommonFields::default();

        // cycle 1: calcPerform fails.
        rec_gbl_set_sevr_msg(
            &mut common,
            alarm_status::CALC_ALARM,
            AlarmSeverity::Invalid,
            "calcPerform",
        );
        let c1 = rec_gbl_reset_alarms(&mut common);
        assert!(c1.amsg_changed);
        assert_eq!(common.sevr, AlarmSeverity::Invalid);
        assert_eq!(common.amsg, "calcPerform");
        assert_eq!(common.namsg, "");

        // cycle 2: still fails, SAME message. C leaves `namsg` alone.
        rec_gbl_set_sevr_msg(
            &mut common,
            alarm_status::CALC_ALARM,
            AlarmSeverity::Invalid,
            "calcPerform",
        );
        let c2 = rec_gbl_reset_alarms(&mut common);
        assert!(!c2.alarm_changed);
        assert!(
            !c2.amsg_changed,
            "an unchanged message is not an AMSG event"
        );
        assert_eq!(common.amsg, "calcPerform");
        assert_eq!(
            common.namsg, "calcPerform",
            "C does NOT clear namsg when it equals amsg (recGbl.c:191-195)"
        );

        // cycle 3: the calc succeeds — no `recGblSetSevr*` call at all.
        let c3 = rec_gbl_reset_alarms(&mut common);
        assert!(c3.alarm_changed, "severity dropped INVALID -> NO_ALARM");
        assert!(
            !c3.amsg_changed,
            "namsg still equals amsg, so recGblResetAlarms does not touch it"
        );
        assert_eq!(common.sevr, AlarmSeverity::NoAlarm);
        assert_eq!(
            common.amsg, "calcPerform",
            "AMSG keeps the last alarm text after the alarm clears"
        );
    }

    #[test]
    fn test_event_mask_ops() {
        let mask = EventMask::VALUE | EventMask::ALARM;
        assert!(mask.contains(EventMask::VALUE));
        assert!(mask.contains(EventMask::ALARM));
        assert!(!mask.contains(EventMask::LOG));
        assert!(mask.intersects(EventMask::VALUE));
        assert!(!mask.intersects(EventMask::PROPERTY));
        assert!(!mask.is_empty());
        assert!(EventMask::NONE.is_empty());
    }

    // ----- ACKS / ACKT auto-raise (recGbl.c:209-222 parity) -----

    /// C `recGblResetAlarms` raises `acks` to the new severity when the
    /// alarm changes, unless `ackt` is true AND the new severity is
    /// below the current `acks`. Default `ackt=true` is the "sticky"
    /// alarm-acknowledge mode — once raised, `acks` only drops when an
    /// operator writes back to ACKS. Without this, sticky-alarm
    /// tracking is dead on every record built with default ACKT=YES.
    #[test]
    fn reset_alarms_raises_acks_to_new_severity() {
        let mut common = CommonFields::default();
        assert_eq!(common.acks, AlarmSeverity::NoAlarm);
        assert!(common.ackt, "ACKT defaults to true (sticky)");

        rec_gbl_set_sevr(&mut common, alarm_status::HIHI_ALARM, AlarmSeverity::Major);
        let result = rec_gbl_reset_alarms(&mut common);

        assert!(result.alarm_changed);
        assert!(result.acks_posted, "first alarm raise must update acks");
        assert_eq!(common.acks, AlarmSeverity::Major);
    }

    /// `ackt=true` + dropping severity must NOT lower `acks` — the
    /// operator clears it via a CA put to ACKS. C path:
    /// `if (!ackt || new_sevr >= acks) acks = new_sevr` — when ackt=true
    /// AND new_sevr < acks, neither branch matches and acks stays.
    #[test]
    fn reset_alarms_keeps_acks_sticky_when_ackt_true_and_severity_drops() {
        let mut common = CommonFields::default();
        // First cycle: raise to MAJOR.
        rec_gbl_set_sevr(&mut common, alarm_status::HIHI_ALARM, AlarmSeverity::Major);
        rec_gbl_reset_alarms(&mut common);
        assert_eq!(common.acks, AlarmSeverity::Major);

        // Second cycle: drop to MINOR. acks must stay at MAJOR (sticky).
        rec_gbl_set_sevr(&mut common, alarm_status::HIGH_ALARM, AlarmSeverity::Minor);
        let result = rec_gbl_reset_alarms(&mut common);
        assert!(result.alarm_changed);
        assert!(
            !result.acks_posted,
            "ackt=true must NOT lower acks when severity drops"
        );
        assert_eq!(common.acks, AlarmSeverity::Major);
    }

    /// R13-62. C posts ACKS whenever the acknowledge RULE fires
    /// (recGbl.c:214-217) — `if (!ackt || new_sevr >= acks) { acks = new_sevr;
    /// db_post_events(&acks, DBE_VALUE); }` — with no value-change test around
    /// the post. A stat-only transition at constant severity therefore re-posts
    /// an ACKS that is already equal to SEVR.
    ///
    /// Compiled C, `ACKT=1`, LINK_ALARM/INVALID → CALC_ALARM/INVALID (STAT
    /// moves 14→12, SEVR stays 3, ACKS already 3):
    ///
    /// ```text
    /// cycle2 posts:  stat(mask=5)  amsg(mask=5)  acks(mask=1)
    /// ```
    ///
    /// Three events, ACKS among them. The pre-fix gate (`acks != sevr`)
    /// emitted STAT and AMSG but not ACKS, so a `DBE_VALUE`-only `.ACKS`
    /// subscriber missed it.
    #[test]
    fn reset_alarms_posts_acks_on_a_stat_only_transition_at_constant_severity() {
        let mut common = CommonFields::default();
        assert!(common.ackt, "ACKT defaults to true");

        // cycle 1: LINK_ALARM / INVALID. Raises acks to INVALID.
        rec_gbl_set_sevr_msg(
            &mut common,
            alarm_status::LINK_ALARM,
            AlarmSeverity::Invalid,
            "field INP",
        );
        let c1 = rec_gbl_reset_alarms(&mut common);
        assert!(c1.acks_posted);
        assert_eq!(common.acks, AlarmSeverity::Invalid);

        // cycle 2: CALC_ALARM / INVALID — STAT moves, SEVR does not, and ACKS
        // is ALREADY equal to SEVR. C's rule fires (`new_sevr >= acks`) and
        // posts anyway.
        rec_gbl_set_sevr_msg(
            &mut common,
            alarm_status::CALC_ALARM,
            AlarmSeverity::Invalid,
            "calcPerform",
        );
        let c2 = rec_gbl_reset_alarms(&mut common);
        assert!(c2.alarm_changed, "STAT moved LINK -> CALC");
        assert_eq!(common.sevr, c2.prev_sevr, "SEVR did not move");
        assert_eq!(
            common.acks,
            AlarmSeverity::Invalid,
            "ACKS value is unchanged — it was already equal"
        );
        assert!(
            c2.acks_posted,
            "C posts ACKS whenever the ack rule fires, not only on a value change"
        );
    }

    /// The other half of the boundary: when the ack rule does NOT fire, there
    /// is no ACKS event at all. `ackt=true` + a severity BELOW the remembered
    /// `acks` misses both arms of `if (!ackt || new_sevr >= acks)`.
    #[test]
    fn reset_alarms_posts_no_acks_when_the_rule_does_not_fire() {
        let mut common = CommonFields::default();
        rec_gbl_set_sevr(&mut common, alarm_status::HIHI_ALARM, AlarmSeverity::Major);
        rec_gbl_reset_alarms(&mut common);
        assert_eq!(common.acks, AlarmSeverity::Major);

        // MAJOR -> MINOR with ackt=true: rule does not fire, so no post.
        rec_gbl_set_sevr(&mut common, alarm_status::HIGH_ALARM, AlarmSeverity::Minor);
        let result = rec_gbl_reset_alarms(&mut common);
        assert!(result.alarm_changed);
        assert!(!result.acks_posted);
        assert_eq!(common.acks, AlarmSeverity::Major);
    }

    /// `ackt=false` (transient alarm acknowledge) drops `acks` together
    /// with severity. C path: `!ackt` short-circuits and assigns
    /// `acks = new_sevr` unconditionally on every alarm change.
    #[test]
    fn reset_alarms_drops_acks_when_ackt_false() {
        let mut common = CommonFields::default();
        common.ackt = false; // transient mode
        rec_gbl_set_sevr(&mut common, alarm_status::HIHI_ALARM, AlarmSeverity::Major);
        rec_gbl_reset_alarms(&mut common);
        assert_eq!(common.acks, AlarmSeverity::Major);

        rec_gbl_set_sevr(&mut common, alarm_status::HIGH_ALARM, AlarmSeverity::Minor);
        let result = rec_gbl_reset_alarms(&mut common);
        assert!(result.acks_posted);
        assert_eq!(
            common.acks,
            AlarmSeverity::Minor,
            "ackt=false drops acks with severity"
        );
    }
}
