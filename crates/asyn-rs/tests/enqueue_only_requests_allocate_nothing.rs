//! A request nobody waits on — `set_params_and_notify` and a
//! `CallbacksGate` section, a plugin's per-frame post — allocates nothing on
//! the posting thread: no reply channel and no cancel token, since nothing
//! answers it and nothing can cancel it or time it out.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use asyn_rs::param::{ParamType, ParamValue};
use asyn_rs::port::{InterfaceMask, PortDriver, PortDriverBase, PortFlags};
use asyn_rs::request::ParamSetValue;
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

// SAFETY: forwards every call to `System` unchanged; the flag is a
// const-initialised thread-local `Cell`, which never allocates.
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
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if COUNTING.try_with(Cell::get).unwrap_or(false) {
            ALLOCS.fetch_add(1, Ordering::SeqCst);
        }
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn counted<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let a0 = ALLOCS.load(Ordering::SeqCst);
    COUNTING.with(|c| c.set(true));
    let r = f();
    COUNTING.with(|c| c.set(false));
    (r, ALLOCS.load(Ordering::SeqCst) - a0)
}

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
}

/// A CANBLOCK port with one Int32 parameter, a count of its posts, and a
/// lock each post then takes, which stalls the actor while the test holds it.
fn port(
    name: &str,
) -> (
    asyn_rs::runtime::PortRuntimeHandle,
    usize,
    Arc<AtomicUsize>,
    Arc<Mutex<()>>,
) {
    let mut base = PortDriverBase::new(
        name,
        1,
        PortFlags {
            can_block: true,
            ..PortFlags::default()
        },
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_INTERRUPTS,
    );
    base.own_interrupts(InterruptGate::opened());
    let reason = base.create_param("VAL", ParamType::Int32).unwrap();
    let (runtime, _actor) =
        create_port_runtime_unclaimed(Device { base }, RuntimeConfig::default()).unwrap();
    let posts = Arc::new(AtomicUsize::new(0));
    let seen = posts.clone();
    let stall = Arc::new(Mutex::new(()));
    let stalled = stall.clone();
    let sub = runtime
        .port_handle()
        .interrupts()
        .register_sync_callback(
            asyn_rs::interrupt::InterruptFilter {
                reason: Some(reason),
                addr: Some(0),
                ..Default::default()
            },
            move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
                drop(stalled.lock());
            },
        )
        .unwrap();
    std::mem::forget(sub);
    (runtime, reason, posts, stall)
}

fn set(reason: usize, v: i32) -> Vec<ParamSetValue> {
    vec![ParamSetValue::new(reason, 0, ParamValue::Int32(v))]
}

const N: i32 = 256;

#[test]
fn set_params_and_notify_allocates_nothing_on_the_posting_thread() {
    let (runtime, reason, posts, _) = port("enqNotify");
    let handle = runtime.port_handle();
    let mut allocs = 0;
    for i in 1..=2 * N {
        let updates = set(reason, i);
        let (r, a) = counted(|| handle.set_params_and_notify_blocking(0, updates));
        r.unwrap();
        // The read queues behind the post, so the actor has run it.
        assert_eq!(handle.read_int32_blocking(reason, 0).unwrap(), i);
        if i > N {
            allocs += a;
        }
    }
    assert_eq!(posts.load(Ordering::SeqCst), 2 * N as usize);
    assert_eq!(allocs, 0, "{N} posts after {N} warm-up ones");
    runtime.shutdown();
}

