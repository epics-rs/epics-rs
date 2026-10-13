//! Once the manager is gone, no save starts. A set task that is already
//! waiting on the set lock behind a slow save must not take the lock and
//! save after the manager dropped (there is no C counterpart: save_restore
//! never drops a running task).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

const PERIOD: Duration = Duration::from_millis(20);

async fn sleep(d: Duration) {
    epics_base_rs::runtime::task::sleep(d).await;
}

/// The test holds the set lock the way a slow save does, lets the strategy's
/// pass queue behind it, drops the manager, then lets the lock go.
async fn saves_after_the_manager_drops(strategy: SaveStrategy, change: bool) -> (u64, bool) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEMP", Box::new(AoRecord::new(25.5)))
        .await
        .unwrap();
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .add_set(SaveSetConfig {
                name: "stop".into(),
                save_path: dir.path().join("stop.sav"),
                strategy,
                request_file: None,
                request_pvs: vec!["TEMP".into()],
                backup: BackupConfig {
                    enable_savb: false,
                    num_seq_files: 0,
                    seq_period: Duration::from_secs(60),
                    enable_dated: false,
                    dated_interval: Duration::from_secs(3600),
                },
                macros: HashMap::new(),
                search_paths: Vec::new(),
            })
            .build()
            .await,
    );
    let (set, lock) = mgr.sets()[0].clone();
    let handle = mgr.clone().start(
        &epics_base_rs::runtime::task::Reactor::current()
            .expect("the test driver enters an executor"),
        db.clone(),
    );
    if change {
        sleep(Duration::from_millis(150)).await; // baseline poll
    }
    let guard = lock.clone().lock_owned().await;
    if change {
        db.put_pv_no_process("TEMP", EpicsValue::Double(30.0))
            .await
            .unwrap();
    }
    sleep(Duration::from_millis(200)).await; // the pass is now behind the lock
    handle.abort();
    drop(mgr);
    drop(guard);

    let start = Instant::now();
    let ended = loop {
        if Arc::strong_count(&set) == 1 {
            break true;
        }
        if start.elapsed() > Duration::from_secs(5) {
            break false;
        }
        sleep(Duration::from_millis(10)).await;
    };
    (set.stats().save_count.load(Ordering::Relaxed), ended)
}

#[epics_macros_rs::epics_test]
async fn a_periodic_pass_queued_behind_a_slow_save_does_not_save_after_the_drop() {
    let (saves, ended) =
        saves_after_the_manager_drops(SaveStrategy::Periodic { interval: PERIOD }, false).await;
    assert_eq!(saves, 0, "a save started after the manager dropped");
    assert!(ended, "the set task outlived its manager");
}

#[epics_macros_rs::epics_test]
async fn an_onchange_save_queued_behind_a_slow_save_does_not_save_after_the_drop() {
    let (saves, ended) = saves_after_the_manager_drops(
        SaveStrategy::OnChange {
            min_interval: PERIOD,
            float_epsilon: 0.0,
        },
        true,
    )
    .await;
    assert_eq!(saves, 0, "a save started after the manager dropped");
    assert!(ended, "the set task outlived its manager");
}
