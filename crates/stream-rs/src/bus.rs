//! `StreamBusInterface.h` / `StreamBusInterface.cc`: the boundary between a
//! stream (the client) and the bus driver that moves its bytes.
//!
//! The client asks; the bus answers through the client's callbacks:
//!
//! | request              | answer                                  |
//! |----------------------|-----------------------------------------|
//! | `lock_request`       | `lock_callback`                         |
//! | `write_request`      | `write_callback`                        |
//! | `read_request`       | `read_callback` (repeatedly)            |
//! | `accept_event`       | `event_callback`                        |
//! | `connect_request`    | `connect_callback`                      |
//! | `disconnect_request` | `disconnect_callback`                   |
//!
//! A request returns false when it cannot be started; the callback then never
//! comes. A callback may come from another thread, or from inside the
//! request call itself, so callbacks take `&self` and the client does its
//! own locking (StreamCore's recursive `lockMutex`), and a bus holds only a
//! [`Weak`] reference to its client. `read_callback` returns how many more
//! bytes the client expects: 0 when it is done, a positive count, or -1 for
//! "unknown, keep reading".

use std::sync::{Arc, Weak};

use parking_lot::RwLock;

use crate::buffer::StreamBuffer;

/// `ENUM (StreamIoStatus, ...)`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamIoStatus {
    StreamIoSuccess,
    StreamIoTimeout,
    StreamIoNoReply,
    StreamIoEnd,
    StreamIoFault,
}

impl StreamIoStatus {
    /// `toStr`
    pub fn to_str(self) -> &'static str {
        match self {
            Self::StreamIoSuccess => "StreamIoSuccess",
            Self::StreamIoTimeout => "StreamIoTimeout",
            Self::StreamIoNoReply => "StreamIoNoReply",
            Self::StreamIoEnd => "StreamIoEnd",
            Self::StreamIoFault => "StreamIoFault",
        }
    }
}

/// What a read hands its client: C's `(input, size)` of `readCallback`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadInput<'a> {
    /// `size` bytes at `input`, none when there is no data (timeout, fault).
    Bytes(&'a [u8]),
    /// A negative `size`: that many bytes an earlier call delivered are taken
    /// back, and `input` is not read. asyn's EOS layer sends -1 when a
    /// two-byte terminator straddles reads (asynInterposeEos.c:201-204).
    TakeBack(usize),
}

impl<'a> ReadInput<'a> {
    /// C's signed `size`.
    pub fn size(self) -> isize {
        match self {
            Self::Bytes(b) => b.len() as isize,
            Self::TakeBack(n) => -(n as isize),
        }
    }

    /// The bytes C's `StreamBuffer(input, size)` holds: none for a take-back.
    pub fn bytes(self) -> &'a [u8] {
        match self {
            Self::Bytes(b) => b,
            Self::TakeBack(_) => &[],
        }
    }

    /// A copy that outlives the bus's buffer.
    pub fn to_owned(self) -> OwnedReadInput {
        match self {
            Self::Bytes(b) => OwnedReadInput::Bytes(b.to_vec()),
            Self::TakeBack(n) => OwnedReadInput::TakeBack(n),
        }
    }
}

/// [`ReadInput`] holding its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnedReadInput {
    Bytes(Vec<u8>),
    TakeBack(usize),
}

impl OwnedReadInput {
    pub fn as_input(&self) -> ReadInput<'_> {
        match self {
            Self::Bytes(b) => ReadInput::Bytes(b),
            Self::TakeBack(n) => ReadInput::TakeBack(*n),
        }
    }
}

/// `StreamBusInterface::Client`
pub trait Client: Send + Sync {
    fn lock_callback(&self, status: StreamIoStatus);
    fn write_callback(&self, _status: StreamIoStatus) {}
    fn read_callback(&self, _status: StreamIoStatus, _input: ReadInput<'_>) -> isize {
        0
    }
    fn event_callback(&self, _status: StreamIoStatus) {}
    fn connect_callback(&self, _status: StreamIoStatus) {}
    /// `connectCallback()` for a connect no request asked for
    /// (AsynDriverInterface.cc:1283-1284), which belongs to no run.
    fn async_connect_callback(&self, status: StreamIoStatus) {
        self.connect_callback(status)
    }
    fn disconnect_callback(&self, _status: StreamIoStatus) {}
    fn priority(&self) -> i64 {
        0
    }
    /// The IOC's `interruptAccept`, before which asynchronous input is
    /// dropped (AsynDriverInterface.cc:1110).
    fn interrupt_accept(&self) -> bool;
    /// `getInTerminator`: `None` when no terminator is defined.
    fn get_in_terminator(&self) -> Option<Vec<u8>>;
    /// `getOutTerminator`: `None` when no terminator is defined.
    fn get_out_terminator(&self) -> Option<Vec<u8>>;
    fn name(&self) -> String;
}

