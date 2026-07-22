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

pub mod dbstate;
pub mod getenv;
pub mod stdio;
pub mod timestamp;

pub use dbstate::DbStateDeviceSupport;
pub use getenv::GetenvDeviceSupport;
pub use stdio::StdioDeviceSupport;
pub use timestamp::SoftTimestampDeviceSupport;

use crate::server::device_support::DeviceSupport;
use crate::server::ioc_app::DeviceSupportContext;

/// Built-in device support that must be dispatched by the runtime
/// [`DeviceSupportContext`] because it needs the record's `INP`/`OUT`.
///
/// `Soft Timestamp` (base `devTimestamp.c`) needs its INST_IO `INP` strftime
/// format string, `stdio` (base `devStdio.c`) needs its INST_IO `OUT` stream
/// name, `Db State` (base `devBiDbState.c` / `devBoDbState.c`) needs its
/// INST_IO `INP` (bi) / `OUT` (bo) state name, and `getenv` (base
/// `devSiEnviron.c` / `devLsiEnviron.c`) needs its INST_IO `INP` env-var name —
/// none of which a context-free static `Fn() -> Box<dyn DeviceSupport>` factory
/// can see. The inner typed record handed to `init`/`read` does NOT expose
/// `INP`/`OUT` either (those live on the `RecordInstance` common header, not the
/// record body), so every base builtin is dispatched here and receives the link
/// at construction. Both [`IocBuilder::new`](crate::server::ioc_builder::IocBuilder::new) and
/// [`IocApplication::new`](crate::server::ioc_app::IocApplication::new) pre-register this as
/// the *base* of the dynamic-factory chain, so a user's
/// `register_dynamic_device_support` factory takes priority and falls through
/// to here for the built-in DTYPs.
pub fn builtin_dynamic_factory(ctx: &DeviceSupportContext) -> Option<Box<dyn DeviceSupport>> {
    match ctx.dtyp {
        "Soft Timestamp" => Some(Box::new(timestamp::SoftTimestampDeviceSupport::new(
            ctx.inp,
        ))),
        "stdio" => Some(Box::new(stdio::StdioDeviceSupport::new(ctx.out))),
        "Db State" => Some(Box::new(dbstate::DbStateDeviceSupport::new(
            ctx.inp, ctx.out,
        ))),
        "getenv" => Some(Box::new(getenv::GetenvDeviceSupport::new(ctx.inp))),
        _ => None,
    }
}
