//! C `dbCaRun` waits for `initOutstanding`, one count per local CA link
//! added before it (`dbLink.c:127-129`, `dbCa.c:370-380`), each released by
//! that link's `CA_INIT_READY`. Here the count is a [`CaLinkInit`] the open
//! carries: staging creates it, and dropping it is the only report. A local
//! channel holds it until its first event's process; a name `dbChannelTest`
//! accepts and `dbChannel_create` refuses (`X.VAL$x`) carries it to the
//! network half, where a fake `ca` lset holds or drops it to put the report
//! at each boundary, the earliest being the moment the open reaches it.

mod calink_ioc;

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use epics_base_rs::server::database::{CaLinkInit, LinkSet, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{EpicsValue, PvString};

const DB: &str = r#"
record(ao, "X") {}
record(ao, "Y") {}
record(calc, "H") { field(INPA, "X CP") field(CALC, "A") }
record(calc, "HN") { field(INPA, "NOPE CP") field(CALC, "A") }
record(calc, "HR") { field(INPA, "X.VAL$x CP") field(CALC, "A") }
"#;

/// A `ca` lset that records each open with whether it carried an init, and
/// holds the init unless told to drop it on arrival.
struct InitLset {
    drop_on_open: bool,
    opens: parking_lot::Mutex<Vec<(String, bool)>>,
    held: parking_lot::Mutex<Vec<CaLinkInit>>,
}

#[async_trait::async_trait]
impl LinkSet for InitLset {
    fn is_connected(&self, _: &[u8]) -> bool {
        false
    }

    async fn get_value(&self, _: &[u8]) -> Option<EpicsValue> {
        None
    }

    async fn connect_link(&self, name: &[u8], init: Option<CaLinkInit>) {
        let name = String::from_utf8_lossy(name).into_owned();
        self.opens.lock().push((name, init.is_some()));
        if let Some(init) = init
            && !self.drop_on_open
        {
            self.held.lock().push(init);
        }
    }
}

impl InitLset {
    fn opened(&self, name: &str) -> Option<bool> {
        let opens = self.opens.lock();
        opens.iter().find(|(n, _)| n == name).map(|&(_, init)| init)
    }
}

async fn ioc(drop_on_open: bool) -> (Arc<PvDatabase>, Arc<InitLset>) {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let lset = Arc::new(InitLset {
        drop_on_open,
        opens: Default::default(),
        held: Default::default(),
    });
    db.register_link_set("ca", lset.clone()).await;
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    (db, lset)
}

fn poll_once<F: Future>(fut: std::pin::Pin<&mut F>) -> std::task::Poll<F::Output> {
    fut.poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
}

/// A local channel reports after its first event's process, so `dbCaRun`
/// returning is the barrier: the holder has already processed.
#[epics_macros_rs::epics_test]
async fn dbcarun_returns_after_a_local_links_first_process() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.put_pv("X", EpicsValue::Double(4.0)).await.unwrap();
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    db.process_cp_links_on_connect().await;
    assert_eq!(db.get_pv("H").unwrap().to_f64(), Some(4.0));
}

/// The report at the earliest point it can exist: the open drops its init
/// as it reaches the network half, so `dbCaRun` waits on `H`'s local link
/// alone.
#[epics_macros_rs::epics_test]
async fn a_link_ready_as_it_opens_leaves_dbcarun_nothing_to_wait_for() {
    let (db, lset) = ioc(true).await;
    calink_ioc::until("X.VAL$x opened", || lset.opened("X.VAL$x").is_some()).await;
    assert_eq!(
        lset.opened("X.VAL$x"),
        Some(true),
        "dbChannelTest accepts X.VAL$x: dbCaRun counts it"
    );
    epics_base_rs::runtime::task::timeout(
        fact_budget::FACT_BUDGET,
        db.process_cp_links_on_connect(),
    )
    .await
    .expect("dbCaRun returns with the network init dropped");
}

/// Held by the network half, the init keeps `dbCaRun` waiting; its drop
/// ends it.
#[epics_macros_rs::epics_test]
async fn dbcarun_waits_until_the_open_drops_its_init() {
    let (db, lset) = ioc(false).await;
    calink_ioc::until("X.VAL$x's init held", || !lset.held.lock().is_empty()).await;
    calink_ioc::until("H processed", || {
        db.get_pv("H").unwrap().to_f64() == Some(0.0) && lset.opened("NOPE").is_some()
    })
    .await;
    let run = db.process_cp_links_on_connect();
    let mut run = std::pin::pin!(run);
    assert!(poll_once(run.as_mut()).is_pending());
    lset.held.lock().clear();
    epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, run)
        .await
        .expect("dbCaRun returns once the init drops");
}

/// No count for a name the database does not serve, nor for a link added
/// after `dbCaRun`; a local name never reaches the network half.
#[epics_macros_rs::epics_test]
async fn only_a_local_link_staged_before_dbcarun_carries_an_init() {
    let (db, lset) = ioc(true).await;
    calink_ioc::until("NOPE opened", || lset.opened("NOPE").is_some()).await;
    assert_eq!(lset.opened("NOPE"), Some(false));
    db.process_cp_links_on_connect().await;
    db.put_record_field_from_ca(
        "H",
        "INPA",
        EpicsValue::String(PvString::from("Y.VAL$x CP")),
    )
    .await
    .unwrap();
    calink_ioc::until("Y.VAL$x opened", || lset.opened("Y.VAL$x").is_some()).await;
    assert_eq!(lset.opened("Y.VAL$x"), Some(false));
    assert_eq!(lset.opened("X"), None, "X is served in process");
}
