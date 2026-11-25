//! A context's events delivered inside a subscription callback are refused,
//! as libca refuses `ca_pend_event` and `ca_pend_io` on a CA callback thread
//! with `ECA_EVDISALLOW` (`ca_client_context.cpp:481-484`, `:529-532`): two
//! callers, each inside one context's callback and each delivering the
//! other's, would otherwise wait on each other's delivery.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::Instant;

use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::database::{
    CaClientContext, EvDisallow, InProcessSubscription, PvDatabase,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DBR_DOUBLE, EpicsValue};

const DB: &str = r#"
record(ao, "B") {}
record(ao, "C") {}
record(ao, "D") {}
record(ao, "E") {}
"#;

async fn ioc() -> Arc<PvDatabase> {
    let ioc = IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = Arc::clone(ioc.database());
    assert!(ioc.run().await);
    db
}

/// `value` put to `record` by another client.
fn post(db: &PvDatabase, record: &str, value: f64) {
    let post = db.put_record_field_from_ca_no_notify(record, "VAL", EpicsValue::Double(value));
    block_on_sync(post).expect("a plain thread blocks").unwrap();
}

/// An in-process subscription of `record` whose callback hands each value
/// to `value`.
fn monitor(
    ctx: &CaClientContext,
    record: &str,
    value: impl Fn(f64) + Send + Sync + 'static,
) -> InProcessSubscription {
    let mask = (EventMask::VALUE | EventMask::ALARM).bits();
    ctx.create_in_process(record)
        .expect(record)
        .subscribe(DBR_DOUBLE, 1, mask, move |s| {
            value(s.value.to_f64().unwrap())
        })
        .unwrap()
}

/// A flag one thread sets and another waits on.
#[derive(Default)]
struct Flag {
    set: Mutex<bool>,
    changed: Condvar,
}

impl Flag {
    fn set(&self) {
        *self.set.lock().unwrap() = true;
        self.changed.notify_all();
    }

    /// Whether it is set, within the budget.
    fn wait(&self) -> bool {
        let set = self.set.lock().unwrap();
        *self
            .changed
            .wait_timeout_while(set, fact_budget::FACT_BUDGET, |s| !*s)
            .unwrap()
            .0
    }
}

/// A thread named `name` that posts `trigger` and delivers `ctx`'s events
/// itself until its delivery runs `trigger`'s callback, which then posts 5.0
/// to `target`: the same delivery takes that next, so `target`'s callback
/// runs on this thread in place of `CAC-event`.
struct Caller {
    _trigger: InProcessSubscription,
    thread: thread::JoinHandle<()>,
}

impl Caller {
    fn start(
        db: &Arc<PvDatabase>,
        ctx: &CaClientContext,
        trigger: &'static str,
        target: &'static str,
        name: &'static str,
    ) -> Self {
        let posted = Arc::new(AtomicBool::new(false));
        let (d, p) = (db.clone(), posted.clone());
        let _trigger = monitor(ctx, trigger, move |_| {
            if thread::current().name() == Some(name) && !p.swap(true, Ordering::SeqCst) {
                post(&d, target, 5.0);
            }
        });
        let (events, db) = (ctx.events().clone(), db.clone());
        let thread = thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let end = Instant::now() + fact_budget::FACT_BUDGET;
                let mut v = 0.0;
                while !posted.load(Ordering::SeqCst) && Instant::now() < end {
                    v += 1.0;
                    post(&db, trigger, v);
                    events.deliver().unwrap();
                }
            })
            .expect("spawn the caller");
        Self { _trigger, thread }
    }
}

/// Two callers, each inside a callback of its own context, each deliver the
/// other's context: both are refused and return.
#[epics_macros_rs::epics_test]
async fn callers_delivering_each_others_context_inside_a_callback_are_refused() {
    let db = ioc().await;
    let ctxs = [db.ca_client(), db.ca_client()];
    let ins = [Arc::new(Flag::default()), Arc::new(Flag::default())];
    let (returned_tx, returned) = mpsc::channel();
    // Context `me`'s subscription of `record`, whose callback on 5.0 waits
    // for the other's to be in, then delivers the other's context.
    let subscribe = |me: usize, record: &str| {
        let (own, other) = (ins[me].clone(), ins[1 - me].clone());
        let (events, r) = (ctxs[1 - me].events().clone(), returned_tx.clone());
        monitor(&ctxs[me], record, move |v| {
            if v != 5.0 {
                return;
            }
            own.set();
            other.wait();
            let delivered = events.deliver();
            let on = thread::current().name().unwrap_or_default().to_string();
            let _ = r.send((on, delivered));
        })
    };
    let subs = [subscribe(0, "B"), subscribe(1, "D")];
    let callers = [
        Caller::start(&db, &ctxs[0], "C", "B", "caller 0"),
        Caller::start(&db, &ctxs[1], "E", "D", "caller 1"),
    ];
    let mut on: Vec<(String, Result<(), EvDisallow>)> = (0..2)
        .map_while(|_| returned.recv_timeout(fact_budget::FACT_BUDGET).ok())
        .collect();
    on.sort_by(|a, b| a.0.cmp(&b.0));
    if on.len() < 2 {
        let inside = ins.each_ref().map(|f| *f.set.lock().unwrap());
        // The two callers wait on each other; nothing may join them.
        std::mem::forget((callers, subs, ctxs, db));
        panic!("deliveries that returned: {on:?}, callbacks that were in: {inside:?}");
    }
    assert_eq!(
        on,
        [
            ("caller 0".to_string(), Err(EvDisallow)),
            ("caller 1".to_string(), Err(EvDisallow)),
        ],
        "deliveries that returned"
    );
    for caller in callers {
        caller.thread.join().unwrap();
    }
}
