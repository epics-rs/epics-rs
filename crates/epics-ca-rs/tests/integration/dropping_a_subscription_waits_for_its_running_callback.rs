//! Dropping a record's CA client subscription, in-process or network, is
//! `ca_clear_subscription`: it waits for the subscription's running
//! callback unless it drops inside that callback, and no callback runs
//! after it. C's `db_cancel_event` waits through `db_sync_event` off the
//! event task (`dbEvent.c:613-634`), and a network clear takes the
//! context's recursive `cbMutex` the receive thread calls under.
#![cfg(feature = "client-core")]

// RTEMS-EXEC-MODEL-ALLOW(1): measured - all five tests pass under
// `EPICS_RS_BUILD_EXEC_BACKEND=thread`, where the tokio runtime that hosts
// them here is not built.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::task::{background_init, block_on_sync};
use epics_base_rs::server::database::{
    CaClientContext, ClientChannel, ClientSubscription, Connection, InProcessSubscription,
    PvDatabase,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{FieldDesc, FieldMetadataOverride, Record};
use epics_base_rs::types::{DBR_CTRL_DOUBLE, DBR_DOUBLE, DbFieldType, EpicsValue};
use epics_ca_rs::calink::install_calink_resolver;
use epics_ca_rs::server::blocking::BlockingCaServer;
use serial_test::serial;

const UPSTREAM_DB: &str = "record(ao, \"DROPWAIT:AO\") { field(VAL, \"0\") }\n";
const DOWN_DB: &str = r#"
record(ao, "B") {}
record(ao, "C") {}
"#;
/// `DBE_VALUE | DBE_ALARM`.
const MASK: u16 = 1 | 4;
/// How long a drop that does not wait is given to return.
const SETTLE: Duration = Duration::from_millis(300);

/// A wait one thread enters and another releases.
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
            .wait_timeout_while(state, fact_budget::FACT_BUDGET * 2, |s| !s.1)
            .unwrap();
    }

    fn until_held(&self) {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, fact_budget::FACT_BUDGET, |s| !s.0)
            .unwrap();
        assert!(state.0, "the gate was never entered");
    }

    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}

/// Releases the gate on every exit, so a failed assertion strands no thread.
struct Release<'a>(&'a Gate);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.0.release();
    }
}

static HELD_READ_FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

/// A record whose limits, which `CAC-event` reads for a control type after
/// taking an entry, hold at `gate` while its value is 1.
struct HeldRead {
    val: f64,
    gate: Arc<Gate>,
}

impl Record for HeldRead {
    fn record_type(&self) -> &'static str {
        "held_read"
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        (name == "VAL").then_some(EpicsValue::Double(self.val))
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match (name, value) {
            ("VAL", EpicsValue::Double(v)) => {
                self.val = v;
                Ok(())
            }
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        HELD_READ_FIELDS
    }

    fn process_passive_fields(&self) -> &'static [&'static str] {
        &["VAL"]
    }

    fn field_metadata_override(&self, _field: &str) -> Option<FieldMetadataOverride> {
        if self.val == 1.0 {
            self.gate.hold();
        }
        None
    }
}

/// An upstream IOC serving `DROPWAIT:AO` over CA, and a downstream one whose
/// CA client reaches it, with `H`'s limits holding at `gate`.
struct Iocs {
    upstream: Arc<PvDatabase>,
    downstream: Arc<PvDatabase>,
    server: Arc<BlockingCaServer>,
    accept: Option<thread::JoinHandle<()>>,
}

impl Iocs {
    fn start(gate: &Arc<Gate>) -> Self {
        background_init();
        let (upstream, _) = block_on_sync(
            IocBuilder::new()
                .db_string(UPSTREAM_DB, &HashMap::new())
                .unwrap()
                .build(),
        )
        .expect("a plain test thread may block")
        .expect("iocInit");
        let server = Arc::new(
            BlockingCaServer::bind(
                "127.0.0.1:0",
                upstream.clone(),
                epics_base_rs::server::access_security::new_acf_cell(None),
            )
            .expect("bind on an ephemeral loopback port"),
        );
        let port = server.local_addr().expect("local_addr").port();
        let srv = server.clone();
        let accept = thread::spawn(move || srv.serve());
        // SAFETY: `#[serial(epics_env)]` keeps the tests apart; the client
        // reads these when the resolver first builds it.
        unsafe {
            std::env::set_var("EPICS_CA_NAME_SERVERS", format!("127.0.0.1:{port}"));
            std::env::set_var("EPICS_CA_ADDR_LIST", "");
            std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        }
        let (downstream, _) = block_on_sync(
            IocBuilder::new()
                .record(
                    "H",
                    HeldRead {
                        val: 0.0,
                        gate: gate.clone(),
                    },
                )
                .db_string(DOWN_DB, &HashMap::new())
                .unwrap()
                .build(),
        )
        .expect("a plain test thread may block")
        .expect("iocInit");
        block_on_sync(install_calink_resolver(
            &downstream,
            epics_base_rs::runtime::task::Reactor::current().expect("reactor"),
        ))
        .expect("a plain test thread may block");
        Self {
            upstream,
            downstream,
            server,
            accept: Some(accept),
        }
    }

