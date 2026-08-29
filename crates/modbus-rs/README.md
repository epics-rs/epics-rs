# epics-modbus-rs

Rust port of the EPICS [`modbus`](https://github.com/epics-modules/modbus)
module — a Modbus TCP/RTU/ASCII driver for `epics-rs`.

> The crate is published on crates.io as `epics-modbus-rs` (the bare
> `modbus-rs` name was already taken by an unrelated project), but the
> Rust library name is `modbus_rs`, so consumers write
> `use modbus_rs::...`.

This is the Rust equivalent of `drvModbusAsyn`: it layers Modbus protocol
framing on top of an `asyn-rs` octet port (IP or serial) and exposes the
PLC register/coil space through the standard asyn interfaces.

## Layers

- `protocol` — Modbus function codes, MBAP header, request/response PDUs.
- `interpose` — link-layer framing: Modbus/TCP (MBAP), RTU (CRC-16),
  ASCII (LRC), built on the `asyn-rs` octet interpose framework.
- `datatype` — the 28 `modbusDataType_t` encodings (INT16, sign-magnitude,
  BCD, 32/64-bit LE/BE with byte-swap variants, FLOAT32/64, strings).
- `driver` — `DrvModbusAsyn`: the read poller, `do_modbus_io`, absolute
  addressing, and I/O statistics / time histogram.
- `ioc` (feature `ioc`) — record and device-support binding.

## Status

Port in progress.
