use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use asyn_rs::port_handle::PortHandle;
use asyn_rs::request::ParamSetValue;
use rumqttc::v5::{AsyncClient, Event, Incoming, MqttOptions};
use tokio::sync::mpsc;

use crate::address::{TopicAddress, ValueType};
use crate::config::MqttConfig;
use crate::driver::PublishRequest;
use crate::payload::{DecodedValue, decode_payload, octet_cstr};

/// Run the MQTT event loop.
///
/// This task:
/// 1. Connects to the MQTT broker and subscribes to all declared topics
/// 2. Dispatches incoming messages to the param cache via `PortHandle`
/// 3. Publishes outgoing messages from EPICS write operations
pub async fn mqtt_event_loop(
    config: MqttConfig,
    topics: Vec<String>,
    topic_map: HashMap<String, Vec<(usize, TopicAddress)>>,
    port_handle: PortHandle,
    publish_rx: mpsc::UnboundedReceiver<PublishRequest>,
    connected_param: usize,
    connected: Arc<AtomicBool>,
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
                tracing::info!("MQTT connected, subscribing to {} topics", topics.len());
                mark_connected(&port_handle, connected_param, &connected).await;
                is_connected = true;
                // Spawn subscribe so we return to `poll()` immediately — the
                // event loop is the only thing that drains rumqttc's command
                // channel, so awaiting subscribe inline risks stalling.
                let sub_client = client.clone();
                let sub_topics = topics.clone();
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
                        vec![ParamSetValue::Int32 {
                            reason: connected_param,
                            addr: 0,
                            value: 0,
                        }],
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
            vec![ParamSetValue::Int32 {
                reason: connected_param,
                addr: 0,
                value: 1,
            }],
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
        // v5 publish wants `P: Into<Bytes>`; the owned `String` payload
        // satisfies it directly (no borrow held across the await).
        if let Err(e) = client.publish(&topic, qos, retained, payload).await {
            tracing::warn!("MQTT publish to '{topic}' failed: {e}");
        }
    }
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
    topic_map: &HashMap<String, Vec<(usize, TopicAddress)>>,
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

    let mut batch_updates = Vec::new();

    for (reason, addr) in subscribers {
        match decode_payload(payload_str, addr) {
            Ok(decoded) => {
                // ParamSetValue carries every inbound value shape:
                // Int32, Float64, Octet, Float64Array, Int32Array,
                // UInt32Digital.
                match decoded {
                    DecodedValue::Int32(v) => {
                        batch_updates.push(ParamSetValue::Int32 {
                            reason: *reason,
                            addr: 0,
                            value: v,
                        });
                    }
                    DecodedValue::Float64(v) => {
                        batch_updates.push(ParamSetValue::Float64 {
                            reason: *reason,
                            addr: 0,
                            value: v,
                        });
                    }
                    DecodedValue::String(v) => {
                        // asyn octet store truncates at the first NUL
                        // (setStringParam(index, val.c_str()), drvMqtt.cpp:299).
                        batch_updates.push(ParamSetValue::Octet {
                            reason: *reason,
                            addr: 0,
                            value: octet_cstr(&v).to_string(),
                        });
                    }
                    DecodedValue::Float64Array(v) => {
                        batch_updates.push(ParamSetValue::Float64Array {
                            reason: *reason,
                            addr: 0,
                            value: v,
                        });
                    }
                    DecodedValue::UInt32(v) => {
                        batch_updates.push(ParamSetValue::UInt32Digital {
                            reason: *reason,
                            addr: 0,
                            value: v,
                            mask: 0xFFFF_FFFF,
                            // Inbound MQTT value: changed bits derive from the
                            // value merge; no forced interrupt mask.
                            interrupt_mask: 0,
                        });
                    }
                    DecodedValue::Int32Array(v) => {
                        batch_updates.push(ParamSetValue::Int32Array {
                            reason: *reason,
                            addr: 0,
                            value: v,
                        });
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
