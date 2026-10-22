//! The pv layer against an IOC: a name the IOC serves is an in-process
//! channel, any other goes to the network client, as `pvCa.c` over libca
//! with `dbContext` installed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_app::{IocApplication, IocInitDecision};
use epics_base_rs::types::{DbFieldType, EpicsValue};
use epics_ca_rs::calink::{install_calink_resolver, register_calink_commands};
use epics_seq_rs::pv::{PvEvent, PvEventType, PvSystem, PvVar};
use epics_seq_rs::{PvSevr, PvStat, PvType};
use serial_test::serial;
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq)]
enum Ev {
    Conn(&'static str, bool),
    Done {
        pv: &'static str,
        evt: PvEventType,
        count: u32,
        value: Option<Vec<f64>>,
        message: String,
    },
}

fn event(pv: &'static str, ev: PvEvent<'_>) -> Ev {
    assert_eq!(ev.status, PvStat::Ok, "statFromCA");
    let normal = ev.message == Some("Normal successful completion");
    assert_eq!(ev.severity == PvSevr::None, normal, "{:?}", ev.message);
    Ev::Done {
        pv,
        evt: ev.evt,
        count: ev.count,
        value: ev
            .value
            .map(|s| match s.value.convert_to(DbFieldType::Double) {
                EpicsValue::Double(v) => vec![v],
                EpicsValue::DoubleArray(a) => a.to_vec(),
                other => panic!("{other:?}"),
            }),
        message: ev.message.unwrap().to_string(),
    }
}

struct Events {
    tx: mpsc::UnboundedSender<Ev>,
    rx: mpsc::UnboundedReceiver<Ev>,
}

impl Events {
    fn new() -> Events {
        let (tx, rx) = mpsc::unbounded_channel();
        Events { tx, rx }
    }

    fn conn(&self, pv: &'static str) -> impl Fn(bool) + Send + Sync + 'static {
        let tx = self.tx.clone();
        move |up| tx.send(Ev::Conn(pv, up)).unwrap()
    }

    fn done(&self, pv: &'static str) -> impl FnOnce(PvEvent<'_>) + Send + 'static {
        let tx = self.tx.clone();
        move |ev| tx.send(event(pv, ev)).unwrap()
    }

    fn monitor(&self, pv: &'static str) -> impl Fn(PvEvent<'_>) + Send + Sync + 'static {
        let tx = self.tx.clone();
        move |ev| tx.send(event(pv, ev)).unwrap()
    }

    async fn next(&mut self, what: &str) -> Ev {
        epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, self.rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{what}"))
            .unwrap()
    }

    /// The next completion, its value and message.
    async fn done_value(&mut self, what: &str) -> (Option<Vec<f64>>, String) {
        match self.next(what).await {
            Ev::Done { value, message, .. } => (value, message),
            other => panic!("{what}: {other:?}"),
        }
    }
}

fn create(sys: &PvSystem, name: &str, events: &Events, tag: &'static str) -> PvVar {
    PvVar::create(sys, name, events.conn(tag)).expect("pvVarCreate")
}

const NORMAL: &str = "Normal successful completion";

