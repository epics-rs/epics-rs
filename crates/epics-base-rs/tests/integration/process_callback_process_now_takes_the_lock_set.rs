//! `ProcessCallback::process_now` is C `ProcessCallback`'s body run by a
//! callback that continues afterwards (StreamEpics.cc
//! `recordProcessCallback`): the record has processed when it returns, and
//! it waited for the lock set another thread held (callback.c:379-387).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use epics_base_rs::server::ioc_builder::IocBuilder;

#[epics_macros_rs::epics_test]
async fn process_now_processes_under_the_lock_set_before_it_returns() {
    let db = IocBuilder::new()
        .db_string(
            "record(calc, \"C\") { field(CALC, \"VAL+1\") }\n",
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    let val = || db.get_pv("C").unwrap().to_f64().unwrap();
    let handle = db.async_handle();
    let cb = handle.process_callback("C");

    std::thread::scope(|s| {
        s.spawn(|| cb.process_now()).join().unwrap();
    });
    assert_eq!(val(), 1.0, "process_now returned before the record ran");

    let held = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    std::thread::scope(|s| {
        s.spawn(|| {
            handle.with_record("C", |_| {
                held.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(200));
                released.store(true, Ordering::Release);
            })
        });
        while !held.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        s.spawn(|| {
            cb.process_now();
            assert!(
                released.load(Ordering::Acquire),
                "process_now ran inside another thread's lock set"
            );
        });
    });
    assert_eq!(val(), 2.0);
}
