//! iocInit runs `drvStreamInit` and the stream dsets' `init` on an IOC
//! with no stream record. `oracle/epics/no_stream_records.stderr` is the C
//! oracle's stderr (streamApp, `record(ai, "x") {}`, iocInit, no
//! STREAM_PROTOCOL_PATH). The IOC runs in a child of this test so its
//! stderr is the process's.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::Ordering;

use epics_base_rs::server::ioc_builder::IocBuilder;
use parking_lot::Mutex;
use stream_rs::error::STREAM_ERROR;

static OUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct Out;

impl Write for Out {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        OUT.lock().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

const CHILD: &str = "STREAM_RS_DRV_INIT_CHILD";

#[test]
fn drv_init_without_stream_records() {
    if std::env::var_os(CHILD).is_some() {
        epics_base_rs::runtime::task::test_block_on(child());
        return;
    }
    let run = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "drv_init_without_stream_records", "--nocapture"])
        .env(CHILD, "1")
        .env_remove("STREAM_PROTOCOL_PATH")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(run.status.success(), "{stderr}");
    let c = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/oracle/epics/no_stream_records.stderr"),
    )
    .unwrap();
    // C's lines, in C's order, among the test harness's own
    let mut rs = stderr.lines();
    for line in c.lines() {
        assert!(rs.any(|l| l == line), "no {line:?} in order in\n{stderr}");
    }
}

async fn child() {
    stream_rs::error::set_stream_debug_file(Some(Box::new(Out)));
    stream_rs::error::set_stream_debug(1);
    // streamInit(1) restores what streamInit(0) saved
    stream_rs::error::set_stream_error(1);
    let ioc = stream_rs::dev::register(IocBuilder::new())
        .db_string(r#"record(ai, "x") { }"#, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    assert!(ioc.run().await);
    stream_rs::error::set_stream_debug(0);

    let out = String::from_utf8_lossy(&OUT.lock()).into_owned();
    assert!(out.contains("drvStreamInit()"), "{out}");
    assert_eq!(STREAM_ERROR.load(Ordering::Relaxed), 1);
    assert_eq!(epics_base_rs::server::ioc_app::ioc_shutdown(), 0);
}
