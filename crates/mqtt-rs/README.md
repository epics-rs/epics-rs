# mqtt-rs

MQTT driver for [epics-rs](https://github.com/epics-rs/epics-rs) — publish/subscribe MQTT topics as EPICS records.

An asyn PortDriver that bridges MQTT brokers to the EPICS record layer. Incoming MQTT messages update PVs via I/O Intr scanning; EPICS output record writes are published to the broker.

Inspired by [epicsMQTT](https://github.com/epics-modules/mqtt) (C++ MQTT support for EPICS using autoparamDriver + Paho). This is an independent Rust implementation, not a port.

## Features

- **Generic MQTT** — works with any MQTT broker and topic structure
- **FLAT payloads** — single values: `INT`, `FLOAT`, `DIGITAL`, `STRING`, `INTARRAY`, `FLOATARRAY`
- **JSON payloads** — extract nested fields via dot-path (e.g. `sensor.temperature`)
- **Bidirectional** — input records subscribe, output records publish
- **Auto-reconnect** — rumqttc handles broker reconnection transparently
- **Connection status PV** — bi record with alarm on disconnect; tracked via ConnAck *and* inbound activity (Publish / PingResp) so the PV cannot get stuck at 0 after a recoverable rumqttc error
- **Zigbee2MQTT builders** — optional device type builders for Z2M (Plug, Light, Switch, TempSensor, Motion, Remote)

## Topic Address Format

Records reference MQTT topics through the asyn drvInfo string:

```
FORMAT:TYPE topic/name [json.field.path]
```

| Field | Values |
|-------|--------|
| FORMAT | `FLAT`, `JSON` |
| TYPE | `INT`, `FLOAT`, `DIGITAL`, `STRING`, `INTARRAY`, `FLOATARRAY` |
| topic | MQTT topic (no wildcards, spaces allowed) |
| json.field.path | Dot-separated path for JSON payloads (required for JSON, forbidden for FLAT) |

Examples:
```
FLAT:FLOAT sensors/temperature
FLAT:INT   sensors/counter
FLAT:STRING device/status
JSON:FLOAT sensors/environment humidity
JSON:INT   sensors/data reading.value
```

## Generic MQTT Usage

For any MQTT broker and topic structure — no Z2M dependency.

### Database (.db file)

```
record(ai, "$(P)Temperature") {
    field(DTYP, "asynFloat64")
    field(INP,  "@asyn($(PORT)) FLAT:FLOAT sensors/temperature")
    field(SCAN, "I/O Intr")
    field(EGU,  "degC")
    field(PREC, "2")
}

record(ao, "$(P)Setpoint") {
    field(DTYP, "asynFloat64")
    field(OUT,  "@asyn($(PORT)) FLAT:FLOAT actuators/setpoint")
}

record(ai, "$(P)Humidity") {
    field(DTYP, "asynFloat64")
    field(INP,  "@asyn($(PORT)) JSON:FLOAT sensors/environment humidity")
    field(SCAN, "I/O Intr")
}
```

### Startup Script

```bash
# Create driver with optional connection status PV.
# No topics are pre-declared: each record creates its topic on demand when it
# binds during iocInit (C parity with Autoparam::Driver).
mqttDriverConfigure("MQTT1", "mqtt://localhost:1883", "epics-client", 1, "TEST:MQTT:Connected")

# Load records — their INP/OUT links carry the topic addresses
dbLoadRecords("db/mqtt.db", "P=TEST:,PORT=MQTT1")

iocInit()
```

### IOC Binary

```rust
use mqtt_rs::ioc::register_mqtt_commands;

let trace = Arc::new(TraceManager::new());
let handle = epics_base_rs::runtime::task::runtime_handle();

let mut app = IocApplication::new();
app = asyn_rs::adapter::register_asyn_device_support(app);
app = register_mqtt_commands(app, handle, trace);

// The `_app` entry point runs C's `rsrvRegistrar` before the script.
epics_ca_rs::server::run_ca_ioc_app(app.startup_script("st.cmd")).await
```

## Zigbee2MQTT Builders

Optional device type builders that auto-register topics AND create EPICS records — no `.db` file needed. Each builder knows the Z2M JSON payload structure for its device type.

Z2M-specific behavior:
- ON/OFF normalization on `/set state` topics: `"1"`/`"on"`/`"true"` → `"ON"`, `"0"`/`"off"`/`"false"` → `"OFF"`
- This normalization only applies to Z2M builder topics, not generic MQTT topics

### Startup Script (Z2M)

```bash
mqttZ2mPlug("MQTT1",       "TEST:MQTT:", "LR:Plug",     "zigbee2mqtt/living room plug")
mqttZ2mTempSensor("MQTT1", "TEST:MQTT:", "LR:Sens",     "zigbee2mqtt/living room sensor")
mqttZ2mLight("MQTT1",      "TEST:MQTT:", "Desk",        "zigbee2mqtt/desk light")
mqttZ2mSwitch("MQTT1",     "TEST:MQTT:", "Bath:Light",  "zigbee2mqtt/bathroom light")
mqttZ2mMotion("MQTT1",     "TEST:MQTT:", "ENT:Motion",  "zigbee2mqtt/entrance motion")
mqttZ2mRemote2("MQTT1",    "TEST:MQTT:", "Bath:Sw",     "zigbee2mqtt/bathroom switch")

mqttDriverConfigure("MQTT1", "mqtt://localhost:1883", "epics-mqtt", 1, "TEST:MQTT:Connected")

iocInit()
```

### Z2M Device Types

| Command | Records Created | Fields |
|---------|----------------|--------|
| `mqttZ2mPlug` | ai, ai, longin, stringin, stringout | power, energy, device_temp, state, set_state |
| `mqttZ2mTempSensor` | ai, ai, longin | temperature, humidity, battery |
| `mqttZ2mLight` | longin, longin, stringin, stringout, longout | brightness, color_temp, state, set_state, set_brightness |
| `mqttZ2mSwitch` | stringin, stringout | state, set_state |
| `mqttZ2mMotion` | stringin, longin | occupancy, battery |
| `mqttZ2mRemote2` | stringin, longin | action, battery |

### IOC Binary (with Z2M)

```rust
use mqtt_rs::ioc::register_mqtt_commands;
use mqtt_rs::z2m::register_z2m_commands;

let mut app = IocApplication::new();
app = asyn_rs::adapter::register_asyn_device_support(app);
app = register_mqtt_commands(app, handle, trace);
app = register_z2m_commands(app);  // adds mqttZ2m* commands
```

## iocsh Commands

### Core

| Command | Arguments | Description |
|---------|-----------|-------------|
| `mqttDriverConfigure` | `portName brokerUrl clientId [qos] [connPvName]` | Create driver, connect to broker |

### Z2M Builders

| Command | Arguments | Description |
|---------|-----------|-------------|
| `mqttZ2mPlug` | `port prefix dev topic` | Smart plug (power/energy/temp/state) |
| `mqttZ2mTempSensor` | `port prefix dev topic` | Temp/humidity sensor |
| `mqttZ2mLight` | `port prefix dev topic` | Dimmable light |
| `mqttZ2mSwitch` | `port prefix dev topic` | On/off switch |
| `mqttZ2mMotion` | `port prefix dev topic` | Motion sensor |
| `mqttZ2mRemote2` | `port prefix dev topic` | 2-button remote |

**QoS values:** 0 = at most once, 1 = at least once (default), 2 = exactly once

## Supported Record Types

| asyn Interface | FLAT Types | JSON Types | Direction |
|---------------|------------|------------|-----------|
| asynInt32 | `FLAT:INT` | `JSON:INT` | Read / Write |
| asynFloat64 | `FLAT:FLOAT` | `JSON:FLOAT` | Read / Write |
| asynUInt32Digital | `FLAT:DIGITAL` | `JSON:DIGITAL` | Read / Write |
| asynOctet | `FLAT:STRING` | `JSON:STRING` | Read / Write |
| asynInt32Array | `FLAT:INTARRAY` | — | Read / Write |
| asynFloat64Array | `FLAT:FLOATARRAY` | — | Read / Write |

## Connection Status PV

`mqttDriverConfigure`'s optional 5th argument creates a `bi` record wired to an
internal `_MQTT_CONNECTED` asyn parameter:

```
mqttDriverConfigure("MQTT1", "mqtt://broker:1883", "clientId", 1, "TEST:MQTT:Connected")
```

The record is defined with `ZNAM="Disconnected"`, `ONAM="Connected"`, and
`ZSV="MAJOR"` (disconnect raises a MAJOR alarm). The driver flips the
underlying value based on the MQTT event stream:

| Event | Connected PV |
|-------|--------------|
| `Incoming::ConnAck` (fresh handshake / every reconnect) | `1` |
| `Incoming::Publish` (subscribed topic message) — when flag is `0` | `1` |
| `Incoming::PingResp` (keep-alive response) — when flag is `0` | `1` |
| `eventloop.poll()` returns `Err(_)` (any `ConnectionError`) | `0` |

Publish/PingResp fallbacks exist because some `rumqttc` errors are recoverable
state errors (e.g. packet-id collision) that return `Err` *without* tearing
down the TCP/MQTT session. Without these fallbacks the PV would latch at `0`
even while data keeps flowing. Any subsequent proof-of-life packet (a
subscribed Publish, a PingResp to the keep-alive heartbeat) will automatically
restore the PV to `1`.

## Logging

mqtt-rs emits `tracing` events; they are silent unless your binary installs a
subscriber. The `mqtt-ioc` example does this via `tracing_subscriber::fmt`;
for your own IOC add the same (or any subscriber) to see mqtt-rs output:

| Event | Level | Message |
|-------|-------|---------|
| Reconnect handshake | `info` | `MQTT connected, subscribing to N topics` |
| `poll()` error | `error` | `MQTT connection error: <ConnectionError variant>` |
| Connected=1 restored from Publish/PingResp | `debug` | `MQTT {Publish,PingResp} received while Connected=0 — restoring Connected=1` |

With the default filter `info` you see reconnects and errors. Use
`RUST_LOG=info,mqtt_rs=debug` to additionally see the recovery path (useful
when diagnosing why the Connected PV moved).

## Dependencies

- [rumqttc](https://crates.io/crates/rumqttc) — async MQTT client
- [serde_json](https://crates.io/crates/serde_json) — JSON parsing
- [asyn-rs](../asyn-rs/) — PortDriver framework

## Examples

- [`examples/mqtt-ioc/`](../../examples/mqtt-ioc/) — Z2M device builders demo
- [`examples/mqtt-ioc/db/mqtt.db`](../../examples/mqtt-ioc/db/mqtt.db) — generic MQTT records (no Z2M)
