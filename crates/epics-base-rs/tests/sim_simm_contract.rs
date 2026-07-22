//! The C simulation-mode contract — `recGbl.c:421-457` (`recGblSaveSimm`,
//! `recGblCheckSimm`, `recGblInitSimm`, `recGblGetSimm`) — as the single owner
//! of a record's SIMM transition.
//!
//! # R12-61 — SIMM=YES with an unset SIML and an unset SIOL must still simulate
//!
//! C's `readValue` dispatches on SIMM alone; the SIML/SIOL links are read
//! INSIDE that dispatch, never as a precondition for it. And an unset link is a
//! CONSTANT link (`dbLink.c::dbLinkIsConstant`), whose `dbConstGetValue`
//! (`dbConstLink.c:219-225`) returns SUCCESS with the caller's buffer
//! untouched. So for `longinRecord.c:411-421`:
//!
//! ```c
//! case menuYesNoYES: {
//!     recGblSetSevr(prec, SIMM_ALARM, prec->sims);       /* unconditional */
//!     status = dbGetLink(&prec->siol, DBR_LONG, &prec->sval, 0, 0);
//!     if (status == 0) {                                 /* constant: yes */
//!         prec->val = prec->sval;
//!         prec->udf = FALSE;
//!     }
//! ```
//!
//! an unset SIOL yields `val = sval` — the "simulate against a constant" idiom
//! (`caput REC.SIMM 1; caput REC.SVAL 42`). The pre-fix port returned
//! `NotSimulated` before SIMM was even read whenever SIML and SIOL were both
//! empty, so the idiom was a complete no-op on every record type.

// RTEMS-EXEC-MODEL-ALLOW(15): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::longin::LonginRecord;
use epics_base_rs::types::EpicsValue;

/// `caput REC.SIMM 1; caput REC.SVAL 42; caput REC.PROC 1` on a `longin` with
/// NO SIML and NO SIOL: C reads SIMM (YES), raises SIMM_ALARM at SIMS, reads
/// the (constant, unset) SIOL — status 0, SVAL untouched — and publishes
/// `VAL = SVAL = 42` with UDF cleared.
#[tokio::test]
async fn simm_yes_with_unset_siml_and_siol_simulates_from_sval() {
    let db = PvDatabase::new();
    let mut li = LonginRecord::new(7); // VAL = 7 (the pre-simulation value)
    li.sims = 2; // SIMS = MAJOR, so the SIMM_ALARM is observable
    // SIML and SIOL are left unset — the case the pre-fix gate short-circuited.
    db.add_record("SIMCONST", Box::new(li)).await.unwrap();

    db.put_pv("SIMCONST.SVAL", EpicsValue::Long(42))
        .await
        .unwrap();
    db.put_pv("SIMCONST.SIMM", EpicsValue::Short(1))
        .await
        .unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("SIMCONST", &mut v, 0)
        .await
        .unwrap();

    let val = db.get_pv("SIMCONST").unwrap();
    assert_eq!(
        val,
        EpicsValue::Long(42),
        "C `val = sval` on the status-0 constant SIOL read; got {val:?}"
    );

    let rec = db.get_record("SIMCONST").unwrap();
    let inst = rec.read();
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Major,
        "C raises recGblSetSevr(SIMM_ALARM, prec->sims) independently of the SIOL read"
    );
    assert_eq!(
        inst.common.stat,
        epics_base_rs::server::recgbl::alarm_status::SIMM_ALARM
    );
    assert!(
        inst.common.udf == 0,
        "C clears UDF on the status-0 SIOL read"
    );
}

