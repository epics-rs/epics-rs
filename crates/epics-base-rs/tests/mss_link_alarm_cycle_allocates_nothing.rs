//! An MSS input link copies its source's alarm message into the reader's
//! `namsg` every cycle, C `recGblInheritSevrMsg` → `recGblSetSevrMsg(..,
//! "%s", msg)` into a fixed `char[40]` (`recGbl.c:276-278`, `dbDbLink.c:229-
//! 232`). Here the copy shares the source's text and a steady message is a
//! pointer compare, so the reader's cycle allocates nothing — the message
//! changing included: only the source's own new text is an allocation.
//! Counted on a primed IOC thread with this binary's own allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::runtime::task::{MandatoryThread, StackSizeClass, ThreadPriority};
use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;

// SAFETY: forwards every call to `System` unchanged; the counters are
// const-initialised thread-local `Cell`s, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|n| n.set(n.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|n| n.set(n.get() + 1));
        }
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const DB: &str = r#"
record(ai, "MSS:SRC") {}
record(ai, "MSS:RD") { field(INP, "MSS:SRC NPP MSS") }
"#;

/// Allocations 100 cycles of `MSS:RD` make on a primed scan thread.
fn allocs(db: Arc<PvDatabase>) -> usize {
    let (tx, rx) = std::sync::mpsc::channel();
    MandatoryThread::new("scan-1", ThreadPriority::ScanLow, StackSizeClass::Big).spawn(move || {
        for _ in 0..3 {
            db.process_record_with_links_already_locked("MSS:RD", &mut ProcStack::new())
                .unwrap();
        }
        let mut visited = ProcStack::new();
        ALLOCS.with(|n| n.set(0));
        COUNTING.with(|c| c.set(true));
        for _ in 0..100 {
            db.process_record_with_links_already_locked("MSS:RD", &mut visited)
                .unwrap();
        }
        COUNTING.with(|c| c.set(false));
        tx.send(ALLOCS.with(Cell::get)).unwrap();
    });
    rx.recv().unwrap()
}

/// The source's committed alarm, as its own cycle would leave it.
fn alarm_source(db: &PvDatabase, amsg: &str) {
    let rec = db.get_record("MSS:SRC").unwrap();
    let mut inst = rec.write();
    inst.common.stat = alarm_status::HIHI_ALARM;
    inst.common.sevr = AlarmSeverity::Major;
    inst.common.amsg = amsg.into();
}

#[epics_macros_rs::epics_test]
async fn an_mss_link_copies_its_source_message_without_allocating() {
    let db = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    db.ioc_init().await;
    let amsg = |db: &PvDatabase| db.get_pv("MSS:RD.AMSG").unwrap().text_g();
    alarm_source(&db, "src-msg");
    assert_eq!(allocs(db.clone()), 0);
    assert_eq!(amsg(&db), "src-msg");
    alarm_source(&db, "src-changed");
    assert_eq!(
        allocs(db.clone()),
        0,
        "a changed source text is shared, not copied"
    );
    assert_eq!(amsg(&db), "src-changed");
    let rd = db.get_record("MSS:RD").unwrap();
    assert_eq!(rd.read().common.sevr, AlarmSeverity::Major);
    assert_eq!(rd.read().common.stat, alarm_status::HIHI_ALARM);
}
