//! An mca `SIML` naming a record another IOC serves is a CA link from
//! `iocInit` (`dbLink.c:118-130`). C oracleMcaIoc with `SIML "RSM"` and `RSM`
//! on a second softIoc: `dbcar` reads `Total 1 CA link, 1 connected`.
//!
//! mca leaves SIML/SIOL to the framework, so their text is on the instance,
//! not in the rset's `get_field`; the locality pass has to read it there.

#![cfg(tokio_backend)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, register_device_row};
use epics_ca_rs::calink::install_calink_resolver;
use epics_ca_rs::server::CaServer;

/// `mcaSupport.dbd`'s `device(mca,CONSTANT,devMCA_soft,"Soft Channel")`.
static SOFT_MCA: DevSup = DevSup {
    name: "devMCA_soft",
    choice: "Soft Channel",
    link_type: DbLinkType::Constant,
    dset: Dset::new(6, &[2, 4, 5], false),
};

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_siml_is_one_connected_ca_link() {
    let remote = CaServer::builder()
        .port(0)
        .db_string(r#"record(bo, "RSM") { field(VAL, "1") }"#, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let port = remote.udp_port();
    // SAFETY: the only test in this binary, and nothing else runs yet.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let _remote = tokio::spawn(async move { remote.run().await });

    let (name, factory) = mca_rs::mca_record_factory();
    register_device_row("mca", &SOFT_MCA);
    let local = CaServer::builder()
        .port(0)
        .register_record_type(name, factory)
        .db_string(
            r#"
record(waveform, "WF") { field(NELM, "4") field(FTVL, "LONG") }
record(mca, "MA") {
    field(DTYP, "Soft Channel")
    field(INP, "WF")
    field(NMAX, "4")
    field(SIML, "RSM")
}
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = local.database().clone();
    let resolver = install_calink_resolver(
        &db,
        epics_base_rs::runtime::task::Reactor::current().expect("test runtime"),
    )
    .await;
    db.setup_external_link_opens().await;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let report = resolver.link_report();
        if report == [("RSM".to_string(), true)] {
            break;
        }
        assert!(Instant::now() < deadline, "dbcar: {report:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
