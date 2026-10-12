//! ERRS is C's `errs[ERR_SIZE]`, formatted in place by `reportError`
//! (asynRecord.c:2037), and the manager's diagnostic is C's fixed
//! `errorMessage` (asynManager.c:1194): every report a record makes under its
//! hold formats without allocating. Counted with this binary's own allocator
//! on the calling thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

use super::io_intr::NoInterface;
use super::{AsynRecord, ERR_SIZE, Errs, IoOutcome, MessageOrEmpty};
use crate::error::{AsynError, AsynStatus};

thread_local! {
    /// Allocation calls made by this thread, so concurrently running tests
    /// cannot move each other's count.
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

// SAFETY: forwards every call to `System` unchanged; the counter is a
// const-initialised thread-local `Cell`, which never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.with(|n| n.set(n.get() + 1));
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.with(|n| n.set(n.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const LONG: &str = "Error in GPIB Serial Poll read, a driver diagnostic long enough \
                    that ERR_SIZE cuts it before its end";

#[test]
fn every_errs_report_formats_without_allocating() {
    let mut rec = AsynRecord::default();
    let e = AsynError::Status {
        status: AsynStatus::Timeout,
        message: "port down".into(),
    };
    let written = EpicsValue::CharArray(b"client text\0\0".to_vec().into());
    let before = ALLOCS.with(Cell::get);

    rec.report_error(format_args!("Int32 read error, {}", e.message_display()));
    let int32 = rec.errs;
    rec.reset_error();
    rec.report_error(format_args!("{LONG}{LONG}"));
    let long = rec.errs;
    let mut out = IoOutcome::default();
    out.report_error(format_args!(
        "Overflow nread {} {}",
        5,
        MessageOrEmpty(Some(&e))
    ));
    rec.set_errs(out.errs.unwrap());
    let overflow = rec.errs;
    rec.report_error(format_args!("{}", NoInterface("asynInt32")));
    let iface = rec.errs;
    // The manager's own refusal, spliced the way `connectDevice` reports it.
    let _ = rec.device_user.is_connected();
    rec.report_connect_error();
    let connect = rec.errs;
    let _ = rec.queue_special();
    let queue = rec.errs;
    rec.put_field_internal("ERRS", written).unwrap();
    let client = rec.errs;

    let allocs = ALLOCS.with(Cell::get) - before;
    assert_eq!(allocs, 0);
    assert_eq!(int32, "Int32 read error, port down");
    assert_eq!(long.len(), ERR_SIZE - 1);
    assert!(LONG.repeat(2).starts_with(long.as_str()));
    assert_eq!(overflow, "Overflow nread 5 port down");
    assert_eq!(iface, "No asynInt32 interface");
    assert_eq!(
        connect,
        "Connect error, status=3, asynManager:isConnected asynUser not connected to device"
    );
    assert_eq!(
        queue,
        "asynManager::queueRequest asynUser not associated with a port"
    );
    assert_eq!(client, "client text");
    assert_eq!(Errs::EMPTY, "");
}