/// Run `db_text` through iocInit with the `ca` link set installed and the
/// CA address list at `addr_list`, then hand the database to `body`.
async fn with_ioc<F, Fut>(db_text: &str, addr_list: String, body: F)
where
    F: FnOnce(Arc<PvDatabase>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    // SAFETY: `#[serial(epics_env)]` keeps every env-touching test apart.
    unsafe {
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_ADDR_LIST", addr_list);
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("seq.db");
    std::fs::write(&db_path, db_text).expect("write db");
    let stcmd_path = dir.path().join("st.cmd");
    std::fs::write(
        &stcmd_path,
        format!("dbLoadRecords(\"{}\")\n", db_path.display()),
    )
    .expect("write st.cmd");
    let reactor = epics_base_rs::runtime::task::Reactor::current().expect("test runtime");
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let finished = ran.clone();
    IocApplication::new()
        .startup_script(stcmd_path.to_str().unwrap())
        .register_link_type("ca", move |db| async move {
            register_calink_commands(install_calink_resolver(&db, reactor).await)
        })
        // C's `-S`: no prompt, so a test's stdin EOF cannot end the IOC
        // under the body.
        .before_ioc_init(|| IocInitDecision::run(false))
        .run(move |config| async move {
            body(config.db.clone()).await;
            finished.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
        .await
        .expect("IOC run");
    assert!(
        ran.load(std::sync::atomic::Ordering::SeqCst),
        "the body did not finish"
    );
}

/// Served names: connect, get, a put the next get sees, a put callback that
/// completes with the processing it started, monitors, and requests libca
/// refuses before queueing.
#[epics_macros_rs::epics_test]
#[serial(epics_env)]
async fn a_served_name_is_an_in_process_channel() {
    with_ioc(
        "record(ao, \"SEQ:AO\") { field(VAL, \"2\") field(DRVH, \"9\") }\n\
         record(calcout, \"SEQ:CO\") { field(CALC, \"A\") field(ODLY, \"0.4\") }\n\
         record(waveform, \"SEQ:WF\") { field(FTVL, \"DOUBLE\") field(NELM, \"10\") }\n",
        "192.0.2.1".into(),
        |db| async move {
            let sys = PvSystem::create(&db);
            assert_eq!(sys.attach(), PvStat::Ok);
            assert_eq!(sys.flush(), PvStat::Ok);
            let mut events = Events::new();

            let ao = create(&sys, "SEQ:AO", &events, "ao");
            assert!(ao.in_process(), "connected in the create");
            assert_eq!(ao.name(), "SEQ:AO");
            assert_eq!(ao.count(), 1);

            assert_eq!(
                ao.get_callback(PvType::TimeDouble, 1, events.done("ao")),
                PvStat::Ok
            );
            assert_eq!(
                events.next("ao get").await,
                Ev::Done {
                    pv: "ao",
                    evt: PvEventType::Get,
                    count: 1,
                    value: Some(vec![2.0]),
                    message: NORMAL.into(),
                }
            );
            assert_eq!(ao.message().as_deref(), Some(NORMAL));

            // A put is applied before a get made after it is answered.
            assert_eq!(
                ao.put_no_block(PvType::Double, 1, EpicsValue::Double(4.0)),
                PvStat::Ok
            );
            ao.get_callback(PvType::TimeDouble, 1, events.done("ao"));
            assert_eq!(
                events.done_value("get after put").await,
                (Some(vec![4.0]), NORMAL.into())
            );

            // `nciu::read`'s ECA_BADCOUNT: refused at once, nothing queued.
            let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
            let refused = ao.get_callback(PvType::TimeDouble, 2, move |_| {
                let _ = tx.send(());
            });
            assert_eq!(refused, PvStat::Ok, "statFromCA(ECA_BADCOUNT)");
            assert_eq!(
                ao.message().as_deref(),
                Some("Invalid element count requested")
            );
            assert!(
                matches!(
                    rx.try_recv(),
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed)
                ),
                "the refused get's callback was dropped uncalled"
            );

            // The first event is the value; a put is seen.
            assert_eq!(
                ao.monitor_on(&sys.gate(), PvType::TimeDouble, 1, events.monitor("ao")),
                PvStat::Ok
            );
            assert_eq!(
                events.next("first event").await,
                Ev::Done {
                    pv: "ao",
                    evt: PvEventType::Monitor,
                    count: 1,
                    value: Some(vec![4.0]),
                    message: NORMAL.into(),
                }
            );
            ao.put_no_block(PvType::Double, 1, EpicsValue::Double(5.0));
            assert_eq!(
                events.done_value("monitored put").await,
                (Some(vec![5.0]), NORMAL.into())
            );
            assert_eq!(ao.monitor_off(), PvStat::Ok);
            assert_eq!(ao.destroy(), PvStat::Ok);

            // A put callback completes when the processing it started does.
            let co = create(&sys, "SEQ:CO.A", &events, "co");
            assert!(co.in_process(), "connected in the create");
            let started = Instant::now();
            co.put_callback(
                PvType::Double,
                1,
                EpicsValue::Double(1.0),
                events.done("co"),
            );
            match events.next("put callback").await {
                Ev::Done {
                    pv: "co",
                    evt: PvEventType::Put,
                    value: None,
                    message,
                    ..
                } => assert_eq!(message, NORMAL),
                other => panic!("put callback: {other:?}"),
            }
            assert!(
                started.elapsed() >= Duration::from_millis(350),
                "completed after {:?}, ahead of the ODLY",
                started.elapsed()
            );

            // An array: the count is the server's; a request moves `count`.
            let wf = create(&sys, "SEQ:WF", &events, "wf");
            assert!(wf.in_process(), "connected in the create");
            assert_eq!(wf.count(), 10);
            wf.put_no_block(
                PvType::Double,
                3,
                EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0, 4.0].into()),
            );
            wf.get_callback(PvType::TimeDouble, 3, events.done("wf"));
            match events.next("array get").await {
                Ev::Done { count, value, .. } => {
                    assert_eq!(count, 3);
                    assert_eq!(value, Some(vec![1.0, 2.0, 3.0]));
                }
                other => panic!("array get: {other:?}"),
            }
        },
    )
    .await;
}

