//! A link read that FAILED inherits no severity from its source. C gates the
//! inheritance tail on the read's own status, in every one of the three link
//! schemes:
//!
//! ```c
//! /* dbDbGetValue, dbDbLink.c:228 */
//! if (!status && precord != dbChannelRecord(chan))
//!     recGblInheritSevrMsg(plink->value.pv_link.pvlMask & pvlOptMsMode, ...);
//!
//! /* dbCaGetValue, dbCa.c:500 */
//! if (!status)
//!     recGblInheritSevr(plink->value.pv_link.pvlMask & pvlOptMsMode,
//!         plink->precord, pca->stat, pca->sevr);
//! ```
//!
//! and pvxs `pvaGetValue` (`ioc/pvalink_lset.cpp:259`) returns `-1` at `:272`
//! for a disconnected channel, before ever reaching its own MS gate at `:424-425`.
//! What a failed read produces instead is `setLinkAlarm` — `LINK_ALARM` /
//! `INVALID`, AMSG `field <NAME>` — which is not inheritance and does not
//! consult the link's MS class.
//!
//! The port inherited unconditionally: `read_link_with_alarm` handed back the
//! source record's committed alarm whenever the record EXISTED, even when the
//! field read off it failed, and both `db_try_get_link` and
//! `db_try_get_link_deferred` then applied it.
//!
//! `MSS` and an `INVALID` source are what make the bug visible. `MSS` is the
//! one mode that copies the source's STAT and AMSG rather than substituting
//! `LINK_ALARM`, and `recGblSetSevrMsg` maximizes — so with the source already
//! at `INVALID`, the `setLinkAlarm` that follows cannot overwrite the wrongly
//! inherited status. A lower source severity would be masked by it.
//!
//! Which case is EVIDENCE and which is a guard follows from that same maximize
//! rule, and the two disagree. Measured by reverting the gate in
//! `read_link_with_alarm` and running this file:
//! `a_failed_inline_read_does_not_inherit_the_sources_status` fails (STAT 4
//! where 14 is required) and `a_failed_dynlink_read_does_not_inherit_the_sources_status`
//! fails (STAT 4 where anything else is required) — those two are the
//! fails-first proof. `a_failed_deferred_read_does_not_inherit_the_sources_status`
//! PASSES with the gate reverted, because that path raises `setLinkAlarm`
//! before it folds the collected alarms, so the wrong status arrived second at
//! equal severity and the strict-greater test dropped it. It is a regression
//! guard for a path the defect never reached, not evidence of the defect.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::swait::SwaitRecord;

const DB: &str = r#"
record(ai, "SRC") { field(VAL, "1") }

# The failed reads: SRC exists, SRC.NOSUCH does not.
record(calc,  "DEFERRED") { field(INPA, "SRC.NOSUCH MSS") field(CALC, "A") }
record(ai,    "INLINE")   { field(SDIS, "SRC.NOSUCH MSS") }
record(swait, "DYNLINK")  { field(INAN, "SRC.NOSUCH MSS") field(CALC, "A") }

# The control: the same MSS link, but a read that succeeds.
record(calc, "OK") { field(INPA, "SRC MSS") field(CALC, "A") }
"#;

type Db = Arc<PvDatabase>;

/// `SRC` parked at INVALID under a status that is NOT `LINK_ALARM`, so a
/// wrongly inherited status is distinguishable from `setLinkAlarm`'s.
async fn build() -> Db {
    // `swait` is synApps `calc`, not Base: an application that loads it says
    // so, the way a real one loads `calcSupport.dbd`.
    let db = IocBuilder::new()
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    {
        let rec = db.get_record("SRC").unwrap();
        let mut inst = rec.write();
        inst.common.stat = alarm_status::HIGH_ALARM;
        inst.common.sevr = AlarmSeverity::Invalid;
        inst.common.amsg = "from the source".into();
    }
    db
}

async fn process(db: &Db, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

fn alarm(db: &Db, name: &str) -> (u16, AlarmSeverity, String) {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr, inst.common.amsg.clone())
}

/// The multi-input fetch (`db_get_link_deferred`), which reads INPA..INPL with
/// the write lock released and folds the alarms in afterwards.
///
/// REGRESSION GUARD, not evidence: this case passes with the gate reverted (see
/// the module doc). It pins that the fold keeps taking `setLinkAlarm`'s answer,
/// which is what would break if the two effects ever swapped order here.
#[epics_macros_rs::epics_test]
async fn a_failed_deferred_read_does_not_inherit_the_sources_status() {
    let db = build().await;
    process(&db, "DEFERRED").await;

    let (stat, sevr, amsg) = alarm(&db, "DEFERRED");
    assert_eq!(
        stat,
        alarm_status::LINK_ALARM,
        "the failed read's own setLinkAlarm owns the status; MSS must not copy SRC's"
    );
    assert_eq!(sevr, AlarmSeverity::Invalid);
    assert_eq!(amsg, "field INPA", "dbLinkFieldName(plink), not SRC's amsg");
}

/// The inline reader (`db_get_link`), which applies the inheritance inside the
/// read primitive and its own `setLinkAlarm` only after — so the source's
/// status landed first and stuck. This is the fails-first case: with the gate
/// reverted it reads STAT 4 (`HIGH_ALARM`, copied from SRC) where 14
/// (`LINK_ALARM`) is required.
#[epics_macros_rs::epics_test]
async fn a_failed_inline_read_does_not_inherit_the_sources_status() {
    let db = build().await;
    process(&db, "INLINE").await;

    let (stat, sevr, amsg) = alarm(&db, "INLINE");
    assert_eq!(stat, alarm_status::LINK_ALARM);
    assert_eq!(sevr, AlarmSeverity::Invalid);
    assert_eq!(amsg, "field SDIS");
}

/// The deferred read that does NOT go through `dbGetLink`: swait's
/// `recDynLinkGet` (`swaitRecord.c:686-705`) has no `setLinkAlarm` to mask a
/// wrong inheritance and no inheritance tail of its own, so the source's
/// status reaches the record undisguised. C answers the failure with
/// `recGblSetSevr(READ_ALARM, INVALID_ALARM)` at `swaitRecord.c:413`.
#[epics_macros_rs::epics_test]
async fn a_failed_dynlink_read_does_not_inherit_the_sources_status() {
    let db = build().await;
    process(&db, "DYNLINK").await;

    let (stat, _sevr, amsg) = alarm(&db, "DYNLINK");
    assert_ne!(
        stat,
        alarm_status::HIGH_ALARM,
        "recDynLinkGet has no inheritance tail; SRC's HIGH must not appear here"
    );
    assert_ne!(amsg, "from the source");
}

/// The boundary the gate must not cross: a read that SUCCEEDS still inherits,
/// message and all. Dropping the alarm on every read would silently undo
/// `link_read_inherits_ms_everywhere`.
#[epics_macros_rs::epics_test]
async fn a_successful_read_still_inherits() {
    let db = build().await;
    process(&db, "OK").await;

    let (stat, sevr, amsg) = alarm(&db, "OK");
    assert_eq!(
        stat,
        alarm_status::HIGH_ALARM,
        "MSS copies the source's stat on a healthy read"
    );
    assert_eq!(sevr, AlarmSeverity::Invalid);
    assert_eq!(amsg, "from the source");
}
