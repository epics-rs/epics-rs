//! A local CP link on an I/O Intr asyn record reads its first event behind
//! the port's edge sweep, C `callbackThread::run` (asynPortDriver.cpp:923-937),
//! which on a CANBLOCK port runs behind the requests queued before the edge
//! and which `dbCaRun` does not wait for. On the quadEM IOCs a slow device let
//! `PoolUsedBuffers` read its UDF `PoolAllocBuffers` first and latch INVALID
//! in ACKS. A port that registers itself, as C's `drvAsyn*` ports do, has no
//! sweep and owes its links nothing. Until such a late link has its first
//! event, no CP process of its holder runs: `PoolUsedBuffers` has two, and the
//! first one in must not read the other unconnected. A link that goes late
//! once every CP process is released holds nothing, as C's links added
//! after `dbCaRun`.
#![cfg(feature = "epics")]

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use asyn_rs::error::AsynResult;
use asyn_rs::param::ParamType;
use asyn_rs::port::{InterfaceMask, PortDriver, PortDriverBase, PortFlags};
use asyn_rs::port_handle::PortHandle;
use asyn_rs::request::RequestOp;
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

/// While it reads `true`, a [`HeldPort`] write cannot finish.
type Hold = Arc<(Mutex<bool>, Condvar)>;

/// A CANBLOCK port whose writes are a device round trip, announced when it
/// starts, that waits out `hold`.
struct HeldPort {
    base: PortDriverBase,
    started: mpsc::Sender<()>,
    hold: Hold,
}

impl PortDriver for HeldPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn write_int32(&mut self, _user: &mut AsynUser, _value: i32) -> AsynResult<()> {
        let _ = self.started.send(());
        let (held, released) = &*self.hold;
        drop(released.wait_while(held.lock().unwrap(), |held| *held));
        Ok(())
    }
}

fn num(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

/// How long a fact the test waits for may take.
const BUDGET: Duration = Duration::from_secs(10);

/// Wait until `done`, failing after [`BUDGET`].
async fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + BUDGET;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(2)).await;
    }
}

/// A port built by `base` with an Int32 `VAL` seeded to `val`, registered
/// on `db`'s gate, whose writes wait out its hold.
struct Port {
    handle: PortHandle,
    param: usize,
    hold: Hold,
    started: mpsc::Receiver<()>,
    _runtime: Box<dyn std::any::Any>,
}

impl Port {
    fn new(
        db: &PvDatabase,
        name: &str,
        val: i32,
        base: fn(&str, usize, PortFlags, InterfaceMask, InterfaceMask) -> PortDriverBase,
    ) -> Self {
        let mut base = base(
            name,
            1,
            PortFlags {
                can_block: true,
                ..PortFlags::default()
            },
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        let param = base.create_param("VAL", ParamType::Int32).unwrap();
        base.set_int32_param(param, 0, val).unwrap();
        let hold: Hold = Arc::new((Mutex::new(true), Condvar::new()));
        let (started_tx, started) = mpsc::channel();
        let (rt, actor) = create_port_runtime_unclaimed(
            HeldPort {
                base,
                started: started_tx,
                hold: hold.clone(),
            },
            RuntimeConfig::default(),
        )
        .unwrap();
        let handle = rt.port_handle().clone();
        asyn_rs::registry::register_port(name, handle.clone(), db.scan_control().interrupt_gate())
            .unwrap();
        Self {
            handle,
            param,
            hold,
            started,
            _runtime: Box::new((rt, actor)),
        }
    }

    /// Queue a write and return once the device holds it.
    fn hold_a_write(&self) -> impl Sized + use<> {
        let write = self
            .handle
            .try_submit(
                RequestOp::Int32Write { value: 1 },
                AsynUser::new(self.param),
            )
            .unwrap();
        self.started.recv_timeout(BUDGET).unwrap();
        write
    }

    fn release(&self) {
        *self.hold.0.lock().unwrap() = false;
        self.hold.1.notify_all();
    }
}

/// `SRC<n>`: an I/O Intr longin on port `name`'s `VAL`.
fn source(n: &str, port: &str) -> String {
    format!(
        r#"record(longin, "SRC{n}") {{
    field(DTYP, "asynInt32") field(INP, "@asyn({port},0,1)VAL") field(SCAN, "I/O Intr")
}}
"#
    )
}

