//! An update that reads the channel's options encodes that read itself:
//! nothing is allocated on the delivering thread beyond the read.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use epics_base_rs::server::pv::MonitorEvent;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::snapshot::{DisplayInfo, PropertySupport, Snapshot};
use epics_base_rs::types::{DBR_GR_DOUBLE, EpicsValue};

use super::monitor::with_live_options;

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

#[test]
fn a_gr_update_allocates_nothing_beyond_the_read() {
    let mut read = Snapshot::new(EpicsValue::Double(1.0), 0, 0, std::time::UNIX_EPOCH);
    read.properties = PropertySupport::NUMERIC;
    read.display = Some(DisplayInfo {
        units: "mm".into(),
        precision: 3,
        upper_disp_limit: 10.0,
        ..Default::default()
    });
    let posted = Snapshot::new(EpicsValue::Double(2.5), 3, 1, std::time::UNIX_EPOCH);
    let event = MonitorEvent::from_snapshot(std::sync::Arc::new(posted), 0, EventMask::VALUE);
    let mut dst = Vec::with_capacity(256);
    let before = ALLOCS.with(Cell::get);
    let delivery = with_live_options(DBR_GR_DOUBLE, event, || Some(read)).expect("a read");
    delivery
        .encode_dbr_into(&mut dst, DBR_GR_DOUBLE)
        .expect("encodes");
    let allocs = ALLOCS.with(Cell::get) - before;
    drop(delivery);
    assert_eq!(allocs, 0);
    assert_eq!(&dst[..4], &[0, 3, 0, 1], "the log's status and severity");
}
