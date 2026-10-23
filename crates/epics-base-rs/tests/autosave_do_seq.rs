//! C `do_seq` (save_restore.c:2146-2215) is the save task's, not the
//! save's: a pass copies the `.sav` it has just written to the next
//! `.savN` once a `.sav` exists and the last sequence file is older than
//! `save_restoreSeqPeriodInSeconds` (:1007-1012), the list's creation
//! counting as the first. A save on its own never writes one.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_file::read_save_file;
use epics_base_rs::server::autosave::save_set::{SaveSet, SaveSetConfig, SaveStrategy};
use epics_base_rs::server::autosave::settings::SrSettings;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

fn config(dir: &Path) -> SaveSetConfig {
    SaveSetConfig {
        name: "s".into(),
        save_path: dir.join("s.sav"),
        request_file: None,
        request_pvs: vec!["TEMP".into()],
        // C's do_backups, without which the task writes no sequence file
        // (save_restore.c:1008).
        backup: BackupConfig { enable_savb: true },
        macros: HashMap::new(),
        macro_string: None,
        search_paths: Vec::new(),
    }
}

/// `save_restoreSeqPeriodInSeconds` at 1, which C holds to 10.
fn shortest_seq_period() -> Arc<SrSettings> {
    let settings = Arc::new(SrSettings::default());
    settings.set_seq_period_in_seconds(1);
    settings
}

async fn db() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEMP", Box::new(AoRecord::new(1.0)))
        .await
        .unwrap();
    db
}

async fn set(dir: &Path) -> SaveSet {
    SaveSet::new(config(dir)).await.unwrap()
}

async fn put(db: &PvDatabase, v: f64) {
    db.put_record_field_from_ca_no_notify("TEMP", "VAL", EpicsValue::Double(v))
        .await
        .unwrap();
}

async fn value(path: &Path) -> f64 {
    read_save_file(path).await.unwrap().unwrap().entries[0]
        .value
        .parse()
        .unwrap()
}

/// The rs save rotated `.sav` into `.sav0` before every write once a
/// `.sav` existed, so a second save left a sequence file at once.
#[epics_macros_rs::epics_test]
async fn a_save_writes_no_sequence_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = db().await;
    let set = set(dir.path()).await;
    set.save_once(&db).await.unwrap();
    set.save_once(&db).await.unwrap();
    for i in 0..3 {
        assert!(!dir.path().join(format!("s.sav{i}")).exists());
    }
}

/// Each `do_seq` copies the `.sav` as it now stands into the next slot.
#[epics_macros_rs::epics_test]
async fn do_seq_copies_the_current_sav_round_robin() {
    let dir = tempfile::tempdir().unwrap();
    let db = db().await;
    let set = set(dir.path()).await;
    for (i, v) in [(0, 2.0), (1, 3.0), (2, 4.0), (0, 5.0)] {
        put(&db, v).await;
        set.save_once(&db).await.unwrap();
        set.do_seq(&db).await;
        assert_eq!(value(&dir.path().join(format!("s.sav{i}"))).await, v);
        assert_eq!(set.recent().unwrap().1, format!("Wrote 's.sav{i}'"));
    }
}

/// The first slot is the first one missing, else the oldest
/// (save_restore.c:2162-2179).
#[epics_macros_rs::epics_test]
async fn the_first_slot_is_the_missing_or_oldest_one() {
    let stamp = |path: &Path, secs_ago: u64| {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(secs_ago))
            .unwrap();
    };
    let db = db().await;

    let dir = tempfile::tempdir().unwrap();
    let set1 = set(dir.path()).await;
    set1.save_once(&db).await.unwrap();
    for (i, age) in [(0, 10), (1, 30), (2, 20)] {
        let slot = dir.path().join(format!("s.sav{i}"));
        std::fs::copy(dir.path().join("s.sav"), &slot).unwrap();
        stamp(&slot, age);
    }
    set1.do_seq(&db).await;
    assert_eq!(set1.recent().unwrap().1, "Wrote 's.sav1'");

    std::fs::remove_file(dir.path().join("s.sav2")).unwrap();
    let set2 = set(dir.path()).await;
    set2.do_seq(&db).await;
    assert_eq!(set2.recent().unwrap().1, "Wrote 's.sav2'");
}

/// A copy that cannot be made is written instead from the values the last
/// save fetched, which C's channels hold; nothing is fetched
/// (save_restore.c:2192-2207).
#[epics_macros_rs::epics_test]
async fn a_failed_copy_writes_the_slot_from_the_last_fetch() {
    let dir = tempfile::tempdir().unwrap();
    let db = db().await;
    let set = set(dir.path()).await;
    set.save_once(&db).await.unwrap();
    // The copy is published through `s.sav0.tmp`.
    let tmp = dir.path().join("s.sav0.tmp");
    std::fs::create_dir(&tmp).unwrap();
    std::fs::write(tmp.join("x"), "").unwrap();
    put(&db, 7.0).await;
    set.do_seq(&db).await;
    assert_eq!(value(&dir.path().join("s.sav0")).await, 1.0);
    assert_eq!(set.recent().unwrap().1, "Wrote 's.sav0'");
}

