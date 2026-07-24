//! R15-77: a failed closed-loop DOL read ABORTS the aao's cycle.
//!
//! C `aaoRecord.c::process` (164-174):
//!
//! ```c
//!     if (!pact) {
//!         prec->udf = FALSE;
//!         if (!!(status = fetchValue(prec, 0)))
//!             return status;                    /* <-- abort */
//!         recGblGetTimeStampSimm(prec, prec->simm, NULL);
//!     }
//!     status = writeValue(prec);
//!     ...
//!     monitor(prec);
//!     recGblFwdLink(prec);
//! ```
//!
//! The `return` is BEFORE `writeValue`, the timestamp, `monitor` and
//! `recGblFwdLink`. So when the desired-output source is unreachable, the record
//! must NOT push its stale VAL to the device / OUT target as if it were the new
//! desired output, must not post it, and must not trigger the forward link.
//! `fetchValue`'s `dbGetLink` has already raised LINK/INVALID
//! (`dbLink.c:316-323` `setLinkAlarm`), and because the abort also skips the
//! `recGblResetAlarms` inside `monitor`, that alarm stays PENDING in nsta/nsev
//! this cycle — it is not committed to STAT/SEVR.
//!
//! The port discarded the pre-input read's outcome entirely, so a dead DOL let
//! every cycle write the last good value to OUT and fire the FLNK, with no
//! alarm at all.
//!
//! Boundaries: dead DOL vs live DOL; OUT target written / not written; FLNK
//! fired / not fired; pending alarm vs committed alarm.

// RTEMS-EXEC-MODEL-ALLOW(4): checked - these run and pass in the feature-ON suite.

use std::collections::HashSet;

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "GOOD:SRC") {
    field(VAL, "2.5")
}
record(waveform, "FLNK:SRC") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
}
record(waveform, "DEAD:OUT") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
}
record(waveform, "DEAD:FLNK") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
    field(INP, "FLNK:SRC")
}
record(aao, "AAO:DEAD") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
    field(OMSL, "closed_loop")
    field(DOL, "NO:SUCH:RECORD")
    field(OUT, "DEAD:OUT")
    field(FLNK, "DEAD:FLNK")
}
record(waveform, "LIVE:OUT") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
}
record(waveform, "LIVE:FLNK") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
    field(INP, "FLNK:SRC")
}
record(aao, "AAO:LIVE") {
    field(FTVL, "DOUBLE")
    field(NELM, "8")
    field(OMSL, "closed_loop")
    field(DOL, "GOOD:SRC")
    field(OUT, "LIVE:OUT")
    field(FLNK, "LIVE:FLNK")
}
"#;

async fn build() -> std::sync::Arc<epics_base_rs::server::database::PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &epics_base_rs::server::database::PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

/// The whole cycle is abandoned: no OUT write, no forward link.
#[tokio::test]
async fn dead_dol_writes_nothing_out_and_does_not_fire_flnk() {
    let db = build().await;
    // A value a client left in the aao — the stale VAL C refuses to write out.
    db.put_pv("AAO:DEAD", EpicsValue::DoubleArray(vec![5.0, 6.0]))
        .await
        .unwrap();
    // A sentinel the FLNK target would pick up through its own INP if it ran.
    db.put_pv("FLNK:SRC", EpicsValue::DoubleArray(vec![42.0]))
        .await
        .unwrap();

    process(&db, "AAO:DEAD").await;

    assert_eq!(
        db.get_pv("DEAD:OUT").unwrap(),
        EpicsValue::DoubleArray(vec![]),
        "C returns BEFORE writeValue: a stale VAL must not reach the OUT target"
    );
    assert_eq!(
        db.get_pv("DEAD:FLNK").unwrap(),
        EpicsValue::DoubleArray(vec![]),
        "C returns BEFORE recGblFwdLink: the forward link must not fire"
    );
}

/// `dbGetLink` raised LINK/INVALID, but the abort skipped `monitor` — so
/// `recGblResetAlarms` never ran and the alarm is still PENDING (nsta/nsev),
/// not committed to STAT/SEVR.
#[tokio::test]
async fn dead_dol_raises_a_pending_link_alarm() {
    let db = build().await;
    process(&db, "AAO:DEAD").await;

    let rec = db.get_record("AAO:DEAD").unwrap();
    let common = &rec.read().common;
    assert_eq!(
        common.nsev,
        AlarmSeverity::Invalid,
        "C `setLinkAlarm`: recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM, ...)"
    );
    assert_eq!(
        common.nsta,
        epics_base_rs::server::recgbl::alarm_status::LINK_ALARM
    );
    assert_eq!(common.namsg, "field DOL", "C AMSG is the link's field name");
    assert_eq!(
        common.sevr,
        AlarmSeverity::Invalid,
        "the abort skips monitor(), so recGblResetAlarms never commits the LINK \
         alarm this cycle — SEVR still carries the INIT-time UDF severity \
         (iocInit.c:521-523), untouched by this process"
    );
    assert_eq!(
        common.stat,
        epics_base_rs::server::recgbl::alarm_status::UDF_ALARM,
        "and STAT is still the born UDF, not the pending LINK"
    );
}

/// The contrast case: a live DOL fetch runs the full cycle — VAL from DOL, OUT
/// written, FLNK fired.
#[tokio::test]
async fn live_dol_completes_the_cycle() {
    let db = build().await;
    db.put_pv("FLNK:SRC", EpicsValue::DoubleArray(vec![42.0]))
        .await
        .unwrap();

    process(&db, "AAO:LIVE").await;

    assert_eq!(
        db.get_pv("AAO:LIVE").unwrap(),
        EpicsValue::DoubleArray(vec![2.5]),
        "the DOL value lands in VAL (one element, NORD=1)"
    );
    assert_eq!(
        db.get_pv("LIVE:OUT").unwrap(),
        EpicsValue::DoubleArray(vec![2.5]),
        "writeValue runs"
    );
    assert_eq!(
        db.get_pv("LIVE:FLNK").unwrap(),
        EpicsValue::DoubleArray(vec![42.0]),
        "recGblFwdLink runs — the FLNK target processed and pulled its own INP"
    );
}

/// The abort is per-cycle state, not sticky: once the DOL source exists, the
/// next cycle completes normally.
#[tokio::test]
async fn cycle_resumes_when_the_dol_source_returns() {
    let db = build().await;
    process(&db, "AAO:DEAD").await;
    assert_eq!(
        db.get_pv("DEAD:OUT").unwrap(),
        EpicsValue::DoubleArray(vec![])
    );

    // The DOL target appears (a soft IOC restarting the source record).
    db.add_pv("NO:SUCH:RECORD", EpicsValue::Double(3.0))
        .await
        .unwrap();
    process(&db, "AAO:DEAD").await;

    assert_eq!(
        db.get_pv("AAO:DEAD").unwrap(),
        EpicsValue::DoubleArray(vec![3.0])
    );
    assert_eq!(
        db.get_pv("DEAD:OUT").unwrap(),
        EpicsValue::DoubleArray(vec![3.0]),
        "the recovered fetch runs the full cycle again"
    );
}
