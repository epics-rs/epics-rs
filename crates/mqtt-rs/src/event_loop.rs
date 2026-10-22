use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;
use std::time::Duration;

use asyn_rs::param::ParamValue;
use asyn_rs::port_handle::PortHandle;
use asyn_rs::request::ParamSetValue;
use asyn_rs::trace::TraceMask;
use rumqttc::{
    AsyncClient, ClientError, ConnectionError, Event, Incoming, MqttOptions, Outgoing, Packet,
    PubAckReason, PubCompReason, PubRecReason, PublishNoticeError, PublishOptions, PublishResult,
    SubAck, SubscribeNoticeError, SubscribeReasonCode,
};
use socket2::{Domain, Socket, Type};
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

use crate::address::{PayloadFormat, TopicAddress, ValueType};
use crate::config::MqttConfig;
use crate::driver::{PublishRequest, SharedTopicMap, TopicMap};
use crate::error::MqttError;
use crate::payload::{DecodedValue, decode_payload, octet_bytes_cstr, octet_cstr};

/// The two lifecycle signals the event loop waits on. Both are raised by the
/// port side and consumed here, so they travel together.
pub struct Lifecycle {
    /// Released by the `AfterScanInit` init hook: C defers the broker connect
    /// (and its first subscribe) to `initHookAfterScanInit`
    /// (drvMqtt.cpp:124,186-189).
    pub start: Arc<Notify>,
    /// Raised by `MqttDriver`'s `Drop`: C disconnects from `~MqttClient`
    /// (mqttClient.cpp:37-41).
    pub shutdown: Arc<Notify>,
    /// The other end of that `Drop`, which is blocked on it. C's `disconnect()`
    /// waits for the packet (`->wait()`, mqttClient.cpp:51-55); this is what
    /// ends the wait. The event loop owns it for its whole life, so every way
    /// the loop can end releases the driver — see
    /// [`MqttDriver::teardown_ack`](crate::driver::MqttDriver::teardown_ack).
    pub done: std::sync::mpsc::Sender<()>,
}

