//! A port's `interruptAccept` is its owner database's, and only that.
//!
//! C has one database per process, so `interruptAccept` is one flag
//! (`asynPortDriver.cpp:838` gates the param flush on it; the device
//! supports drop pre-accept, `devAsynInt32.c:545-556`). Here each database
//! owns a gate and a port reads the gate of the database it was registered
//! to: its seed is delivered once, on that owner's open edge. A record of
//! another database on the port is delivered to whenever the owner accepts,
//! and its own database's running check decides whether it processes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use asyn_rs::asyn_record::register_port;
use asyn_rs::param::{ParamType, ParamValue};
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::port_handle::PortHandle;
use asyn_rs::request::ParamSetValue;
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime_unclaimed;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

struct IntPort {
    base: PortDriverBase,
}

impl PortDriver for IntPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

/// A port that is created but not yet registered: ownerless.
fn unregistered(name: &str) -> PortHandle {
    let mut base = PortDriverBase::new(
        name,
        1,
        PortFlags::default(),
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_INTERRUPTS,
    );
    base.create_param("VAL", ParamType::Int32).unwrap();
    let (runtime, _join) =
        create_port_runtime_unclaimed(IntPort { base }, RuntimeConfig::default())
            .expect("port runtime");
    let handle = runtime.port_handle().clone();
    drop(runtime);
    handle
}

/// A port registered to `owner`'s database.
fn port(name: &str, owner: &PvDatabase) -> PortHandle {
    let handle = unregistered(name);
    register_port(name, handle.clone(), owner.scan_control().interrupt_gate())
        .expect("port name is free");
    handle
}

/// A database made before its records: its gate has to exist for the port
/// the records bind to be registered to it.
fn new_database() -> Arc<PvDatabase> {
    Arc::new(PvDatabase::new())
}