/// `dbChannelIO::initiateConnect` connects an in-process channel inside
/// `ca_create_channel`.
#[epics_macros_rs::epics_test]
#[serial(epics_env)]
async fn an_in_process_channel_is_connected_when_created() {
    with_ioc(
        "record(ao, \"SEQ:AO\") {}\n",
        "192.0.2.1".into(),
        |db| async move {
            let sys = PvSystem::create(&db);
            let mut events = Events::new();
            let ao = create(&sys, "SEQ:AO", &events, "ao");
            assert_eq!(ao.count(), 1);
            assert_eq!(
                ao.put_no_block(PvType::Double, 1, EpicsValue::Double(3.0)),
                PvStat::Ok
            );
            assert_eq!(ao.message(), None, "not refused as disconnected");
            epics_base_rs::runtime::task::sleep(Duration::from_millis(50)).await;
            assert!(events.rx.try_recv().is_err(), "no connection handler call");
        },
    )
    .await;
}

/// An in-process request runs on the caller's thread, as `dbContext`'s
/// does: a put has processed the record (`dbPutField`) when it returns, and
/// the callback of a get, or of a put whose processing ends at once, has
/// run.
#[epics_macros_rs::epics_test]
#[serial(epics_env)]
async fn an_in_process_request_completes_in_the_caller() {
    with_ioc(
        "record(ao, \"SEQ:AO\") { field(VAL, \"9.95\") field(DRVH, \"226\") field(DRVL, \"-229\") }\n",
        "192.0.2.1".into(),
        |db| async move {
            let sys = PvSystem::create(&db);
            let mut events = Events::new();
            let ao = create(&sys, "SEQ:AO", &events, "ao");
            assert!(ao.in_process(), "connected in the create");
            let drvh = create(&sys, "SEQ:AO.DRVH", &events, "drvh");
            assert!(drvh.in_process(), "connected in the create");

            // DRVH is pp(TRUE): the put processes the record, which clamps
            // VAL to it.
            assert_eq!(
                drvh.put_no_block(PvType::Double, 1, EpicsValue::Double(0.0)),
                PvStat::Ok
            );
            assert_eq!(db.get_pv("SEQ:AO").unwrap(), EpicsValue::Double(0.0));

            assert_eq!(
                ao.get_callback(PvType::TimeDouble, 1, events.done("ao")),
                PvStat::Ok
            );
            assert_eq!(
                events.rx.try_recv().ok(),
                Some(Ev::Done {
                    pv: "ao",
                    evt: PvEventType::Get,
                    count: 1,
                    value: Some(vec![0.0]),
                    message: NORMAL.into(),
                })
            );

            assert_eq!(
                drvh.put_callback(
                    PvType::Double,
                    1,
                    EpicsValue::Double(-1.0),
                    events.done("drvh")
                ),
                PvStat::Ok
            );
            assert_eq!(
                events.rx.try_recv().ok(),
                Some(Ev::Done {
                    pv: "drvh",
                    evt: PvEventType::Put,
                    count: 1,
                    value: None,
                    message: NORMAL.into(),
                })
            );
            assert_eq!(db.get_pv("SEQ:AO").unwrap(), EpicsValue::Double(-1.0));
        },
    )
    .await;
}

