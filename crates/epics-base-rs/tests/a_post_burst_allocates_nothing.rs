//! A record's posts run under its hold, so a steady-state burst — the posts,
//! the consumer's wake — may neither allocate nor free. C's
//! `db_queue_event_log` takes a free-list entry and signals `ppendsem`.
//! Counted with this binary's own allocator on the calling thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake};

use epics_base_rs::server::event_queue::{
    CircuitEvent, EventUser, LogKind, PostBurst, PostOutcome, attach, event_que_size,
};
use epics_base_rs::server::pv::MonitorEvent;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::EpicsValue;

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static CALLS: Cell<usize> = const { Cell::new(0) };
    static FREES: Cell<usize> = const { Cell::new(0) };
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
        if COUNTING.with(Cell::get) {
            FREES.with(|n| n.set(n.get() + 1));
        }
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
    MonitorEvent::post_log(
        &Snapshot::new(EpicsValue::Long(v), 0, 0, std::time::SystemTime::UNIX_EPOCH),
        0,
        EventMask::VALUE,
    )
}

#[test]
fn a_steady_state_burst_allocates_and_frees_nothing() {
    let user = EventUser::new();
    let (a, _ra) = attach(&user, 1, LogKind::Val);
    let (b, _rb) = attach(&user, 2, LogKind::Val);
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
        // C's ring is allocated with the queue, so the first round is
        // steady state too.
        assert_eq!(CALLS.with(Cell::get), 0, "round {round}");
        if round > 0 {
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
    let (sink, mut reader) = attach(&user, 1, LogKind::Val);
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
        assert_eq!(CALLS.with(Cell::get), 0, "round {round}");
        assert_eq!(wakes.0.load(Ordering::SeqCst), round as usize + 1);
        assert!(matches!(reader.poll_recv(&mut cx), Poll::Ready(Some(_))));
    }
}

/// `f` under the counter: (allocator calls, frees).
fn counted<R>(f: impl FnOnce() -> R) -> (R, usize, usize) {
    CALLS.with(|n| n.set(0));
    FREES.with(|n| n.set(0));
    COUNTING.with(|c| c.set(true));
    let r = f();
    COUNTING.with(|c| c.set(false));
    (r, CALLS.with(Cell::get), FREES.with(Cell::get))
}

/// Every arm of C `db_queue_event_log` on a by-value monitor writes into the
/// ring made with the queue: append above the replace threshold, replace at
/// `rngSpace <= EVENTSPERQUE`, and replace under flow control.
#[test]
fn a_val_post_allocates_nothing_on_any_arm() {
    let user = EventUser::new();
    let (sink, mut reader) = attach(&user, 1, LogKind::Val);
    let mut appended = 0;
    let mut replaced = 0;
    for v in 0..event_que_size() as i32 {
        let event = ev(v);
        let (outcome, calls, _) = counted(|| sink.post(event));
        assert_eq!(calls, 0, "post {v}: {outcome:?}");
        match outcome {
            PostOutcome::Appended { .. } => appended += 1,
            PostOutcome::Replaced => replaced += 1,
            other => panic!("post {v}: {other:?}"),
        }
    }
    assert!(appended > 0 && replaced > 0, "both ring arms ran");
    while reader.try_recv().is_ok() {}
    user.flow_ctrl_on();
    for v in 0..3 {
        let event = ev(v);
        let (outcome, calls, _) = counted(|| sink.post(event));
        assert_eq!(calls, 0, "flow control post {v}: {outcome:?}");
    }
    assert_eq!(reader.npend(), 1, "flow control replaced in place");
}

fn owned(v: f64) -> MonitorEvent {
    MonitorEvent::read_log(
        &mut Snapshot::new(
            EpicsValue::DoubleArray(vec![v; 64].into()),
            0,
            0,
            std::time::SystemTime::UNIX_EPOCH,
        ),
        EventMask::VALUE,
    )
}

/// A filter's copy (`arr`) is freed only where C frees its `dtor` log: by the
/// replace that displaces it, or by the consumer after delivery.
#[test]
fn a_filter_copy_is_freed_on_replace_or_by_the_consumer_only() {
    let user = EventUser::new();
    let (sink, mut reader) = attach(&user, 1, LogKind::Ref);
    let first = owned(1.0);
    let (_, _, frees) = counted(|| sink.post(first));
    assert_eq!(frees, 0, "an append keeps the copy");
    user.flow_ctrl_on();
    let second = owned(2.0);
    let (outcome, _, frees) = counted(|| sink.post(second));
    assert_eq!(outcome, PostOutcome::Replaced);
    assert!(frees > 0, "the replace frees the copy it displaces");
    user.flow_ctrl_off();
    let (event, _, frees) = counted(|| reader.try_recv().unwrap());
    assert_eq!(frees, 0, "taking the entry moves the copy out");
    let (_, _, frees) = counted(|| drop(event));
    assert!(frees > 0, "the consumer frees it");
}
