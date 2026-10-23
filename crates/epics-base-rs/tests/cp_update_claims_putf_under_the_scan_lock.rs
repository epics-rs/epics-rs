//! dbCa's worker holds `dbScanLock` across `db_process` (dbCa.c:1319-1321),
//! and the cycle clears PUTF before the lock is given up, so no other thread
//! taking the holder's lock ever sees PUTF set.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "S") { field(VAL, "0") }
record(calc, "H") { field(INPA, "S CP") field(CALC, "A") }
"#;

#[epics_macros_rs::epics_test]
async fn cp_update_claims_putf_under_the_scan_lock() {
    let ioc = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);

    let holder = db.get_record("H").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(AtomicUsize::new(0));
    let observer = {
        let (stop, seen) = (stop.clone(), seen.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if holder.read().common.putf {
                    seen.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
    };
    let end = Instant::now() + Duration::from_secs(2);
    let mut v = 0.0;
    while Instant::now() < end {
        v += 1.0;
        db.put_pv_and_post("S", EpicsValue::Double(v))
            .await
            .unwrap();
        if (v as u64).is_multiple_of(64) {
            db.ca_sync().await;
        }
    }
    db.ca_sync().await;
    stop.store(true, Ordering::Relaxed);
    observer.join().unwrap();
    assert_eq!(
        seen.load(Ordering::Relaxed),
        0,
        "PUTF seen outside the cycle"
    );
}
