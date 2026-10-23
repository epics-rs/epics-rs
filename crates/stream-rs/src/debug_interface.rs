//! `DebugInterface.cc`: the bus named "debug", a non-blocking example bus.
//! It answers every request from inside the request call: the lock and
//! the write succeed, and every read is one fixed reply.

use std::sync::{Once, Weak};

use crate::bus::{self, Client, StreamBusInterface, StreamIoStatus};
use crate::debug;

/// The reply of every read, its terminating NUL included: C passes
/// `sizeof(input)` (DebugInterface.cc:187-188).
const INPUT: &[u8] = b"Receviced input 3.1415\r\n\0";

/// The [`StreamBusInterface`] [`get_bus_interface`] hands out.
pub struct DebugInterface {
    client: Weak<dyn Client>,
    /// `clientName()`, read once: a request comes with the client's lock
    /// held, and the client's `name()` takes it.
    client_name: String,
}

impl DebugInterface {
    fn client(&self) -> Option<std::sync::Arc<dyn Client>> {
        self.client.upgrade()
    }
}

/// `DebugInterface::getBusInterface`: the bus whose name is "debug", at any
/// address.
pub fn get_bus_interface(
    client: Weak<dyn Client>,
    busname: &str,
    addr: i32,
    _param: &str,
) -> Option<Box<dyn StreamBusInterface>> {
    if busname != "debug" {
        return None;
    }
    let client_name = client.upgrade().map(|c| c.name()).unwrap_or_default();
    let interface = DebugInterface {
        client,
        client_name,
    };
    debug!(format!(
        "DebugInterface::getBusInterface({busname}, {addr}): new Interface allocated\n"
    ));
    Some(Box::new(interface))
}

impl StreamBusInterface for DebugInterface {
    fn lock_request(&self, lock_timeout_ms: u64) -> bool {
        debug!(format!(
            "DebugInterface::lockRequest({}, {lock_timeout_ms} msec)\n",
            self.client_name
        ));
        if let Some(c) = self.client() {
            c.lock_callback(StreamIoStatus::StreamIoSuccess);
        }
        true
    }

    fn unlock(&self) -> bool {
        debug!(format!("DebugInterface::unlock({})\n", self.client_name));
        true
    }

    fn write_request(&self, output: &[u8], write_timeout_ms: u64) -> bool {
        // `%.*s` stops at the first NUL.
        let shown = output.split(|&b| b == 0).next().unwrap_or_default();
        debug!(
            format!("DebugInterface::writeRequest({}, \"", self.client_name),
            shown,
            format!("\", {write_timeout_ms} msec)\n")
        );
        if let Some(c) = self.client() {
            c.write_callback(StreamIoStatus::StreamIoSuccess);
        }
        true
    }

    fn read_request(
        &self,
        reply_timeout_ms: u64,
        read_timeout_ms: u64,
        expected_length: isize,
        async_: bool,
    ) -> bool {
        debug!(format!(
            "DebugInterface::readRequest({}, {reply_timeout_ms} msec reply, \
             {read_timeout_ms} msec read, expect {expected_length} bytes, asyn={})\n",
            self.client_name,
            if async_ { "yes" } else { "no" }
        ));
        if async_ {
            return false;
        }
        if let Some(c) = self.client() {
            c.read_callback(StreamIoStatus::StreamIoEnd, bus::ReadInput::Bytes(INPUT));
        }
        true
    }
}

/// `RegisterStreamBusInterface(DebugInterface)`: once per process, as C's
/// static registrar runs once.
pub fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| bus::register_stream_bus_interface("DebugInterface", get_bus_interface));
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    struct Rec {
        log: Mutex<Vec<String>>,
    }

    impl Client for Rec {
        fn lock_callback(&self, status: StreamIoStatus) {
            self.log.lock().push(format!("lock {}", status.to_str()));
        }
        fn write_callback(&self, status: StreamIoStatus) {
            self.log.lock().push(format!("write {}", status.to_str()));
        }
        fn read_callback(&self, status: StreamIoStatus, input: bus::ReadInput<'_>) -> isize {
            self.log.lock().push(format!(
                "read {} {:?}",
                status.to_str(),
                String::from_utf8_lossy(input.bytes())
            ));
            0
        }
        fn interrupt_accept(&self) -> bool {
            true
        }
        fn get_in_terminator(&self) -> Option<Vec<u8>> {
            None
        }
        fn get_out_terminator(&self) -> Option<Vec<u8>> {
            None
        }
        fn name(&self) -> String {
            "rec".into()
        }
    }

    fn rec() -> (Arc<Rec>, Arc<dyn Client>) {
        let rec = Arc::new(Rec {
            log: Mutex::new(Vec::new()),
        });
        let client: Arc<dyn Client> = rec.clone();
        (rec, client)
    }

    /// DebugInterface.cc:76: only the bus named "debug", whatever the
    /// address and parameter.
    #[test]
    fn only_the_debug_bus_is_ours() {
        let (_rec, client) = rec();
        let weak = Arc::downgrade(&client);
        assert!(get_bus_interface(weak.clone(), "debug", -1, "x").is_some());
        assert!(get_bus_interface(weak.clone(), "Debug", 0, "").is_none());
        assert!(get_bus_interface(weak, "debugX", 0, "").is_none());
    }

    /// DebugInterface.cc:101, :148, :183-188: each answer comes from inside
    /// its request; an async read is refused with no answer.
    #[test]
    fn every_request_is_answered_at_once() {
        let (rec, client) = rec();
        let bus = get_bus_interface(Arc::downgrade(&client), "debug", 0, "").unwrap();
        assert!(bus.lock_request(0));
        assert!(bus.write_request(b"X\r\n", 100));
        assert!(!bus.read_request(1000, 100, 0, true));
        assert!(bus.read_request(1000, 100, 5, false));
        assert!(bus.unlock());
        assert!(!bus.supports_async_read());
        assert!(!bus.supports_event());
        assert_eq!(
            *rec.log.lock(),
            [
                "lock StreamIoSuccess",
                "write StreamIoSuccess",
                "read StreamIoEnd \"Receviced input 3.1415\\r\\n\\0\"",
            ]
        );
    }
}
