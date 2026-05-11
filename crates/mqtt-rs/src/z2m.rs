//! Zigbee2MQTT device type builders.
//!
//! Each builder registers MQTT topics and generates EPICS records
//! for a specific Z2M device type, eliminating manual db/st.cmd boilerplate.
//!
//! # Usage (st.cmd)
//! ```text
//! mqttZ2mPlug("MQTT1", "TEST:MQTT:", "SWR:Plug", "zigbee2mqtt/living room plug")
//! mqttZ2mTempSensor("MQTT1", "TEST:MQTT:", "LR:Sens", "zigbee2mqtt/living room sensor")
//! mqttZ2mLight("MQTT1", "TEST:MQTT:", "SWR:Desk", "zigbee2mqtt/desk light")
//! mqttZ2mSwitch("MQTT1", "TEST:MQTT:", "MBath:Light", "zigbee2mqtt/bathroom light")
//! mqttZ2mMotion("MQTT1", "TEST:MQTT:", "ENT:Motion", "zigbee2mqtt/entrance motion")
//! mqttZ2mRemote2("MQTT1", "TEST:MQTT:", "LBath:Sw", "zigbee2mqtt/bathroom switch")
//! mqttDriverConfigure("MQTT1", "mqtt://localhost:1883", "epics-mqtt-ioc", 1)
//! iocInit()
//! ```

use std::collections::HashMap;
use std::fmt::Write;

use epics_base_rs::server::db_loader;
use epics_base_rs::server::iocsh::registry::*;

use crate::address::TopicAddress;
use crate::ioc::register_pending_topic;

/// A single record definition to be generated.
struct RecordDef {
    record_type: &'static str,
    suffix: &'static str,
    dtyp: &'static str,
    link_field: &'static str, // "INP" or "OUT"
    drv_info: String,         // e.g. "JSON:FLOAT zigbee2mqtt/living room plug power"
    egu: &'static str,
    prec: Option<i16>,
    scan_io_intr: bool,
}

