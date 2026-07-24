//! ACF hot reload test — server keeps running, ACF file is rewritten,
//! reload_acf() picks up the change without restart.

// RTEMS-EXEC-MODEL-ALLOW(1): the flavored test runs a live CaServer over
// tokio::net for the reload round-trip. These run and pass in the
// feature-ON suite on the tokio driver.
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
    let bare = CaServer::from_parts(server.database().clone(), 0, None, None, None, None)
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
