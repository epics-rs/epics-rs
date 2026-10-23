//! `CFloat` allocates nothing for a conversion that fits its stack text, so
//! a put of a number into a `DBF_STRING` field (`cvtDoubleToString`'s
//! `sprintf` fall-backs) stays allocation-free through it. Counted per thread
//! with this binary's own allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use epics_libcom_rs::cfmt::CFloat;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

static ALLOCS: AtomicU64 = AtomicU64::new(0);

struct Counting;

// SAFETY: forwards every call to `System` unchanged; the gate reads a
// const-initialised thread-local and the counter is an atomic.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.with(Cell::get) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if ARMED.with(Cell::get) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// A fixed sink, so the count sees only the conversion.
struct Sink {
    buf: [u8; 128],
    len: usize,
}

impl Write for Sink {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len + s.len();
        self.buf
            .get_mut(self.len..end)
            .ok_or(std::fmt::Error)?
            .copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

#[test]
fn every_conversion_that_fits_allocates_nothing() {
    let values = [
        0.0,
        -0.0,
        1.5,
        -2.5e-7,
        123456789.125,
        1e16,
        -1e-300,
        f64::NAN,
        -f64::NAN,
        f64::NEG_INFINITY,
    ];
    let mut sink = Sink {
        buf: [0; 128],
        len: 0,
    };
    ARMED.with(|a| a.set(true));
    for x in values {
        let x = CFloat(x);
        for p in [0usize, 3, 6, 17] {
            sink.len = 0;
            write!(sink, "{x:.p$}|{x:>24.p$e}|{x:.p$E}").unwrap();
            sink.len = 0;
            write!(sink, "{:.p$}|{:#012.p$}", x.g(), x.g()).unwrap();
        }
    }
    ARMED.with(|a| a.set(false));
    assert_eq!(ALLOCS.load(Ordering::Relaxed), 0);
}
