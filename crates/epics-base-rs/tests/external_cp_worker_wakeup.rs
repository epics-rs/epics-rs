//! C `addAction` / `dbCaTask` (`dbCa.c:132-161`, `:1185-1195`): a
//! request made after the worker took the previous one and before it waits
//! again is still served, because the event keeps the signal for the next
//! wait. The holder's link read parks the worker inside that window while
//! another thread makes the next request.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use epics_base_rs::server::database::{ExternalCpProcess, LinkRef, LinkSet, PvDatabase};
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;

/// An lset serving the requester's latest value `cur`. A read of a value
/// above `open` records it in `entered` and parks until `open` reaches it.
struct CounterLset {
    cur: AtomicU64,
    entered: AtomicU64,
    open: AtomicU64,
}

impl CounterLset {
    fn new(open: u64) -> Arc<Self> {
        Arc::new(Self {
            cur: AtomicU64::new(0),
            entered: AtomicU64::new(0),
            open: AtomicU64::new(open),
        })
    }

    fn value(&self) -> Option<EpicsValue> {
        let v = self.cur.load(Ordering::Acquire);
        self.entered.fetch_max(v, Ordering::AcqRel);
        until(
            || self.open.load(Ordering::Acquire) >= v,
            "the read is opened",
        );
        Some(EpicsValue::Double(v as f64))
    }
}

/// Spin until `done`, failing after 5 s.
fn until(done: impl Fn() -> bool, what: &str) {
    let end = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < end, "timed out waiting until {what}");
        std::thread::yield_now();
    }
}

#[async_trait::async_trait]
impl LinkSet for CounterLset {
    fn is_connected(&self, _: LinkRef<'_>) -> bool {
        true
    }

    async fn get_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        self.value()
    }

    fn get_cached_value(&self, _: LinkRef<'_>) -> Option<EpicsValue> {
        self.value()
    }

    async fn connect_link(
        &self,
        _: LinkRef<'_>,
        _: Option<epics_base_rs::server::database::CaLinkInit>,
    ) {
    }
}

async fn holder_db(lset: Arc<CounterLset>) -> PvDatabase {
    let db = PvDatabase::new();
    db.register_link_set("ca", lset).await;
    db.add_record("HOLDER", Box::new(AiRecord::new(-1.0)))
        .await
        .unwrap();
    {
        let rec = db.get_record("HOLDER").unwrap();
        let mut inst = rec.write();
        inst.common.text.inp = "ca://UP:PV CP".into();
        inst.install_link_cache(
            "INP",
            Arc::new(epics_base_rs::server::record::parse_link_v2(
                "ca://UP:PV CP",
            )),
        )
        .expect("a common link");
    }
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    // C `dbCaRun`: the CP workers run from here.
    db.process_cp_links_on_connect().await;
    db
}

/// `HOLDER`'s database with a worker for `UP:PV` on a thread of its own;
/// The process of `HOLDER`'s INP link, the one its worker processes for.
fn holder_process(db: &PvDatabase) -> ExternalCpProcess {
    let (_, _, inp) = db.record_link_fields("HOLDER").remove(0);
    let inp = inp.external().unwrap();
    ExternalCpProcess::new(LinkRef::new(inp.name, inp.open))
}

/// the worker never returns, the test process ends it.
fn holder_with_worker(lset: &Arc<CounterLset>) -> (PvDatabase, Arc<ExternalCpProcess>) {
    let db = epics_base_rs::runtime::task::block_on_sync(holder_db(lset.clone()))
        .expect("a plain test thread");
    let process = Arc::new(holder_process(&db));
    {
        let (process, db) = (process.clone(), db.clone());
        std::thread::spawn(move || {
            let worker = process.run_worker(|| Some(db.clone()));
            epics_base_rs::runtime::task::block_on_sync(worker).expect("a plain worker thread");
        });
    }
    (db, process)
}

fn holder_reads(db: &PvDatabase, v: u64) -> bool {
    db.get_pv("HOLDER").ok() == Some(EpicsValue::Double(v as f64))
}

/// The request lands before the worker has ever waited.
#[test]
fn a_request_before_the_worker_first_waits_is_processed() {
    let lset = CounterLset::new(u64::MAX);
    let (db, process) = holder_with_worker(&lset);
    lset.cur.store(1, Ordering::Release);
    process.request();
    until(|| holder_reads(&db, 1), "request 1 is processed");
}

/// Request `n + 1` is made while the worker, having taken request `n`, is
/// parked in its read: after the take and before the next wait.
#[test]
fn every_request_made_between_the_take_and_the_wait_is_processed() {
    const REQUESTS: u64 = 2000;
    let lset = CounterLset::new(0);
    let (db, process) = holder_with_worker(&lset);
    // Let the worker reach its first wait; a request before it is the case
    // above.
    std::thread::sleep(Duration::from_millis(200));
    lset.cur.store(1, Ordering::Release);
    process.request();
    for n in 1..=REQUESTS {
        until(
            || lset.entered.load(Ordering::Acquire) >= n,
            &format!("request {n} is processed"),
        );
        if n < REQUESTS {
            lset.cur.store(n + 1, Ordering::Release);
            process.request();
        }
        lset.open.store(n, Ordering::Release);
    }
    until(|| holder_reads(&db, REQUESTS), "the last request lands");
}
