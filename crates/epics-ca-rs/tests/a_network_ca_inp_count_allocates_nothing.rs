//! A compress whose INP is a CA link to a remote PV latches INPN from the
//! channel's element count, `dbCaGetNelements`'s `pca->nelements`
//! (`dbCa.c:662-670`), and asking it allocates nothing on the scan thread,
//! whatever units and description the channel's metadata carries. Counted
//! with this binary's own allocator over 100 cycles. C softIoc, the source
//! a waveform NELM=16 holding [1,2,3], reads `INPN=16 NUSE=6 VAL=1 2 3 1 2
//! 3` after two passes.

#![cfg(tokio_backend)]
#![cfg(feature = "client-core")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::error::CaError;
use epics_base_rs::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::calink::CaLinkResolver;
use epics_ca_rs::client::{CaClient, CaClientConfig};
use epics_ca_rs::server::CaServerBuilder;
use serial_test::serial;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|n| n.set(n.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|n| n.set(n.get() + 1));
        }
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const SRC: &str = "NET:SRC";

const SERVED: &str = r#"
record(waveform, "NET:SRC") { field(FTVL, "DOUBLE") field(NELM, "16") field(EGU, "mm") field(DESC, "d") }
"#;

const DB: &str = r#"
record(compress, "CN") { field(INP, "NET:SRC CA") field(ALG, "Circular Buffer") field(NSAM, "40") }
"#;

/// A CA server for [`SERVED`], its UDP port.
async fn serve() -> u16 {
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
    let port = server.udp_port();
    tokio::spawn(async move { server.run().await });
    port
}

#[tokio::test(flavor = "multi_thread")]
#[serial(epics_env)]
async fn a_network_ca_inp_count_allocates_nothing() {
    let port = serve().await;
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
    assert!(
        resolver
            .wait_for_link_connected(SRC, fact_budget::FACT_BUDGET)
            .await,
        "{SRC} never connected"
    );
    let link = db
        .record_link_fields("CN")
        .into_iter()
        .find_map(|(field, _, link)| (field == "INP").then_some(link))
        .unwrap();
    let link = link.external().expect("a CA link");
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while db.external_link_metadata(link).is_none() {
        assert!(Instant::now() < deadline, "CN.INP has no metadata");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    for _ in 0..2 {
        db.process_record_with_links("CN", &mut ProcStack::new())
            .await
            .unwrap();
    }
    let num = |pv: &str| db.get_pv(pv).unwrap().to_f64().unwrap();
    assert_eq!((num("CN.INPN"), num("CN.NUSE")), (16.0, 6.0));
    assert_eq!(
        db.get_pv("CN").unwrap(),
        EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 1.0, 2.0, 3.0].into())
    );

    let (tx, rx) = std::sync::mpsc::channel();
    let scan = db.clone();
    MandatoryThread::new("scan-1", ThreadPriority::ScanLow, StackSizeClass::Big).spawn(move || {
        let cycle = |visited: &mut ProcStack| {
            scan.process_record_with_links_already_locked("CN", visited)
                .or_else(CaError::unless_process_status)
                .unwrap();
        };
        // The thread's first cycle sets up its thread-locals.
        cycle(&mut ProcStack::new());
        let mut visited = ProcStack::new();
        COUNTING.with(|c| c.set(true));
        for _ in 0..100 {
            cycle(&mut visited);
        }
        COUNTING.with(|c| c.set(false));
        tx.send(ALLOCS.with(Cell::get)).unwrap();
    });
    let allocs = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the cycle wedged");
    assert_eq!(allocs, 0);
}
