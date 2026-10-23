//! A database no lifecycle has started is not running: C `scanInit` leaves
//! `scanCtl` at `ctlPause` and only `scanRun` opens `interruptAccept`
//! (`dbScan.c:199`, `:218`). State and gate are one source, so a bare
//! `IocBuilder` database reads not-running and its event posts queue nothing
//! (`dbScan.c:538`) until `run()`.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::scan::ScanCtl;

const DB: &str = r#"
record(calc, "C") { field(SCAN, "Event") field(EVNT, "5") field(CALC, "VAL+1") }
"#;

#[epics_macros_rs::epics_test]
async fn a_bare_database_reads_not_running_and_posts_nothing_until_run() {
    let db = IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let scan = db.scan_control();
    let val = || db.get_pv("C").unwrap().to_f64().unwrap();

    assert!(!scan.is_running());
    assert_eq!(scan.ctl(), ScanCtl::Pause);
    assert!(!scan.interrupt_gate().is_open());

    db.post_event_named("5").await;
    assert_eq!(val(), 0.0, "a posted event queues nothing before run()");

    scan.run();
    assert!(scan.is_running());
    assert_eq!(scan.ctl(), ScanCtl::Run);
    assert!(scan.interrupt_gate().is_open());
    db.post_event_named("5").await;
    assert_eq!(val(), 1.0, "after run() the same post processes");

    scan.pause();
    assert_eq!((scan.is_running(), scan.ctl()), (false, ScanCtl::Pause));
    scan.stop();
    assert_eq!((scan.is_running(), scan.ctl()), (false, ScanCtl::Exit));
    scan.run();
    assert_eq!(scan.ctl(), ScanCtl::Run, "run() re-arms after a stop");
}
