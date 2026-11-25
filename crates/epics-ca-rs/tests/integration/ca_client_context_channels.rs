//! The CA client a record opens its own channels on (sscan's recDynLink):
//! `ca_create_channel` on a context `dbServiceIOInit` gave `dbContext`
//! (`dbContext.cpp:59-66`), so a name the IOC serves is an in-process
//! channel and any other goes to the network context.
#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::{ClientChannel, ClientSubscription, Connection, PvDatabase};
use epics_base_rs::server::ioc_app::{IocApplication, RunOutcome};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{DBR_CTRL_DOUBLE, DBR_TIME_DOUBLE, EpicsValue};
use epics_ca_rs::calink::{install_calink_resolver, register_calink_commands};
use epics_ca_rs::protocol::{CA_PROTO_READ_NOTIFY, CA_PROTO_SEARCH, CaHeader};
use epics_ca_rs::server::CaServer;
use serial_test::serial;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

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
    // Without the `iocInit` the script is a custom `main`'s and the body
    // never runs.
    std::fs::write(
        &stcmd_path,
        format!("dbLoadRecords(\"{}\")\niocInit\n", db_path.display()),
    )
    .expect("write st.cmd");
    let reactor = epics_base_rs::runtime::task::Reactor::current().expect("test runtime");
    let outcome = IocApplication::new()
        .startup_script(stcmd_path.to_str().unwrap())
        .register_link_type("ca", move |db| async move {
            register_calink_commands(install_calink_resolver(&db, reactor).await)
        })
        .run_outcome(move |config| async move {
            body(config.db.clone()).await;
            Ok(())
        })
        .await
        .expect("IOC run");
    assert_eq!(outcome, RunOutcome::Served);
}

async fn within<T>(what: &str, fut: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(fact_budget::FACT_BUDGET, fut)
        .await
        .unwrap_or_else(|_| panic!("{what}"))
}

/// A `DBR_TIME_DOUBLE` subscription of `ch` and the values it delivers.
async fn subscribe(
    ch: &dyn ClientChannel,
) -> (
    ClientSubscription,
    tokio::sync::mpsc::UnboundedReceiver<Snapshot>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let callback = Box::new(move |s| {
        let _ = tx.send(s);
    });
    let sub = ch
        .subscribe(DBR_TIME_DOUBLE, 1, 5, callback)
        .await
        .expect("monitor");
    (sub, rx)
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

            let (_sub, mut monitor) = subscribe(&*ao).await;
            let first = within("no first event", monitor.recv()).await.unwrap();
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
            let next = within("the put was not seen", monitor.recv())
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
            let (_sub, mut monitor) = subscribe(&*ai).await;
            let first = within("no first event", monitor.recv()).await.unwrap();
            assert_eq!(first.value.to_f64(), Some(3.0));

            let done = ai
                .put(EpicsValue::Double(6.0), true)
                .await
                .expect("issued")
                .expect("a completion");
            within("never completed", done).await.expect("completed");
            let next = within("the put was not seen", monitor.recv())
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

/// A proxy in front of a server's search port `udp` and circuit port `tcp`
/// that relays both but swallows every `CA_PROTO_READ_NOTIFY` the client
/// sends, so a get on a connected channel is never answered. Returns the
/// proxy's search port.
async fn read_swallowing_proxy(udp: u16, tcp: u16) -> u16 {
    let search = UdpSocket::bind(("127.0.0.1", 0)).await.expect("proxy UDP");
    let circuits = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("proxy TCP");
    let search_port = search.local_addr().unwrap().port();
    let circuit_port = circuits.local_addr().unwrap().port();
    let server: SocketAddr = ([127, 0, 0, 1], udp).into();
    tokio::spawn(async move {
        let upstream = UdpSocket::bind(("127.0.0.1", 0))
            .await
            .expect("upstream UDP");
        let mut buf = [0u8; 4096];
        while let Ok((n, client)) = search.recv_from(&mut buf).await {
            let _ = upstream.send_to(&buf[..n], server).await;
            let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_secs(1), upstream.recv(&mut buf)).await
            else {
                continue;
            };
            // The reply's data type carries the circuit port: make it ours.
            let mut at = 0;
            while at + CaHeader::SIZE <= n {
                if u16::from_be_bytes([buf[at], buf[at + 1]]) == CA_PROTO_SEARCH {
                    buf[at + 4..at + 6].copy_from_slice(&circuit_port.to_be_bytes());
                }
                at += CaHeader::SIZE + usize::from(u16::from_be_bytes([buf[at + 2], buf[at + 3]]));
            }
            let _ = search.send_to(&buf[..n], client).await;
        }
    });
    tokio::spawn(async move {
        while let Ok((client, _)) = circuits.accept().await {
            let upstream = TcpStream::connect(("127.0.0.1", tcp))
                .await
                .expect("upstream TCP");
            let ((mut cr, mut cw), (mut sr, mut sw)) = (client.into_split(), upstream.into_split());
            tokio::spawn(async move { tokio::io::copy(&mut sr, &mut cw).await });
            tokio::spawn(async move {
                // Frames are scalar-sized: no extended header.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while let Ok(n @ 1..) = cr.read(&mut chunk).await {
                    buf.extend_from_slice(&chunk[..n]);
                    while buf.len() >= CaHeader::SIZE {
                        let len =
                            CaHeader::SIZE + usize::from(u16::from_be_bytes([buf[2], buf[3]]));
                        if buf.len() < len {
                            break;
                        }
                        let frame: Vec<u8> = buf.drain(..len).collect();
                        let command = u16::from_be_bytes([frame[0], frame[1]]);
                        if command != CA_PROTO_READ_NOTIFY && sw.write_all(&frame).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    search_port
}

/// A get on a network channel is `ca_array_get_callback`, which has no
/// deadline: one the server never answers is still waiting past 30 s, and
/// ends with the disconnect.
#[epics_macros_rs::epics_test]
#[serial(epics_env)]
async fn a_network_get_waits_for_its_answer_with_no_deadline() {
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
                .record("CCGET:AI", AiRecord::new(3.0))
                .build()
                .await
                .expect("CA server");
            port_tx
                .send((server.udp_port(), server.tcp_port()))
                .unwrap();
            tokio::spawn(async move { server.run().await });
            let _ = stop_rx.await;
        });
        rt.shutdown_timeout(Duration::from_secs(1));
    });
    let (udp, tcp) = port_rx.recv().expect("server ports");
    let port = read_swallowing_proxy(udp, tcp).await;
    // SAFETY: `#[serial(epics_env)]`.
    unsafe {
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let db = Arc::new(PvDatabase::new());
    let reactor = epics_base_rs::runtime::task::Reactor::current().expect("test runtime");
    install_calink_resolver(&db, reactor).await;
    let ai = db
        .ca_client()
        .create_channel("CCGET:AI")
        .await
        .expect("a network half is registered");
    assert_eq!(
        within("never connected", ai.next_connection()).await,
        Some(Connection::Connected)
    );
    let mut got = ai.get(DBR_TIME_DOUBLE, 1);
    let early = tokio::time::timeout(Duration::from_secs(31), &mut got).await;
    assert!(early.is_err(), "the get ended unanswered: {early:?}");
    stop_tx.send(()).unwrap();
    let late = within("the disconnect did not end the get", got).await;
    assert!(late.is_err(), "an unanswered get read {late:?}");
    serving.join().unwrap();
    // SAFETY: `#[serial(epics_env)]`.
    unsafe {
        std::env::remove_var("EPICS_CA_SERVER_PORT");
    }
}
