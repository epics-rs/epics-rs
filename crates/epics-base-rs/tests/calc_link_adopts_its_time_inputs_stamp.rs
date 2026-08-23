//! A `lnkCalc` input link names one of its inputs as the time source —
//! `{calc:{"expr":"A+B","args":["X","Y"],"time":"B"}}` — and the record
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
//! (`:749-762`) serves the cached pair to anyone who asks the link later.
//!
//! The utag is the part that separates this from every other input link. A
//! plain DB/CA source reaches the record through `dbGetTimeStamp`, the
//! `ptag == NULL` spelling of `dbGetTimeStampTag` (`dbLink.c:415-418`), so
//! its UTAG is deliberately dropped — see the sibling file
//! `soft_input_adopts_the_source_timestamp.rs`. A calc source is the one
//! class whose tag arrives, because the copy at `lnkCalc.c:581` is the link's
//! own and passes no NULL.
//!
//! Asserted here at each boundary of the rule: which link class, which
//! letter, which gate value, and local vs non-local source.

use epics_base_rs::server::database::LinkSet;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Stamps no clock in this process will produce, so an assertion that finds
/// one can only have got it from the record it was planted on.
const A_STAMP: Duration = Duration::new(1_000_000, 111_111_111);
const B_STAMP: Duration = Duration::new(2_000_000, 222_222_222);
const A_UTAG: u64 = 0xA1A1_A1A1;
const B_UTAG: u64 = 0xB2B2_B2B2;

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

