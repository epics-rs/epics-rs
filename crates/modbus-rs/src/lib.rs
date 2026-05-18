//! # epics-modbus-rs
//!
//! Rust port of the EPICS [`modbus`](https://github.com/epics-modules/modbus)
//! module — a Modbus TCP/RTU/ASCII driver for `epics-rs`.
//!
//! Published on crates.io as `epics-modbus-rs`; the Rust library name is
//! `modbus_rs`, so consumers write `use modbus_rs::...`.
//!
//! This is the equivalent of `drvModbusAsyn`: it layers Modbus protocol
//! framing on top of an `asyn-rs` octet port and exposes a PLC's register
//! and coil space through the standard asyn interfaces.
//!
//! ## Module layout
//!
//! - [`protocol`] — function codes, the MBAP header, request/response PDUs.
//! - [`interpose`] — link-layer framing (MBAP / RTU CRC-16 / ASCII LRC).
//! - [`error`] — error and exception types.
//!
//! Further layers (`datatype` for the 28 `modbusDataType_t` encodings,
//! `driver` for the read poller, and the `ioc` feature for record binding)
//! are added as the port progresses.

#![warn(missing_docs)]

pub mod datatype;
pub mod driver;
pub mod error;
pub mod interpose;
#[cfg(feature = "ioc")]
pub mod ioc;
pub mod protocol;

pub use datatype::ModbusDataType;
pub use driver::{IoStatistics, ModbusConfig, ModbusEngine, ModbusFunctionCode, OctetTransport};
pub use error::{ExceptionCode, ModbusError, ModbusResult};
pub use interpose::{LinkType, ModbusFramer, UnwrappedResponse};
pub use protocol::{FunctionCode, MbapHeader, RequestPdu, ResponsePdu};
