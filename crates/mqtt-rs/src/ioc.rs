use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime;
use asyn_rs::trace::TraceManager;
use epics_base_rs::server::iocsh::registry::*;

use crate::address::TopicAddress;
use crate::config::{MqttConfig, QoS};
use crate::driver::MqttDriver;
use crate::event_loop::mqtt_event_loop;

/// Per-port pre-bind config overlay.
///
/// The driver creates topic params on demand as records bind (C parity:
/// `Autoparam::Driver` lazy `createParam`), so topics are NOT pre-declared.
/// This registry only carries per-topic configuration the drvInfo grammar
/// cannot express — currently the Z2M builders' `normalize_on_off` — which
/// `mqttDriverConfigure` hands to the driver as an overlay consulted during
/// on-demand creation. Populated by [`register_pending_topic`] (the Z2M
/// builders), drained once by [`take_pending_topics`] at driver creation.
static PENDING_TOPICS: Mutex<Option<HashMap<String, Vec<TopicAddress>>>> = Mutex::new(None);

pub(crate) fn register_pending_topic(port_name: &str, addr: TopicAddress) {
    let mut guard = PENDING_TOPICS.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    map.entry(port_name.to_string()).or_default().push(addr);
}

fn take_pending_topics(port_name: &str) -> Vec<TopicAddress> {
    let mut guard = PENDING_TOPICS.lock().unwrap();
    guard
        .as_mut()
        .and_then(|map| map.remove(port_name))
        .unwrap_or_default()
}

/// Create the `mqttDriverConfigure` command definition.
///
/// Usage: `mqttDriverConfigure portName brokerUrl clientId qos`
///
/// No topics are pre-declared: the driver creates topic params on demand as
/// records bind (C parity with `Autoparam::Driver`). Records reference a topic
/// directly through their `INP`/`OUT` `@asyn(port) FORMAT:TYPE topic[ field]`
/// link.
pub fn mqtt_driver_configure_command(
    handle: epics_base_rs::runtime::task::RuntimeHandle,
    trace: Arc<TraceManager>,
) -> CommandDef {
    CommandDef::new(
        "mqttDriverConfigure",
        vec![
            ArgDesc {
                name: "portName",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "brokerUrl",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "clientId",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "qos",
                arg_type: ArgType::Int,
                optional: true,
            },
            ArgDesc {
                name: "connPvName",
                arg_type: ArgType::String,
                optional: true,
            },
        ],
        "mqttDriverConfigure portName brokerUrl clientId [qos] [connPvName] - Create MQTT driver",
        MqttConfigHandler { handle, trace },
    )
}

/// Resolve the iocsh `qos` argument to a [`QoS`].
///
/// C registers `qos` as an optional `iocshArgInt` and passes `args[3].ival`
/// straight through (drvMqtt.cpp:745,764). iocsh zero-fills an omitted int arg,
/// so `mqttDriverConfigure` with no qos uses QoS 0 / AtMostOnce — not the
/// library-ergonomic `QoS::default()` (AtLeastOnce), which would publish and
/// subscribe one delivery tier above C on the omitted-arg path.
fn resolve_iocsh_qos(arg: &ArgValue) -> QoS {
    match arg {
        ArgValue::Int(v) => QoS::from_int(*v as i32),
        _ => QoS::from_int(0),
    }
}

struct MqttConfigHandler {
    handle: epics_base_rs::runtime::task::RuntimeHandle,
    trace: Arc<TraceManager>,
}

