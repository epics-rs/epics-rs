//! The record types' `dev*Stream.c`, one module each, and the `device()`
//! lines of `stream-base.dbd` that bind them to `DTYP "stream"`.
//!
//! A record type's file ports C's `readData`, `writeData` and
//! `initRecord` as its [`StreamRecordType`](crate::epics::StreamRecordType)
//! and adds its row to `RECORD_TYPES`; it reaches its record's members
//! through `Record::as_any_mut`.
//!
//! | C source                | module          |
//! |-------------------------|-----------------|
//! | `devaoStream.c`         | [`ao`]          |
//! | `devaiStream.c`         | [`ai`]          |
//! | `devboStream.c`         | [`bo`]          |
//! | `devbiStream.c`         | [`bi`]          |
//! | `devmbboStream.c`       | [`mbbo`]        |
//! | `devmbbiStream.c`       | [`mbbi`]        |
//! | `devmbboDirectStream.c` | [`mbbo_direct`] |
//! | `devmbbiDirectStream.c` | [`mbbi_direct`] |
//! | `devlongoutStream.c`    | [`longout`]     |
//! | `devlonginStream.c`     | [`longin`]      |
//! | `devstringoutStream.c`  | [`stringout`]   |
//! | `devstringinStream.c`   | [`stringin`]    |
//! | `devwaveformStream.c`   | [`waveform`]    |
//! | `devaaiStream.c`        | [`aai`]         |
//! | `devaaoStream.c`        | [`aao`]         |
//! | `devcalcoutStream.c`    | [`calcout`]     |
//! | `devlsiStream.c`        | [`lsi`]         |
//! | `devlsoStream.c`        | [`lso`]         |
//! | `devint64inStream.c`    | [`int64in`]     |
//! | `devint64outStream.c`   | [`int64out`]    |
//! | `devscalcoutStream.c`   | [`scalcout`]    |

use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::ioc_app::{DeviceSupportContext, IocApplication};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::DevSup;

use crate::epics::StreamDevice;

pub mod aai;
pub mod aao;
pub mod ai;
pub mod ao;
pub mod bi;
pub mod bo;
pub mod calcout;
pub mod int64in;
pub mod int64out;
pub mod longin;
pub mod longout;
pub mod lsi;
pub mod lso;
pub mod mbbi;
pub mod mbbi_direct;
pub mod mbbo;
pub mod mbbo_direct;
pub mod scalcout;
pub mod stringin;
pub mod stringout;
pub mod waveform;

/// A record type's row: its name, its `device()` line and its device.
struct RecordType {
    name: &'static str,
    device: &'static DevSup,
    new: fn() -> StreamDevice,
}

/// The rows of `stream-base.dbd`, in its order.
const RECORD_TYPES: &[RecordType] = &[
    RecordType {
        name: "ao",
        device: &ao::DEV_AO_STREAM,
        new: ao::device,
    },
    RecordType {
        name: "ai",
        device: &ai::DEV_AI_STREAM,
        new: ai::device,
    },
    RecordType {
        name: "bo",
        device: &bo::DEV_BO_STREAM,
        new: bo::device,
    },
    RecordType {
        name: "bi",
        device: &bi::DEV_BI_STREAM,
        new: bi::device,
    },
    RecordType {
        name: "mbbo",
        device: &mbbo::DEV_MBBO_STREAM,
        new: mbbo::device,
    },
    RecordType {
        name: "mbbi",
        device: &mbbi::DEV_MBBI_STREAM,
        new: mbbi::device,
    },
    RecordType {
        name: "mbboDirect",
        device: &mbbo_direct::DEV_MBBO_DIRECT_STREAM,
        new: mbbo_direct::device,
    },
    RecordType {
        name: "mbbiDirect",
        device: &mbbi_direct::DEV_MBBI_DIRECT_STREAM,
        new: mbbi_direct::device,
    },
    RecordType {
        name: "longout",
        device: &longout::DEV_LONGOUT_STREAM,
        new: longout::device,
    },
    RecordType {
        name: "longin",
        device: &longin::DEV_LONGIN_STREAM,
        new: longin::device,
    },
    RecordType {
        name: "stringout",
        device: &stringout::DEV_STRINGOUT_STREAM,
        new: stringout::device,
    },
    RecordType {
        name: "stringin",
        device: &stringin::DEV_STRINGIN_STREAM,
        new: stringin::device,
    },
    RecordType {
        name: "waveform",
        device: &waveform::DEV_WAVEFORM_STREAM,
        new: waveform::device,
    },
    RecordType {
        name: "aai",
        device: &aai::DEV_AAI_STREAM,
        new: aai::device,
    },
    RecordType {
        name: "aao",
        device: &aao::DEV_AAO_STREAM,
        new: aao::device,
    },
    RecordType {
        name: "calcout",
        device: &calcout::DEV_CALCOUT_STREAM,
        new: calcout::device,
    },
    RecordType {
        name: "lsi",
        device: &lsi::DEV_LSI_STREAM,
        new: lsi::device,
    },
    RecordType {
        name: "lso",
        device: &lso::DEV_LSO_STREAM,
        new: lso::device,
    },
    RecordType {
        name: "int64in",
        device: &int64in::DEV_INT64IN_STREAM,
        new: int64in::device,
    },
    RecordType {
        name: "int64out",
        device: &int64out::DEV_INT64OUT_STREAM,
        new: int64out::device,
    },
    RecordType {
        name: "scalcout",
        device: &scalcout::DEV_SCALCOUT_STREAM,
        new: scalcout::device,
    },
];

/// The `device()` lines, as `register_dynamic_device_support` takes them.
pub fn devices() -> Vec<(&'static str, &'static [DevSup])> {
    RECORD_TYPES
        .iter()
        .map(|t| (t.name, std::slice::from_ref(t.device)))
        .collect()
}

/// The device of a record whose `DTYP` is a stream row's.
pub fn device_support_factory(ctx: &DeviceSupportContext) -> Option<Box<dyn DeviceSupport>> {
    let t = RECORD_TYPES
        .iter()
        .find(|t| t.name == ctx.record_type && t.device.choice == ctx.dtyp)?;
    Some(Box::new((t.new)()))
}

/// Load `stream-base.dbd`'s `device()` lines into `builder`.
pub fn register(builder: IocBuilder) -> IocBuilder {
    crate::epics::register_driver();
    builder.register_dynamic_device_support(&devices(), device_support_factory)
}

/// `streamApp.dbd` on an IOC: [`register`]'s `device()` lines, the bus
/// interfaces in CONFIG_STREAM's `BUSSES` order (C's registrars run in link
/// order), and the iocsh commands of `streamRegistrar` and
/// `AsynDriverInterfaceRegistrar`.
pub fn register_app(app: IocApplication) -> IocApplication {
    crate::debug_interface::register();
    crate::dummy::register();
    crate::asyn::register();
    crate::epics::register_driver();
    let app = app.register_dynamic_device_support(&devices(), device_support_factory);
    crate::epics::iocsh_commands()
        .into_iter()
        .chain(crate::asyn::iocsh_commands())
        .fold(app, IocApplication::register_startup_command)
}
