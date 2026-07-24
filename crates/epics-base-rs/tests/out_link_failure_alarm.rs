//! R14-62: a failed OUT/SIOL link put raises LINK_ALARM/INVALID on the WRITING
//! record, in the SAME processing cycle.
//!
//! C `dbPutLink` (dbLink.c:434-448) — and its async twin `dbPutLinkAsync`
//! (:459-471) — call `setLinkAlarm(plink)` on a nonzero `putValue` status;
//! `setLinkAlarm` is `recGblSetSevrMsg(precord, LINK_ALARM, INVALID_ALARM,
//! "field %s", dbLinkFieldName(plink))` (dbLink.c:318-323). The raise lands in
//! the record's PENDING alarm (`nsta`/`nsev`), and record `process()` runs
//! `checkAlarms` → `writeValue` → `monitor()` (aoRecord.c:196-232), so the
//! cycle's `recGblResetAlarms` inside `monitor()` commits it — the alarm is
//! visible after ONE process, not one cycle later.
//!
//! The port used to swallow every put failure (an `eprintln!`, a discarded
//! bool) and drove OUT links in the post-commit forward-link tail, so even a
//! raised alarm could not have folded into this cycle. Each test below is a
//! boundary of the put owner: local DB target, external `ca://` target, SIOL
//! simulated output, and the recovery cycle.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::{LinkPutOp, LinkSet, PvDatabase};
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

/// An external link set whose puts always fail — C's never-connected CA link
/// (`dbCa.c::dbCaPutLink` returns -1 when `pca->connected` is false).
struct FailingLset;

#[async_trait::async_trait]
impl LinkSet for FailingLset {
    fn is_connected(&self, _: &str) -> bool {
        false
    }
    fn get_cached_value(&self, _: &str) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, name: &str) -> Option<EpicsValue> {
        self.get_cached_value(name)
    }
    async fn put_value(&self, _: &str, _: EpicsValue, _: LinkPutOp) -> Result<(), String> {
        Err("not connected".to_string())
    }
}

/// An external link set whose puts always succeed — the control case.
struct OkLset {
    puts: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl LinkSet for OkLset {
    fn is_connected(&self, _: &str) -> bool {
        true
    }
    fn get_cached_value(&self, _: &str) -> Option<EpicsValue> {
        None
    }
    async fn get_value(&self, name: &str) -> Option<EpicsValue> {
        self.get_cached_value(name)
    }
    async fn put_value(&self, name: &str, _: EpicsValue, _: LinkPutOp) -> Result<(), String> {
        self.puts.lock().unwrap().push(name.to_string());
        Ok(())
    }
}

/// Soft-Channel ao (DTYP empty) with VAL=3.0 and the given OUT link — the
/// plain soft OUT-link write path.
async fn add_ao_with_out(db: &PvDatabase, name: &str, out: &str) {
    db.add_record(name, Box::new(AoRecord::new(3.0)))
        .await
        .unwrap();
    let rec = db.get_record(name).expect("just added");
    let mut inst = rec.write();
    inst.put_common_field("OUT", EpicsValue::String(out.into()))
        .unwrap();
    inst.common.udf = 0;
}

async fn alarm_of(db: &PvDatabase, name: &str) -> (u16, AlarmSeverity) {
    let rec = db.get_record(name).expect("record exists");
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr)
}

/// Boundary 1 — the OUT link names no local record and no external link set is
/// registered, so the put fails. One process must leave LINK/INVALID.
#[epics_macros_rs::epics_test]
async fn r14_62_failing_local_out_put_raises_link_invalid_same_cycle() {
    let db = PvDatabase::new();
    add_ao_with_out(&db, "AO_BADOUT", "NO_SUCH_TARGET").await;

    let mut visited = HashSet::new();
    db.process_record_with_links("AO_BADOUT", &mut visited, 0)
        .await
        .unwrap();

    assert_eq!(
        alarm_of(&db, "AO_BADOUT").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Invalid),
        "a failed OUT put must raise LINK/INVALID in the cycle that performed it \
         (dbLink.c:434-448 setLinkAlarm inside dbPutLink)"
    );
}

/// Boundary 2 — an external `ca://` OUT link whose put fails takes the same
/// raise: C routes DB and CA links through the identical `dbPutLink` gate, the
/// alarm comes from `dbPutLink` itself, not from the lset.
#[epics_macros_rs::epics_test]
async fn r14_62_failing_external_out_put_raises_link_invalid() {
    let db = PvDatabase::new();
    db.register_link_set("ca", Arc::new(FailingLset)).await;

    add_ao_with_out(&db, "AO_BADCA", "ca://REMOTE:OUT").await;

    let mut visited = HashSet::new();
    db.process_record_with_links("AO_BADCA", &mut visited, 0)
        .await
        .unwrap();

    assert_eq!(
        alarm_of(&db, "AO_BADCA").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Invalid),
        "a failed external OUT put must raise LINK/INVALID — the raise is inside \
         dbPutLink, above the lset"
    );
}

