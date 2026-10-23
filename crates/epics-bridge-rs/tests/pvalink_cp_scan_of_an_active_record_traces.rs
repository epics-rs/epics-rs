//! pvxs `ScanTrack::scan` (`ioc/pvalink_channel.cpp:314-325`): a CP update
//! reaching a record that is still active sets its RPRO and, with TPRO on,
//! prints `"<thread>: Active <record>"` on stdout.
//!
//! Tokio only: the pvalink runtime is.
#![cfg(all(feature = "pvalink", tokio_backend, unix))]

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;
use epics_bridge_rs::pvalink::install_pvalink_resolver;
use epics_pva_rs::server::PvDatabaseSource;
use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};

const REMOTE: &str = r#"
record(ai, "SRC") { field(VAL, "3") }
"#;

const DB: &str = r#"
record(ai, "T") { field(INP, {pva:{pv:"SRC", proc:"CP"}}) }
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_cp_update_of_an_active_record_prints_the_tpro_line() {
    let (remote, _) = IocBuilder::new()
        .db_string(REMOTE, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let server = PvaServer::start(
        Arc::new(PvDatabaseSource::new(remote.clone())),
        PvaServerConfig::isolated(),
    )
    .expect("source PVA server starts");
    // SAFETY: before the reader IOC builds its PVA client; nothing else in
    // this test binary touches the environment.
    unsafe {
        std::env::set_var("EPICS_PVA_NAME_SERVERS", server.tcp_addr().to_string());
        std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
    }
    let (db, _) = IocBuilder::new()
        .register_link_type("pva", |db| async move {
            install_pvalink_resolver(&db).await;
            Vec::new()
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db.process_cp_links_on_connect().await;

    let t = db.get_record("T").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while t.read().common.sevr != AlarmSeverity::NoAlarm {
        assert!(Instant::now() < deadline, "T never read SRC");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    {
        let mut inst = t.write();
        inst.common.tpro = 1;
        inst.enter_pact();
    }

    let out_sink = tempfile::NamedTempFile::new().expect("stdout capture");
    // SAFETY: fd 1 is process-global; this binary holds one test.
    let saved_out = unsafe {
        let saved = libc::dup(1);
        libc::dup2(out_sink.as_file().as_raw_fd(), 1);
        saved
    };
    remote
        .put_record_field_from_ca("SRC", "VAL", EpicsValue::Double(4.0))
        .await
        .unwrap();
    while t.read().common.rpro == 0 {
        assert!(Instant::now() < deadline, "the update never reached T");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // SAFETY: restoring what this test replaced.
    unsafe {
        libc::dup2(saved_out, 1);
        libc::close(saved_out);
    }
    let out = std::fs::read_to_string(out_sink.path()).expect("stdout capture");
    let active: Vec<&str> = out
        .lines()
        .filter(|line| line.ends_with(": Active T"))
        .collect();
    assert_eq!(active.len(), 1, "{out:?}");
}
