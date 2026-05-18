# epics-modbus-rs database templates

Standard EPICS record templates for binding records to an
`epics-modbus-rs` driver port (Rust library name `modbus_rs`). These are
ported from `epics-modules/modbus`'s `modbusApp/Db` and are compatible
with the `epics-rs` asyn record device support.

## Macros

Common macros across templates:

- `$(P)$(R)` — record name prefix/suffix.
- `$(PORT)` — the asyn port name passed to `drvModbusAsynConfigure`.
- `$(OFFSET)` — register/coil offset (the asyn `addr`).
- `$(SCAN)` — scan rate; use `I/O Intr` for poller-driven updates.

## drvInfo strings

- `MODBUS_DATA` — use the port's **default** data type (the `dataType`
  argument of `drvModbusAsynConfigure`).
- A data-type string (`UINT16`, `INT32_LE`, `FLOAT32_BE`, `ZSTRING_HIGH`, …)
  — override the data type for that record.
- `READ_OK` / `WRITE_OK` / `IO_ERRORS` / `LAST_IO_TIME` / `MAX_IO_TIME` /
  `POLL_DELAY` / `ENABLE_HISTOGRAM` / `READ_HISTOGRAM` /
  `HISTOGRAM_BIN_TIME` / `HISTOGRAM_TIME_AXIS` — driver statistics and the
  read-time histogram.

## Note on the C `=N` string length

Upstream modbus allows a `drvInfo` suffix like `ZSTRING_HIGH=20` to cap the
string length. `epics-modbus-rs` drops the `=N` suffix (see `ioc.rs`): a
string record's length comes from its own record buffer (`NELM`).
Templates that relied on `=N` shorter than `NELM` should set `NELM` to
the intended length.
