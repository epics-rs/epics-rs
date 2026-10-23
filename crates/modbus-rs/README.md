# epics-modbus-rs

Rust port of the EPICS [`modbus`](https://github.com/epics-modules/modbus)
module (`R3-4-10-gb1009d0`) — a Modbus TCP/RTU/ASCII driver for `epics-rs`.

> The crate is published on crates.io as `epics-modbus-rs` (the bare
> `modbus-rs` name was already taken by an unrelated project), but the
> Rust library name is `modbus_rs`, so consumers write
> `use modbus_rs::...`.

This is the Rust equivalent of `drvModbusAsyn`: it layers Modbus protocol
framing on top of an `asyn-rs` octet port (IP or serial) and exposes the
PLC register/coil space through the standard asyn interfaces.

## Layers

- `protocol` — Modbus function codes, MBAP header, request/response PDUs.
- `interpose` — link-layer framing (`modbusInterpose.c`): Modbus/TCP and
  UDP (MBAP), RTU (CRC-16), ASCII (LRC).
- `datatype` — all 37 `modbusDataType_t` encodings (INT16, sign-magnitude,
  BCD, 32/64-bit LE/BE with byte-swap variants, FLOAT32/64, the four string
  and four zero-terminated-string orders).
- `driver` — `ModbusEngine`, the transport-agnostic core of `drvModbusAsyn`
  over an `OctetTransport`: the read poll, `do_modbus_io`, absolute addressing
  (`modbusStartAddress` -1), and I/O statistics / time histogram.
- `ioc` (feature `ioc`) — `ModbusPortDriver`, the asyn-rs `PortDriver` over
  `ModbusEngine`; `SyncIoTransport` onto an asyn-rs octet port; and the
  `modbusInterposeConfig` / `drvModbusAsynConfigure` iocsh commands.

## Features

| feature | what it adds |
|---|---|
| `ioc` (off by default) | the `ioc` module, with `asyn-rs`, `epics-base-rs`, `epics-ca-rs` and `tokio`; without it the crate builds the protocol and driver layers only |

## Usage

Register the commands next to the asyn ones
(`examples/modbus-ioc/src/bin/modbus_ioc.rs`):

```rust
let handle = epics_base_rs::runtime::task::runtime_handle();
app = asyn_rs::adapter::register_asyn_device_support(app);
app = modbus_rs::ioc::register_modbus_commands(app, handle);
```

and configure the ports as in C (`examples/modbus-ioc/ioc/st.cmd`):

```
drvAsynIPPortConfigure("MBTCP1", "127.0.0.1:502")
# link type: 0=TCP 1=RTU 2=ASCII 3=UDP
modbusInterposeConfig("MBTCP1", 0, 1000, 0)
# port, octetPort, slave, function, startAddr, length, dataType, pollMsec, plcType
drvModbusAsynConfigure("MB:HR", "MBTCP1", 0, 3, 0, 10, "UINT16", 100, "")
```

The record templates ported from `modbusApp/Db` are in `db/`.

## Not ported

`testModbusSyncIO.cpp` (`testModbusSyncIOConfigure`) and the `testClient.cpp`
program.
