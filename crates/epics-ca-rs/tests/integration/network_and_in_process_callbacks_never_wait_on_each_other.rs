//! A record's own CA client context delivers its network monitor values and
//! its in-process ones apart, so neither waits on the other: in C the
//! network callbacks run on libca's receive thread under the context's
//! `cbMutex` (`tcpiiu.cpp:498-519`) and the in-process ones on `CAC-event`,
//! which takes only the context's `mutex` (`dbContext.cpp:176-192`).
#![cfg(feature = "client-core")]

// RTEMS-EXEC-MODEL-ALLOW(1): measured - both tests pass under
// `EPICS_RS_BUILD_EXEC_BACKEND=thread`, where the tokio runtime that hosts
// them here is not built.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use epics_base_rs::runtime::task::{background_init, block_on_sync};
use epics_base_rs::server::database::{
    CaClientContext, ClientChannel, ClientSubscription, Connection, PvDatabase,
};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{DBR_DOUBLE, EpicsValue};
use epics_ca_rs::calink::install_calink_resolver;
use epics_ca_rs::server::blocking::BlockingCaServer;
use serial_test::serial;

const UPSTREAM_DB: &str = "record(ao, \"NETIND:AO\") { field(VAL, \"0\") }\n";
const DOWN_DB: &str = r#"
record(ao, "A") { field(FLNK, "S") }
record(sub, "S") { field(SNAM, "hold") }
record(ao, "B") {}
"#;
/// `DBE_VALUE | DBE_ALARM`.
const MASK: u16 = 1 | 4;

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

/// An upstream IOC serving `NETIND:AO` over CA, and a downstream one whose
/// CA client reaches it, with `S`'s subroutine holding at `gate`.
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
        let held = gate.clone();
        let (downstream, _) = block_on_sync(
            IocBuilder::new()
                .register_subroutine("hold", move |_| {
                    held.hold();
                    Ok(0)
                })
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

    /// `value` put to upstream `NETIND:AO` by another client.
    fn post_upstream(&self, value: f64) {
        let post = self.upstream.put_record_field_from_ca_no_notify(
            "NETIND:AO",
            "VAL",
            EpicsValue::Double(value),
        );
        block_on_sync(post)
            .expect("a plain test thread may block")
            .unwrap();
    }

    /// `value` put to downstream `record` by another client.
    fn post_downstream(&self, record: &str, value: f64) {
        let post = self.downstream.put_record_field_from_ca_no_notify(
            record,
            "VAL",
            EpicsValue::Double(value),
        );
        block_on_sync(post)
            .expect("a plain test thread may block")
            .unwrap();
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

/// A subscription of `ch` whose callback hands each value to `value`.
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

/// While the context's in-process put on `A` is held in `A`'s forward link,
/// with `A` monitored by the context, a value of `NETIND:AO` reaches the
/// context's network monitor.
#[test]
#[serial(epics_env)]
fn a_network_value_lands_while_an_in_process_put_is_held() {
    on_a_blocking_thread(network_value_while_in_process_put_held);
}

fn network_value_while_in_process_put_held() {
    let gate = Arc::new(Gate::default());
    let iocs = Iocs::start(&gate);
    let ctx = iocs.downstream.ca_client();

    let (a_tx, a_rx) = mpsc::channel();
    let a = ctx.create_in_process("A").expect("A");
    let _a = a
        .subscribe(DBR_DOUBLE, 1, MASK, move |s| {
            let _ = a_tx.send(s.value.to_f64().unwrap());
        })
        .expect("subscribe A");
    assert_eq!(a_rx.recv_timeout(fact_budget::FACT_BUDGET), Ok(0.0));

    let net = connect(&ctx, "NETIND:AO");
    let (n_tx, n_rx) = mpsc::channel();
    let _n = monitor(&*net, move |v| {
        let _ = n_tx.send(v);
    });
    assert_eq!(n_rx.recv_timeout(fact_budget::FACT_BUDGET), Ok(0.0));

    thread::scope(|s| {
        let release = Release(&gate);
        let put = ctx.create_in_process("A").expect("A");
        let putter = s.spawn(move || put.put(EpicsValue::Double(1.0)));
        gate.until_held();
        iocs.post_upstream(7.0);
        assert_eq!(
            n_rx.recv_timeout(fact_budget::FACT_BUDGET),
            Ok(7.0),
            "the network monitor while the in-process put is held"
        );
        drop(release);
        putter.join().unwrap().unwrap();
    });
}

/// While the context's network monitor callback is held, a value posted on
/// `B` reaches the context's in-process monitor of `B`.
#[test]
#[serial(epics_env)]
fn an_in_process_value_lands_while_a_network_callback_is_held() {
    on_a_blocking_thread(in_process_value_while_network_callback_held);
}

fn in_process_value_while_network_callback_held() {
    let gate = Arc::new(Gate::default());
    let iocs = Iocs::start(&gate);
    let ctx = iocs.downstream.ca_client();

    let net = connect(&ctx, "NETIND:AO");
    let (n_tx, n_rx) = mpsc::channel();
    let held = gate.clone();
    let _n = monitor(&*net, move |v| {
        let _ = n_tx.send(v);
        if v == 7.0 {
            held.hold();
        }
    });
    assert_eq!(n_rx.recv_timeout(fact_budget::FACT_BUDGET), Ok(0.0));

    let (b_tx, b_rx) = mpsc::channel();
    let b = ctx.create_in_process("B").expect("B");
    let _b = b
        .subscribe(DBR_DOUBLE, 1, MASK, move |s| {
            let _ = b_tx.send(s.value.to_f64().unwrap());
        })
        .expect("subscribe B");
    assert_eq!(b_rx.recv_timeout(fact_budget::FACT_BUDGET), Ok(0.0));

    let release = Release(&gate);
    iocs.post_upstream(7.0);
    gate.until_held();
    iocs.post_downstream("B", 5.0);
    assert_eq!(
        b_rx.recv_timeout(fact_budget::FACT_BUDGET),
        Ok(5.0),
        "the in-process monitor while the network callback is held"
    );
    drop(release);
}
