//! Synchronous convenience API for port driver I/O.
//!
//! All methods acquire the `Mutex<dyn PortDriver>` lock, because `PortDriver`
//! methods require `&mut self`. The lock is shared with the broader port driver
//! infrastructure (adapter, worker, auto-connect) via `Arc<Mutex<dyn PortDriver>>`.

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::error::AsynResult;
use crate::manager::PortManager;
use crate::param::EnumEntry;
use crate::port::PortDriver;
use crate::port_handle::PortHandle;
use crate::request::RequestOp;
use crate::user::AsynUser;

/// Synchronous I/O handle bound to a specific port and address.
///
/// All operations acquire the port mutex lock.
pub struct SyncIO {
    port: Arc<Mutex<dyn PortDriver>>,
    addr: i32,
    timeout: Duration,
}

impl SyncIO {
    /// Connect to a named port via the PortManager.
    pub fn connect(manager: &PortManager, port_name: &str, addr: i32, timeout: Duration) -> AsynResult<Self> {
        let port = manager.find_port(port_name)?;
        Ok(Self { port, addr, timeout })
    }

    /// Create directly from a port reference (for testing or when PortManager is not used).
    pub fn from_port(port: Arc<Mutex<dyn PortDriver>>, addr: i32, timeout: Duration) -> Self {
        Self { port, addr, timeout }
    }

    fn user(&self, reason: usize) -> AsynUser {
        AsynUser::new(reason)
            .with_addr(self.addr)
            .with_timeout(self.timeout)
    }

    pub fn read_int32(&self, reason: usize) -> AsynResult<i32> {
        let mut port = self.port.lock();
        port.read_int32(&self.user(reason))
    }

    pub fn write_int32(&self, reason: usize, value: i32) -> AsynResult<()> {
        let mut port = self.port.lock();
        port.write_int32(&mut self.user(reason), value)
    }

    pub fn read_int64(&self, reason: usize) -> AsynResult<i64> {
        let mut port = self.port.lock();
        port.read_int64(&self.user(reason))
    }

    pub fn write_int64(&self, reason: usize, value: i64) -> AsynResult<()> {
        let mut port = self.port.lock();
        port.write_int64(&mut self.user(reason), value)
    }

    pub fn read_float64(&self, reason: usize) -> AsynResult<f64> {
        let mut port = self.port.lock();
        port.read_float64(&self.user(reason))
    }

    pub fn write_float64(&self, reason: usize, value: f64) -> AsynResult<()> {
        let mut port = self.port.lock();
        port.write_float64(&mut self.user(reason), value)
    }

    pub fn read_octet(&self, reason: usize, buf: &mut [u8]) -> AsynResult<usize> {
        let mut port = self.port.lock();
        port.read_octet(&self.user(reason), buf)
    }

    pub fn write_octet(&self, reason: usize, data: &[u8]) -> AsynResult<()> {
        let mut port = self.port.lock();
        port.write_octet(&mut self.user(reason), data)
    }

    pub fn read_uint32_digital(&self, reason: usize, mask: u32) -> AsynResult<u32> {
        let mut port = self.port.lock();
        port.read_uint32_digital(&self.user(reason), mask)
    }

    pub fn write_uint32_digital(&self, reason: usize, value: u32, mask: u32) -> AsynResult<()> {
        let mut port = self.port.lock();
        port.write_uint32_digital(&mut self.user(reason), value, mask)
    }

    pub fn read_enum(&self, reason: usize) -> AsynResult<(usize, Arc<[EnumEntry]>)> {
        let mut port = self.port.lock();
        port.read_enum(&self.user(reason))
    }

    pub fn write_enum(&self, reason: usize, index: usize) -> AsynResult<()> {
        let mut port = self.port.lock();
        port.write_enum(&mut self.user(reason), index)
    }

    pub fn read_generic_pointer(&self, reason: usize) -> AsynResult<Arc<dyn Any + Send + Sync>> {
        let mut port = self.port.lock();
        port.read_generic_pointer(&self.user(reason))
    }

