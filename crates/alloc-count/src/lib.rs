//! The counting allocator every crate's `alloc` test binary installs.
//!
//! A crate's `tests/alloc/main.rs` makes [`Counting`] its
//! `#[global_allocator]`, and each module opens a window around the code whose
//! heap traffic it asserts on. Every heap call is classified one way for all
//! of them: `alloc`, `alloc_zeroed` and `realloc` take a block, `dealloc` and
//! `realloc` give one back. A realloc is both, as the default
//! `GlobalAlloc::realloc` is an alloc and a dealloc, so `allocs - frees` is
//! always the change in live blocks.
//!
//! Two windows:
//!
//! * [`thread::open`] counts the calling thread and nothing else.
//! * [`named::open`] counts every thread whose name matches, one slot per
//!   [`named::Name`], for code on threads the test does not run (a scan
//!   thread, a port actor, a callback band). It is process-wide, so it is
//!   only as private as the process: nextest runs each test in a process of
//!   its own, which is what keeps one test's threads out of another's window.
//!   Two named windows never overlap; the second `open` waits for the first.
//!
//! [`exempt`] takes a stretch of the calling thread out of both.
//!
//! Nothing here allocates from inside the allocator: the per-thread state is a
//! const-initialised thread-local with no destructor, and a heap call made
//! while this crate is itself at work on a thread (its name lookup, a split
//! predicate) is not counted.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

/// The global allocator: [`System`], with every call counted into whichever
/// windows are open.
pub struct Counting;

// SAFETY: every call is forwarded to `System` unchanged; counting touches
// only const-initialised thread-local `Cell`s and atomics, which never
// allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(1, 0, layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(1, 0, layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(1, 1, new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note(0, 1, 0);
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// What a window counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Blocks taken: `alloc`, `alloc_zeroed` and `realloc` calls.
    pub allocs: u64,
    /// Blocks given back: `dealloc` and `realloc` calls.
    pub frees: u64,
    /// The largest block taken, in bytes (a realloc's new size).
    pub largest: usize,
}

/// Runs `f` with the calling thread counted by no window.
pub fn exempt<T>(f: impl FnOnce() -> T) -> T {
    struct Loud;
    impl Drop for Loud {
        fn drop(&mut self) {
            let _ = LOCAL.try_with(|l| l.quiet.set(l.quiet.get() - 1));
        }
    }
    LOCAL.with(|l| l.quiet.set(l.quiet.get() + 1));
    let _loud = Loud;
    f()
}

/// A window on the calling thread.
pub mod thread {
    use std::marker::PhantomData;

    use super::{Counts, LOCAL};

    /// Counts the thread that opened it until it is closed or dropped. One
    /// at a time per thread, and never sent to another.
    #[must_use = "a window counts only while it is held"]
    pub struct Window {
        _here: PhantomData<*const ()>,
    }

    /// Opens a window on this thread.
    ///
    /// # Panics
    ///
    /// If one is already open on it.
    pub fn open() -> Window {
        start(None)
    }

    /// Opens a window that also counts, apart, what this thread does while
    /// `when` returns true; [`Window::close_split`] returns both. A heap call
    /// `when` itself makes is not counted.
    ///
    /// # Panics
    ///
    /// If a window is already open on this thread.
    pub fn open_split(when: fn() -> bool) -> Window {
        start(Some(when))
    }

    fn start(when: Option<fn() -> bool>) -> Window {
        LOCAL.with(|l| {
            assert!(!l.open.get(), "a window is already open on this thread");
            l.all.reset();
            l.under.reset();
            l.when.set(when);
            l.open.set(true);
        });
        Window { _here: PhantomData }
    }

    impl Window {
        /// What it has counted so far.
        pub fn counts(&self) -> Counts {
            LOCAL.with(|l| l.all.get())
        }

        /// Closes it and returns what it counted.
        pub fn close(self) -> Counts {
            self.close_split().0
        }

        /// Closes it and returns what it counted in all, and of that what
        /// happened while its `when` held (zero for a window from [`open`]).
        pub fn close_split(self) -> (Counts, Counts) {
            LOCAL.with(|l| {
                l.open.set(false);
                (l.all.get(), l.under.get())
            })
        }
    }

    impl Drop for Window {
        fn drop(&mut self) {
            let _ = LOCAL.try_with(|l| {
                l.open.set(false);
                l.when.set(None);
            });
        }
    }

