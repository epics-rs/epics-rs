//! `ProcessCallback` — C `callbackRequestProcessCallback` on a record-owned
//! `epicsCallback` — re-enters the record in place and allocates nothing, on
//! the requesting thread or on the band worker, where the minted token,
//! put-notify and spawned re-entry it replaces allocate per cycle. Counted per
//! thread with this binary's own allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use epics_base_rs::runtime::task::CallbackPriority;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;

const BAND: CallbackPriority = CallbackPriority::High;

thread_local! {
    /// 0 not yet asked, 1 the band's worker, 2 any other thread. Asking may
    /// itself allocate; that reentry reads 2 and is not counted.
    static ON_BAND: Cell<u8> = const { Cell::new(0) };
    static REQUESTING: Cell<bool> = const { Cell::new(false) };
}

static BAND_ARMED: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicU64 = AtomicU64::new(0);

fn on_band() -> bool {
    ON_BAND.with(|s| match s.get() {
        0 => {
            s.set(2);
            let yes = std::thread::current()
                .name()
                .is_some_and(|n| n.starts_with(BAND.name_prefix()));
            s.set(if yes { 1 } else { 2 });
            yes
        }
        v => v == 1,
    })
}

fn counted() -> bool {
    REQUESTING.with(Cell::get) || (BAND_ARMED.load(Ordering::Relaxed) && on_band())
}

struct Counting;

// SAFETY: forwards every call to `System` unchanged; the gate reads
// const-initialised thread-locals and atomics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if counted() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if counted() {
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

const N: u64 = 500;

async fn db() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(
            "record(calc, \"C\") { field(CALC, \"VAL+1\") }\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn val(db: &PvDatabase) -> u64 {
    db.get_pv("C").expect("C").to_f64().expect("double") as u64
}

fn wait_val(db: &PvDatabase, want: u64) {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while val(db) < want {
        assert!(Instant::now() < deadline, "C never reached {want}");
        std::thread::yield_now();
    }
}

#[epics_macros_rs::epics_test]
async fn a_process_callback_reentry_allocates_nothing() {
    let db = db().await;
    let cb = db.async_handle().process_callback("C");
    // Warm the band and the record: the first cycle arms nothing.
    cb.request(BAND).unwrap();
    wait_val(&db, 1);

    BAND_ARMED.store(true, Ordering::Relaxed);
    for i in 0..N {
        REQUESTING.with(|r| r.set(true));
        cb.request(BAND).unwrap();
        REQUESTING.with(|r| r.set(false));
        wait_val(&db, i + 2);
    }
    BAND_ARMED.store(false, Ordering::Relaxed);
    assert_eq!(val(&db), N + 1, "one process per request");
    assert_eq!(
        ALLOCS.load(Ordering::Relaxed),
        0,
        "{N} ProcessCallback re-entries allocated"
    );

    // The minted re-entry it replaces is counted by the same gate.
    REQUESTING.with(|r| r.set(true));
    let token = db.mint_async_token("C").expect("C");
    REQUESTING.with(|r| r.set(false));
    assert!(
        ALLOCS.load(Ordering::Relaxed) > 0,
        "the counter missed a token"
    );
    drop(token);
}

/// A run supersedes the record's outstanding tokens, as the mint-and-fire it
/// replaces did, and re-enters nothing once the database is gone.
#[epics_macros_rs::epics_test]
async fn a_process_callback_supersedes_tokens_and_outlives_nothing() {
    let db = db().await;
    let cb = db.async_handle().process_callback("C");
    let token = db.mint_async_token("C").expect("C");
    cb.request(BAND).unwrap();
    wait_val(&db, 1);
    assert!(
        !token.is_current(),
        "the re-entry left an older token current"
    );

    let gone = db.async_handle().process_callback("NOT_A_RECORD");
    gone.request(BAND).unwrap();
    let weak = std::sync::Arc::downgrade(&db);
    drop(db);
    assert!(
        weak.upgrade().is_none(),
        "the callback kept the database alive"
    );
    cb.request(BAND).unwrap();
}
