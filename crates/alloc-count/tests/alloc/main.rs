//! The boundaries of the windows every `alloc` binary counts with. Each
//! worker thread is started before a window opens and ends after it closes,
//! so only the work it is told to do falls inside.

use std::cell::Cell;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use alloc_count::named::{self, Name};
use alloc_count::{Counting, Counts, exempt, thread};

#[global_allocator]
static GLOBAL: Counting = Counting;

const IDLE: u8 = 0;
const RUN: u8 = 1;
const DONE: u8 = 2;
const EXIT: u8 = 3;
const STARTING: u8 = 4;

/// A thread that runs `work` each time it is told to, and nothing else. Its
/// start, which allocates, is over before `start` returns.
struct Worker {
    state: Arc<AtomicU8>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Worker {
    fn start(name: Option<&str>, work: fn()) -> Worker {
        let state = Arc::new(AtomicU8::new(STARTING));
        let s = state.clone();
        let body = move || {
            s.store(IDLE, Ordering::Release);
            loop {
                match s.load(Ordering::Acquire) {
                    RUN => {
                        work();
                        s.store(DONE, Ordering::Release);
                    }
                    EXIT => return,
                    _ => std::thread::yield_now(),
                }
            }
        };
        let handle = match name {
            Some(name) => std::thread::Builder::new()
                .name(name.to_owned())
                .spawn(body)
                .unwrap(),
            None => std::thread::spawn(body),
        };
        while state.load(Ordering::Acquire) == STARTING {
            std::thread::yield_now();
        }
        Worker {
            state,
            handle: Some(handle),
        }
    }

    fn run(&self) {
        self.state.store(RUN, Ordering::Release);
        while self.state.load(Ordering::Acquire) != DONE {
            std::thread::yield_now();
        }
        self.state.store(IDLE, Ordering::Release);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.state.store(EXIT, Ordering::Release);
        self.handle.take().unwrap().join().unwrap();
    }
}

fn one() {
    drop(black_box(Box::new(0u64)));
}

fn two() {
    one();
    one();
}

#[test]
fn a_realloc_is_a_block_taken_and_a_block_given_back() {
    let window = thread::open();
    let mut v: Vec<u8> = black_box(Vec::with_capacity(8));
    assert_eq!(window.counts().allocs, 1);
    v.reserve_exact(64);
    drop(black_box(v));
    drop(black_box(vec![0u8; 16]));
    assert_eq!(
        window.close(),
        Counts {
            allocs: 3,
            frees: 3,
            deallocs: 2,
            largest: 64
        },
        "alloc 8, realloc to 64, dealloc, alloc_zeroed 16, dealloc"
    );
}

#[test]
fn a_thread_window_counts_only_its_own_thread() {
    let other = Worker::start(Some("ac-other"), two);
    let window = thread::open();
    other.run();
    assert_eq!(window.close(), Counts::default());
}

#[test]
fn a_window_opened_again_starts_from_nothing() {
    let (_, first) = thread::counted(one);
    one();
    let (_, again) = thread::counted(|| ());
    assert_eq!((first.allocs, again), (1, Counts::default()));
}

#[test]
#[should_panic(expected = "already open")]
fn a_second_window_on_one_thread_is_refused() {
    let _first = thread::open();
    let _second = thread::open();
}

#[test]
fn a_dropped_window_is_closed() {
    drop(thread::open());
    drop(named::open(&[Name::Exact("ac-dropped")]));
    let _thread = thread::open();
    let _named = named::open(&[Name::Exact("ac-dropped")]);
}

thread_local! {
    static HELD: Cell<bool> = const { Cell::new(false) };
}

fn held() -> bool {
    HELD.with(Cell::get)
}

#[test]
fn a_split_window_counts_apart_what_happens_while_its_predicate_holds() {
    let window = thread::open_split(held);
    let a = black_box(Box::new(1u8));
    HELD.with(|h| h.set(true));
    let b = black_box(Box::new(2u8));
    drop(a);
    HELD.with(|h| h.set(false));
    drop(b);
    let (all, under) = window.close_split();
    assert_eq!((all.allocs, all.frees), (2, 2));
    assert_eq!((under.allocs, under.frees), (1, 1));
}

#[test]
fn exempt_hides_a_call_from_both_windows_and_ends_with_its_outermost_call() {
    let window = thread::open();
    exempt(|| {
        exempt(one);
        one();
    });
    one();
    assert_eq!(window.close().allocs, 1);

    fn nested() {
        exempt(|| {
            exempt(one);
            one();
        });
        one();
    }
    let worker = Worker::start(Some("ac-exempt"), nested);
    let window = named::open(&[Name::Exact("ac-exempt")]);
    worker.run();
    assert_eq!(window.close()[0].allocs, 1);
}

#[test]
fn a_thread_counts_in_the_first_slot_it_matches_and_in_no_other() {
    let exact = Worker::start(Some("ac-one"), one);
    let prefixed = Worker::start(Some("ac-two"), two);
    let unnamed = Worker::start(None, two);
    let window = named::open(&[Name::Exact("ac-one"), Name::Prefix("ac-")]);
    exact.run();
    prefixed.run();
    unnamed.run();
    let [exact, prefixed] = window.close().map(|c| c.allocs);
    assert_eq!((exact, prefixed), (1, 2));
}

#[test]
fn any_counts_an_unnamed_thread() {
    let unnamed = Worker::start(None, two);
    let window = named::open(&[Name::Exact("ac-nobody"), Name::Any]);
    unnamed.run();
    let [nobody, any] = window.close().map(|c| c.allocs);
    // `Any` is every other thread of the process too, so only a floor.
    assert_eq!(nobody, 0);
    assert!(any >= 2, "{any}");
}

const LONG: &str = "ac-long-0123456789-0123456789-0123456789-0123456789-0123456789-0123456789";
/// `LONG`'s first 64 bytes, all a thread keeps of it.
const KEPT: &str = "ac-long-0123456789-0123456789-0123456789-0123456789-0123456789-0";

#[test]
fn a_name_longer_than_is_kept_matches_by_prefix_and_never_exactly() {
    assert_eq!(&LONG[..64], KEPT);
    let long = Worker::start(Some(LONG), one);
    let window = named::open(&[Name::Exact(KEPT), Name::Prefix(KEPT)]);
    long.run();
    let [exact, prefix] = window.close().map(|c| c.allocs);
    assert_eq!((exact, prefix), (0, 1));
}

#[test]
fn a_thread_is_matched_by_its_name_again_under_the_next_window() {
    let worker = Worker::start(Some("ac-again"), one);
    let window = named::open(&[Name::Exact("ac-again")]);
    worker.run();
    let first = window.close()[0].allocs;
    let window = named::open(&[Name::Exact("ac-elsewhere"), Name::Prefix("ac-ag")]);
    worker.run();
    let [elsewhere, again] = window.close().map(|c| c.allocs);
    assert_eq!((first, elsewhere, again), (1, 0, 1));
}

#[test]
fn a_second_named_window_waits_for_the_first() {
    static OPENED: AtomicBool = AtomicBool::new(false);
    let first = named::open(&[Name::Exact("ac-first")]);
    let second = std::thread::spawn(|| {
        let _second = named::open(&[Name::Exact("ac-second")]);
        OPENED.store(true, Ordering::Release);
    });
    std::thread::sleep(Duration::from_millis(100));
    assert!(!OPENED.load(Ordering::Acquire), "opened beside the first");
    drop(first);
    second.join().unwrap();
    assert!(OPENED.load(Ordering::Acquire));
}
