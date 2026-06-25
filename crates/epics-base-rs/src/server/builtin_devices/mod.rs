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
pub mod stdio;
pub mod timestamp;

pub use getenv::GetenvDeviceSupport;
pub use stdio::StdioDeviceSupport;
pub use timestamp::SoftTimestampDeviceSupport;

use crate::server::device_support::DeviceSupport;
use crate::server::ioc_app::DeviceSupportContext;

/// Built-in device support that must be dispatched by the runtime
/// [`DeviceSupportContext`] because it needs the record's `INP`/`OUT`.
///
/// `Soft Timestamp` (base `devTimestamp.c`) needs its INST_IO `INP`
/// strftime format string and `stdio` (base `devStdio.c`) needs its INST_IO
/// `OUT` stream name — neither of which the static `register_device_support`
/// factory (a `Fn() -> Box<dyn DeviceSupport>`) can see; only the context
/// carries them. Both [`IocBuilder::new`](crate::server::IocBuilder) and
/// [`IocApplication::new`](crate::server::IocApplication) pre-register this as
/// the *base* of the dynamic-factory chain, so a user's
/// `register_dynamic_device_support` factory takes priority and falls through
/// to here for the built-in DTYPs.
pub fn builtin_dynamic_factory(ctx: &DeviceSupportContext) -> Option<Box<dyn DeviceSupport>> {
    match ctx.dtyp {
        "Soft Timestamp" => Some(Box::new(timestamp::SoftTimestampDeviceSupport::new(
            ctx.inp,
        ))),
        "stdio" => Some(Box::new(stdio::StdioDeviceSupport::new(ctx.out))),
        _ => None,
    }
}