impl CommandHandler for MqttConfigHandler {
    fn call(&self, args: &[ArgValue], ctx: &CommandContext) -> CommandResult {
        let port_name = match &args[0] {
            ArgValue::String(s) => s.clone(),
            _ => return Err("portName required".into()),
        };
        let broker_url = match &args[1] {
            ArgValue::String(s) => s.clone(),
            _ => return Err("brokerUrl required".into()),
        };
        let client_id = match &args[2] {
            ArgValue::String(s) => s.clone(),
            _ => return Err("clientId required".into()),
        };
        let qos = resolve_iocsh_qos(&args[3]);
        let conn_pv_name = match &args[4] {
            ArgValue::String(s) if !s.is_empty() => Some(s.clone()),
            _ => None,
        };

        let (host, port) = MqttConfig::parse_broker_url(&broker_url);
        let config = MqttConfig {
            broker_host: host,
            broker_port: port,
            client_id,
            qos,
            ..MqttConfig::default()
        };

        // Pre-bind config overlay (Z2M `normalize_on_off`); empty for generic
        // MQTT. Topic params themselves are created on demand when records bind.
        let overlay = take_pending_topics(&port_name);
        println!(
            "mqttDriverConfigure: port={port_name} broker={}:{}",
            config.broker_host, config.broker_port,
        );

        let (publish_tx, publish_rx) = tokio::sync::mpsc::unbounded_channel();
        // Shared broker-connection flag: the event loop is the sole writer, the
        // driver write path the reader (C parity: publish fails while
        // disconnected, mqttClient.cpp:70-72). Starts down until the first
        // ConnAck.
        let connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let driver = MqttDriver::new(&port_name, &config, overlay, publish_tx, connected.clone());
        let topic_map = driver.topic_map();
        let connected_param = driver.connected_param;

        // A port whose actor thread the OS refused is not a port: fail the
        // command before the registration below claims the name for it. Same
        // end state as C by a different route — `MqttDriver` derives from
        // `Autoparam::Driver` and so from `asynPortDriver` (drvMqtt.h:23,
        // autoparamDriver.h:245) and `mqttDriverConfigure` builds it with a
        // bare `new` (drvMqtt.cpp:736-739), so a failed `registerPort` throws
        // out of its constructor (asynPortDriver.cpp:4036-4040), iocsh catches
        // it (iocsh.cpp:1274-1284) and st.cmd continues with the port missing.
        // The `?` is the same error channel the duplicate-name failure below
        // already uses.
        let (runtime_handle, _actor_jh) =
            create_port_runtime(driver, RuntimeConfig::default()).map_err(|e| e.to_string())?;
        let port_handle = runtime_handle.port_handle().clone();

        if let Err(e) =
            asyn_rs::asyn_record::register_port(&port_name, port_handle.clone(), self.trace.clone())
        {
            // Nothing published this port, so ask the actor to stop.
            runtime_handle.shutdown();
            return Err(e.to_string());
        }
        // The registry above holds a live `PortHandle` for this port, which is
        // what keeps its actor alive; the `PortRuntimeHandle` may drop here.
        drop(runtime_handle);

        // Create Connected PV if name was provided
        let mut conn_pv_error: Option<String> = None;
        if let Some(pv_name) = conn_pv_name {
            let mut db_str = String::new();
            let mut w = crate::db_gen::RecordWriter::new(&mut db_str, "bi", &pv_name);
            w.field("DTYP", "asynInt32");
            // The port name reaches here from `st.cmd`, so it is escaped like
            // any other caller-supplied text going into a quoted value.
            w.field(
                "INP",
                &format!("@asyn({port_name}) {}", crate::driver::PARAM_CONNECTED),
            );
            w.field("SCAN", "I/O Intr");
            w.field("ZNAM", "Disconnected");
            w.field("ONAM", "Connected");
            w.field("ZSV", "MAJOR");
            w.field("OSV", "NO_ALARM");
            w.end();
            // A bad `connPvName` (a space, `.`, `$`, `\'` or `"` — rejected by
            // db_loader::validate_name) used to be discarded here and the
            // success line printed anyway, so the IOC booted reporting a
            // Connected PV that `dbl` never listed. The failure is carried to
            // the end of the command instead of returned here, so the port
            // this call already registered still gets its init hook and its
            // event loop.
            match crate::db_gen::load_generated_db("mqttDriverConfigure", &db_str, ctx) {
                Ok(()) => println!("mqttDriverConfigure: connected PV = {pv_name}"),
                Err(e) => {
                    eprintln!("{e}");
                    conn_pv_error = Some(e);
                }
            }
        }

        // C parity: defer the broker connect (and its first subscribe) until
        // after iocInit, when records have bound and registered (or not) their
        // `I/O Intr` interrupts. `drvMqtt` does this via `setInitHook(initHook)`
        // → `mqttClient.connect()` at `initHookAfterScanInit`
        // (drvMqtt.cpp:124,186-189). The event loop waits on `start`; the
        // `AfterScanInit` hook below releases it.
        let start = Arc::new(tokio::sync::Notify::new());
        let start_hook = start.clone();
        epics_base_rs::server::ioc_app::init_hook_register(Arc::new(move |state| {
            if matches!(
                state,
                epics_base_rs::server::ioc_app::InitHookState::AfterScanInit
            ) {
                start_hook.notify_one();
            }
        }));

        // Spawn the MQTT event loop as a background task
        let event_config = config.clone();
        self.handle.spawn(async move {
            mqtt_event_loop(
                event_config,
                topic_map,
                port_handle,
                publish_rx,
                connected_param,
                connected,
                start,
            )
            .await;
        });

        if let Some(e) = conn_pv_error {
            return Err(e);
        }
        Ok(CommandOutcome::Continue)
    }
}

/// Register all MQTT iocsh commands on an `IocApplication`.
///
/// Call this in your IOC's main function:
/// ```ignore
/// app = mqtt_rs::ioc::register_mqtt_commands(app, handle, trace);
/// ```
pub fn register_mqtt_commands(
    app: epics_ca_rs::server::ioc_app::IocApplication,
    handle: epics_base_rs::runtime::task::RuntimeHandle,
    trace: Arc<TraceManager>,
) -> epics_ca_rs::server::ioc_app::IocApplication {
    app.register_startup_command(mqtt_driver_configure_command(handle, trace))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_qos_defaults_to_atmostonce_like_c_iocsh_zerofill() {
        // C iocsh zero-fills the omitted `iocshArgInt` qos to 0 → AtMostOnce.
        assert_eq!(resolve_iocsh_qos(&ArgValue::Missing), QoS::AtMostOnce);
    }

    #[test]
    fn explicit_qos_arg_is_honoured() {
        assert_eq!(resolve_iocsh_qos(&ArgValue::Int(0)), QoS::AtMostOnce);
        assert_eq!(resolve_iocsh_qos(&ArgValue::Int(1)), QoS::AtLeastOnce);
        assert_eq!(resolve_iocsh_qos(&ArgValue::Int(2)), QoS::ExactlyOnce);
        // Out-of-range falls back to AtLeastOnce via QoS::from_int.
        assert_eq!(resolve_iocsh_qos(&ArgValue::Int(99)), QoS::AtLeastOnce);
    }
}