/// The same record with SIMM back at NO must NOT simulate: the SVAL is ignored
/// and the real (soft, empty INP) device path runs, leaving VAL alone.
#[tokio::test]
async fn simm_no_with_unset_links_does_not_simulate() {
    let db = PvDatabase::new();
    let mut li = LonginRecord::new(7);
    li.sims = 2;
    db.add_record("SIMOFF", Box::new(li)).await.unwrap();

    db.put_pv("SIMOFF.SVAL", EpicsValue::Long(42))
        .await
        .unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("SIMOFF", &mut v, 0)
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("SIMOFF").unwrap(),
        EpicsValue::Long(7),
        "SIMM=NO must not copy SVAL into VAL"
    );
    let rec = db.get_record("SIMOFF").unwrap();
    assert_ne!(
        rec.read().common.stat,
        epics_base_rs::server::recgbl::alarm_status::SIMM_ALARM,
        "SIMM=NO raises no SIMM_ALARM"
    );
}

// ---------------------------------------------------------------------------
// R12-64 — the SIMM↔SSCN scan swap (`recGblCheckSimm`, recGbl.c:427-437)
// ---------------------------------------------------------------------------
//
// ```c
// void recGblCheckSimm(struct dbCommon *pcommon, epicsEnum16 *psscn,
//     const epicsEnum16 oldsimm, const epicsEnum16 simm) {
//     if (*psscn == USHRT_MAX) return;
//     if (simm != oldsimm) {
//         epicsUInt16 scan = pcommon->scan;
//         scanDelete(pcommon);
//         pcommon->scan = *psscn;
//         scanAdd(pcommon);
//         *psscn = scan;
//     }
// }
// ```
//
// Reached from `special(SPC_MOD)` pass 1 on a SIMM put (longinRecord.c:171-177)
// and from the tail of `recGblGetSimm`/`recGblInitSimm`. Before the fix, SSCN
// and OLDSIMM were inert storage: no site in the port swapped anything.

use epics_base_rs::server::record::ScanType;
use epics_base_rs::server::records::busy::BusyRecord;

/// menuScan index of "1 second" (`menuScan.dbd`: 0 Passive … 6 "1 second").
const SCAN_1_SECOND: u16 = 6;
const SCAN_PASSIVE: u16 = 0;

/// `field(SCAN,"1 second") field(SSCN,"Passive")`: entering simulation stops the
/// periodic scan, and SSCN comes back holding the scan the record just left.
/// Leaving simulation swaps them back. Both directions are a swap, not an
/// assignment.
#[tokio::test]
async fn simm_transition_swaps_scan_with_sscn() {
    let db = PvDatabase::new();
    db.add_record("SWAP", Box::new(LonginRecord::new(1)))
        .await
        .unwrap();
    db.put_pv("SWAP.SCAN", EpicsValue::Enum(SCAN_1_SECOND))
        .await
        .unwrap();
    db.put_pv("SWAP.SSCN", EpicsValue::Enum(SCAN_PASSIVE))
        .await
        .unwrap();

    // SIMM NO -> YES: scan and sscn trade places.
    db.put_pv("SWAP.SIMM", EpicsValue::Short(1)).await.unwrap();
    {
        let rec = db.get_record("SWAP").unwrap();
        let inst = rec.read();
        assert_eq!(
            inst.common.scan,
            ScanType::Passive,
            "C `pcommon->scan = *psscn` — simulation adopts SSCN's scan"
        );
        assert_eq!(
            inst.get_common_field("SSCN"),
            Some(EpicsValue::Enum(SCAN_1_SECOND)),
            "C `*psscn = scan` — SSCN takes the scan the record just left"
        );
        assert_eq!(
            inst.get_common_field("OLDSIMM"),
            Some(EpicsValue::Short(0)),
            "C `recGblSaveSimm` latched the OUTGOING mode (NO) before the put"
        );
    }

    // SIMM YES -> NO: swapped back.
    db.put_pv("SWAP.SIMM", EpicsValue::Short(0)).await.unwrap();
    {
        let rec = db.get_record("SWAP").unwrap();
        let inst = rec.read();
        assert_eq!(inst.common.scan, ScanType::Sec1);
        assert_eq!(
            inst.get_common_field("SSCN"),
            Some(EpicsValue::Enum(SCAN_PASSIVE))
        );
        assert_eq!(
            inst.get_common_field("OLDSIMM"),
            Some(EpicsValue::Short(1)),
            "the latch now holds the mode the record left (YES)"
        );
    }
}

