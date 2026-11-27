//! ACF hot reload test — server keeps running, ACF file is rewritten,
//! reload_acf() picks up the change without restart.

#![cfg(tokio_backend)]

use epics_ca_rs::server::CaServer;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn reload_acf_swaps_in_new_rules() {
    let dir = tempfile::tempdir().expect("temp");
    let acf_path = dir.path().join("test.acf");

    // Initial ACF — DEFAULT group denies write.
    std::fs::write(&acf_path, "ASG(DEFAULT) { RULE(1, READ) }").expect("write acf v1");

    let server = CaServer::builder()
        .port(0)
        .pv("HOT:VAL", epics_base_rs::types::EpicsValue::Long(0))
        .acf_file(acf_path.to_str().unwrap())
        .expect("acf v1")
        .build()
        .await
        .expect("build");

    // Source path was captured.
    assert_eq!(
        server.acf_source_path().as_deref(),
        Some(acf_path.to_str().unwrap())
    );

    // Rewrite the file with a permissive policy.
    std::fs::write(&acf_path, "ASG(DEFAULT) { RULE(1, WRITE) }").expect("write acf v2");

    // reload_acf must succeed and not panic.
    server.reload_acf().await.expect("reload");

    // No source path → reload_acf_from must work but reload_acf would
    // fail on a server constructed without acf_file.
    let bare = CaServer::from_parts(
        server.database().clone(),
        0,
        None,
        epics_base_rs::server::access_security::new_acf_cell(None),
        None,
        None,
    )
    .await
    .expect("build bare server");
    assert!(bare.reload_acf().await.is_err());
    assert!(
        bare.reload_acf_from(acf_path.to_str().unwrap())
            .await
            .is_ok()
    );

    // Now this server's source path is set.
    assert!(bare.acf_source_path().is_some());

    // Don't actually run the server here — we only care about the
    // reload mechanics. End the test before tempdir drops.
    drop(dir);
    tokio::time::sleep(Duration::from_millis(10)).await;
}

/// C `asInit` returns only once `asCaStart` has the inputs' first readings
/// (`asCaTaskWait`), so a check right after a reload sees them.
#[tokio::test(flavor = "multi_thread")]
async fn reload_acf_from_returns_with_the_first_readings_in_place() {
    use epics_base_rs::server::database::PvDatabase;
    let db = std::sync::Arc::new(PvDatabase::new());
    db.add_record(
        "rv",
        Box::new(epics_base_rs::server::records::ai::AiRecord::new(5.0)),
    )
    .await
    .unwrap();
    db.put_record_field_from_ca("rv", "VAL", epics_base_rs::types::EpicsValue::Double(5.0))
        .await
        .unwrap();
    let dir = tempfile::tempdir().expect("temp");
    let path = dir.path().join("x.acf");
    std::fs::write(
        &path,
        r#"ASG(DEFAULT) { INPA(rv) RULE(1,READ) { CALC("A>1") } }"#,
    )
    .unwrap();
    let acf = epics_base_rs::server::access_security::new_acf_cell_watching(None, &db);
    let server = CaServer::from_parts(db.clone(), 0, None, acf.clone(), None, None)
        .await
        .expect("build");
    // Hold `rv`'s lock across the reload: the first reading is in place at
    // the assert only if the reload waited for the watcher to take it.
    let (locked, held) = std::sync::mpsc::channel();
    let rec = db.get_record("rv").unwrap();
    let holder = std::thread::spawn(move || {
        let _guard = rec.write();
        locked.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
    });
    held.recv().unwrap();
    server
        .reload_acf_from(path.to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        acf.load_full().unwrap().asg[b"DEFAULT".as_slice()].inp_bad(),
        0
    );
    holder.join().unwrap();
}