    pub fn write_generic_pointer(&self, reason: usize, value: Arc<dyn Any + Send + Sync>) -> AsynResult<()> {
        let mut port = self.port.lock();
        port.write_generic_pointer(&mut self.user(reason), value)
    }
}

/// Synchronous I/O handle backed by a [`PortHandle`] (actor model).
///
/// All operations submit requests to the actor and block until completion.
pub struct SyncIOHandle {
    handle: PortHandle,
    addr: i32,
    timeout: Duration,
}

impl SyncIOHandle {
    /// Create from a PortHandle.
    pub fn from_handle(handle: PortHandle, addr: i32, timeout: Duration) -> Self {
        Self { handle, addr, timeout }
    }

    /// Connect to a named port via the PortManager (actor path).
    pub fn connect(manager: &PortManager, port_name: &str, addr: i32, timeout: Duration) -> AsynResult<Self> {
        let handle = manager.find_port_handle(port_name)?;
        Ok(Self { handle, addr, timeout })
    }

    fn user(&self, reason: usize) -> AsynUser {
        AsynUser::new(reason)
            .with_addr(self.addr)
            .with_timeout(self.timeout)
    }

    pub fn read_int32(&self, reason: usize) -> AsynResult<i32> {
        self.handle.read_int32_blocking(reason, self.addr)
    }

    pub fn write_int32(&self, reason: usize, value: i32) -> AsynResult<()> {
        self.handle.write_int32_blocking(reason, self.addr, value)
    }

    pub fn read_float64(&self, reason: usize) -> AsynResult<f64> {
        self.handle.read_float64_blocking(reason, self.addr)
    }

    pub fn write_float64(&self, reason: usize, value: f64) -> AsynResult<()> {
        self.handle.write_float64_blocking(reason, self.addr, value)
    }

    pub fn read_octet(&self, reason: usize, buf_size: usize) -> AsynResult<Vec<u8>> {
        let user = self.user(reason);
        let result = self.handle.submit_blocking(RequestOp::OctetRead { buf_size }, user)?;
        result.data.ok_or_else(|| crate::error::AsynError::Status {
            status: crate::error::AsynStatus::Error,
            message: "octet read returned no data".into(),
        })
    }

    pub fn write_octet(&self, reason: usize, data: &[u8]) -> AsynResult<()> {
        let user = self.user(reason);
        self.handle.submit_blocking(RequestOp::OctetWrite { data: data.to_vec() }, user)?;
        Ok(())
    }

    pub fn read_uint32_digital(&self, reason: usize, mask: u32) -> AsynResult<u32> {
        let user = self.user(reason);
        let result = self.handle.submit_blocking(RequestOp::UInt32DigitalRead { mask }, user)?;
        result.uint_val.ok_or_else(|| crate::error::AsynError::Status {
            status: crate::error::AsynStatus::Error,
            message: "uint32 read returned no value".into(),
        })
    }

    pub fn write_uint32_digital(&self, reason: usize, value: u32, mask: u32) -> AsynResult<()> {
        let user = self.user(reason);
        self.handle.submit_blocking(RequestOp::UInt32DigitalWrite { value, mask }, user)?;
        Ok(())
    }

    pub fn read_int64(&self, reason: usize) -> AsynResult<i64> {
        let user = self.user(reason);
        let result = self.handle.submit_blocking(RequestOp::Int64Read, user)?;
        result.int64_val.ok_or_else(|| crate::error::AsynError::Status {
            status: crate::error::AsynStatus::Error,
            message: "int64 read returned no value".into(),
        })
    }

    pub fn write_int64(&self, reason: usize, value: i64) -> AsynResult<()> {
        let user = self.user(reason);
        self.handle.submit_blocking(RequestOp::Int64Write { value }, user)?;
        Ok(())
    }

