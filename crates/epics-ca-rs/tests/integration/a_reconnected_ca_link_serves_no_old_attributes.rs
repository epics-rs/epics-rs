//! A network CA link's CTRL attributes belong to one connection: C
//! `connectionCallback` clears `gotAttributes` at every connect
//! (`dbCa.c:925`) and every getter answers -1 until the new
//! `getAttribEventCallback` lands (`dbCa.c:730-800`, `:1104-1155`). A proxy
//! in front of two servers moves the link from a 16-element `mm` source to
//! an 8-element `um` one and holds the second connection's CTRL get.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use epics_base_rs::server::database::{
    LinkDbfType, LinkMetadata, LinkRef, LinkSet, LinkShape, PvDatabase,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::DBR_CTRL_DOUBLE;
use epics_ca_rs::calink::CaLinkResolver;
use epics_ca_rs::client::{CaClient, CaClientConfig};
use epics_ca_rs::protocol::{CA_PROTO_READ_NOTIFY, CA_PROTO_SEARCH, CaHeader};
use epics_ca_rs::server::CaServerBuilder;
use parking_lot::Mutex;
use serial_test::serial;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::watch;
use tokio::task::AbortHandle;

const SRC: &str = "NET:SRC";

const DB: &str = r#"
record(ao, "AO") { field(OUT, "NET:SRC CA") }
"#;

/// A CA server for one `NET:SRC` of `nelm` elements in `egu`: its UDP and
/// TCP addresses.
async fn serve(nelm: u32, egu: &str) -> (SocketAddr, SocketAddr) {
    let served = format!(
        r#"record(waveform, "{SRC}") {{ field(FTVL, "DOUBLE") field(NELM, "{nelm}") field(EGU, "{egu}") }}"#
    );
    let (db, _) = IocBuilder::new()
        .db_string(&served, &Default::default())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let acf = epics_base_rs::server::access_security::new_acf_cell_watching(None, &db);
    let server = CaServerBuilder::serving(db, acf, None, None)
        .port(0)
        .build()
        .await
        .expect("CA server");
    let at = |port: u16| SocketAddr::from(([127, 0, 0, 1], port));
    let addrs = (at(server.udp_port()), at(server.tcp_port()));
    tokio::spawn(async move { server.run().await });
    addrs
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

/// One client circuit through the proxy.
struct Circuit {
    held: Arc<AtomicUsize>,
    release: watch::Sender<()>,
    relays: [AbortHandle; 2],
}

/// A UDP+TCP proxy in front of whichever server `upstream` names, rewriting
/// the TCP port its SEARCH replies advertise to the proxy's own, and
/// holding the CTRL gets of each circuit until that circuit is released.
struct Proxy {
    search_port: u16,
    upstream: Arc<Mutex<(SocketAddr, SocketAddr)>>,
    circuits: Arc<Mutex<Vec<Circuit>>>,
}

impl Proxy {
    async fn start(upstream: (SocketAddr, SocketAddr)) -> Self {
        let udp = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let tcp = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let search_port = udp.local_addr().unwrap().port();
        let circuit_port = tcp.local_addr().unwrap().port();
        let upstream = Arc::new(Mutex::new(upstream));
        let circuits = Arc::new(Mutex::new(Vec::new()));

        let to = upstream.clone();
        tokio::spawn(async move {
            let up = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
            let mut buf = [0u8; 4096];
            loop {
                let Ok((n, client)) = udp.recv_from(&mut buf).await else {
                    return;
                };
                let search = to.lock().0;
                if up.send_to(&buf[..n], search).await.is_err() {
                    continue;
                }
                let mut reply = [0u8; 4096];
                let Ok(Ok((rn, _))) =
                    tokio::time::timeout(Duration::from_secs(2), up.recv_from(&mut reply)).await
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

        let to = upstream.clone();
        let list = circuits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((client, _)) = tcp.accept().await else {
                    return;
                };
                let circuit = to.lock().1;
                let Ok(server) = TcpStream::connect(circuit).await else {
                    continue;
                };
                let (cr, cw) = client.into_split();
                let (sr, sw) = server.into_split();
                let held = Arc::new(AtomicUsize::new(0));
                let (release, released) = watch::channel(());
                let up = tokio::spawn(relay_holding_ctrl(cr, sw, held.clone(), released));
                let down = tokio::spawn(relay(sr, cw));
                list.lock().push(Circuit {
                    held,
                    release,
                    relays: [up.abort_handle(), down.abort_handle()],
                });
            }
        });

        Self {
            search_port,
            upstream,
            circuits,
        }
    }

    /// CTRL gets held on circuit `n` (0 before it is accepted).
    fn held(&self, n: usize) -> usize {
        self.circuits
            .lock()
            .get(n)
            .map_or(0, |c| c.held.load(Ordering::SeqCst))
    }

    fn release(&self, n: usize) {
        self.circuits.lock()[n].release.send(()).unwrap();
    }

    /// Close circuit `n` on both sides, and answer the client's next search
    /// and circuit from `next`.
    fn move_to(&self, n: usize, next: (SocketAddr, SocketAddr)) {
        *self.upstream.lock() = next;
        for relay in &self.circuits.lock()[n].relays {
            relay.abort();
        }
    }
}

async fn until(what: &str, mut fact: impl FnMut() -> bool) {
    let deadline = epics_libcom_rs::runtime::time::deadline_from_now(fact_budget::FACT_BUDGET);
    while !fact() {
        assert!(Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_read_between_reconnect_and_the_new_ctrl_reply_serves_no_attributes() {
    let wide = serve(16, "mm").await;
    let narrow = serve(8, "um").await;
    let proxy = Proxy::start(wide).await;
    let port = proxy.search_port;
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
    db.process_cp_links_on_connect().await;
    let link = db
        .record_link_fields("AO")
        .into_iter()
        .find_map(|(field, _, link)| (field == "OUT").then_some(link))
        .unwrap();
    let link = link.external().expect("a CA link");
    let lref = LinkRef::new(link.name, link.open);
    let shape = |count| {
        Some(LinkShape {
            dbf_type: Some(LinkDbfType::Double),
            element_count: Some(count),
        })
    };
    let units = |meta: Option<LinkMetadata>| meta.and_then(|m| m.units);

    until("the first CTRL get never went out", || proxy.held(0) > 0).await;
    proxy.release(0);
    until("the first connection's attributes never landed", || {
        units(resolver.link_metadata(lref)).is_some()
    })
    .await;
    assert_eq!(
        resolver.link_metadata(lref).map(|m| m.shape()),
        shape(16),
        "the first connection's attributes"
    );
    assert_eq!(units(resolver.link_metadata(lref)).as_deref(), Some("mm"));

    proxy.move_to(0, narrow);
    until("the second CTRL get never went out", || proxy.held(1) > 0).await;
    until("the link never reconnected to the 8-element source", || {
        resolver.link_shape(lref) == shape(8)
    })
    .await;
    assert_eq!(
        resolver.link_metadata(lref),
        None,
        "the reconnected link served the first connection's attributes"
    );

    proxy.release(1);
    until("the second connection's attributes never landed", || {
        resolver.link_metadata(lref).is_some()
    })
    .await;
    let meta = resolver.link_metadata(lref).unwrap();
    assert_eq!(Some(meta.shape()), shape(8));
    assert_eq!(meta.units.as_deref(), Some("um"));
}
