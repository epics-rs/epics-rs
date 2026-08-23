//! A backup copy that did not land must say so on the error log.
//!
//! `publish_copy` is the only place a backup artefact is written, and its
//! `io::Result` used to reach the console from nowhere: the rotation
//! callers turned it into the save set's error status, and the
//! first-cycle `.savB` seed dropped it entirely with `let _ =`. A save
//! set could therefore stop producing backups for as long as the
//! destination stayed unwritable while every visible sign said the set
//! was saving normally.
//!
//! Both shapes are covered here: the failure that aborts the cycle, and
//! the one the cycle deliberately survives.
//!
//! No C reference: synApps `save_restore.c` is not present on this
//! machine, so these pin the port's own stated invariant.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct CaptureBuf(Arc<Mutex<Vec<u8>>>);

impl CaptureBuf {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for CaptureBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CaptureBuf {
    type Writer = CaptureBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Thread-local capture of the `errlog` sink. `#[epics_test]` drives the
/// body on one current-thread runtime, and `publish_copy` logs after its
/// blocking hand-off has been awaited, so the line is emitted on this
/// thread.
fn capture_errlog() -> (DefaultGuard, CaptureBuf) {
    let buf = CaptureBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .without_time()
        .finish();
    (tracing::subscriber::set_default(subscriber), buf)
}

fn savb_only() -> BackupConfig {
    BackupConfig {
        enable_savb: true,
        num_seq_files: 0,
        seq_period: Duration::from_secs(60),
        enable_dated: false,
        dated_interval: Duration::from_secs(3600),
    }
}

async fn one_pv_manager(save_path: std::path::PathBuf) -> AutosaveBuilder {
    AutosaveBuilder::new().add_set(SaveSetConfig {
        name: "set".into(),
        save_path,
        strategy: SaveStrategy::Manual,
        request_file: None,
        request_pvs: vec!["TEMP".into()],
        backup: savb_only(),
        macros: HashMap::new(),
        search_paths: Vec::new(),
    })
}

async fn one_pv_db() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    db.add_record("TEMP", Box::new(AoRecord::new(25.5)))
        .await
        .unwrap();
    db
}

/// The rotation copy: a `.savB` that cannot be written aborts the cycle,
/// and the console names the file rather than leaving the operator to
/// notice a set stuck in `error`.
#[epics_macros_rs::epics_test]
async fn a_failed_rotation_copy_names_the_backup_it_could_not_write() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("set.sav");
    // A valid previous generation, so `rotate_backups` gets past its
    // "nothing to preserve" early return.
    epics_base_rs::runtime::fs::write(&sav, "# autosave-rs V1.0\nTEMP 1.0\n<END>\n")
        .await
        .unwrap();
    // The destination is a directory, so the rename onto it always fails.
    std::fs::create_dir(dir.path().join("set.savB")).unwrap();

    let db = one_pv_db().await;
    let mgr = one_pv_manager(sav).await.build().await;

    let (_guard, log) = capture_errlog();
    assert!(
        mgr.manual_save("set", &db).await.is_err(),
        "a rotation that could not preserve the .sav must abort the cycle"
    );

    let text = log.contents();
    assert!(
        text.contains("set.savB") && text.contains("not written"),
        "the failed backup must be on the error log; got: {text:?}"
    );
}

/// The first-cycle seed: the `.sav` is already on disk, so the cycle
/// succeeds and the seed failure has no error status to be read from —
/// the log line is the only trace there has ever been.
#[epics_macros_rs::epics_test]
async fn a_failed_savb_seed_is_announced_even_though_the_save_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    // No prior `.sav`: `rotate_backups` returns early and the seed at the
    // end of the cycle is the only backup write. The seed runs only when
    // `.savB` is absent, so the copy is blocked at the temp file
    // `publish_copy` stages it through instead of at the destination.
    std::fs::create_dir(dir.path().join("set.savB.tmp")).unwrap();

    let db = one_pv_db().await;
    let mgr = one_pv_manager(dir.path().join("set.sav"))
        .await
        .build()
        .await;

    let (_guard, log) = capture_errlog();
    assert_eq!(
        mgr.manual_save("set", &db)
            .await
            .expect("the .sav write itself must still succeed"),
        1
    );

    let text = log.contents();
    assert!(
        text.contains("set.savB") && text.contains("not written"),
        "the seed failure must still reach the error log; got: {text:?}"
    );
}
