//! An IOC thread's first drive allocates nothing: the thread's waker is built
//! on its first park, an `Arc` plus the node glibc callocs inside
//! `__cxa_thread_atexit_impl` to drop it at thread exit. Without
//! `enter_ioc_thread` building it at entry, both land in the first pass of a
//! fresh scan thread. The glibc node never reaches a `GlobalAlloc`, so the
//! registrations are counted where std makes them.

#![cfg(all(target_os = "linux", target_env = "gnu"))]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ffi::c_int;
use std::sync::{OnceLock, mpsc};

use epics_libcom_rs::runtime::task::{
    MandatoryThread, StackSizeClass, ThreadPriority, block_on_sync,
};

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
    static REGISTRATIONS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|n| n.set(n.get() + 1));
        }
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

type ThreadAtexit = unsafe extern "C" fn(unsafe extern "C" fn(*mut u8), *mut u8, *mut u8) -> c_int;

fn real_thread_atexit() -> ThreadAtexit {
    static REAL: OnceLock<usize> = OnceLock::new();
    let addr = *REAL.get_or_init(|| {
        // SAFETY: a NUL-terminated name; RTLD_NEXT finds glibc's definition.
        let p = unsafe { libc::dlsym(libc::RTLD_NEXT, c"__cxa_thread_atexit_impl".as_ptr()) };
        assert!(!p.is_null(), "glibc has no __cxa_thread_atexit_impl");
        p as usize
    });
    // SAFETY: glibc's symbol has this signature.
    unsafe { std::mem::transmute::<usize, ThreadAtexit>(addr) }
}

/// std registers each thread-local destructor here; this definition takes
/// the binary's reference to glibc's and forwards it.
#[unsafe(no_mangle)]
unsafe extern "C" fn __cxa_thread_atexit_impl(
    dtor: unsafe extern "C" fn(*mut u8),
    obj: *mut u8,
    dso: *mut u8,
) -> c_int {
    if COUNTING.with(Cell::get) {
        REGISTRATIONS.with(|n| n.set(n.get() + 1));
    }
    // SAFETY: the caller's arguments, forwarded unchanged.
    unsafe { real_thread_atexit()(dtor, obj, dso) }
}

const THREADS: usize = 4;

#[test]
fn the_first_drive_of_an_ioc_thread_allocates_nothing() {
    real_thread_atexit();
    for i in 0..THREADS {
        let (tx, rx) = mpsc::channel();
        MandatoryThread::new(
            format!("drive{i}"),
            ThreadPriority::Low,
            StackSizeClass::Small,
        )
        .spawn(move || {
            COUNTING.with(|c| c.set(true));
            // A scan thread's first pass, as `TickDriver::drive` makes it.
            block_on_sync(async {}).unwrap();
            COUNTING.with(|c| c.set(false));
            tx.send((ALLOCS.with(Cell::get), REGISTRATIONS.with(Cell::get)))
                .unwrap();
        })
        .join()
        .unwrap();
        let (allocs, registrations) = rx.recv().unwrap();
        assert_eq!(
            (allocs, registrations),
            (0, 0),
            "allocations and thread-exit registrations in the first drive of IOC thread {i}"
        );
    }
}