/// Run the MQTT event loop.
///
/// This task:
/// 1. Waits for `start` (the `AfterScanInit` init hook) before connecting, then
///    connects to the broker — C parity: `drvMqtt` defers `mqttClient.connect()`
///    to `setInitHook(initHook)` so the connection (and its first subscribe) only
///    happens after records have bound (drvMqtt.cpp:124,186-189).
/// 2. On every connect, subscribes only to the topics of records bound by
///    `I/O Intr` / `asyn:READBACK` — the live interrupt-variable set
///    (`getInterruptVariables()`, drvMqtt.cpp:207-213).
/// 3. Dispatches incoming messages to the param cache via `PortHandle`,
///    delivering only to interrupt-bound records (drvMqtt.cpp:250-255).
/// 4. Publishes outgoing messages from EPICS write operations.
/// 5. On `shutdown` (the driver's `Drop`), sends a DISCONNECT and returns —
///    C parity: `~MqttClient` calls `disconnect()` (mqttClient.cpp:37-41).
pub async fn mqtt_event_loop(
    config: MqttConfig,
    topic_map: SharedTopicMap,
    port_handle: PortHandle,
    publish_rx: mpsc::UnboundedReceiver<PublishRequest>,
    connected_param: usize,
    connected: Arc<AtomicBool>,
    lifecycle: Lifecycle,
) {
    let Lifecycle {
        start,
        shutdown,
        done,
    } = lifecycle;
    let mut mqttoptions =
        MqttOptions::new(&config.client_id, (&config.broker_host, config.broker_port));
    // MQTT carries the keep-alive in 16 bits (paho's `keepAliveInterval` too).
    mqttoptions.set_keep_alive(u16::try_from(config.keep_alive_secs).unwrap_or(u16::MAX));
    // C connects with MQTT v5 connOpts (mqttClient.cpp:20-22): clean_start +
    // keepAlive, no will. rumqttc's v5 client uses `clean_start` (the v5
    // spelling) in place of v3.1.1's `clean_session`.
    mqttoptions.set_clean_start(config.clean_session);
    mqttoptions.set_socket_connector(|host, _| paho_connect(host));
    // paho's `connectTimeout` in C's v5 connect options (MQTTAsync.h:1394).
    mqttoptions.set_connect_timeout(Duration::from_secs(30));

    // Unbounded: `Commands` hands with `try_`, also from the task that drains
    // the channel, and the exit hands everything still waiting at once.
    let (client, mut eventloop) = AsyncClient::builder(mqttoptions).unbounded().build();

    // `EventLoop::poll()` is not cancel-safe (rumqttc internal iterators can be
    // left half-advanced if the future is dropped mid-poll), so we must never
    // drive it inside a `tokio::select!`. Instead, outbound publishes are
    // forwarded on a dedicated task, and the main loop only awaits `poll()`.
    let (acks, ack_rx) = mpsc::unbounded_channel();
    tokio::spawn(ack_task(ack_rx, port_handle.clone()));
    let commands = Commands::new(client.clone(), acks.clone(), port_handle.clone());
    tokio::spawn(publish_task(commands.clone(), publish_rx));

    // MQ4 shutdown. C's `~MqttClient` calls `disconnect()`
    // (mqttClient.cpp:37-41), which sends a DISCONNECT and waits for the packet
    // to go out while the session is up (mqttClient.cpp:51-55). Its default
    // options carry `timeout` 0 (MQTTAsync.h:1484), so paho does not wait for
    // in-flight QoS 1/2 flows (`MQTTAsync_checkDisconnect`,
    // MQTTAsyncUtils.c:975-978): `disconnect_now`, not rumqttc's graceful
    // `disconnect`, which holds the DISCONNECT until every flow is acked.
    // `poll()` writes AND FLUSHES the packet before it yields
    // `Outgoing::Disconnect` (rumqttc eventloop.rs:1293-1310, :1473-1478,
    // :1506-1533), and the loop breaks on that event — so the break happens
    // after the bytes are on the wire, which is what `disconnect()->wait()`
    // guarantees in C.
    //
    // Armed here, ahead of the `start` wait below, because an IOC can be told
    // to exit before it finishes booting: a shutdown raised while this task is
    // still parked on `start` would otherwise be seen by nobody, and the
    // `Drop` that raised it would wait out its whole timeout.
    let shutting_down = Arc::new(AtomicBool::new(false));
    // A second signal rather than a second waiter on `shutdown`: `notify_one`
    // wakes exactly one waiter, so two waiters on the one signal would race for
    // it and the loser would wait forever. `notify_one` also leaves a permit
    // when nobody is waiting yet, so the `select!` below cannot miss it by
    // registering late.
    let abort = Arc::new(Notify::new());
    {
        let flag = shutting_down.clone();
        let dc = client.clone();
        let abort = abort.clone();
        let commands = commands.clone();
        tokio::spawn(async move {
            shutdown.notified().await;
            flag.store(true, Ordering::Release);
            abort.notify_one();
            commands.close();
            let _ = dc.disconnect_now().await;
        });
    }

    // C parity: `drvMqtt` does not connect from its constructor; it registers
    // `setInitHook(initHook)` (drvMqtt.cpp:124) and only calls
    // `mqttClient.connect()` from that hook at `initHookAfterScanInit`
    // (drvMqtt.cpp:186-189), i.e. after every record has bound and registered
    // (or not) its `I/O Intr`. We mirror that by holding the first `poll()`
    // (which triggers rumqttc's TCP connect) until the `AfterScanInit` hook
    // fires `start`. This guarantees the first ConnAck's subscribe sees the
    // fully-populated interrupt-variable set rather than racing iocInit.
    tokio::select! {
        _ = start.notified() => {}
        // Shutdown before `AfterScanInit`: the IOC is leaving before it ever
        // connected, and C sends nothing with the session down
        // (`if (client_.is_connected())`, mqttClient.cpp:52). Returning drops
        // `done`, which is what releases the driver's `Drop`.
        _ = abort.notified() => return,
    }

    // Records create their topic params on demand during iocInit
    // (`drv_user_create`), so the shared topic map is only final once
    // `AfterScanInit` has fired. EPICS never creates records at runtime, so the
    // map is now frozen: snapshot it once and read only the local copy below.
    let topic_map = topic_map.lock().unwrap().clone();
    // Reverse index reason -> MQTT topic, used on ConnAck to translate the live
    // interrupt-variable set (asyn reasons) back into the topics to subscribe.
    let reason_to_topic = reason_topic_index(&topic_map);

    // Subscriptions are driven exclusively on ConnAck (covers both the first
    // connect and every reconnect), so no pre-loop subscribe is needed.
    //
    // `session` mirrors the Connected PV locally so we can detect the
    // "stuck at 0" case where a recoverable rumqttc error flipped the PV to 0
    // but the underlying TCP/MQTT session is still alive (no ConnAck will come
    // to flip it back). Any inbound packet (`Publish`, `PingResp`) is direct
    // proof the session is alive, so we use it as a fallback recovery signal.
    let mut session = Session::Booting;
    let mut backoff = Backoff::new(Instant::now());
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Incoming::Publish(publish))) => {
                if session != Session::Up {
                    tracing::debug!(
                        "MQTT Publish received while Connected=0 — restoring Connected=1"
                    );
                    mark_connected(&port_handle, connected_param, &connected).await;
                    session = Session::Up;
                    commands.session_up(None);
                }
                // v5 Publish.topic is raw bytes; a declared topic_map key (the
                // asyn drvInfo) is UTF-8 text, so a non-UTF-8 topic can never
                // match any subscriber and is dropped. Distinct from MQ38's
                // payload gate: there the drop denied readable subscribers,
                // here the lookup has nothing to find either way.
                match std::str::from_utf8(&publish.topic) {
                    Ok(topic) => {
                        handle_incoming_message(topic, &publish.payload, &topic_map, &port_handle)
                            .await;
                    }
                    Err(e) => asyn_print(
                        &port_handle,
                        TraceMask::ERROR,
                        format_args!("MqttDriver::onMessageCb: Non-UTF8 MQTT topic: {e}\n"),
                    ),
                }
            }
            Ok(Event::Incoming(Incoming::ConnAck(connack))) => {
                mark_connected(&port_handle, connected_param, &connected).await;
                backoff.connected();
                if session == Session::Spent {
                    asyn_print(
                        &port_handle,
                        TraceMask::IO_DRIVER | TraceMask::ERROR,
                        format_args!("MqttDriver::onConnectCb: Reconnected.\n"),
                    );
                }
                session = Session::Up;
                // paho's `maxInflightMessages`: 65535 in C's v5 connect options
                // (MQTTAsync.h:1394), lowered to the broker's Receive Maximum
                // (MQTTAsyncUtils.c:2199-2206).
                commands.session_up(Some(
                    connack
                        .properties
                        .and_then(|p| p.receive_max)
                        .unwrap_or(u16::MAX),
                ));
                asyn_print(
                    &port_handle,
                    TraceMask::IO_DRIVER,
                    format_args!("MqttDriver::onConnectCb: Connected to broker\n"),
                );
                // C parity: `onConnectCb` subscribes only the topics of records
                // bound by `I/O Intr` / `asyn:READBACK` — the live
                // interrupt-variable set (`getInterruptVariables()`,
                // drvMqtt.cpp:207-213). Resolving it at every ConnAck means a
                // reconnect re-subscribes whatever set is currently bound.
                let sub_topics = subscribe_topics(
                    &reason_to_topic,
                    &port_handle.interrupts().subscribed_bindings(),
                );
                for topic in sub_topics {
                    commands.push(Command::Subscribe {
                        topic,
                        qos: config.qos.into(),
                    });
                }
            }
            Ok(Event::Outgoing(Outgoing::Publish(pkid))) => commands.written(pkid != 0),
            Ok(Event::Outgoing(Outgoing::Subscribe(_))) => commands.written(false),
            // rumqttc's `inflight -= 1` (state.rs:1805, :1853, :1924).
            Ok(Event::Incoming(Incoming::PubAck(_) | Incoming::PubComp(_))) => commands.acked(),
            Ok(Event::Incoming(Incoming::PubRec(rec)))
                if !matches!(
                    rec.reason,
                    PubRecReason::Success | PubRecReason::NoMatchingSubscribers
                ) =>
            {
                commands.acked()
            }
            Ok(Event::Incoming(Incoming::PingResp)) => {
                if session != Session::Up {
                    tracing::debug!(
                        "MQTT PingResp received while Connected=0 — restoring Connected=1"
                    );
                    mark_connected(&port_handle, connected_param, &connected).await;
                    session = Session::Up;
                    commands.session_up(None);
                }
            }
            Ok(Event::Outgoing(Outgoing::Disconnect)) => {
                tracing::info!("MQTT DISCONNECT sent; event loop exiting");
                break;
            }
            Err(e) => {
                // rumqttc ends the connection on every error. paho fails what
                // the session owed before `connectionLost` reports the loss,
                // and keeps what it had not yet written.
                commands.session_down();
                end_session(&acks).await;
                // A shutdown with the session already down: C sends nothing at
                // all (`if (client_.is_connected())`, mqttClient.cpp:52), so
                // exit without reconnecting rather than sleeping and retrying.
                if shutting_down.load(Ordering::Acquire) {
                    tracing::info!("MQTT event loop exiting while disconnected: {e}");
                    break;
                }
                let retry_at = if session == Session::Up {
                    backoff.lost(Instant::now())
                } else {
                    backoff.failed(Instant::now())
                };
                // C reports the loss of a live session from `onDisconnectCb`
                // (drvMqtt.cpp:219-220), with no cause, and a failed attempt of
                // the initHook connect from `onFailCb` (drvMqtt.cpp:234-235).
                match session {
                    Session::Up => {
                        asyn_print(
                            &port_handle,
                            TraceMask::ERROR,
                            format_args!(
                                "MqttDriver::onDisconnectCb: Connection lost. Reconnecting...\n"
                            ),
                        );
                        session = Session::Spent;
                    }
                    Session::Booting => {
                        let failure = ConnectFailure::of(&e);
                        print_fail(&port_handle, failure.text());
                        if failure != ConnectFailure::Dispatch {
                            session = Session::Spent;
                        }
                    }
                    Session::Spent => {}
                }
                // Close the publish gate first so a concurrent write fails fast
                // (C: publish throws while disconnected) instead of buffering.
                connected.store(false, Ordering::Release);
                let _ = port_handle
                    .set_params_and_notify(
                        0,
                        vec![ParamSetValue::new(connected_param, 0, ParamValue::Int32(0))],
                    )
                    .await;
                // `poll()` is not running, so this `select!` drops nothing
                // half-done; a shutdown cannot wait out a 72 s retry.
                tokio::select! {
                    _ = tokio::time::sleep_until(retry_at) => {}
                    _ = abort.notified() => {
                        tracing::info!("MQTT event loop exiting while disconnected");
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    // The loop breaks only after `Outgoing::Disconnect` — the packet is written
    // and flushed by then — or on an error while shutting down, where C sends
    // nothing at all because the session is already down (mqttClient.cpp:52).
    // Either way the goodbye this driver owed its broker is finished, so
    // release the `Drop` that is blocked waiting for it. Dropping `done` would
    // release it too; sending first is what lets the far side tell a loop that
    // ended from a task that died.
    end_session(&acks).await;
    let _ = done.send(());
}

/// How paho ends one SUBSCRIBE or PUBLISH: `onSuccess` on the granted SUBACK,
/// on the PUBACK/PUBCOMP, or on the write for QoS 0 (MQTTAsyncUtils.c:1474-1500,
/// :3157-3209); `onFailure5` on a refusal, a SUBACK for the one filter C
/// subscribes at a time or a PUBACK/PUBREC/PUBCOMP whose reason is 0x80 or
/// above (:2234-2248, :3199-3209), with the session left up, and when the
/// request dies with its session (:1902-1938).
#[derive(Debug, PartialEq, Eq)]
enum AckLine {
    Succeeded,
    /// `onFailCb`'s text: empty where paho hands `onFailure5` no message, a
    /// broker refusal or a request its session took with it.
    Failed(String),
}

fn subscribe_line(result: &Result<SubAck, SubscribeNoticeError>) -> AckLine {
    match result {
        Ok(ack)
            if matches!(
                ack.return_codes.as_slice(),
                [SubscribeReasonCode::Success(_)]
            ) =>
        {
            AckLine::Succeeded
        }
        Ok(_) | Err(SubscribeNoticeError::SessionReset | SubscribeNoticeError::Recv) => {
            AckLine::Failed(String::new())
        }
        Err(e) => AckLine::Failed(e.to_string()),
    }
}

fn publish_line(result: &Result<PublishResult, PublishNoticeError>) -> AckLine {
    let refused = match result {
        Ok(PublishResult::Qos0Flushed) => false,
        Ok(PublishResult::Qos1(ack)) => !matches!(
            ack.reason,
            PubAckReason::Success | PubAckReason::NoMatchingSubscribers
        ),
        Ok(PublishResult::Qos2PubRecRejected(_)) => true,
        Ok(PublishResult::Qos2Completed(ack)) => ack.reason != PubCompReason::Success,
        Err(PublishNoticeError::SessionReset | PublishNoticeError::Recv) => true,
        Err(e) => return AckLine::Failed(e.to_string()),
    };
    if refused {
        AckLine::Failed(String::new())
    } else {
        AckLine::Succeeded
    }
}

/// A SUBSCRIBE or PUBLISH rumqttc took: the ack that ends it and its
/// `onSuccess` text.
struct Owed {
    ack: std::pin::Pin<Box<dyn std::future::Future<Output = AckLine> + Send>>,
    success: String,
}

enum AckRequest {
    Owe(Owed),
    /// The session ended; answered once every owed request has its line.
    SessionEnded(tokio::sync::oneshot::Sender<()>),
}

/// The one owner of every SUBSCRIBE and PUBLISH line: paho's `responses`
/// list. A request prints its ack's line, or fails with its session, since
/// `MQTTAsync_closeSession` fails every request still owed an ack (v5
/// sessionExpiry is 0) on a loss and on the exit disconnect alike
/// (MQTTAsyncUtils.c:979, :1687, :1902-1938, :2491-2498). An ack that came in
/// before the end keeps its own line.
async fn ack_task(mut requests: mpsc::UnboundedReceiver<AckRequest>, port_handle: PortHandle) {
    let mut owed: Vec<Owed> = Vec::new();
    let mut closed = false;
    std::future::poll_fn(|cx| {
        while !closed {
            match requests.poll_recv(cx) {
                Poll::Ready(Some(AckRequest::Owe(o))) => owed.push(o),
                Poll::Ready(Some(AckRequest::SessionEnded(ended))) => {
                    for mut o in owed.drain(..) {
                        let line = match o.ack.as_mut().poll(cx) {
                            Poll::Ready(line) => line,
                            Poll::Pending => AckLine::Failed(String::new()),
                        };
                        print_ack(&port_handle, line, &o.success);
                    }
                    let _ = ended.send(());
                }
                Poll::Ready(None) => closed = true,
                Poll::Pending => break,
            }
        }
        owed.retain_mut(|o| match o.ack.as_mut().poll(cx) {
            Poll::Ready(line) => {
                print_ack(&port_handle, line, &o.success);
                false
            }
            Poll::Pending => true,
        });
        if closed && owed.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await
}

/// End the session for [`ack_task`] and wait until it has printed what the
/// session owed, so those lines come before the loss is reported.
async fn end_session(acks: &mpsc::UnboundedSender<AckRequest>) {
    let (ended, printed) = tokio::sync::oneshot::channel();
    if acks.send(AckRequest::SessionEnded(ended)).is_ok() {
        let _ = printed.await;
    }
}

fn print_ack(port_handle: &PortHandle, line: AckLine, success: &str) {
    match line {
        AckLine::Succeeded => {
            asyn_print(port_handle, TraceMask::IO_DRIVER, format_args!("{success}"))
        }
        AckLine::Failed(e) => print_fail(port_handle, &e),
    }
}

fn print_fail(port_handle: &PortHandle, e: &str) {
    asyn_print(
        port_handle,
        TraceMask::ERROR,
        // paho's `errorMsg` ends in '\n' (mqttClient.cpp:167).
        format_args!("MqttDriver::onFailCb: MQTT Operation error: 'Error: {e}\n'\n"),
    );
}

/// One SUBSCRIBE or PUBLISH the IOC asked for.
enum Command {
    Subscribe { topic: String, qos: rumqttc::QoS },
    Publish(PublishRequest),
}

impl Command {
    fn flow_controlled(&self) -> bool {
        match self {
            Self::Subscribe { .. } => true,
            Self::Publish(p) => p.qos != crate::config::QoS::AtMostOnce,
        }
    }
}

/// paho's `MQTTAsync_commands` for this client. paho writes a command only
/// with the session up and keeps an unwritten one over a loss for the next
/// session (MQTTAsyncUtils.c:1248-1250); only what it wrote fails with the
/// session (:1902-1938). rumqttc fails every request it holds when a clean
/// session starts, so it is handed one command at a time: the next goes once
/// rumqttc yields the last one's `Outgoing` event, and the rest wait here.
#[derive(Clone)]
struct Commands(Arc<CommandsInner>);

struct CommandsInner {
    client: AsyncClient,
    acks: mpsc::UnboundedSender<AckRequest>,
    port_handle: PortHandle,
    queue: std::sync::Mutex<CommandQueue>,
}

struct CommandQueue {
    waiting: VecDeque<Command>,
    /// The handed command rumqttc has not written yet, by its number.
    held: Option<u64>,
    handed: u64,
    /// QoS 1/2 publishes written and not yet acked: paho's `outboundMsgs`.
    inflight: u16,
    /// paho's `maxInflightMessages`.
    window: u16,
    up: bool,
    /// The driver is going: what waits is handed at once, or fails.
    closing: bool,
}

impl Commands {
    fn new(
        client: AsyncClient,
        acks: mpsc::UnboundedSender<AckRequest>,
        port_handle: PortHandle,
    ) -> Self {
        Self(Arc::new(CommandsInner {
            client,
            acks,
            port_handle,
            queue: std::sync::Mutex::new(CommandQueue {
                waiting: VecDeque::new(),
                held: None,
                handed: 0,
                inflight: 0,
                window: u16::MAX,
                up: false,
                closing: false,
            }),
        }))
    }

    fn push(&self, command: Command) {
        let mut queue = self.0.queue.lock().unwrap();
        queue.waiting.push_back(command);
        self.feed(&mut queue);
    }

    /// rumqttc wrote the held command, a QoS 1/2 publish if `inflight`.
    fn written(&self, inflight: bool) {
        let mut queue = self.0.queue.lock().unwrap();
        queue.held = None;
        if inflight {
            queue.inflight += 1;
        }
        self.feed(&mut queue);
    }

    /// The broker ended a QoS 1/2 publish's flow.
    fn acked(&self) {
        let mut queue = self.0.queue.lock().unwrap();
        queue.inflight = queue.inflight.saturating_sub(1);
        self.feed(&mut queue);
    }

    /// rumqttc refused command `n` without writing it, so no `Outgoing` event
    /// will release it.
    fn refused(&self, n: u64) {
        let mut queue = self.0.queue.lock().unwrap();
        if queue.held == Some(n) {
            queue.held = None;
            self.feed(&mut queue);
        }
    }

    /// A session is up, with the broker's window if a ConnAck brought it.
    fn session_up(&self, window: Option<u16>) {
        let mut queue = self.0.queue.lock().unwrap();
        queue.up = true;
        if let Some(window) = window {
            queue.window = window;
        }
        self.feed(&mut queue);
    }

    /// The held command ends with the session; its ack is owed to
    /// [`ack_task`], which fails it. What waits stays for the next session.
    fn session_down(&self) {
        let mut queue = self.0.queue.lock().unwrap();
        queue.up = false;
        queue.held = None;
        queue.inflight = 0;
    }

    /// `~MqttClient`: paho sends what it queued ahead of the DISCONNECT
    /// (MQTTAsyncUtils.c:803-830) and fails what it never could
    /// (`MQTTAsync_freeCommands`, :1950-1990).
    fn close(&self) {
        let mut queue = self.0.queue.lock().unwrap();
        queue.closing = true;
        self.feed(&mut queue);
    }

    /// Hand the head of the queue on while rumqttc can write it at once.
    /// paho's flow control holds a QoS 1/2 PUBLISH or a SUBSCRIBE at the head
    /// while the window is full (MQTTAsyncUtils.c:1252-1262); rumqttc's
    /// holds the publish alone, unwritten, where the loss would fail it.
    fn feed(&self, queue: &mut CommandQueue) {
        while queue.closing || (queue.up && queue.held.is_none()) {
            let Some(head) = queue.waiting.front() else {
                return;
            };
            if !queue.closing && head.flow_controlled() && queue.inflight >= queue.window {
                return;
            }
            let Some(command) = queue.waiting.pop_front() else {
                return;
            };
            if !queue.up {
                print_fail(&self.0.port_handle, "");
                continue;
            }
            queue.handed += 1;
            if self.hand(command, queue.handed) {
                queue.held = Some(queue.handed);
            }
        }
    }

    /// Hand `command` to rumqttc as number `n` and its notice to
    /// [`ack_task`]. `try_`: the event loop task feeds too, and it is what
    /// drains the request channel.
    fn hand(&self, command: Command, n: u64) -> bool {
        let CommandsInner {
            client,
            acks,
            port_handle,
            ..
        } = &*self.0;
        let commands = Arc::downgrade(&self.0);
        let refused = move || {
            if let Some(inner) = commands.upgrade() {
                Commands(inner).refused(n);
            }
        };
        let owed = match command {
            Command::Subscribe { topic, qos } => {
                match client.try_subscribe_tracked(topic.as_str(), qos) {
                    Ok(notice) => Owed {
                        ack: Box::pin(async move {
                            let result = notice.wait_async().await;
                            if matches!(result, Err(SubscribeNoticeError::BrokerOnlySessionResume))
                            {
                                refused();
                            }
                            subscribe_line(&result)
                        }),
                        success: format!(
                            "MqttDriver::onSubscribeCb: Subscribed to topic '{topic}'\n"
                        ),
                    },
                    Err(e) => {
                        let e = match e {
                            ClientError::InvalidRequest(_) => {
                                format!("invalid topic filter {topic}")
                            }
                            e => e.to_string(),
                        };
                        print_fail(port_handle, &e);
                        return false;
                    }
                }
            }
            Command::Publish(PublishRequest {
                topic,
                payload,
                qos,
                retained,
            }) => {
                let options = PublishOptions::new(qos.into()).retain(retained);
                // The owned `Vec<u8>` carries raw non-UTF-8 octet bytes through.
                match client.try_publish_tracked(topic.as_str(), payload, options) {
                    Ok(notice) => Owed {
                        ack: Box::pin(async move {
                            let result = notice.wait_async().await;
                            if matches!(
                                result,
                                Err(PublishNoticeError::RetainNotSupported
                                    | PublishNoticeError::BrokerOnlySessionResume)
                            ) {
                                refused();
                            }
                            publish_line(&result)
                        }),
                        success: format!("MqttDriver::onPublishCb: Published to topic '{topic}'\n"),
                    },
                    Err(e) => {
                        print_fail(port_handle, &e.to_string());
                        return false;
                    }
                }
            }
        };
        let _ = acks.send(AckRequest::Owe(owed));
        true
    }
}

/// The session as paho's connect token sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Session {
    /// The initHook `connect()` token is live. Its `onFailure` reports an
    /// attempt that fails while paho dispatches it and stays set for the
    /// retry (paho.mqtt.c MQTTAsyncUtils.c:1574-1626); its `onSuccess` makes
    /// the first ConnAck a connect rather than `Reconnected.` (:2195).
    Booting,
    Up,
    /// Down with the token spent: by a ConnAck (:2173-2174), or by a failure
    /// met once the TCP connect was in flight, which `nextOrClose` reports
    /// and then nulls (:1685-1712). A failed retry prints nothing and the
    /// next ConnAck is `Reconnected.`.
    Spent,
}

/// How paho reports a failed initHook connect through `onFailure5`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectFailure {
    /// Met while `MQTTAsync_processCommand` dispatches the CONNECT: no
    /// message, and the token stays for the retry (MQTTAsyncUtils.c:1574-1626).
    Dispatch,
    /// Met by `nextOrClose`, which nulls the token (:1685-1712), with the
    /// message of the site that called it.
    Closed(&'static str),
}

impl ConnectFailure {
    fn of(e: &ConnectionError) -> Self {
        Self::Closed(match e {
            ConnectionError::Io(io) => {
                match io.get_ref().and_then(|e| e.downcast_ref::<TcpFailure>()) {
                    Some(TcpFailure::Dispatch(_)) => return Self::Dispatch,
                    // `MQTTAsync_connecting`'s `SO_ERROR` (:3055-3063, :3051).
                    Some(TcpFailure::Completion(_)) => "TCP/TLS connect failure",
                    None => "TCP connect completion failure",
                }
            }
            ConnectionError::Timeout(_) => "TCP connect timeout",
            ConnectionError::ConnectionRefused(_) => "CONNACK return code",
            ConnectionError::NotConnAck(p) if matches!(**p, Packet::Disconnect(_)) => {
                "Received disconnect"
            }
            // What the CONNACK wait read was no CONNACK (:3095-3099).
            _ => "TCP connect completion failure",
        })
    }

    fn text(self) -> &'static str {
        match self {
            Self::Dispatch => "",
            Self::Closed(message) => message,
        }
    }
}

/// Where [`paho_connect`]'s TCP connect failed.
#[derive(Debug)]
enum TcpFailure {
    /// Before the connect is in flight: the address lookup, `socket()`, or a
    /// non-blocking `connect()` that fails at once (Socket.c:1194-1279).
    Dispatch(io::Error),
    /// The in-flight connect's completion.
    Completion(io::Error),
}

impl std::fmt::Display for TcpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dispatch(e) | Self::Completion(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for TcpFailure {}

/// paho's `Socket_new`: the first IPv4 address, else the last one, and one
/// non-blocking `connect()` (Socket.c:1194-1279).
async fn paho_connect(host: String) -> io::Result<tokio::net::TcpStream> {
    let dispatch = |e: io::Error| io::Error::new(e.kind(), TcpFailure::Dispatch(e));
    let completion = |e: io::Error| io::Error::new(e.kind(), TcpFailure::Completion(e));
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(host)
        .await
        .map_err(dispatch)?
        .collect();
    let Some(&addr) = addrs.iter().find(|a| a.is_ipv4()).or(addrs.last()) else {
        return Err(dispatch(io::Error::new(
            io::ErrorKind::InvalidInput,
            "could not resolve to any address",
        )));
    };
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, None).map_err(dispatch)?;
    socket.set_nonblocking(true).map_err(dispatch)?;
    if let Err(e) = socket.connect(&addr.into())
        && !connect_pending(&e)
    {
        return Err(dispatch(e));
    }
    let stream = tokio::net::TcpStream::from_std(socket.into()).map_err(completion)?;
    stream.writable().await.map_err(completion)?;
    match stream.take_error().map_err(completion)? {
        Some(e) => Err(completion(e)),
        None => Ok(stream),
    }
}

/// paho's `rc == EINPROGRESS || rc == EWOULDBLOCK` (Socket.c:1280).
fn connect_pending(e: &io::Error) -> bool {
    #[cfg(unix)]
    if e.raw_os_error() == Some(libc::EINPROGRESS) {
        return true;
    }
    e.kind() == io::ErrorKind::WouldBlock
}

/// paho's `automaticReconnect` retry with the v5 connect-option defaults C
/// builds (`minRetryInterval` 1 s, `maxRetryInterval` 60 s, MQTTAsync.h:1394).
///
/// paho retries only from `MQTTAsync_checkTimeouts`, which runs at most once
/// per 3 s (MQTTAsyncUtils.c:1734-1736) on the send thread `MQTTAsync_connect`
/// starts (MQTTAsync.c:670), so a retry lands on that 3 s grid.
struct Backoff {
    /// `currentIntervalBase` in seconds; `None` while not `retrying`.
    base: Option<u32>,
    /// The initHook connect, where the grid starts.
    ticks_from: Instant,
}

const RETRY_MIN_SECS: u32 = 1;
const RETRY_MAX_SECS: u32 = 60;
const RETRY_TICK_SECS: u64 = 3;

impl Backoff {
    fn new(ticks_from: Instant) -> Self {
        Self {
            base: None,
            ticks_from,
        }
    }

    /// `MQTTAsync_startConnectRetry` (MQTTAsyncUtils.c:951-967) after an
    /// attempt failed at `now`: the base doubles up to the maximum, the
    /// interval is a whole second drawn around it, and the retry is the first
    /// tick past it (`elapsed > currentInterval`, :1761).
    fn failed(&mut self, now: Instant) -> Instant {
        let base = self
            .base
            .map_or(RETRY_MIN_SECS, |b| (b * 2).min(RETRY_MAX_SECS));
        self.base = Some(base);
        let draw =
            std::hash::BuildHasher::hash_one(&std::collections::hash_map::RandomState::new(), base);
        self.tick_after(now + Duration::from_secs(u64::from(jitter_secs(base, draw))))
    }

    /// The loss of a live session at `now`: `onDisconnectCb`'s `reconnect()`
    /// resets the base to the minimum and sets `reconnectNow`
    /// (MQTTAsync.c:956-964), paho's own `startConnectRetry` then doubles the
    /// base (MQTTAsyncUtils.c:1714-1719), and the retry is the next tick.
    fn lost(&mut self, now: Instant) -> Instant {
        self.base = Some(RETRY_MIN_SECS * 2);
        self.tick_after(now)
    }

    fn tick_after(&self, t: Instant) -> Instant {
        let secs = t.saturating_duration_since(self.ticks_from).as_secs();
        self.ticks_from + Duration::from_secs((secs / RETRY_TICK_SECS + 1) * RETRY_TICK_SECS)
    }

    /// A ConnAck clears `retrying` (MQTTAsyncUtils.c:2014).
    fn connected(&mut self) {
        self.base = None;
    }
}

/// `MQTTAsync_randomJitter` (MQTTAsyncUtils.c:124-153): a whole number of
/// seconds in `[max(min, base) / 1.2, min(max, base) * 1.2]`, truncated as C's
/// `(int)` casts truncate.
fn jitter_secs(base: u32, draw: u64) -> u32 {
    let hi = (f64::from(base.min(RETRY_MAX_SECS)) * 1.2) as u32;
    let lo = (f64::from(base.max(RETRY_MIN_SECS)) / 1.2) as u32;
    if lo >= hi {
        return lo;
    }
    lo + (draw % u64::from(hi - lo + 1)) as u32
}

/// Mark the session up: raise the internal publish-gate flag and the
/// EPICS-visible Connected PV together. The event loop is the single writer of
/// both, so the driver write path (which reads `connected`) and the Connected
/// PV never disagree.
async fn mark_connected(port_handle: &PortHandle, connected_param: usize, connected: &AtomicBool) {
    connected.store(true, Ordering::Release);
    let _ = port_handle
        .set_params_and_notify(
            0,
            vec![ParamSetValue::new(connected_param, 0, ParamValue::Int32(1))],
        )
        .await;
}

/// Queue the publish requests of EPICS writes as [`Commands`]. Runs on its
/// own task so the main event-loop task can own `poll()` exclusively without
/// cancel-safety hazards.
async fn publish_task(commands: Commands, mut publish_rx: mpsc::UnboundedReceiver<PublishRequest>) {
    while let Some(req) = publish_rx.recv().await {
        commands.push(Command::Publish(req));
    }
}

/// Build the reverse index `reason -> topic` from the topic map. The ConnAck
/// subscribe uses it to translate the interrupt-variable set (asyn reasons)
/// back into the MQTT topics to subscribe.
fn reason_topic_index(topic_map: &TopicMap) -> HashMap<usize, String> {
    let mut index = HashMap::new();
    for (topic, subs) in topic_map {
        for (reason, _addr) in subs {
            index.insert(*reason, topic.clone());
        }
    }
    index
}

/// What one subscriber consumes from a payload: bytes for the flat octet
/// store, text for everything that parses.
///
/// The distinction is C's, not an optimisation. `onMessageCb` builds one
/// `std::string val = payload` (drvMqtt.cpp:248) and then treats it two ways
/// inside the per-record loop: `asynParamOctet` hands `val.c_str()` straight to
/// `setStringParam` (drvMqtt.cpp:299) with no parse and no encoding
/// requirement, while every other `asynType` runs a text parser over it. Only
/// the second needs a `&str`, so only the second can fail on encoding.
enum SubscriberInput<'a> {
    /// Flat + STRING: the raw bytes up to the first NUL, stored verbatim.
    OctetBytes(&'a [u8]),
    /// Everything else: the text C's parser for this subscriber would read.
    Text(&'a str),
}

/// What this subscriber consumes, or `None` when C's own handling could not
/// have succeeded either.
///
/// MQ38: every record parses inside C's per-record loop, so bytes that are not
/// text fail *that* record and leave the topic's other records alone — which is
/// why the UTF-8 test belongs here, once per subscriber, rather than at the
/// message where it denied the whole topic. The flat octet store never takes
/// that test at all, because C never decodes for it.
///
/// A JSON subscriber still requires UTF-8 even for an octet value: its `val`
/// comes from `json::parse(payload)` (drvMqtt.cpp:255-264), so a payload that
/// is not text fails C's parse too and the record is skipped.
fn subscriber_input<'a>(payload: &'a [u8], addr: &TopicAddress) -> Option<SubscriberInput<'a>> {
    if let (PayloadFormat::Flat, ValueType::String) = (addr.format, addr.value_type) {
        return Some(SubscriberInput::OctetBytes(octet_bytes_cstr(payload)));
    }
    std::str::from_utf8(payload).ok().map(SubscriberInput::Text)
}

/// An array payload: C posts it with Autoparam doCallbacksArray where it is
/// decoded (drvMqtt.cpp:301-322). It is stored as well, because the rs
/// `read_*_array` serves the last payload where C has no read handler.
fn push_array(batch: &mut Vec<ParamSetValue>, reason: usize, value: ParamValue) {
    batch.push(ParamSetValue::new(reason, 0, value.clone()));
    batch.push(ParamSetValue::array(reason, 0, value));
}

/// Decode one payload for every interrupt-bound subscriber on its topic.
///
/// Split out of [`handle_incoming_message`] so the per-subscriber independence
/// MQ38 restores is testable without a broker: the decision is pure, and only
/// the batch it returns touches the port.
fn decode_for_subscribers(
    payload: &[u8],
    subscribers: &[(usize, TopicAddress)],
    bound_reasons: &HashSet<usize>,
    topic: &str,
    report: &mut dyn FnMut(std::fmt::Arguments<'_>),
) -> Vec<ParamSetValue> {
    let mut batch_updates = Vec::new();

    for (reason, addr) in subscribers {
        if !bound_reasons.contains(reason) {
            continue;
        }
        let Some(input) = subscriber_input(payload, addr) else {
            // C parity: `onMessageCb` fails this record's parse and moves to
            // the next (drvMqtt.cpp:250-296); before MQ38 a payload that is not
            // UTF-8 returned from the message and denied every subscriber on
            // the topic an update C would have delivered to each.
            // A JSON subscriber fails in `json::parse`, a flat one in its value
            // parser.
            let json = addr.format == PayloadFormat::Json;
            report_drop(
                report,
                topic,
                payload,
                addr,
                json,
                &"payload is not UTF-8 text",
            );
            continue;
        };
        let text = match input {
            // No parse step: C's octet branch stores the bytes it was handed.
            SubscriberInput::OctetBytes(bytes) => {
                batch_updates.push(ParamSetValue::new(
                    *reason,
                    0,
                    ParamValue::Octet(bytes.into()),
                ));
                continue;
            }
            SubscriberInput::Text(text) => text,
        };
        match decode_payload(text, addr) {
            Ok(decoded) => {
                // ParamSetValue carries every inbound value shape:
                // Int32, Float64, Octet, Float64Array, Int32Array,
                // UInt32Digital.
                match decoded {
                    DecodedValue::Int32(v) => {
                        batch_updates.push(ParamSetValue::new(*reason, 0, ParamValue::Int32(v)));
                    }
                    DecodedValue::Float64(v) => {
                        batch_updates.push(ParamSetValue::new(*reason, 0, ParamValue::Float64(v)));
                    }
                    DecodedValue::String(v) => {
                        // asyn octet store truncates at the first NUL
                        // (setStringParam(index, val.c_str()), drvMqtt.cpp:299).
                        batch_updates.push(ParamSetValue::new(
                            *reason,
                            0,
                            ParamValue::Octet(octet_cstr(&v).as_bytes().into()),
                        ));
                    }
                    DecodedValue::Float64Array(v) => {
                        push_array(
                            &mut batch_updates,
                            *reason,
                            ParamValue::Float64Array(v.into()),
                        );
                    }
                    DecodedValue::UInt32(v) => {
                        // Inbound MQTT value: changed bits derive from the value
                        // merge; no forced interrupt mask.
                        batch_updates.push(ParamSetValue::uint32_digital(
                            *reason,
                            0,
                            v,
                            0xFFFF_FFFF,
                            0,
                        ));
                    }
                    DecodedValue::Int32Array(v) => {
                        push_array(
                            &mut batch_updates,
                            *reason,
                            ParamValue::Int32Array(v.into()),
                        );
                    }
                }
            }
            Err(e) => {
                let json = matches!(e, MqttError::JsonParse(_) | MqttError::JsonFieldNotFound(_));
                report_drop(report, topic, payload, addr, json, &e);
            }
        }
    }

    batch_updates
}

/// The `ASYN_TRACE_ERROR` line C's `onMessageCb` prints for a subscriber it
/// skips: the JSON text when the parse or field lookup failed
/// (drvMqtt.cpp:268-270), the value text otherwise (drvMqtt.cpp:328-330).
fn report_drop(
    report: &mut dyn FnMut(std::fmt::Arguments<'_>),
    topic: &str,
    payload: &[u8],
    addr: &TopicAddress,
    json: bool,
    why: &dyn std::fmt::Display,
) {
    if json {
        let field = addr.json_field.as_deref().unwrap_or_default();
        report(format_args!(
            "MqttDriver::onMessageCb: Failed to parse JSON payload for topic '{topic}', field '{field}': {why}\n"
        ));
    } else {
        // C's argument order, unbalanced `)` included.
        report(format_args!(
            "MqttDriver::onMessageCb:{why}: Unexpected value received for topic: '{topic}': {})\n",
            CText(payload),
        ));
    }
}

/// C's `payload.c_str()` as trace text: cut at the first NUL, bytes that are
/// not UTF-8 shown as U+FFFD.
struct CText<'a>(&'a [u8]);

impl std::fmt::Display for CText<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for chunk in octet_bytes_cstr(self.0).utf8_chunks() {
            f.write_str(chunk.valid())?;
            if !chunk.invalid().is_empty() {
                f.write_str("\u{FFFD}")?;
            }
        }
        Ok(())
    }
}

