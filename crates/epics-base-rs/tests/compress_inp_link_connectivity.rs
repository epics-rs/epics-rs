//! R18-106: compress samples INP only when INP is a CONNECTED link; every
//! other cycle raises LINK/INVALID and ingests nothing.
//!
//! `compressRecord.c:327-343`:
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
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
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
        EpicsValue::DoubleArray(vec![].into()),
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
        EpicsValue::DoubleArray(vec![7.0, 7.0].into())
    );
}

/// INP has ONE source. `COMPRESS_FIELDS` declares no INP (nor does
/// `compressRecord.dbd.pod`), so the link is a dbCommon field and `.INP` reads
/// what softIoc reads: `dbgf C:LINK.INP` → `"SRC NPP NMS"`, C `dbGetString`'s
/// rendering of the parsed link rather than the text the `.db` carried. The
/// record used to
/// carry a private `inp` field that the loader never wrote and that shadowed
/// the common one — the port answered `""`.
#[epics_macros_rs::epics_test]
async fn inp_reads_back_the_link_text() {
    let db = build().await;

    assert_eq!(
        db.get_pv("C:LINK.INP").unwrap(),
        EpicsValue::String("SRC NPP NMS".into())
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

/// The INP read lands as C's `dbGetLink(&prec->inp, DBF_DOUBLE, wptr, ...)`
/// whatever the source carries. softIoc, one `dbpf .PROC 1` each (XS twice):
///
/// ```text
/// X   (DOUBLE[16] 1..16, N to 1 Average, N=2) : 1.5 3.5 ... 15.5  NUSE=8 UDF=0 NO_ALARM
/// XL  (LONG[4] 2 4 6 9, N to 1 Average, N=2)  : 3 7.5
/// XS  (calc 7.5, Circular Buffer, NSAM=3)     : 7.5 7.5  NUSE=2
/// XIF (DOUBLE[16], N to 1 High, N=4, LIFO)    : 16 12 8 4  NO_ALARM
/// ```
#[epics_macros_rs::epics_test]
async fn inp_ingest_matches_softioc_for_each_source_carrier() {
    const DB: &str = r#"
record(waveform, "ARR") { field(NELM, "16") field(FTVL, "DOUBLE") field(INP, [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16]) }
record(waveform, "LARR") { field(NELM, "4") field(FTVL, "LONG") field(INP, [2,4,6,9]) }
record(calc, "SC") { field(CALC, "7.5") }
record(compress, "X") { field(INP, "ARR") field(ALG, "N to 1 Average") field(NSAM, "8") field(N, "2") }
record(compress, "XL") { field(INP, "LARR") field(ALG, "N to 1 Average") field(NSAM, "4") field(N, "2") }
record(compress, "XS") { field(INP, "SC") field(ALG, "Circular Buffer") field(NSAM, "3") field(N, "1") }
record(compress, "XIF") { field(INP, "ARR") field(ALG, "N to 1 High Value") field(NSAM, "4") field(N, "4") field(BALG, "LIFO Buffer") }
"#;
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for rec in ["SC", "X", "XL", "XS", "XS", "XIF"] {
        process(&db, rec).await;
    }
    let val = |rec: &str| match db.get_pv(rec).unwrap() {
        EpicsValue::DoubleArray(v) => v.to_vec(),
        other => panic!("{rec}.VAL must be a DoubleArray, got {other:?}"),
    };

    assert_eq!(val("X"), vec![1.5, 3.5, 5.5, 7.5, 9.5, 11.5, 13.5, 15.5]);
    assert_eq!(nuse(&db, "X").await, 8.0);
    assert_eq!(db.get_record("X").unwrap().read().common.udf, 0);
    assert_eq!(alarm(&db, "X").await.0, AlarmSeverity::NoAlarm);
    assert_eq!(val("XL"), vec![3.0, 7.5]);
    assert_eq!(val("XS"), vec![7.5, 7.5]);
    assert_eq!(nuse(&db, "XS").await, 2.0);
    assert_eq!(val("XIF"), vec![16.0, 12.0, 8.0, 4.0]);
    assert_eq!(alarm(&db, "XIF").await.0, AlarmSeverity::NoAlarm);
}
