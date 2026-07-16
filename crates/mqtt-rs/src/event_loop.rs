use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use asyn_rs::param::ParamValue;
use asyn_rs::port_handle::PortHandle;
use asyn_rs::request::ParamSetValue;
use rumqttc::v5::{AsyncClient, Event, Incoming, MqttOptions};
use tokio::sync::{Notify, mpsc};

use crate::address::ValueType;
use crate::config::MqttConfig;
use crate::driver::{PublishRequest, SharedTopicMap, TopicMap};
use crate::payload::{DecodedValue, decode_payload, octet_cstr};

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
pub async fn mqtt_event_loop(
    config: MqttConfig,
    topic_map: SharedTopicMap,
    port_handle: PortHandle,
    publish_rx: mpsc::UnboundedReceiver<PublishRequest>,
    connected_param: usize,
    connected: Arc<AtomicBool>,
    start: Arc<Notify>,
) {
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

    // C parity: `drvMqtt` does not connect from its constructor; it registers
    // `setInitHook(initHook)` (drvMqtt.cpp:124) and only calls
    // `mqttClient.connect()` from that hook at `initHookAfterScanInit`
    // (drvMqtt.cpp:186-189), i.e. after every record has bound and registered
    // (or not) its `I/O Intr`. We mirror that by holding the first `poll()`
    // (which triggers rumqttc's TCP connect) until the `AfterScanInit` hook
    // fires `start`. This guarantees the first ConnAck's subscribe sees the
    // fully-populated interrupt-variable set rather than racing iocInit.
    start.notified().await;

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
                // match and is dropped (mirrors the non-UTF-8 payload guard).
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
            Err(e) => {
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
    let payload_str = match std::str::from_utf8(payload) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Non-UTF8 payload on topic '{topic}': {e}");
            return;
        }
    };

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

    let mut batch_updates = Vec::new();

    for (reason, addr) in subscribers {
        if !bound_reasons.contains(reason) {
            continue;
        }
        match decode_payload(payload_str, addr) {
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
                            ParamValue::Octet(octet_cstr(&v).to_string()),
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
    use crate::address::TopicAddress;

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
