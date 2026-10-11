//! A record's posts run under its hold, so a steady-state burst — the posts,
//! the consumer's wake — may neither allocate nor free. C's
//! `db_queue_event_log` takes a free-list entry and signals `ppendsem`.
//! Counted with this binary's own allocator on the calling thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake};

use epics_base_rs::server::event_queue::{CircuitEvent, EventUser, PostBurst, attach};
use epics_base_rs::server::pv::MonitorEvent;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::EpicsValue;

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static CALLS: Cell<usize> = const { Cell::new(0) };
}

fn count() {
    if COUNTING.with(Cell::get) {
        CALLS.with(|n| n.set(n.get() + 1));
    }
}

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
        count();
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

struct Wakes(AtomicUsize);

impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn ev(v: i32) -> MonitorEvent {
    MonitorEvent {
        snapshot: Arc::new(Snapshot::new(
            EpicsValue::Long(v),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        )),
        origin: 0,
        mask: EventMask::VALUE,
    }
}

#[test]
fn a_steady_state_burst_allocates_and_frees_nothing() {
    let user = EventUser::new();
    let (a, _ra) = attach(&user, 1);
    let (b, _rb) = attach(&user, 2);
    let wakes = Arc::new(Wakes(AtomicUsize::new(0)));
    let waker = std::task::Waker::from(wakes.clone());
    let mut cx = Context::from_waker(&waker);
    let mut drain = || {
        let mut n = 0;
        while let Poll::Ready(CircuitEvent::Event { .. }) = user.poll_next(&mut cx) {
            n += 1;
        }
        n
    };

    for round in 0..4 {
        let events = [ev(round), ev(round + 100)];
        let [ea, eb] = events;
        CALLS.with(|n| n.set(0));
        COUNTING.with(|c| c.set(true));
        {
            let mut burst = PostBurst::new();
            a.post_in(&mut burst, ea);
            b.post_in(&mut burst, eb);
        }
        COUNTING.with(|c| c.set(false));
        if round > 0 {
            // Round 0 sizes the two monitors' entry lists; C's ring is
            // allocated with the queue.
            assert_eq!(CALLS.with(Cell::get), 0, "round {round}");
            assert_eq!(wakes.0.load(Ordering::SeqCst), round as usize);
        }
        assert_eq!(drain(), 2);
    }
}

/// The in-process shape: one reader on a private user, parked through
/// `poll_recv` as the QSRV group drain parks.
#[test]
fn a_parked_poll_recv_reader_costs_its_post_nothing() {
    let user = EventUser::new();
    let (sink, mut reader) = attach(&user, 1);
    let wakes = Arc::new(Wakes(AtomicUsize::new(0)));
    let waker = std::task::Waker::from(wakes.clone());
    let mut cx = Context::from_waker(&waker);
    for round in 0..4 {
        let event = ev(round);
        assert!(reader.poll_recv(&mut cx).is_pending());
        CALLS.with(|n| n.set(0));
        COUNTING.with(|c| c.set(true));
        sink.post(event);
        COUNTING.with(|c| c.set(false));
        if round > 0 {
            assert_eq!(CALLS.with(Cell::get), 0, "round {round}");
        }
        assert_eq!(wakes.0.load(Ordering::SeqCst), round as usize + 1);
        assert!(matches!(reader.poll_recv(&mut cx), Poll::Ready(Some(_))));
    }
}