    /// Runs `f` in a window on this thread.
    pub fn counted<T>(f: impl FnOnce() -> T) -> (T, Counts) {
        let window = open();
        let out = f();
        (out, window.close())
    }
}

/// A window on every thread of the process whose name matches.
pub mod named {
    use std::sync::atomic::Ordering;
    use std::sync::{Condvar, Mutex, PoisonError};

    use super::{ARMED, Armed, Counts, MAX_SLOTS, NAME_CAP};

    /// Which threads a slot counts. A thread counts in the first slot it
    /// matches, and in none if it matches none.
    #[derive(Clone, Copy, Debug)]
    pub enum Name {
        /// A thread named exactly this.
        Exact(&'static str),
        /// A thread whose name starts with this.
        Prefix(&'static str),
        /// Any thread, named or not.
        Any,
    }

    /// Counts the threads its names match until it is closed or dropped.
    #[must_use = "a window counts only while it is held"]
    pub struct Window<const N: usize> {
        armed: &'static Armed,
        _turn: Turn,
    }

    /// Opens a window counting each thread in the slot of the first of
    /// `names` it matches. Waits while another named window is open.
    ///
    /// A thread's name is read once, on its first heap call while a named
    /// window is open, and kept: a thread's name never changes.
    pub fn open<const N: usize>(names: &'static [Name; N]) -> Window<N> {
        const { assert!(N <= MAX_SLOTS, "too many slots") };
        for name in names {
            if let Name::Exact(s) | Name::Prefix(s) = name {
                assert!(s.len() <= NAME_CAP, "{s:?} is longer than a kept name");
            }
        }
        let turn = Turn::take();
        let armed: &'static Armed = Box::leak(Box::new(Armed::new(names)));
        ARMED.store(std::ptr::from_ref(armed).cast_mut(), Ordering::Release);
        Window { armed, _turn: turn }
    }

    impl<const N: usize> Window<N> {
        /// What each slot has counted so far.
        pub fn counts(&self) -> [Counts; N] {
            std::array::from_fn(|i| self.armed.slots[i].get())
        }

        /// Closes it and returns what each slot counted.
        pub fn close(self) -> [Counts; N] {
            disarm(self.armed);
            self.counts()
        }
    }

    impl<const N: usize> Drop for Window<N> {
        fn drop(&mut self) {
            disarm(self.armed);
        }
    }

    // A thread that loaded the pointer just before this still counts into
    // `armed`, which is never freed and never read again by anyone else.
    fn disarm(armed: &'static Armed) {
        let _ = ARMED.compare_exchange(
            std::ptr::from_ref(armed).cast_mut(),
            std::ptr::null_mut(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    static TAKEN: Mutex<bool> = Mutex::new(false);
    static FREED: Condvar = Condvar::new();

    /// One named window at a time, held from any thread.
    struct Turn;

    impl Turn {
        fn take() -> Turn {
            let mut taken = TAKEN.lock().unwrap_or_else(PoisonError::into_inner);
            while *taken {
                taken = FREED.wait(taken).unwrap_or_else(PoisonError::into_inner);
            }
            *taken = true;
            Turn
        }
    }

    impl Drop for Turn {
        fn drop(&mut self) {
            *TAKEN.lock().unwrap_or_else(PoisonError::into_inner) = false;
            FREED.notify_one();
        }
    }
}

const MAX_SLOTS: usize = 4;
/// Bytes of a thread's name kept; a longer name matches only a prefix this
/// long or shorter.
const NAME_CAP: usize = 64;

/// The open named window. Leaked when opened, so a thread that read the
/// pointer just before the window closed counts into memory nobody frees.
static ARMED: AtomicPtr<Armed> = AtomicPtr::new(std::ptr::null_mut());

struct Armed {
    names: &'static [named::Name],
    slots: [Shared; MAX_SLOTS],
}

impl Armed {
    fn new(names: &'static [named::Name]) -> Self {
        Armed {
            names,
            slots: std::array::from_fn(|_| Shared::default()),
        }
    }

    fn slot(&self, name: Option<&[u8]>, cut: bool) -> Option<usize> {
        self.names.iter().position(|n| match *n {
            named::Name::Any => true,
            named::Name::Exact(s) => !cut && name == Some(s.as_bytes()),
            named::Name::Prefix(p) => name.is_some_and(|b| b.starts_with(p.as_bytes())),
        })
    }
}

#[derive(Default)]
struct Shared {
    allocs: AtomicU64,
    frees: AtomicU64,
    largest: AtomicUsize,
}

impl Shared {
    fn add(&self, allocs: u64, frees: u64, size: usize) {
        self.allocs.fetch_add(allocs, Ordering::Relaxed);
        self.frees.fetch_add(frees, Ordering::Relaxed);
        self.largest.fetch_max(size, Ordering::Relaxed);
    }

    fn get(&self) -> Counts {
        Counts {
            allocs: self.allocs.load(Ordering::Relaxed),
            frees: self.frees.load(Ordering::Relaxed),
            largest: self.largest.load(Ordering::Relaxed),
        }
    }
}

struct Tally {
    allocs: Cell<u64>,
    frees: Cell<u64>,
    largest: Cell<usize>,
}

impl Tally {
    const fn new() -> Self {
        Tally {
            allocs: Cell::new(0),
            frees: Cell::new(0),
            largest: Cell::new(0),
        }
    }

    fn add(&self, allocs: u64, frees: u64, size: usize) {
        self.allocs.set(self.allocs.get() + allocs);
        self.frees.set(self.frees.get() + frees);
        self.largest.set(self.largest.get().max(size));
    }

    fn get(&self) -> Counts {
        Counts {
            allocs: self.allocs.get(),
            frees: self.frees.get(),
            largest: self.largest.get(),
        }
    }

    fn reset(&self) {
        self.allocs.set(0);
        self.frees.set(0);
        self.largest.set(0);
    }
}

#[derive(Clone, Copy)]
enum Known {
    Unasked,
    Unnamed,
    /// The first `len` bytes of `Local::name`; `cut` if the name was longer.
    Named {
        len: usize,
        cut: bool,
    },
}

struct Local {
    /// Nonzero inside [`exempt`] and while this crate is at work here.
    quiet: Cell<u32>,
    open: Cell<bool>,
    when: Cell<Option<fn() -> bool>>,
    all: Tally,
    under: Tally,
    known: Cell<Known>,
    name: Cell<[u8; NAME_CAP]>,
}

impl Local {
    const fn new() -> Self {
        Local {
            quiet: Cell::new(0),
            open: Cell::new(false),
            when: Cell::new(None),
            all: Tally::new(),
            under: Tally::new(),
            known: Cell::new(Known::Unasked),
            name: Cell::new([0; NAME_CAP]),
        }
    }

    /// Runs `f` with this thread's heap calls uncounted.
    fn quietly<T>(&self, f: impl FnOnce() -> T) -> T {
        self.quiet.set(self.quiet.get() + 1);
        let out = f();
        self.quiet.set(self.quiet.get() - 1);
        out
    }

    fn known(&self) -> Known {
        if let Known::Unasked = self.known.get() {
            let known = self.quietly(|| match std::thread::current().name() {
                None => Known::Unnamed,
                Some(name) => {
                    let len = name.len().min(NAME_CAP);
                    let mut buf = [0; NAME_CAP];
                    buf[..len].copy_from_slice(&name.as_bytes()[..len]);
                    self.name.set(buf);
                    Known::Named {
                        len,
                        cut: name.len() > NAME_CAP,
                    }
                }
            });
            self.known.set(known);
        }
        self.known.get()
    }
}

thread_local! {
    static LOCAL: Local = const { Local::new() };
}

fn note(allocs: u64, frees: u64, size: usize) {
    let _ = LOCAL.try_with(|l| {
        if l.quiet.get() != 0 {
            return;
        }
        if l.open.get() {
            l.all.add(allocs, frees, size);
            if let Some(when) = l.when.get()
                && l.quietly(when)
            {
                l.under.add(allocs, frees, size);
            }
        }
        let armed = ARMED.load(Ordering::Acquire);
        if armed.is_null() {
            return;
        }
        // SAFETY: a published `Armed` is leaked, so it outlives every reader.
        let armed = unsafe { &*armed };
        let slot = match l.known() {
            Known::Unasked => unreachable!(),
            Known::Unnamed => armed.slot(None, false),
            Known::Named { len, cut } => armed.slot(Some(&l.name.get()[..len]), cut),
        };
        if let Some(slot) = slot {
            armed.slots[slot].add(allocs, frees, size);
        }
    });
}
