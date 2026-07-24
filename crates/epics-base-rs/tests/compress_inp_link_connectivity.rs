//! R18-106: compress samples INP only when INP is a CONNECTED link; every
//! other cycle raises LINK/INVALID and ingests nothing.
//!
//! `compressRecord.c:326-343`:
//!
//! ```c
//! prec->pact = TRUE;
//! if (!dbIsLinkConnected(&prec->inp) ||
//!     dbGetNelements(&prec->inp, &nelements) ||
//!     nelements <= 0) {
//!     recGblSetSevr(prec, LINK_ALARM, INVALID_ALARM);
//! }
//! else {
//!     ... realloc wptr, dbGetLink, put_value/compress_array ...
//!     if (status || nelements <= 0) {
//!         recGblSetSevr(prec, LINK_ALARM, INVALID_ALARM);
//!         status = 0;
//!     }
//! }
//! ```
//!
//! `dbIsLinkConnected` reads the link's `lset` and returns FALSE when there is
//! none — an unset link and a CONSTANT link both have none. So C treats
//! `field(INP,"5")` exactly like `field(INP,"")`: no sample, LINK/INVALID.
//!
//! The port had no gate at all: `pre_process_actions` emitted the INP read
//! whenever the string was non-empty, and a constant link happily delivers its
//! literal on every read. Two divergences from the one missing gate — a dead
//! INP never alarmed, and a constant INP fabricated a sample per cycle.
//!
//! softIoc (`bin/linux-x86_64`), NSAM=4 ALG="Circular Buffer", `dbpf .PROC 1`:
//!
//! ```text
//! INP unset : SEVR=INVALID STAT=LINK   NUSE=0   VAL=(empty)
//! INP="5"   : SEVR=INVALID STAT=LINK   NUSE=0   VAL=(empty)
//! INP="SRC" : SEVR=NO_ALARM STAT=NO_ALARM  NUSE=2  VAL=7 7   (two processes)
//! ```

use std::collections::HashSet;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ai, "SRC") { field(VAL, "7") }
record(compress, "C:NOLINK") { field(NSAM, "4") field(ALG, "Circular Buffer") }
record(compress, "C:CONST")  { field(NSAM, "4") field(ALG, "Circular Buffer") field(INP, "5") }
record(compress, "C:LINK")   { field(NSAM, "4") field(ALG, "Circular Buffer") field(INP, "SRC") }
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

async fn alarm(db: &PvDatabase, rec: &str) -> (AlarmSeverity, u16) {
    let inst = db.get_record(rec).unwrap();
    let c = &inst.read().common;
    (c.sevr, c.stat)
}

async fn nuse(db: &PvDatabase, rec: &str) -> f64 {
    db.get_pv(&format!("{rec}.NUSE")).unwrap().to_f64().unwrap()
}

/// An unset INP is not a connected link: LINK/INVALID, nothing ingested.
#[epics_macros_rs::epics_test]
async fn unset_inp_raises_link_invalid_and_ingests_nothing() {
    let db = build().await;

    process(&db, "C:NOLINK").await;

    let (sevr, stat) = alarm(&db, "C:NOLINK").await;
    assert_eq!(sevr, AlarmSeverity::Invalid, "C: SEVR=INVALID");
    assert_eq!(stat, alarm_status::LINK_ALARM, "C: STAT=LINK");
    assert_eq!(nuse(&db, "C:NOLINK").await, 0.0, "no sample was taken");
}

/// The fabricated-data half: a CONSTANT INP has no lset, so C never samples it.
/// The port ingested the literal on every process — NUSE climbed and the
/// buffer filled with a repeated constant that no source ever produced.
#[epics_macros_rs::epics_test]
async fn constant_inp_is_not_connected_and_is_never_ingested() {
    let db = build().await;
    process(&db, "C:CONST").await;
    process(&db, "C:CONST").await;
    process(&db, "C:CONST").await;

    let (sevr, stat) = alarm(&db, "C:CONST").await;
    assert_eq!(
        sevr,
        AlarmSeverity::Invalid,
        "C: field(INP,\"5\") is NOT connected — SEVR=INVALID"
    );
    assert_eq!(stat, alarm_status::LINK_ALARM, "C: STAT=LINK");
    assert_eq!(
        nuse(&db, "C:CONST").await,
        0.0,
        "three processes on a constant INP must ingest nothing"
    );
    assert_eq!(
        db.get_pv("C:CONST").unwrap(),
        EpicsValue::DoubleArray(vec![]),
        "C: VAL is DBF_DOUBLE[0] (empty)"
    );
}

/// The gate must not fire on a real link — that is the whole record's job.
#[epics_macros_rs::epics_test]
async fn a_connected_db_link_ingests_and_stays_no_alarm() {
    let db = build().await;

    process(&db, "C:LINK").await;
    process(&db, "C:LINK").await;

    let (sevr, _stat) = alarm(&db, "C:LINK").await;
    assert_eq!(
        sevr,
        AlarmSeverity::NoAlarm,
        "a connected INP does not alarm"
    );
    assert_eq!(nuse(&db, "C:LINK").await, 2.0);
    assert_eq!(
        db.get_pv("C:LINK").unwrap(),
        EpicsValue::DoubleArray(vec![7.0, 7.0])
    );
}

/// INP has ONE source. `COMPRESS_FIELDS` declares no INP (nor does
/// `compressRecord.dbd.pod`), so the link is a dbCommon field and `.INP` reads
/// the link text softIoc reads: `dbgf C:LINK.INP` → `"SRC"`. The record used to
/// carry a private `inp` field that the loader never wrote and that shadowed
/// the common one — the port answered `""`.
#[epics_macros_rs::epics_test]
async fn inp_reads_back_the_link_text() {
    let db = build().await;

    assert_eq!(
        db.get_pv("C:LINK.INP").unwrap(),
        EpicsValue::String("SRC".into())
    );
    assert_eq!(
        db.get_pv("C:CONST.INP").unwrap(),
        EpicsValue::String("5".into())
    );
}

/// The alarm is a per-cycle fact, not a latch: a link that starts delivering
/// clears it on the next process (C re-derives it every `process()` from
/// `nsta`/`nsev`, which `recGblResetAlarms` clears each cycle), and a record
/// whose link goes away raises it again.
#[epics_macros_rs::epics_test]
async fn the_link_alarm_is_re_derived_every_cycle() {
    let db = build().await;

    process(&db, "C:LINK").await;
    assert_eq!(alarm(&db, "C:LINK").await.0, AlarmSeverity::NoAlarm);

    // Take the link away — C's `dbIsLinkConnected` would now be FALSE.
    db.put_record_field_from_ca("C:LINK", "INP", EpicsValue::String("".into()))
        .await
        .unwrap();
    process(&db, "C:LINK").await;
    assert_eq!(
        alarm(&db, "C:LINK").await.0,
        AlarmSeverity::Invalid,
        "a link that disappears raises LINK/INVALID on the next cycle"
    );
    assert_eq!(
        nuse(&db, "C:LINK").await,
        1.0,
        "and the buffer keeps what it already had — the cycle ingests nothing"
    );
}