/// `StreamBusInterface`: what a bus driver implements. The defaults are
/// the C base class's: nothing supported.
pub trait StreamBusInterface: Send + Sync {
    fn write_request(&self, _output: &[u8], _timeout_ms: u64) -> bool {
        false
    }
    fn read_request(
        &self,
        _replytimeout_ms: u64,
        _readtimeout_ms: u64,
        _expected_length: isize,
        _async_: bool,
    ) -> bool {
        false
    }
    fn supports_event(&self) -> bool {
        false
    }
    fn supports_async_read(&self) -> bool {
        false
    }
    /// Implement when `supports_event` returns true.
    fn accept_event(&self, _mask: u64, _replytimeout_ms: u64) -> bool {
        false
    }
    /// `release`: the client is done with the bus. C deletes the object;
    /// here the [`BusInterface`] is dropped right after this returns.
    fn release(&self) {}
    fn connect_request(&self, _connecttimeout_ms: u64) -> bool {
        false
    }
    fn disconnect_request(&self) -> bool {
        false
    }
    fn finish(&self) {}
    fn print_status(&self, _buffer: &mut StreamBuffer) {}
    fn lock_request(&self, timeout_ms: u64) -> bool;
    fn unlock(&self) -> bool;
}

/// A bus attached to a client by [`find`], with the name C sets on it:
/// `"<busname> <addr>"`, or `"<busname>"` for a negative address.
pub struct BusInterface {
    name: String,
    bus: Box<dyn StreamBusInterface>,
}

impl BusInterface {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn bus(&self) -> &dyn StreamBusInterface {
        &*self.bus
    }
}

/// `C::getBusInterface(client, busname, addr, param)`: a bus driver's
/// constructor, returning `None` when the bus is not one of its own.
pub type GetBusInterface =
    fn(Weak<dyn Client>, &str, i32, &str) -> Option<Box<dyn StreamBusInterface>>;

struct Registrar {
    name: &'static str,
    find: GetBusInterface,
}

static REGISTRARS: RwLock<Vec<Registrar>> = RwLock::new(Vec::new());

/// `RegisterStreamBusInterface(interface)`: registrars are asked in
/// registration order.
pub fn register_stream_bus_interface(name: &'static str, find: GetBusInterface) {
    REGISTRARS.write().push(Registrar { name, find });
}

/// `StreamBusInterfaceClass`: the registered interface names, in order.
pub fn stream_bus_interface_names() -> Vec<&'static str> {
    REGISTRARS.read().iter().map(|r| r.name).collect()
}

