//! A CA link to a name the IOC's own database serves resolves in-process,
//! as C `dbCa` does through `dbContext` (`dbContext.cpp:87-121`): no search,
//! no circuit, and no CA client at all. The database's own `ca` entry serves
//! it; the installed resolver is the network half and never sees the name.
#![cfg(feature = "client-core")]

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use epics_base_rs::server::database::{LinkPutOp, LinkRef, PvDatabase};
use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::record::{OpenLatch, ParsedLink};
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::calink::{CaLinkResolver, install_calink_resolver, register_calink_commands};
use serial_test::serial;

/// Run `db_text` through iocInit with the `ca` link set installed and the
/// CA address list pointed at TEST-NET-1, where nothing answers, then hand
/// the database and the resolver to `body`.
async fn with_ioc<F, Fut>(db_text: &str, body: F)
where
    F: FnOnce(Arc<PvDatabase>, CaLinkResolver) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    // SAFETY: `#[serial(epics_env)]` keeps every env-touching test apart.
    unsafe {
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_ADDR_LIST", "192.0.2.1");
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("local.db");
    std::fs::write(&db_path, db_text).expect("write db");
    let stcmd_path = dir.path().join("st.cmd");
    std::fs::write(
        &stcmd_path,
        format!("dbLoadRecords(\"{}\")\n", db_path.display()),
    )
    .expect("write st.cmd");

    let slot = Arc::new(OnceLock::new());
    let installed = slot.clone();
    let reactor = epics_base_rs::runtime::task::Reactor::current().expect("test runtime");
    IocApplication::new()
        .startup_script(stcmd_path.to_str().unwrap())
        .register_link_type("ca", move |db| async move {
            let resolver = install_calink_resolver(&db, reactor).await;
            let _ = installed.set(resolver.clone());
            register_calink_commands(resolver)
        })
        .run(move |config| async move {
            let resolver = slot.get().expect("ca link set installed").clone();
            body(config.db.clone(), resolver).await;
            Ok(())
        })
        .await
        .expect("IOC run");
}

fn val(db: &PvDatabase, name: &str) -> Option<f64> {
    let rec = db.get_record(name).expect("record loaded");
    let inst = rec.read();
    inst.record.val().and_then(|v| v.to_f64())
}

fn sevr(db: &PvDatabase, name: &str) -> u16 {
    let rec = db.get_record(name).expect("record loaded");
    let inst = rec.read();
    inst.common.sevr as u16
}

/// `record`'s held `field` link.
fn held(db: &PvDatabase, record: &str, field: &str) -> ParsedLink {
    db.record_link_fields(record)
        .into_iter()
        .find_map(|(f, _, link)| (f == field).then_some(link))
        .expect("a link field")
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
}

/// The reported defect: with the address list pointed where nothing
/// answers, `RPRO:A`'s ` CA` link to its own IOC's `RPRO:B` never
/// connected, and an INP read left VAL 0 / SEVR INVALID. C serves the name
/// from `dbContext` and reads 5 with no alarm.
#[epics_macros_rs::epics_test]
#[serial(epics_env)]
async fn a_ca_link_to_a_local_record_connects_with_no_server_reachable() {
    with_ioc(
        "record(ai, \"RPRO:B\") { field(VAL, \"5\") }\n\
         record(ai, \"RPRO:A\") { field(INP, \"RPRO:B CA\") }\n",
        |db, resolver| async move {
            let ca = db.link_set("ca").await.expect("ca entry");
            // RPRO:A's own link, opened by iocInit's `dbCaAddLink`.
            let inp = held(&db, "RPRO:A", "INP");
            let link = LinkRef::new(b"RPRO:B", inp.external().unwrap().open);
            until("the local CA link never connected", || {
                ca.is_connected(link)
            })
            .await;
            db.process_record("RPRO:A").await.expect("process RPRO:A");
            assert_eq!(val(&db, "RPRO:A"), Some(5.0));
            assert_eq!(sevr(&db, "RPRO:A"), 0, "RPRO:A must carry no LINK alarm");
            // `dbContext` claimed the name, so the network context — and with
            // it the search that reported RPRO:B on two servers — never existed.
            assert!(
                resolver.client_connection_count().await.is_none(),
                "a local link must not create the CA client"
            );
        },
    )
    .await;
}