/// Boundary 3 — the SIMM-mode simulated output. C `writeValue` (aoRecord.c:
/// 384-406) puts the simulated value through `dbPutLink(&prec->siol, …)`, the
/// same gate, so a failing SIOL raises LINK/INVALID exactly as a failing OUT
/// does. The port's `write_sim_siol_value` used to drop its error entirely.
#[epics_macros_rs::epics_test]
async fn r14_62_failing_siol_sim_write_raises_link_invalid() {
    let db = PvDatabase::new();
    db.add_record("SIM_ON", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();

    let mut ao = AoRecord::new(55.0);
    ao.siml = "SIM_ON".to_string(); // nonzero → SIMM=YES this cycle
    ao.siol = "NO_SUCH_SIOL".to_string(); // no local record, no link set
    db.add_record("AO_BADSIOL", Box::new(ao)).await.unwrap();

    let mut visited = HashSet::new();
    db.process_record_with_links("AO_BADSIOL", &mut visited, 0)
        .await
        .unwrap();

    assert_eq!(
        alarm_of(&db, "AO_BADSIOL").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Invalid),
        "a failed SIOL simulated-output put must raise LINK/INVALID this cycle"
    );
}

/// Boundary 4 — recovery. The alarm is PENDING state, re-raised per cycle by
/// the put that fails; once the put succeeds, the next `recGblResetAlarms`
/// commits NO_ALARM (recGbl.c:186-220: `nsta`/`nsev` are re-initialised to 0
/// at the end of every commit). A latched alarm would stay INVALID forever.
#[epics_macros_rs::epics_test]
async fn r14_62_successful_put_next_cycle_clears_the_link_alarm() {
    let puts = Arc::new(Mutex::new(Vec::new()));
    let db = PvDatabase::new();
    db.register_link_set("ca", Arc::new(FailingLset)).await;

    add_ao_with_out(&db, "AO_RECOVER", "ca://REMOTE:OUT").await;

    let mut visited = HashSet::new();
    db.process_record_with_links("AO_RECOVER", &mut visited, 0)
        .await
        .unwrap();
    assert_eq!(
        alarm_of(&db, "AO_RECOVER").await,
        (alarm_status::LINK_ALARM, AlarmSeverity::Invalid),
        "cycle 1: the put fails, so the record is in LINK/INVALID"
    );

    // The link comes up: the same OUT link now puts successfully.
    db.register_link_set(
        "ca",
        Arc::new(OkLset {
            puts: Arc::clone(&puts),
        }),
    )
    .await;

    let mut visited = HashSet::new();
    db.process_record_with_links("AO_RECOVER", &mut visited, 0)
        .await
        .unwrap();

    // A plain OUT put is staged on the link-put queue and the record returns
    // — C `dbCaPutLink` does the same (`dbCa.c:622-624`), and the wire write
    // happens on the `dbCaTask`. `dbCaSync` (`dbCa.c:1191-1194`) is the
    // barrier that makes it observable.
    db.sync_external_link_puts().await;

    assert_eq!(
        puts.lock().unwrap().as_slice(),
        ["REMOTE:OUT"],
        "cycle 2: the OUT put reached the (now working) external link set"
    );
    assert_eq!(
        alarm_of(&db, "AO_RECOVER").await,
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm),
        "cycle 2: a succeeding put raises nothing, so the commit clears the alarm"
    );
}

/// Companion gate — a working OUT link must NOT raise any alarm. Pins that the
/// raise fires on a real put failure only.
#[epics_macros_rs::epics_test]
async fn r14_62_successful_local_out_put_raises_no_alarm() {
    let db = PvDatabase::new();
    db.add_record("AO_GOOD_DEST", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();

    add_ao_with_out(&db, "AO_GOODOUT", "AO_GOOD_DEST").await;

    let mut visited = HashSet::new();
    db.process_record_with_links("AO_GOODOUT", &mut visited, 0)
        .await
        .unwrap();

    assert_eq!(
        alarm_of(&db, "AO_GOODOUT").await,
        (alarm_status::NO_ALARM, AlarmSeverity::NoAlarm),
        "a successful OUT put must leave the writing record un-alarmed"
    );
    assert_eq!(
        db.get_pv("AO_GOOD_DEST").unwrap().to_f64(),
        Some(3.0),
        "…and the value must have landed on the target"
    );
}
