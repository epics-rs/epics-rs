//! A CA client context's in-process subscriptions share its one event
//! context (`dbContext.cpp:219-257`): each attaches to the source's list
//! when it is made (`dbSubscriptionIO.cpp:47-53`), so a post queues one
//! entry per subscription in that order (`dbEvent.c:876-905`), and the
//! context's one consumer runs each callback to its return before the next
//! (`dbEvent.c:972-996`): a value posted while a callback runs reaches each
//! subscription after the earlier posts have reached all of them. Two
//! contexts have a consumer each, so theirs may run together.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::database::{CaClientContext, ClientSubscription, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{DBR_TIME_DOUBLE, EpicsValue};

const DB: &str = r#"record(ao, "AO") {}"#;

/// How long the first subscription's callback waits for the second's to
/// start on the same value: what it sees if the two may run together.
const OVERLAP: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    Start(char, i64),
    End(char, i64),
    /// The test has posted 2, while 'a' runs on 1.
    Posted,
}

#[derive(Default)]
struct Log {
    steps: Mutex<Vec<Step>>,
    changed: Condvar,
}

impl Log {
    fn push(&self, step: Step) {
        self.steps.lock().unwrap().push(step);
        self.changed.notify_all();
    }
}

/// A subscription of `AO` on `ctx` logging its callback's start and end.
/// Between the two, the one named 'a' holds 1 until 2 is posted, and then
/// waits up to [`OVERLAP`] for 'b' to start on the same value.
async fn subscribe(ctx: &CaClientContext, name: char, log: &Arc<Log>) -> ClientSubscription {
    let ch = ctx.create_channel("AO").await.expect("AO");
    let log = log.clone();
    let callback = Box::new(move |s: Snapshot| {
        let v = s.value.to_f64().unwrap() as i64;
        log.push(Step::Start(name, v));
        if name == 'a' {
            let mut steps = log.steps.lock().unwrap();
            if v == 1 {
                (steps, _) = log
                    .changed
                    .wait_timeout_while(steps, fact_budget::FACT_BUDGET, |steps| {
                        !steps.contains(&Step::Posted)
                    })
                    .unwrap();
            }
            let _ = log
                .changed
                .wait_timeout_while(steps, OVERLAP, |steps| {
                    !steps.contains(&Step::Start('b', v))
                })
                .unwrap();
        }
        log.push(Step::End(name, v));
    });
    let mask = (EventMask::VALUE | EventMask::ALARM).bits();
    ch.subscribe(DBR_TIME_DOUBLE, 1, mask, callback)
        .await
        .expect("subscribe")
}

/// Until `log` holds each of `steps`.
async fn until(log: &Log, steps: &[Step]) {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    loop {
        let logged = log.steps.lock().unwrap().clone();
        if steps.iter().all(|step| logged.contains(step)) {
            return;
        }
        assert!(Instant::now() < deadline, "no {steps:?} in {logged:?}");
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
}

/// The test's posts: `AO` put to 1 and, once 'a' runs on 1, to 2, from a
/// thread of their own, as a record's processing posts on its own.
fn post(db: Arc<PvDatabase>, log: Arc<Log>) -> std::thread::JoinHandle<()> {
    let put = move |v: i64| {
        let put = db.put_record_field_from_ca_no_notify("AO", "VAL", EpicsValue::Double(v as f64));
        block_on_sync(put).expect("a plain thread blocks").unwrap();
    };
    std::thread::spawn(move || {
        put(1);
        let steps = log.steps.lock().unwrap();
        let (steps, _) = log
            .changed
            .wait_timeout_while(steps, fact_budget::FACT_BUDGET, |steps| {
                !steps.contains(&Step::Start('a', 1))
            })
            .unwrap();
        drop(steps);
        put(2);
        log.push(Step::Posted);
    })
}

/// 'a' then 'b' subscribed on `ctx_a` and `ctx_b`, then [`post`]; what
/// their callbacks did.
async fn run(contexts: impl Fn(&PvDatabase) -> (CaClientContext, CaClientContext)) -> Vec<Step> {
    let ioc = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = Arc::clone(ioc.database());
    assert!(ioc.run().await);
    let (ctx_a, ctx_b) = contexts(&db);
    let log = Arc::new(Log::default());
    let _a = subscribe(&ctx_a, 'a', &log).await;
    let _b = subscribe(&ctx_b, 'b', &log).await;
    until(&log, &[Step::End('a', 0), Step::End('b', 0)]).await;
    let posting = post(db, log.clone());
    until(&log, &[Step::End('a', 2), Step::End('b', 2)]).await;
    posting.join().unwrap();
    log.steps.lock().unwrap().clone()
}

#[epics_macros_rs::epics_test]
async fn one_contexts_callbacks_run_one_at_a_time_in_post_order() {
    let steps = run(|db| {
        let ctx = db.ca_client();
        (ctx.clone(), ctx)
    })
    .await;
    let mut expected: Vec<Step> = (0..=2)
        .flat_map(|v| {
            [
                Step::Start('a', v),
                Step::End('a', v),
                Step::Start('b', v),
                Step::End('b', v),
            ]
        })
        .collect();
    expected.insert(5, Step::Posted);
    assert_eq!(steps, expected);
}

/// The control: with a consumer each, 'b' starts on a put's value before
/// 'a' has returned from it.
#[epics_macros_rs::epics_test]
async fn two_contexts_callbacks_run_together() {
    let steps = run(|db| (db.ca_client(), db.ca_client())).await;
    let at = |step| steps.iter().position(|s| *s == step).unwrap();
    for v in [1, 2] {
        assert!(
            at(Step::Start('b', v)) < at(Step::End('a', v)),
            "{v}: {steps:?}"
        );
    }
}