/// `StreamBusInterface::find`: the first registered bus driver that takes
/// `busname`.
pub fn find(
    client: &Arc<dyn Client>,
    busname: &str,
    addr: i32,
    param: &str,
) -> Option<BusInterface> {
    crate::debug!(format!(
        "StreamBusInterface::find({}, {}, {}, \"{}\")\n",
        client.name(),
        busname,
        addr,
        param
    ));
    let registrars: Vec<(&'static str, GetBusInterface)> =
        REGISTRARS.read().iter().map(|r| (r.name, r.find)).collect();
    for (name, get) in registrars {
        crate::debug!(format!(
            "StreamBusInterface::find {} check {}\n",
            client.name(),
            name
        ));
        let bus = get(Arc::downgrade(client), busname, addr, param);
        crate::debug!(format!(
            "StreamBusInterface::find {} {}\n",
            name,
            if bus.is_some() {
                "matches"
            } else {
                "does not match"
            }
        ));
        if let Some(bus) = bus {
            let name = if addr >= 0 {
                format!("{busname} {addr}")
            } else {
                busname.to_string()
            };
            return Some(BusInterface { name, bus });
        }
    }
    None
}

/// The client side helpers (`busSupportsEvent` ...), on the client's
/// optional bus: each is false / a no-op while no bus is attached. The
/// client calls them on its own reference to the bus, so a callback that
/// comes from inside the request finds the client's bus slot unborrowed.
pub trait ClientBus {
    fn bus_supports_event(&self) -> bool;
    fn bus_supports_async_read(&self) -> bool;
    fn bus_accept_event(&self, mask: u64, replytimeout_ms: u64) -> bool;
    /// `busRelease`: release and drop the bus.
    fn bus_release(&mut self);
    fn bus_lock_request(&self, timeout_ms: u64) -> bool;
    fn bus_unlock(&self) -> bool;
    fn bus_write_request(&self, output: &[u8], timeout_ms: u64) -> bool;
    fn bus_read_request(
        &self,
        replytimeout_ms: u64,
        readtimeout_ms: u64,
        expected_length: isize,
        async_: bool,
    ) -> bool;
    fn bus_finish(&self);
    fn bus_connect_request(&self, timeout_ms: u64) -> bool;
    fn bus_disconnect(&self) -> bool;
    fn bus_print_status(&self, buffer: &mut StreamBuffer);
}

impl ClientBus for Option<Arc<BusInterface>> {
    fn bus_supports_event(&self) -> bool {
        self.as_ref().is_some_and(|b| b.bus.supports_event())
    }
    fn bus_supports_async_read(&self) -> bool {
        self.as_ref().is_some_and(|b| b.bus.supports_async_read())
    }
    fn bus_accept_event(&self, mask: u64, replytimeout_ms: u64) -> bool {
        self.as_ref()
            .is_some_and(|b| b.bus.accept_event(mask, replytimeout_ms))
    }
    fn bus_release(&mut self) {
        if let Some(b) = self.take() {
            b.bus.release();
        }
    }
    fn bus_lock_request(&self, timeout_ms: u64) -> bool {
        self.as_ref()
            .is_some_and(|b| b.bus.lock_request(timeout_ms))
    }
    fn bus_unlock(&self) -> bool {
        self.as_ref().is_some_and(|b| b.bus.unlock())
    }
    fn bus_write_request(&self, output: &[u8], timeout_ms: u64) -> bool {
        self.as_ref()
            .is_some_and(|b| b.bus.write_request(output, timeout_ms))
    }
    fn bus_read_request(
        &self,
        replytimeout_ms: u64,
        readtimeout_ms: u64,
        expected_length: isize,
        async_: bool,
    ) -> bool {
        self.as_ref().is_some_and(|b| {
            b.bus
                .read_request(replytimeout_ms, readtimeout_ms, expected_length, async_)
        })
    }
    fn bus_finish(&self) {
        if let Some(b) = self {
            b.bus.finish();
        }
    }
    fn bus_connect_request(&self, timeout_ms: u64) -> bool {
        self.as_ref()
            .is_some_and(|b| b.bus.connect_request(timeout_ms))
    }
    fn bus_disconnect(&self) -> bool {
        self.as_ref().is_some_and(|b| b.bus.disconnect_request())
    }
    fn bus_print_status(&self, buffer: &mut StreamBuffer) {
        if let Some(b) = self {
            b.bus.print_status(buffer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    struct Rec {
        log: Mutex<Vec<String>>,
    }

    impl Client for Rec {
        fn lock_callback(&self, status: StreamIoStatus) {
            self.log.lock().push(format!("lock {}", status.to_str()));
        }
        fn interrupt_accept(&self) -> bool {
            true
        }
        fn get_in_terminator(&self) -> Option<Vec<u8>> {
            None
        }
        fn get_out_terminator(&self) -> Option<Vec<u8>> {
            Some(b"\r\n".to_vec())
        }
        fn name(&self) -> String {
            "rec".into()
        }
    }

    /// Answers the lock from inside the request, as a synchronous bus may.
    struct SyncBus {
        client: Weak<dyn Client>,
    }

    impl StreamBusInterface for SyncBus {
        fn lock_request(&self, _timeout_ms: u64) -> bool {
            if let Some(c) = self.client.upgrade() {
                c.lock_callback(StreamIoStatus::StreamIoSuccess);
            }
            true
        }
        fn unlock(&self) -> bool {
            true
        }
    }

    fn get_sync(
        client: Weak<dyn Client>,
        busname: &str,
        _addr: i32,
        _param: &str,
    ) -> Option<Box<dyn StreamBusInterface>> {
        (busname == "sync").then(|| Box::new(SyncBus { client }) as Box<dyn StreamBusInterface>)
    }

    fn get_none(
        _client: Weak<dyn Client>,
        _busname: &str,
        _addr: i32,
        _param: &str,
    ) -> Option<Box<dyn StreamBusInterface>> {
        None
    }

    #[test]
    fn find_asks_registrars_in_order_and_names_the_bus() {
        register_stream_bus_interface("None", get_none);
        register_stream_bus_interface("Sync", get_sync);
        assert_eq!(stream_bus_interface_names(), ["None", "Sync"]);
        let rec = Arc::new(Rec {
            log: Mutex::new(Vec::new()),
        });
        let client: Arc<dyn Client> = rec.clone();
        assert!(find(&client, "other", 0, "").is_none());
        let mut bus = find(&client, "sync", 3, "").map(Arc::new);
        assert_eq!(bus.as_ref().unwrap().name(), "sync 3");
        assert_eq!(find(&client, "sync", -1, "").unwrap().name(), "sync");
        assert!(bus.bus_lock_request(100));
        assert_eq!(*rec.log.lock(), ["lock StreamIoSuccess"]);
        // The base class supports none of the optional requests.
        assert!(!bus.bus_write_request(b"x", 100));
        assert!(!bus.bus_supports_event());
        bus.bus_release();
        assert!(!bus.bus_lock_request(100));
    }

    #[test]
    fn a_take_back_is_cs_negative_size_and_no_bytes() {
        let back = ReadInput::TakeBack(1);
        assert_eq!((back.size(), back.bytes()), (-1, &[][..]));
        assert_eq!(back.to_owned().as_input(), back);
        let bytes = ReadInput::Bytes(b"\r");
        assert_eq!((bytes.size(), bytes.bytes()), (1, &b"\r"[..]));
        assert_eq!(bytes.to_owned().as_input(), bytes);
    }
}