/// `*psscn == USHRT_MAX` (the dbd default, `initial("65535")`) — C returns
/// immediately from BOTH recGblSaveSimm and recGblCheckSimm, so SCAN is
/// untouched and OLDSIMM is never even latched.
#[tokio::test]
async fn unset_sscn_leaves_scan_alone_on_a_simm_transition() {
    let db = PvDatabase::new();
    db.add_record("NOSSCN", Box::new(LonginRecord::new(1)))
        .await
        .unwrap();
    db.put_pv("NOSSCN.SCAN", EpicsValue::Enum(SCAN_1_SECOND))
        .await
        .unwrap();

    db.put_pv("NOSSCN.SIMM", EpicsValue::Short(1))
        .await
        .unwrap();

    let rec = db.get_record("NOSSCN").unwrap();
    let inst = rec.read();
    assert_eq!(inst.common.scan, ScanType::Sec1);
    assert_eq!(
        inst.get_common_field("SSCN"),
        Some(EpicsValue::Enum(65535)),
        "the sentinel must survive the transition"
    );
    assert_eq!(
        inst.get_common_field("OLDSIMM"),
        Some(EpicsValue::Short(0)),
        "recGblSaveSimm returns before the latch when sscn == USHRT_MAX"
    );
}

/// `busy` (`busyRecord.dbd`) declares SIMM/SIML/SIOL but NO SSCN and NO
/// OLDSIMM, and `busyRecord.c` calls neither recGblSaveSimm nor
/// recGblCheckSimm — it has no `special()` at all. So a SIMM transition on a
/// busy record must NOT move its SCAN, even with SSCN set. Same for `swait`.
#[tokio::test]
async fn busy_has_no_sscn_so_a_simm_transition_never_swaps_its_scan() {
    let db = PvDatabase::new();
    db.add_record("BUSY", Box::new(BusyRecord::new()))
        .await
        .unwrap();
    db.put_pv("BUSY.SCAN", EpicsValue::Enum(SCAN_1_SECOND))
        .await
        .unwrap();
    db.put_pv("BUSY.SSCN", EpicsValue::Enum(SCAN_PASSIVE))
        .await
        .unwrap();

    db.put_pv("BUSY.SIMM", EpicsValue::Short(1)).await.unwrap();

    let rec = db.get_record("BUSY").unwrap();
    let inst = rec.read();
    assert_eq!(
        inst.common.scan,
        ScanType::Sec1,
        "busy's C support never reaches recGblCheckSimm"
    );
    assert_eq!(
        inst.get_common_field("SSCN"),
        Some(EpicsValue::Enum(SCAN_PASSIVE))
    );
}

// ---------------------------------------------------------------------------
// R12-65 — a FAILED SIML read raises LINK_ALARM
// ---------------------------------------------------------------------------
//
// Two C shapes, and the difference is not cosmetic:
//
// `recGblGetSimm` (recGbl.c:448-457) reads SIML with `dbTryGetLink`, which —
// unlike `dbGetLink` — does NOT call `setLinkAlarm`. It then raises the alarm
// by writing `nsta` DIRECTLY:
//
// ```c
// status = dbTryGetLink(psiml, DBR_USHORT, psimm, 0);
// if (status && !pcommon->nsev) pcommon->nsta = LINK_ALARM;
// ```
//
// NOT `recGblSetSevr`. So `recGblResetAlarms` publishes STAT=LINK_ALARM with
// SEVR still NO_ALARM — an alarm status with no severity. `busy` and `swait`
// read SIML with a plain `dbGetLink` (busyRecord.c:399, swaitRecord.c:402),
// whose failure path DOES go through `setLinkAlarm` (dbLink.c:319-323) →
// `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM)`, a full severity raise.
//
// The port raised neither.

use epics_base_rs::server::recgbl::alarm_status;

