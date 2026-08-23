//! R3-CA-1: the change-detection watermark must not advance past a
//! save that failed, or the changed value is never written and the
//! next IOC restart restores the stale one.
//!
//! Boundary cases, not scenarios: save succeeds, save fails, save
//! fails then succeeds, and the same pair on each `Triggered` edge.
//!
//! Failure is injected the way `test_save_once_failure_updates_stats`
//! does it — a `save_path` under a directory that does not exist — so
//! the write, and only the write, fails. This is a correctness
//! property of this implementation; no synApps autosave source is on
//! this machine, so nothing here asserts C parity.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::{AutosaveBuilder, AutosaveManager};
use epics_base_rs::server::autosave::save_file::read_save_file;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy, TriggerMode};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

const POLL: Duration = Duration::from_millis(20);
/// Observation window for the negative half of a case — that nothing
/// further is saved. A loaded machine polls fewer times in it, never
/// more, so widening it cannot turn a pass into a failure.
const QUIET: Duration = Duration::from_millis(200);

fn quick_backup() -> BackupConfig {
    BackupConfig {
        enable_savb: false,
        num_seq_files: 0,
        seq_period: Duration::from_secs(60),
        enable_dated: false,
        dated_interval: Duration::from_secs(3600),
    }
}

async fn setup_db() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEMP", Box::new(AoRecord::new(25.5)))
        .await
        .unwrap();
    db.add_record("TRIG", Box::new(AoRecord::new(0.0)))
        .await
        .unwrap();
    db
}

async fn build(save_path: std::path::PathBuf, strategy: SaveStrategy) -> Arc<AutosaveManager> {
    Arc::new(
        AutosaveBuilder::new()
            .add_set(SaveSetConfig {
                name: "wm".into(),
                save_path,
                strategy,
                request_file: None,
                request_pvs: vec!["TEMP".into()],
                backup: quick_backup(),
                macros: HashMap::new(),
                search_paths: Vec::new(),
            })
            .build()
            .await,
    )
}

fn onchange() -> SaveStrategy {
    SaveStrategy::OnChange {
        min_interval: POLL,
        float_epsilon: 0.0,
    }
}

fn triggered(mode: TriggerMode) -> SaveStrategy {
    SaveStrategy::Triggered {
        trigger_pv: "TRIG".into(),
        mode,
        poll_interval: POLL,
    }
}

async fn sleep(d: Duration) {
    epics_base_rs::runtime::task::sleep(d).await;
}

fn saves(mgr: &AutosaveManager) -> u64 {
    mgr.sets()[0].0.stats().save_count.load(Ordering::Relaxed)
}

fn errors(mgr: &AutosaveManager) -> u64 {
    mgr.sets()[0].0.stats().error_count.load(Ordering::Relaxed)
}

/// Wait for a count to reach `at_least`, rather than sleeping a fixed
/// budget, so a loaded machine slows the test down instead of failing
/// it. Returns the count reached, so the caller still asserts.
async fn wait_for(mgr: &AutosaveManager, count: fn(&AutosaveManager) -> u64, at_least: u64) -> u64 {
    for _ in 0..400 {
        let n = count(mgr);
        if n >= at_least {
            return n;
        }
        sleep(Duration::from_millis(10)).await;
    }
    count(mgr)
}

/// Boundary: the save succeeds. The watermark must advance, so the
/// unchanged set is not saved again on every later poll.
#[epics_macros_rs::epics_test]
async fn onchange_successful_save_advances_the_watermark() {
    let dir = tempfile::tempdir().unwrap();
    let mgr = build(dir.path().join("oc.sav"), onchange()).await;
    let db = setup_db().await;
    let handle = mgr.clone().start(db.clone());

    sleep(QUIET).await; // baseline poll, nothing changed yet
    assert_eq!(saves(&mgr), 0, "an unchanged set must not be saved");

    db.put_pv_no_process("TEMP", EpicsValue::Double(30.0))
        .await
        .unwrap();
    assert_eq!(wait_for(&mgr, saves, 1).await, 1);
    sleep(QUIET).await;

    let (saved, failed) = (saves(&mgr), errors(&mgr));
    mgr.shutdown();
    let _ = handle.await;

    assert_eq!(saved, 1, "no re-save while the set is unchanged");
    assert_eq!(failed, 0);
}

