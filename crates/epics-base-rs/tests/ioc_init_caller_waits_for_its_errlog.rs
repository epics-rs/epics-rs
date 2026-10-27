//! C `iocBuild_1` raises its caller to ok-to-block before it logs
//! (`iocInit.c:125-127`), so `msgbufCommit` flushes each line it posts
//! (`errlog.c:186-187`): `Starting iocInit` is on the console before iocInit
//! goes on, however far behind the errlog worker is.

use std::collections::HashMap;
use std::io::Read;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epics_base_rs::runtime::log::{
    errlog_add_listener, errlog_init, errlog_set_console, errlog_vprintf,
};
use epics_base_rs::runtime::task::BlockingBridge;
use epics_base_rs::server::ioc_builder::IocBuilder;

#[epics_macros_rs::epics_test]
async fn the_ioc_init_caller_flushes_its_own_errlog_lines() {
    errlog_init(0);
    let console = tempfile::NamedTempFile::new().expect("console file");
    errlog_set_console(console.reopen().expect("console writer"));

    // Hold the worker inside a listener, after it wrote the hold line.
    let (entered_tx, entered) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let released = Mutex::new(released);
    errlog_add_listener(move |m| {
        if m.contains("errlog worker held") {
            entered_tx.send(()).unwrap();
            let _ = released.lock().unwrap().recv();
        }
    });
    errlog_vprintf(format_args!("errlog worker held\n"));
    entered
        .recv_timeout(fact_budget::FACT_BUDGET)
        .expect("worker entered the listener");

    let bridge = BlockingBridge::capture();
    let path = Arc::new(console.path().to_path_buf());
    let (done_tx, done) = mpsc::channel();
    let build = std::thread::spawn(move || {
        bridge
            .block_on(
                IocBuilder::new()
                    .db_string("record(ai, \"F:A\") {}\n", &HashMap::new())
                    .unwrap()
                    .build_ioc(),
            )
            .unwrap();
        let mut at_return = String::new();
        std::fs::File::open(&*path)
            .unwrap()
            .read_to_string(&mut at_return)
            .unwrap();
        done_tx.send(at_return).unwrap();
    });

    epics_base_rs::runtime::task::sleep(Duration::from_millis(300)).await;
    assert!(
        done.try_recv().is_err(),
        "iocInit went on while its `Starting iocInit` was still queued"
    );

    release.send(()).unwrap();
    let mut at_return = None;
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while std::time::Instant::now() < deadline {
        if let Ok(text) = done.try_recv() {
            at_return = Some(text);
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    let at_return = at_return.expect("iocInit returned");
    build.join().unwrap();
    assert!(
        at_return.contains("Starting iocInit"),
        "the console when iocInit returned: {at_return:?}"
    );
}
