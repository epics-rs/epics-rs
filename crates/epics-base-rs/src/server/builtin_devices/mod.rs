//! Built-in device support that EPICS base has historically shipped
//! with every IOC.
//!
//! These device support implementations are not protocol-specific —
//! they run inside the generic record processing loop and read or
//! write values directly to the local Rust process state. Users
//! typically don't have to register them by hand; the IOC builder
//! pre-registers each one so a `.db` file can name the DTYP and get
//! the expected behaviour with zero setup.
//!
//! See each submodule for the upstream lineage and the records it
//! applies to.

pub mod getenv;

pub use getenv::GetenvDeviceSupport;
