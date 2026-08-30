//! `SNAM` resolution at init: `sub` always, `aSub` only under `LFLG=IGNORE`.
//!
//! ```c
//! if (prec->lflg == aSubLFLG_IGNORE &&
//!     prec->snam[0] != 0) {
//!     pfunc = (GENFUNCPTR)registryFunctionFind(prec->snam);
//!     ...
//! }
//! strcpy(prec->onam, prec->snam);
//! ```
//! (`aSubRecord.c:151-162`.) An `LFLG=READ` aSub therefore leaves `sadr`
//! NULL at init, and `fetch_values` (`:254-275`) re-resolves only when the
//! name the `SUBL` link delivered DIFFERS from `onam` — which init just made
//! equal to `snam`. So a READ aSub with no SUBL link never binds a
//! subroutine at all, and `do_sub` (`:456-463`) takes its null-pointer exit:
//! `BAD_SUB_ALARM` at `INVALID_ALARM`, returning `S_db_BadSub`.
//!
//! The port resolved SNAM at init for both flavours, so that record ran its
//! subroutine and read back `NO_ALARM`. The rule already had an owner —
//! `Record::is_subroutine_name_field`, which the put path consults for the
//! same `LFLG` reason — so init asks it instead of re-deriving the type test.
//!
//! Second half: `sub` with an EMPTY SNAM.
//!
//! ```c
//! if (prec->snam[0] == 0) {
//!     epicsPrintf("%s.SNAM is empty\n", prec->name);
//!     prec->pact = TRUE;
//!     return 0;
//! }
//! ```
//! (`subRecord.c:118-122`.) `epicsPrintf` is `errlogPrintf` (`errlog.h:90`),
//! so the line belongs to the errlog and not to stderr. The PACT half was
//! already ported (`SubRecord::parks_pact`); only the line was missing.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;

/// C `S_db_BadSub` (`dbAccessDefs.h:189`), aSub's VAL on the null-`sadr` exit.
const S_DB_BAD_SUB: i32 = (511 << 16) | 35;

/// The empty-SNAM line is written during `build()`, so the listener goes first.
fn listen() -> Arc<Mutex<Vec<String>>> {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    epics_base_rs::runtime::log::errlog_add_listener(move |m| {
        sink.lock().expect("sink").push(m.to_string());
    });
    heard
}

fn heard(sink: &Arc<Mutex<Vec<String>>>) -> String {
    epics_base_rs::runtime::log::errlog_flush();
    sink.lock().expect("sink").join("\n")
}