/// iocInit to `dbCaRun` over `records`, with a write held in each of
/// `ports`' queues from before the edge.
async fn boot(
    db: Arc<PvDatabase>,
    records: &str,
    links: u64,
    ports: &[&Port],
) -> (Arc<PvDatabase>, impl Sized + use<>) {
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(db)
        .db_string(records, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.initialize_link_locality().await;
    db.setup_cp_links().await;
    until("the links open", || {
        db.external_link_opens_completed() >= links
    })
    .await;
    let writes: Vec<_> = ports.iter().map(|port| port.hold_a_write()).collect();
    db.scan_control().run();
    epics_base_rs::runtime::task::timeout(BUDGET, db.process_cp_links_on_connect())
        .await
        .expect("dbCaRun returns while the device holds the port");
    (db, writes)
}

/// Boundary — an asynPortDriver port: no first event while its sweep waits
/// behind the held write; once it runs, the first event reads the source
/// it processed.
#[epics_base_rs::epics_test]
async fn the_first_event_reads_the_source_the_sweep_processed() {
    let db = Arc::new(PvDatabase::new());
    let port = Port::new(&db, "CPSWEEP_DRIVER", 5, PortDriverBase::new);
    let records = source("", "CPSWEEP_DRIVER")
        + r#"record(calc, "HOLD") { field(INPA, "SRC CP MS") field(CALC, "A") }"#;
    let (db, _writes) = boot(db, &records, 1, &[&port]).await;
    db.ca_sync().await;
    assert_eq!(num(&db, "HOLD.UDF"), 1.0, "no first event while held");

    port.release();
    until("the holder's first event", || num(&db, "HOLD.UDF") == 0.0).await;
    db.ca_sync().await;

    assert_eq!(num(&db, "HOLD.A"), 5.0);
    assert_eq!(
        (num(&db, "HOLD.SEVR"), num(&db, "HOLD.ACKS")),
        (0.0, 0.0),
        "the holder's first event read the swept source"
    );
}

/// Boundary — a self-registered port, which has no sweep: the first event
/// is read at `dbCaRun` while the write still holds the port.
#[epics_base_rs::epics_test]
async fn a_port_with_no_sweep_owes_its_links_nothing() {
    let db = Arc::new(PvDatabase::new());
    let port = Port::new(&db, "CPSWEEP_BARE", 5, PortDriverBase::new_exact_flags);
    let records = source("", "CPSWEEP_BARE")
        + r#"record(calc, "HOLD") { field(INPA, "SRC CP MS") field(CALC, "A") }"#;
    let (db, _writes) = boot(db, &records, 1, &[&port]).await;
    until("the holder's first event", || num(&db, "HOLD.UDF") == 0.0).await;
    assert!(*port.hold.0.lock().unwrap(), "read while the port is held");
    port.release();
}

/// Boundary — two late links of one holder on two ports, the second
/// released well after the first: the first link's event does not process
/// the holder while the second is unconnected.
#[epics_base_rs::epics_test]
async fn a_holder_waits_for_each_of_its_late_links() {
    let db = Arc::new(PvDatabase::new());
    let first = Port::new(&db, "CPSWEEP_FIRST", 5, PortDriverBase::new);
    let second = Port::new(&db, "CPSWEEP_SECOND", 7, PortDriverBase::new);
    let records = source("1", "CPSWEEP_FIRST")
        + &source("2", "CPSWEEP_SECOND")
        + r#"record(calc, "HOLD") { field(INPA, "SRC1 CP MS") field(INPB, "SRC2 CP MS") field(CALC, "A+B") }"#;
    let (db, _writes) = boot(db, &records, 2, &[&first, &second]).await;

    first.release();
    until("the first source's sweep", || num(&db, "SRC1.UDF") == 0.0).await;
    // Room for the first link's event to process the holder, were it let.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        (num(&db, "HOLD.UDF"), num(&db, "HOLD.ACKS")),
        (1.0, 0.0),
        "the holder waits for its second link"
    );

    second.release();
    until("the holder's process", || num(&db, "HOLD.UDF") == 0.0).await;
    db.ca_sync().await;
    assert_eq!(
        (
            num(&db, "HOLD.VAL"),
            num(&db, "HOLD.SEVR"),
            num(&db, "HOLD.ACKS")
        ),
        (12.0, 0.0, 0.0)
    );
}

/// Boundary — two late links of one holder on one port: the sweep releases
/// both at once, and neither's event processes the holder before the
/// other's is in.
#[epics_base_rs::epics_test]
async fn one_sweep_releases_a_holders_late_links_together() {
    let db = Arc::new(PvDatabase::new());
    let port = Port::new(&db, "CPSWEEP_SHARED", 5, PortDriverBase::new);
    let records = source("1", "CPSWEEP_SHARED")
        + &source("2", "CPSWEEP_SHARED")
        + r#"record(calc, "HOLD") { field(INPA, "SRC1 CP MS") field(INPB, "SRC2 CP MS") field(CALC, "A+B") }"#;
    let (db, _writes) = boot(db, &records, 2, &[&port]).await;

    port.release();
    until("the holder's process", || num(&db, "HOLD.UDF") == 0.0).await;
    db.ca_sync().await;
    assert_eq!(
        (
            num(&db, "HOLD.VAL"),
            num(&db, "HOLD.SEVR"),
            num(&db, "HOLD.ACKS")
        ),
        (10.0, 0.0, 0.0)
    );
}

