//! On a blocking port C's asynRecord completes through
//! `callbackRequestProcessCallback(&pasynRecPvt->callback, ...)`
//! (asynRecord.c:830-831), the `epicsCallback` its private structure owns
//! (:227): the port thread queues it and the callback thread re-enters the
//! record, neither allocating. Counted per thread with this binary's own
//! allocator, on the port's actor and the callback bands, over an IOC put
//! cycle.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use asyn_rs::asyn_record::AsynRecord;
use asyn_rs::error::AsynResult;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::registry::register_port;
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const PORT: &str = "REENTRY";

thread_local! {
    /// 0 not yet asked, 1 the port's actor, 2 a callback band, 3 any other
    /// thread. Asking may itself allocate; that reentry reads 3.
    static ROLE: Cell<u8> = const { Cell::new(0) };
}

static ARMED: AtomicBool = AtomicBool::new(false);
static ACTOR: AtomicU64 = AtomicU64::new(0);
static BAND: AtomicU64 = AtomicU64::new(0);

fn role() -> u8 {
    ROLE.with(|r| match r.get() {
        0 => {
            r.set(3);
            let role = match std::thread::current().name() {
                Some(PORT) => 1,
                Some(n) if n.starts_with("cb") => 2,
                _ => 3,
            };
            r.set(role);
            role
        }
        v => v,
    })
}

fn count() {
    if ARMED.load(Ordering::Relaxed) {
        match role() {
            1 => ACTOR.fetch_add(1, Ordering::Relaxed),
            2 => BAND.fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };
    }
}

struct Counting;

// SAFETY: forwards every call to `System` unchanged; the gate reads
// const-initialised thread-locals and atomics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

static READS: AtomicI32 = AtomicI32::new(0);

struct Device {
    base: PortDriverBase,
}

impl PortDriver for Device {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn read_int32(&mut self, _user: &AsynUser) -> AsynResult<i32> {
        Ok(READS.fetch_add(1, Ordering::Relaxed) + 1)
    }
}

/// The read the last completion applied, and whether the record is still busy.
fn state(db: &PvDatabase) -> (Option<EpicsValue>, bool) {
    let rec = db.get_record("A").unwrap();
    let inst = rec.read();
    (inst.record.get_field("I32INP"), inst.is_processing())
}

/// One put to PROC, waited out: the re-entry has applied the read and left.
async fn cycle(db: &PvDatabase, n: i32) {
    db.put_record_field_from_ca_no_notify("A", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while state(db) != (Some(EpicsValue::Long(n)), false) {
        assert!(Instant::now() < deadline, "cycle {n} never completed");
        std::thread::yield_now();
    }
}

#[epics_base_rs::epics_test]
async fn a_blocking_port_reentry_allocates_nothing() {
    let base = PortDriverBase::new(
        PORT,
        1,
        PortFlags {
            can_block: true,
            ..PortFlags::default()
        },
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_INTERRUPTS,
    );
    let (rt, _jh) =
        create_port_runtime_unclaimed(Device { base }, RuntimeConfig::default()).unwrap();
    assert!(
        rt.port_handle().can_block(),
        "precondition: a blocking port"
    );
    register_port(
        PORT,
        rt.port_handle().clone(),
        &asyn_rs::registry::open_owner(),
    )
    .unwrap();
    let (db, _) = IocBuilder::new()
        .register_record_type("asyn", || Box::new(AsynRecord::default()))
        .db_string(
            "record(asyn, \"A\") {\n  field(PORT, \"REENTRY\")\n  \
             field(IFACE, \"asynInt32\")\n  field(TMOD, \"Read\")\n}\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;

    // The first cycles resolve what each thread builds once.
    for n in 1..=3 {
        cycle(&db, n).await;
    }
    ARMED.store(true, Ordering::Relaxed);
    for n in 4..24 {
        cycle(&db, n).await;
    }
    ARMED.store(false, Ordering::Relaxed);
    assert_eq!(
        (ACTOR.load(Ordering::Relaxed), BAND.load(Ordering::Relaxed)),
        (0, 0),
        "20 re-entry cycles allocated (actor, callback band)"
    );
}
