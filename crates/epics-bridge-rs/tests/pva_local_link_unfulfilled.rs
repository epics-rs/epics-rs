//! A `local:true` pva link whose channel this IOC does not serve opens no
//! channel on any path and prints pvxs's line verbatim, at iocInit and on a
//! put of the link: pvxs `pvaOpenLink` reports it and drops the lset
//! (`ioc/pvalink_lset.cpp:70-76`), so `dbpvar` lists nothing.
//!
//! Tokio only: the pvalink runtime is.
#![cfg(all(feature = "pvalink", tokio_backend, unix))]

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_bridge_rs::pvalink::install_pvalink_resolver;

const DB: &str = r#"
record(ai, "L:INP") { field(INP, {pva:{pv:"L:NOSUCH", local:true}}) }
record(ao, "L:OUT") { field(OUT, {pva:{pv:"L:NOSUCH", local:true}}) }
record(calc, "L:FLNK") { field(FLNK, {pva:{pv:"L:NOSUCH", local:true}}) }
"#;

/// What `f` prints on stderr, read once `lines` lines are in (an open run on
/// the reactor prints after `f` returns) and a duplicate would have followed.
async fn stderr_of<T>(lines: usize, f: impl Future<Output = T>) -> (T, String) {
    let sink = tempfile::NamedTempFile::new().expect("stderr capture");
    // SAFETY: fd 2 is process-global; this binary holds one test.
    let saved = unsafe {
        let saved = libc::dup(2);
        libc::dup2(sink.as_file().as_raw_fd(), 2);
        saved
    };
    let out = f.await;
    let read = || std::fs::read_to_string(sink.path()).expect("stderr capture");
    let deadline = Instant::now() + Duration::from_secs(20);
    while read().lines().count() < lines && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    // SAFETY: restoring what this function replaced.
    unsafe {
        libc::dup2(saved, 2);
        libc::close(saved);
    }
    (out, read())
}

#[tokio::test(flavor = "multi_thread")]
async fn unfulfilled_local_link_opens_no_channel_and_prints_pvxs_line() {
    // Installed where every IOC installs it, at `dbCaLinkInit`, so iocInit's
    // open pass asks it.
    let installed = Arc::new(std::sync::OnceLock::new());
    let into = Arc::clone(&installed);
    let build = IocBuilder::new()
        .register_link_type("pva", move |db| async move {
            let _ = into.set(install_pvalink_resolver(&db).await);
            Vec::new()
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build();
    let (db, err) = stderr_of(3, async { build.await.unwrap().0 }).await;
    let db: Arc<PvDatabase> = db;
    let resolver = installed.get().expect("installed").clone();
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

    // softIocPVX 1.5.3 prints the same line for a dbpf of the link.
    let put = db.put_record_field_from_ca(
        "L:INP",
        "INP",
        EpicsValue::String(r#"{pva:{pv:"L:OTHER", local:true}}"#.into()),
    );
    let (put, err) = stderr_of(1, put).await;
    put.unwrap();
    assert_eq!(
        err.lines().collect::<Vec<_>>(),
        ["L:INP Error: local:true link to 'L:OTHER' can't be fulfilled"],
        "{err:?}"
    );
    let channels = resolver.channel_diagnostics();
    assert!(channels.is_empty(), "{channels:?}");

    // FLNK is no device link, so its put fails `dbLinkIsDefined` with
    // S_db_badHWaddr (`dbAccess.c:1168-1174`), clearing it included: the
    // field keeps its text and nothing is printed. OUT re-opens.
    let kept = r#"{pva:{pv:"L:NOSUCH",local:true}}"#;
    for text in [r#"{pva:{pv:"L:OTHER", local:true}}"#, ""] {
        let put = db.put_record_field_from_ca("L:FLNK", "FLNK", EpicsValue::String(text.into()));
        let (put, err) = stderr_of(0, put).await;
        assert!(put.is_err(), "{text:?}: {put:?}");
        assert_eq!(err, "", "{text:?}");
        assert_eq!(
            db.get_pv("L:FLNK.FLNK").unwrap(),
            EpicsValue::String(kept.into()),
            "{text:?}"
        );
    }
    let put = db.put_record_field_from_ca(
        "L:OUT",
        "OUT",
        EpicsValue::String(r#"{pva:{pv:"L:OTHER", local:true}}"#.into()),
    );
    let (put, err) = stderr_of(1, put).await;
    put.unwrap();
    assert_eq!(
        err.lines().collect::<Vec<_>>(),
        ["L:OUT Error: local:true link to 'L:OTHER' can't be fulfilled"],
        "{err:?}"
    );
}
