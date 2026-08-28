//! Trouble with the `.savB` backup must say so on the error log.
//!
//! A backup artefact write is the one thing a save set does that nothing
//! else looks at: its `io::Result` used to reach the console from
//! nowhere, so a set could stop producing backups for as long as the
//! destination stayed unwritable while every visible sign said it was
//! saving normally.
//!
//! Both shapes are covered here: the failure that aborts the cycle, and
//! the bad backup the cycle deliberately survives after replacing it —
//! C prints for both (`save_restore.c` `write_save_file` at
//! `R6-0-20-g186f467`).

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

/// A `.savB` that was bad: the cycle replaces it and succeeds, so the
/// log line is the only trace there has ever been that a backup
/// generation was lost.
#[epics_macros_rs::epics_test]
async fn a_bad_backup_is_announced_even_though_the_save_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let sav = dir.path().join("set.sav");
    epics_base_rs::runtime::fs::write(&sav, "# autosave-rs V1.0\nTEMP 1.0\n<END>\n")
        .await
        .unwrap();
    // Present but truncated: no `<END>`, which is what C's `check_file`
    // calls `BS_BAD` and what `find_best_save_file` would refuse.
    epics_base_rs::runtime::fs::write(dir.path().join("set.savB"), "TEMP 1.0\n")
        .await
        .unwrap();

    let db = one_pv_db().await;
    let mgr = one_pv_manager(sav).await.build().await;

    let (_guard, log) = capture_errlog();
    assert_eq!(
        mgr.manual_save("set", &db)
            .await
            .expect("a bad backup is replaced, not a reason to fail the cycle"),
        1
    );

    let text = log.contents();
    assert!(
        text.contains("set.savB") && text.contains("bad or not found"),
        "the replaced backup must reach the error log; got: {text:?}"
    );
    let kept: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("set.savB_SBAD_"))
        })
        .collect();
    assert_eq!(
        kept.len(),
        1,
        "the corrupt backup must be kept for diagnosis"
    );
}
