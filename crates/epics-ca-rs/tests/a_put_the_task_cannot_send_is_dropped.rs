//! A CA output put the link could not send because the channel went down
//! between staging and sending is dropped: C `dbCaTask` consumes the action
//! bits and skips the unconnected link (`dbCa.c:1195`, `:1225`), and no
//! connect re-arms them (`dbCa.c:556`, `:918-923`). `newOutNative` stays set,
//! so the next put counts in `nNoWrite` (`dbCa.c:582-583`). A dropped put
//! callback completes at once; C leaves it to `caLinkDec` on a relink
//! (`dbCa.c:184-197`), a C defect not copied.

#![cfg(feature = "client-core")]
#![cfg(tokio_backend)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::{
    CaLinkInit, CpTarget, LinkHolder, LinkOpen, LinkPutDone, LinkPutOp, LinkSet, PutAdmission,
    PvDatabase,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::calink::CaLinkResolver;
use epics_ca_rs::server::CaServer;
use serial_test::serial;
use tokio::sync::{Semaphore, mpsc};

/// The resolver, with its puts held at the lset's door until the test
/// closes `gate`: the window between `dbCaPutLink` staging a value and
/// `dbCaTask` sending it.
struct Held {
    inner: CaLinkResolver,
    gate: Arc<Semaphore>,
    entered: mpsc::UnboundedSender<()>,
}

#[async_trait::async_trait]
impl LinkSet for Held {
    fn is_connected(&self, name: &[u8]) -> bool {
        self.inner.is_connected(name)
    }
    fn init_ready(&self, name: &[u8]) -> bool {
        self.inner.init_ready(name)
    }
    async fn get_value(&self, name: &[u8]) -> Option<EpicsValue> {
        self.inner.get_value(name).await
    }
    fn open_link(&self, holder: LinkHolder<'_>, name: &[u8]) -> LinkOpen {
        self.inner.open_link(holder, name)
    }
    fn remove_link(&self, holder: LinkHolder<'_>, name: &[u8]) {
        self.inner.remove_link(holder, name)
    }
    async fn connect_link(&self, name: &[u8], init: Option<CaLinkInit>) {
        self.inner.connect_link(name, init).await
    }
    fn cp_link_added(&self, name: &[u8], holder: CpTarget) {
        self.inner.cp_link_added(name, holder)
    }
    fn put_admission(&self, name: &[u8]) -> PutAdmission {
        self.inner.put_admission(name)
    }
    async fn put_value_done(
        &self,
        name: &[u8],
        value: EpicsValue,
        op: LinkPutOp,
    ) -> (LinkPutDone, Result<(), String>) {
        let _ = self.entered.send(());
        // Closed is the release.
        let _ = self.gate.acquire().await;
        self.inner.put_value_done(name, value, op).await
    }
}

async fn until(budget: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    while !done() {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    true
}

/// Stage a put of 5 from `SRC` (DTYP `dtyp`) to `RMT`, take `RMT` off the
/// server before the put is sent, then put it back. Returns the IOC database
/// with its handle and the server's database, with `RMT` re-added at 0 and
/// the link connected to it again.
async fn put_across_an_outage(dtyp: &str) -> (Arc<PvDatabase>, impl Sized, Arc<PvDatabase>) {
    let server = CaServer::builder()
        .port(0)
        .pv("RMT", EpicsValue::Double(0.0))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let sdb = server.database().clone();
    tokio::spawn(async move { server.run().await });
    // SAFETY: `#[serial(epics_env)]` keeps every env-touching test apart.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }

    let db_text = format!(
        r#"
record(ao, "SRC") {{
    field(DTYP, "{dtyp}")
    field(OUT,  "RMT CA")
}}
"#
    );
    let (db, h) = IocBuilder::new()
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    let resolver = CaLinkResolver::new(
        epics_base_rs::runtime::task::Reactor::current().expect("test runtime"),
    );
    resolver.attach_database((*db).clone());
    let gate = Arc::new(Semaphore::new(0));
    let (entered, mut entered_rx) = mpsc::unbounded_channel();
    db.register_link_set(
        "ca",
        Arc::new(Held {
            inner: resolver.clone(),
            gate: gate.clone(),
            entered,
        }),
    )
    .await;
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.scan_control().run();
    db.process_cp_links_on_connect().await;
    assert!(
        resolver
            .wait_for_link_connected("RMT", fact_budget::FACT_BUDGET)
            .await,
        "the remote link connects"
    );

    write(&db, 5.0).await;
    tokio::time::timeout(Duration::from_secs(10), entered_rx.recv())
        .await
        .expect("the put reaches the lset");
    sdb.remove_simple_pv("RMT").await.expect("RMT served");
    assert!(
        until(Duration::from_secs(10), || !resolver.is_connected(b"RMT")).await,
        "the link sees the outage"
    );
    gate.close();
    // The put has met the outage and found no channel to go on.
    tokio::time::sleep(Duration::from_millis(200)).await;
    sdb.add_pv("RMT", EpicsValue::Double(0.0)).await.unwrap();
    assert!(
        resolver
            .wait_for_link_connected("RMT", fact_budget::FACT_BUDGET)
            .await,
        "the link reconnects"
    );
    (db, h, sdb)
}

/// A plain `SRC` process writing `v` — C `dbPutField` without a put
/// callback, so a `Soft Channel` OUT goes through `dbCaPutLink`.
async fn write(db: &PvDatabase, v: f64) {
    db.put_pv("SRC", EpicsValue::Double(v)).await.unwrap();
    db.process_record("SRC").await.unwrap();
}

fn rmt(sdb: &PvDatabase) -> f64 {
    sdb.get_pv("RMT")
        .ok()
        .and_then(|v| v.to_f64())
        .unwrap_or(f64::NAN)
}

/// Boundary — a plain put (`CA_PUT`): the reconnect writes nothing, and the
/// next put lands and counts the dropped one in `nNoWrite`.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_plain_put_not_sent_is_dropped() {
    let (db, _h, sdb) = put_across_an_outage("Soft Channel").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    db.sync_external_link_puts().await;
    assert_eq!(rmt(&sdb), 0.0, "the reconnect wrote the dropped put");
    assert_eq!(db.external_link_puts_coalesced(), 0);

    write(&db, 6.0).await;
    assert!(
        until(fact_budget::FACT_BUDGET, || rmt(&sdb) == 6.0).await,
        "the next put lands, RMT is {}",
        rmt(&sdb)
    );
    assert_eq!(
        db.external_link_puts_coalesced(),
        1,
        "the dropped put left newOutNative set"
    );
}

/// Boundary — a put callback (`CA_PUT_CALLBACK`, `devAoSoftCallback`): the
/// record completes when its put is dropped, without a relink, and the
/// reconnect writes nothing.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_put_callback_not_sent_completes_its_record() {
    let (db, _h, sdb) = put_across_an_outage("Async Soft Channel").await;
    let src = db.get_record("SRC").unwrap();
    assert!(
        until(Duration::from_secs(5), || !src.read().is_processing()).await,
        "SRC is still PACT after its put was dropped"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    db.sync_external_link_puts().await;
    assert_eq!(rmt(&sdb), 0.0, "the reconnect wrote the dropped put");
}
