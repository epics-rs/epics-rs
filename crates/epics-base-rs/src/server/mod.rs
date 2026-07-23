pub mod access_security;
pub mod autosave;
pub mod builtin_devices;
pub mod cvt_bpt;
pub mod database;
pub mod db_loader;
pub mod device_support;
pub mod event_queue;
pub mod ioc_app;
pub mod ioc_builder;
pub mod iocsh;
pub mod pv;
pub mod recgbl;
pub mod record;
pub mod records;
pub mod scan;
pub mod snapshot;
pub mod status_pv;

use crate::server::record::Record;

/// Factory function type for creating device support instances.
pub type DeviceSupportFactory =
    Box<dyn Fn() -> Box<dyn device_support::DeviceSupport> + Send + Sync>;

/// Factory function type for creating record instances.
pub type RecordFactory = Box<dyn Fn() -> Box<dyn Record> + Send + Sync>;
