//! A Passive CP holder of an external link goes LINK/INVALID when the
//! link loses READ access — C `dbCa.c`'s access-rights path, end to end.
//!
//! C reference. `accessRightsCallback` (`dbCa.c:1014-1040`) caches the new
//! rights and, for a `pvlOptCP` link (or a `pvlOptCPP` link whose holder
//! has `scan == 0`), adds `CA_DBPROCESS` whenever the rights are not fully
//! held while connected (`dbCa.c:1029` skips only when
//! `hasReadAccess && hasWriteAccess`). The `dbCaTask` worker then runs
//! `db_process(prec)` (`dbCa.c:1249-1257`); that process calls `dbGetLink`
//! → `dbCaGetLink`, which returns `-1` with `pca->sevr = INVALID_ALARM;
//! pca->stat = LINK_ALARM` because `!pca->hasReadAccess` (`dbCa.c:430-434`)
//! even though `pca->isConnected` is still TRUE, and the record commits
//! LINK/INVALID.
//!
//! The half this file pins is the one the resolver unit tests cannot see:
//! that a dispatch on a link whose CIRCUIT IS STILL UP but whose value read
//! is denied actually lands the alarm on the holder. This is the boundary
//! that distinguishes it from `external_cp_disconnect_alarm.rs`, where
//! `is_connected` and the value gate close together — here `is_connected`
//! stays `true` throughout (C's lset `isConnected`, `dbCa.c:604-612`, does
//! not consult access rights) and only the value read fails.

use std::sync::Arc;

use epics_base_rs::server::database::{LinkSet, PvDatabase};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;

/// An lset with a switchable READ gate, modelling `CaLink::value()`'s
/// access-rights gate: while `readable` is false the value read reports
/// `None`, but `is_connected` stays `true` — the circuit is up, only READ
/// access was revoked. The cached snapshot is deliberately kept: the bug
/// being pinned is that a *stale but present* cache must not be served
/// once the server denies reads.
struct ReadGateLset {
    readable: parking_lot::Mutex<bool>,
    cached: EpicsValue,
}

impl ReadGateLset {
    fn new(v: f64) -> Arc<Self> {
        Arc::new(Self {
            readable: parking_lot::Mutex::new(true),
            cached: EpicsValue::Double(v),
        })
    }

    fn set_readable(&self, readable: bool) {
        *self.readable.lock() = readable;
    }

    fn readable(&self) -> bool {
        *self.readable.lock()
    }
}

#[async_trait::async_trait]
impl LinkSet for ReadGateLset {
    /// Always up: C's lset `isConnected` (`dbCa.c:604-612`) returns
    /// `pca->isConnected` alone — READ-access loss does not disconnect.
    fn is_connected(&self, _: &str) -> bool {
        true
    }

    async fn get_value(&self, _: &str) -> Option<EpicsValue> {
        self.readable().then(|| self.cached.clone())
    }

    fn get_cached_value(&self, _: &str) -> Option<EpicsValue> {
        self.readable().then(|| self.cached.clone())
    }

    async fn connect_link(&self, _: &str) {}
}

fn alarm_of(db: &PvDatabase, name: &str) -> (u16, u16) {
    let rec = db.get_record(name).expect("record exists");
    let inst = rec.read();
    (inst.common.stat, inst.common.sevr as u16)
}

async fn holder_db(lset: Arc<ReadGateLset>) -> PvDatabase {
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

/// The gate itself: with read access held, a dispatch pulls the upstream
/// value in with no alarm; with read access revoked — circuit still up —
/// the very same dispatch commits LINK/INVALID and leaves VAL where it
/// was. This is the dispatch C's `accessRightsCallback` adds
/// (`dbCa.c:1032-1037`) and that calink did not: without it the holder is
/// Passive, is never processed again, and reports its last good value
/// with SEVR=0 for as long as the server denies reads.
#[epics_macros_rs::epics_test]
async fn a_dispatch_on_a_read_denied_link_commits_link_invalid() {
    use epics_base_rs::server::recgbl::alarm_status::LINK_ALARM;

    let lset = ReadGateLset::new(42.5);
    let db = holder_db(lset.clone()).await;

    // Rights fully held: the CP dispatch is the ordinary monitor-event
    // path. This is also the write-only-loss outcome — C dispatches on
    // write loss too (`dbCa.c:1029`), but `dbCaGetLink` does not consult
    // `hasWriteAccess`, so the dispatched holder reads a good value and
    // lands no alarm.
    db.dispatch_external_cp_targets("UP:PV");
    assert_eq!(
        db.get_pv("HOLDER").ok(),
        Some(EpicsValue::Double(42.5)),
        "a readable CP link must deliver its value on dispatch"
    );
    assert_eq!(
        alarm_of(&db, "HOLDER"),
        (0, 0),
        "a readable CP link must leave the holder in NO_ALARM"
    );

    // READ access revoked, circuit still up.
    lset.set_readable(false);
    db.dispatch_external_cp_targets("UP:PV");

    assert_eq!(
        alarm_of(&db, "HOLDER"),
        (LINK_ALARM, 3),
        "a read-denied CP link must commit LINK_ALARM / INVALID_ALARM on \
         its holder even though the circuit is up"
    );
    assert_eq!(
        db.get_pv("HOLDER").ok(),
        Some(EpicsValue::Double(42.5)),
        "a failed link read must leave VAL untouched (C `dbGetLink` returns \
         -1 before writing the destination)"
    );
}

/// Recovery: C dispatches NOTHING on a full rights regain
/// (`dbCa.c:1029` `goto done`), so the alarm clears on the next monitor
/// event — modelled here as the next dispatch after the gate reopens.
#[epics_macros_rs::epics_test]
async fn the_link_alarm_clears_on_the_next_event_after_read_access_returns() {
    let lset = ReadGateLset::new(7.25);
    let db = holder_db(lset.clone()).await;

    lset.set_readable(false);
    db.dispatch_external_cp_targets("UP:PV");
    assert_eq!(
        alarm_of(&db, "HOLDER").1,
        3,
        "read denial must raise INVALID"
    );

    lset.set_readable(true);
    db.dispatch_external_cp_targets("UP:PV");
    assert_eq!(
        alarm_of(&db, "HOLDER"),
        (0, 0),
        "the holder must return to NO_ALARM once the link reads again"
    );
    assert_eq!(db.get_pv("HOLDER").ok(), Some(EpicsValue::Double(7.25)));
}
