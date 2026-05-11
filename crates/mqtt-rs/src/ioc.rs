use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::{PortRuntimeHandle, create_port_runtime};
use asyn_rs::trace::TraceManager;
use epics_base_rs::server::iocsh::registry::*;

use crate::address::TopicAddress;
use crate::config::{MqttConfig, QoS};
use crate::driver::MqttDriver;
use crate::event_loop::mqtt_event_loop;

/// Global pending topic registry.
/// Topics are registered via `mqttAddTopic` before `mqttDriverConfigure` creates the driver.
static PENDING_TOPICS: Mutex<Option<HashMap<String, Vec<TopicAddress>>>> = Mutex::new(None);

/// Global storage for port runtime handles.
/// Dropping a PortRuntimeHandle shuts down the actor, so we must keep them alive.
static PORT_RUNTIMES: Mutex<Option<Vec<PortRuntimeHandle>>> = Mutex::new(None);

fn keep_runtime(handle: PortRuntimeHandle) {
    let mut guard = PORT_RUNTIMES.lock().unwrap();
    let vec = guard.get_or_insert_with(Vec::new);
    vec.push(handle);
}

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

/// Create the `mqttAddTopic` command definition.
///
/// Usage: `mqttAddTopic portName "FLAT:INT test/temperature"`
pub fn mqtt_add_topic_command() -> CommandDef {
    CommandDef::new(
        "mqttAddTopic",
        vec![
            ArgDesc {
                name: "portName",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "drvInfo",
                arg_type: ArgType::String,
                optional: false,
            },
        ],
        "mqttAddTopic portName drvInfo - Register an MQTT topic before driver creation",
        |args: &[ArgValue], _ctx: &CommandContext| -> CommandResult {
            let port_name = match &args[0] {
                ArgValue::String(s) => s.clone(),
                _ => return Err("portName required".into()),
            };
            let drv_info = match &args[1] {
                ArgValue::String(s) => s.clone(),
                _ => return Err("drvInfo required".into()),
            };

            let addr = TopicAddress::parse(&drv_info).map_err(|e| e.to_string())?;
            println!(
                "mqttAddTopic: port={port_name} topic={}",
                addr.to_drv_info()
            );
            register_pending_topic(&port_name, addr);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Create the `mqttDriverConfigure` command definition.
///
/// Usage: `mqttDriverConfigure portName brokerUrl clientId qos`
///
/// Topics must be registered first via `mqttAddTopic`.
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
        let qos = match &args[3] {
            ArgValue::Int(v) => QoS::from_int(*v as i32),
            _ => QoS::default(),
        };
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

        let topics = take_pending_topics(&port_name);
        if topics.is_empty() {
            println!("mqttDriverConfigure: WARNING — no topics registered for port '{port_name}'");
            println!("  Use mqttAddTopic before mqttDriverConfigure");
        } else {
            println!(
                "mqttDriverConfigure: port={port_name} broker={}:{} topics={}",
                config.broker_host,
                config.broker_port,
                topics.len()
            );
        }

        let (publish_tx, publish_rx) = tokio::sync::mpsc::unbounded_channel();
        let driver = MqttDriver::new(&port_name, &config, topics, publish_tx);
        let subscribed_topics = driver.subscribed_topics();
        let topic_map = driver.topic_map().clone();
        let connected_param = driver.connected_param;

        let (runtime_handle, _actor_jh) = create_port_runtime(driver, RuntimeConfig::default());
        let port_handle = runtime_handle.port_handle().clone();

        asyn_rs::asyn_record::register_port(&port_name, port_handle.clone(), self.trace.clone());

        // Keep the runtime handle alive — dropping it shuts down the actor
        keep_runtime(runtime_handle);

        // Create Connected PV if name was provided
        if let Some(pv_name) = conn_pv_name {
            let db_str = format!(
                concat!(
                    "record(bi, \"{pv}\") {{\n",
                    "    field(DTYP, \"asynInt32\")\n",
                    "    field(INP, \"@asyn({port}) {param}\")\n",
                    "    field(SCAN, \"I/O Intr\")\n",
                    "    field(ZNAM, \"Disconnected\")\n",
                    "    field(ONAM, \"Connected\")\n",
                    "    field(ZSV, \"MAJOR\")\n",
                    "    field(OSV, \"NO_ALARM\")\n",
                    "}}\n",
                ),
                pv = pv_name,
                port = port_name,
                param = crate::driver::PARAM_CONNECTED,
            );
            let macros = std::collections::HashMap::new();
            if let Ok(defs) = epics_base_rs::server::db_loader::parse_db(&db_str, &macros) {
                for def in defs {
                    if let Ok(mut record) =
                        epics_base_rs::server::db_loader::create_record(&def.record_type)
                    {
                        let mut common_fields = Vec::new();
                        let _ = epics_base_rs::server::db_loader::apply_fields(
                            &mut record,
                            &def.fields,
                            &mut common_fields,
                        );
                        ctx.block_on(async {
                            if let Err(e) = ctx.db().add_record(&def.name, record).await {
                                eprintln!(
                                    "mqttDriverConfigure: register '{}' skipped: {e}",
                                    def.name
                                );
                                return;
                            }
                            if let Some(rec_arc) = ctx.db().get_record(&def.name).await {
                                let mut instance = rec_arc.write().await;
                                for (name, value) in common_fields {
                                    let _ = instance.put_common_field(&name, value);
                                }
                            }
                        });
                    }
                }
            }
            println!("mqttDriverConfigure: connected PV = {pv_name}");
        }

        // Spawn the MQTT event loop as a background task
        let event_config = config.clone();
        self.handle.spawn(async move {
            mqtt_event_loop(
                event_config,
                subscribed_topics,
                topic_map,
                port_handle,
                publish_rx,
                connected_param,
            )
            .await;
        });

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
    app.register_startup_command(mqtt_add_topic_command())
        .register_startup_command(mqtt_driver_configure_command(handle, trace))
}