/// A put sends the variable's bits as its put type: an unsigned short
/// 0xffff is DBR_SHORT -1, and a char of either sign is the byte
/// `dbChannel_put` stores as DBR_UCHAR.
#[epics_macros_rs::epics_test]
#[serial(epics_env)]
async fn a_put_sends_the_variables_bits_as_its_put_type() {
    with_ioc(
        "record(ao, \"SEQ:AO\") {}\n\
         record(waveform, \"SEQ:WF\") { field(FTVL, \"SHORT\") field(NELM, \"4\") }\n",
        "192.0.2.1".into(),
        |db| async move {
            let sys = PvSystem::create(&db);
            let mut events = Events::new();
            let ao = create(&sys, "SEQ:AO", &events, "ao");
            assert!(ao.in_process(), "connected in the create");
            for (ty, value, want) in [
                (PvType::Short, EpicsValue::UShort(0xffff), -1.0),
                (PvType::Long, EpicsValue::ULong(0xffff_ffff), -1.0),
                (PvType::Char, EpicsValue::UChar(0xff), 255.0),
                (PvType::Char, EpicsValue::Char(0xff), 255.0),
            ] {
                let what = format!("{value:?} as {ty:?}");
                assert_eq!(ao.put_no_block(ty, 1, value.clone()), PvStat::Ok);
                ao.get_callback(PvType::TimeDouble, 1, events.done("ao"));
                assert_eq!(
                    events.done_value(&what).await,
                    (Some(vec![want]), NORMAL.into()),
                    "{what}"
                );
                ao.put_no_block(PvType::Double, 1, EpicsValue::Double(0.0));
                ao.put_callback(ty, 1, value, events.done("ao"));
                assert_eq!(events.done_value(&what).await, (None, NORMAL.into()));
                ao.get_callback(PvType::TimeDouble, 1, events.done("ao"));
                assert_eq!(
                    events.done_value(&what).await,
                    (Some(vec![want]), NORMAL.into()),
                    "put callback {what}"
                );
            }

            let wf = create(&sys, "SEQ:WF", &events, "wf");
            assert!(wf.in_process(), "connected in the create");
            wf.put_no_block(
                PvType::Short,
                2,
                EpicsValue::UShortArray(vec![0xffff, 0x8000, 1].into()),
            );
            wf.get_callback(PvType::TimeDouble, 2, events.done("wf"));
            assert_eq!(
                events.done_value("array").await,
                (Some(vec![-1.0, -32768.0]), NORMAL.into())
            );
        },
    )
    .await;
}

