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

/// Allocation calls this thread has made so far.
pub(super) fn allocs() -> u64 {
    ALLOCS.with(Cell::get)
}

#[test]
fn a_gr_update_allocates_nothing_beyond_the_read() {
    epics_base_rs::runtime::task::test_ioc_thread(
        a_gr_update_allocates_nothing_beyond_the_read_body,
    );
}

fn a_gr_update_allocates_nothing_beyond_the_read_body() {
    let mut read = Snapshot::new(EpicsValue::Double(1.0), 0, 0, std::time::UNIX_EPOCH);
    read.properties = PropertySupport::NUMERIC;
    read.display = Some(DisplayInfo {
        units: "mm".into(),
        precision: 3,
        upper_disp_limit: 10.0,
        ..Default::default()
    });
    let mut posted = Snapshot::new(EpicsValue::Double(2.5), 3, 1, std::time::UNIX_EPOCH);
    let event = MonitorEvent::read_log(&mut posted, EventMask::VALUE);
    let stage = super::DeliveryStage::plain();
    let mut dst = Vec::with_capacity(256);
    let (mut slot, mut log) = (None, None);
    let before = ALLOCS.with(Cell::get);
    let update = with_live_options(
        DBR_GR_DOUBLE,
        std::borrow::Cow::Owned(event),
        &mut slot,
        || Some(read),
    )
    .expect("a read");
    let delivery = update
        .prepare(&stage, 0, &mut log)
        .expect("an identity stage drops nothing");
    delivery
        .encode_dbr_into(&mut dst, DBR_GR_DOUBLE)
        .expect("encodes");
    let allocs = ALLOCS.with(Cell::get) - before;
    assert!(
        epics_base_rs::runtime::task::in_test_ioc_thread(),
        "counted off an IOC thread"
    );
    drop(delivery);
    assert_eq!(allocs, 0);
    assert_eq!(&dst[..4], &[0, 3, 0, 1], "the log's status and severity");
}
