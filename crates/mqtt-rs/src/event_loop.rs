use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use asyn_rs::param::ParamValue;
use asyn_rs::port_handle::PortHandle;
use asyn_rs::request::ParamSetValue;
use rumqttc::Outgoing;
use rumqttc::v5::{AsyncClient, Event, Incoming, MqttOptions};
use tokio::sync::{Notify, mpsc};

use crate::address::{PayloadFormat, TopicAddress, ValueType};
use crate::config::MqttConfig;
use crate::driver::{PublishRequest, SharedTopicMap, TopicMap};
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
        MqttOptions::new(&config.client_id, &config.broker_host, config.broker_port);
    mqttoptions.set_keep_alive(Duration::from_secs(config.keep_alive_secs));
    // C connects with MQTT v5 connOpts (mqttClient.cpp:20-22): clean_start +
    // keepAlive, no will. rumqttc's v5 client uses `clean_start` (the v5
    // spelling) in place of v3.1.1's `clean_session`.
    mqttoptions.set_clean_start(config.clean_session);

    let (client, mut eventloop) = AsyncClient::new(mqttoptions, 256);

    // `EventLoop::poll()` is not cancel-safe (rumqttc internal iterators can be
    // left half-advanced if the future is dropped mid-poll), so we must never
    // drive it inside a `tokio::select!`. Instead, outbound publishes are
    // forwarded on a dedicated task, and the main loop only awaits `poll()`.
    tokio::spawn(publish_task(client.clone(), publish_rx));

    // MQ4 shutdown. C's `~MqttClient` calls `disconnect()`
    // (mqttClient.cpp:37-41), which sends a DISCONNECT and waits for the packet
    // to go out while the session is up (mqttClient.cpp:51-55). `poll()` is not
    // cancel-safe, so the signal cannot be `select!`ed against it; it is routed
    // through rumqttc's command channel instead. The watcher enqueues a
    // Disconnect request, `poll()` writes AND FLUSHES the packet before it
    // yields `Outgoing::Disconnect` (rumqttc v5 eventloop.rs:213-215), and the
    // loop breaks on that event — so the break happens after the bytes are on
    // the wire, which is what `disconnect()->wait()` guarantees in C.
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
        tokio::spawn(async move {
            shutdown.notified().await;
            flag.store(true, Ordering::Release);
            abort.notify_one();
            let _ = dc.disconnect().await;
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
    // `is_connected` mirrors the Connected PV locally so we can detect the
    // "stuck at 0" case where a recoverable rumqttc error flipped the PV to 0
    // but the underlying TCP/MQTT session is still alive (no ConnAck will come
    // to flip it back). Any inbound packet (`Publish`, `PingResp`) is direct
    // proof the session is alive, so we use it as a fallback recovery signal.
    let mut is_connected = false;
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Incoming::Publish(publish))) => {
                if !is_connected {
                    tracing::debug!(
                        "MQTT Publish received while Connected=0 — restoring Connected=1"
                    );
                    mark_connected(&port_handle, connected_param, &connected).await;
                    is_connected = true;
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
                    Err(e) => tracing::warn!("Non-UTF8 MQTT topic: {e}"),
                }
            }
            Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                mark_connected(&port_handle, connected_param, &connected).await;
                is_connected = true;
                // C parity: `onConnectCb` subscribes only the topics of records
                // bound by `I/O Intr` / `asyn:READBACK` — the live
                // interrupt-variable set (`getInterruptVariables()`,
                // drvMqtt.cpp:207-213). Resolving it at every ConnAck means a
                // reconnect re-subscribes whatever set is currently bound.
                let sub_topics = subscribe_topics(
                    &reason_to_topic,
                    &port_handle.interrupts().subscribed_bindings(),
                );
                tracing::info!(
                    "MQTT connected, subscribing to {} topic(s)",
                    sub_topics.len()
                );
                // Spawn subscribe so we return to `poll()` immediately — the
                // event loop is the only thing that drains rumqttc's command
                // channel, so awaiting subscribe inline risks stalling.
                let sub_client = client.clone();
                let sub_qos = config.qos;
                tokio::spawn(async move {
                    subscribe_all(&sub_client, &sub_topics, sub_qos).await;
                });
            }
            Ok(Event::Incoming(Incoming::PingResp(_))) => {
                if !is_connected {
                    tracing::debug!(
                        "MQTT PingResp received while Connected=0 — restoring Connected=1"
                    );
                    mark_connected(&port_handle, connected_param, &connected).await;
                    is_connected = true;
                }
            }
            Ok(Event::Outgoing(Outgoing::Disconnect)) => {
                tracing::info!("MQTT DISCONNECT sent; event loop exiting");
                break;
            }
            Err(e) => {
                // A shutdown with the session already down: C sends nothing at
                // all (`if (client_.is_connected())`, mqttClient.cpp:52), so
                // exit without reconnecting rather than sleeping and retrying.
                if shutting_down.load(Ordering::Acquire) {
                    tracing::info!("MQTT event loop exiting while disconnected: {e}");
                    break;
                }
                tracing::error!("MQTT connection error: {e}");
                // Close the publish gate first so a concurrent write fails fast
                // (C: publish throws while disconnected) instead of buffering.
                connected.store(false, Ordering::Release);
                let _ = port_handle
                    .set_params_and_notify(
                        0,
                        vec![ParamSetValue::new(connected_param, 0, ParamValue::Int32(0))],
                    )
                    .await;
                is_connected = false;
                tokio::time::sleep(Duration::from_secs(1)).await;
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
    let _ = done.send(());
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

/// Forward publish requests from EPICS writes into rumqttc's command channel.
/// Runs on its own task so the main event-loop task can own `poll()`
/// exclusively without cancel-safety hazards.
async fn publish_task(
    client: AsyncClient,
    mut publish_rx: mpsc::UnboundedReceiver<PublishRequest>,
) {
    while let Some(req) = publish_rx.recv().await {
        let PublishRequest {
            topic,
            payload,
            qos,
            retained,
        } = req;
        let qos: rumqttc::v5::mqttbytes::QoS = qos.into();
        // v5 publish wants `P: Into<Bytes>`; the owned `Vec<u8>` payload
        // satisfies it directly (no borrow held across the await), carrying raw
        // non-UTF-8 octet bytes through unchanged.
        if let Err(e) = client.publish(&topic, qos, retained, payload).await {
            tracing::warn!("MQTT publish to '{topic}' failed: {e}");
        }
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
            tracing::debug!(
                "Non-text payload on topic '{topic}' for {}; this subscriber skipped",
                addr.value_type.label(),
            );
            continue;
        };
        let text = match input {
            // No parse step: C's octet branch stores the bytes it was handed.
            SubscriberInput::OctetBytes(bytes) => {
                batch_updates.push(ParamSetValue::new(
                    *reason,
                    0,
                    ParamValue::Octet(bytes.to_vec()),
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
                            ParamValue::Octet(octet_cstr(&v).as_bytes().to_vec()),
                        ));
                    }
                    DecodedValue::Float64Array(v) => {
                        batch_updates.push(ParamSetValue::new(
                            *reason,
                            0,
                            ParamValue::Float64Array(v.into()),
                        ));
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
                        batch_updates.push(ParamSetValue::new(
                            *reason,
                            0,
                            ParamValue::Int32Array(v.into()),
                        ));
                    }
                }
            }
            Err(e) => {
                tracing::debug!(
                    "Failed to decode '{}' on topic '{topic}': {e}",
                    addr.value_type.label(),
                );
            }
        }
    }

    batch_updates
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

