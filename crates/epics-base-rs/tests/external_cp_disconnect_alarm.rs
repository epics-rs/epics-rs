//! A Passive CP holder of an external link goes LINK/INVALID when the
//! link drops — C `dbCa.c`'s disconnect path, end to end.
//!
//! C reference. `connectionCallback` (`dbCa.c:848-873`) clears
//! `pca->isConnected` and, for a `pvlOptCP` link (or a `pvlOptCPP` link
//! whose holder has `scan == 0`), sets `link_action |= CA_DBPROCESS`. The
//! `dbCaTask` worker then runs `db_process(prec)` (`dbCa.c:1295`). That
//! process calls `dbGetLink` → `dbCaGetLink`, which returns `-1` with
//! `pca->sevr = INVALID_ALARM; pca->stat = LINK_ALARM` because
//! `!pca->isConnected` (`dbCa.c:459-463`), and the record commits
//! LINK/INVALID.
//!
//! The half this file pins is the one a unit test on the resolver cannot
//! see: that a dispatch on a link which has *stopped serving a value*
//! actually lands the alarm on the holder. Measured missing on target —
//! stage C6 criterion 4, `doc/calink-rtems-design.md` §11.4: the guest's
//! downstream records held their last good value with `SEVR=0 STAT=0` for
//! the whole 65 s upstream outage.

use std::sync::Arc;

use epics_base_rs::server::database::{LinkSet, PvDatabase};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;

/// An lset with a switchable circuit, modelling `CaLink`'s servability
/// gate: while `up` is false every value-derived accessor reports `None`,
/// exactly as `CaLink::with_servable` does once the connection watcher
/// has cleared the flag. The cached snapshot is deliberately kept — the
/// bug being pinned is that a *stale but present* cache must not be
/// served once the circuit is down.
struct SwitchableLset {
    up: parking_lot::Mutex<bool>,
    cached: EpicsValue,
}

impl SwitchableLset {
    fn new(v: f64) -> Arc<Self> {
        Arc::new(Self {
            up: parking_lot::Mutex::new(true),
            cached: EpicsValue::Double(v),
        })
    }

    fn set_up(&self, up: bool) {
        *self.up.lock() = up;
    }

    fn servable(&self) -> bool {
        *self.up.lock()
    }
}

#[async_trait::async_trait]
impl LinkSet for SwitchableLset {
    fn is_connected(&self, _: &str) -> bool {
        self.servable()
    }

    async fn get_value(&self, _: &str) -> Option<EpicsValue> {
        self.servable().then(|| self.cached.clone())
    }

    fn get_cached_value(&self, _: &str) -> Option<EpicsValue> {
        self.servable().then(|| self.cached.clone())
    }

    async fn connect_link(&self, _: &str) {}
}

fn alarm_of(db: &PvDatabase, name: &str) -> (u16, u16) {
    let rec = db.get_record(name).expect("record exists");
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr as u16)
}

async fn holder_db(lset: Arc<SwitchableLset>) -> PvDatabase {
    let db = PvDatabase::new();
    db.register_link_set("ca", lset).await;
    db.add_record("HOLDER", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("HOLDER").unwrap();
        let mut inst = rec.write();
        inst.common.inp = "ca://UP:PV CP".to_string();
        inst.parsed_inp = epics_base_rs::server::record::parse_link_v2("ca://UP:PV CP");
    }
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db
}

/// The gate itself: with the circuit up, a dispatch pulls the upstream
/// value in with no alarm; with the circuit down, the very same dispatch
/// commits LINK/INVALID and leaves VAL where it was.
#[epics_macros_rs::epics_test]
async fn a_dispatch_on_a_dropped_link_commits_link_invalid() {
    use epics_base_rs::server::recgbl::alarm_status::LINK_ALARM;

    let lset = SwitchableLset::new(42.5);
    let db = holder_db(lset.clone()).await;

    // Circuit up: the CP dispatch is the ordinary monitor-event path.
    db.dispatch_external_cp_targets("UP:PV");
    assert_eq!(
        db.get_pv("HOLDER").ok(),
        Some(EpicsValue::Double(42.5)),
        "a connected CP link must deliver its value on dispatch"
    );
    assert_eq!(
        alarm_of(&db, "HOLDER"),
        (0, 0),
        "a connected CP link must leave the holder in NO_ALARM"
    );

    // The outage. This is the dispatch C's `connectionCallback` adds and
    // that calink did not: without it the holder is Passive, is never
    // processed again, and reports its last good value with SEVR=0 for
    // the whole outage.
    lset.set_up(false);
    db.dispatch_external_cp_targets("UP:PV");

    assert_eq!(
        alarm_of(&db, "HOLDER"),
        (LINK_ALARM, 3),
        "a dropped CP link must commit LINK_ALARM / INVALID_ALARM on its holder"
    );
    assert_eq!(
        db.get_pv("HOLDER").ok(),
        Some(EpicsValue::Double(42.5)),
        "a failed link read must leave VAL untouched (C `dbGetLink` returns -1 \
         before writing the destination)"
    );
}

/// Recovery, the other half of criterion 4: the alarm must clear on the
/// next event after the circuit comes back, with no restart of the IOC.
#[epics_macros_rs::epics_test]
async fn the_link_alarm_clears_on_the_next_event_after_recovery() {
    let lset = SwitchableLset::new(7.25);
    let db = holder_db(lset.clone()).await;

    lset.set_up(false);
    db.dispatch_external_cp_targets("UP:PV");
    assert_eq!(alarm_of(&db, "HOLDER").1, 3, "outage must raise INVALID");

    lset.set_up(true);
    db.dispatch_external_cp_targets("UP:PV");
    assert_eq!(
        alarm_of(&db, "HOLDER"),
        (0, 0),
        "the holder must return to NO_ALARM once the link serves again"
    );
    assert_eq!(db.get_pv("HOLDER").ok(), Some(EpicsValue::Double(7.25)));
}
