# modbus-ioc

Example IOC demonstrating the [`modbus-rs`](../../crates/modbus-rs) driver —
the Rust port of EPICS `modbus` / `drvModbusAsyn`.

## Run

A reachable Modbus/TCP server is required. Start a simulator first:

```sh
pip install pymodbus
pymodbus.simulator                 # listens on 127.0.0.1:502
# or:  diagslave -m tcp
```

Then run the IOC:

```sh
cargo run --release -p modbus-ioc --bin modbus_ioc --features ioc -- ioc/st.cmd
```

## What it does

`ioc/st.cmd`:

1. `drvAsynIPPortConfigure` — opens the TCP octet port to the server.
2. `modbusInterposeConfig` — selects Modbus/TCP framing (link type 0).
3. `drvModbusAsynConfigure` — two driver ports: a holding-register **read**
   port (function 3, polled every 100 ms) and a **write** port (function 16).
4. `dbLoadRecords` — loads `db/modbus.db`.

`db/modbus.db` binds records to register offsets via
`@asyn(<port> <offset>) <dataType>`, showing 16-bit, 32-bit big-endian,
and `FLOAT32_BE` decodings plus the driver's `READ_OK` / `IO_ERRORS`
statistics.

## Try it

```sh
camonitor TEST:MB:Reg0 TEST:MB:Reg1 TEST:MB:RegF
caput     TEST:MB:SetReg0 1234
caget     TEST:MB:ReadOK TEST:MB:IOErrors
```
