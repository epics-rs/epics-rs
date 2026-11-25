//! An IOC thread's first wait in tokio outside a runtime allocates nothing:
//! tokio builds the thread's parker there, and a port actor makes that wait
//! when its channel's `try_recv` meets a send half done, mid-cycle. A thread
//! without the prologue is the control that the wait counted builds it.

use alloc_count::thread;
use epics_libcom_rs::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};

/// The allocations of this thread's first tokio wait.
fn first_wait() -> u64 {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tx.send(()).unwrap();
    let (r, counts) = thread::counted(|| rx.blocking_recv());
    r.unwrap();
    counts.allocs
}

#[test]
fn the_first_tokio_wait_of_an_ioc_thread_allocates_nothing() {
    let control = std::thread::spawn(first_wait).join().unwrap();
    assert!(
        control > 0,
        "a plain thread's first wait no longer builds anything"
    );
    let (tx, rx) = std::sync::mpsc::channel();
    MandatoryThread::new("tokiowait", ThreadPriority::Low, StackSizeClass::Small)
        .spawn(move || tx.send(first_wait()).unwrap())
        .join()
        .unwrap();
    assert_eq!(rx.recv().unwrap(), 0);
}