/// Boundary — a holder with a live link and a late one on a silent
/// device: `dbCaRun` does not wait for the live link's held process, the
/// holder is not processed until the late link is in, and another holder
/// of the live link is processed meanwhile.
#[epics_base_rs::epics_test]
async fn a_silent_late_link_holds_only_its_own_holder() {
    let db = Arc::new(PvDatabase::new());
    let port = Port::new(&db, "CPSWEEP_SILENT", 5, PortDriverBase::new);
    let records = source("", "CPSWEEP_SILENT")
        + r#"
record(ao, "LIVE") { field(VAL, "3") }
record(calc, "HOLD") { field(INPA, "LIVE CP MS") field(INPB, "SRC CP MS") field(CALC, "A+B") }
record(calc, "OTHER") { field(INPA, "LIVE CP MS") field(CALC, "A") }
"#;
    let (db, _writes) = boot(db, &records, 3, &[&port]).await;

    until("the other holder's process", || {
        num(&db, "OTHER.UDF") == 0.0
    })
    .await;
    assert_eq!(num(&db, "OTHER.VAL"), 3.0);
    // Room for the live link's event to process the holder, were it let.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        (num(&db, "HOLD.UDF"), num(&db, "HOLD.ACKS")),
        (1.0, 0.0),
        "the holder waits for its late link"
    );

    port.release();
    until("the holder's process", || num(&db, "HOLD.UDF") == 0.0).await;
    db.ca_sync().await;
    assert_eq!(
        (
            num(&db, "HOLD.VAL"),
            num(&db, "HOLD.SEVR"),
            num(&db, "HOLD.ACKS")
        ),
        (8.0, 0.0, 0.0)
    );
}

/// Boundary — a holder with a late link and one to a source on a port with
/// no sweep, whose first event is at `dbCaRun`: that event does not process
/// the holder while the late link is unconnected.
#[epics_base_rs::epics_test]
async fn a_sibling_on_a_port_with_no_sweep_waits_for_the_late_link() {
    let db = Arc::new(PvDatabase::new());
    let late = Port::new(&db, "CPSWEEP_OWED", 5, PortDriverBase::new);
    let bare = Port::new(&db, "CPSWEEP_UNOWED", 7, PortDriverBase::new_exact_flags);
    let records = source("1", "CPSWEEP_OWED")
        + &source("2", "CPSWEEP_UNOWED")
        + r#"
record(calc, "HOLD") { field(INPA, "SRC1 CP MS") field(INPB, "SRC2 CP") field(CALC, "A+B") }
record(calc, "OTHER") { field(INPA, "SRC2 CP") field(CALC, "A") }
"#;
    let (db, _writes) = boot(db, &records, 3, &[&late, &bare]).await;

    until("the other holder's process", || {
        num(&db, "OTHER.UDF") == 0.0
    })
    .await;
    // Room for the unowed link's event to process the holder, were it let.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        (num(&db, "HOLD.UDF"), num(&db, "HOLD.ACKS")),
        (1.0, 0.0),
        "the holder waits for its late link"
    );

    late.release();
    until("the holder's process", || num(&db, "HOLD.UDF") == 0.0).await;
    db.ca_sync().await;
    assert_eq!(
        (
            num(&db, "HOLD.VAL"),
            num(&db, "HOLD.SEVR"),
            num(&db, "HOLD.ACKS")
        ),
        (5.0, 0.0, 0.0)
    );
    bare.release();
}

/// Boundary — a link made late once every CP process is released holds
/// none, as C's links added after `dbCaRun`: its holder's live link
/// processes the holder meanwhile.
#[epics_base_rs::epics_test]
async fn a_link_late_after_dbcarun_holds_nothing() {
    let db = Arc::new(PvDatabase::new());
    let port = Port::new(&db, "CPSWEEP_RELINK", 5, PortDriverBase::new);
    let records = source("", "CPSWEEP_RELINK")
        + r#"
record(ao, "LIVE") { field(VAL, "3") }
record(calc, "HOLD") { field(INPA, "LIVE CP MS") field(CALC, "A+B") }
"#;
    let (db, _writes) = boot(db, &records, 1, &[&port]).await;
    until("the holder's first event", || num(&db, "HOLD.UDF") == 0.0).await;

    db.put_record_field_from_ca("HOLD", "INPB", EpicsValue::String("SRC CP MS".into()))
        .await
        .unwrap();
    until("the late link opens", || {
        db.external_link_opens_completed() >= 2
    })
    .await;
    // Room for the late link to subscribe, were it to hold the holder.
    epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
    db.put_record_field_from_ca("LIVE", "VAL", EpicsValue::Double(4.0))
        .await
        .unwrap();
    until("the live link's process", || num(&db, "HOLD.A") == 4.0).await;

    port.release();
    until("the late link's process", || num(&db, "HOLD.VAL") == 9.0).await;
}
