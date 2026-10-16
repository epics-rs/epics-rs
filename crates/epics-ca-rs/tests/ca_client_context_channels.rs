//! The CA client a record opens its own channels on (sscan's recDynLink):
//! `ca_create_channel` on a context `dbServiceIOInit` gave `dbContext`
//! (`dbContext.cpp:59-66`), so a name the IOC serves is an in-process
//! channel and any other goes to the network context.
#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::{Connection, PvDatabase};
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::{DBR_CTRL_DOUBLE, DBR_TIME_DOUBLE, EpicsValue};
use epics_ca_rs::calink::{install_calink_resolver, register_calink_commands};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// Run `db_text` through iocInit with the `ca` link set installed and the
/// CA address list at `addr_list`, then hand the database to `body`.
async fn with_ioc<F, Fut>(db_text: &str, addr_list: String, body: F)
where
    F: FnOnce(Arc<PvDatabase>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    // SAFETY: `#[serial(epics_env)]` keeps every env-touching test apart.
    unsafe {
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_ADDR_LIST", addr_list);
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("client.db");
    std::fs::write(&db_path, db_text).expect("write db");
    let stcmd_path = dir.path().join("st.cmd");
    std::fs::write(
        &stcmd_path,
        format!("dbLoadRecords(\"{}\")\n", db_path.display()),
    )
    .expect("write st.cmd");
    let reactor = epics_base_rs::runtime::task::Reactor::current().expect("test runtime");
    IocApplication::new()
        .startup_script(stcmd_path.to_str().unwrap())
        .register_link_type("ca", move |db| async move {
            register_calink_commands(install_calink_resolver(&db, reactor).await)
        })
        .run(move |config| async move {
            body(config.db.clone()).await;
            Ok(())
        })
        .await
        .expect("IOC run");
}

async fn within<T>(what: &str, fut: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(fact_budget::FACT_BUDGET, fut)
        .await
        .unwrap_or_else(|_| panic!("{what}"))
}

/// A served name connects at once, and a put callback completes only when
/// the target's processing does — here after the calcout's ODLY.
#[epics_macros_rs::epics_test]
#[serial(epics_env)]
async fn a_served_name_is_an_in_process_channel() {
    with_ioc(
        "record(ao, \"CC:AO\") { field(VAL, \"2\") field(DRVH, \"9\") field(PREC, \"3\") }\n\
         record(calcout, \"CC:CO\") { field(CALC, \"A\") field(ODLY, \"0.4\") }\n",
        "192.0.2.1".into(),
        |db| async move {
            let ctx = db.ca_client();
            let ao = ctx.create_channel("CC:AO").await.expect("served name");
            assert_eq!(ao.next_connection().await, Some(Connection::Connected));
            assert_eq!(ao.element_count(), Some(1));

            let ctrl = ao.get(DBR_CTRL_DOUBLE, 1).await.expect("ctrl get");
            assert_eq!(ctrl.value.to_f64(), Some(2.0));

            let mut monitor = ao.subscribe(DBR_TIME_DOUBLE, 1, 5).await.expect("monitor");
            let first = within("no first event", monitor.next()).await.unwrap();
            assert_eq!(
                first.value.to_f64(),
                Some(2.0),
                "the first event is the value"
            );

            assert!(
                ao.put(EpicsValue::Double(4.0), false)
                    .await
                    .unwrap()
                    .is_none()
            );
            let next = within("the put was not seen", monitor.next())
                .await
                .unwrap();
            assert_eq!(next.value.to_f64(), Some(4.0));

            let co = ctx.create_channel("CC:CO.A").await.expect("served name");
            let started = Instant::now();
            let done = co
                .put(EpicsValue::Double(1.0), true)
                .await
                .expect("put callback issued")
                .expect("a put callback has a completion");
            within("the put callback never completed", done)
                .await
                .expect("completed");
            assert!(
                started.elapsed() >= Duration::from_millis(350),
                "completed after {:?}, ahead of the ODLY",
                started.elapsed()
            );
        },
    )
    .await;
}

/// A name the IOC does not serve goes to the network context: it connects
/// when its server answers and reports the disconnect when it goes away.
#[epics_macros_rs::epics_test]
#[serial(epics_env)]
async fn an_unserved_name_goes_to_the_network_context() {
    // The server runs on a runtime of its own, so dropping that runtime
    // closes its circuits: the disconnect the channel must report.
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let serving = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("server runtime");
        rt.block_on(async move {
            let server = CaServer::builder()
                .port(0)
                .record("CCNET:AI", AiRecord::new(3.0))
                .build()
                .await
                .expect("CA server");
            port_tx.send(server.udp_port()).unwrap();
            tokio::spawn(async move { server.run().await });
            let _ = stop_rx.await;
        });
        rt.shutdown_timeout(Duration::from_secs(1));
    });
    let port = port_rx.recv().expect("server port");
    // SAFETY: `#[serial(epics_env)]`.
    unsafe {
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    with_ioc(
        "record(ao, \"CC:LOCAL\") {}\n",
        format!("127.0.0.1:{port}"),
        move |db| async move {
            let ai = db
                .ca_client()
                .create_channel("CCNET:AI")
                .await
                .expect("a network half is registered");
            assert_eq!(
                within("never connected", ai.next_connection()).await,
                Some(Connection::Connected)
            );
            let ctrl = ai.get(DBR_CTRL_DOUBLE, 1).await.expect("ctrl get");
            assert_eq!(ctrl.value.to_f64(), Some(3.0));
            let mut monitor = ai.subscribe(DBR_TIME_DOUBLE, 1, 5).await.expect("monitor");
            let first = within("no first event", monitor.next()).await.unwrap();
            assert_eq!(first.value.to_f64(), Some(3.0));

            let done = ai
                .put(EpicsValue::Double(6.0), true)
                .await
                .expect("issued")
                .expect("a completion");
            within("never completed", done).await.expect("completed");
            let next = within("the put was not seen", monitor.next())
                .await
                .unwrap();
            assert_eq!(next.value.to_f64(), Some(6.0));

            stop_tx.send(()).unwrap();
            assert_eq!(
                within("the disconnect was not reported", ai.next_connection()).await,
                Some(Connection::Disconnected)
            );
        },
    )
    .await;
    serving.join().unwrap();
    // SAFETY: `#[serial(epics_env)]`.
    unsafe {
        std::env::remove_var("EPICS_CA_SERVER_PORT");
    }
}
