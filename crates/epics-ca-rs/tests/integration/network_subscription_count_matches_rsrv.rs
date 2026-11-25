//! The element count a record's own CA client gets from a network
//! subscription, at each boundary of the count it asks for, against a
//! 4-element waveform holding 2. Count 0 is autosize, served as the
//! elements the record holds (`camessage.c` `read_reply`); any other count
//! is served at that count with the rest zeroed, and one past the channel's
//! is cut to the channel's (libca `netSubscription::getCount`, rsrv
//! `event_add_action`).
#![cfg(feature = "client-core")]

use std::collections::HashMap;
use std::sync::Arc;
use std::thread;

use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::server::database::{ClientChannel, Connection, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{DBR_LONG, EpicsValue};
use epics_ca_rs::calink::install_calink_resolver;
use epics_ca_rs::server::blocking::BlockingCaServer;
use serial_test::serial;

const UPSTREAM_DB: &str =
    "record(waveform, \"NETCNT:WF\") { field(FTVL, \"LONG\") field(NELM, \"4\") }\n";
/// `DBE_VALUE | DBE_ALARM`.
const MASK: u16 = 1 | 4;

async fn build_db(db: &str) -> Arc<PvDatabase> {
    let (db, _autosave) = IocBuilder::new()
        .db_string(db, &HashMap::new())
        .expect("load db string")
        .build()
        .await
        .expect("iocInit");
    db
}

async fn connect(db: &PvDatabase, name: &str) -> Arc<dyn ClientChannel> {
    let ch = db.ca_client().create_network(name).await.expect("channel");
    while let Some(c) = ch.next_connection().await {
        if c == Connection::Connected {
            return ch;
        }
    }
    panic!("{name} never connected")
}

/// The first value of a `DBR_LONG` subscription of `count` elements to
/// `NETCNT:WF` once `[7, 8]` is put to it.
async fn first_value(count: u32) -> EpicsValue {
    let upstream = build_db(UPSTREAM_DB).await;
    let server = Arc::new(
        BlockingCaServer::bind(
            "127.0.0.1:0",
            upstream,
            epics_base_rs::server::access_security::new_acf_cell(None),
        )
        .expect("bind on an ephemeral loopback port"),
    );
    let port = server.local_addr().expect("local_addr").port();
    let srv = server.clone();
    let accept = thread::spawn(move || srv.serve());
    // SAFETY: `#[serial(epics_env)]` keeps the tests apart; the client
    // reads these when the resolver first builds it.
    unsafe {
        std::env::set_var("EPICS_CA_NAME_SERVERS", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_ADDR_LIST", "");
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
    }
    let downstream = build_db("record(ao, \"NETCNT:LOCAL\") {}\n").await;
    install_calink_resolver(
        &downstream,
        epics_base_rs::runtime::task::Reactor::current().expect("reactor"),
    )
    .await;

    let ch = connect(&downstream, "NETCNT:WF").await;
    assert_eq!(ch.element_count(), Some(4));
    let done = ch
        .put(EpicsValue::LongArray(vec![7, 8].into()), true)
        .await
        .expect("issued")
        .expect("a put callback has a completion");
    epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, done)
        .await
        .expect("the put never completed")
        .expect("completed");

    let (tx, mut rx) = mpsc::channel(8);
    let _sub = ch
        .subscribe(
            DBR_LONG,
            count,
            MASK,
            Box::new(move |s| {
                let _ = tx.try_send(s.value);
            }),
        )
        .await
        .expect("subscribe");
    let value = epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, rx.recv())
        .await
        .expect("no first event")
        .expect("the subscription ended");

    server.shutdown();
    accept.join().expect("the accept thread exits on shutdown");
    value
}

#[epics_macros_rs::epics_test]
#[serial(epics_env)]
async fn a_zero_count_gets_the_elements_the_record_holds() {
    assert_eq!(
        first_value(0).await,
        EpicsValue::LongArray(vec![7, 8].into())
    );
}

#[epics_macros_rs::epics_test]
#[serial(epics_env)]
async fn the_channels_count_gets_its_elements_zero_filled() {
    assert_eq!(
        first_value(4).await,
        EpicsValue::LongArray(vec![7, 8, 0, 0].into())
    );
}

#[epics_macros_rs::epics_test]
#[serial(epics_env)]
async fn a_count_past_the_channels_gets_the_channels_count() {
    assert_eq!(
        first_value(6).await,
        EpicsValue::LongArray(vec![7, 8, 0, 0].into())
    );
}
