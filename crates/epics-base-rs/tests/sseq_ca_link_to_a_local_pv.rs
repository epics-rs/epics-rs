//! A `CA` link naming a record of this IOC is a `CA_LINK` to sseq, whatever
//! status it reads: `special()` posts it `Local PV` by `dbNameToAddr` alone
//! (sseqRecord.c:1006, :1051), `checkLinks` leaves that status and raises no
//! `WERRn` for it (:863, :896-910), and its step waits on the
//! put-callback (:717). `init_record` instead reads it external (:208,
//! :227). The expected values are the C oracle's (calc f207871) for the same
//! database and puts.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::{ProcStack, PutSplit, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::EpicsValue;

const EXT_OK: i16 = 1;
const LOC: i16 = 2;
/// C dbStatic `DBF_DOUBLE`.
const DBF_DOUBLE: i16 = 10;

/// `T` stays busy 1 s once processed, so a put-callback into `T.PROC` is
/// outstanding that long.
const DB: &str = r#"
record(ao, "L") { field(VAL, "5") }
record(ao, "X") { field(VAL, "0") }
record(sseq, "T") { field(DLY1, "1") field(DO1, "1") field(LNK1, "X") }
record(sseq, "SA") { field(WAIT1, "Wait") field(DO1, "3") }
record(sseq, "SI") { field(DOL1, "L CA") field(LNK1, "L CA") field(WAIT1, "Wait") }
record(sseq, "SW") { field(DO1, "3") field(LNK1, "T.PROC CA") field(WAIT1, "Wait") }
"#;

async fn build() -> Arc<PvDatabase> {
    let ioc = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    db
}

fn read(db: &PvDatabase, pv: &str) -> i16 {
    db.get_pv(pv)
        .ok()
        .and_then(|v| v.to_f64())
        .map(|f| f as i16)
        .unwrap_or_else(|| panic!("{pv} not readable as a number"))
}

async fn settled(db: &PvDatabase, pv: &str, want: i16) {
    for _ in 0..400 {
        if db.get_pv(pv).ok().and_then(|v| v.to_f64()) == Some(f64::from(want)) {
            return;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    panic!("{pv} never reached {want}: {:?}", db.get_pv(pv));
}

/// Puts `link` = `to` on `rec` and reads its status under the put's hold, so
/// no task can post between the two.
fn status_right_after(db: &PvDatabase, rec: &str, link: &str, to: &str) -> i16 {
    db.put_many_locked(&[rec], 1, |held| {
        let r = held.record(0).unwrap();
        held.put_record_field_from_ca_no_notify(
            r,
            link,
            EpicsValue::String(to.into()),
            PutSplit::DbPutField,
        )
        .unwrap();
        read(db, &format!("{rec}.{link}V"))
    })
}

async fn put_link(db: &PvDatabase, rec: &str, link: &str, to: &str) {
    db.put_record_field_from_ca(rec, link, EpicsValue::String(to.into()))
        .await
        .unwrap();
}

async fn kick(db: &PvDatabase, rec: &str) {
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .unwrap();
}

#[epics_base_rs::epics_test]
async fn a_ca_lnk_put_naming_a_local_pv_reads_local_at_once() {
    let db = build().await;
    assert_eq!(status_right_after(&db, "SA", "LNK1", "L CA"), LOC);
}

#[epics_base_rs::epics_test]
async fn a_ca_dol_put_naming_a_local_pv_reads_local_at_once() {
    let db = build().await;
    assert_eq!(status_right_after(&db, "SA", "DOL1", "L CA"), LOC);
}

/// The pass a plain link raised `WERR1` in rescinds it once the link is
/// `CA`, and leaves the `Local PV` `special()` posted.
#[epics_base_rs::epics_test]
async fn the_pass_keeps_a_ca_link_to_a_local_pv_local_and_rescinds_werr() {
    let db = build().await;
    put_link(&db, "SA", "LNK1", "L").await;
    settled(&db, "SA.WERR1", 1).await;
    put_link(&db, "SA", "LNK1", "L CA").await;
    settled(&db, "SA.WERR1", 0).await;
    assert_eq!(read(&db, "SA.LNK1V"), LOC);
    assert_eq!(read(&db, "SA.LT1"), DBF_DOUBLE);
}

/// Fires `rec`'s step 1 into the busy `T` and asserts it waits on the
/// put-callback until `T` is done, as C's `SW`.
async fn waits_on_t(db: &PvDatabase, rec: &str) {
    kick(db, rec).await;
    settled(db, &format!("{rec}.WTG1"), 1).await;
    assert_eq!(read(db, &format!("{rec}.BUSY")), 1);
    // The put reaches `T` from the link put queue, after `WTG1`.
    settled(db, "T.BUSY", 1).await;
    assert_eq!(read(db, &format!("{rec}.WTG1")), 1);
    settled(db, &format!("{rec}.WTG1"), 0).await;
    assert_eq!(read(db, "T.BUSY"), 0);
    settled(db, &format!("{rec}.BUSY"), 0).await;
}

/// `processCallback` keys the put-callback on the link's type: the step
/// waits on it through `Local PV`.
#[epics_base_rs::epics_test]
async fn a_ca_link_put_to_a_local_pv_waits_on_its_put_callback() {
    let db = build().await;
    put_link(&db, "SA", "LNK1", "T.PROC").await;
    settled(&db, "SA.WERR1", 1).await;
    put_link(&db, "SA", "LNK1", "T.PROC CA").await;
    // The pass after the put, which found the new channel connected.
    settled(&db, "SA.WERR1", 0).await;
    assert!(read(&db, "SA.LT1") >= 0);
    assert_eq!(read(&db, "SA.LNK1V"), LOC);
    waits_on_t(&db, "SA").await;
}

/// The same link from the database: `init_record` reads it external, and it
/// waits the same.
#[epics_base_rs::epics_test]
async fn a_ca_link_to_a_local_pv_from_init_waits_on_its_put_callback() {
    let db = build().await;
    settled(&db, "SW.LNK1V", EXT_OK).await;
    waits_on_t(&db, "SW").await;
}

/// A plain link to the same record is a `DB_LINK`: the step does not wait on
/// it, and the pass raises `WERR1`.
#[epics_base_rs::epics_test]
async fn a_plain_link_put_to_a_local_pv_does_not_wait() {
    let db = build().await;
    put_link(&db, "SA", "LNK1", "T.PROC").await;
    settled(&db, "SA.WERR1", 1).await;
    kick(&db, "SA").await;
    settled(&db, "T.BUSY", 1).await;
    settled(&db, "SA.BUSY", 0).await;
    assert_eq!(read(&db, "T.BUSY"), 1);
    assert_eq!(read(&db, "SA.WTG1"), 0);
}

/// `init_record` reads the same link external; its pass finds it connected.
#[epics_base_rs::epics_test]
async fn init_reads_a_ca_link_to_a_local_pv_external() {
    let db = build().await;
    settled(&db, "SI.LNK1V", EXT_OK).await;
    assert_eq!(read(&db, "SI.DOL1V"), EXT_OK);
    assert_eq!(read(&db, "SI.WERR1"), 0);
}