async fn subscribe_all(client: &AsyncClient, topics: &[String], qos: crate::config::QoS) {
    let rqos: rumqttc::v5::mqttbytes::QoS = qos.into();
    for topic in topics {
        if let Err(e) = client.subscribe(topic, rqos).await {
            tracing::warn!("MQTT subscribe to '{topic}' failed: {e}");
        }
    }
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

    let batch_updates = decode_for_subscribers(payload, subscribers, &bound_reasons, topic);

    if !batch_updates.is_empty()
        && let Err(e) = port_handle.set_params_and_notify(0, batch_updates).await
    {
        eprintln!("set_params_and_notify error (mqtt payload): {e}");
    }
}

impl ValueType {
    fn label(&self) -> &'static str {
        match self {
            ValueType::Int => "INT",
            ValueType::Float => "FLOAT",
            ValueType::Digital => "DIGITAL",
            ValueType::String => "STRING",
            ValueType::IntArray => "INTARRAY",
            ValueType::FloatArray => "FLOATARRAY",
        }
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
        let batch =
            decode_for_subscribers(b"hello\x00\xff\xfe", &subscribers, &bound, "sensor/raw");
        assert_eq!(
            batch.len(),
            1,
            "exactly the STRING subscriber updates: {batch:?}"
        );
        match &batch[0] {
            ParamSetValue::Value { reason, value, .. } => {
                assert_eq!(*reason, 1);
                match value {
                    ParamValue::Octet(v) => assert_eq!(v.as_slice(), b"hello"),
                    other => panic!("expected Octet, got {other:?}"),
                }
            }
            other => panic!("expected a value set, got {other:?}"),
        }