/// The 21 recGblGetSimm records: STAT=LINK_ALARM, SEVR untouched.
#[tokio::test]
async fn failed_siml_read_sets_nsta_link_alarm_without_touching_sevr() {
    let db = PvDatabase::new();
    let mut li = LonginRecord::new(5);
    // A DB link to a record that does not exist: `dbTryGetLink` fails.
    li.siml = "NO:SUCH:RECORD".to_string();
    db.add_record("SIMLFAIL", Box::new(li)).await.unwrap();
    // VAL written so the record's own UDF alarm (INVALID) cannot mask the
    // severity-less LINK_ALARM we are asserting on.
    db.put_pv("SIMLFAIL", EpicsValue::Long(5)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("SIMLFAIL", &mut v, 0)
        .await
        .unwrap();

    let rec = db.get_record("SIMLFAIL").unwrap();
    let inst = rec.read();
    assert_eq!(
        inst.common.stat,
        alarm_status::LINK_ALARM,
        "C `pcommon->nsta = LINK_ALARM` on the failed dbTryGetLink"
    );
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::NoAlarm,
        "C writes nsta DIRECTLY, not through recGblSetSevr — SEVR stays NO_ALARM"
    );
}

/// `busy` reads SIML with a plain `dbGetLink`, so its failure is a full
/// `setLinkAlarm` — LINK_ALARM at INVALID severity.
#[tokio::test]
async fn busy_failed_siml_read_raises_link_alarm_at_invalid_severity() {
    let db = PvDatabase::new();
    let mut b = BusyRecord::new();
    b.siml = "NO:SUCH:RECORD".to_string();
    db.add_record("BUSYFAIL", Box::new(b)).await.unwrap();
    db.put_pv("BUSYFAIL", EpicsValue::Short(0)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("BUSYFAIL", &mut v, 0)
        .await
        .unwrap();

    let rec = db.get_record("BUSYFAIL").unwrap();
    let inst = rec.read();
    assert_eq!(inst.common.stat, alarm_status::LINK_ALARM);
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "C `dbGetLink` -> `setLinkAlarm` -> recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM)"
    );
}

// ---------------------------------------------------------------------------
// W10-E4 — a failed SIOL read raises LINK_ALARM, and the SIMM/LINK tie is
//          decided by C's CALL ORDER
// ---------------------------------------------------------------------------
//
// A base record reads SIOL with a plain `dbGetLink` (`longinRecord.c:416`,
// aiRecord, histogramRecord, waveformRecord), whose failure path is
// `setLinkAlarm` (dbLink.c:322) -> `recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM,
// "field %s")`. The port raised only SIMM_ALARM, so under the DEFAULT
// `SIMS = NO_ALARM` a broken simulation link was completely silent.
//
// The order is load-bearing. C raises SIMM_ALARM at the TOP of the YES arm
// (`longinRecord.c:414`), BEFORE the read at `:416`. `recGblSetSevr` is a
// strict-greater MAXIMIZE, so at `SIMS = INVALID` the two alarms tie and the one
// raised FIRST (SIMM) keeps STAT. Compiled C (`recGblResetAlarms` +
// `recGblSetSevrVMsg`, verbatim):
//
// ```text
// longin (SIMM then LINK), SIMS=INVALID:   sevr=3 stat=19 amsg=''
// longin (SIMM then LINK), SIMS=NO_ALARM:  sevr=3 stat=14 amsg='field SIOL'
// ```
//
// swait reverses the order (`dbGetLink` at :416, `recGblSetSevr` at :420) and so
// reverses the winner — pinned in `swait_simulation_mode.rs`.

