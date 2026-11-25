//! An in-process put holds back what it posts on its own record until it
//! returns, as C's `dbChannelIO::write` is ordinarily back in its caller
//! before `CAC-event` runs. A put stuck in its record's processing holds up
//! no other record's monitor of the context; C's `CAC-event` waits on the
//! record's whole lockset only because it reads under `dbScanLock`
//! (`db_access.c:158`).

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::database::{
    CaClientContext, InProcessChannel, InProcessSubscription, PvDatabase,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DBR_DOUBLE, EpicsValue};

const DB: &str = r#"
record(ao, "A") { field(FLNK, "S") }
record(sub, "S") { field(SNAM, "hold") }
record(ao, "B") {}
"#;

/// How long a callback the put should hold back is given to run anyway.
const SETTLE: Duration = Duration::from_millis(300);

/// `S`'s subroutine: each call reports it is in and waits to be released.
#[derive(Default)]
struct Gate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl Gate {
    fn hold(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.changed.notify_all();
        let _ = self
            .changed
            .wait_timeout_while(state, fact_budget::FACT_BUDGET, |s| !s.1)
            .unwrap();
    }

    fn until_held(&self) {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, fact_budget::FACT_BUDGET, |s| !s.0)
            .unwrap();
        assert!(state.0, "S's subroutine never ran");
    }

    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}

/// Releases the gate on every exit, so a failed assertion stops no put.
struct Release<'a>(&'a Gate);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

async fn ioc(gate: &Arc<Gate>) -> Arc<PvDatabase> {
    let gate = gate.clone();
    let ioc = IocBuilder::new()
        .register_subroutine("hold", move |_| {
            gate.hold();
            Ok(0)
        })
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = Arc::clone(ioc.database());
    assert!(ioc.run().await);
    db
}

fn open(ctx: &CaClientContext, name: &str) -> InProcessChannel {
    ctx.create_in_process(name).expect(name)
}

/// A subscription whose values past its first reach `value`.
fn monitor(
    chan: &InProcessChannel,
    value: impl Fn(f64) + Send + Sync + 'static,
) -> InProcessSubscription {
    let mask = (EventMask::VALUE | EventMask::ALARM).bits();
    let (first, got_first) = mpsc::channel();
    let first = Mutex::new(Some(first));
    let sub = chan
        .subscribe(DBR_DOUBLE, 1, mask, move |s| {
            let v = s.value.to_f64().unwrap();
            match first.lock().unwrap().take() {
                Some(first) => first.send(()).unwrap(),
                None => value(v),
            }
        })
        .unwrap();
    got_first.recv_timeout(fact_budget::FACT_BUDGET).unwrap();
    sub
}

/// While a put on `S` is held inside `S`'s processing, a value posted on
/// `B` reaches the context's monitor of `B`.
#[epics_macros_rs::epics_test]
async fn a_monitor_lands_while_a_put_on_another_record_is_held() {
    let gate = Arc::new(Gate::default());
    let db = ioc(&gate).await;
    let ctx = db.ca_client();
    std::thread::scope(|s| {
        let release = Release(&gate);
        let (values, b_values) = mpsc::channel();
        let _b = monitor(&open(&ctx, "B"), move |v| values.send(v).unwrap());
        let proc = open(&ctx, "S.PROC");
        let putter = s.spawn(move || proc.put(EpicsValue::Short(1)));
        gate.until_held();
        s.spawn(|| {
            let post = db.put_record_field_from_ca_no_notify("B", "VAL", EpicsValue::Double(5.0));
            block_on_sync(post).expect("a plain thread blocks").unwrap();
        });
        assert_eq!(
            b_values.recv_timeout(fact_budget::FACT_BUDGET),
            Ok(5.0),
            "B's monitor while the put on S is held"
        );
        drop(release);
        putter.join().unwrap().unwrap();
    });
}

/// A put's own post on its record, through another channel of the record,
/// does not reach the context's monitor while the put is held in its
/// forward link after posting; it does once the put is released.
#[epics_macros_rs::epics_test]
async fn a_puts_own_record_monitor_waits_for_the_put() {
    let gate = Arc::new(Gate::default());
    let db = ioc(&gate).await;
    let ctx = db.ca_client();
    std::thread::scope(|s| {
        let release = Release(&gate);
        let (values, a_values) = mpsc::channel();
        let _a = monitor(&open(&ctx, "A"), move |v| values.send(v).unwrap());
        let put = open(&ctx, "A");
        let putter = s.spawn(move || put.put(EpicsValue::Double(1.0)));
        gate.until_held();
        assert!(
            a_values.recv_timeout(SETTLE).is_err(),
            "A's monitor while its put is held"
        );
        drop(release);
        putter.join().unwrap().unwrap();
        assert_eq!(a_values.recv_timeout(fact_budget::FACT_BUDGET), Ok(1.0));
    });
}

/// The same for a put through a channel `create_channel` opened, as
/// sscan's `saveData` and motor's `motorUtil` put.
#[epics_macros_rs::epics_test]
async fn a_create_channel_puts_own_record_monitor_waits_for_the_put() {
    let gate = Arc::new(Gate::default());
    let db = ioc(&gate).await;
    let ctx = db.ca_client();
    let put = ctx.create_channel("A").await.expect("A");
    std::thread::scope(|s| {
        let release = Release(&gate);
        let (values, a_values) = mpsc::channel();
        let _a = monitor(&open(&ctx, "A"), move |v| values.send(v).unwrap());
        let putter = s.spawn(move || block_on_sync(put.put(EpicsValue::Double(1.0), false)));
        gate.until_held();
        // absence-window: `CAC-event` waits on the held put with the entry
        // taken, so no later event of the context can mark the wait.
        assert!(
            a_values.recv_timeout(SETTLE).is_err(),
            "A's monitor while its put is held"
        );
        drop(release);
        let put = putter.join().unwrap().expect("a plain thread blocks");
        assert!(put.unwrap().is_none(), "a plain put has no completion");
        assert_eq!(a_values.recv_timeout(fact_budget::FACT_BUDGET), Ok(1.0));
    });
}