        // Text that both can read still reaches both.
        let batch = decode_for_subscribers(b"42", &subscribers, &bound, "sensor/raw");
        assert_eq!(batch.len(), 2, "a readable payload reaches both: {batch:?}");

        // Bytes that are not text at all and carry no NUL: C's octet branch
        // never decodes, so `setStringParam(val.c_str())` stores both bytes and
        // only the INT record's parse fails. The port matches now that
        // `ParamValue::Octet` carries `Vec<u8>`.
        let batch = decode_for_subscribers(b"\xff\xfe", &subscribers, &bound, "sensor/raw");
        assert_eq!(
            batch.len(),
            1,
            "the octet store takes raw bytes; only INT is denied: {batch:?}"
        );
        match &batch[0] {
            ParamSetValue::Value { reason, value, .. } => {
                assert_eq!(*reason, 1);
                match value {
                    ParamValue::Octet(v) => assert_eq!(v.as_slice(), b"\xff\xfe"),
                    other => panic!("expected Octet, got {other:?}"),
                }
            }
            other => panic!("expected a value set, got {other:?}"),
        }

        // The NUL is still the octet terminator, and it is now the ONLY thing
        // that truncates: a payload that is nothing but non-text bytes before
        // its NUL stores that prefix rather than being denied.
        let batch = decode_for_subscribers(b"\xff\xfe\x00tail", &subscribers, &bound, "sensor/raw");
        assert_eq!(batch.len(), 1, "{batch:?}");
        match &batch[0] {
            ParamSetValue::Value { value, .. } => match value {
                ParamValue::Octet(v) => assert_eq!(v.as_slice(), b"\xff\xfe"),
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
        let batch = decode_for_subscribers(b"\xff\xfe", &json_subs, &json_bound, "sensor/raw");
        assert!(batch.is_empty(), "JSON octet still needs text: {batch:?}");
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
        use asyn_rs::runtime::port::create_port_runtime;
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
        let (rt, _jh) = create_port_runtime(driver, RuntimeConfig::default())
            .expect("the port actor thread must start");
        let port = rt.port_handle().clone();

        let subscribers = vec![(reason, TopicAddress::parse(spec).unwrap())];
        let bound: HashSet<usize> = [reason].into_iter().collect();

        // Not text, and a NUL after it: C stores `val.c_str()`, i.e. the two
        // raw bytes, and drops the tail.
        let batch = decode_for_subscribers(b"\xff\xfe\x00tail", &subscribers, &bound, "sensor/raw");
        port.set_params_and_notify(0, batch)
            .await
            .expect("the store takes the batch");
        let got = port.read_octet(reason, 0, 32).await.expect("octet read");
        assert_eq!(
            got.as_slice(),
            b"\xff\xfe",
            "the record must read the raw bytes, not U+FFFD and not nothing"
        );

        // And a plain text payload still round-trips unchanged.
        let batch = decode_for_subscribers(b"hello", &subscribers, &bound, "sensor/raw");
        port.set_params_and_notify(0, batch).await.unwrap();
        let got = port.read_octet(reason, 0, 32).await.unwrap();
        assert_eq!(got.as_slice(), b"hello");

        rt.shutdown();
    }

    /// Read one MQTT control packet from `sock` and return its type nibble.
    /// Framing only — the variable header and payload are skipped, so a 0xE0
    /// byte *inside* a packet cannot be mistaken for a DISCONNECT.
    async fn read_packet_type(sock: &mut tokio::net::TcpStream) -> Option<u8> {
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
        Some(first >> 4)
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
            let (mut sock, _) = listener.accept().await.unwrap();
            assert_eq!(read_packet_type(&mut sock).await, Some(1), "CONNECT");
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
        let (runtime, _jh) = asyn_rs::runtime::port::create_port_runtime(
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
        let (runtime, actor) = asyn_rs::runtime::port::create_port_runtime(
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

    #[test]
    fn subscribe_topics_skips_binding_without_topic() {
        // An interrupt binding on a reason that maps to no topic (e.g. the
        // internal _MQTT_CONNECTED param) contributes nothing to the subscribe
        // set rather than panicking or subscribing an empty topic.
        let topic_map: TopicMap = HashMap::new();
        let idx = reason_topic_index(&topic_map);
        assert!(subscribe_topics(&idx, &[(99, 0)]).is_empty());
    }
}