/// The in-process monitor: the link's cache follows each post to the
/// source, `dbSubscriptionIO` delivering it as `dbCa`'s `eventCallback`
/// would, the first event being the current value.
#[epics_macros_rs::epics_test]
#[serial(epics_env)]
async fn a_local_ca_link_follows_the_source_through_the_in_process_monitor() {
    with_ioc(
        "record(ai, \"LMON:SRC\") { field(VAL, \"1\") }\n",
        |db, resolver| async move {
            let ca = db.link_set("ca").await.expect("ca entry");
            let open = OpenLatch::default();
            let link = LinkRef::new(b"LMON:SRC", &open);
            ca.connect_link(link, None).await;
            let value = || ca.get_cached_value(link).and_then(|v| v.to_f64());
            until("the first event never arrived", || value() == Some(1.0)).await;
            for v in [7.0, 11.0] {
                db.put_pv_and_post("LMON:SRC", EpicsValue::Double(v))
                    .await
                    .expect("post the source");
                until("the link missed a source post", || value() == Some(v)).await;
            }
            assert!(resolver.client_connection_count().await.is_none());
        },
    )
    .await;
}

/// The in-process put: a plain put is `dbChannel_put`, a put callback
/// completes only once the target's processing has (`dbPutNotifyBlocker`).
#[epics_macros_rs::epics_test]
#[serial(epics_env)]
async fn a_local_ca_put_writes_the_target_plain_and_with_callback() {
    with_ioc(
        "record(ao, \"LPUT:TGT\") { field(VAL, \"0\") }\n",
        |db, resolver| async move {
            let ca = db.link_set("ca").await.expect("ca entry");
            let open = OpenLatch::default();
            let link = LinkRef::new(b"LPUT:TGT", &open);
            ca.connect_link(link, None).await;
            until("the local CA link never connected", || {
                ca.is_connected(link)
            })
            .await;
            ca.put_value(link, EpicsValue::Double(3.0), LinkPutOp::Plain)
                .await
                .expect("plain put");
            assert_eq!(val(&db, "LPUT:TGT"), Some(3.0));
            ca.put_value(link, EpicsValue::Double(9.0), LinkPutOp::Async)
                .await
                .expect("put callback");
            assert_eq!(val(&db, "LPUT:TGT"), Some(9.0));
            assert!(resolver.client_connection_count().await.is_none());
        },
    )
    .await;
}

/// A CP link put on `RL:H` after iocRun, to a name `RL:HP`'s link already
/// serves: C opens the new link its own `caLink`, whose first event
/// processes `RL:H` once (`dbCa.c:958-962`).
#[epics_macros_rs::epics_test]
#[serial(epics_env)]
async fn a_cp_link_added_to_a_served_name_processes_its_holder_once() {
    with_ioc(
        "record(ao, \"RL:X\") {}\n\
         record(calc, \"RL:HP\") { field(INPA, \"ca://RL:X CP\") field(CALC, \"VAL+1\") }\n\
         record(calc, \"RL:H\") { field(CALC, \"VAL+1\") }\n",
        |db, _resolver| async move {
            until("RL:HP never processed on connect", || {
                val(&db, "RL:HP") == Some(1.0)
            })
            .await;
            db.put_record_field_from_ca("RL:H", "INPA", EpicsValue::String("ca://RL:X CP".into()))
                .await
                .expect("put RL:H.INPA");
            until("RL:H never processed on its link's add", || {
                val(&db, "RL:H") == Some(1.0)
            })
            .await;
            db.put_record_field_from_ca("RL:X", "VAL", EpicsValue::Double(1.0))
                .await
                .expect("put RL:X");
            // RL:H's edge is RL:X's last, so this pass decided RL:HP's.
            until("RL:X's post never processed RL:H", || {
                val(&db, "RL:H") == Some(2.0)
            })
            .await;
            assert_eq!(val(&db, "RL:HP"), Some(2.0), "RL:HP once per post");
        },
    )
    .await;
}

/// `dbCaRun` returns for links that never become local channels or that
/// join one already open: `RI:R.VAL$x` names a local record but
/// `dbChannel_create` refuses it, so its link takes the network path, and
/// `RI:X` is opened by two links, `RI:X CP` and `ca://RI:X CP`.
#[epics_macros_rs::epics_test]
#[serial(epics_env)]
async fn iocinit_completes_over_refused_and_shared_local_names() {
    let ioc = with_ioc(
        "record(ao, \"RI:R\") {}\n\
         record(ao, \"RI:X\") { field(VAL, \"4\") }\n\
         record(calc, \"RI:HR\") { field(INPA, \"RI:R.VAL$x CP\") field(CALC, \"A\") }\n\
         record(calc, \"RI:H1\") { field(INPA, \"RI:X CP\") field(CALC, \"A\") }\n\
         record(calc, \"RI:H2\") { field(INPA, \"ca://RI:X CP\") field(CALC, \"A\") }\n",
        |db, resolver| async move {
            assert!(
                resolver.client_connection_count().await.is_some(),
                "RI:R.VAL$x took the network path"
            );
            // dbCaRun returned after both holders' first event.
            assert_eq!(val(&db, "RI:H1"), Some(4.0));
            assert_eq!(val(&db, "RI:H2"), Some(4.0));
        },
    );
    epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, ioc)
        .await
        .expect("iocInit's dbCaRun returned");
}