/// The actor reaches each section's sets before it ends, so every end is
/// the held one that sends the callbacks on: both requests are counted, and
/// the sets, built in the `updates` the actor handed back.
#[test]
fn a_held_callbacks_gate_section_allocates_nothing_on_the_posting_thread() {
    let (runtime, reason, posts, _) = port("enqGate");
    let handle = runtime.port_handle();
    let mut gate = handle.callbacks_gate();
    let mut allocs = 0;
    for i in 1..=2 * N {
        let g = &mut gate;
        let (after, opened) = counted(move || {
            let mut updates = g.updates();
            updates.push(ParamSetValue::new(reason, 0, ParamValue::Int32(i)));
            g.open(0, None, updates).unwrap()
        });
        assert_eq!(handle.read_int32_blocking(reason, 0).unwrap(), i);
        assert_eq!(posts.load(Ordering::SeqCst), i as usize - 1, "held");
        let (r, ended) = counted(|| after.end());
        r.unwrap();
        // Any queued request runs after the held callbacks.
        handle.read_int32_blocking(reason, 0).unwrap();
        assert_eq!(posts.load(Ordering::SeqCst), i as usize, "fired at the end");
        if i > N {
            allocs += opened + ended;
        }
    }
    assert_eq!(allocs, 0, "{N} sections after {N} warm-up ones");
    runtime.shutdown();
}

/// The actor stalled behind a post while its caller queues `DEEP` sections:
/// once one backlog that deep has come back, later ones take every section's
/// `updates` from the ones handed back. The second backlog may still grow the
/// request queue by a block, which it then reuses too.
#[test]
fn a_backlog_of_sections_allocates_nothing_once_one_came_back() {
    const DEEP: usize = 16;
    const ROUNDS: usize = 4;
    let (runtime, reason, posts, stall) = port("enqBacklog");
    let handle = runtime.port_handle();
    let mut gate = handle.callbacks_gate();
    let mut allocs = Vec::new();
    for _ in 0..ROUNDS {
        let stalled = stall.lock().unwrap();
        let before = posts.load(Ordering::SeqCst);
        handle
            .set_params_and_notify_blocking(0, set(reason, 0))
            .unwrap();
        while posts.load(Ordering::SeqCst) == before {
            std::thread::yield_now();
        }
        let g = &mut gate;
        let ((), a) = counted(move || {
            for i in 1..=DEEP as i32 {
                let mut updates = g.updates();
                updates.push(ParamSetValue::new(reason, 0, ParamValue::Int32(i)));
                g.open(0, None, updates).unwrap().end().unwrap();
            }
        });
        drop(stalled);
        // The read queues behind every section.
        assert_eq!(handle.read_int32_blocking(reason, 0).unwrap(), DEEP as i32);
        assert_eq!(posts.load(Ordering::SeqCst), before + 1 + DEEP);
        allocs.push(a);
    }
    assert!(allocs[0] >= DEEP, "every section grew updates: {allocs:?}");
    assert!(
        allocs[2..].iter().all(|&a| a == 0),
        "backlogs of {DEEP} sections: {allocs:?}"
    );
    runtime.shutdown();
}

/// A section ended before the actor reaches its sets has its callbacks fired
/// by the actor, and its `updates` are back on the gate by then: a caller
/// those callbacks release builds its next section in them.
#[test]
fn an_ended_sections_updates_are_back_before_its_callbacks_fire() {
    const MARK: i32 = 7;
    let (runtime, reason, posts, stall) = port("enqBackFirst");
    let handle = runtime.port_handle();
    let fired = Arc::new(std::sync::Barrier::new(2));
    let at_mark = fired.clone();
    let sub = handle
        .interrupts()
        .register_sync_callback(
            asyn_rs::interrupt::InterruptFilter {
                reason: Some(reason),
                addr: Some(0),
                ..Default::default()
            },
            move |v| {
                if matches!(v.value, ParamValue::Int32(MARK)) {
                    at_mark.wait();
                    at_mark.wait();
                }
            },
        )
        .unwrap();
    let mut gate = handle.callbacks_gate();
    let stalled = stall.lock().unwrap();
    handle
        .set_params_and_notify_blocking(0, set(reason, 0))
        .unwrap();
    while posts.load(Ordering::SeqCst) == 0 {
        std::thread::yield_now();
    }
    let mut updates = gate.updates();
    updates.push(ParamSetValue::new(reason, 0, ParamValue::Int32(MARK)));
    gate.open(0, None, updates).unwrap().end().unwrap();
    drop(stalled);
    fired.wait();
    let spare = gate.updates();
    fired.wait();
    assert!(spare.capacity() > 0, "the section's updates came back late");
    drop(sub);
    runtime.shutdown();
}
