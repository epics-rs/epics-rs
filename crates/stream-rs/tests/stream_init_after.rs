//! `streamInit(1)`, which `finishDevSup` calls (iocInit.c:479), turns the
//! errors `streamInit(0)` enabled for the protocol files back to the
//! shell's `streamError`; and the registrar's commands reach the shell.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use epics_base_rs::server::ioc_app::{IocApplication, IocInitDecision};
use parking_lot::Mutex;
use stream_rs::error::STREAM_ERROR;

#[test]
fn stream_error_restored_after_init() {
    epics_base_rs::runtime::task::test_block_on(run());
}

async fn run() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("rec.db");
    let before_init = dir.path().join("report.out");
    std::fs::write(
        &db,
        r#"record(ai, "rec") { field(DTYP, "stream") field(INP, "@l.proto p NOBUS") }"#,
    )
    .unwrap();

    // var streamError 0, before iocInit
    stream_rs::error::set_stream_error(0);
    let seen = Arc::new(Mutex::new(None));
    let runner_seen = seen.clone();
    stream_rs::dev::register_app(IocApplication::new())
        .port(0)
        .startup_line(&format!("dbLoadRecords(\"{}\")", db.display()))
        .startup_line(&format!(
            "streamReportRecord rec > {}",
            before_init.display()
        ))
        .before_ioc_init(|| IocInitDecision::run(false))
        .run(move |_config| async move {
            let report = |name| {
                let mut out = Vec::new();
                stream_rs::epics::stream_report_record(name, &mut out);
                out
            };
            // no name is every record (StreamEpics.cc:358), a pattern globs
            assert_eq!(report(None), report(Some("r?c")));
            assert_eq!(report(Some("x*")), b"");
            *runner_seen.lock() = Some((STREAM_ERROR.load(Ordering::Relaxed), report(Some("rec"))));
            Ok(())
        })
        .await
        .unwrap();

    // no record is initialized yet: the command ran and found no stream
    assert_eq!(std::fs::read(&before_init).unwrap(), b"");
    let (stream_error, report) = seen.lock().take().unwrap();
    assert_eq!(stream_error, 0);
    // C lists a stream whose record failed its init too
    let c = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/oracle/epics/report_badbus.report"),
    )
    .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&report),
        String::from_utf8_lossy(&c)
    );
}
