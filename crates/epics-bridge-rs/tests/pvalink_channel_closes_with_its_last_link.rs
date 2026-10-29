//! pvxs keeps a channel only while a link holds it (`linkGlobal->channels`
//! holds `weak_ptr`s, `ioc/pvalink.h:118`, and `~pvaLinkChannel` erases the
//! entry, `ioc/pvalink_channel.cpp:95-99`): a put that moves the only link
//! off a channel frees it, so `dbpvar` no longer lists it.
//!
//! Tokio only: the pvalink runtime is.
#![cfg(all(feature = "pvalink", tokio_backend))]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_bridge_rs::pvalink::install_pvalink_resolver;

const DB: &str = r#"
record(ai, "H") { field(INP, {pva:{pv:"T:OLD"}}) }
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_put_moving_the_last_link_off_a_channel_closes_it() {
    let closed = named_port::HeldPort::closed_tcp();
    // SAFETY: before the IOC builds its PVA client; nothing else in this
    // test binary touches the environment. Nothing listens on `closed.port`,
    // and the hold keeps a stranger from binding it until the test ends.
    unsafe {
        std::env::set_var(
            "EPICS_PVA_NAME_SERVERS",
            format!("127.0.0.1:{}", closed.port),
        );
        std::env::set_var("EPICS_PVA_ADDR_LIST", "");
        std::env::set_var("EPICS_PVA_AUTO_ADDR_LIST", "NO");
    }
    let (db, _) = IocBuilder::new()
        .register_link_type("pva", |_db| async { Vec::new() })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let resolver = install_pvalink_resolver(&db).await;
    let channels = || -> Vec<String> {
        resolver
            .channel_diagnostics()
            .into_iter()
            .map(|c| c.pv_name)
            .collect()
    };
    assert_eq!(channels(), ["T:OLD"]);

    db.put_record_field_from_ca(
        "H",
        "INP",
        EpicsValue::String(r#"{pva:{pv:"T:NEW"}}"#.into()),
    )
    .await
    .unwrap();
    // The new channel opens on the reactor.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !channels().iter().any(|pv| pv == "T:NEW") {
        assert!(Instant::now() < deadline, "T:NEW never opened");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(channels(), ["T:NEW"]);
}
