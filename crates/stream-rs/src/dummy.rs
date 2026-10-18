//! `DummyInterface.cc`: the debug and example bus, with no I/O. Every
//! request is refused, so a record on it ends offline.

use std::sync::{Once, Weak};

use crate::bus::{self, Client, StreamBusInterface};

/// `DummyInterface`
struct DummyInterface;

impl StreamBusInterface for DummyInterface {
    fn lock_request(&self, _lock_timeout_ms: u64) -> bool {
        false
    }
    fn unlock(&self) -> bool {
        false
    }
}

/// `getBusInterface(client, busname, addr, param)`: the bus named "dummy",
/// at any address.
fn get_bus_interface(
    _client: Weak<dyn Client>,
    busname: &str,
    _addr: i32,
    _param: &str,
) -> Option<Box<dyn StreamBusInterface>> {
    (busname == "dummy").then(|| Box::new(DummyInterface) as Box<dyn StreamBusInterface>)
}

/// `RegisterStreamBusInterface(DummyInterface)`: once per process, as C's
/// static registrar runs once.
pub fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| bus::register_stream_bus_interface("DummyInterface", get_bus_interface));
}
