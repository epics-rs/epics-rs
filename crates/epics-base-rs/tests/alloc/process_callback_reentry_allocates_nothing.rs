//! `ProcessCallback` — C `callbackRequestProcessCallback` on a record-owned
//! `epicsCallback` — re-enters the record in place and allocates nothing, on
//! the requesting thread or on the band worker, where the minted token,
//! put-notify and spawned re-entry it replaces allocate per cycle. Counted per
//! thread with the alloc binary's allocator.

use std::collections::HashMap;
use std::time::Instant;

use alloc_count::named::{self, Name};
use alloc_count::thread;
use epics_base_rs::runtime::task::CallbackPriority;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;

const BAND: CallbackPriority = CallbackPriority::High;

/// `BAND`'s workers.
static BAND_WORKERS: [Name; 1] = [Name::Prefix("cbHigh")];

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

    let band = named::open(&BAND_WORKERS);
    let mut requests = 0;
    for i in 0..N {
        let (r, counts) = thread::counted(|| cb.request(BAND));
        r.unwrap();
        requests += counts.allocs;
        wait_val(&db, i + 2);
    }
    let [band] = band.close().map(|c| c.allocs);
    assert_eq!(val(&db), N + 1, "one process per request");
    assert_eq!(
        (requests, band),
        (0, 0),
        "{N} ProcessCallback re-entries allocated (requester, band)"
    );

    // The minted re-entry it replaces is counted the same way.
    let (token, minted) = thread::counted(|| db.mint_async_token("C").expect("C"));
    assert!(minted.allocs > 0, "the counter missed a token");
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