    /// `value` put to upstream `DROPWAIT:AO` by another client.
    fn post_upstream(&self, value: f64) {
        let post = self.upstream.put_record_field_from_ca_no_notify(
            "DROPWAIT:AO",
            "VAL",
            EpicsValue::Double(value),
        );
        block_on_sync(post)
            .expect("a plain test thread may block")
            .unwrap();
    }

    /// `value` put to downstream `record` by another client.
    fn post_downstream(&self, record: &str, value: f64) {
        post(&self.downstream, record, value);
    }
}

impl Drop for Iocs {
    fn drop(&mut self) {
        self.server.shutdown();
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

/// `value` put to `record` of `db` by another client.
fn post(db: &PvDatabase, record: &str, value: f64) {
    let post = db.put_record_field_from_ca_no_notify(record, "VAL", EpicsValue::Double(value));
    block_on_sync(post)
        .expect("a plain test thread may block")
        .unwrap();
}

/// `body` on a thread that may block, beside the reactor the CA client runs
/// on.
#[cfg(tokio_backend)]
fn on_a_blocking_thread(body: impl FnOnce() + Send + 'static) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build runtime");
    if let Err(e) = rt.block_on(rt.spawn_blocking(body)) {
        std::panic::resume_unwind(e.into_panic());
    }
}

/// `body` on the calling thread, which the exec reactor does not run on.
#[cfg(exec_backend)]
fn on_a_blocking_thread(body: impl FnOnce() + Send + 'static) {
    body();
}

fn connect(ctx: &CaClientContext, name: &str) -> Arc<dyn ClientChannel> {
    block_on_sync(async {
        let ch = ctx.create_network(name).await.expect("channel");
        while let Some(c) = ch.next_connection().await {
            if c == Connection::Connected {
                return ch;
            }
        }
        panic!("{name} never connected")
    })
    .expect("a plain test thread may block")
}

/// A network subscription of `ch` whose callback hands each value to
/// `value`.
fn monitor(
    ch: &dyn ClientChannel,
    value: impl Fn(f64) + Send + Sync + 'static,
) -> ClientSubscription {
    block_on_sync(ch.subscribe(
        DBR_DOUBLE,
        1,
        MASK,
        Box::new(move |s| value(s.value.to_f64().unwrap())),
    ))
    .expect("a plain test thread may block")
    .expect("subscribe")
}

/// An in-process subscription of `record` whose callback hands each value
/// to `value`.
fn monitor_in_process(
    ctx: &CaClientContext,
    record: &str,
    dbr_type: u16,
    value: impl Fn(f64) + Send + Sync + 'static,
) -> InProcessSubscription {
    ctx.create_in_process(record)
        .expect(record)
        .subscribe(dbr_type, 1, MASK, move |s| value(s.value.to_f64().unwrap()))
        .expect("subscribe")
}

/// A callback that holds at its gate on `at`, and then marks its end.
#[derive(Default)]
struct Held {
    gate: Gate,
    ended: AtomicBool,
}

impl Held {
    fn on(&self, v: f64, at: f64) {
        if v == at {
            self.gate.hold();
            self.ended.store(true, Ordering::SeqCst);
        }
    }

    /// `sub` dropped on another thread while the callback holds: whether
    /// the callback had ended when the drop returned.
    fn ended_when_dropped(&self, sub: impl Send) -> bool {
        thread::scope(|s| {
            let release = Release(&self.gate);
            self.gate.until_held();
            let (began, dropping) = mpsc::channel();
            let dropper = s.spawn(move || {
                let _ = began.send(());
                drop(sub);
                self.ended.load(Ordering::SeqCst)
            });
            dropping
                .recv_timeout(fact_budget::FACT_BUDGET)
                .expect("the drop never began");
            // absence-window: a drop that does not wait returns inside it,
            // with the callback held; one that waits gives no sign.
            thread::sleep(SETTLE);
            drop(release);
            dropper.join().unwrap()
        })
    }
}

/// A drop of an in-process subscription on another thread returns only once
/// its running callback has.
#[test]
#[serial(epics_env)]
fn an_in_process_drop_waits_for_its_running_callback() {
    on_a_blocking_thread(|| {
        let iocs = Iocs::start(&Arc::new(Gate::default()));
        let ctx = iocs.downstream.ca_client();
        let held = Arc::new(Held::default());
        let (tx, values) = mpsc::channel();
        let h = held.clone();
        let sub = monitor_in_process(&ctx, "B", DBR_DOUBLE, move |v| {
            let _ = tx.send(v);
            h.on(v, 5.0);
        });
        assert_eq!(values.recv_timeout(fact_budget::FACT_BUDGET), Ok(0.0));
        iocs.post_downstream("B", 5.0);
        assert!(
            held.ended_when_dropped(sub),
            "the drop returned while its callback ran"
        );
    });
}

/// A drop of a network subscription on another thread returns only once its
/// running callback has.
#[test]
#[serial(epics_env)]
fn a_network_drop_waits_for_its_running_callback() {
    on_a_blocking_thread(|| {
        let iocs = Iocs::start(&Arc::new(Gate::default()));
        let ctx = iocs.downstream.ca_client();
        let net = connect(&ctx, "DROPWAIT:AO");
        let held = Arc::new(Held::default());
        let (tx, values) = mpsc::channel();
        let h = held.clone();
        let sub = monitor(&*net, move |v| {
            let _ = tx.send(v);
            h.on(v, 7.0);
        });
        assert_eq!(values.recv_timeout(fact_budget::FACT_BUDGET), Ok(0.0));
        iocs.post_upstream(7.0);
        assert!(
            held.ended_when_dropped(sub),
            "the drop returned while its callback ran"
        );
    });
}

/// An in-process subscription dropped inside its own callback: the drop
/// returns there.
#[test]
#[serial(epics_env)]
fn an_in_process_subscription_dropped_inside_its_callback_returns() {
    on_a_blocking_thread(|| {
        let iocs = Iocs::start(&Arc::new(Gate::default()));
        let ctx = iocs.downstream.ca_client();
        let slot = Arc::new(Mutex::new(None));
        let (tx, values) = mpsc::channel();
        let own = slot.clone();
        let sub = monitor_in_process(&ctx, "B", DBR_DOUBLE, move |v| {
            if v == 5.0 {
                drop(own.lock().unwrap().take());
            }
            let _ = tx.send(v);
        });
        *slot.lock().unwrap() = Some(sub);
        assert_eq!(values.recv_timeout(fact_budget::FACT_BUDGET), Ok(0.0));
        iocs.post_downstream("B", 5.0);
        assert_eq!(
            values.recv_timeout(fact_budget::FACT_BUDGET),
            Ok(5.0),
            "the drop inside the callback returned"
        );
    });
}

/// A network subscription dropped inside its own callback: the drop returns
/// there.
#[test]
#[serial(epics_env)]
fn a_network_subscription_dropped_inside_its_callback_returns() {
    on_a_blocking_thread(|| {
        let iocs = Iocs::start(&Arc::new(Gate::default()));
        let ctx = iocs.downstream.ca_client();
        let net = connect(&ctx, "DROPWAIT:AO");
        let slot = Arc::new(Mutex::new(None));
        let (tx, values) = mpsc::channel();
        let own = slot.clone();
        let sub = monitor(&*net, move |v| {
            if v == 7.0 {
                drop(own.lock().unwrap().take());
            }
            let _ = tx.send(v);
        });
        *slot.lock().unwrap() = Some(sub);
        assert_eq!(values.recv_timeout(fact_budget::FACT_BUDGET), Ok(0.0));
        iocs.post_upstream(7.0);
        assert_eq!(
            values.recv_timeout(fact_budget::FACT_BUDGET),
            Ok(7.0),
            "the drop inside the callback returned"
        );
    });
}

/// An in-process subscription dropped while `CAC-event` reads its record for
/// a value already taken: that value reaches no callback.
#[test]
#[serial(epics_env)]
fn no_in_process_callback_runs_after_its_drop() {
    #[derive(Debug, PartialEq)]
    enum Seen {
        H(f64),
        C(f64),
    }
    on_a_blocking_thread(|| {
        let gate = Arc::new(Gate::default());
        let iocs = Iocs::start(&gate);
        let ctx = iocs.downstream.ca_client();
        // One channel: `CAC-event` calls the two callbacks in post order.
        let (tx, seen) = mpsc::channel();
        let h = tx.clone();
        // A control type, so the read takes the record's limits.
        let sub = monitor_in_process(&ctx, "H", DBR_CTRL_DOUBLE, move |v| {
            let _ = h.send(Seen::H(v));
        });
        assert_eq!(
            seen.recv_timeout(fact_budget::FACT_BUDGET),
            Ok(Seen::H(0.0))
        );
        let _c = monitor_in_process(&ctx, "C", DBR_DOUBLE, move |v| {
            let _ = tx.send(Seen::C(v));
        });
        assert_eq!(
            seen.recv_timeout(fact_budget::FACT_BUDGET),
            Ok(Seen::C(0.0))
        );
        let db = iocs.downstream.clone();
        thread::scope(|s| {
            let release = Release(&gate);
            let poster = s.spawn(move || post(&db, "H", 1.0));
            gate.until_held();
            drop(sub);
            drop(release);
            poster.join().unwrap();
        });
        iocs.post_downstream("C", 1.0);
        fact_budget::barrier::until(
            "a callback after its subscription dropped",
            |s: &Seen| matches!(s, Seen::H(_)),
            |s| *s == Seen::C(1.0),
            |left| seen.recv_timeout(left).ok(),
        );
    });
}
