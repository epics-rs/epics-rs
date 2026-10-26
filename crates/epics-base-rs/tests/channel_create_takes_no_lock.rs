//! C `dbChannelCreate` (`dbChannel.c:448-532`) takes no lock: it addresses the
//! field from its declaration and the record's `cvt_dbaddr`. A channel on a
//! record another thread holds is made at once, so `seq_connect` and a local
//! CA link's first open do not wait out the hold, and a holder that creates a
//! channel on a record whose holder waits for its own set does not deadlock.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::database::channel_io::{LongStringMode, channel_create};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::DbFieldType;

async fn ioc(db: &str) -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    db
}

/// The native type and element count a channel on `name` announces, made on
/// a thread of its own.
fn create(
    db: &Arc<PvDatabase>,
    name: &'static str,
) -> std::thread::JoinHandle<Option<(DbFieldType, u32, bool)>> {
    let db = db.clone();
    std::thread::spawn(move || {
        epics_base_rs::runtime::task::test_block_on(async move {
            let chan = channel_create(&db, name.as_bytes(), None).await.ok()?;
            let dollar = matches!(chan.long_string_mode, LongStringMode::DollarChar { .. });
            Some((chan.native_type, chan.final_element_count, dollar))
        })
    })
}

async fn join_within<T>(thread: &std::thread::JoinHandle<T>, secs: u64, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !thread.is_finished() {
        assert!(Instant::now() < deadline, "{what}");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
}

#[epics_macros_rs::epics_test]
async fn a_held_record_is_addressed_without_waiting_for_the_hold() {
    let db = ioc("record(ai, \"B\") {}\n\
         record(waveform, \"W\") { field(FTVL, \"LONG\") field(NELM, \"8\") }\n\
         record(stringin, \"S\") {}\n")
    .await;
    let h = db.async_handle();
    let (held_tx, held_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        h.with_record("B", |_| {
            h.with_record("W", |_| {
                h.with_record("S", |_| {
                    held_tx.send(()).unwrap();
                    go_rx.recv().unwrap();
                })
            })
        })
    });
    held_rx.recv().unwrap();
    let made = [create(&db, "B"), create(&db, "W"), create(&db, "S.VAL$")];
    for chan in &made {
        join_within(chan, 10, "channel_create waited for the hold").await;
    }
    go_tx.send(()).unwrap();
    holder.join().unwrap();
    let made: Vec<_> = made.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(
        made,
        [
            Some((DbFieldType::Double, 1, false)),
            Some((DbFieldType::Long, 8, false)),
            Some((DbFieldType::Char, 40, true)),
        ]
    );
}

#[epics_macros_rs::epics_test]
async fn a_holder_creating_a_channel_does_not_deadlock_with_the_targets_holder() {
    let db = ioc("record(ai, \"X\") {}\nrecord(ai, \"B\") {}\n").await;
    let h = db.async_handle();
    let (held_tx, held_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    // B's holder then waits for X's set.
    let b_holder = {
        let h = h.clone();
        std::thread::spawn(move || {
            h.with_record("B", |_| {
                held_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                h.with_record("X", |_| {});
            })
        })
    };
    held_rx.recv().unwrap();
    // X's holder creates a channel on B.
    let x_holder = {
        let db = db.clone();
        std::thread::spawn(move || {
            let mut made = None;
            h.with_record("X", |_| {
                go_tx.send(()).unwrap();
                // B's holder is waiting on X by now.
                std::thread::sleep(Duration::from_millis(200));
                made = epics_base_rs::runtime::task::test_block_on(async {
                    channel_create(&db, b"B", None)
                        .await
                        .ok()
                        .map(|c| c.native_type)
                });
            });
            made
        })
    };
    join_within(&x_holder, 20, "the holders deadlocked").await;
    join_within(&b_holder, 20, "the holders deadlocked").await;
    b_holder.join().unwrap();
    assert_eq!(x_holder.join().unwrap(), Some(DbFieldType::Double));
}