async fn ioc(db_text: &str) -> Arc<PvDatabase> {
    IocBuilder::new()
        // Returns 0, and stamps VAL so a bound subroutine is distinguishable
        // from an unbound one by more than the alarm.
        .register_subroutine("theSub", |rec| {
            let _ = rec.put_field("VALA", EpicsValue::Long(42));
            Ok(0)
        })
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &PvDatabase, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

/// `LFLG=IGNORE` (the default) binds at init and runs; `LFLG=READ` does not
/// bind, and takes `do_sub`'s null-pointer exit on the very first cycle.
#[epics_macros_rs::epics_test]
async fn an_lflg_read_asub_does_not_bind_its_snam_at_init() {
    let db = ioc(concat!(
        "record(aSub, \"T:IGN\") { field(SNAM,\"theSub\") field(LFLG,\"IGNORE\")\n",
        "                          field(FTVA,\"LONG\") field(NOVA,\"1\") }\n",
        "record(aSub, \"T:RD\")  { field(SNAM,\"theSub\") field(LFLG,\"READ\")\n",
        "                          field(FTVA,\"LONG\") field(NOVA,\"1\") }\n",
    ))
    .await;

    process(&db, "T:IGN").await;
    process(&db, "T:RD").await;

    let ign = db.get_record("T:IGN").expect("T:IGN");
    let ign = ign.read();
    assert_eq!(
        (ign.common.stat, ign.common.sevr),
        (0, AlarmSeverity::NoAlarm),
        "C `aSubRecord.c:151`: LFLG=IGNORE resolves SNAM at init and runs"
    );
    assert_eq!(
        ign.record.get_field("VAL"),
        Some(EpicsValue::Long(0)),
        "the subroutine ran and returned 0"
    );
    drop(ign);

    let rd = db.get_record("T:RD").expect("T:RD");
    let rd = rd.read();
    assert_eq!(
        (rd.common.stat, rd.common.sevr),
        (alarm_status::BAD_SUB_ALARM, AlarmSeverity::Invalid),
        "C `aSubRecord.c:456-463`: LFLG=READ left `sadr` NULL, and `onam == \
         snam` keeps `fetch_values` from re-resolving it"
    );
    assert_eq!(
        rd.record.get_field("VAL"),
        Some(EpicsValue::Long(S_DB_BAD_SUB)),
        "C `process`: prec->val = do_sub() = S_db_BadSub"
    );
}

/// C `subRecord.c:118-122` — the report and the PACT park are one branch.
#[epics_macros_rs::epics_test]
async fn a_sub_with_an_empty_snam_says_so_on_the_errlog() {
    let sink = listen();
    let db = ioc(concat!(
        "record(sub, \"T:EMPTY\") { }\n",
        "record(sub, \"T:NAMED\") { field(SNAM,\"theSub\") }\n",
    ))
    .await;

    let log = heard(&sink);
    assert!(
        log.contains("T:EMPTY.SNAM is empty"),
        "C `subRecord.c:119` through `epicsPrintf`, got {log:?}"
    );
    assert!(
        !log.contains("T:NAMED.SNAM is empty"),
        "a named SNAM takes no such branch, got {log:?}"
    );
    assert_eq!(
        db.get_pv("T:EMPTY.PACT").expect("PACT reads back"),
        EpicsValue::Char(1),
        "C `subRecord.c:120`: prec->pact = TRUE, and it is never released"
    );
}

/// An empty SNAM on an aSub is `aSubRecord.c:152`'s `snam[0] != 0` test,
/// which says nothing at all — the report belongs to `sub` alone.
#[epics_macros_rs::epics_test]
async fn an_empty_snam_on_an_asub_is_silent() {
    let sink = listen();
    let db = ioc("record(aSub, \"T:A\") { }\n").await;

    let log = heard(&sink);
    assert!(
        !log.contains("T:A.SNAM is empty"),
        "C reports an empty SNAM only from `subRecord.c:119`, got {log:?}"
    );
    process(&db, "T:A").await;
    let rec = db.get_record("T:A").expect("T:A");
    let rec = rec.read();
    assert_eq!(
        (rec.common.stat, rec.common.sevr),
        (0, AlarmSeverity::NoAlarm),
        "C `aSubRecord.c:459`: an empty SNAM returns 0 before the null check"
    );
}

/// C `subRecord.c:130-132` is the tail BELOW the two early returns, so a
/// record whose SNAM is empty (`:122`) or unregistered (`:128`) never seeds
/// its trackers. Measured on softIoc R7.0.10 with `dbpr REC 4`:
/// `MLST: 0 ALST: 0 LALM: 0` for both, against `VAL : 5`.
#[epics_macros_rs::epics_test]
async fn only_a_resolved_snam_seeds_the_trackers() {
    let db = ioc(concat!(
        "record(sub, \"T:OK\")    { field(SNAM,\"theSub\") field(VAL,\"5\") }\n",
        "record(sub, \"T:MISS\")  { field(SNAM,\"noSuchSub\") field(VAL,\"5\") }\n",
        "record(sub, \"T:EMPTY\") { field(VAL,\"5\") }\n",
        "record(sub, \"T:BADIN\") { field(INAM,\"noSuchInit\") field(SNAM,\"theSub\")\n",
        "                          field(VAL,\"5\") }\n",
    ))
    .await;

    let trackers = |name: &str| {
        let rec = db.get_record(name).expect("record");
        let rec = rec.read();
        (
            rec.record.get_field("MLST"),
            rec.record.get_field("ALST"),
            rec.record.get_field("LALM"),
        )
    };
    let seeded = (
        Some(EpicsValue::Double(5.0)),
        Some(EpicsValue::Double(5.0)),
        Some(EpicsValue::Double(5.0)),
    );
    let unseeded = (
        Some(EpicsValue::Double(0.0)),
        Some(EpicsValue::Double(0.0)),
        Some(EpicsValue::Double(0.0)),
    );

    assert_eq!(trackers("T:OK"), seeded, "C `subRecord.c:130-132`");
    assert_eq!(
        trackers("T:MISS"),
        unseeded,
        "C `subRecord.c:128` returns S_db_BadSub above the tail"
    );
    assert_eq!(
        trackers("T:EMPTY"),
        unseeded,
        "C `subRecord.c:122` returns 0 above the tail"
    );
    // C `subRecord.c:113` returns S_db_BadSub for an INAM that does not
    // resolve, which skips the SNAM lookup as well — so a perfectly good SNAM
    // stays unbound and the tail is never reached.
    assert_eq!(
        trackers("T:BADIN"),
        unseeded,
        "C `subRecord.c:110-114` returns above both the lookup and the tail"
    );
    assert!(
        db.get_record("T:BADIN")
            .expect("T:BADIN")
            .read()
            .subroutine
            .is_none(),
        "the INAM early return skips `prec->sadr = registryFunctionFind(snam)`"
    );
}
