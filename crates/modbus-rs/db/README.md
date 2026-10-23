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
- `MODBUS_READ` (`poll_trigger.template`) — writing it runs one poll cycle
  before the write completes; C only wakes the poller.

## `=N` string length

A string data type takes an optional `=N` suffix (`ZSTRING_HIGH=20`) that caps
the record's octet length, as in C. The suffix is refused on a non-string type
or when `N` is not a non-negative integer, and record init then fails.
