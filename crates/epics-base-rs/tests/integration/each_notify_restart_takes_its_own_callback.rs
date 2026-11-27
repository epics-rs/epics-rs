//! C replays a record's restart list one entry per low-band callback:
//! `restartCheck` pops the first waiting put-notify and `callbackRequest`s it
//! (`dbNotify.c:149-170`), and the next entry is only requested when that one
//! is done — by its cycle's `dbNotifyCompletion` (`:463-465`) or, when it
//! drives no cycle, by `processNotifyCommon` itself (`:263-264`). Another
//! record's restart already waiting on the band therefore runs between two
//! entries of the same record's list.

use std::sync::{Arc, Mutex};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::background::callback_executor::set_parallel_threads;
use epics_base_rs::runtime::task::{CallbackPriority, spawn_blocking_background};
use epics_base_rs::server::database::ProcStack;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::*;
use epics_base_rs::types::{DbFieldType, EpicsValue};

type Log = Arc<Mutex<Vec<(&'static str, i32)>>>;

/// Goes async on its first pass and completes synchronously after; every pass
/// logs which record saw which VAL.
struct AsyncOnceRecord {
    name: &'static str,
    val: i32,
    pending: bool,
    log: Log,
}

static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Long, false)];

impl Record for AsyncOnceRecord {
    fn record_type(&self) -> &'static str {
        "asynconce"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        self.log.lock().unwrap().push((self.name, self.val));
        if std::mem::take(&mut self.pending) {
            Ok(ProcessOutcome {
                result: RecordProcessResult::AsyncPending,
                actions: Vec::new(),
                device_did_compute: false,
                status: Ok(()),
            })
        } else {
            Ok(ProcessOutcome::complete())
        }
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        (name == "VAL").then_some(EpicsValue::Long(self.val))
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match (name, value) {
            ("VAL", EpicsValue::Long(v)) => {
                self.val = v;
                Ok(())
            }
            ("VAL", _) => Err(CaError::TypeMismatch("VAL".into())),
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }

    fn process_passive_fields(&self) -> &'static [&'static str] {
        &["VAL"]
    }
}

/// Puts each record's `puts` behind its own async cycle, completes both
/// cycles while the low band is held so both restarts wait there, releases
/// the band, and returns the VALs the replayed puts processed, in order.
async fn replayed_order(
    puts: [(&'static str, Vec<(&str, EpicsValue)>); 2],
) -> Vec<(&'static str, i32)> {
    // C's default: one low-band worker, so its queue runs in request order.
    set_parallel_threads(1, Some(CallbackPriority::Low));
    let (db, _) = IocBuilder::new().build().await.unwrap();
    let log = Log::default();
    let mut waiters = Vec::new();
    for (name, queue) in &puts {
        db.add_record(
            name,
            Box::new(AsyncOnceRecord {
                name,
                val: 0,
                pending: true,
                log: log.clone(),
            }),
        )
        .await
        .unwrap();
        db.process_record_with_links(name, &mut ProcStack::new())
            .await
            .unwrap();
        for (field, value) in queue {
            waiters.push(
                db.put_record_field_from_ca(name, field, value.clone())
                    .await
                    .expect("a put onto a PACT record is accepted")
                    .into_handle()
                    .expect("and waits on the restart list"),
            );
        }
    }

    let (release, held) = std::sync::mpsc::channel::<()>();
    let (busy, is_busy) = std::sync::mpsc::channel();
    let hold = spawn_blocking_background(CallbackPriority::Low, move |_| {
        busy.send(()).unwrap();
        let _ = held.recv_timeout(fact_budget::FACT_BUDGET);
    });
    is_busy.recv_timeout(fact_budget::FACT_BUDGET).unwrap();
    for (name, _) in &puts {
        db.complete_async_record(name).await.unwrap();
    }
    release.send(()).unwrap();
    let _ = hold.await;

    for done in waiters {
        epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, done)
            .await
            .expect("every queued put-notify is answered")
            .expect("and answered ok");
    }
    log.lock()
        .unwrap()
        .iter()
        .copied()
        .filter(|&(_, v)| v != 0)
        .collect()
}

/// Boundary — each replayed entry drives a cycle: the other record's restart
/// runs between the two entries of the first record's list.
#[epics_macros_rs::epics_test]
async fn a_replay_that_processes_yields_the_band_to_the_next_restart() {
    let order = replayed_order([
        (
            "RST:A",
            vec![("VAL", EpicsValue::Long(2)), ("VAL", EpicsValue::Long(3))],
        ),
        (
            "RST:B",
            vec![("VAL", EpicsValue::Long(12)), ("VAL", EpicsValue::Long(13))],
        ),
    ])
    .await;
    assert_eq!(
        order,
        [("RST:A", 2), ("RST:B", 12), ("RST:A", 3), ("RST:B", 13)]
    );
}

/// Boundary — the first entry drives no cycle (DESC is not pp): handing the
/// record on still costs a callback, so the other record's waiting restart
/// processes before the first record's next entry.
#[epics_macros_rs::epics_test]
async fn a_replay_that_processes_nothing_yields_the_band_too() {
    let order = replayed_order([
        (
            "RST:A",
            vec![
                ("DESC", EpicsValue::String("x".into())),
                ("VAL", EpicsValue::Long(2)),
            ],
        ),
        ("RST:B", vec![("VAL", EpicsValue::Long(12))]),
    ])
    .await;
    assert_eq!(order, [("RST:B", 12), ("RST:A", 2)]);
}
