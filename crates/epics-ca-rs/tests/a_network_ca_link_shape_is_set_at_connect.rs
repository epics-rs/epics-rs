//! A connected network CA link answers its DBF type and element count from
//! the channel, before its CTRL attribute reply is in: C `getDBFtype` /
//! `getElements` (`dbCa.c:633`, `:666`) read `pca->dbrType` and
//! `pca->nelements`, which `connectionCallback` sets (`dbCa.c:862-864`); the
//! `DBR_CTRL_DOUBLE` get only fills the attributes. A proxy in front of the
//! server holds that get until the test releases it.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use epics_base_rs::server::database::{LinkDbfType, LinkRef, LinkSet, LinkShape, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{DBR_CTRL_DOUBLE, EpicsValue};
use epics_ca_rs::calink::{CaLinkResolver, install_calink_resolver};
use epics_ca_rs::client::{CaClient, CaClientConfig};
use epics_ca_rs::protocol::{CA_PROTO_READ_NOTIFY, CA_PROTO_SEARCH, CaHeader};
use epics_ca_rs::server::CaServerBuilder;
use serial_test::serial;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::watch;

const SRC: &str = "NET:SRC";

const SERVED: &str = r#"
record(waveform, "NET:SRC") { field(FTVL, "DOUBLE") field(NELM, "16") }
"#;

const DB: &str = r#"
record(ao, "AO") { field(OUT, "NET:SRC CA") }
"#;

const CP_DB: &str = r#"
record(compress, "CC") { field(INP, "NET:SRC CP") field(ALG, "Circular Buffer") field(NSAM, "40") }
"#;

/// A CA server for [`SERVED`], its UDP and TCP ports.
async fn serve() -> (u16, u16) {
    let (db, _) = IocBuilder::new()
        .db_string(SERVED, &Default::default())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db.put_pv(SRC, EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0].into()))
        .await
        .unwrap();
    let acf = epics_base_rs::server::access_security::new_acf_cell_watching(None, &db);
    let server = CaServerBuilder::serving(db, acf, None, None)
        .port(0)
        .build()
        .await
        .expect("CA server");
    let ports = (server.udp_port(), server.tcp_port());
    tokio::spawn(async move { server.run().await });
    ports
}

/// Relay client-to-server frames, holding each `DBR_CTRL_DOUBLE`
/// `CA_PROTO_READ_NOTIFY` until `release` fires. No frame on this circuit is
/// long enough for an extended (0xffff) header.
async fn relay_holding_ctrl(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    held_count: Arc<AtomicUsize>,
    mut release: watch::Receiver<()>,
) {
    let mut buf = Vec::new();
    let mut held: Vec<Vec<u8>> = Vec::new();
    let mut released = false;
    let mut chunk = [0u8; 4096];
    loop {
        tokio::select! {
            read = from.read(&mut chunk) => {
                let n = match read {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&chunk[..n]);
            }
            // A dropped sender releases too.
            _ = release.changed(), if !released => {
                released = true;
                for frame in held.drain(..) {
                    if to.write_all(&frame).await.is_err() {
                        return;
                    }
                }
            }
        }
        while buf.len() >= CaHeader::SIZE {
            let cmmd = u16::from_be_bytes([buf[0], buf[1]]);
            let postsize = u16::from_be_bytes([buf[2], buf[3]]) as usize;
            let data_type = u16::from_be_bytes([buf[4], buf[5]]);
            let total = CaHeader::SIZE + postsize;
            if buf.len() < total {
                break;
            }
            let frame: Vec<u8> = buf.drain(..total).collect();
            if cmmd == CA_PROTO_READ_NOTIFY && data_type == DBR_CTRL_DOUBLE && !released {
                held_count.fetch_add(1, Ordering::SeqCst);
                held.push(frame);
                continue;
            }
            if to.write_all(&frame).await.is_err() {
                return;
            }
        }
    }
}

/// Relay server-to-client frames unchanged.
async fn relay(mut from: tokio::net::tcp::OwnedReadHalf, mut to: tokio::net::tcp::OwnedWriteHalf) {
    let _ = tokio::io::copy(&mut from, &mut to).await;
}