/// A `.sav` gone by the time of `do_seq` is written again from the last
/// fetch, as C's `write_save_file` writes its channels (:2181-2183).
#[epics_macros_rs::epics_test]
async fn a_missing_sav_is_written_from_the_last_fetch() {
    let dir = tempfile::tempdir().unwrap();
    let db = db().await;
    let set = set(dir.path()).await;
    set.save_once(&db).await.unwrap();
    std::fs::remove_file(dir.path().join("s.sav")).unwrap();
    put(&db, 7.0).await;
    set.do_seq(&db).await;
    assert_eq!(value(&dir.path().join("s.sav")).await, 1.0);
    assert_eq!(value(&dir.path().join("s.sav0")).await, 1.0);
}

/// A slot that can be neither copied nor written keeps its number for the
/// next `do_seq`.
#[epics_macros_rs::epics_test]
async fn a_slot_not_written_is_retried() {
    let dir = tempfile::tempdir().unwrap();
    let db = db().await;
    let set = set(dir.path()).await;
    set.save_once(&db).await.unwrap();
    set.do_seq(&db).await;
    let slot1 = dir.path().join("s.sav1");
    std::fs::create_dir(&slot1).unwrap();
    set.do_seq(&db).await;
    assert_eq!(set.recent().unwrap().1, "Can't write 's.sav1'");
    std::fs::remove_dir(&slot1).unwrap();
    set.do_seq(&db).await;
    assert_eq!(set.recent().unwrap().1, "Wrote 's.sav1'");
    assert!(!dir.path().join("s.sav2").exists());
}

/// The task's gate: nothing before the period has passed since the list
/// was made, then one copy once a save exists. C holds the period to
/// 10 s and looks every half period (:905-909), so the first copy lands
/// on the 10 s or the 15 s look.
#[epics_macros_rs::epics_test]
async fn the_task_writes_the_first_sequence_file_after_the_period() {
    let dir = tempfile::tempdir().unwrap();
    let db = db().await;
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .settings(shortest_seq_period())
            .add_set(config(dir.path()), SaveStrategy::Manual)
            .build()
            .await,
    );
    let task = mgr.clone().start(db.clone());
    mgr.manual_save("s", &db).await.unwrap();
    let slot0 = dir.path().join("s.sav0");
    sleep(Duration::from_secs(9)).await;
    assert!(!slot0.exists(), "the 5 s look is inside the period");
    for _ in 0..70 {
        if slot0.exists() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(slot0.exists());
    assert!(!dir.path().join("s.sav1").exists());
    mgr.shutdown();
    drop(task);
}

/// C sets `last_save_file` on every `write_save_file` that wrote its file
/// (save_restore.c:2110), a save under a `configName` too, and that is what
/// lets the task's `do_seq` run (:1009). With no `cfg.sav` before, the
/// copy after the write fails that save as a WARN (:2112-2124), which
/// still lets it run.
#[epics_macros_rs::epics_test]
async fn a_config_menu_save_lets_the_sequence_file_be_written() {
    let dir = tempfile::tempdir().unwrap();
    let db = db().await;
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .settings(shortest_seq_period())
            .add_set(config(dir.path()), SaveStrategy::Manual)
            .build()
            .await,
    );
    let task = mgr.clone().start(db.clone());
    let set = mgr.sets()[0].0.clone();
    let _ = set
        .save_to(set.get_channel_values(&db), &dir.path().join("cfg.sav"))
        .await;
    let slot0 = dir.path().join("s.sav0");
    for _ in 0..170 {
        if slot0.exists() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(slot0.exists());
    mgr.shutdown();
    drop(task);
}

/// `save_restoreSet_NumSeqFiles(0)` while a `do_seq` is under way: C's
/// `if (++n >= N) n = 0` (save_restore.c:2214) wraps to slot 0 instead of
/// dividing by the count.
#[epics_macros_rs::epics_test]
async fn a_zero_sequence_count_wraps_the_slot() {
    let dir = tempfile::tempdir().unwrap();
    let db = db().await;
    let settings = Arc::new(SrSettings::default());
    let set = SaveSet::new_with_settings(config(dir.path()), settings.clone())
        .await
        .unwrap();
    set.save_once(&db).await.unwrap();
    settings.set_num_seq_files(0);
    set.do_seq(&db).await;
    assert!(dir.path().join("s.sav0").exists());
}
