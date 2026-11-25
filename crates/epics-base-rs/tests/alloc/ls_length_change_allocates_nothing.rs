//! A long-string VAL whose length moves reuses its buffer. C allocates VAL
//! and OVAL once at SIZV (lsoRecord.c:63-64) and `memcpy`s into them, so a
//! cycle whose DOL text alternates between "0" and "10" costs what one
//! alternating between "0" and "1" does. Counted with the alloc binary's
//! allocator on the calling thread, nobody subscribed.

use std::collections::HashMap;

use alloc_count::thread;
use epics_base_rs::server::ioc_builder::IocBuilder;

/// Allocations per process cycle of `C`, which forward-links the lso and the
/// lsi that read its VAL as text.
async fn per_cycle(calc: &str) -> u64 {
    let db = IocBuilder::new()
        .db_string(
            &format!(
                r#"
record(calc, "C") {{ field(CALC, "{calc}") field(FLNK, "F") }}
record(fanout, "F") {{ field(LNK1, "LSO") field(LNK2, "LSI") }}
record(lso, "LSO") {{ field(SIZV, "64") field(OMSL, "closed_loop") field(DOL, "C") }}
record(lsi, "LSI") {{ field(SIZV, "64") field(INP, "C") }}
"#
            ),
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    for _ in 0..10 {
        db.process_record("C").await.unwrap();
    }
    const N: u64 = 100;
    let window = thread::open();
    for _ in 0..N {
        db.process_record("C").await.unwrap();
    }
    let allocs = window.close().allocs;
    assert!(
        epics_base_rs::runtime::task::in_test_ioc_thread(),
        "counted off an IOC thread"
    );
    allocs / N
}

#[test]
fn a_val_length_change_allocates_nothing() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(a_val_length_change_allocates_nothing_body())
    });
}

async fn a_val_length_change_allocates_nothing_body() {
    let moving = per_cycle("VAL?0:10").await;
    let still = per_cycle("VAL?0:1").await;
    assert_eq!(moving, still, "length-alternating cycle vs same-length");
}
