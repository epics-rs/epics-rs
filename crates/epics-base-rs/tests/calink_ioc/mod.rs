//! A CP/CPP link is a CA link whatever its target (`dbLink.c:118-127`), served
//! for a local name by the database's own `ca` entry with no lset installed.
//! Its processing runs on the dbCa worker, so a presence claim waits.

#![allow(dead_code)]

use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;

/// iocInit's link open passes followed by C `dbCaRun`.
pub async fn connect(db: &PvDatabase) {
    db.setup_cp_links().await;
    db.setup_external_link_opens().await;
    db.process_cp_links_on_connect().await;
}

/// Wait for a fact the dbCa worker makes true, failing after the fact budget.
pub async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(2)).await;
    }
}
