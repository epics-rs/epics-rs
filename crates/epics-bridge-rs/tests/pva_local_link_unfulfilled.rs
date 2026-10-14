//! A `local:true` pva link whose channel this IOC does not serve opens no
//! channel on any path and prints pvxs's load line verbatim: pvxs
//! `pvaOpenLink` reports it and drops the lset (`ioc/pvalink_lset.cpp:70-76`),
//! so `dbpvar` lists nothing.
//!
//! Tokio only: the pvalink runtime is.
#![cfg(all(feature = "pvalink", tokio_backend, unix))]

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_bridge_rs::pvalink::install_pvalink_resolver;

const DB: &str = r#"
record(ai, "L:INP") { field(INP, {pva:{pv:"L:NOSUCH", local:true}}) }
record(ao, "L:OUT") { field(OUT, {pva:{pv:"L:NOSUCH", local:true}}) }
record(calc, "L:FLNK") { field(FLNK, {pva:{pv:"L:NOSUCH", local:true}}) }
"#;

#[tokio::test(flavor = "multi_thread")]
async fn unfulfilled_local_link_opens_no_channel_and_prints_pvxs_line() {
    let db: Arc<PvDatabase> = IocBuilder::new()
        .register_link_type("pva", |_db| async { Vec::new() })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    let err_sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: fd 2 is process-global; this binary holds one test.
    let saved_err = unsafe {
        let saved = libc::dup(2);
        libc::dup2(err_sink.as_file().as_raw_fd(), 2);
        saved
    };
    let resolver = install_pvalink_resolver(&db).await;
    // SAFETY: restoring what this test replaced.
    unsafe {
        libc::dup2(saved_err, 2);
        libc::close(saved_err);
    }
    let err = std::fs::read_to_string(err_sink.path()).expect("stderr capture");
    let mut lines: Vec<&str> = err.lines().collect();
    lines.sort_unstable();
    assert_eq!(
        lines,
        [
            "L:FLNK Error: local:true link to 'L:NOSUCH' can't be fulfilled",
            "L:INP Error: local:true link to 'L:NOSUCH' can't be fulfilled",
            "L:OUT Error: local:true link to 'L:NOSUCH' can't be fulfilled",
        ],
        "{err:?}"
    );

    // Every cycle path a refused link can reach: the INP read and its staged
    // open, the OUT put, the forward worker.
    for _ in 0..3 {
        for rec in ["L:INP", "L:OUT", "L:FLNK"] {
            let _ = db
                .process_record_with_links(rec, &mut ProcStack::new())
                .await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let channels = resolver.channel_diagnostics();
    assert!(channels.is_empty(), "{channels:?}");
}
