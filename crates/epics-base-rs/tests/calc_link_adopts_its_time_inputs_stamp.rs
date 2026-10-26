//! A `lnkCalc` input link names one of its inputs as the time source —
//! `{calc:{"expr":"A+B","args":[{pva:"X"},{pva:"Y"}],"time":"B"}}` — and the record
//! reading that link takes BOTH halves of that input's stamp.
//!
//! C does it inside the link read, not in the dset: `lnkCalc_getValue`
//! (`lnkCalc.c:571-582`) reads the `tinp` child through `readLocked`, which
//! calls `dbGetTimeStampTag` and so fills `clink->time` AND `clink->utag`,
//! and then
//!
//! ```c
//! if (dbLinkIsConstant(&prec->tsel) &&
//!     prec->tse == epicsTimeEventDeviceTime) {
//!     prec->time = clink->time;
//!     prec->utag = clink->utag;
//! }
//! ```
//!
//! That is the SAME gate `devAiSoft.c:73-74` uses to decide whether the dset
//! fills `prec->time` at all, which is why the two never fight: on a soft
//! channel with a calc INP both fire and agree, and `lnkCalc_getTimestampTag`
//! (`lnkCalc.c:749-762`) serves the cached pair to anyone who asks the link
//! later.
//!
//! The utag is the part that separates this from every other input link. A
//! plain DB/CA source reaches the record through `dbGetTimeStamp`, the
//! `ptag == NULL` spelling of `dbGetTimeStampTag` (`dbLink.c:413-416`), so
//! its UTAG is deliberately dropped — see the sibling file
//! `soft_input_adopts_the_source_timestamp.rs`. A calc source is the one
//! class whose tag arrives, because the copy at `lnkCalc.c:581` is the link's
//! own and passes no NULL.
//!
//! The same copy runs on a put (`lnkCalc.c:651-656`), and for any reader,
//! not only a soft input. `clink->time`/`utag` change only on a good read of
//! the `time` input (`readLocked`, `:539-549`), so a failed read, a literal,
//! or a letter past the args leaves what was there, `{0,0}` at first.
//!
//! Asserted here at each boundary of the rule: which link class, which
//! letter, which gate value, read vs put, and a good vs a failed read.

use epics_base_rs::runtime::general_time::epics_epoch;
use epics_base_rs::server::database::{LinkRef, LinkSet};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_base_rs::types::WallTime;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Stamps no clock in this process will produce, so an assertion that finds
/// one can only have got it from the record it was planted on.
const A_STAMP: Duration = Duration::new(1_000_000, 111_111_111);
const B_STAMP: Duration = Duration::new(2_000_000, 222_222_222);
const A_UTAG: u64 = 0xA1A1_A1A1;
const B_UTAG: u64 = 0xB2B2_B2B2;

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

/// The `{pva:…}` inputs: `SRC_A` is 3 and `SRC_B` is 5, each with its own
/// stamp, until `down` fails every read.
struct Sources {
    down: AtomicBool,
}

impl Sources {
    fn source(&self, link: LinkRef<'_>) -> Option<(f64, Duration, u64)> {
        if self.down.load(Ordering::Relaxed) {
            return None;
        }
        match link.name {
            b"SRC_A" => Some((3.0, A_STAMP, A_UTAG)),
            b"SRC_B" => Some((5.0, B_STAMP, B_UTAG)),
            _ => None,
        }
    }
}

#[epics_base_rs::async_trait]
impl LinkSet for Sources {
    fn is_connected(&self, link: LinkRef<'_>) -> bool {
        self.source(link).is_some()
    }
    fn get_cached_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        self.source(link).map(|(v, ..)| EpicsValue::Double(v))
    }
    async fn get_value(&self, link: LinkRef<'_>) -> Option<EpicsValue> {
        self.get_cached_value(link)
    }
    fn time_stamp(&self, link: LinkRef<'_>) -> Option<(i64, i32, u64)> {
        self.source(link)
            .map(|(_, t, tag)| (t.as_secs() as i64, t.subsec_nanos() as i32, tag))
    }
}

