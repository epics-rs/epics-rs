//! C's one save_restore task (save_restore.c:782-1260): each method's
//! timer or CA monitor sets a bit in the list's `save_state`, and a pass,
//! at most once per `MIN_DELAY` (1 s), turns the bits into one save.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::{AutosaveBuilder, AutosaveManager};
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

fn config(dir: &std::path::Path) -> SaveSetConfig {
    SaveSetConfig {
        name: "task".into(),
        save_path: dir.join("task.sav"),
        request_file: None,
        request_pvs: vec!["TEMP".into()],
        backup: BackupConfig { enable_savb: false },
        macros: HashMap::new(),
        macro_string: String::new(),
        search_paths: Vec::new(),
    }
}

async fn db() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    for name in ["TEMP", "TRIG"] {
        db.add_record(name, Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();
    }
    db
}

async fn start(
    dir: &std::path::Path,
    db: &Arc<PvDatabase>,
    methods: Vec<SaveStrategy>,
) -> Arc<AutosaveManager> {
    let mut builder = AutosaveBuilder::new();
    for m in methods {
        builder = builder.add_set(config(dir), m);
    }
    let mgr = Arc::new(builder.build().await);
    let reactor = epics_base_rs::runtime::task::Reactor::current().unwrap();
    // Detached; `shutdown` ends the task.
    drop(mgr.clone().start(&reactor, db.clone()));
    mgr
}

fn saves(mgr: &AutosaveManager) -> u64 {
    mgr.sets()[0].0.stats().save_count.load(Ordering::Relaxed)
}

/// A CA put: `dbPutField`, so the record processes and posts.
async fn caput(db: &PvDatabase, name: &str, v: f64) {
    db.put_record_field_from_ca_no_notify(name, "VAL", EpicsValue::Double(v))
        .await
        .unwrap();
}

async fn sleep(ms: u64) {
    epics_base_rs::runtime::task::sleep(Duration::from_millis(ms)).await;
}

async fn wait_for_saves(mgr: &AutosaveManager, n: u64, ms: u64) -> u64 {
    for _ in 0..ms / 20 {
        if saves(mgr) >= n {
            break;
        }
        sleep(20).await;
    }
    saves(mgr)
}

/// `ca_add_event` delivers the trigger's value when the list is enabled,
/// and `triggered_save` saves on it: no change to the trigger is needed.
#[epics_macros_rs::epics_test]
async fn a_triggered_set_saves_on_the_triggers_first_event() {
    let dir = tempfile::tempdir().unwrap();
    let db = db().await;
    let mgr = start(
        dir.path(),
        &db,
        vec![SaveStrategy::Triggered {
            trigger_pv: "TRIG".into(),
        }],
    )
    .await;
    assert_eq!(wait_for_saves(&mgr, 1, 3000).await, 1);
    mgr.shutdown();
}

/// `on_change_save` counts events, not differences: a member that goes
/// A, B and back to A inside one window has changed.
#[epics_macros_rs::epics_test]
async fn a_member_that_returns_to_its_value_within_a_window_still_saves() {
    let dir = tempfile::tempdir().unwrap();
    let db = db().await;
    let mgr = start(
        dir.path(),
        &db,
        vec![SaveStrategy::OnChange {
            min_interval: Duration::from_millis(20),
        }],
    )
    .await;
    // The first window saves the values the monitors delivered at enable.
    assert_eq!(wait_for_saves(&mgr, 1, 3000).await, 1);
    caput(&db, "TEMP", 5.0).await;
    caput(&db, "TEMP", 0.0).await;
    assert_eq!(wait_for_saves(&mgr, 2, 3000).await, 2);
    // And with no event, no further save.
    sleep(2200).await;
    assert_eq!(saves(&mgr), 2);
    mgr.shutdown();
}

/// Two methods firing in the same pass cost the list one save: the bits
/// share one `save_state`.
#[epics_macros_rs::epics_test]
async fn methods_due_in_one_pass_save_the_set_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = db().await;
    let mgr = start(
        dir.path(),
        &db,
        vec![
            SaveStrategy::Periodic {
                interval: Duration::from_millis(20),
            },
            SaveStrategy::Triggered {
                trigger_pv: "TRIG".into(),
            },
        ],
    )
    .await;
    let begun = std::time::Instant::now();
    let mut v = 0.0;
    while begun.elapsed() < Duration::from_millis(3500) {
        v += 1.0;
        caput(&db, "TRIG", v).await;
        sleep(50).await;
    }
    let passes = begun.elapsed().as_secs() + 1;
    let n = saves(&mgr);
    assert!(
        (1..=passes).contains(&n),
        "{n} saves in {passes} passes: more than one save a pass"
    );
    mgr.shutdown();
}
