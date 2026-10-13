//! A port's `interruptAccept` is its records' databases', not the process's.
//!
//! C has one database per process, so `interruptAccept` is one flag
//! (`asynPortDriver.cpp:838` gates the param flush on it; the device
//! supports drop pre-accept, `devAsynInt32.c:545-556`). Here each database
//! owns a gate and a port reads the gates its records bound, so one
//! database's facility stopping must not silence another's records on the
//! same port, and a paused database's seed must wait for its own `run`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use asyn_rs::asyn_record::register_port;
use asyn_rs::param::{ParamType, ParamValue};
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::port_handle::PortHandle;
use asyn_rs::request::ParamSetValue;
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime;
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

fn port(name: &str) -> PortHandle {
    let mut base = PortDriverBase::new(name, 1, PortFlags::default());
    base.create_param("VAL", ParamType::Int32).unwrap();
    let (runtime, _join) =
        create_port_runtime(IntPort { base }, RuntimeConfig::default()).expect("port runtime");
    let handle = runtime.port_handle().clone();
    register_port(name, handle.clone()).expect("port name is free");
    drop(runtime);
    handle
}

/// One database with one `I/O Intr` ai on `port`, scan facility not yet run.
async fn database(rec: &str, port: &str) -> Arc<PvDatabase> {
    let db_text = format!(
        r#"record(ai, "{rec}") {{
            field(DTYP, "asynInt32")
            field(INP,  "@asyn({port},0)VAL")
            field(SCAN, "I/O Intr")
        }}"#
    );
    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
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

/// Two databases, both running, one port: A's facility stops (its owner
/// dropped); B's record still takes the driver's callbacks, A's does not.
/// This is the record pump's own gate (`scan_control().is_running()`); the
/// port's flush is what the next test pins.
#[epics_base_rs::epics_test]
async fn a_stopped_databases_record_does_not_process_while_the_others_does_on_one_port() {
    let handle = port("GATE_SHARED");
    let a = database("A:REC", "GATE_SHARED").await;
    let b = database("B:REC", "GATE_SHARED").await;
    a.scan_control().run();
    b.scan_control().run();

    publish(&handle, 1);
    assert!(value(&a, "A:REC", 1.0).await, "A takes the first callback");
    assert!(value(&b, "B:REC", 1.0).await, "B takes the first callback");

    a.scan_control().stop();
    publish(&handle, 2);
    assert!(
        value(&b, "B:REC", 2.0).await,
        "B is running: its record updates"
    );
    assert!(stays(&a, "A:REC", 2.0).await, "A is stopped: its does not");
}

/// A paused database on a port only it uses: the seed is retained, and the
/// database's own `run` delivers it once.
#[epics_base_rs::epics_test]
async fn a_paused_databases_seed_is_deferred_until_its_own_run() {
    let handle = port("GATE_SOLO");
    let b = database("S:REC", "GATE_SOLO").await;
    // Another database running must not release this one's seed: the port
    // is bound to this database's gate alone.
    let other = port("GATE_OTHER");
    let c = database("C:REC", "GATE_OTHER").await;
    c.scan_control().run();
    publish(&other, 1);
    assert!(value(&c, "C:REC", 1.0).await);
    b.scan_control().pause();

    publish(&handle, 7);
    assert!(
        stays(&b, "S:REC", 7.0).await,
        "paused: the callback is deferred"
    );

    b.scan_control().run();
    assert!(
        value(&b, "S:REC", 7.0).await,
        "run delivers the retained seed"
    );
}

/// How many values a sync subscriber on `handle` has been given.
fn count_deliveries(handle: &PortHandle) -> (Arc<std::sync::atomic::AtomicUsize>, impl Sized) {
    let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sink = n.clone();
    let sub = handle.interrupts().register_sync_callback(
        asyn_rs::interrupt::InterruptFilter {
            reason: Some(0),
            addr: Some(0),
            uint32_mask: None,
            iface: None,
        },
        move |_| {
            sink.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        },
    );
    (n, sub)
}

/// The driver's own flush — `call_param_callbacks` on the port — is what
/// stops for a stopped database and continues for a running one. Three
/// ports: one bound to A only, one to B only, one to both.
#[epics_base_rs::epics_test]
async fn the_ports_own_flush_follows_the_databases_its_records_bound() {
    use std::sync::atomic::Ordering::SeqCst;

    let (pa, pb, ps) = (port("GATE_PA"), port("GATE_PB"), port("GATE_PS"));
    let a = PvDatabase::new();
    let b = PvDatabase::new();
    pa.interrupts()
        .bind_gate(a.scan_control().interrupt_gate().clone());
    pb.interrupts()
        .bind_gate(b.scan_control().interrupt_gate().clone());
    ps.interrupts()
        .bind_gate(a.scan_control().interrupt_gate().clone());
    ps.interrupts()
        .bind_gate(b.scan_control().interrupt_gate().clone());
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
    assert_eq!(na.load(SeqCst), 1, "A's own port stops flushing");
    assert_eq!(nb.load(SeqCst), 2, "B's own port keeps flushing");
    assert_eq!(ns.load(SeqCst), 2, "a port bound to both follows B");
}

async fn settle() {
    epics_base_rs::runtime::task::sleep(Duration::from_millis(300)).await;
}