/// A UDP+TCP proxy in front of the server, rewriting the TCP port its
/// SEARCH replies advertise (header `data_type`) to the proxy's own. The
/// proxy's search port, the count of CTRL gets it holds, and their release.
async fn ctrl_holding_proxy(
    server_udp: u16,
    server_tcp: u16,
) -> (u16, Arc<AtomicUsize>, watch::Sender<()>) {
    let udp = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
    let tcp = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let search_port = udp.local_addr().unwrap().port();
    let circuit_port = tcp.local_addr().unwrap().port();
    let search: SocketAddr = format!("127.0.0.1:{server_udp}").parse().unwrap();
    let circuit: SocketAddr = format!("127.0.0.1:{server_tcp}").parse().unwrap();

    tokio::spawn(async move {
        let upstream = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let mut buf = [0u8; 4096];
        loop {
            let Ok((n, client)) = udp.recv_from(&mut buf).await else {
                return;
            };
            if upstream.send_to(&buf[..n], search).await.is_err() {
                continue;
            }
            let mut reply = [0u8; 4096];
            let Ok(Ok((rn, _))) =
                tokio::time::timeout(Duration::from_secs(2), upstream.recv_from(&mut reply)).await
            else {
                continue;
            };
            let mut off = 0usize;
            while off + CaHeader::SIZE <= rn {
                let cmmd = u16::from_be_bytes([reply[off], reply[off + 1]]);
                let postsize = u16::from_be_bytes([reply[off + 2], reply[off + 3]]) as usize;
                if cmmd == CA_PROTO_SEARCH {
                    reply[off + 4..off + 6].copy_from_slice(&circuit_port.to_be_bytes());
                }
                off += CaHeader::SIZE + postsize;
            }
            let _ = udp.send_to(&reply[..rn], client).await;
        }
    });

    let held = Arc::new(AtomicUsize::new(0));
    let (release, released) = watch::channel(());
    let count = held.clone();
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = tcp.accept().await else {
                return;
            };
            let Ok(upstream) = TcpStream::connect(circuit).await else {
                continue;
            };
            let (cr, cw) = client.into_split();
            let (sr, sw) = upstream.into_split();
            tokio::spawn(relay_holding_ctrl(cr, sw, count.clone(), released.clone()));
            tokio::spawn(relay(sr, cw));
        }
    });

    (search_port, held, release)
}

#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_network_ca_link_shape_is_set_at_connect() {
    let (server_udp, server_tcp) = serve().await;
    let (port, held, release) = ctrl_holding_proxy(server_udp, server_tcp).await;
    // SAFETY: every test touching the process-wide CA env is
    // `#[serial(epics_env)]`.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let client = CaClient::new_with_config(CaClientConfig::default())
        .await
        .expect("CA client");
    let resolver = CaLinkResolver::with_client(Arc::new(client));
    let db = Arc::new(PvDatabase::new());
    db.register_link_set("ca", Arc::new(resolver.clone())).await;
    let (db, _) = IocBuilder::new()
        .with_database(db)
        .db_string(DB, &Default::default())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db.setup_external_link_opens().await;
    assert!(
        resolver
            .wait_for_link_connected(SRC, fact_budget::FACT_BUDGET)
            .await,
        "{SRC} never connected"
    );
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while held.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the CTRL get never went out");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let link = db
        .record_link_fields("AO")
        .into_iter()
        .find_map(|(field, _, link)| (field == "OUT").then_some(link))
        .unwrap();
    let link = link.external().expect("a CA link");
    let lref = LinkRef::new(link.name, link.open);
    let served = Some(LinkShape {
        dbf_type: Some(LinkDbfType::Double),
        element_count: Some(16),
    });
    assert!(
        resolver.link_metadata(lref).is_none(),
        "the CTRL reply is held"
    );
    assert_eq!(resolver.link_shape(lref), served);

    release.send(()).unwrap();
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while resolver.link_metadata(lref).is_none() {
        assert!(
            Instant::now() < deadline,
            "the released CTRL reply never landed"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(resolver.link_shape(lref), served);
}

/// A CP compress processing on connect, before the CTRL reply, latches the
/// channel's count: C `compressRecord.c:327-331` asks `dbGetNelements`,
/// which is `pca->nelements`, set by `connectionCallback`.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_cp_compress_latches_the_count_before_the_ctrl_reply() {
    let (server_udp, server_tcp) = serve().await;
    let (port, held, release) = ctrl_holding_proxy(server_udp, server_tcp).await;
    // SAFETY: every test touching the process-wide CA env is
    // `#[serial(epics_env)]`.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    let (db, _) = IocBuilder::new()
        .db_string(CP_DB, &Default::default())
        .unwrap()
        .build()
        .await
        .unwrap();
    let resolver = install_calink_resolver(
        &db,
        epics_base_rs::runtime::task::Reactor::current().expect("test runtime"),
    )
    .await;
    db.initialize_link_locality().await;
    db.setup_external_link_opens().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    let num = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while num("CC.UDF") != 0.0 {
        assert!(Instant::now() < deadline, "CC never processed on connect");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let link = db
        .record_link_fields("CC")
        .into_iter()
        .find_map(|(field, _, link)| (field == "INP").then_some(link))
        .unwrap();
    let link = link.external().expect("a CA link");
    assert!(held.load(Ordering::SeqCst) > 0, "the CTRL get went out");
    assert!(
        resolver
            .link_metadata(LinkRef::new(link.name, link.open))
            .is_none(),
        "the CTRL reply is held"
    );
    assert_eq!(
        (num("CC.INPN"), num("CC.NUSE"), num("CC.SEVR")),
        (16.0, 3.0, 0.0)
    );
    assert_eq!(
        db.get_pv("CC").unwrap(),
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0].into())
    );
    drop(release);
}
