//! A driver's enum-table notify runs inline under a record's hold. With the
//! record's enum client armed behind a real database and its consumer not
//! draining, N tables fired on the notify thread allocate nothing: the
//! client keeps the latest in the mailbox and coalesces, it does not queue.
//! The other half — a table fired while the gate is closed is dropped at
//! fire time — is `an_enum_table_before_the_gate_opens_is_not_applied_or_posted`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use asyn_rs::asyn_record::register_port;
use asyn_rs::interrupt::{InterruptFilter, InterruptValue};
use asyn_rs::param::{EnumEntry, ParamType, ParamValue};
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime_unclaimed;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if COUNTING.try_with(Cell::get).unwrap_or(false) {
            ALLOCS.fetch_add(1, Ordering::SeqCst);
        }
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static A: Counting = Counting;

struct EnumPort {
    base: PortDriverBase,
}

impl PortDriver for EnumPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

fn entry(s: &str, value: i32) -> EnumEntry {
    EnumEntry {
        string: s.to_string(),
        value,
        severity: 0,
    }
}

#[epics_base_rs::epics_test]
async fn enum_table_fires_with_a_ring_client_allocate_nothing_on_the_notify_thread() {
    const N: usize = 256;
    let mut base = PortDriverBase::new(
        "enumAlloc",
        1,
        PortFlags::default(),
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_ALL,
    );
    base.create_param("MODE", ParamType::Enum).unwrap();
    base.set_enum_choices_param(0, 0, vec![entry("OFF", 0), entry("ON", 1)].into())
        .unwrap();
    let (runtime, _join) =
        create_port_runtime_unclaimed(EnumPort { base }, RuntimeConfig::default())
            .expect("port runtime starts");
    let handle = runtime.port_handle().clone();
    let owner = Arc::new(PvDatabase::new());
    register_port(
        "enumAlloc",
        handle.clone(),
        owner.scan_control().interrupt_gate(),
    )
    .expect("port name is free");
    drop(runtime);

    let (database, _) =
        asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
            .with_database(owner)
            .db_string(
                r#"record(mbbi, "ENUM:ALLOC") {
                    field(DTYP, "asynInt32")
                    field(INP,  "@asyn(enumAlloc,0)MODE")
                    field(SCAN, "Passive")
                }"#,
                &HashMap::new(),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
    database.scan_control().run();

    // A ring-style synchronous client beside the record's enum client.
    let ring = Arc::new(AtomicUsize::new(0));
    let seen = ring.clone();
    let _ring = handle.interrupts().register_sync_callback(
        InterruptFilter {
            reason: Some(0),
            addr: Some(0),
            uint32_mask: None,
            iface: None,
        },
        move |_| {
            seen.fetch_add(1, Ordering::Relaxed);
        },
    );

    // Distinct tables, so each one is a change the client must hold.
    let fires: Vec<InterruptValue> = (0..N)
        .map(|i| InterruptValue {
            reason: 0,
            addr: 0,
            value: ParamValue::Enum {
                index: 0,
                choices: vec![entry("A", i as i32), entry("B", 1)].into(),
            },
            ..Default::default()
        })
        .collect();
    let mut it = fires.into_iter();
    let a0 = ALLOCS.load(Ordering::SeqCst);
    COUNTING.with(|c| c.set(true));
    for v in it.by_ref() {
        handle.interrupts().notify(v);
    }
    COUNTING.with(|c| c.set(false));
    let allocs = ALLOCS.load(Ordering::SeqCst) - a0;
    drop(it);

    // `>=`: the boot flush may land one more.
    assert!(
        ring.load(Ordering::Relaxed) >= N,
        "the ring client saw each"
    );
    assert_eq!(
        allocs, 0,
        "{N} enum fires: allocations on the notify thread"
    );
}
