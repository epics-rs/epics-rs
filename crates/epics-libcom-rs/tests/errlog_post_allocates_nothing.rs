//! Posting an errlog message MUST NOT allocate: C `errlogVprintf` formats into
//! the arena `errlogInitPvt` allocated once (`errlog.c:121-144`, `:592-593`),
//! and the caller may hold a record lock. Counted on the posting thread only, an
//! IOC thread as a record's is; the worker that drains the arena is not the
//! poster.
#![cfg(target_os = "linux")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use epics_libcom_rs::runtime::log::{
    ErrlogSevEnum, errlog_init, errlog_messages_lost, errlog_set_console, errlog_sev_vprintf,
    errlog_vprintf,
};
use epics_libcom_rs::runtime::task::{in_test_ioc_thread, test_ioc_thread};

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct PosterAllocs;

unsafe impl GlobalAlloc for PosterAllocs {
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
static GLOBAL: PosterAllocs = PosterAllocs;

fn allocations_in(f: impl FnOnce()) -> usize {
    ALLOCS.with(|n| n.set(0));
    COUNTING.with(|c| c.set(true));
    f();
    COUNTING.with(|c| c.set(false));
    assert!(in_test_ioc_thread(), "counted off an IOC thread");
    ALLOCS.with(Cell::get)
}

/// A full one-page pipe as the console: the worker blocks in its first write,
/// so the arena fills and the burst reaches the drop path whatever the
/// scheduling.
fn stalled_console() -> (std::io::PipeReader, std::fs::File) {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let (reader, writer) = std::io::pipe().expect("pipe");
    // SAFETY: F_SETPIPE_SZ on a pipe this test owns.
    assert!(unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETPIPE_SZ, 4096) } >= 4096);
    let mut console = std::fs::File::from(std::os::fd::OwnedFd::from(writer));
    console.write_all(&[b'.'; 4096]).expect("fill the pipe");
    (reader, console)
}

#[test]
fn a_post_allocates_nothing_whether_taken_or_dropped() {
    test_ioc_thread(posts_allocate_nothing);
}

fn posts_allocate_nothing() {
    errlog_init(0);
    let (reader, console) = stalled_console();
    errlog_set_console(console);
    // Each call site registers with `tracing` on its first use, once; the
    // worker contends only until it blocks in its write.
    errlog_vprintf(format_args!("warm\n"));
    errlog_sev_vprintf(ErrlogSevEnum::Major, format_args!("warm\n"));
    std::thread::sleep(std::time::Duration::from_millis(200));

    let value = 2.5;
    let name = "R:AI";
    let n = allocations_in(|| {
        for i in 0..200 {
            errlog_vprintf(format_args!("{name}.VAL {value} pass {i}\n"));
            errlog_sev_vprintf(ErrlogSevEnum::Major, format_args!("{name}: pass {i}\n"));
        }
    });
    let lost = errlog_messages_lost();
    drop(reader);
    assert_eq!(n, 0, "allocations on the posting thread");
    assert!(lost > 0, "the burst never reached the drop path");
}