async fn build(db_text: &str) -> (Db, Arc<Sources>) {
    let db = IocBuilder::new()
        .register_link_type("pva", |_db| async { Vec::new() })
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let sources = Arc::new(Sources {
        down: AtomicBool::new(false),
    });
    db.register_link_set("pva", sources.clone()).await;
    (db, sources)
}

async fn proc(db: &Db, rec: &str) {
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

fn stamp_of(db: &Db, rec: &str) -> (WallTime, u64) {
    let r = db.get_record(rec).unwrap();
    let inst = r.read();
    (inst.common.time, inst.common.utag)
}

/// The rule itself: `time:"A"` on a TSE=-2 soft input hands the record the
/// named input's time AND its userTag. Pre-fix the owner's `_ => None` arm
/// swallowed every calc link, so the record kept the `general_time` seed and
/// a zero tag while its VAL was the freshly computed 3.0.
#[epics_macros_rs::epics_test]
async fn a_calc_inps_time_letter_supplies_both_halves_of_the_stamp() {
    let (db, _) = build(
        r#"
record(ai, "DST") {
    field(INP, "{calc:{\"expr\":\"A\",\"args\":[{\"pva\":\"SRC_A\"}],\"time\":\"A\"}}")
    field(TSE, "-2")
}
"#,
    )
    .await;
    proc(&db, "DST").await;

    assert_eq!(
        db.get_record("DST").unwrap().read().record.get_field("VAL"),
        Some(EpicsValue::Double(3.0)),
        "the calc itself must still evaluate"
    );
    assert_eq!(
        stamp_of(&db, "DST"),
        (WallTime::UNIX_EPOCH + A_STAMP, A_UTAG),
        "C `lnkCalc.c:580-581` copies clink->time AND clink->utag into the record"
    );
}

/// The letter indexes `args`, so `time:"B"` must take the SECOND input's
/// stamp — the boundary that separates "reads the time source" from "reads
/// the first input and happens to be right".
#[epics_macros_rs::epics_test]
async fn the_time_letter_selects_which_input_supplies_the_stamp() {
    let (db, _) = build(
        r#"
record(ai, "DST") {
    field(INP, "{calc:{\"expr\":\"A+B\",\"args\":[{\"pva\":\"SRC_A\"},{\"pva\":\"SRC_B\"}],\"time\":\"B\"}}")
    field(TSE, "-2")
}
"#,
    )
    .await;
    proc(&db, "DST").await;

    assert_eq!(
        db.get_record("DST").unwrap().read().record.get_field("VAL"),
        Some(EpicsValue::Double(8.0)),
    );
    assert_eq!(
        stamp_of(&db, "DST"),
        (WallTime::UNIX_EPOCH + B_STAMP, B_UTAG),
        "`time:\"B\"` is args[1], C `clink->tinp = tinp - 'A'` (lnkCalc.c:184)"
    );
}

/// C upper-cases the letter before range-testing it —
/// `tinp = toupper((int) val[0])`, `lnkCalc.c:179` — so `time:"a"` is input
/// A, not "no time source". Pre-fix the parser's `('A'..='L')` filter
/// silently turned a lower-case letter into `time_source: None`, which is
/// indistinguishable at the record from having written no `time` key at all.
#[epics_macros_rs::epics_test]
async fn a_lower_case_time_letter_names_the_same_input_as_upper_case() {
    let (db, _) = build(
        r#"
record(ai, "DST") {
    field(INP, "{calc:{\"expr\":\"A\",\"args\":[{\"pva\":\"SRC_A\"}],\"time\":\"a\"}}")
    field(TSE, "-2")
}
"#,
    )
    .await;
    proc(&db, "DST").await;

    assert_eq!(
        stamp_of(&db, "DST"),
        (WallTime::UNIX_EPOCH + A_STAMP, A_UTAG),
        "C toupper()s the `time` letter before testing its range"
    );
}

/// No `time` key is C's `clink->tinp = -1` (`lnkCalc.c:96`), and
/// `lnkCalc_getTimestampTag` then returns -1 (`:761`) — the record keeps
/// whatever `apply_timestamp` left it. The lower boundary of the rule above:
/// without it, "adopt the source stamp" could be unconditional and every
/// case here but this one would still pass.
#[epics_macros_rs::epics_test]
async fn a_calc_inp_without_a_time_key_supplies_no_stamp() {
    let (db, _) = build(
        r#"
record(ai, "DST") {
    field(INP, "{calc:{\"expr\":\"A\",\"args\":[{\"pva\":\"SRC_A\"}]}}")
    field(TSE, "-2")
}
"#,
    )
    .await;
    proc(&db, "DST").await;

    let (time, utag) = stamp_of(&db, "DST");
    assert_ne!(
        time,
        WallTime::UNIX_EPOCH + A_STAMP,
        "no `time` key → C's tinp = -1 → the record keeps its own stamp"
    );
    assert_eq!(utag, 0, "and no userTag arrives either");
}

/// The gate: TSE must be `epicsTimeEventDeviceTime` (-2). C tests it inside
/// the link read (`lnkCalc.c:578-579`) exactly as the dset tests it
/// (`devAiSoft.c:73-74`), so a record on the default TSE=0 gets its own
/// cycle time no matter what the link says. Boundary partner of the first
/// case, which is the same database with TSE=-2.
#[epics_macros_rs::epics_test]
async fn a_calc_inp_on_the_default_tse_supplies_no_stamp() {
    let (db, _) = build(
        r#"
record(ai, "DST") {
    field(INP, "{calc:{\"expr\":\"A\",\"args\":[{\"pva\":\"SRC_A\"}],\"time\":\"A\"}}")
}
"#,
    )
    .await;
    proc(&db, "DST").await;

    let (time, utag) = stamp_of(&db, "DST");
    assert_ne!(
        time,
        WallTime::UNIX_EPOCH + A_STAMP,
        "TSE=0 → `dbLinkIsConstant(&tsel) && tse == -2` is false → no adoption"
    );
    assert_eq!(utag, 0);
}

async fn dbpf(db: &Db, rec: &str, value: EpicsValue) {
    db.put_pv(rec, value).await.unwrap();
    db.process_record_with_links(rec, &mut epics_base_rs::server::database::ProcStack::new())
        .await
        .or_else(epics_base_rs::error::CaError::unless_process_status)
        .unwrap();
}

/// What softIoc (8f5015b66) printed for `dbpr` on these records, each
/// stamped once on `TSE` 0 and run again on `TSE` -2. C has no child link
/// that carries a time, so the stamp a `time` input leaves is `{0,0}`; one
/// past the args is never read, so only a soft input's dset, asking the link
/// for its time, takes it.
#[epics_macros_rs::epics_test]
async fn a_device_time_reader_takes_the_stamp_the_link_holds() {
    let (db, _) = build(
        r#"
record(ao, "OUT1") { field(OUT, "{calc:{expr:'A', time:'A', out:{state:'t1'}, args:[5]}}") }
record(ao, "OUT2") { field(OUT, "{calc:{expr:'A', out:{state:'t2'}, args:[5]}}") }
record(ao, "OUT4") { field(OUT, "{calc:{expr:'A', time:'B', out:{state:'t4'}, args:[5]}}") }
record(calc, "C1") { field(INPA, "{calc:{expr:'A', time:'A', args:[5]}}") field(CALC, "A") }
record(calc, "C4") { field(INPA, "{calc:{expr:'A', time:'B', args:[5]}}") field(CALC, "A") }
record(ai, "AI2") { field(INP, "{calc:{expr:'A+1', time:'A', args:[5]}}") }
record(ai, "AI4") { field(INP, "{calc:{expr:'A+1', time:'B', args:[5]}}") }
"#,
    )
    .await;
    let outs = ["OUT1", "OUT2", "OUT4"];
    let ins = ["C1", "C4", "AI2", "AI4"];
    for rec in outs {
        dbpf(&db, rec, EpicsValue::Double(1.0)).await;
    }
    for rec in ins {
        proc(&db, rec).await;
    }
    let first: Vec<_> = outs
        .iter()
        .chain(&ins)
        .map(|r| stamp_of(&db, r).0)
        .collect();
    assert!(first.iter().all(|t| *t != epics_epoch()));
    for rec in outs.iter().chain(&ins) {
        db.put_pv(&format!("{rec}.TSE"), EpicsValue::Short(-2))
            .await
            .unwrap();
    }
    for rec in outs {
        dbpf(&db, rec, EpicsValue::Double(2.0)).await;
    }
    for rec in ins {
        proc(&db, rec).await;
    }
    let undefined = epics_epoch();
    let now: Vec<_> = outs
        .iter()
        .chain(&ins)
        .map(|r| stamp_of(&db, r).0)
        .collect();
    assert_eq!(
        now,
        [
            undefined, first[1], first[2], undefined, first[4], undefined, undefined
        ]
    );
}

const READERS: &str = r#"
record(ao, "OUTS") { field(TSE, "-2") field(OUT, "{calc:{expr:'A', time:'A', out:{state:'t3'}, args:[{pva:'SRC_A'}]}}") }
record(calc, "CS") { field(TSE, "-2") field(INPA, "{calc:{expr:'A', time:'A', args:[{pva:'SRC_A'}]}}") field(CALC, "A") }
record(ai, "AIS") { field(TSE, "-2") field(INP, "{calc:{expr:'A', time:'A', args:[{pva:'SRC_B'}]}}") }
record(calc, "CN") { field(TSE, "-2") field(INPA, "{calc:{expr:'A+B', time:'A', args:[{pva:'SRC_A'}, {calc:{expr:'2', time:'A', args:[{pva:'SRC_B'}]}}]}}") field(CALC, "A") }
record(ai, "AIN") { field(TSE, "-2") field(INP, "{calc:{expr:'A+B', time:'A', args:[{pva:'SRC_A'}, {calc:{expr:'2', time:'A', args:[{pva:'SRC_B'}]}}]}}") }
"#;

#[epics_macros_rs::epics_test]
async fn the_time_input_stamps_a_writer_and_any_reader() {
    let (db, _) = build(READERS).await;
    dbpf(&db, "OUTS", EpicsValue::Double(1.0)).await;
    assert_eq!(
        stamp_of(&db, "OUTS"),
        (WallTime::UNIX_EPOCH + A_STAMP, A_UTAG)
    );
    proc(&db, "CS").await;
    assert_eq!(
        stamp_of(&db, "CS"),
        (WallTime::UNIX_EPOCH + A_STAMP, A_UTAG)
    );
}

/// The inner link copies after the outer one, so a record takes the inner
/// stamp whole; a soft input's dset then puts back the outer link's time
/// and leaves the inner tag.
#[epics_macros_rs::epics_test]
async fn a_nested_time_input_copies_last() {
    let (db, _) = build(READERS).await;
    proc(&db, "CN").await;
    assert_eq!(
        stamp_of(&db, "CN"),
        (WallTime::UNIX_EPOCH + B_STAMP, B_UTAG)
    );
    proc(&db, "AIN").await;
    assert_eq!(
        stamp_of(&db, "AIN"),
        (WallTime::UNIX_EPOCH + A_STAMP, B_UTAG)
    );
}

/// A failed read of the `time` input leaves the stamp of the last good one.
#[epics_macros_rs::epics_test]
async fn a_failed_time_input_keeps_the_last_good_stamp() {
    let (db, sources) = build(READERS).await;
    proc(&db, "AIS").await;
    let good = (WallTime::UNIX_EPOCH + B_STAMP, B_UTAG);
    assert_eq!(stamp_of(&db, "AIS"), good);
    sources.down.store(true, Ordering::Relaxed);
    proc(&db, "AIS").await;
    assert_eq!(stamp_of(&db, "AIS"), good);
}
