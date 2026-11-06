//! A QSRV put's reply reaches the client after the updates its processing
//! posted, whichever of the two sources carries the put and the monitor, so
//! a caller reading its monitor after the put sees the value.
#![cfg(feature = "qsrv")]
#![cfg(tokio_backend)]

use std::sync::Arc;
use std::time::Duration;

use epics_bridge_rs::qsrv::{BridgeProvider, QsrvMount, QsrvPvStore};
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::pvdata::{PvField, ScalarValue};
use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};
use parking_lot::Mutex;

const DB: &str = r#"record(ao, "ORD:ao") {}"#;
const GROUP: &str = r#"{"ORD:G": {"v": {"+channel": "ORD:ao.VAL", "+putorder": 0}}}"#;
const PUTS: u32 = 300;

struct Ioc {
    _server: PvaServer,
    client: PvaClient,
}

async fn ioc() -> Ioc {
    let (db, _) = epics_base_rs::server::ioc_builder::IocBuilder::new()
        .db_string(DB, &Default::default())
        .unwrap()
        .build()
        .await
        .unwrap();
    let provider = Arc::new(BridgeProvider::new(db.clone()));
    provider.load_group_config(GROUP).expect("load group");
    assert_eq!(provider.process_groups().await, 1);
    let store = QsrvPvStore::new(
        epics_base_rs::runtime::task::Reactor::current().expect("the test enters a runtime"),
        provider,
    );
    let composite = epics_pva_rs::server_native::CompositeSource::new();
    QsrvMount {
        store: Arc::new(store),
        acf: epics_base_rs::server::access_security::new_acf_cell(None),
    }
    .install_channel_sources(&db, &composite)
    .expect("qsrv sources mount");
    let server =
        PvaServer::start(composite, PvaServerConfig::isolated()).expect("qsrv server starts");
    let client = PvaClient::builder()
        .server_addr(server.tcp_addr())
        .timeout(Duration::from_secs(5))
        .build();
    Ioc {
        _server: server,
        client,
    }
}

fn double_at(v: &PvField, path: &str) -> Option<f64> {
    let leaf = path.split('.').try_fold(v, |at, name| match at {
        PvField::Structure(s) => s.get_field(name),
        _ => None,
    });
    match leaf {
        Some(PvField::Scalar(ScalarValue::Double(x))) => Some(*x),
        _ => None,
    }
}

/// How many of `PUTS` puts through `put` resolved before `monitor` had
/// delivered the put value at `path`.
async fn stale_puts(
    client: &PvaClient,
    monitor: &str,
    path: &'static str,
    put: impl AsyncFn(f64),
) -> u32 {
    let latest = Arc::new(Mutex::new(None));
    let seen = latest.clone();
    let _monitor = client
        .pvmonitor_handle(
            monitor,
            move |_, value| *seen.lock() = double_at(value, path),
            |_| {},
        )
        .await
        .expect("subscribe");
    tokio::time::timeout(Duration::from_secs(5), async {
        while latest.lock().is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial value");

    let mut stale = 0;
    for n in 1..=PUTS {
        let value = f64::from(n) + 0.5;
        put(value).await;
        if *latest.lock() != Some(value) {
            stale += 1;
        }
    }
    stale
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_record_put_reply_follows_its_record_update() {
    let ioc = ioc().await;
    let client = &ioc.client;
    let stale = stale_puts(client, "ORD:ao", "value", async |v| {
        client.pvput("ORD:ao", &v.to_string()).await.expect("put")
    })
    .await;
    assert_eq!(stale, 0, "{stale}/{PUTS} puts resolved before their update");
}

/// The put goes through `qsrvSingle`, the update through the `qsrvGroup`
/// drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_record_put_reply_follows_its_group_update() {
    let ioc = ioc().await;
    let client = &ioc.client;
    let stale = stale_puts(client, "ORD:G", "v.value", async |v| {
        client.pvput("ORD:ao", &v.to_string()).await.expect("put")
    })
    .await;
    assert_eq!(stale, 0, "{stale}/{PUTS} puts resolved before their update");
}

/// The put goes through `qsrvGroup`, the update through the `qsrvSingle`
/// drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_group_put_reply_follows_its_record_update() {
    let ioc = ioc().await;
    let client = &ioc.client;
    let stale = stale_puts(client, "ORD:ao", "value", async |v| {
        client
            .pvput_field("ORD:G", "v.value", &v.to_string())
            .await
            .expect("put")
    })
    .await;
    assert_eq!(stale, 0, "{stale}/{PUTS} puts resolved before their update");
}