/// Generate a .db string from record definitions and load it via db_loader.
fn load_records(prefix: &str, dev: &str, port: &str, records: &[RecordDef], ctx: &CommandContext) {
    let mut db_string = String::new();
    for r in records {
        let pv_name = format!("{prefix}{dev}:{}", r.suffix);
        let _ = writeln!(db_string, "record({}, \"{pv_name}\") {{", r.record_type);
        let _ = writeln!(db_string, "    field(DTYP, \"{}\")", r.dtyp);
        if r.scan_io_intr {
            let _ = writeln!(db_string, "    field(SCAN, \"I/O Intr\")");
        }
        let _ = writeln!(
            db_string,
            "    field({}, \"@asyn({port}) {}\")",
            r.link_field, r.drv_info
        );
        if !r.egu.is_empty() {
            let _ = writeln!(db_string, "    field(EGU, \"{}\")", r.egu);
        }
        if let Some(prec) = r.prec {
            let _ = writeln!(db_string, "    field(PREC, \"{prec}\")");
        }
        let _ = writeln!(db_string, "}}");
    }

    let macros = HashMap::new();
    match db_loader::parse_db(&db_string, &macros) {
        Ok(defs) => {
            for def in defs {
                match db_loader::create_record(&def.record_type) {
                    Ok(mut record) => {
                        let mut common_fields = Vec::new();
                        if let Err(e) =
                            db_loader::apply_fields(&mut record, &def.fields, &mut common_fields)
                        {
                            eprintln!("z2m: apply_fields for {}: {e}", def.name);
                            continue;
                        }
                        ctx.block_on(async {
                            if let Err(e) = ctx.db().add_record(&def.name, record).await {
                                eprintln!("z2m: register '{}' skipped: {e}", def.name);
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
                    Err(e) => eprintln!("z2m: create_record({}): {e}", def.record_type),
                }
            }
        }
        Err(e) => eprintln!("z2m: parse_db failed: {e}"),
    }
}

/// Register a topic and return the drvInfo string.
fn add_topic(port: &str, drv_info: &str) -> String {
    add_topic_opts(port, drv_info, false)
}

/// Register a topic with ON/OFF normalization enabled.
fn add_topic_normalized(port: &str, drv_info: &str) -> String {
    add_topic_opts(port, drv_info, true)
}

fn add_topic_opts(port: &str, drv_info: &str, normalize: bool) -> String {
    if let Ok(mut addr) = TopicAddress::parse(drv_info) {
        addr.normalize_on_off = normalize;
        register_pending_topic(port, addr);
    }
    drv_info.to_string()
}

// ============ Helper: common 4-arg extraction ============

fn extract_args(args: &[ArgValue]) -> Result<(String, String, String, String), String> {
    let port = match &args[0] {
        ArgValue::String(s) => s.clone(),
        _ => return Err("portName required".into()),
    };
    let prefix = match &args[1] {
        ArgValue::String(s) => s.clone(),
        _ => return Err("prefix required".into()),
    };
    let dev = match &args[2] {
        ArgValue::String(s) => s.clone(),
        _ => return Err("devName required".into()),
    };
    let topic = match &args[3] {
        ArgValue::String(s) => s.clone(),
        _ => return Err("mqttTopic required".into()),
    };
    Ok((port, prefix, dev, topic))
}

fn z2m_arg_defs() -> Vec<ArgDesc> {
    vec![
        ArgDesc {
            name: "portName",
            arg_type: ArgType::String,
            optional: false,
        },
        ArgDesc {
            name: "prefix",
            arg_type: ArgType::String,
            optional: false,
        },
        ArgDesc {
            name: "devName",
            arg_type: ArgType::String,
            optional: false,
        },
        ArgDesc {
            name: "mqttTopic",
            arg_type: ArgType::String,
            optional: false,
        },
    ]
}

// ============ Device type builders ============

/// Smart plug: power(W), energy(kWh), device_temperature(degC), state + control
pub fn cmd_z2m_plug() -> CommandDef {
    CommandDef::new(
        "mqttZ2mPlug",
        z2m_arg_defs(),
        "mqttZ2mPlug port prefix dev topic - Z2M smart plug (power/energy/temp/state)",
        |args: &[ArgValue], ctx: &CommandContext| {
            let (port, prefix, dev, topic) = extract_args(args)?;
            println!("mqttZ2mPlug: {dev} -> {topic}");

            let records = vec![
                RecordDef {
                    record_type: "ai",
                    suffix: "Power",
                    dtyp: "asynFloat64",
                    link_field: "INP",
                    drv_info: add_topic(&port, &format!("JSON:FLOAT {topic} power")),
                    egu: "W",
                    prec: Some(1),
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "ai",
                    suffix: "Energy",
                    dtyp: "asynFloat64",
                    link_field: "INP",
                    drv_info: add_topic(&port, &format!("JSON:FLOAT {topic} energy")),
                    egu: "kWh",
                    prec: Some(2),
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "longin",
                    suffix: "DevTemp",
                    dtyp: "asynInt32",
                    link_field: "INP",
                    drv_info: add_topic(&port, &format!("JSON:INT {topic} device_temperature")),
                    egu: "degC",
                    prec: None,
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "stringin",
                    suffix: "State",
                    dtyp: "asynOctetRead",
                    link_field: "INP",
                    drv_info: add_topic(&port, &format!("JSON:STRING {topic} state")),
                    egu: "",
                    prec: None,
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "stringout",
                    suffix: "SetState",
                    dtyp: "asynOctetWrite",
                    link_field: "OUT",
                    drv_info: add_topic_normalized(
                        &port,
                        &format!("JSON:STRING {topic}/set state"),
                    ),
                    egu: "",
                    prec: None,
                    scan_io_intr: false,
                },
            ];
            load_records(&prefix, &dev, &port, &records, ctx);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Temperature/humidity sensor: temperature(degC), humidity(%), battery(%)
pub fn cmd_z2m_temp_sensor() -> CommandDef {
    CommandDef::new(
        "mqttZ2mTempSensor",
        z2m_arg_defs(),
        "mqttZ2mTempSensor port prefix dev topic - Z2M temp/humidity sensor",
        |args: &[ArgValue], ctx: &CommandContext| {
            let (port, prefix, dev, topic) = extract_args(args)?;
            println!("mqttZ2mTempSensor: {dev} -> {topic}");

            let records = vec![
                RecordDef {
                    record_type: "ai",
                    suffix: "Temp",
                    dtyp: "asynFloat64",
                    link_field: "INP",
                    drv_info: add_topic(&port, &format!("JSON:FLOAT {topic} temperature")),
                    egu: "degC",
                    prec: Some(1),
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "ai",
                    suffix: "Hum",
                    dtyp: "asynFloat64",
                    link_field: "INP",
                    drv_info: add_topic(&port, &format!("JSON:FLOAT {topic} humidity")),
                    egu: "%",
                    prec: Some(1),
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "longin",
                    suffix: "Batt",
                    dtyp: "asynInt32",
                    link_field: "INP",
                    drv_info: add_topic(&port, &format!("JSON:INT {topic} battery")),
                    egu: "%",
                    prec: None,
                    scan_io_intr: true,
                },
            ];
            load_records(&prefix, &dev, &port, &records, ctx);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Light (Aqara-style): brightness, color_temp, state + control (state, brightness)
pub fn cmd_z2m_light() -> CommandDef {
    CommandDef::new(
        "mqttZ2mLight",
        z2m_arg_defs(),
        "mqttZ2mLight port prefix dev topic - Z2M dimmable light",
        |args: &[ArgValue], ctx: &CommandContext| {
            let (port, prefix, dev, topic) = extract_args(args)?;
            println!("mqttZ2mLight: {dev} -> {topic}");

            let records = vec![
                RecordDef {
                    record_type: "longin",
                    suffix: "Brightness",
                    dtyp: "asynInt32",
                    link_field: "INP",
                    drv_info: add_topic(&port, &format!("JSON:INT {topic} brightness")),
                    egu: "",
                    prec: None,
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "longin",
                    suffix: "ColorTemp",
                    dtyp: "asynInt32",
                    link_field: "INP",
                    drv_info: add_topic(&port, &format!("JSON:INT {topic} color_temp")),
                    egu: "mired",
                    prec: None,
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "stringin",
                    suffix: "State",
                    dtyp: "asynOctetRead",
                    link_field: "INP",
                    drv_info: add_topic(&port, &format!("JSON:STRING {topic} state")),
                    egu: "",
                    prec: None,
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "stringout",
                    suffix: "SetState",
                    dtyp: "asynOctetWrite",
                    link_field: "OUT",
                    drv_info: add_topic_normalized(
                        &port,
                        &format!("JSON:STRING {topic}/set state"),
                    ),
                    egu: "",
                    prec: None,
                    scan_io_intr: false,
                },
                RecordDef {
                    record_type: "longout",
                    suffix: "SetBright",
                    dtyp: "asynInt32",
                    link_field: "OUT",
                    drv_info: add_topic(&port, &format!("JSON:INT {topic}/set brightness")),
                    egu: "",
                    prec: None,
                    scan_io_intr: false,
                },
            ];
            load_records(&prefix, &dev, &port, &records, ctx);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// On/off switch module: state + control
pub fn cmd_z2m_switch() -> CommandDef {
    CommandDef::new(
        "mqttZ2mSwitch",
        z2m_arg_defs(),
        "mqttZ2mSwitch port prefix dev topic - Z2M on/off switch",
        |args: &[ArgValue], ctx: &CommandContext| {
            let (port, prefix, dev, topic) = extract_args(args)?;
            println!("mqttZ2mSwitch: {dev} -> {topic}");

            let records = vec![
                RecordDef {
                    record_type: "stringin",
                    suffix: "State",
                    dtyp: "asynOctetRead",
                    link_field: "INP",
                    drv_info: add_topic(&port, &format!("JSON:STRING {topic} state")),
                    egu: "",
                    prec: None,
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "stringout",
                    suffix: "SetState",
                    dtyp: "asynOctetWrite",
                    link_field: "OUT",
                    drv_info: add_topic_normalized(
                        &port,
                        &format!("JSON:STRING {topic}/set state"),
                    ),
                    egu: "",
                    prec: None,
                    scan_io_intr: false,
                },
            ];
            load_records(&prefix, &dev, &port, &records, ctx);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Motion sensor: occupancy (string "true"/"false"), battery(%)
pub fn cmd_z2m_motion() -> CommandDef {
    CommandDef::new(
        "mqttZ2mMotion",
        z2m_arg_defs(),
        "mqttZ2mMotion port prefix dev topic - Z2M motion sensor",
        |args: &[ArgValue], ctx: &CommandContext| {
            let (port, prefix, dev, topic) = extract_args(args)?;
            println!("mqttZ2mMotion: {dev} -> {topic}");

            let records = vec![
                RecordDef {
                    record_type: "stringin",
                    suffix: "Occ",
                    dtyp: "asynOctetRead",
                    link_field: "INP",
                    drv_info: add_topic(&port, &format!("JSON:STRING {topic} occupancy")),
                    egu: "",
                    prec: None,
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "longin",
                    suffix: "Batt",
                    dtyp: "asynInt32",
                    link_field: "INP",
                    drv_info: add_topic(&port, &format!("JSON:INT {topic} battery")),
                    egu: "%",
                    prec: None,
                    scan_io_intr: true,
                },
            ];
            load_records(&prefix, &dev, &port, &records, ctx);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// 2-button remote: action (string), battery(%)
pub fn cmd_z2m_remote2() -> CommandDef {
    CommandDef::new(
        "mqttZ2mRemote2",
        z2m_arg_defs(),
        "mqttZ2mRemote2 port prefix dev topic - Z2M 2-button remote",
        |args: &[ArgValue], ctx: &CommandContext| {
            let (port, prefix, dev, topic) = extract_args(args)?;
            println!("mqttZ2mRemote2: {dev} -> {topic}");

            let records = vec![
                RecordDef {
                    record_type: "stringin",
                    suffix: "Act",
                    dtyp: "asynOctetRead",
                    link_field: "INP",
                    drv_info: add_topic(&port, &format!("JSON:STRING {topic} action")),
                    egu: "",
                    prec: None,
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "longin",
                    suffix: "Batt",
                    dtyp: "asynInt32",
                    link_field: "INP",
                    drv_info: add_topic(&port, &format!("JSON:INT {topic} battery")),
                    egu: "%",
                    prec: None,
                    scan_io_intr: true,
                },
            ];
            load_records(&prefix, &dev, &port, &records, ctx);
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Register all Z2M builder commands on an IocApplication.
pub fn register_z2m_commands(
    app: epics_ca_rs::server::ioc_app::IocApplication,
) -> epics_ca_rs::server::ioc_app::IocApplication {
    app.register_startup_command(cmd_z2m_plug())
        .register_startup_command(cmd_z2m_temp_sensor())
        .register_startup_command(cmd_z2m_light())
        .register_startup_command(cmd_z2m_switch())
        .register_startup_command(cmd_z2m_motion())
        .register_startup_command(cmd_z2m_remote2())
}