/// Boundary: the save fails. The watermark must stay put, so the very
/// next poll re-detects the same change and retries it.
#[epics_macros_rs::epics_test]
async fn onchange_failed_save_keeps_the_watermark_and_retries() {
    let dir = tempfile::tempdir().unwrap();
    // Parent directory absent -> every write_save_file_with_mode fails.
    let mgr = build(dir.path().join("absent/oc.sav"), onchange()).await;
    let db = setup_db().await;
    let handle = mgr.clone().start(db.clone());

    sleep(QUIET).await;
    db.put_pv_no_process("TEMP", EpicsValue::Double(30.0))
        .await
        .unwrap();
    let failed = wait_for(&mgr, errors, 2).await;

    let saved = saves(&mgr);
    mgr.shutdown();
    let _ = handle.await;

    assert_eq!(saved, 0, "no save can have succeeded");
    assert!(
        failed >= 2,
        "one PV change produced {failed} failed save(s): the watermark advanced \
         past the failure, so the change is never retried"
    );
}

/// Boundary: the save fails, then the obstruction clears. The changed
/// value must reach the `.sav` file without any further change to the
/// set.
#[epics_macros_rs::epics_test]
async fn onchange_change_survives_a_failed_save_and_lands_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("absent/oc.sav");
    let mgr = build(sav.clone(), onchange()).await;
    let db = setup_db().await;
    let handle = mgr.clone().start(db.clone());

    sleep(QUIET).await;
    db.put_pv_no_process("TEMP", EpicsValue::Double(30.0))
        .await
        .unwrap();
    assert!(wait_for(&mgr, errors, 1).await >= 1, "the save must fail");
    std::fs::create_dir(sav.parent().unwrap()).unwrap();
    let saved = wait_for(&mgr, saves, 1).await;

    mgr.shutdown();
    let _ = handle.await;

    assert_eq!(saved, 1, "the held change was never retried");
    let entries = read_save_file(&sav)
        .await
        .unwrap()
        .expect("the retried save must have written the file")
        .entries;
    let temp = entries.iter().find(|e| e.pv_name == "TEMP").unwrap();
    assert!(
        (temp.value.parse::<f64>().unwrap() - 30.0).abs() < 1e-10,
        "stale value on disk: {}",
        temp.value
    );
}

/// Boundary: `AnyChange`, the save fails on the trigger edge.
/// `last_value` is this mode's watermark; holding it is what makes the
/// next poll still see the change, without the trigger PV moving again.
#[epics_macros_rs::epics_test]
async fn triggered_any_change_retries_after_a_failed_save() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("absent/trig.sav");
    let mgr = build(sav.clone(), triggered(TriggerMode::AnyChange)).await;
    let db = setup_db().await;
    let handle = mgr.clone().start(db.clone());

    sleep(QUIET).await; // baseline poll of TRIG
    db.put_pv_no_process("TRIG", EpicsValue::Double(1.0))
        .await
        .unwrap();
    let failed = wait_for(&mgr, errors, 2).await;
    std::fs::create_dir(sav.parent().unwrap()).unwrap();
    let saved = wait_for(&mgr, saves, 1).await;

    mgr.shutdown();
    let _ = handle.await;

    assert!(
        failed >= 2,
        "one trigger change produced {failed} failed save(s): `last_value` \
         advanced past the failure, so the next poll sees no change"
    );
    assert_eq!(saved, 1, "the trigger edge was lost with the save");
    assert!(sav.exists());
}

/// Boundary: `NonZero`, the save fails on the rising edge. `armed` is
/// the marker that loses the retry: cleared on the edge, the set is not
/// saved again until the trigger falls back to zero and rises anew.
#[epics_macros_rs::epics_test]
async fn triggered_non_zero_stays_armed_after_a_failed_save() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("absent/nz.sav");
    let mgr = build(sav.clone(), triggered(TriggerMode::NonZero)).await;
    let db = setup_db().await;
    let handle = mgr.clone().start(db.clone());

    sleep(QUIET).await; // baseline poll: TRIG is 0, armed
    db.put_pv_no_process("TRIG", EpicsValue::Double(1.0))
        .await
        .unwrap();
    let failed = wait_for(&mgr, errors, 2).await;
    // TRIG never returns to 0, so only a held `armed` can save now.
    std::fs::create_dir(sav.parent().unwrap()).unwrap();
    let saved = wait_for(&mgr, saves, 1).await;

    mgr.shutdown();
    let _ = handle.await;

    assert!(
        failed >= 2,
        "one rising edge produced {failed} failed save(s): `armed` was cleared \
         past the failure, so the save is not retried on the level"
    );
    assert_eq!(saved, 1, "the rising edge was lost with the save");
    assert!(sav.exists());
}