async fn build(db_text: &str) -> Db {
    IocBuilder::new()
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// SRC_A / SRC_B are never processed in these tests, so a hand-planted stamp
/// stays put and any record carrying it took it from that source.
fn stamp_sources(db: &Db) {
    for (name, t, tag) in [("SRC_A", A_STAMP, A_UTAG), ("SRC_B", B_STAMP, B_UTAG)] {
        if let Some(rec) = db.get_record(name) {
            let mut inst = rec.write();
            inst.common.time = SystemTime::UNIX_EPOCH + t;
            inst.common.utag = tag;
        }
    }
}

async fn proc(db: &Db, rec: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(rec, &mut visited, 0)
        .await
        .unwrap();
}

fn stamp_of(db: &Db, rec: &str) -> (SystemTime, u64) {
    let r = db.get_record(rec).unwrap();
    let inst = r.read();
    (inst.common.time, inst.common.utag)
}

/// The rule itself: `time:"A"` on a TSE=-2 soft input hands the record the
/// named input's time AND its userTag. Pre-fix the owner's `_ => None` arm
/// swallowed every calc link, so the record kept the `general_time` seed and
/// a zero tag while its VAL was the freshly computed 7.0.
#[epics_macros_rs::epics_test]
async fn a_calc_inps_time_letter_supplies_both_halves_of_the_stamp() {
    let db = build(
        r#"
record(ai, "SRC_A") { field(VAL, "7") }
record(ai, "DST") {
    field(INP, "{calc:{\"expr\":\"A\",\"args\":[\"SRC_A\"],\"time\":\"A\"}}")
    field(TSE, "-2")
}
"#,
    )
    .await;
    stamp_sources(&db);
    proc(&db, "DST").await;

    assert_eq!(
        db.get_record("DST").unwrap().read().record.get_field("VAL"),
        Some(EpicsValue::Double(7.0)),
        "the calc itself must still evaluate"
    );
    assert_eq!(
        stamp_of(&db, "DST"),
        (SystemTime::UNIX_EPOCH + A_STAMP, A_UTAG),
        "C `lnkCalc.c:580-581` copies clink->time AND clink->utag into the record"
    );
}

/// The letter indexes `args`, so `time:"B"` must take the SECOND input's
/// stamp — the boundary that separates "reads the time source" from "reads
/// the first input and happens to be right".
#[epics_macros_rs::epics_test]
async fn the_time_letter_selects_which_input_supplies_the_stamp() {
    let db = build(
        r#"
record(ai, "SRC_A") { field(VAL, "3") }
record(ai, "SRC_B") { field(VAL, "5") }
record(ai, "DST") {
    field(INP, "{calc:{\"expr\":\"A+B\",\"args\":[\"SRC_A\",\"SRC_B\"],\"time\":\"B\"}}")
    field(TSE, "-2")
}
"#,
    )
    .await;
    stamp_sources(&db);
    proc(&db, "DST").await;

    assert_eq!(
        db.get_record("DST").unwrap().read().record.get_field("VAL"),
        Some(EpicsValue::Double(8.0)),
    );
    assert_eq!(
        stamp_of(&db, "DST"),
        (SystemTime::UNIX_EPOCH + B_STAMP, B_UTAG),
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
    let db = build(
        r#"
record(ai, "SRC_A") { field(VAL, "7") }
record(ai, "DST") {
    field(INP, "{calc:{\"expr\":\"A\",\"args\":[\"SRC_A\"],\"time\":\"a\"}}")
    field(TSE, "-2")
}
"#,
    )
    .await;
    stamp_sources(&db);
    proc(&db, "DST").await;

    assert_eq!(
        stamp_of(&db, "DST"),
        (SystemTime::UNIX_EPOCH + A_STAMP, A_UTAG),
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
    let db = build(
        r#"
record(ai, "SRC_A") { field(VAL, "7") }
record(ai, "DST") {
    field(INP, "{calc:{\"expr\":\"A\",\"args\":[\"SRC_A\"]}}")
    field(TSE, "-2")
}
"#,
    )
    .await;
    stamp_sources(&db);
    proc(&db, "DST").await;

    let (time, utag) = stamp_of(&db, "DST");
    assert_ne!(
        time,
        SystemTime::UNIX_EPOCH + A_STAMP,
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
    let db = build(
        r#"
record(ai, "SRC_A") { field(VAL, "7") }
record(ai, "DST") {
    field(INP, "{calc:{\"expr\":\"A\",\"args\":[\"SRC_A\"],\"time\":\"A\"}}")
}
"#,
    )
    .await;
    stamp_sources(&db);
    proc(&db, "DST").await;

    let (time, utag) = stamp_of(&db, "DST");
    assert_ne!(
        time,
        SystemTime::UNIX_EPOCH + A_STAMP,
        "TSE=0 → `dbLinkIsConstant(&tsel) && tse == -2` is false → no adoption"
    );
    assert_eq!(utag, 0);
}

/// Each `A..` input is its own `dbInitLink` link (`lnkCalc.c:353`), so an
/// input naming a record this IOC does not hold is a CA link and its stamp
/// comes from the CA lset. Same locality rule as a plain DB link resolving to
/// CA, and the reason `db_get_time_stamp_tag`'s calc arm recurses into
/// `record_time_stamp_tag` instead of re-deriving locality itself: a second
/// derivation of the same rule is what drifts, and this one is the boundary
/// where the drift would be invisible.
#[epics_macros_rs::epics_test]
async fn a_non_local_time_input_supplies_the_remote_stamp() {
    struct RemoteLset;
    #[epics_base_rs::async_trait]
    impl LinkSet for RemoteLset {
        fn is_connected(&self, _: &str) -> bool {
            true
        }
        fn get_cached_value(&self, name: &str) -> Option<EpicsValue> {
            (name == "REMOTE:A").then_some(EpicsValue::Double(10.0))
        }
        async fn get_value(&self, name: &str) -> Option<EpicsValue> {
            self.get_cached_value(name)
        }
        fn time_stamp(&self, name: &str) -> Option<(i64, i32, u64)> {
            (name == "REMOTE:A").then_some((1_700_000_456, 321, A_UTAG))
        }
    }

    let db = build(
        r#"
record(ai, "DST") {
    field(INP, "{calc:{\"expr\":\"A\",\"args\":[\"REMOTE:A\"],\"time\":\"A\"}}")
    field(TSE, "-2")
}
"#,
    )
    .await;
    db.register_link_set("ca", Arc::new(RemoteLset)).await;
    proc(&db, "DST").await;

    assert_eq!(
        db.get_record("DST").unwrap().read().record.get_field("VAL"),
        Some(EpicsValue::Double(10.0)),
        "the non-local input must resolve through the CA lset"
    );
    assert_eq!(
        stamp_of(&db, "DST"),
        (
            SystemTime::UNIX_EPOCH + Duration::new(1_700_000_456, 321),
            A_UTAG
        ),
        "a non-local time input is a CA link; its stamp is the lset's"
    );
}
