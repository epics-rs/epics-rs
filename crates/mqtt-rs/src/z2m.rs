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

use epics_base_rs::server::iocsh::registry::*;

use crate::address::TopicAddress;
use crate::db_gen::RecordWriter;
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

/// Render record definitions as `.db` text.
///
/// Split out from [`load_records`] so the generated text can be parsed in a
/// test without a live database: a topic containing a space is written with
/// quotes around it, and getting the escaping wrong fails `parse_db` for the
/// whole device rather than for one field.
fn build_db_string(prefix: &str, dev: &str, port: &str, records: &[RecordDef]) -> String {
    let mut db_string = String::new();
    for r in records {
        let pv_name = format!("{prefix}{dev}:{}", r.suffix);
        let mut w = RecordWriter::new(&mut db_string, r.record_type, &pv_name);
        w.field("DTYP", r.dtyp);
        if r.scan_io_intr {
            w.field("SCAN", "I/O Intr");
        }
        // `drv_info` carries the caller's topic, and a Z2M friendly name with
        // a space is spelled with quotes around it (`TopicAddress::to_drv_info`).
        w.field(r.link_field, &format!("@asyn({port}) {}", r.drv_info));
        if !r.egu.is_empty() {
            w.field("EGU", r.egu);
        }
        if let Some(prec) = r.prec {
            w.field("PREC", &prec.to_string());
        }
        w.end();
    }
    db_string
}

/// Generate a .db string from record definitions and load it.
fn load_records(
    prefix: &str,
    dev: &str,
    port: &str,
    records: &[RecordDef],
    ctx: &CommandContext,
) -> Result<(), String> {
    crate::db_gen::load_generated_db("z2m", &build_db_string(prefix, dev, port, records), ctx)
}

/// Register a Z2M JSON topic and return its canonical drvInfo (param name).
///
/// Z2M friendly names may contain spaces, so the topic is wrapped in the
/// quoted-topic syntax (see [`TopicAddress::parse`]); the JSON field is the
/// literal device member key. The returned string is the canonical
/// `to_drv_info()` form so it matches the param name the driver registers
/// (`MqttDriver::new` creates each param under `addr.to_drv_info()`).
fn add_json(port: &str, type_tag: &str, topic: &str, field: &str) -> String {
    add_topic_opts(port, &format!("JSON:{type_tag} \"{topic}\" {field}"), false)
}

/// Like [`add_json`], with ON/OFF normalization enabled (for `/set` controls).
fn add_json_normalized(port: &str, type_tag: &str, topic: &str, field: &str) -> String {
    add_topic_opts(port, &format!("JSON:{type_tag} \"{topic}\" {field}"), true)
}

