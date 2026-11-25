//! C `myFileCopy` (dbrestore.c:169-219) names the file it could not open on
//! the error log, through `ERRLOG`'s `__FILE__:__func__` prefix:
//!
//! ```text
//! ../dbrestore.c:myFileCopy Can't open file '/dir/set.savB'
//! ```

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ao::AoRecord;
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct CaptureBuf(Arc<Mutex<Vec<u8>>>);

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

/// Thread-local capture of the errlog sink; `#[epics_test]` runs the body
/// on one current-thread runtime.
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

/// The `.savB` copy after a good save, onto a destination that cannot be
/// opened.
#[epics_macros_rs::epics_test]
async fn an_unopenable_destination_is_named_as_c() {
    let dir = tempfile::tempdir().unwrap();
    let good = "# autosave-rs V1.0\nTEMP 1.0\n<END>\n";
    std::fs::write(dir.path().join("set.sav"), good).unwrap();
    std::fs::write(dir.path().join("set.savB"), good).unwrap();
    // The copy is published through `set.savB.tmp`.
    let tmp = dir.path().join("set.savB.tmp");
    std::fs::create_dir(&tmp).unwrap();
    std::fs::write(tmp.join("x"), "").unwrap();

    let db = Arc::new(PvDatabase::new());
    db.add_record("TEMP", Box::new(AoRecord::new(25.5)))
        .await
        .unwrap();
    let mgr = AutosaveBuilder::new()
        .add_set(
            SaveSetConfig {
                name: "set".into(),
                save_path: dir.path().join("set.sav"),
                request_file: None,
                request_pvs: vec!["TEMP".into()],
                backup: BackupConfig { enable_savb: true },
                macros: HashMap::new(),
                macro_string: None,
                search_paths: Vec::new(),
            },
            SaveStrategy::Manual,
        )
        .build()
        .await;

    let (_guard, log) = capture_errlog();
    assert!(mgr.manual_save("set", &db).await.is_err());
    let text = String::from_utf8_lossy(&log.0.lock().unwrap()).into_owned();
    let want = format!(
        "../dbrestore.c:myFileCopy Can't open file '{}'",
        dir.path().join("set.savB").display()
    );
    assert!(text.contains(&want), "{text:?}");
    assert!(!text.contains("autosave: backup"), "{text:?}");
}
