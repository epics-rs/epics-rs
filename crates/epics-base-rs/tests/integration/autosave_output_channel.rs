//! C autosave's status and error lines go to the errlog (`ERRLOG`,
//! `errlogPrintf`), and a command's listing to the shell's stdout
//! (`printf`), which an iocsh `>` redirects. None of them is a bare
//! write to stderr.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::autosave::AutosaveStartupConfig;
use epics_base_rs::server::autosave::backup::BackupConfig;
use epics_base_rs::server::autosave::manager::AutosaveBuilder;
use epics_base_rs::server::autosave::save_set::{SaveSetConfig, SaveStrategy};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::IocShell;
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

impl CaptureBuf {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// The errlog's lines, from every thread: the boot hooks and the shell run
/// off the test's own.
fn capture_errlog() -> CaptureBuf {
    let buf = CaptureBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .without_time()
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    buf
}

fn set_config(dir: &std::path::Path) -> SaveSetConfig {
    SaveSetConfig {
        name: "s".into(),
        save_path: dir.join("s.sav"),
        request_file: None,
        request_pvs: vec!["T:C.CALC".into()],
        backup: BackupConfig { enable_savb: false },
        macros: HashMap::new(),
        macro_string: None,
        search_paths: Vec::new(),
    }
}

/// A boot restore that cannot read its file (dbrestore.c:827-828).
#[epics_macros_rs::epics_test]
async fn a_failed_boot_restore_is_on_the_errlog() {
    let log = capture_errlog();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.sav"), "# R1.9\nT:A 1\n").unwrap();
    IocApplication::new()
        .port(0)
        .autosave_startup(Arc::new(Mutex::new(AutosaveStartupConfig::new())))
        .startup_line(&format!("set_savefile_path(\"{}\")", dir.path().display()))
        .startup_line("set_pass0_restoreFile(\"s.sav\")")
        .run(|_| async { Ok(()) })
        .await
        .unwrap();
    assert!(
        log.text()
            .contains("../dbrestore.c:reboot_restore Can't open save file."),
        "{:?}",
        log.text()
    );
}

/// A value the record refuses (dbrestore.c:982-983).
#[epics_macros_rs::epics_test]
async fn a_failed_restore_put_is_on_the_errlog() {
    let log = capture_errlog();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.sav"), "# h\nT:C.CALC 1+(\n<END>\n").unwrap();
    let db = Arc::new(PvDatabase::new());
    db.add_record(
        "T:C",
        Box::new(epics_base_rs::server::records::calc::CalcRecord::default()),
    )
    .await
    .unwrap();
    let result = epics_base_rs::server::autosave::save_set::restore_from_entries(
        &db,
        &dir.path().join("s.sav"),
    )
    .await
    .unwrap();
    assert_eq!(result.failed_puts.len(), 1, "{result:?}");
    assert!(
        log.text()
            .contains("../dbrestore.c:reboot_restore restore for PV 'T:C.CALC' failed"),
        "{:?}",
        log.text()
    );
}

/// `save_restoreShow`'s listing on the shell's output, and `fdbrestore`'s refusal on
/// the errlog.
#[epics_macros_rs::epics_test]
async fn a_listing_is_the_shells_and_an_error_the_errlogs() {
    let log = capture_errlog();
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(PvDatabase::new());
    let mgr = Arc::new(
        AutosaveBuilder::new()
            .add_set(set_config(dir.path()), SaveStrategy::Manual)
            .build()
            .await,
    );
    let _task = mgr.clone().start(db.clone());
    let out = dir.path().join("out.txt");
    let line = format!("save_restoreShow > \"{}\"", out.display());
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let shell_mgr = mgr.clone();
    std::thread::spawn(move || {
        epics_base_rs::runtime::task::set_thread_ok_to_block(true);
        let shell = IocShell::new(db, bridge);
        let mut cfg = AutosaveStartupConfig::new();
        cfg.manager = Some(shell_mgr);
        let holder = Arc::new(Mutex::new(cfg));
        for cmd in AutosaveStartupConfig::register_startup_commands(holder) {
            shell.register(cmd);
        }
        shell.execute_line(&line).unwrap();
        shell.execute_line("fdbrestore nosuch").unwrap();
    })
    .join()
    .unwrap();
    mgr.ops_served().await;
    mgr.shutdown();
    let listed = std::fs::read_to_string(&out).unwrap();
    assert!(listed.contains("\n  s: \n"), "{listed:?}");
    assert!(
        log.text()
            .contains("../save_restore.c:do_manual_restore Can't open save file."),
        "{:?}",
        log.text()
    );
}