/// Parse `drv_info`, register the pending topic, and return the canonical
/// drvInfo. Returning `to_drv_info()` (not the raw input) guarantees the
/// record link text matches the driver's param name even when the input used
/// the quoted-topic extension.
fn add_topic_opts(port: &str, drv_info: &str, normalize: bool) -> String {
    match TopicAddress::parse(drv_info) {
        Ok(mut addr) => {
            addr.normalize_on_off = normalize;
            let canonical = addr.to_drv_info();
            register_pending_topic(port, addr);
            canonical
        }
        Err(_) => drv_info.to_string(),
    }
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
                    drv_info: add_json(&port, "FLOAT", &topic, "power"),
                    egu: "W",
                    prec: Some(1),
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "ai",
                    suffix: "Energy",
                    dtyp: "asynFloat64",
                    link_field: "INP",
                    drv_info: add_json(&port, "FLOAT", &topic, "energy"),
                    egu: "kWh",
                    prec: Some(2),
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "longin",
                    suffix: "DevTemp",
                    dtyp: "asynInt32",
                    link_field: "INP",
                    drv_info: add_json(&port, "INT", &topic, "device_temperature"),
                    egu: "degC",
                    prec: None,
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "stringin",
                    suffix: "State",
                    dtyp: "asynOctetRead",
                    link_field: "INP",
                    drv_info: add_json(&port, "STRING", &topic, "state"),
                    egu: "",
                    prec: None,
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "stringout",
                    suffix: "SetState",
                    dtyp: "asynOctetWrite",
                    link_field: "OUT",
                    drv_info: add_json_normalized(
                        &port,
                        "STRING",
                        &format!("{topic}/set"),
                        "state",
                    ),
                    egu: "",
                    prec: None,
                    scan_io_intr: false,
                },
            ];
            load_records(&prefix, &dev, &port, &records, ctx)?;
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
                    drv_info: add_json(&port, "FLOAT", &topic, "temperature"),
                    egu: "degC",
                    prec: Some(1),
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "ai",
                    suffix: "Hum",
                    dtyp: "asynFloat64",
                    link_field: "INP",
                    drv_info: add_json(&port, "FLOAT", &topic, "humidity"),
                    egu: "%",
                    prec: Some(1),
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "longin",
                    suffix: "Batt",
                    dtyp: "asynInt32",
                    link_field: "INP",
                    drv_info: add_json(&port, "INT", &topic, "battery"),
                    egu: "%",
                    prec: None,
                    scan_io_intr: true,
                },
            ];
            load_records(&prefix, &dev, &port, &records, ctx)?;
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
                    drv_info: add_json(&port, "INT", &topic, "brightness"),
                    egu: "",
                    prec: None,
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "longin",
                    suffix: "ColorTemp",
                    dtyp: "asynInt32",
                    link_field: "INP",
                    drv_info: add_json(&port, "INT", &topic, "color_temp"),
                    egu: "mired",
                    prec: None,
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "stringin",
                    suffix: "State",
                    dtyp: "asynOctetRead",
                    link_field: "INP",
                    drv_info: add_json(&port, "STRING", &topic, "state"),
                    egu: "",
                    prec: None,
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "stringout",
                    suffix: "SetState",
                    dtyp: "asynOctetWrite",
                    link_field: "OUT",
                    drv_info: add_json_normalized(
                        &port,
                        "STRING",
                        &format!("{topic}/set"),
                        "state",
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
                    drv_info: add_json(&port, "INT", &format!("{topic}/set"), "brightness"),
                    egu: "",
                    prec: None,
                    scan_io_intr: false,
                },
            ];
            load_records(&prefix, &dev, &port, &records, ctx)?;
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
                    drv_info: add_json(&port, "STRING", &topic, "state"),
                    egu: "",
                    prec: None,
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "stringout",
                    suffix: "SetState",
                    dtyp: "asynOctetWrite",
                    link_field: "OUT",
                    drv_info: add_json_normalized(
                        &port,
                        "STRING",
                        &format!("{topic}/set"),
                        "state",
                    ),
                    egu: "",
                    prec: None,
                    scan_io_intr: false,
                },
            ];
            load_records(&prefix, &dev, &port, &records, ctx)?;
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
                    drv_info: add_json(&port, "STRING", &topic, "occupancy"),
                    egu: "",
                    prec: None,
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "longin",
                    suffix: "Batt",
                    dtyp: "asynInt32",
                    link_field: "INP",
                    drv_info: add_json(&port, "INT", &topic, "battery"),
                    egu: "%",
                    prec: None,
                    scan_io_intr: true,
                },
            ];
            load_records(&prefix, &dev, &port, &records, ctx)?;
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
                    drv_info: add_json(&port, "STRING", &topic, "action"),
                    egu: "",
                    prec: None,
                    scan_io_intr: true,
                },
                RecordDef {
                    record_type: "longin",
                    suffix: "Batt",
                    dtyp: "asynInt32",
                    link_field: "INP",
                    drv_info: add_json(&port, "INT", &topic, "battery"),
                    egu: "%",
                    prec: None,
                    scan_io_intr: true,
                },
            ];
            load_records(&prefix, &dev, &port, &records, ctx)?;
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use epics_base_rs::server::db_loader;

    use super::*;

    /// The module's own documented invocation (`z2m.rs:8`): a Z2M friendly
    /// name with a space, which `TopicAddress::to_drv_info` spells with quotes
    /// around the topic. Those quotes land inside a quoted `.db` field value,
    /// so an unescaped one ends the value early and `parse_db` fails for the
    /// WHOLE device — not one record of the five is created.
    #[test]
    fn test_spaced_topic_generates_loadable_db() {
        let drv_info = TopicAddress::parse("JSON:FLOAT \"zigbee2mqtt/living room plug\" power")
            .unwrap()
            .to_drv_info();
        assert!(drv_info.contains('"'), "drv_info = {drv_info:?}");

        let records = [RecordDef {
            record_type: "ai",
            suffix: "Power",
            dtyp: "asynFloat64",
            link_field: "INP",
            drv_info: drv_info.clone(),
            egu: "W",
            prec: Some(1),
            scan_io_intr: true,
        }];
        let db = build_db_string("TEST:MQTT:", "SWR:Plug", "MQTT1", &records);
        let defs = db_loader::parse_db(&db, &HashMap::new())
            .unwrap_or_else(|e| panic!("parse_db failed: {e}\n{db}"));

        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "TEST:MQTT:SWR:Plug:Power");
        let (_, inp) = defs[0]
            .fields
            .iter()
            .find(|(k, _)| k == "INP")
            .expect("INP field");
        // The loader must hand the driver back the exact param name the
        // driver registered under `addr.to_drv_info()`.
        assert_eq!(inp.to_string(), format!("@asyn(MQTT1) {drv_info}"));
    }
}