/// An unserved name goes to the network client. A disconnect completes the
/// put callback in flight with `ECA_DISCONN` before the connection handler
/// hears of it, and a request made while disconnected is refused.
#[cfg(tokio_backend)]
#[epics_macros_rs::epics_test]
#[serial(epics_env)]
async fn a_disconnect_fails_what_is_in_flight_then_tells_the_handler() {
    use epics_base_rs::server::records::ai::AiRecord;
    use epics_ca_rs::server::CaServer;

    const DISCONN: &str = "Virtual circuit disconnect";

    // The server runs on a runtime of its own, so dropping that runtime
    // closes its circuits: the disconnect the channels must report.
    let (port_tx, port_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let serving = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("server runtime");
        rt.block_on(async move {
            let server = CaServer::builder()
                .port(0)
                .record("SEQNET:AI", AiRecord::new(3.0))
                .db_string(
                    "record(calcout, \"SEQNET:CO\") { field(CALC, \"A\") field(ODLY, \"300\") }\n",
                    &Default::default(),
                )
                .expect("db")
                .build()
                .await
                .expect("CA server");
            port_tx.send(server.udp_port()).unwrap();
            tokio::spawn(async move { server.run().await });
            let _ = stop_rx.await;
        });
        rt.shutdown_timeout(Duration::from_secs(1));
    });
    let port = port_rx.recv().expect("server port");
    // SAFETY: `#[serial(epics_env)]`.
    unsafe {
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
    with_ioc(
        "record(ao, \"SEQ:LOCAL\") {}\n",
        format!("127.0.0.1:{port}"),
        move |db| async move {
            let sys = PvSystem::create(&db);
            let mut events = Events::new();
            let ai = create(&sys, "SEQNET:AI", &events, "ai");
            assert_eq!(events.next("ai connect").await, Ev::Conn("ai", true));
            assert!(!ai.in_process());
            assert_eq!(ai.count(), 1);
            ai.get_callback(PvType::TimeDouble, 1, events.done("ai"));
            assert_eq!(
                events.done_value("network get").await,
                (Some(vec![3.0]), NORMAL.into())
            );
            ai.put_callback(
                PvType::Double,
                1,
                EpicsValue::Double(6.0),
                events.done("ai"),
            );
            assert_eq!(
                events.done_value("network put callback").await,
                (None, NORMAL.into())
            );
            ai.get_callback(PvType::TimeDouble, 1, events.done("ai"));
            assert_eq!(
                events.done_value("get after put").await,
                (Some(vec![6.0]), NORMAL.into())
            );

            let co = create(&sys, "SEQNET:CO.A", &events, "co");
            assert_eq!(events.next("co connect").await, Ev::Conn("co", true));
            co.put_callback(
                PvType::Double,
                1,
                EpicsValue::Double(1.0),
                events.done("co"),
            );

            stop_tx.send(()).unwrap();
            let mut seen = Vec::new();
            while !seen.contains(&Ev::Conn("co", false)) || !seen.contains(&Ev::Conn("ai", false)) {
                seen.push(events.next("the disconnect was not reported").await);
            }
            let co_seen: Vec<_> = seen
                .iter()
                .filter(|e| matches!(e, Ev::Conn("co", _) | Ev::Done { pv: "co", .. }))
                .cloned()
                .collect();
            assert_eq!(
                co_seen,
                [
                    Ev::Done {
                        pv: "co",
                        evt: PvEventType::Put,
                        count: 1,
                        value: None,
                        message: DISCONN.into(),
                    },
                    Ev::Conn("co", false),
                ]
            );

            let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
            assert_eq!(
                ai.get_callback(PvType::TimeDouble, 1, move |_| {
                    let _ = tx.send(());
                }),
                PvStat::Ok
            );
            assert_eq!(ai.message().as_deref(), Some(DISCONN));
            assert!(matches!(
                rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed)
            ));
            assert_eq!(ai.count(), 0, "ca_element_count of a disconnected channel");
            assert_eq!(ai.destroy(), PvStat::Ok);
            assert_eq!(co.destroy(), PvStat::Ok);
        },
    )
    .await;
    serving.join().unwrap();
    // SAFETY: `#[serial(epics_env)]`.
    unsafe {
        std::env::remove_var("EPICS_CA_SERVER_PORT");
    }
}
