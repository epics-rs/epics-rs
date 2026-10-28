//! A driver's value notify runs inline under a record's hold. An Enum
//! param's value fire also reaches the record's asynEnum client, carrying
//! the table it already applied; N of them on the notify thread allocate
//! nothing. A table change may allocate: it is a configuration event.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use asyn_rs::asyn_record::register_port;
use asyn_rs::error::AsynResult;
use asyn_rs::interrupt::{InterruptFilter, InterruptValue};
use asyn_rs::param::{EnumEntry, ParamType, ParamValue};
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime_unclaimed;
use asyn_rs::user::AsynUser;
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
    fn read_enum(&mut self, user: &AsynUser) -> AsynResult<(usize, Arc<[EnumEntry]>)> {
        self.base.get_enum_param(user.reason, user.addr)
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
async fn enum_value_fires_with_a_ring_client_allocate_nothing_on_the_notify_thread() {
    const N: usize = 256;
    let mut base = PortDriverBase::new(
        "enumAlloc",
        1,
        PortFlags::default(),
        InterfaceMask::ENUM | InterfaceMask::DRV_USER,
        InterfaceMask::ENUM,
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
    let _ring = handle
        .interrupts()
        .register_sync_callback(
            InterruptFilter {
                reason: Some(0),
                addr: Some(0),
                uint32_mask: None,
                iface: None,
                ..Default::default()
            },
            move |_| {
                seen.fetch_add(1, Ordering::Relaxed);
            },
        )
        .unwrap();

    // The seeded table at a moving index: value callbacks, not table changes.
    let table: Arc<[EnumEntry]> = vec![entry("OFF", 0), entry("ON", 1)].into();
    let fires: Vec<InterruptValue> = (0..N)
        .map(|i| InterruptValue {
            reason: 0,
            addr: 0,
            value: ParamValue::Enum {
                index: i % 2,
                choices: table.clone(),
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
        "{N} enum value fires: allocations on the notify thread"
    );
}
