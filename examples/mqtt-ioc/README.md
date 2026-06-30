# MQTT IOC Example

Demonstrates the [mqtt-rs](../../crates/mqtt-rs/) driver with Channel Access.

Two usage modes:
- **Z2M builders** (`st.cmd`) — auto-create records for Zigbee2MQTT devices
- **Generic MQTT** (`db/mqtt.db`) — manual records for any MQTT broker/topic

## Prerequisites

An MQTT broker on `localhost:1883`:

```bash
# macOS
brew install mosquitto && mosquitto -v

# Docker
docker run -p 1883:1883 eclipse-mosquitto

# Linux
sudo apt install mosquitto && mosquitto -v
```

## Build and Run

```bash
cargo build --release -p mqtt-ioc --features ioc

./target/release/mqtt_ioc examples/mqtt-ioc/ioc/st.cmd
```

### Logging

The binary installs a `tracing_subscriber` controlled by `RUST_LOG`
(default: `info`):

```bash
# Default — reconnects (info) and errors (error) only
./target/release/mqtt_ioc examples/mqtt-ioc/ioc/st.cmd

# Also show Connected PV recovery path (Publish/PingResp restoring Connected=1)
RUST_LOG=info,mqtt_rs=debug ./target/release/mqtt_ioc examples/mqtt-ioc/ioc/st.cmd
```

Useful lines to watch for:

- `MQTT connection error: <variant>` — the `rumqttc` `ConnectionError`
  variant is what tells you whether a disconnect was a real network
  failure vs. a recoverable state error.
- `MQTT connected, subscribing to N topics` — logged on every fresh
  ConnAck (initial connect and every reconnect).

## Z2M Builder Example

The default `st.cmd` uses Z2M device type builders. Each line registers MQTT topics and creates EPICS records automatically:

```bash
mqttZ2mPlug("MQTT1",       "TEST:MQTT:", "LR:Plug",  "zigbee2mqtt/living room plug")
mqttZ2mTempSensor("MQTT1", "TEST:MQTT:", "LR:Sens",  "zigbee2mqtt/living room sensor")
mqttZ2mLight("MQTT1",      "TEST:MQTT:", "BR1:Desk", "zigbee2mqtt/desk light")
mqttDriverConfigure("MQTT1", "mqtt://localhost:1883", "epics-mqtt-ioc", 1, "TEST:MQTT:Connected")
iocInit()
```

### Test Z2M devices

```bash
# Monitor connection status
caget TEST:MQTT:Connected

# Read sensor values
caget TEST:MQTT:LR:Sens:Temp
caget TEST:MQTT:LR:Sens:Hum

# Control plug (accepts ON/OFF/1/0/on/off/true/false)
caput TEST:MQTT:LR:Plug:SetState ON
caput TEST:MQTT:LR:Plug:SetState 0

# Control light brightness
caput TEST:MQTT:BR1:Desk:SetBright 128
caput TEST:MQTT:BR1:Desk:SetState OFF

# Monitor power usage
camonitor TEST:MQTT:LR:Plug:Power TEST:MQTT:BR1:Plug:Power
```

## Generic MQTT Example

For non-Z2M topics, load a `.db` file whose record links carry the topic
addresses. Topics are created on demand as records bind — there is no separate
registration step. See `db/mqtt.db` for examples.

```bash
# Create driver
mqttDriverConfigure("MQTT1", "mqtt://localhost:1883", "epics-client", 1)

# Load records — each INP/OUT link's `FORMAT:TYPE topic` creates its topic
dbLoadRecords("db/mqtt.db", "P=TEST:,R=MQTT:,PORT=MQTT1")

iocInit()
```

### Test generic MQTT

```bash
# Publish a value, read via CA
mosquitto_pub -t sensors/temperature -m "25.3"
caget TEST:MQTT:Example:Temperature

# Publish JSON, read extracted field
mosquitto_pub -t sensors/environment -m '{"humidity": 65.2}'
caget TEST:MQTT:Example:Humidity

# Monitor updates
camonitor TEST:MQTT:Example:Temperature TEST:MQTT:Example:Humidity
```