/// The headline: with the DEFAULT `SIMS = NO_ALARM`, a broken SIOL is reported
/// as LINK_ALARM/INVALID with AMSG "field SIOL". The port reported NO_ALARM.
#[tokio::test]
async fn failed_siol_read_raises_link_alarm_at_default_sims() {
    let db = PvDatabase::new();
    let mut li = LonginRecord::new(5);
    li.simm = 1; // YES — simulate
    li.siol = "NO:SUCH:RECORD".to_string(); // dbGetLink fails
    li.sims = 0; // NO_ALARM — the dbd default
    db.add_record("SIOLFAIL", Box::new(li)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("SIOLFAIL", &mut v, 0)
        .await
        .unwrap();

    let rec = db.get_record("SIOLFAIL").unwrap();
    let inst = rec.read();
    assert_eq!(
        inst.common.sevr,
        AlarmSeverity::Invalid,
        "C `dbGetLink` -> `setLinkAlarm` -> recGblSetSevrMsg(LINK_ALARM, INVALID)"
    );
    assert_eq!(
        inst.common.stat,
        alarm_status::LINK_ALARM,
        "SIMS=NO_ALARM raises nothing, so LINK_ALARM lands unopposed"
    );
    assert_eq!(
        inst.common.amsg, "field SIOL",
        "C `setLinkAlarm` formats \"field %s\" from the link's field name"
    );
}

/// The ordering boundary: at `SIMS = INVALID` the two alarms are EQUAL, and a
/// base record raised SIMM FIRST — so the strict-greater `recGblSetSevr` leaves
/// SIMM_ALARM in STAT and the LINK_ALARM loses the tie. (AMSG is empty: the
/// SIMM raise went through message-less `recGblSetSevr`, which CLEARS namsg.)
#[tokio::test]
async fn failed_siol_read_loses_the_tie_to_simm_alarm_at_sims_invalid() {
    let db = PvDatabase::new();
    let mut li = LonginRecord::new(5);
    li.simm = 1;
    li.siol = "NO:SUCH:RECORD".to_string();
    li.sims = 3; // INVALID — same severity as the LINK_ALARM
    db.add_record("SIOLTIE", Box::new(li)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("SIOLTIE", &mut v, 0)
        .await
        .unwrap();

    let rec = db.get_record("SIOLTIE").unwrap();
    let inst = rec.read();
    assert_eq!(inst.common.sevr, AlarmSeverity::Invalid);
    assert_eq!(
        inst.common.stat,
        alarm_status::SIMM_ALARM,
        "C raises SIMM BEFORE the SIOL read (longinRecord.c:414 then :416), so the \
         equal-severity LINK_ALARM cannot displace it"
    );
    assert_eq!(
        inst.common.amsg, "",
        "the SIMM raise is a message-less recGblSetSevr, which clears namsg"
    );
}

// ---------------------------------------------------------------------------
// R11-C12 — SIMM = 2 (RAW) on a `menu(menuYesNo)` record is the `default:` arm
// ---------------------------------------------------------------------------
//
// The legal SIMM arms are the choices of the RECORD'S OWN menu. Eight records
// (ai/ao/bi/bo/mbbi/mbbo/mbbiDirect/mbboDirect) have `menu(menuSimm)` —
// NO/YES/RAW. The other thirteen (event, histogram, int64in, int64out, longin,
// longout, lsi, lso, stringin, stringout, waveform, aai, aao) plus `busy` have
// `menu(menuYesNo)` — NO/YES only. On those, `SIMM = 2` is not RAW, it is
// out-of-menu, and C's switch sends it to:
//
// ```c
// default:
//     recGblSetSevr(prec, SOFT_ALARM, INVALID_ALARM);
//     status = -1;
// ```
//
// No device substitution of any kind: no device read, no device write, no SIOL
// round-trip, no SIMM_ALARM, no VAL/UDF change. The port instead ran its RAW
// branch on these records. Nothing validates the index either — `recGblGetSimm`
// writes SIMM straight from `dbTryGetLink`, and `dbPut` of a numeric DBR into a
// DBF_MENU does no menu check — so ANY out-of-menu SIMM lands on this arm.
//
// `swait` is the one exception (`swaitRecord.c:407-421` has no `default:`).

use epics_base_rs::server::records::longout::LongoutRecord;

/// A menuYesNo INPUT record with SIMM=2: SOFT_ALARM/INVALID, and SVAL is NOT
/// copied into VAL (C never reaches the SIOL read on this arm).
#[tokio::test]
async fn simm_raw_on_a_menu_yesno_input_is_soft_alarm_and_no_substitution() {
    let db = PvDatabase::new();
    let mut li = LonginRecord::new(7);
    li.sims = 2; // MAJOR — would be the SIMM_ALARM severity if YES were taken
    db.add_record("RAWIN", Box::new(li)).await.unwrap();
    db.put_pv("RAWIN.SVAL", EpicsValue::Long(42)).await.unwrap();
    db.put_pv("RAWIN.SIMM", EpicsValue::Short(2)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("RAWIN", &mut v, 0)
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("RAWIN").unwrap(),
        EpicsValue::Long(7),
        "C's default arm performs NO device substitution — SVAL must not reach VAL"
    );
    let rec = db.get_record("RAWIN").unwrap();
    let inst = rec.read();
    assert_eq!(
        inst.common.stat,
        alarm_status::SOFT_ALARM,
        "C `recGblSetSevr(prec, SOFT_ALARM, INVALID_ALARM)`"
    );
    assert_eq!(inst.common.sevr, AlarmSeverity::Invalid);
}

/// A menuYesNo OUTPUT record with SIMM=2: SOFT_ALARM/INVALID, and NOTHING is
/// written — not the device/OUT link, not SIOL. C `writeValue` returns -1 from
/// the default arm, before either write.
#[tokio::test]
async fn simm_raw_on_a_menu_yesno_output_writes_nothing() {
    let db = PvDatabase::new();
    db.add_record("SINK", Box::new(LonginRecord::new(0)))
        .await
        .unwrap();
    let mut lo = LongoutRecord::new(0);
    lo.siol = "SINK".to_string();
    db.add_record("RAWOUT", Box::new(lo)).await.unwrap();
    db.put_pv("RAWOUT.SIMM", EpicsValue::Short(2))
        .await
        .unwrap();
    db.put_pv("RAWOUT", EpicsValue::Long(99)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("RAWOUT", &mut v, 0)
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("SINK").unwrap(),
        EpicsValue::Long(0),
        "the default arm returns before the SIOL redirect — SIOL must not be written"
    );
    let rec = db.get_record("RAWOUT").unwrap();
    let inst = rec.read();
    assert_eq!(inst.common.stat, alarm_status::SOFT_ALARM);
    assert_eq!(inst.common.sevr, AlarmSeverity::Invalid);
}

/// `busy` is menuYesNo too (`busyRecord.c:409-413` is the `else` of its YES
/// test): SIMM=2 raises SOFT_ALARM/INVALID and writes nothing.
#[tokio::test]
async fn busy_simm_raw_is_soft_alarm_and_writes_nothing() {
    let db = PvDatabase::new();
    db.add_record("BSINK", Box::new(LonginRecord::new(0)))
        .await
        .unwrap();
    let mut b = BusyRecord::new();
    b.siol = "BSINK".to_string();
    db.add_record("BRAW", Box::new(b)).await.unwrap();
    db.put_pv("BRAW.SIMM", EpicsValue::Short(2)).await.unwrap();
    db.put_pv("BRAW", EpicsValue::Short(1)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("BRAW", &mut v, 0)
        .await
        .unwrap();

    assert_eq!(db.get_pv("BSINK").unwrap(), EpicsValue::Long(0));
    let rec = db.get_record("BRAW").unwrap();
    let inst = rec.read();
    assert_eq!(inst.common.stat, alarm_status::SOFT_ALARM);
    assert_eq!(inst.common.sevr, AlarmSeverity::Invalid);
}

/// The guard on the other side: `ai` IS `menu(menuSimm)`, so SIMM=2 is the
/// legal RAW arm — it must still simulate (SIOL -> SVAL -> RVAL -> conversion),
/// with SIMM_ALARM and no SOFT_ALARM.
#[tokio::test]
async fn simm_raw_on_a_menu_simm_record_still_simulates() {
    use epics_base_rs::server::records::ai::AiRecord;

    let db = PvDatabase::new();
    let mut ai = AiRecord::new(0.0);
    ai.sims = 1; // MINOR
    ai.sval = 5.0;
    db.add_record("AIRAW", Box::new(ai)).await.unwrap();
    db.put_pv("AIRAW.SIMM", EpicsValue::Short(2)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("AIRAW", &mut v, 0)
        .await
        .unwrap();

    let rec = db.get_record("AIRAW").unwrap();
    let inst = rec.read();
    assert_eq!(
        inst.common.stat,
        alarm_status::SIMM_ALARM,
        "menuSimm's RAW arm is legal — SIMM_ALARM, never SOFT_ALARM"
    );
    assert_eq!(inst.common.sevr, AlarmSeverity::Minor);
}

// ---------------------------------------------------------------------------
// W10-E5 — a failed SIML read on `busy` returns BEFORE the output write
// ---------------------------------------------------------------------------
//
// busyRecord.c:388-401 `writeValue`:
//
// ```c
//     status = dbGetLink(&prec->siml, DBR_USHORT, &prec->simm, 0, 0);
//     if (status)
//         return status;                     /* <- before write_busy AND
//                                               before the SIOL dbPutLink */
//     if (prec->simm == menuYesNoNO) {
//         status = pdset->write_busy(prec);
//         ...
// ```
//
// The 21 `recGblGetSimm` records cannot take this branch: recGbl.c:456 ends
// `recGblGetSimm` with an unconditional `return 0`, so their
// `if (status) return status;` is dead code. `swait` reads SIML with a plain
// `dbGetLink` but never tests the status (swaitRecord.c:402). `busy` is the
// only record that actually aborts.

/// A broken SIML on a `busy` with a local OUT link: C returns from
/// `writeValue` before the write, so the OUT target must not be driven.
#[tokio::test]
async fn w10_e5_busy_failed_siml_read_performs_no_output_write() {
    let db = PvDatabase::new();
    db.add_record("E5TGT", Box::new(LonginRecord::new(0)))
        .await
        .unwrap();

    let mut b = BusyRecord::new();
    b.siml = "NO:SUCH:RECORD".to_string();
    db.add_record("E5BUSY", Box::new(b)).await.unwrap();
    db.put_pv("E5BUSY.OUT", EpicsValue::String("E5TGT".into()))
        .await
        .unwrap();
    db.put_pv("E5BUSY", EpicsValue::Short(1)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("E5BUSY", &mut v, 0)
        .await
        .unwrap();

    let inst = db.get_record("E5BUSY").unwrap();
    {
        let inst = inst.read();
        assert_eq!(
            inst.common.stat,
            alarm_status::LINK_ALARM,
            "dbGetLink -> setLinkAlarm on the failed SIML read"
        );
        assert_eq!(inst.common.sevr, AlarmSeverity::Invalid);
    }

    assert_eq!(
        db.get_pv("E5TGT").unwrap(),
        EpicsValue::Long(0),
        "busyRecord.c:399-401 returns before write_busy — the OUT target keeps its value"
    );
}

/// The same busy in SIMM=YES: the abort precedes the SIOL redirect too, so a
/// failed SIML read means NO write of any kind — not even the simulated one.
#[tokio::test]
async fn w10_e5_busy_failed_siml_read_suppresses_the_siol_redirect_as_well() {
    let db = PvDatabase::new();
    db.add_record("E5STGT", Box::new(LonginRecord::new(0)))
        .await
        .unwrap();

    let mut b = BusyRecord::new();
    b.siml = "NO:SUCH:RECORD".to_string();
    b.siol = "E5STGT".to_string();
    b.simm = 1; // YES — but the SIML read overwrites SIMM before it is tested,
    // and in C the failed read returns before that test is ever reached.
    db.add_record("E5SBUSY", Box::new(b)).await.unwrap();
    db.put_pv("E5SBUSY", EpicsValue::Short(1)).await.unwrap();

    let mut v = HashSet::new();
    db.process_record_with_links("E5SBUSY", &mut v, 0)
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("E5STGT").unwrap(),
        EpicsValue::Long(0),
        "the `if (status) return status` precedes the `dbPutLink(&prec->siol, ...)`"
    );
}
