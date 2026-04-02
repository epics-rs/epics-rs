//! Synchronous convenience API for port driver I/O.
//!
//! [`SyncIOHandle`] wraps a [`PortHandle`] and provides blocking read/write
//! methods for each parameter type.

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use crate::error::AsynResult;
use crate::manager::PortManager;
use crate::port_handle::PortHandle;
use crate::request::RequestOp;
use crate::user::AsynUser;

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
        Self {
            handle,
            addr,
            timeout,
        }
    }

    /// Connect to a named port via the PortManager (actor path).
    pub fn connect(
        manager: &PortManager,
        port_name: &str,
        addr: i32,
        timeout: Duration,
    ) -> AsynResult<Self> {
        let handle = manager.find_port_handle(port_name)?;
        Ok(Self {
            handle,
            addr,
            timeout,
        })
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
        let result = self
            .handle
            .submit_blocking(RequestOp::OctetRead { buf_size }, user)?;
        result.data.ok_or_else(|| crate::error::AsynError::Status {
            status: crate::error::AsynStatus::Error,
            message: "octet read returned no data".into(),
        })
    }

    pub fn write_octet(&self, reason: usize, data: &[u8]) -> AsynResult<()> {
        let user = self.user(reason);
        self.handle.submit_blocking(
            RequestOp::OctetWrite {
                data: data.to_vec(),
            },
            user,
        )?;
        Ok(())
    }

    pub fn read_uint32_digital(&self, reason: usize, mask: u32) -> AsynResult<u32> {
        let user = self.user(reason);
        let result = self
            .handle
            .submit_blocking(RequestOp::UInt32DigitalRead { mask }, user)?;
        result
            .uint_val
            .ok_or_else(|| crate::error::AsynError::Status {
                status: crate::error::AsynStatus::Error,
                message: "uint32 read returned no value".into(),
            })
    }

    pub fn write_uint32_digital(&self, reason: usize, value: u32, mask: u32) -> AsynResult<()> {
        let user = self.user(reason);
        self.handle
            .submit_blocking(RequestOp::UInt32DigitalWrite { value, mask }, user)?;
        Ok(())
    }

    pub fn read_int64(&self, reason: usize) -> AsynResult<i64> {
        let user = self.user(reason);
        let result = self.handle.submit_blocking(RequestOp::Int64Read, user)?;
        result
            .int64_val
            .ok_or_else(|| crate::error::AsynError::Status {
                status: crate::error::AsynStatus::Error,
                message: "int64 read returned no value".into(),
            })
    }

    pub fn write_int64(&self, reason: usize, value: i64) -> AsynResult<()> {
        let user = self.user(reason);
        self.handle
            .submit_blocking(RequestOp::Int64Write { value }, user)?;
        Ok(())
    }

    pub fn read_enum(&self, reason: usize) -> AsynResult<usize> {
        let user = self.user(reason);
        let result = self.handle.submit_blocking(RequestOp::EnumRead, user)?;
        result
            .enum_index
            .ok_or_else(|| crate::error::AsynError::Status {
                status: crate::error::AsynStatus::Error,
                message: "enum read returned no index".into(),
            })
    }

    pub fn write_enum(&self, reason: usize, index: usize) -> AsynResult<()> {
        let user = self.user(reason);
        self.handle
            .submit_blocking(RequestOp::EnumWrite { index }, user)?;
        Ok(())
    }

    pub fn read_generic_pointer(&self, _reason: usize) -> AsynResult<Arc<dyn Any + Send + Sync>> {
        Err(crate::error::AsynError::Status {
            status: crate::error::AsynStatus::Error,
            message: "generic pointer read not supported via actor".into(),
        })
    }

    pub fn drv_user_create(&self, drv_info: &str) -> AsynResult<usize> {
        self.handle.drv_user_create_blocking(drv_info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::param::ParamType;
    use crate::port::{PortDriver, PortDriverBase, PortFlags};
    use crate::runtime::{RuntimeConfig, create_port_runtime};

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
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
    }

    use crate::runtime::PortRuntimeHandle;

    fn make_sync_io() -> (SyncIOHandle, PortRuntimeHandle) {
        let (handle, _jh) = create_port_runtime(TestPort::new(), RuntimeConfig::default());
        let sio =
            SyncIOHandle::from_handle(handle.port_handle().clone(), 0, Duration::from_secs(1));
        (sio, handle)
    }

    #[test]
    fn test_sync_io_int32_roundtrip() {
        let (sio, _rt) = make_sync_io();
        sio.write_int32(0, 42).unwrap();
        assert_eq!(sio.read_int32(0).unwrap(), 42);
    }

    #[test]
    fn test_sync_io_float64_roundtrip() {
        let (sio, _rt) = make_sync_io();
        sio.write_float64(1, 3.14).unwrap();
        assert!((sio.read_float64(1).unwrap() - 3.14).abs() < 1e-10);
    }

    #[test]
    fn test_sync_io_octet_roundtrip() {
        let (sio, _rt) = make_sync_io();
        sio.write_octet(2, b"hello").unwrap();
        let data = sio.read_octet(2, 32).unwrap();
        assert_eq!(&data[..5], b"hello");
    }

    #[test]
    fn test_sync_io_uint32_digital_roundtrip() {
        let (sio, _rt) = make_sync_io();
        sio.write_uint32_digital(3, 0xFF, 0x0F).unwrap();
        assert_eq!(sio.read_uint32_digital(3, 0xFF).unwrap(), 0x0F);
    }

    #[test]
    fn test_sync_io_int64_roundtrip() {
        let (sio, _rt) = make_sync_io();
        sio.write_int64(6, i64::MIN).unwrap();
        assert_eq!(sio.read_int64(6).unwrap(), i64::MIN);
    }

    #[test]
    fn test_sync_io_via_manager() {
        let mgr = PortManager::new();
        mgr.register_port(TestPort::new());
        let sio = SyncIOHandle::connect(&mgr, "synctest", 0, Duration::from_secs(1)).unwrap();
        sio.write_int32(0, 100).unwrap();
        assert_eq!(sio.read_int32(0).unwrap(), 100);
    }
}
