//! A CA link to a remote `DBR_ENUM` channel read as `DBR_STRING` serves the
//! server's string off a `DBR_STRING` monitor, which the first such read adds
//! and fails on (`dbCa.c:436-454`). An mbbi state with no name is `""`, not
//! the index digits a label table past its `no_str` gives.
//!
//! The string monitor's events are the link's alarm and time too, and a
//! reconnect that changes the channel's type drops the monitor
//! (`dbCa.c:878-903`).

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::error::CaError;
use epics_base_rs::server::database::{
    EnumString, LinkDbfType, LinkRef, LinkSet, ProcStack, PvDatabase,
};
use epics_base_rs::server::record::OpenLatch;
use epics_base_rs::server::records::bi::BiRecord;
use epics_base_rs::server::records::mbbi::MbbiRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::calink::CaLinkResolver;
use epics_ca_rs::client::{CaClient, CaClientConfig};
use epics_ca_rs::server::CaServer;
use serial_test::serial;

/// Point the ambient `EPICS_CA_*` env at `127.0.0.1:port`. Tests here are
/// `#[serial(epics_env)]` so the process-wide env is not raced.
fn pin_env(port: u16) {
    // SAFETY: serialized by `#[serial(epics_env)]`; no other thread
    // reads/writes these vars concurrently.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

fn text(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.as_str_lossy().into_owned(),
        other => panic!("{pv} is {other:?}"),
    }
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// A failed read is `devSiSoft`'s status, returned from `process`.
async fn process(db: &PvDatabase, rec: &str) {
    db.process_record_with_links(rec, &mut ProcStack::new())
        .await
        .or_else(CaError::unless_process_status)
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_ca_link_reads_an_unnamed_mbbi_state_as_its_empty_string() {
    let mut src = MbbiRecord::new(2);
    src.zrst = "zero".into();
    src.onst = "one".into();
    let server = CaServer::builder()
        .port(0)
        .record("CALINK:ENUMSTR:SRC", src)
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let _server = tokio::spawn(async move { server.run().await });

    pin_env(port);
    let client = Arc::new(
        CaClient::new_with_config(CaClientConfig::default())
            .await
            .expect("CA client"),
    );
    let db = PvDatabase::new();
    let resolver = CaLinkResolver::with_client(client);
    db.register_link_set("ca", Arc::new(resolver.clone())).await;
    db.add_record("S", Box::new(StringinRecord::new("was")))
        .await
        .unwrap();
    db.put_pv_no_process("S.INP", EpicsValue::String("CALINK:ENUMSTR:SRC CA".into()))
        .await
        .unwrap();
    assert!(db.ca_link_init());
    db.setup_external_link_opens().await;
    db.process_cp_links_on_connect().await;
    assert!(
        resolver
            .wait_for_link_connected("CALINK:ENUMSTR:SRC", fact_budget::FACT_BUDGET)
            .await,
        "CA link must connect before record processing"
    );

    process(&db, "S").await;
    assert_eq!(text(&db, "S"), "was", "the first read fails");
    assert_eq!(num(&db, "S.STAT"), 14.0, "LINK");
    assert_eq!(num(&db, "S.SEVR"), 3.0, "INVALID");

    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    loop {
        process(&db, "S").await;
        if num(&db, "S.SEVR") == 0.0 || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(num(&db, "S.SEVR"), 0.0, "the string monitor delivers");
    assert_eq!(text(&db, "S"), "");
}

async fn until(mut done: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + fact_budget::FACT_BUDGET;
    while !done() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    true
}

/// `link`'s string read, retried until the string monitor has delivered.
async fn read_string(resolver: &CaLinkResolver, link: LinkRef<'_>) -> String {
    let mut got = None;
    let delivered = until(|| match LinkSet::get_cached_enum_string(resolver, link) {
        EnumString::Got(s) => {
            got = Some(s.as_str_lossy().into_owned());
            true
        }
        _ => false,
    })
    .await;
    assert!(delivered, "the string monitor delivers");
    got.unwrap()
}

async fn client_resolver(port: u16) -> CaLinkResolver {
    pin_env(port);
    let client = Arc::new(
        CaClient::new_with_config(CaClientConfig::default())
            .await
            .expect("CA client"),
    );
    CaLinkResolver::with_client(client)
}

/// C `eventCallback` overwrites `pca->stat`, `sevr` and `timeStamp` on every
/// event of either monitor (`dbCa.c:1001-1003`). This link is no native
/// input, so the string monitor is its only feed.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn the_string_monitors_event_is_the_links_alarm_and_time() {
    const PV: &str = "CALINK:STRSTAMP:SRC";
    let mut src = BiRecord::new(0);
    src.znam = "off".into();
    src.zsv = 2;
    let server = CaServer::builder()
        .port(0)
        .record(PV, src)
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let sdb = server.database().clone();
    let _server = tokio::spawn(async move { server.run().await });
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    sdb.process_record(PV).await.unwrap();
    let sevr = num(&sdb, &format!("{PV}.SEVR")) as i32;
    let stat = num(&sdb, &format!("{PV}.STAT")) as i32;
    assert!(sevr > 0, "the source is in alarm");

    let resolver = client_resolver(port).await;
    let open = OpenLatch::default();
    let link = LinkRef::new(PV.as_bytes(), &open);
    LinkSet::connect_link(&resolver, link, None).await;
    assert_eq!(read_string(&resolver, link).await, "off");

    assert_eq!(LinkSet::alarm_severity(&resolver, link), Some(sevr));
    assert_eq!(LinkSet::alarm_status(&resolver, link), Some(stat));
    let (secs, _, _) = LinkSet::time_stamp(&resolver, link).expect("the event's time");
    assert!(secs >= before, "the time of the processed source");
}

/// A reconnect that changes the channel's type drops the string monitor with
/// its `pvlOptInpString` and `pgetString` (`dbCa.c:878-903`): `dbcar` stops
/// showing it, and the next string read of the ENUM channel re-adds it and
/// fails until its first event.
#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_type_changing_reconnect_drops_the_string_monitor() {
    const PV: &str = "CALINK:STRTYPE:SRC";
    let server = CaServer::builder()
        .port(0)
        .pv(PV, EpicsValue::Enum(0))
        .build()
        .await
        .expect("CA server");
    let port = server.udp_port();
    let sdb = server.database().clone();
    let _server = tokio::spawn(async move { server.run().await });

    let resolver = client_resolver(port).await;
    let open = OpenLatch::default();
    let link = LinkRef::new(PV.as_bytes(), &open);
    LinkSet::connect_link(&resolver, link, None).await;
    read_string(&resolver, link).await;
    let input_string = async || {
        LinkSet::link_diagnostics(&resolver, link)
            .await
            .expect("an opened link reports")
            .input_string
    };
    assert!(
        input_string().await,
        "the read asked for the string monitor"
    );

    let redefine = async |value: EpicsValue, dbf: LinkDbfType| {
        sdb.remove_simple_pv(PV).await.expect("served");
        assert!(
            until(|| !LinkSet::is_connected(&resolver, link)).await,
            "the link sees the outage"
        );
        sdb.add_pv(PV, value).await.unwrap();
        assert!(
            until(
                || LinkSet::link_metadata(&resolver, link).is_some_and(|m| m.dbf_type == Some(dbf))
            )
            .await,
            "the link reconnects as {dbf:?}"
        );
    };
    redefine(EpicsValue::Double(1.5), LinkDbfType::Double).await;
    assert!(
        !input_string().await,
        "the type change cleared pvlOptInpString"
    );

    redefine(EpicsValue::Enum(1), LinkDbfType::Enum).await;
    assert!(
        matches!(
            LinkSet::get_cached_enum_string(&resolver, link),
            EnumString::Pending
        ),
        "the first string read re-adds the monitor and fails"
    );
    read_string(&resolver, link).await;
}
