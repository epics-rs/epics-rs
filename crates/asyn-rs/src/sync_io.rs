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

    /// Read the configured per-call timeout. The handle is `&self`-only
    /// for I/O methods, so a `read_*` / `write_*` cannot mutate this
    /// value — accessor exists primarily for asyn upstream issue #220
    /// regression coverage (`pasynOctetSyncIO->read` mutated the
    /// caller's `pasynUser->timeout`; we cannot, by construction).
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn read_int32(&self, reason: usize) -> AsynResult<i32> {
        // Build the user via `self.user` so the configured timeout is
        // honored — the `*_blocking` shortcuts hard-code the 1s default.
        let result = self
            .handle
            .submit_blocking(RequestOp::Int32Read, self.user(reason))?;
        result
            .int_val
            .ok_or_else(|| crate::error::AsynError::Status {
                status: crate::error::AsynStatus::Error,
                message: "int32 read returned no value".into(),
            })
    }

    pub fn write_int32(&self, reason: usize, value: i32) -> AsynResult<()> {
        self.handle
            .submit_blocking(RequestOp::Int32Write { value }, self.user(reason))?;
        Ok(())
    }

    pub fn read_float64(&self, reason: usize) -> AsynResult<f64> {
        let result = self
            .handle
            .submit_blocking(RequestOp::Float64Read, self.user(reason))?;
        result
            .float_val
            .ok_or_else(|| crate::error::AsynError::Status {
                status: crate::error::AsynStatus::Error,
                message: "float64 read returned no value".into(),
            })
    }

    pub fn write_float64(&self, reason: usize, value: f64) -> AsynResult<()> {
        self.handle
            .submit_blocking(RequestOp::Float64Write { value }, self.user(reason))?;
        Ok(())
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

    /// Returns the number of bytes transferred (C `asynOctet::write`'s
    /// `*nbytesTransfered`).
    pub fn write_octet(&self, reason: usize, data: &[u8]) -> AsynResult<usize> {
        let user = self.user(reason);
        let result = self.handle.submit_blocking(
            RequestOp::OctetWrite {
                data: data.to_vec(),
            },
            user,
        )?;
        Ok(result.nbytes)
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
        // C `asynOctetSyncIO::connect` resolves a drvInfo only on a port that
        // registered asynDrvUser (`findInterface(asynDrvUserType)`,
        // asynOctetSyncIO.c:143-156); on a port without it the pasynUser keeps
        // reason 0 and the connect succeeds. The interface registry answers that,
        // not a failed `create`.
        if !self
            .handle
            .has_interface(crate::interfaces::InterfaceType::DrvUser)
        {
            return Ok(0);
        }
        // Synchronous I/O binds at the port level: no per-record asyn addr (use
        // addr 0, C `getAddr` default) and no record interface behind the bind.
        // Surface only the shared reason.
        self.handle
            .drv_user_create_blocking(&crate::port::DrvUserRequest::new(drv_info, 0))
            .map(|info| info.reason)
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
        let (handle, _jh) = create_port_runtime(TestPort::new(), RuntimeConfig::default())
            .expect("the port runtime thread must start");
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

    /// R11-48: C `asynOctetSyncIO::connect` resolves a drvInfo only on a port
    /// that registered asynDrvUser (asynOctetSyncIO.c:141-156); on a byte
    /// transport the connect succeeds with the pasynUser still at reason 0. The
    /// port reported the transport's `ParamNotFound` instead.
    #[test]
    fn drv_user_create_on_a_port_without_asyn_drv_user_yields_reason_zero() {
        struct Transport(PortDriverBase);
        impl PortDriver for Transport {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
                crate::interfaces::octet_transport_capabilities()
            }
        }

        let (rt, _jh) = create_port_runtime(
            Transport(PortDriverBase::new(
                "syncnodrvuser",
                1,
                PortFlags::default(),
            )),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        let sio = SyncIOHandle::from_handle(rt.port_handle().clone(), 0, Duration::from_secs(1));

        assert_eq!(sio.drv_user_create("ANYTHING").unwrap(), 0);

        // Negative control: a port that registers asynDrvUser still resolves the
        // name — and still rejects one it does not know.
        let (sio2, _rt2) = make_sync_io();
        assert_eq!(sio2.drv_user_create("FLOAT_VAL").unwrap(), 1);
        assert!(sio2.drv_user_create("NO_SUCH_PARAM").is_err());
    }

    #[test]
    fn test_sync_io_via_manager() {
        let mgr = PortManager::new();
        mgr.register_port(TestPort::new()).unwrap();
        let sio = SyncIOHandle::connect(&mgr, "synctest", 0, Duration::from_secs(1)).unwrap();
        sio.write_int32(0, 100).unwrap();
        assert_eq!(sio.read_int32(0).unwrap(), 100);
    }

    /// Reproducer for asyn upstream issue #220: `pasynOctetSyncIO->read`
    /// mutated the caller's `pasynUser->timeout`. In our actor model,
    /// `SyncIOHandle::read_*` / `write_*` take `&self` and the timeout
    /// is a `Copy` struct field — the type system forbids mutation.
    /// This test pins that contract behaviourally so a future refactor
    /// that takes `&mut self` would still have to leave the timeout
    /// untouched (or break this assertion and own the consequence).
    #[test]
    fn read_write_does_not_mutate_timeout_field() {
        let (sio, _rt) = make_sync_io();
        let before = sio.timeout();
        sio.write_int32(0, 5).unwrap();
        assert_eq!(sio.timeout(), before, "write must preserve timeout");
        let _ = sio.read_int32(0).unwrap();
        assert_eq!(sio.timeout(), before, "read must preserve timeout");
        // Several rounds of mixed I/O.
        for i in 0..10 {
            sio.write_int32(0, i).unwrap();
            let _ = sio.read_int32(0).unwrap();
        }
        assert_eq!(sio.timeout(), before, "10× r/w must preserve timeout");
    }

    /// R7-46: C `asynOctetSyncIO::read` hands the caller `*nbytesTransfered`
    /// alongside the failing status — `pasynOctet->read` fills the caller's
    /// buffer before returning `asynTimeout`, so a partial line is not lost to
    /// a SyncIO caller. `read_octet`'s `Err` must therefore carry the transfer:
    /// the bytes ride inside `AsynError::PartialRead`, not in a buffer that `?`
    /// drops on the way out of the actor.
    #[test]
    fn read_octet_error_carries_the_partial_transfer() {
        use crate::error::{AsynError, AsynResult, AsynStatus};
        use crate::interpose::{EomReason, PartialOctetRead};
        use crate::user::AsynUser;

        struct PartialThenTimeout(PortDriverBase);
        impl PortDriver for PartialThenTimeout {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn io_read_octet_eom(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> AsynResult<(usize, EomReason)> {
                buf[..3].copy_from_slice(b"abc");
                Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "read timeout".into(),
                }
                .with_partial_read(PartialOctetRead {
                    data: b"abc".to_vec(),
                    eom_reason: EomReason::empty(),
                }))
            }
        }

        let (handle, _jh) = create_port_runtime(
            PartialThenTimeout(PortDriverBase::new("syncpartial", 1, PortFlags::default())),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        let sio =
            SyncIOHandle::from_handle(handle.port_handle().clone(), 0, Duration::from_secs(1));

        let err = sio.read_octet(0, 32).unwrap_err();
        assert_eq!(
            err.status(),
            AsynStatus::Timeout,
            "the timeout still surfaces"
        );
        let partial = err
            .partial_read()
            .expect("the transfer must survive the actor dispatch and reach the caller");
        assert_eq!(partial.data, b"abc");
        assert_eq!(partial.nbytes_transferred(), 3);
    }
}
