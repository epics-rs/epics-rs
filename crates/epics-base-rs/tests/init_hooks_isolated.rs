//! C `iocBuildIsolated` (`iocInit.c:232-247`) without `dbInitServers` and
//! with `dbCaLinkInitIsolated`; its init-hook sequence is checked against C
//! in `init_hooks_close_links.rs`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use epics_base_rs::server::ioc_app::{BuildMode, ioc_shutdown};
use epics_base_rs::server::ioc_builder::IocBuilder;

/// No `dbInitServers` in `iocBuildIsolated`: a registered server is not
/// initialised.
#[epics_macros_rs::epics_test]
async fn an_isolated_build_initialises_no_server() {
    let inits = Arc::new(AtomicUsize::new(0));
    let counted = inits.clone();
    let mut ioc = IocBuilder::new()
        .build_mode(BuildMode::Isolated)
        .register_server_init(
            "probe",
            epics_base_rs::server::ioc_app::ServerInitArgs {
                port: 0,
                tcp_port: None,
            },
            move |_| async move {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .build_ioc()
        .await
        .unwrap();
    assert_eq!(inits.load(Ordering::SeqCst), 0);
    assert!(ioc.servers().take::<()>("probe").is_none());
}

/// Counts every question a `ca` network half is asked.
struct Network(Arc<AtomicUsize>);

#[epics_base_rs::async_trait]
impl epics_base_rs::server::database::LinkSet for Network {
    fn is_connected(&self, _name: epics_base_rs::server::database::LinkRef<'_>) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst);
        true
    }
    async fn get_value(
        &self,
        _name: epics_base_rs::server::database::LinkRef<'_>,
    ) -> Option<epics_base_rs::types::EpicsValue> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Some(epics_base_rs::types::EpicsValue::Double(7.0))
    }
    fn get_cached_value(
        &self,
        _name: epics_base_rs::server::database::LinkRef<'_>,
    ) -> Option<epics_base_rs::types::EpicsValue> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Some(epics_base_rs::types::EpicsValue::Double(7.0))
    }
}

/// `dbCaLinkInitIsolated` (`dbCa.c:357-360`): a CA link to a name the
/// database does not serve gets `dbChannelNOOP` (`dbContext.cpp:95-97`), so
/// the network half is never reached.
#[epics_macros_rs::epics_test]
async fn an_isolated_ca_link_never_reaches_the_network() {
    let asked = Arc::new(AtomicUsize::new(0));
    let network = asked.clone();
    let ioc = IocBuilder::new()
        .build_mode(BuildMode::Isolated)
        .register_link_type("ca", move |db| {
            let network = network.clone();
            async move {
                db.register_link_set("ca", Arc::new(Network(network))).await;
                Vec::new()
            }
        })
        .db_string(
            r#"record(calc, "ISO:ca") { field(INPA, "ISO:remote CA") field(CALC, "A") }"#,
            &HashMap::new(),
        )
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    db.process_record("ISO:ca").await.unwrap();
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    assert_eq!(ioc_shutdown(), 0);
}
