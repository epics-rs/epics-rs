//! A bare `dbior` on `register_app`'s IOC against C streamApp's
//! (`tests/oracle/dbior/bare.out`, see `run-bare.sh`): every driver in
//! streamApp.dbd's include order, asyn.dbd's `drvAsyn` before stream.dbd's
//! `stream`, and asyn.dbd's `ai` rows before stream.dbd's in `devList`.

#[path = "dbior/harness.rs"]
mod harness;

use std::sync::Arc;

use epics_base_rs::server::ioc_app::{IocApplication, IocInitDecision};
use parking_lot::Mutex;

#[test]
fn bare_dbior_lists_drivers_in_stream_app_dbd_order() {
    epics_base_rs::runtime::task::test_block_on(async {
        let db = Arc::new(Mutex::new(None));
        let seen = db.clone();
        stream_rs::dev::register_app(IocApplication::new())
            .port(0)
            .before_ioc_init(|| IocInitDecision::run(false))
            .run(move |config| async move {
                *seen.lock() = Some(config.db);
                Ok(())
            })
            .await
            .unwrap();
        let db = db.lock().take().unwrap();
        harness::assert_levels(&db, "bare.out", "\"\"", &["0", "1"]).await;
        let dump = harness::shell(&db, "dbDumpDevice pdbbase ai").await;
        let names: Vec<&str> = std::str::from_utf8(&dump)
            .unwrap()
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix("device name:"))
            .map(str::trim)
            .collect();
        let c = std::fs::read_to_string(format!("{}/bare-ai.devices", harness::DIR)).unwrap();
        assert_eq!(names, c.lines().collect::<Vec<_>>());
    });
}
