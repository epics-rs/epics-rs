//! A record's own CA client (motorUtil's `ca_add_event`, sscan saveData's)
//! may block in a monitor callback of a name served over the network, as
//! C's does on libca's receive thread (`tcpiiu.cpp:498-519`, under
//! `cbMutex`): saveData's `epicsMessageQueueSend` waits for room, and the
//! value waits with it instead of being lost.
//!
//! Exec backend only: there a callback run on the reactor may not block,
//! which is the configuration that lost the value.

#![cfg(exec_backend)]
#![cfg(feature = "client-core")]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread;

use epics_base_rs::runtime::sync::mpsc;
use epics_base_rs::runtime::task::{background_init, block_on_sync};
use epics_base_rs::server::database::{ClientChannel, Connection, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{DBR_DOUBLE, EpicsValue};
use epics_ca_rs::calink::install_calink_resolver;
use epics_ca_rs::server::blocking::BlockingCaServer;

const UPSTREAM_DB: &str = "record(ao, \"NETBLK:AO\") { field(VAL, \"0\") }\n";
/// `DBE_VALUE | DBE_ALARM`.
const MASK: u16 = 1 | 4;
const PUTS: usize = 5;

fn build_db(db: &str) -> Arc<PvDatabase> {
    let (db, _autosave) = block_on_sync(
        IocBuilder::new()
            .db_string(db, &HashMap::new())
            .expect("load db string")
            .build(),
    )
    .expect("a plain test thread may block")
    .expect("iocInit");
    db
}

fn connect(db: &PvDatabase, name: &str) -> Arc<dyn ClientChannel> {
    block_on_sync(async {
        let ch = db.ca_client().create_channel(name).await.expect("channel");
        while let Some(c) = ch.next_connection().await {
            if c == Connection::Connected {
                return ch;
            }
        }
        panic!("{name} never connected")
    })
    .expect("a plain test thread may block")
}

fn value(s: &Snapshot) -> f64 {
    s.value.to_f64().expect("a double")
}

#[test]
fn a_network_monitor_callback_may_wait_for_room() {
    background_init();
    let upstream = build_db(UPSTREAM_DB);
    let server = Arc::new(
        BlockingCaServer::bind(
            "127.0.0.1:0",
            upstream,
            epics_base_rs::server::access_security::new_acf_cell(None),
        )
        .expect("bind on an ephemeral loopback port"),
    );
    let addr = server.local_addr().expect("local_addr");
    let srv = server.clone();
    let accept = thread::spawn(move || srv.serve());
    // SAFETY: the only test in this file, so nothing else touches the
    // environment; the client reads it when the resolver first builds it.
    unsafe {
        std::env::set_var(
            "EPICS_CA_NAME_SERVERS",
            format!("127.0.0.1:{}", addr.port()),
        );
        std::env::set_var("EPICS_CA_ADDR_LIST", "");
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
    }
    let downstream = build_db("record(ao, \"NETBLK:LOCAL\") {}\n");
    block_on_sync(install_calink_resolver(
        &downstream,
        epics_base_rs::runtime::task::Reactor::current().expect("reactor"),
    ))
    .expect("a plain test thread may block");

    // The waiting consumer: one slot, and a callback that waits for room
    // as motorUtil's and saveData's do.
    let ch = connect(&downstream, "NETBLK:AO");
    let (tx, mut rx) = mpsc::channel::<f64>(1);
    let waiting = block_on_sync(ch.subscribe(
        DBR_DOUBLE,
        1,
        MASK,
        Box::new(move |s| {
            if let Err(mpsc::error::TrySendError::Full(v)) = tx.try_send(value(&s)) {
                let _ = block_on_sync(tx.send(v));
            }
        }),
    ))
    .expect("a plain test thread may block")
    .expect("subscribe");

    // A second context's subscription to the same name, to pace the puts:
    // each value has crossed the wire before the next is put.
    let pace_ch = connect(&downstream, "NETBLK:AO");
    let (pace_tx, pace_rx) = std_mpsc::channel::<f64>();
    let _pace = block_on_sync(pace_ch.subscribe(
        DBR_DOUBLE,
        1,
        MASK,
        Box::new(move |s| {
            let _ = pace_tx.send(value(&s));
        }),
    ))
    .expect("a plain test thread may block")
    .expect("subscribe");
    let seen = |want: f64| loop {
        let got = pace_rx
            .recv_timeout(fact_budget::FACT_BUDGET)
            .unwrap_or_else(|_| panic!("{want} never reached the pacing monitor"));
        if got == want {
            break;
        }
    };
    seen(0.0);

    // Nothing drains the consumer while the puts go out.
    for k in 1..=PUTS {
        block_on_sync(ch.put(EpicsValue::Double(k as f64), false))
            .expect("a plain test thread may block")
            .expect("put");
        seen(k as f64);
    }

    let mut got = Vec::new();
    while got.len() < PUTS + 1 {
        match block_on_sync(async {
            epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, rx.recv()).await
        })
        .expect("a plain test thread may block")
        {
            Ok(Some(v)) => got.push(v),
            _ => break,
        }
    }
    let want: Vec<f64> = (0..=PUTS).map(|k| k as f64).collect();
    assert_eq!(got, want, "every value, in order, waited for room");

    drop(waiting);
    server.shutdown();
    accept.join().expect("the accept thread exits on shutdown");
}