/// Load one `I/O Intr` ai on `port` into `db`; the scan facility is not run.
async fn database(rec: &str, port: &str, db: Arc<PvDatabase>) -> Arc<PvDatabase> {
    let db_text = format!(
        r#"record(ai, "{rec}") {{
            field(DTYP, "asynInt32")
            field(INP,  "@asyn({port},0)VAL")
            field(SCAN, "I/O Intr")
        }}"#
    );
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(db)
        .db_string(&db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

fn publish(handle: &PortHandle, v: i32) {
    handle
        .set_params_and_notify_blocking(0, vec![ParamSetValue::new(0, 0, ParamValue::Int32(v))])
        .unwrap();
}

async fn value(db: &PvDatabase, rec: &str, want: f64) -> bool {
    for _ in 0..200 {
        if matches!(db.get_pv(rec), Ok(EpicsValue::Double(v)) if v == want) {
            return true;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    false
}

async fn stays(db: &PvDatabase, rec: &str, not: f64) -> bool {
    epics_base_rs::runtime::task::sleep(Duration::from_millis(300)).await;
    !matches!(db.get_pv(rec), Ok(EpicsValue::Double(v)) if v == not)
}

/// Two databases, both running, one port owned by A: B's facility stops.
/// The port still delivers (its owner accepts); B's record is held by B's own
/// running check — the record pump's gate (`scan_control().is_running()`) —
/// while A's takes the value.
#[epics_base_rs::epics_test]
async fn a_stopped_foreign_database_record_does_not_process_while_the_owners_does() {
    let (a, b) = (new_database(), new_database());
    let handle = port("GATE_SHARED", &a);
    let a = database("A:REC", "GATE_SHARED", a).await;
    let b = database("B:REC", "GATE_SHARED", b).await;
    a.scan_control().run();
    b.scan_control().run();

    publish(&handle, 1);
    assert!(value(&a, "A:REC", 1.0).await, "A takes the first callback");
    assert!(value(&b, "B:REC", 1.0).await, "B takes the first callback");

    b.scan_control().stop();
    publish(&handle, 2);
    assert!(
        value(&a, "A:REC", 2.0).await,
        "the owner is running: its record updates"
    );
    assert!(stays(&b, "B:REC", 2.0).await, "B is stopped: its does not");
}

/// A database that is built but not run: its port's seed is retained, and
/// the owner's `run` delivers it once. Another database running must not
/// release it.
#[epics_base_rs::epics_test]
async fn a_paused_databases_seed_is_deferred_until_its_own_run() {
    let b = new_database();
    let handle = port("GATE_SOLO", &b);
    let b = database("S:REC", "GATE_SOLO", b).await;
    let c = new_database();
    let other = port("GATE_OTHER", &c);
    let c = database("C:REC", "GATE_OTHER", c).await;
    c.scan_control().run();
    publish(&other, 1);
    assert!(value(&c, "C:REC", 1.0).await);

    publish(&handle, 7);
    assert!(
        stays(&b, "S:REC", 7.0).await,
        "not run: the callback is deferred"
    );

    b.scan_control().run();
    assert!(
        value(&b, "S:REC", 7.0).await,
        "run delivers the retained seed"
    );
}

/// Both databases build; the port belongs to B and A runs first. A's open
/// must neither deliver the port's seed nor consume it: B's own `run` is
/// the edge that delivers, to B's record and to A's.
#[epics_base_rs::epics_test]
async fn held_while_both_build() {
    let (a, b) = (new_database(), new_database());
    let handle = port("GATE_HELD", &b);
    let a = database("HA:REC", "GATE_HELD", a).await;
    let b = database("HB:REC", "GATE_HELD", b).await;

    publish(&handle, 7);
    a.scan_control().run();
    assert!(
        stays(&a, "HA:REC", 7.0).await && stays(&b, "HB:REC", 7.0).await,
        "A running does not open the port"
    );

    b.scan_control().run();
    assert!(
        value(&b, "HB:REC", 7.0).await,
        "B's run delivers the seed to B's record"
    );
    assert!(
        value(&a, "HA:REC", 7.0).await,
        "and to the running database's record on the same port"
    );
}

/// How many values a sync subscriber on `handle` has been given.
fn count_deliveries(handle: &PortHandle) -> (Arc<std::sync::atomic::AtomicUsize>, impl Sized) {
    let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sink = n.clone();
    let sub = handle
        .interrupts()
        .register_sync_callback(
            asyn_rs::interrupt::InterruptFilter {
                reason: Some(0),
                addr: Some(0),
                uint32_mask: None,
                iface: None,
                ..Default::default()
            },
            move |_| {
                sink.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            },
        )
        .unwrap();
    (n, sub)
}

/// The driver's own flush — `call_param_callbacks` on the port — follows the
/// port's owner: three ports, two owned by A and one by B.
#[epics_base_rs::epics_test]
async fn the_ports_own_flush_follows_its_owner() {
    use std::sync::atomic::Ordering::SeqCst;

    let (a, b) = (PvDatabase::new(), PvDatabase::new());
    let (pa, pb, ps) = (
        port("GATE_PA", &a),
        port("GATE_PB", &b),
        port("GATE_PS", &a),
    );
    let (na, _sa) = count_deliveries(&pa);
    let (nb, _sb) = count_deliveries(&pb);
    let (ns, _ss) = count_deliveries(&ps);
    a.scan_control().run();
    b.scan_control().run();

    for h in [&pa, &pb, &ps] {
        publish(h, 1);
    }
    settle().await;
    assert_eq!(
        (na.load(SeqCst), nb.load(SeqCst), ns.load(SeqCst)),
        (1, 1, 1)
    );

    a.scan_control().stop();
    for h in [&pa, &pb, &ps] {
        publish(h, 2);
    }
    settle().await;
    assert_eq!(na.load(SeqCst), 1, "A's port stops flushing");
    assert_eq!(nb.load(SeqCst), 2, "B's port keeps flushing");
    assert_eq!(ns.load(SeqCst), 1, "so does A's other port");
}

async fn settle() {
    epics_base_rs::runtime::task::sleep(Duration::from_millis(300)).await;
}

/// Database A is running when a port is created and seeded; the port is then
/// registered to database B, which is not. The seed waits for B's open, and
/// is delivered once: A's state is irrelevant to this port.
#[epics_base_rs::epics_test]
async fn a_port_seeded_while_another_database_runs_keeps_its_seed_until_its_owner_opens() {
    use std::sync::atomic::Ordering::SeqCst;

    let a = PvDatabase::new();
    a.scan_control().run();
    let b = PvDatabase::new();

    let handle = unregistered("GATE_LATE");
    let (n, _sub) = count_deliveries(&handle);
    publish(&handle, 7); // the seed: written before registration
    settle().await;
    assert_eq!(n.load(SeqCst), 0, "ownerless: closed whatever A does");

    register_port(
        "GATE_LATE",
        handle.clone(),
        b.scan_control().interrupt_gate(),
    )
    .unwrap();
    settle().await;
    assert_eq!(
        n.load(SeqCst),
        0,
        "owner B is closed; A running is no owner"
    );

    b.scan_control().run();
    for _ in 0..200 {
        if n.load(SeqCst) == 1 {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    settle().await;
    assert_eq!(n.load(SeqCst), 1, "B's open delivers the seed once");
}