/// Resolve the de-duplicated set of MQTT topics to subscribe from the live
/// interrupt-variable bindings.
///
/// C parity: `onConnectCb` walks `getInterruptVariables()` and subscribes
/// `addr.topicName` for each (drvMqtt.cpp:207-213); a topic shared by several
/// bound records is subscribed once. A binding whose reason has no topic (e.g.
/// the internal `_MQTT_CONNECTED` param) maps to nothing and is skipped.
fn subscribe_topics(
    reason_to_topic: &HashMap<usize, String>,
    bindings: &[(usize, i32)],
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut topics = Vec::new();
    for (reason, _addr) in bindings {
        if let Some(topic) = reason_to_topic.get(reason)
            && seen.insert(topic.as_str())
        {
            topics.push(topic.clone());
        }
    }
    topics
}

/// C `asynPrint(pasynUserSelf, mask, ...)`: gated by this port's
/// `asynSetTraceMask`, written to its trace file.
fn asyn_print(port_handle: &PortHandle, mask: TraceMask, args: std::fmt::Arguments<'_>) {
    port_handle
        .trace()
        .print(Some(port_handle.port_name()), None, 0, mask, args);
}

async fn handle_incoming_message(
    topic: &str,
    payload: &[u8],
    topic_map: &TopicMap,
    port_handle: &PortHandle,
) {
    let subscribers = match topic_map.get(topic) {
        Some(subs) => subs,
        None => return,
    };

    // C parity: `onMessageCb` delivers an inbound payload only to records that
    // are interrupt-bound — it iterates `getInterruptVariables()`
    // (drvMqtt.cpp:250-255). The subscribe filter already keeps us off topics
    // with no interrupt-bound record, but a single topic can carry both an
    // `I/O Intr` record and an output/periodic record on the same reason set,
    // so we re-filter per delivery: a non-interrupt param must not have its
    // value set from the wire.
    let bound_reasons: HashSet<usize> = port_handle
        .interrupts()
        .subscribed_bindings()
        .into_iter()
        .map(|(reason, _addr)| reason)
        .collect();

    // C `asynPrint(pasynUserSelf, ASYN_TRACE_ERROR, ...)`: on by default and
    // governed by this port's `asynSetTraceMask`.
    let mut report =
        |args: std::fmt::Arguments<'_>| asyn_print(port_handle, TraceMask::ERROR, args);
    let batch_updates =
        decode_for_subscribers(payload, subscribers, &bound_reasons, topic, &mut report);

    if !batch_updates.is_empty()
        && let Err(e) = port_handle.set_params_and_notify(0, batch_updates).await
    {
        epics_libcom_rs::stderr_println!("set_params_and_notify error (mqtt payload): {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(topic: &str) -> TopicAddress {
        TopicAddress::parse(&format!("FLAT:INT {topic}")).unwrap()
    }

    #[test]
    fn subscribe_topics_selects_only_interrupt_bound_reasons() {
        let mut topic_map: TopicMap = HashMap::new();
        topic_map.insert("a/b".into(), vec![(1, addr("a/b")), (3, addr("a/b"))]);
        topic_map.insert("c/d".into(), vec![(2, addr("c/d"))]);
        let idx = reason_topic_index(&topic_map);

        // Only reason 1 is interrupt-bound -> only its topic is subscribed; the
        // pre-registered-but-unbound topic c/d is NOT subscribed (MQ2: subscribe
        // the I/O-Intr set, not all declared topics).
        assert_eq!(subscribe_topics(&idx, &[(1, 0)]), vec!["a/b".to_string()]);

        // reasons 1 and 3 share topic a/b -> subscribed exactly once (C
        // onConnectCb subscribes each interrupt var's topic, deduped on the wire).
        assert_eq!(
            subscribe_topics(&idx, &[(1, 0), (3, 0)]),
            vec!["a/b".to_string()]
        );

        // No interrupt bindings -> nothing subscribed (setAutoInterrupts(false):
        // a port with no I/O-Intr record subscribes nothing).
        assert!(subscribe_topics(&idx, &[]).is_empty());
    }

    /// MQ38 (small half): a payload that is not valid UTF-8 must fail only the
    /// subscribers whose parse C would also have failed, not every subscriber
    /// on the topic. C runs one `try`/`catch` per interrupt variable
    /// (drvMqtt.cpp:250-296), so a STRING record still stores `val.c_str()`
    /// while a sibling INT record on the same topic fails to parse.
    ///
    /// Before the fix `handle_incoming_message` returned at the first
    /// `from_utf8` error, so both records were denied the update.
    #[test]
    fn a_non_utf8_payload_denies_only_the_subscribers_c_would_deny() {
        let string_addr = TopicAddress::parse("FLAT:STRING sensor/raw").unwrap();
        let int_addr = TopicAddress::parse("FLAT:INT sensor/raw").unwrap();
        let subscribers = vec![(1usize, string_addr), (2usize, int_addr)];
        let bound: HashSet<usize> = [1usize, 2].into_iter().collect();

        // C: `std::string val("hello\0\xff\xfe", 8)`; setStringParam stores
        // val.c_str() == "hello". The INT record's strtol never consumes it.
        let batch = decode_for_subscribers(
            b"hello\x00\xff\xfe",
            &subscribers,
            &bound,
            "sensor/raw",
            &mut |_| {},
        );
        assert_eq!(
            batch.len(),
            1,
            "exactly the STRING subscriber updates: {batch:?}"
        );
        match &batch[0] {
            ParamSetValue::Value { reason, value, .. } => {
                assert_eq!(*reason, 1);
                match value {
                    ParamValue::Octet(v) => assert_eq!(&v[..], b"hello"),
                    other => panic!("expected Octet, got {other:?}"),
                }
            }
            other => panic!("expected a value set, got {other:?}"),
        }

        // Text that both can read still reaches both.
        let batch = decode_for_subscribers(b"42", &subscribers, &bound, "sensor/raw", &mut |_| {});
        assert_eq!(batch.len(), 2, "a readable payload reaches both: {batch:?}");

        // Bytes that are not text at all and carry no NUL: C's octet branch
        // never decodes, so `setStringParam(val.c_str())` stores both bytes and
        // only the INT record's parse fails. The port matches now that
        // `ParamValue::Octet` carries `Vec<u8>`.
        let batch =
            decode_for_subscribers(b"\xff\xfe", &subscribers, &bound, "sensor/raw", &mut |_| {});
        assert_eq!(
            batch.len(),
            1,
            "the octet store takes raw bytes; only INT is denied: {batch:?}"
        );
        match &batch[0] {
            ParamSetValue::Value { reason, value, .. } => {
                assert_eq!(*reason, 1);
                match value {
                    ParamValue::Octet(v) => assert_eq!(&v[..], b"\xff\xfe"),
                    other => panic!("expected Octet, got {other:?}"),
                }
            }
            other => panic!("expected a value set, got {other:?}"),
        }

        // The NUL is still the octet terminator, and it is now the ONLY thing
        // that truncates: a payload that is nothing but non-text bytes before
        // its NUL stores that prefix rather than being denied.
        let batch = decode_for_subscribers(
            b"\xff\xfe\x00tail",
            &subscribers,
            &bound,
            "sensor/raw",
            &mut |_| {},
        );
        assert_eq!(batch.len(), 1, "{batch:?}");
        match &batch[0] {
            ParamSetValue::Value { value, .. } => match value {
                ParamValue::Octet(v) => assert_eq!(&v[..], b"\xff\xfe"),
                other => panic!("expected Octet, got {other:?}"),
            },
            other => panic!("expected a value set, got {other:?}"),
        }

        // A JSON subscriber still needs UTF-8 for its octet value: C's `val`
        // comes from `json::parse(payload)` (drvMqtt.cpp:255-264), which fails
        // on the same bytes, so the record is skipped there too.
        let json_addr = TopicAddress::parse("JSON:STRING sensor/raw name").unwrap();
        let json_subs = vec![(3usize, json_addr)];
        let json_bound: HashSet<usize> = [3usize].into_iter().collect();
        let batch = decode_for_subscribers(
            b"\xff\xfe",
            &json_subs,
            &json_bound,
            "sensor/raw",
            &mut |_| {},
        );
        assert!(batch.is_empty(), "JSON octet still needs text: {batch:?}");
    }

    /// onMessageCb posts an array with Autoparam doCallbacksArray as it
    /// decodes it, typed, and the scalars at its closing callParamCallbacks
    /// (drvMqtt.cpp:301-333).
    #[test]
    fn an_array_payload_posts_typed_ahead_of_the_scalar_flush() {
        use crate::driver::MqttDriver;
        use asyn_rs::interfaces::InterfaceType;
        use asyn_rs::port::{DrvUserRequest, PortDriver};
        use std::sync::atomic::AtomicBool;

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut driver = MqttDriver::new(
            "MQARRAY",
            &crate::config::MqttConfig::default(),
            Vec::new(),
            tx,
            Arc::new(AtomicBool::new(true)),
        );
        let specs = [
            "FLAT:STRING sensor/v",
            "FLAT:FLOATARRAY sensor/v",
            "FLAT:INTARRAY sensor/v",
        ];
        let reasons: Vec<usize> = specs
            .iter()
            .map(|spec| {
                driver
                    .drv_user_create(&DrvUserRequest::new(*spec, 0))
                    .unwrap()
                    .reason
            })
            .collect();
        let base = driver.base_mut();
        base.own_interrupts(epics_libcom_rs::runtime::interrupt_accept::InterruptGate::opened());
        base.call_param_callbacks(0).unwrap();
        let mut rx = base.interrupts.subscribe_async(64, 4096);

        let subscribers: Vec<_> = reasons
            .iter()
            .zip(specs)
            .map(|(&r, spec)| (r, TopicAddress::parse(spec).unwrap()))
            .collect();
        let bound: HashSet<usize> = reasons.iter().copied().collect();
        let batch = decode_for_subscribers(b"1,2", &subscribers, &bound, "sensor/v", &mut |_| {});
        for update in &batch {
            base.set_param(update).unwrap();
        }
        base.call_param_callbacks(0).unwrap();

        let posted: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|v| (v.reason, v.iface))
            .collect();
        assert_eq!(
            posted,
            [
                (reasons[1], Some(InterfaceType::Float64Array)),
                (reasons[2], Some(InterfaceType::Int32Array)),
                (reasons[0], None),
            ]
        );
        // The rs read of an array record serves the last payload.
        let mut buf = [0.0; 4];
        let user = asyn_rs::user::AsynUser::new(reasons[1]);
        assert_eq!(driver.read_float64_array(&user, &mut buf).unwrap(), 2);
        assert_eq!(buf[..2], [1.0, 2.0]);
    }

    /// MQ38 end to end: the raw bytes an inbound payload carries reach the octet
    /// a STRING record reads back, through the real port actor and param store.
    ///
    /// This is the observable the row names — "a STRING record holds the raw
    /// bytes up to the first NUL, as `setStringParam` does (drvMqtt.cpp:299)" —
    /// and it needs the whole chain, because each link used to lose it
    /// separately: the message-level `from_utf8` denied the topic, the
    /// subscriber-level one denied the octet store, and `ParamValue::Octet`
    /// could not hold the bytes even once they arrived.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_non_utf8_payload_reaches_the_octet_a_string_record_reads() {
        use crate::driver::MqttDriver;
        use asyn_rs::port::{DrvUserRequest, PortDriver};
        use asyn_rs::runtime::config::RuntimeConfig;
        use asyn_rs::runtime::port::create_port_runtime_unclaimed;
        use std::sync::atomic::AtomicBool;

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut driver = MqttDriver::new(
            "MQ38",
            &crate::config::MqttConfig::default(),
            Vec::new(),
            tx,
            Arc::new(AtomicBool::new(true)),
        );
        let spec = "FLAT:STRING sensor/raw";
        let reason = driver
            .drv_user_create(&DrvUserRequest::new(spec, 0))
            .expect("bind the STRING topic")
            .reason;
        let (rt, _jh) = create_port_runtime_unclaimed(driver, RuntimeConfig::default())
            .expect("the port actor thread must start");
        let port = rt.port_handle().clone();

        let subscribers = vec![(reason, TopicAddress::parse(spec).unwrap())];
        let bound: HashSet<usize> = [reason].into_iter().collect();

        // Not text, and a NUL after it: C stores `val.c_str()`, i.e. the two
        // raw bytes, and drops the tail.
        let batch = decode_for_subscribers(
            b"\xff\xfe\x00tail",
            &subscribers,
            &bound,
            "sensor/raw",
            &mut |_| {},
        );
        port.set_params_and_notify(0, batch)
            .await
            .expect("the store takes the batch");
        // asynPortDriver::readOctet counts the NUL: `strlen(value) + 1`.
        let (got, status) = port.read_octet(reason, 0, 32).await.into_parts();
        status.expect("octet read");
        assert_eq!(
            got.as_slice(),
            b"\xff\xfe\0",
            "the record must read the raw bytes, not U+FFFD and not nothing"
        );

        // And a plain text payload still round-trips unchanged.
        let batch =
            decode_for_subscribers(b"hello", &subscribers, &bound, "sensor/raw", &mut |_| {});
        port.set_params_and_notify(0, batch).await.unwrap();
        let (got, status) = port.read_octet(reason, 0, 32).await.into_parts();
        status.unwrap();
        assert_eq!(got.as_slice(), b"hello\0");

        rt.shutdown();
    }

    /// The client's connection, its CONNECT read. The listener's ephemeral
    /// port is open to every process on the box, so a connection that closes
    /// before its first packet is someone else's and is skipped; any other
    /// first packet fails.
    async fn accept_connect(listener: &tokio::net::TcpListener) -> tokio::net::TcpStream {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            if let Some(kind) = read_packet_type(&mut sock).await {
                assert_eq!(kind, 1, "CONNECT");
                return sock;
            }
        }
    }

    #[tokio::test]
    #[should_panic(expected = "CONNECT")]
    async fn a_wrong_first_packet_fails_the_accept() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        // PINGREQ, and the connection stays open.
        client.write_all(&[0xC0, 0x00]).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), accept_connect(&listener)).await;
    }

    /// Read one MQTT control packet from `sock` and return its type nibble.
    /// Framing only — the variable header and payload are skipped, so a 0xE0
    /// byte *inside* a packet cannot be mistaken for a DISCONNECT.
    async fn read_packet_type(sock: &mut tokio::net::TcpStream) -> Option<u8> {
        read_packet(sock).await.map(|(kind, _)| kind)
    }

    /// One MQTT control packet: its type nibble and everything after the
    /// fixed header.
    async fn read_packet(sock: &mut tokio::net::TcpStream) -> Option<(u8, Vec<u8>)> {
        use tokio::io::AsyncReadExt;
        let first = sock.read_u8().await.ok()?;
        let mut remaining = 0usize;
        let mut shift = 0;
        loop {
            let b = sock.read_u8().await.ok()?;
            remaining |= ((b & 0x7f) as usize) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        let mut body = vec![0u8; remaining];
        sock.read_exact(&mut body).await.ok()?;
        Some((first >> 4, body))
    }

    /// MQ4: at teardown the broker must receive an MQTT DISCONNECT packet
    /// (type 14), not a bare TCP close. C's `~MqttClient` calls `disconnect()`
    /// (mqttClient.cpp:37-41,51-55); here the driver's `Drop` raises the signal
    /// and the event loop turns it into the packet.
    ///
    /// Before the fix `mqtt_event_loop`'s `loop` had no `break` and no path
    /// that sent DISCONNECT, so this test saw the connection close with no
    /// packet and failed on the `Some(14)` assertion.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_sends_disconnect_before_the_loop_exits() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let broker_port = listener.local_addr().unwrap().port();
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<Option<u8>>();
        let (up_tx, up_rx) = tokio::sync::oneshot::channel::<()>();

        // Minimal broker: CONNACK the CONNECT, then report the type of the next
        // packet the client sends (or None if it just closed the socket).
        tokio::spawn(async move {
            let mut sock = accept_connect(&listener).await;
            // v5 CONNACK: session_present=0, reason=Success, 0 properties.
            sock.write_all(&[0x20, 0x03, 0x00, 0x00, 0x00])
                .await
                .unwrap();
            // The session is up. Announced before the read below, because the
            // teardown this test is about is only defined from here on.
            let _ = up_tx.send(());
            let _ = seen_tx.send(read_packet_type(&mut sock).await);
        });

        let config = MqttConfig {
            broker_host: "127.0.0.1".into(),
            broker_port,
            client_id: "mq4-test".into(),
            // Long enough that no PINGREQ can arrive before the DISCONNECT.
            keep_alive_secs: 3600,
            ..MqttConfig::default()
        };

        let (publish_tx, publish_rx) = mpsc::unbounded_channel();
        let connected = Arc::new(AtomicBool::new(false));
        let mut driver = crate::driver::MqttDriver::new(
            "MQ4TEST",
            &config,
            Vec::new(),
            publish_tx,
            connected.clone(),
        );
        let topic_map = driver.topic_map();
        let connected_param = driver.connected_param;
        let shutdown = driver.shutdown_signal();
        let teardown_done = driver.teardown_ack();
        let (runtime, _jh) = asyn_rs::runtime::port::create_port_runtime_unclaimed(
            driver,
            asyn_rs::runtime::config::RuntimeConfig::default(),
        )
        .unwrap();
        let port_handle = runtime.port_handle().clone();

        let start = Arc::new(Notify::new());
        let loop_start = start.clone();
        let ev = tokio::spawn(async move {
            mqtt_event_loop(
                config,
                topic_map,
                port_handle,
                publish_rx,
                connected_param,
                connected,
                Lifecycle {
                    start: loop_start,
                    shutdown,
                    done: teardown_done,
                },
            )
            .await;
        });
        start.notify_one();

        // Wait for the session before tearing it down. C's `~MqttClient` sends
        // the packet under `if (client_.is_connected())` (mqttClient.cpp:52),
        // so an unconnected teardown is a different claim entirely — and it has
        // its own test in `a_shutdown_before_the_start_hook_still_releases_the_driver`
        // below. Without this the two signals reached `mqtt_event_loop`'s
        // `select!` together and it was free to take the `abort` arm and return
        // before connecting: the broker then saw no connection at all and the
        // 10 s budget below expired, on 2 of 20 SOLO runs.
        tokio::time::timeout(Duration::from_secs(10), up_rx)
            .await
            .expect("the broker CONNACKed within 10s")
            .expect("broker task died");

        // Stop the port actor, which drops the driver — the production teardown
        // point, and the one C destructs at.
        runtime.shutdown();

        let seen = tokio::time::timeout(Duration::from_secs(10), seen_rx)
            .await
            .expect("broker saw no further packet within 10s")
            .expect("broker task died");
        assert_eq!(
            seen,
            Some(14),
            "broker must receive a DISCONNECT (type 14) at teardown, not a bare close"
        );

        // And the loop must actually return rather than reconnecting.
        tokio::time::timeout(Duration::from_secs(10), ev)
            .await
            .expect("event loop did not exit after DISCONNECT")
            .unwrap();
    }

    /// C prints the session's edges through `asynPrint` (drvMqtt.cpp:202-205,
    /// :219, :234): one boundary each. A refused boot connect fails once the
    /// TCP connect is in flight (Linux answers a non-blocking `connect()` with
    /// EINPROGRESS even on loopback), so paho reports it once and spends the
    /// token: later failures print nothing and the first ConnAck is
    /// `Reconnected.`. An error on a live session is a loss, a failed retry
    /// after it prints nothing, and the ConnAck after it is `Reconnected.`.
    /// A loopback port the test's broker owns from first bind to last: a
    /// socket bound and never listening holds it, so a connect is refused,
    /// and each [`Self::listen`] binds a listener beside it. A port released
    /// between phases is free for any socket on the box to take — under load,
    /// an outgoing connect's source port — and the next bind then fails.
    struct BrokerPort {
        _hold: (socket2::Socket, std::net::TcpStream),
        addr: SocketAddr,
    }

    impl BrokerPort {
        fn new() -> Self {
            let hold = Self::socket();
            // Unix shares a bound port only among `SO_REUSEPORT` sockets, so
            // the hold sets it; Windows lets an `SO_REUSEADDR` bind join any
            // non-exclusive one, so the hold sets nothing.
            #[cfg(unix)]
            hold.set_reuse_port(true).unwrap();
            hold.bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
                .unwrap();
            let addr = hold.local_addr().unwrap().as_socket().unwrap();
            // Connected to a peer it keeps: Darwin and the BSDs hand a SYN to
            // a bound socket that is not listening and drop it there, so the
            // connect would time out instead of being refused.
            let peer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            hold.connect(&peer.local_addr().unwrap().into()).unwrap();
            let (peer, _) = peer.accept().unwrap();
            Self {
                _hold: (hold, peer),
                addr,
            }
        }

        fn socket() -> socket2::Socket {
            socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap()
        }

        /// The broker, up until the listener drops.
        fn listen(&self) -> tokio::net::TcpListener {
            let l = Self::socket();
            // REUSEADDR also takes the port past the last session's TIME_WAIT.
            l.set_reuse_address(true).unwrap();
            #[cfg(unix)]
            l.set_reuse_port(true).unwrap();
            l.bind(&self.addr.into()).unwrap();
            l.listen(128).unwrap();
            l.set_nonblocking(true).unwrap();
            tokio::net::TcpListener::from_std(l.into()).unwrap()
        }
    }

    /// The release-and-rebind this replaces lost the number to a socket that
    /// took it in between; held, no plain bind can take it, and the broker
    /// still comes up on it (refused, up, refused, up). Plain: no reuse
    /// option, as a `connect()`'s implicit bind; Darwin's lookup for an
    /// `SO_REUSEADDR` one skips the connected hold.
    #[tokio::test]
    async fn a_held_broker_port_keeps_its_number_between_sessions() {
        let port = BrokerPort::new();
        let refused = || std::net::TcpStream::connect(port.addr).unwrap_err().kind();
        let plain_bind = || {
            BrokerPort::socket()
                .bind(&port.addr.into())
                .unwrap_err()
                .kind()
        };
        assert_eq!(refused(), io::ErrorKind::ConnectionRefused);
        assert_eq!(plain_bind(), io::ErrorKind::AddrInUse);
        for _ in 0..2 {
            let listener = port.listen();
            let client = tokio::net::TcpStream::connect(port.addr).await.unwrap();
            let (served, _) = listener.accept().await.unwrap();
            drop((listener, served, client));
            assert_eq!(refused(), io::ErrorKind::ConnectionRefused);
            assert_eq!(plain_bind(), io::ErrorKind::AddrInUse);
        }
    }

    /// The ConnAck arm's last line. A reader that waits for an earlier one,
    /// `Reconnected.`, can look between the arm's two prints.
    const CONNACK_ARM_DONE: &str = "onConnectCb: Connected to broker";

    /// An event loop for a broker at `addr`, its port tracing ERROR and
    /// IO_DRIVER into `file`, started, and the driver's publish channel.
    fn spawn_traced_loop(
        port: &str,
        addr: SocketAddr,
        file: std::fs::File,
    ) -> (
        asyn_rs::runtime::port::PortRuntimeHandle,
        tokio::task::JoinHandle<()>,
        mpsc::UnboundedSender<PublishRequest>,
    ) {
        use asyn_rs::runtime::config::RuntimeConfig;
        use asyn_rs::services::PortServices;
        use asyn_rs::trace::TraceFile;

        let config = MqttConfig {
            broker_host: "127.0.0.1".into(),
            broker_port: addr.port(),
            client_id: format!("{port}-test"),
            keep_alive_secs: 3600,
            ..MqttConfig::default()
        };
        let (publish_tx, publish_rx) = mpsc::unbounded_channel();
        let connected = Arc::new(AtomicBool::new(false));
        let mut driver = crate::driver::MqttDriver::new(
            port,
            &config,
            Vec::new(),
            publish_tx.clone(),
            connected.clone(),
        );
        let topic_map = driver.topic_map();
        let connected_param = driver.connected_param;
        let shutdown = driver.shutdown_signal();
        let teardown_done = driver.teardown_ack();
        let services = PortServices::new();
        let trace = services.trace().clone();
        let (runtime, _jh) = asyn_rs::runtime::port::create_port_runtime_unclaimed(
            driver,
            RuntimeConfig {
                services,
                ..RuntimeConfig::default()
            },
        )
        .unwrap();
        trace.set_trace_file(
            Some(port),
            TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        trace.set_trace_mask(Some(port), TraceMask::ERROR | TraceMask::IO_DRIVER);

        let port_handle = runtime.port_handle().clone();
        let start = Arc::new(Notify::new());
        let loop_start = start.clone();
        let ev = tokio::spawn(async move {
            mqtt_event_loop(
                config,
                topic_map,
                port_handle,
                publish_rx,
                connected_param,
                connected,
                Lifecycle {
                    start: loop_start,
                    shutdown,
                    done: teardown_done,
                },
            )
            .await;
        });
        start.notify_one();
        (runtime, ev, publish_tx)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_edges_print_c_s_trace_lines() {
        use tokio::io::AsyncWriteExt;

        const CONNACK: [u8; 5] = [0x20, 0x03, 0x00, 0x00, 0x00];
        // No broker at boot.
        let port = BrokerPort::new();
        let addr = port.addr;

        let path = std::env::temp_dir().join(format!("mqedge-{}.log", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        async fn wait_for(path: &std::path::Path, needle: &str) {
            wait_for_n(path, needle, 1).await;
        }
        async fn wait_for_n(path: &std::path::Path, needle: &str, n: usize) {
            for _ in 0..100 {
                if std::fs::read_to_string(path)
                    .unwrap()
                    .matches(needle)
                    .count()
                    >= n
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!(
                "no '{needle}' in:\n{}",
                std::fs::read_to_string(path).unwrap()
            );
        }
        let (runtime, ev, _) = spawn_traced_loop("MQEDGE", addr, file);

        // The first failed attempt is reported, the retries are not.
        wait_for(&path, "onFailCb: MQTT Operation error: 'Error: ").await;
        // Past the retry on the 3 s tick.
        tokio::time::sleep(Duration::from_millis(3500)).await;

        // First session, then the broker goes away entirely.
        let listener = port.listen();
        let mut sock = accept_connect(&listener).await;
        sock.write_all(&CONNACK).await.unwrap();
        wait_for(&path, CONNACK_ARM_DONE).await;
        // `onConnectCb`'s lines, in C's order (drvMqtt.cpp:202-206).
        let connect_cb = |text: &str| -> Vec<String> {
            text.lines()
                .filter_map(|l| l.split_once("MqttDriver::onConnectCb: "))
                .map(|(_, m)| m.to_owned())
                .collect()
        };
        let booted = std::fs::read_to_string(&path).unwrap();
        assert_eq!(booted.matches("Operation error").count(), 1, "{booted}");
        assert_eq!(
            connect_cb(&booted),
            ["Reconnected.", "Connected to broker"],
            "{booted}"
        );
        drop(listener);
        drop(sock);
        wait_for(&path, "onDisconnectCb: Connection lost. Reconnecting...").await;
        // The retry on the next 3 s tick is refused.
        tokio::time::sleep(Duration::from_millis(3500)).await;

        // The broker is back.
        let listener = port.listen();
        let mut sock = accept_connect(&listener).await;
        sock.write_all(&CONNACK).await.unwrap();
        wait_for_n(&path, CONNACK_ARM_DONE, 2).await;

        let text = std::fs::read_to_string(&path).unwrap();
        let n = |needle: &str| text.matches(needle).count();
        assert_eq!(
            connect_cb(&text),
            [
                "Reconnected.",
                "Connected to broker",
                "Reconnected.",
                "Connected to broker"
            ],
            "{text}"
        );
        assert_eq!(n("Connection lost"), 1, "{text}");
        assert_eq!(n("Operation error"), 1, "{text}");
        // C names no cause (drvMqtt.cpp:219-220).
        assert!(
            text.lines().any(
                |l| l.ends_with("MqttDriver::onDisconnectCb: Connection lost. Reconnecting...")
            ),
            "{text}"
        );
        // paho's error text ends in '\n' (mqttClient.cpp:167), so C's closing
        // quote opens the next line.
        let fail = text
            .lines()
            .position(|l| l.contains("onFailCb: MQTT Operation error: 'Error: "))
            .unwrap();
        assert!(
            text.lines().nth(fail).unwrap().ends_with(
                "MqttDriver::onFailCb: MQTT Operation error: 'Error: TCP/TLS connect failure"
            ),
            "{text}"
        );
        assert_eq!(text.lines().nth(fail + 1), Some("'"), "{text}");

        runtime.shutdown();
        assert_eq!(read_packet_type(&mut sock).await, Some(14), "DISCONNECT");
        tokio::time::timeout(Duration::from_secs(10), ev)
            .await
            .expect("event loop did not exit")
            .unwrap();
        let _ = std::fs::remove_file(&path);
    }

    /// Holds the event loop between the ConnAck arm's two prints: the trace
    /// sink is a pipe filled so that `Reconnected.` takes its last byte and
    /// `Connected to broker` blocks. A reader there has seen the first line,
    /// so only the arm's last one may end a wait.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_between_the_connack_prints_has_not_seen_the_arm() {
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        use tokio::io::AsyncWriteExt;

        // Blocking reads, off the runtime: `n` bytes, or up to `needle`.
        async fn read_exact(r: std::io::PipeReader, n: usize) -> (std::io::PipeReader, String) {
            tokio::task::spawn_blocking(move || {
                let mut r = r;
                let mut buf = vec![0; n];
                r.read_exact(&mut buf).unwrap();
                (r, String::from_utf8(buf).unwrap())
            })
            .await
            .unwrap()
        }
        async fn read_through(
            r: std::io::PipeReader,
            needle: &'static str,
        ) -> (std::io::PipeReader, String) {
            tokio::task::spawn_blocking(move || {
                let mut r = r;
                let mut text = Vec::new();
                while !String::from_utf8_lossy(&text).contains(needle)
                    || text.last() != Some(&b'\n')
                {
                    let mut buf = [0; 512];
                    let n = r.read(&mut buf).unwrap();
                    assert_ne!(n, 0, "the sink closed");
                    text.extend_from_slice(&buf[..n]);
                }
                (r, String::from_utf8(text).unwrap())
            })
            .await
            .unwrap()
        }
        let unread = |r: &std::io::PipeReader| {
            let mut n: libc::c_int = 0;
            assert_eq!(
                unsafe { libc::ioctl(r.as_raw_fd(), libc::FIONREAD, &mut n) },
                0
            );
            n
        };

        const CONNACK: [u8; 5] = [0x20, 0x03, 0x00, 0x00, 0x00];
        let port = BrokerPort::new();
        let addr = port.addr;
        let (reader, writer) = std::io::pipe().unwrap();
        let mut filler = writer.try_clone().unwrap();
        let (runtime, ev, _) =
            spawn_traced_loop("MQHOLD", addr, std::os::fd::OwnedFd::from(writer).into());

        // The refused boot connect spends the token, so the ConnAck after the
        // retry prints both lines.
        let (reader, boot) = read_through(reader, "Operation error").await;
        let listener = port.listen();
        let mut sock = accept_connect(&listener).await;
        assert_eq!(unread(&reader), 0, "a line after {boot:?}");

        // Every line carries a prefix of the same width.
        const FIRST: &str = "MqttDriver::onConnectCb: Reconnected.\n";
        let first = boot.find("MqttDriver::").unwrap() + FIRST.len();
        let capacity =
            usize::try_from(unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETPIPE_SZ) })
                .unwrap();
        filler.write_all(&vec![b'.'; capacity - first]).unwrap();
        sock.write_all(&CONNACK).await.unwrap();

        // Everything the pipe held: the event loop is between its prints.
        let (reader, held) = read_exact(reader, capacity).await;
        let held = &held[capacity - first..];
        assert!(held.ends_with(FIRST), "{held:?}");
        assert!(!held.contains(CONNACK_ARM_DONE), "{held:?}");

        let (reader, rest) = read_through(reader, CONNACK_ARM_DONE).await;
        assert!(rest.contains(CONNACK_ARM_DONE), "{rest:?}");
        drop(reader);

        runtime.shutdown();
        assert_eq!(read_packet_type(&mut sock).await, Some(14), "DISCONNECT");
        tokio::time::timeout(Duration::from_secs(10), ev)
            .await
            .expect("event loop did not exit")
            .unwrap();
    }

    /// One failure per site that hands paho's connect token its message.
    #[tokio::test]
    async fn a_boot_connect_failure_carries_paho_s_message() {
        let tcp = |f: fn(io::Error) -> TcpFailure| {
            let e = io::Error::from(io::ErrorKind::ConnectionRefused);
            ConnectionError::Io(io::Error::new(e.kind(), f(e)))
        };
        let elapsed = tokio::time::timeout(Duration::ZERO, std::future::pending::<()>())
            .await
            .unwrap_err();
        let disconnect = rumqttc::Disconnect {
            reason_code: rumqttc::DisconnectReasonCode::NormalDisconnection,
            properties: None,
        };
        let cases = [
            (tcp(TcpFailure::Dispatch), ""),
            (tcp(TcpFailure::Completion), "TCP/TLS connect failure"),
            (
                ConnectionError::Io(io::ErrorKind::ConnectionReset.into()),
                "TCP connect completion failure",
            ),
            (
                ConnectionError::MqttState(rumqttc::StateError::ConnectionAborted),
                "TCP connect completion failure",
            ),
            (ConnectionError::Timeout(elapsed), "TCP connect timeout"),
            (
                ConnectionError::ConnectionRefused(rumqttc::ConnectReturnCode::NotAuthorized),
                "CONNACK return code",
            ),
            (
                ConnectionError::NotConnAck(Box::new(Packet::Disconnect(disconnect))),
                "Received disconnect",
            ),
            (
                ConnectionError::NotConnAck(Box::new(Packet::PingResp)),
                "TCP connect completion failure",
            ),
        ];
        for (e, text) in cases {
            let failure = ConnectFailure::of(&e);
            assert_eq!(failure.text(), text, "{e:?}");
            assert_eq!(
                failure == ConnectFailure::Dispatch,
                text.is_empty(),
                "{e:?}"
            );
        }
    }

    /// A connect that fails while paho dispatches it keeps the token's
    /// `onFailure` for the retry (MQTTAsyncUtils.c:1574-1626), so every attempt
    /// is reported: Linux refuses a `connect()` to the broadcast address with
    /// ENETUNREACH before anything is in flight.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_dispatch_failure_of_the_boot_connect_is_reported() {
        use asyn_rs::services::PortServices;
        use asyn_rs::trace::TraceFile;

        let config = MqttConfig {
            broker_host: "255.255.255.255".into(),
            broker_port: 1883,
            client_id: "dispatch-test".into(),
            keep_alive_secs: 3600,
            ..MqttConfig::default()
        };
        let (publish_tx, publish_rx) = mpsc::unbounded_channel();
        let connected = Arc::new(AtomicBool::new(false));
        let mut driver = crate::driver::MqttDriver::new(
            "MQDISPATCH",
            &config,
            Vec::new(),
            publish_tx,
            connected.clone(),
        );
        let topic_map = driver.topic_map();
        let connected_param = driver.connected_param;
        let shutdown = driver.shutdown_signal();
        let teardown_done = driver.teardown_ack();
        let services = PortServices::new();
        let trace = services.trace().clone();
        let (runtime, _actor) = asyn_rs::runtime::port::create_port_runtime_unclaimed(
            driver,
            asyn_rs::runtime::config::RuntimeConfig {
                services,
                ..Default::default()
            },
        )
        .unwrap();
        let path = std::env::temp_dir().join(format!("mqdispatch-{}.log", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        trace.set_trace_file(
            Some("MQDISPATCH"),
            TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        trace.set_trace_mask(Some("MQDISPATCH"), TraceMask::ERROR | TraceMask::IO_DRIVER);
        let port_handle = runtime.port_handle().clone();
        let start = Arc::new(Notify::new());
        let loop_start = start.clone();
        let ev = tokio::spawn(async move {
            mqtt_event_loop(
                config,
                topic_map,
                port_handle,
                publish_rx,
                connected_param,
                connected,
                Lifecycle {
                    start: loop_start,
                    shutdown,
                    done: teardown_done,
                },
            )
            .await;
        });
        start.notify_one();

        let log = || std::fs::read_to_string(&path).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while log().matches("onFailCb: MQTT Operation error").count() < 2 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("fewer than two onFailCb lines in:\n{}", log()));

        runtime.shutdown();
        tokio::time::timeout(Duration::from_secs(10), ev)
            .await
            .expect("event loop did not exit")
            .unwrap();
        let _ = std::fs::remove_file(&path);
    }

    /// The other side of the `start` boundary: an IOC told to exit *before*
    /// `AfterScanInit` ever fired — a boot that failed after
    /// `mqttDriverConfigure` and before `iocInit` finished, which
    /// `IocApplication::run` tears down exactly like a normal exit.
    ///
    /// There is no session, so there is no DISCONNECT to send: C's
    /// `disconnect()` checks `client_.is_connected()` first
    /// (mqttClient.cpp:52). But the driver's `Drop` is waiting all the same, so
    /// the loop must still release it — by returning, not by letting it wait
    /// out `TEARDOWN_TIMEOUT`. The bounds below are under that timeout, so a
    /// regression to "nobody is listening on `shutdown` yet" fails here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_shutdown_before_the_start_hook_still_releases_the_driver() {
        let config = MqttConfig {
            broker_host: "127.0.0.1".into(),
            // Nothing listens here, and nothing has to: `start` is never fired,
            // so the loop never reaches its first `poll()` and never connects.
            broker_port: 1,
            client_id: "mq4-preinit".into(),
            ..MqttConfig::default()
        };

        let (publish_tx, publish_rx) = mpsc::unbounded_channel();
        let connected = Arc::new(AtomicBool::new(false));
        let mut driver = crate::driver::MqttDriver::new(
            "MQ4PREINIT",
            &config,
            Vec::new(),
            publish_tx,
            connected.clone(),
        );
        let topic_map = driver.topic_map();
        let connected_param = driver.connected_param;
        let shutdown = driver.shutdown_signal();
        let teardown_done = driver.teardown_ack();
        let (runtime, actor) = asyn_rs::runtime::port::create_port_runtime_unclaimed(
            driver,
            asyn_rs::runtime::config::RuntimeConfig::default(),
        )
        .unwrap();
        let port_handle = runtime.port_handle().clone();

        let ev = tokio::spawn(async move {
            mqtt_event_loop(
                config,
                topic_map,
                port_handle,
                publish_rx,
                connected_param,
                connected,
                Lifecycle {
                    start: Arc::new(Notify::new()),
                    shutdown,
                    done: teardown_done,
                },
            )
            .await;
        });

        runtime.shutdown();

        tokio::time::timeout(Duration::from_secs(2), ev)
            .await
            .expect("the event loop must return when shutdown lands before AfterScanInit")
            .unwrap();
        // And the actor thread — the one whose driver `Drop` is being released —
        // must be finished, not still counting down its own timeout.
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || actor.join()),
        )
        .await
        .expect("the port actor must stop once its driver's Drop is released")
        .unwrap()
        .unwrap();
    }

    /// A shutdown that lands while the loop waits out a retry interval (here
    /// the third, at least 3 s) releases the driver at once, not after the
    /// wait.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_shutdown_during_a_retry_wait_releases_the_driver() {
        // Every attempt is accepted and dropped before a CONNACK.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = MqttConfig {
            broker_host: "127.0.0.1".into(),
            broker_port: listener.local_addr().unwrap().port(),
            client_id: "mq-backoff-exit".into(),
            ..MqttConfig::default()
        };
        let (publish_tx, publish_rx) = mpsc::unbounded_channel();
        let connected = Arc::new(AtomicBool::new(false));
        let mut driver = crate::driver::MqttDriver::new(
            "MQBACKOFF",
            &config,
            Vec::new(),
            publish_tx,
            connected.clone(),
        );
        let topic_map = driver.topic_map();
        let connected_param = driver.connected_param;
        let shutdown = driver.shutdown_signal();
        let teardown_done = driver.teardown_ack();
        let (runtime, _actor) = asyn_rs::runtime::port::create_port_runtime_unclaimed(
            driver,
            asyn_rs::runtime::config::RuntimeConfig::default(),
        )
        .unwrap();
        let port_handle = runtime.port_handle().clone();
        let start = Arc::new(Notify::new());
        let loop_start = start.clone();
        let ev = tokio::spawn(async move {
            mqtt_event_loop(
                config,
                topic_map,
                port_handle,
                publish_rx,
                connected_param,
                connected,
                Lifecycle {
                    start: loop_start,
                    shutdown,
                    done: teardown_done,
                },
            )
            .await;
        });
        start.notify_one();

        tokio::time::timeout(Duration::from_secs(10), async {
            for _ in 0..3 {
                drop(listener.accept().await.unwrap());
            }
        })
        .await
        .expect("three failed attempts");
        // Let the third failure reach the loop, which then waits.
        tokio::time::sleep(Duration::from_millis(200)).await;

        runtime.shutdown();
        tokio::time::timeout(Duration::from_secs(2), ev)
            .await
            .expect("the event loop must not wait out the retry interval")
            .unwrap();
    }

    /// A broker that refuses a subscription gets C's `onFailCb` line and the
    /// session stays up: no reconnect, and a later PUBLISH on the same
    /// connection still lands in the param (MQTTAsyncUtils.c:2234-2248).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_subscribe_is_reported_and_the_session_stays_up() {
        use asyn_rs::interrupt::InterruptFilter;
        use asyn_rs::port::PortDriver;
        use asyn_rs::services::PortServices;
        use asyn_rs::trace::TraceFile;
        use tokio::io::AsyncWriteExt;

        const CONNACK: [u8; 5] = [0x20, 0x03, 0x00, 0x00, 0x00];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = MqttConfig {
            broker_host: "127.0.0.1".into(),
            broker_port: listener.local_addr().unwrap().port(),
            client_id: "suback-refused".into(),
            keep_alive_secs: 3600,
            ..MqttConfig::default()
        };
        let (publish_tx, publish_rx) = mpsc::unbounded_channel();
        let connected = Arc::new(AtomicBool::new(false));
        let mut driver = crate::driver::MqttDriver::new(
            "MQSUBACK",
            &config,
            Vec::new(),
            publish_tx,
            connected.clone(),
        );
        let reason = driver
            .drv_user_create(&asyn_rs::port::DrvUserRequest::new("FLAT:INT dev/t", 0))
            .unwrap()
            .reason;
        let topic_map = driver.topic_map();
        let connected_param = driver.connected_param;
        let shutdown = driver.shutdown_signal();
        let teardown_done = driver.teardown_ack();
        let services = PortServices::new();
        let trace = services.trace().clone();
        let (runtime, _actor) = asyn_rs::runtime::port::create_port_runtime_unclaimed(
            driver,
            asyn_rs::runtime::config::RuntimeConfig {
                services,
                ..Default::default()
            },
        )
        .unwrap();
        let path = std::env::temp_dir().join(format!("mqsuback-{}.log", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        trace.set_trace_file(
            Some("MQSUBACK"),
            TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        trace.set_trace_mask(Some("MQSUBACK"), TraceMask::ERROR | TraceMask::IO_DRIVER);
        // An `I/O Intr` binding on the topic, so the ConnAck subscribes it.
        let _binding = runtime
            .port_handle()
            .interrupts()
            .register_sync_callback(
                InterruptFilter {
                    reason: Some(reason),
                    addr: Some(0),
                    ..Default::default()
                },
                |_| {},
            )
            .unwrap();

        let port_handle = runtime.port_handle().clone();
        let start = Arc::new(Notify::new());
        let loop_start = start.clone();
        let ev = tokio::spawn(async move {
            mqtt_event_loop(
                config,
                topic_map,
                port_handle,
                publish_rx,
                connected_param,
                connected,
                Lifecycle {
                    start: loop_start,
                    shutdown,
                    done: teardown_done,
                },
            )
            .await;
        });
        start.notify_one();

        let mut sock = accept_connect(&listener).await;
        sock.write_all(&CONNACK).await.unwrap();
        let (kind, body) = read_packet(&mut sock).await.unwrap();
        assert_eq!(kind, 8, "SUBSCRIBE");
        // SUBACK: packet id, no properties, reason 0x87 Not authorized.
        sock.write_all(&[0x90, 0x04, body[0], body[1], 0x00, 0x87])
            .await
            .unwrap();

        let log = || std::fs::read_to_string(&path).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !log().contains("MqttDriver::onFailCb: MQTT Operation error: 'Error: \n'") {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no onFailCb line in:\n{}", log()));

        // The same connection still delivers: PUBLISH dev/t "5", QoS 0.
        sock.write_all(&[
            0x30, 0x09, 0x00, 0x05, b'd', b'e', b'v', b'/', b't', 0x00, b'5',
        ])
        .await
        .unwrap();
        let reader = runtime.port_handle().clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            while reader.read_int32(reason, 0).await.ok() != Some(5) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the PUBLISH after the refusal never landed:\n{}", log()));
        assert!(!log().contains("Connection lost"), "{}", log());
        // paho calls only `onFailure5` for the refused filter.
        assert!(!log().contains("Subscribed to topic"), "{}", log());

        runtime.shutdown();
        assert_eq!(read_packet_type(&mut sock).await, Some(14), "DISCONNECT");
        tokio::time::timeout(Duration::from_secs(10), ev)
            .await
            .expect("event loop did not exit")
            .unwrap();
        let _ = std::fs::remove_file(&path);
    }

    /// paho queues every `onConnectCb` subscribe however many there are: 300
    /// `I/O Intr` topics give 300 SUBSCRIBEs and 300 `onSubscribeCb` lines.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_bound_topic_is_subscribed_past_the_request_channel() {
        use asyn_rs::interrupt::InterruptFilter;
        use asyn_rs::port::PortDriver;
        use asyn_rs::services::PortServices;
        use asyn_rs::trace::TraceFile;
        use tokio::io::AsyncWriteExt;

        const CONNACK: [u8; 5] = [0x20, 0x03, 0x00, 0x00, 0x00];
        const TOPICS: usize = 300;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = MqttConfig {
            broker_host: "127.0.0.1".into(),
            broker_port: listener.local_addr().unwrap().port(),
            client_id: "many-topics".into(),
            keep_alive_secs: 3600,
            ..MqttConfig::default()
        };
        let (publish_tx, publish_rx) = mpsc::unbounded_channel();
        let connected = Arc::new(AtomicBool::new(false));
        let mut driver = crate::driver::MqttDriver::new(
            "MQMANY",
            &config,
            Vec::new(),
            publish_tx,
            connected.clone(),
        );
        let reasons: Vec<usize> = (0..TOPICS)
            .map(|i| {
                driver
                    .drv_user_create(&asyn_rs::port::DrvUserRequest::new(
                        format!("FLAT:INT dev/t{i}"),
                        0,
                    ))
                    .unwrap()
                    .reason
            })
            .collect();
        let topic_map = driver.topic_map();
        let connected_param = driver.connected_param;
        let shutdown = driver.shutdown_signal();
        let teardown_done = driver.teardown_ack();
        let services = PortServices::new();
        let trace = services.trace().clone();
        let (runtime, _actor) = asyn_rs::runtime::port::create_port_runtime_unclaimed(
            driver,
            asyn_rs::runtime::config::RuntimeConfig {
                services,
                ..Default::default()
            },
        )
        .unwrap();
        let path = std::env::temp_dir().join(format!("mqmany-{}.log", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        trace.set_trace_file(
            Some("MQMANY"),
            TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        trace.set_trace_mask(Some("MQMANY"), TraceMask::ERROR | TraceMask::IO_DRIVER);
        let _bindings: Vec<_> = reasons
            .iter()
            .map(|&reason| {
                runtime
                    .port_handle()
                    .interrupts()
                    .register_sync_callback(
                        InterruptFilter {
                            reason: Some(reason),
                            addr: Some(0),
                            ..Default::default()
                        },
                        |_| {},
                    )
                    .unwrap()
            })
            .collect();

        let port_handle = runtime.port_handle().clone();
        let start = Arc::new(Notify::new());
        let loop_start = start.clone();
        let ev = tokio::spawn(async move {
            mqtt_event_loop(
                config,
                topic_map,
                port_handle,
                publish_rx,
                connected_param,
                connected,
                Lifecycle {
                    start: loop_start,
                    shutdown,
                    done: teardown_done,
                },
            )
            .await;
        });
        start.notify_one();

        let mut sock = accept_connect(&listener).await;
        sock.write_all(&CONNACK).await.unwrap();
        let log = || std::fs::read_to_string(&path).unwrap();
        let mut topics = HashSet::new();
        while topics.len() < TOPICS {
            let (kind, body) = tokio::time::timeout(Duration::from_secs(5), read_packet(&mut sock))
                .await
                .unwrap_or_else(|_| panic!("{} of {TOPICS} subscribed:\n{}", topics.len(), log()))
                .unwrap();
            assert_eq!(kind, 8, "SUBSCRIBE");
            // Packet id, an empty property list, then the one filter.
            let len = usize::from(u16::from_be_bytes([body[3], body[4]]));
            topics.insert(String::from_utf8(body[5..5 + len].to_vec()).unwrap());
            sock.write_all(&[0x90, 0x04, body[0], body[1], 0x00, 0x00])
                .await
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while log().matches("onSubscribeCb: Subscribed to topic").count() < TOPICS {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("fewer than {TOPICS} onSubscribeCb lines in:\n{}", log()));
        assert!(!log().contains("onFailCb"), "{}", log());

        runtime.shutdown();
        assert_eq!(read_packet_type(&mut sock).await, Some(14), "DISCONNECT");
        tokio::time::timeout(Duration::from_secs(10), ev)
            .await
            .expect("event loop did not exit")
            .unwrap();
        let _ = std::fs::remove_file(&path);
    }

    /// paho fails every SUBSCRIBE and QoS 1/2 PUBLISH still owed an ack when
    /// its session closes, on a loss before `onDisconnectCb` and on the exit
    /// disconnect (MQTTAsyncUtils.c:979, :1902-1938, :2491-2498); one acked
    /// before the loss keeps its `onPublishCb` line, and the new session
    /// fails nothing twice.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn requests_owed_an_ack_fail_with_their_session() {
        use asyn_rs::interrupt::InterruptFilter;
        use asyn_rs::port::PortDriver;
        use asyn_rs::services::PortServices;
        use asyn_rs::trace::TraceFile;
        use tokio::io::AsyncWriteExt;

        const CONNACK: [u8; 5] = [0x20, 0x03, 0x00, 0x00, 0x00];
        const FAIL: &str = "MqttDriver::onFailCb: MQTT Operation error: 'Error: \n'";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = MqttConfig {
            broker_host: "127.0.0.1".into(),
            broker_port: addr.port(),
            client_id: "owed-acks".into(),
            keep_alive_secs: 3600,
            ..MqttConfig::default()
        };
        let (publish_tx, publish_rx) = mpsc::unbounded_channel();
        let connected = Arc::new(AtomicBool::new(false));
        let mut driver = crate::driver::MqttDriver::new(
            "MQOWED",
            &config,
            Vec::new(),
            publish_tx.clone(),
            connected.clone(),
        );
        let reason = driver
            .drv_user_create(&asyn_rs::port::DrvUserRequest::new("FLAT:INT dev/t", 0))
            .unwrap()
            .reason;
        let topic_map = driver.topic_map();
        let connected_param = driver.connected_param;
        let shutdown = driver.shutdown_signal();
        let teardown_done = driver.teardown_ack();
        let services = PortServices::new();
        let trace = services.trace().clone();
        let (runtime, _actor) = asyn_rs::runtime::port::create_port_runtime_unclaimed(
            driver,
            asyn_rs::runtime::config::RuntimeConfig {
                services,
                ..Default::default()
            },
        )
        .unwrap();
        let path = std::env::temp_dir().join(format!("mqowed-{}.log", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        trace.set_trace_file(
            Some("MQOWED"),
            TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        trace.set_trace_mask(Some("MQOWED"), TraceMask::ERROR | TraceMask::IO_DRIVER);
        let _binding = runtime
            .port_handle()
            .interrupts()
            .register_sync_callback(
                InterruptFilter {
                    reason: Some(reason),
                    addr: Some(0),
                    ..Default::default()
                },
                |_| {},
            )
            .unwrap();

        let port_handle = runtime.port_handle().clone();
        let start = Arc::new(Notify::new());
        let loop_start = start.clone();
        let ev = tokio::spawn(async move {
            mqtt_event_loop(
                config,
                topic_map,
                port_handle,
                publish_rx,
                connected_param,
                connected,
                Lifecycle {
                    start: loop_start,
                    shutdown,
                    done: teardown_done,
                },
            )
            .await;
        });
        start.notify_one();
        let publish = |topic: &str| {
            publish_tx
                .send(PublishRequest {
                    topic: topic.into(),
                    payload: b"1".to_vec(),
                    qos: crate::config::QoS::AtLeastOnce,
                    retained: false,
                })
                .unwrap();
        };
        let log = || std::fs::read_to_string(&path).unwrap();
        let wait_for = |needle: &'static str, n: usize| async move {
            tokio::time::timeout(Duration::from_secs(5), async {
                while log().matches(needle).count() < n {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("fewer than {n} '{needle}' in:\n{}", log()));
        };

        // First session: the SUBSCRIBE and one PUBLISH stay unacked, the
        // PUBLISH after it is acked.
        let mut sock = accept_connect(&listener).await;
        sock.write_all(&CONNACK).await.unwrap();
        assert_eq!(read_packet_type(&mut sock).await, Some(8), "SUBSCRIBE");
        publish("dev/owed");
        assert_eq!(read_packet_type(&mut sock).await, Some(3), "PUBLISH");
        publish("dev/acked");
        let (kind, body) = read_packet(&mut sock).await.unwrap();
        assert_eq!(kind, 3, "PUBLISH");
        let at = 2 + usize::from(u16::from_be_bytes([body[0], body[1]]));
        sock.write_all(&[0x40, 0x03, body[at], body[at + 1], 0x00])
            .await
            .unwrap();
        wait_for("Published to topic 'dev/acked'", 1).await;
        drop(sock);
        wait_for("onDisconnectCb: Connection lost", 1).await;
        let lost = log();
        assert_eq!(lost.matches(FAIL).count(), 2, "{lost}");
        assert!(
            lost.rfind(FAIL) < lost.find("Connection lost"),
            "the failures come before the loss:\n{lost}"
        );

        // The broker is back: the new session fails nothing again. A stray
        // connection queued ahead of the reconnect is not the client.
        drop(tokio::net::TcpStream::connect(addr).await.unwrap());
        let mut sock = accept_connect(&listener).await;
        sock.write_all(&CONNACK).await.unwrap();
        let (kind, body) = read_packet(&mut sock).await.unwrap();
        assert_eq!(kind, 8, "SUBSCRIBE");
        sock.write_all(&[0x90, 0x04, body[0], body[1], 0x00, 0x00])
            .await
            .unwrap();
        wait_for("Subscribed to topic 'dev/t'", 1).await;
        assert_eq!(log().matches(FAIL).count(), 2, "{}", log());

        // A PUBLISH in flight at the exit fails with the exit disconnect.
        publish("dev/exit");
        assert_eq!(read_packet_type(&mut sock).await, Some(3), "PUBLISH");
        runtime.shutdown();
        assert_eq!(read_packet_type(&mut sock).await, Some(14), "DISCONNECT");
        tokio::time::timeout(Duration::from_secs(10), ev)
            .await
            .expect("event loop did not exit")
            .unwrap();
        assert_eq!(log().matches(FAIL).count(), 3, "{}", log());
        assert!(
            !log().contains("Published to topic 'dev/owed'"),
            "{}",
            log()
        );
        assert!(
            !log().contains("Published to topic 'dev/exit'"),
            "{}",
            log()
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A refused PUBREC ends that QoS 2 flow and frees its slot, as paho's
    /// `onFailure5` path frees the message (MQTTAsyncUtils.c:3199-3209): with
    /// Receive Maximum 2, five refused publishes all reach the broker.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refused_pubrecs_past_receive_maximum_do_not_stall_publishing() {
        use tokio::io::AsyncWriteExt;

        // CONNACK with one property: Receive Maximum (0x21) = 2.
        const CONNACK: [u8; 8] = [0x20, 0x06, 0x00, 0x00, 0x03, 0x21, 0x00, 0x02];
        const SENT: usize = 5;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = MqttConfig {
            broker_host: "127.0.0.1".into(),
            broker_port: listener.local_addr().unwrap().port(),
            client_id: "pubrec-refused".into(),
            keep_alive_secs: 3600,
            ..MqttConfig::default()
        };
        let (publish_tx, publish_rx) = mpsc::unbounded_channel();
        let connected = Arc::new(AtomicBool::new(false));
        let mut driver = crate::driver::MqttDriver::new(
            "MQPUBREC",
            &config,
            Vec::new(),
            publish_tx.clone(),
            connected.clone(),
        );
        let topic_map = driver.topic_map();
        let connected_param = driver.connected_param;
        let shutdown = driver.shutdown_signal();
        let teardown_done = driver.teardown_ack();
        let (runtime, _actor) = asyn_rs::runtime::port::create_port_runtime_unclaimed(
            driver,
            asyn_rs::runtime::config::RuntimeConfig::default(),
        )
        .unwrap();
        let port_handle = runtime.port_handle().clone();
        let start = Arc::new(Notify::new());
        let loop_start = start.clone();
        let ev = tokio::spawn(async move {
            mqtt_event_loop(
                config,
                topic_map,
                port_handle,
                publish_rx,
                connected_param,
                connected,
                Lifecycle {
                    start: loop_start,
                    shutdown,
                    done: teardown_done,
                },
            )
            .await;
        });
        start.notify_one();

        let mut sock = accept_connect(&listener).await;
        sock.write_all(&CONNACK).await.unwrap();
        for i in 0..SENT {
            publish_tx
                .send(PublishRequest {
                    topic: "dev/q2".into(),
                    payload: i.to_string().into_bytes(),
                    qos: crate::config::QoS::ExactlyOnce,
                    retained: false,
                })
                .unwrap();
        }
        let mut seen = 0;
        while seen < SENT {
            let (kind, body) = tokio::time::timeout(Duration::from_secs(5), read_packet(&mut sock))
                .await
                .unwrap_or_else(|_| panic!("publishing stalled after {seen} of {SENT}"))
                .unwrap();
            assert_eq!(kind, 3, "PUBLISH");
            seen += 1;
            // Packet id follows the topic; PUBREC 0x80 Unspecified error.
            let at = 2 + usize::from(u16::from_be_bytes([body[0], body[1]]));
            sock.write_all(&[0x50, 0x03, body[at], body[at + 1], 0x80])
                .await
                .unwrap();
        }

        runtime.shutdown();
        assert_eq!(read_packet_type(&mut sock).await, Some(14), "DISCONNECT");
        tokio::time::timeout(Duration::from_secs(10), ev)
            .await
            .expect("event loop did not exit")
            .unwrap();
    }

    /// With Receive Maximum 1 and `a` unacked, `b` and `c` wait unwritten.
    /// The loss fails `a` alone; paho sends `b` and `c` in the next session
    /// (MQTTAsyncUtils.c:1248-1262, :1902-1938).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_loss_keeps_the_publishes_not_yet_written() {
        use tokio::io::AsyncWriteExt;

        // CONNACK with one property: Receive Maximum (0x21) = 1.
        const CONNACK: [u8; 8] = [0x20, 0x06, 0x00, 0x00, 0x03, 0x21, 0x00, 0x01];
        const FAIL: &str = "MqttDriver::onFailCb: MQTT Operation error: 'Error: \n'";
        let port = BrokerPort::new();
        let path = std::env::temp_dir().join(format!("mqkeep-{}.log", std::process::id()));
        let (runtime, ev, publish) =
            spawn_traced_loop("MQKEEP", port.addr, std::fs::File::create(&path).unwrap());
        let log = || std::fs::read_to_string(&path).unwrap();
        /// The topic of a PUBLISH, and its PUBACK.
        async fn read_publish(sock: &mut tokio::net::TcpStream) -> (String, [u8; 4]) {
            let (kind, body) = tokio::time::timeout(Duration::from_secs(10), read_packet(sock))
                .await
                .expect("no PUBLISH")
                .unwrap();
            assert_eq!(kind, 3, "PUBLISH");
            let len = usize::from(u16::from_be_bytes([body[0], body[1]]));
            let topic = String::from_utf8(body[2..2 + len].to_vec()).unwrap();
            (topic, [0x40, 0x02, body[2 + len], body[3 + len]])
        }

        let listener = port.listen();
        let mut sock = accept_connect(&listener).await;
        sock.write_all(&CONNACK).await.unwrap();
        for topic in ["dev/a", "dev/b", "dev/c"] {
            publish
                .send(PublishRequest {
                    topic: topic.into(),
                    payload: b"1".to_vec(),
                    qos: crate::config::QoS::AtLeastOnce,
                    retained: false,
                })
                .unwrap();
        }
        assert_eq!(read_publish(&mut sock).await.0, "dev/a");
        assert!(
            tokio::time::timeout(Duration::from_millis(300), read_packet(&mut sock))
                .await
                .is_err(),
            "a second PUBLISH inside Receive Maximum 1"
        );
        drop((listener, sock));

        let listener = port.listen();
        let mut sock = accept_connect(&listener).await;
        sock.write_all(&CONNACK).await.unwrap();
        for topic in ["dev/b", "dev/c"] {
            let (sent, puback) = read_publish(&mut sock).await;
            assert_eq!(sent, topic);
            sock.write_all(&puback).await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while !log().contains("Published to topic 'dev/c'") {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("dev/c not acked in:\n{}", log()));

        let text = log();
        assert_eq!(text.matches(FAIL).count(), 1, "{text}");
        assert!(
            text.find(FAIL).unwrap() < text.find("Connection lost").unwrap(),
            "{text}"
        );
        assert!(text.contains("Published to topic 'dev/b'"), "{text}");
        runtime.shutdown();
        assert_eq!(read_packet_type(&mut sock).await, Some(14), "DISCONNECT");
        tokio::time::timeout(Duration::from_secs(10), ev)
            .await
            .expect("event loop did not exit")
            .unwrap();
        let _ = std::fs::remove_file(&path);
    }

    /// C's exit disconnect does not wait for an unacknowledged QoS 1 flow:
    /// `disconnect()` carries `timeout` 0 (MQTTAsync.h:1484), and
    /// `MQTTAsync_checkDisconnect` closes the session at once
    /// (MQTTAsyncUtils.c:975-978).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_does_not_wait_for_an_unacked_publish() {
        use tokio::io::AsyncWriteExt;

        const CONNACK: [u8; 5] = [0x20, 0x03, 0x00, 0x00, 0x00];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = MqttConfig {
            broker_host: "127.0.0.1".into(),
            broker_port: listener.local_addr().unwrap().port(),
            client_id: "unacked-exit".into(),
            keep_alive_secs: 3600,
            ..MqttConfig::default()
        };
        let (publish_tx, publish_rx) = mpsc::unbounded_channel();
        let connected = Arc::new(AtomicBool::new(false));
        let mut driver = crate::driver::MqttDriver::new(
            "MQUNACK",
            &config,
            Vec::new(),
            publish_tx.clone(),
            connected.clone(),
        );
        let topic_map = driver.topic_map();
        let connected_param = driver.connected_param;
        let shutdown = driver.shutdown_signal();
        let teardown_done = driver.teardown_ack();
        let (runtime, _actor) = asyn_rs::runtime::port::create_port_runtime_unclaimed(
            driver,
            asyn_rs::runtime::config::RuntimeConfig::default(),
        )
        .unwrap();
        let port_handle = runtime.port_handle().clone();
        let start = Arc::new(Notify::new());
        let loop_start = start.clone();
        let ev = tokio::spawn(async move {
            mqtt_event_loop(
                config,
                topic_map,
                port_handle,
                publish_rx,
                connected_param,
                connected,
                Lifecycle {
                    start: loop_start,
                    shutdown,
                    done: teardown_done,
                },
            )
            .await;
        });
        start.notify_one();

        let mut sock = accept_connect(&listener).await;
        sock.write_all(&CONNACK).await.unwrap();
        publish_tx
            .send(PublishRequest {
                topic: "dev/q1".into(),
                payload: b"1".to_vec(),
                qos: crate::config::QoS::AtLeastOnce,
                retained: false,
            })
            .unwrap();
        assert_eq!(read_packet_type(&mut sock).await, Some(3), "PUBLISH");

        // No PUBACK: the flow stays in flight through the exit.
        runtime.shutdown();
        let seen = tokio::time::timeout(Duration::from_secs(2), read_packet_type(&mut sock))
            .await
            .expect("the DISCONNECT waited for the unacked PUBLISH");
        assert_eq!(seen, Some(14), "DISCONNECT");
        tokio::time::timeout(Duration::from_secs(10), ev)
            .await
            .expect("event loop did not exit")
            .unwrap();
    }

    #[test]
    fn subscribe_topics_skips_binding_without_topic() {
        // An interrupt binding on a reason that maps to no topic (e.g. the
        // internal _MQTT_CONNECTED param) contributes nothing to the subscribe
        // set rather than panicking or subscribing an empty topic.
        let topic_map: TopicMap = HashMap::new();
        let idx = reason_topic_index(&topic_map);
        assert!(subscribe_topics(&idx, &[(99, 0)]).is_empty());
    }

    /// C prints an `ASYN_TRACE_ERROR` line for every subscriber it skips
    /// (drvMqtt.cpp:268-270, :328-330); the port used to log these at debug,
    /// so a malformed or partial payload left no trace on the console.
    #[test]
    fn every_skipped_subscriber_is_reported_with_c_s_text() {
        let subscribers = vec![
            (
                1usize,
                TopicAddress::parse("JSON:FLOAT dev temperature").unwrap(),
            ),
            (2usize, TopicAddress::parse("JSON:INT dev battery").unwrap()),
            (
                3usize,
                TopicAddress::parse("JSON:FLOAT dev humidity").unwrap(),
            ),
        ];
        let bound: HashSet<usize> = [1usize, 2, 3].into_iter().collect();
        let mut lines = Vec::new();
        let batch = decode_for_subscribers(
            br#"{"temperature":"warm","battery":99.7}"#,
            &subscribers,
            &bound,
            "dev",
            &mut |args| lines.push(args.to_string()),
        );
        assert!(batch.is_empty());
        assert_eq!(lines.len(), 3, "{lines:#?}");
        // C: "%s::%s:%s: Unexpected value received for topic: '%s': %s)\n", the
        // cause before the topic and the whole payload last.
        let tail = concat!(
            r#": Unexpected value received for topic: 'dev': {"temperature":"warm","battery":99.7})"#,
            "\n"
        );
        for line in &lines[..2] {
            assert!(line.starts_with("MqttDriver::onMessageCb:"), "{line}");
            assert!(line.ends_with(tail), "{line}");
        }
        assert!(lines[2].starts_with(
            "MqttDriver::onMessageCb: Failed to parse JSON payload for topic 'dev', field 'humidity'"
        ));

        lines.clear();
        let batch =
            decode_for_subscribers(b"notjson", &subscribers[..1], &bound, "dev", &mut |args| {
                lines.push(args.to_string())
            });
        assert!(batch.is_empty());
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("Failed to parse JSON payload for topic 'dev', field 'temperature'")
        );

        // The payload prints as C's `c_str()`: cut at the NUL.
        lines.clear();
        let flat = vec![(4usize, TopicAddress::parse("FLAT:INT dev").unwrap())];
        let bound: HashSet<usize> = [4usize].into_iter().collect();
        let batch = decode_for_subscribers(b"x\xffy\0tail", &flat, &bound, "dev", &mut |args| {
            lines.push(args.to_string())
        });
        assert!(batch.is_empty());
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].ends_with(": Unexpected value received for topic: 'dev': x\u{FFFD}y)\n"),
            "{}",
            lines[0]
        );
    }

    /// paho's jitter window per base, at both ends of the draw: base 1 may
    /// wait 0 s, the cap reaches 72 s.
    #[test]
    fn jitter_matches_paho_random_jitter_windows() {
        for (base, lo, hi) in [
            (1, 0, 1),
            (2, 1, 2),
            (4, 3, 4),
            (8, 6, 9),
            (32, 26, 38),
            (60, 50, 72),
        ] {
            assert_eq!(jitter_secs(base, 0), lo, "base {base}");
            assert_eq!(jitter_secs(base, u64::from(hi - lo)), hi, "base {base}");
            assert_eq!(jitter_secs(base, u64::from(hi - lo + 1)), lo, "base {base}");
        }
    }

    /// The base after each transition: a failed boot starts at the minimum,
    /// a loss retries at once from base 2, failures double to the cap, and a
    /// ConnAck clears it.
    #[test]
    fn backoff_follows_paho_start_connect_retry() {
        let t0 = Instant::now();
        let mut b = Backoff::new(t0);
        let bases = |b: &mut Backoff, n| {
            (0..n)
                .map(|_| {
                    b.failed(t0);
                    b.base.unwrap()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(bases(&mut b, 8), [1, 2, 4, 8, 16, 32, 60, 60]);
        b.connected();
        assert_eq!(b.base, None);
        b.lost(t0);
        assert_eq!(bases(&mut b, 2), [4, 8]);
        // Base 16: an interval of 13-19 s, so the tick at 15 or 18 s, or the
        // one at 21 s for a 19 s draw.
        let wait = b.failed(t0) - t0;
        assert!([15, 18, 21].contains(&wait.as_secs()), "{wait:?}");
    }

    /// paho retries on its 3 s `checkTimeouts` grid: a loss at the next tick,
    /// a failure at the first tick strictly past its interval.
    #[test]
    fn retries_land_on_paho_s_three_second_tick() {
        let t0 = Instant::now();
        let mut b = Backoff::new(t0);
        let s = Duration::from_secs;
        assert_eq!(b.lost(t0 + Duration::from_millis(100)), t0 + s(3));
        assert_eq!(b.lost(t0 + s(3)), t0 + s(6));
        // Base 4 after the loss: an interval of 3 or 4 s.
        let at = b.failed(t0 + Duration::from_millis(3500)) - t0;
        assert!(at == s(9), "{at:?}");
        assert_eq!(b.tick_after(t0), t0 + s(3));
        assert_eq!(b.tick_after(t0 + Duration::from_millis(2999)), t0 + s(3));
    }

    /// Each PUBLISH prints from its own ack, whatever order the acks come in.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn each_publish_prints_from_its_own_ack() {
        use asyn_rs::services::PortServices;
        use asyn_rs::trace::TraceFile;
        use tokio::io::AsyncWriteExt;

        const CONNACK: [u8; 5] = [0x20, 0x03, 0x00, 0x00, 0x00];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = MqttConfig {
            broker_host: "127.0.0.1".into(),
            broker_port: listener.local_addr().unwrap().port(),
            client_id: "puback-order".into(),
            keep_alive_secs: 3600,
            ..MqttConfig::default()
        };
        let (publish_tx, publish_rx) = mpsc::unbounded_channel();
        let connected = Arc::new(AtomicBool::new(false));
        let mut driver = crate::driver::MqttDriver::new(
            "MQPUBACK",
            &config,
            Vec::new(),
            publish_tx.clone(),
            connected.clone(),
        );
        let topic_map = driver.topic_map();
        let connected_param = driver.connected_param;
        let shutdown = driver.shutdown_signal();
        let teardown_done = driver.teardown_ack();
        let services = PortServices::new();
        let trace = services.trace().clone();
        let (runtime, _actor) = asyn_rs::runtime::port::create_port_runtime_unclaimed(
            driver,
            asyn_rs::runtime::config::RuntimeConfig {
                services,
                ..Default::default()
            },
        )
        .unwrap();
        let path = std::env::temp_dir().join(format!("mqpuback-{}.log", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        trace.set_trace_file(
            Some("MQPUBACK"),
            TraceFile::File(Arc::new(std::sync::Mutex::new(file))),
        );
        trace.set_trace_mask(Some("MQPUBACK"), TraceMask::ERROR | TraceMask::IO_DRIVER);
        let port_handle = runtime.port_handle().clone();
        let start = Arc::new(Notify::new());
        let loop_start = start.clone();
        let ev = tokio::spawn(async move {
            mqtt_event_loop(
                config,
                topic_map,
                port_handle,
                publish_rx,
                connected_param,
                connected,
                Lifecycle {
                    start: loop_start,
                    shutdown,
                    done: teardown_done,
                },
            )
            .await;
        });
        start.notify_one();

        let mut sock = accept_connect(&listener).await;
        sock.write_all(&CONNACK).await.unwrap();
        let mut pkids = Vec::new();
        for topic in ["dev/a", "dev/b"] {
            publish_tx
                .send(PublishRequest {
                    topic: topic.into(),
                    payload: b"1".to_vec(),
                    qos: crate::config::QoS::AtLeastOnce,
                    retained: false,
                })
                .unwrap();
            let (kind, body) = read_packet(&mut sock).await.unwrap();
            assert_eq!(kind, 3, "PUBLISH");
            let at = 2 + usize::from(u16::from_be_bytes([body[0], body[1]]));
            pkids.push([body[at], body[at + 1]]);
        }
        // dev/b granted first, then dev/a refused with 0x87 Not authorized.
        let [b0, b1] = pkids[1];
        let [a0, a1] = pkids[0];
        sock.write_all(&[0x40, 0x03, b0, b1, 0x00, 0x40, 0x03, a0, a1, 0x87])
            .await
            .unwrap();

        let log = || std::fs::read_to_string(&path).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !(log().contains("MqttDriver::onPublishCb: Published to topic 'dev/b'")
                && log().contains("MqttDriver::onFailCb: MQTT Operation error: 'Error: \n'"))
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("missing ack lines in:\n{}", log()));
        assert!(!log().contains("'dev/a'"), "{}", log());

        runtime.shutdown();
        assert_eq!(read_packet_type(&mut sock).await, Some(14), "DISCONNECT");
        tokio::time::timeout(Duration::from_secs(10), ev)
            .await
            .expect("event loop did not exit")
            .unwrap();
        let _ = std::fs::remove_file(&path);
    }

    mod ack_lines {
        use super::super::*;
        use rumqttc::{PubAck, PubComp, PubRec, PubRecReason};

        fn suback(code: SubscribeReasonCode) -> Result<SubAck, SubscribeNoticeError> {
            Ok(SubAck {
                pkid: 1,
                return_codes: vec![code],
                properties: None,
            })
        }

        fn puback(reason: PubAckReason) -> Result<PublishResult, PublishNoticeError> {
            Ok(PublishResult::Qos1(PubAck {
                pkid: 1,
                reason,
                properties: None,
            }))
        }

        fn pubcomp(reason: PubCompReason) -> Result<PublishResult, PublishNoticeError> {
            Ok(PublishResult::Qos2Completed(PubComp {
                pkid: 1,
                reason,
                properties: None,
            }))
        }

        fn refused() -> AckLine {
            AckLine::Failed(String::new())
        }

        #[test]
        fn subscribed_only_on_a_granted_suback() {
            let ok = SubscribeReasonCode::Success(rumqttc::QoS::AtLeastOnce);
            assert_eq!(subscribe_line(&suback(ok)), AckLine::Succeeded);
            assert_eq!(
                subscribe_line(&suback(SubscribeReasonCode::NotAuthorized)),
                refused()
            );
            assert_eq!(
                subscribe_line(&Err(SubscribeNoticeError::SessionReset)),
                refused()
            );
        }

        #[test]
        fn published_on_write_for_qos0_and_on_the_last_ack_otherwise() {
            assert_eq!(
                publish_line(&Ok(PublishResult::Qos0Flushed)),
                AckLine::Succeeded
            );
            assert_eq!(
                publish_line(&puback(PubAckReason::Success)),
                AckLine::Succeeded
            );
            assert_eq!(
                publish_line(&puback(PubAckReason::NoMatchingSubscribers)),
                AckLine::Succeeded
            );
            assert_eq!(
                publish_line(&puback(PubAckReason::NotAuthorized)),
                refused()
            );
            assert_eq!(
                publish_line(&pubcomp(PubCompReason::Success)),
                AckLine::Succeeded
            );
            assert_eq!(
                publish_line(&pubcomp(PubCompReason::PacketIdentifierNotFound)),
                refused()
            );
            let pubrec = PubRec {
                pkid: 1,
                reason: PubRecReason::UnspecifiedError,
                properties: None,
            };
            assert_eq!(
                publish_line(&Ok(PublishResult::Qos2PubRecRejected(pubrec))),
                refused()
            );
        }

        /// A publish its session took with it fails without a message, as
        /// `MQTTAsync_freeResponses` fails it (MQTTAsyncUtils.c:1902-1938).
        #[test]
        fn a_publish_lost_with_its_session_fails() {
            assert_eq!(
                publish_line(&Err(PublishNoticeError::SessionReset)),
                refused()
            );
            assert_eq!(
                publish_line(&Err(PublishNoticeError::Qos0NotFlushed)),
                AckLine::Failed("qos0 publish was not flushed to the network".into())
            );
        }
    }
}