    pub fn read_enum(&self, reason: usize) -> AsynResult<usize> {
        let user = self.user(reason);
        let result = self.handle.submit_blocking(RequestOp::EnumRead, user)?;
        result.enum_index.ok_or_else(|| crate::error::AsynError::Status {
            status: crate::error::AsynStatus::Error,
            message: "enum read returned no index".into(),
        })
    }

    pub fn write_enum(&self, reason: usize, index: usize) -> AsynResult<()> {
        let user = self.user(reason);
        self.handle.submit_blocking(RequestOp::EnumWrite { index }, user)?;
        Ok(())
    }

    pub fn drv_user_create(&self, drv_info: &str) -> AsynResult<usize> {
        self.handle.drv_user_create_blocking(drv_info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::param::ParamType;
    use crate::port::{PortDriverBase, PortFlags};

    struct TestPort {
        base: PortDriverBase,
    }

    impl TestPort {
        fn new() -> Self {
            let mut base = PortDriverBase::new("synctest", 1, PortFlags::default());
            base.create_param("INT_VAL", ParamType::Int32).unwrap();
            base.create_param("FLOAT_VAL", ParamType::Float64).unwrap();
            base.create_param("STR_VAL", ParamType::Octet).unwrap();
            base.create_param("BITS", ParamType::UInt32Digital).unwrap();
            base.create_param("MODE", ParamType::Enum).unwrap();
            base.create_param("PTR", ParamType::GenericPointer).unwrap();
            base.create_param("BIG_VAL", ParamType::Int64).unwrap();
            Self { base }
        }
    }

    impl PortDriver for TestPort {
        fn base(&self) -> &PortDriverBase { &self.base }
        fn base_mut(&mut self) -> &mut PortDriverBase { &mut self.base }
    }

    fn make_sync_io() -> SyncIO {
        let port: Arc<Mutex<dyn PortDriver>> = Arc::new(Mutex::new(TestPort::new()));
        SyncIO::from_port(port, 0, Duration::from_secs(1))
    }

    #[test]
    fn test_sync_io_int32_roundtrip() {
        let sio = make_sync_io();
        sio.write_int32(0, 42).unwrap();
        assert_eq!(sio.read_int32(0).unwrap(), 42);
    }

    #[test]
    fn test_sync_io_float64_roundtrip() {
        let sio = make_sync_io();
        sio.write_float64(1, 3.14).unwrap();
        assert!((sio.read_float64(1).unwrap() - 3.14).abs() < 1e-10);
    }

    #[test]
    fn test_sync_io_octet_roundtrip() {
        let sio = make_sync_io();
        sio.write_octet(2, b"hello").unwrap();
        let mut buf = [0u8; 32];
        let n = sio.read_octet(2, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
    }

    #[test]
    fn test_sync_io_uint32_digital_roundtrip() {
        let sio = make_sync_io();
        sio.write_uint32_digital(3, 0xFF, 0x0F).unwrap();
        assert_eq!(sio.read_uint32_digital(3, 0xFF).unwrap(), 0x0F);
    }

    #[test]
    fn test_sync_io_enum_roundtrip() {
        let sio = make_sync_io();
        // Default has sentinel, write index 0
        sio.write_enum(4, 0).unwrap();
        let (idx, _) = sio.read_enum(4).unwrap();
        assert_eq!(idx, 0);
    }

    #[test]
    fn test_sync_io_int64_roundtrip() {
        let sio = make_sync_io();
        sio.write_int64(6, i64::MIN).unwrap();
        assert_eq!(sio.read_int64(6).unwrap(), i64::MIN);
    }

    #[test]
    fn test_sync_io_connect_via_manager() {
        let mgr = PortManager::new();
        mgr.register_port(TestPort::new());
        let sio = SyncIO::connect(&mgr, "synctest", 0, Duration::from_secs(1)).unwrap();
        sio.write_int32(0, 100).unwrap();
        assert_eq!(sio.read_int32(0).unwrap(), 100);
    }
}
