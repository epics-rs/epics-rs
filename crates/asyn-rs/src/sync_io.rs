//! Synchronous convenience API for port driver I/O.
//!
//! [`SyncIOHandle`] wraps a [`PortHandle`] and provides blocking read/write
//! methods for each parameter type.

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::interfaces::InterfaceType;
use crate::manager::PortManager;
use crate::port_handle::PortHandle;
use crate::request::RequestOp;
use crate::services::PortServices;
use crate::trace::TraceMask;
use crate::user::AsynUser;

/// Synchronous I/O handle backed by a [`PortHandle`] (actor model).
///
/// All operations submit requests to the actor and block until completion.
pub struct SyncIOHandle {
    link: Link,
    addr: i32,
    timeout: Duration,
}

/// Where a C SyncIO `connect` left its asynUser.
enum Link {
    Port(PortHandle),
    /// `connectDevice` found the port but it lacks an interface the SyncIO
    /// needs: the user is on the port (its trace), and C leaves the ioPvt
    /// interface NULL, dereferenced by the next transfer. That crash is not
    /// copied: every transfer refuses with the connect's message.
    NoInterface(PortHandle, String),
    /// `connectDevice` failed (`pport == NULL`).
    NoPort,
}

/// C SyncIO `connect`'s `findInterface` checks: `asynCommon`, then the
/// interface the SyncIO transfers on, each refusal in its C file's words.
fn missing_interface(handle: &PortHandle, iface: InterfaceType) -> Option<String> {
    for needed in [InterfaceType::Common, iface] {
        if handle.has_interface(needed) {
            continue;
        }
        let message = match iface {
            // asynOctetSyncIO.c:124-137, asynUInt32DigitalSyncIO.c.
            InterfaceType::Octet | InterfaceType::UInt32Digital => {
                format!("{needed} interface not supported")
            }
            // asynInt32SyncIO.c, asynInt64SyncIO.c, asynEnumSyncIO.c,
            // asynOptionSyncIO.c.
            InterfaceType::Int32
            | InterfaceType::Int64
            | InterfaceType::Enum
            | InterfaceType::Option => format!("port does not implement interface {needed}"),
            // asynCommonSyncIO.c:76-80, asynFloat64SyncIO.c, the array and
            // genericPointer SyncIOs.
            _ => format!("interface {needed} is not supported by port"),
        };
        return Some(message);
    }
    None
}

fn refused(message: String) -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message,
    }
}

impl SyncIOHandle {
    /// Create from a PortHandle.
    pub fn from_handle(handle: PortHandle, addr: i32, timeout: Duration) -> Self {
        Self {
            link: Link::Port(handle),
            addr,
            timeout,
        }
    }

    /// The `asyn<iface>SyncIO->connect` of a named port via the PortManager
    /// (actor path); `iface` is [`InterfaceType::Common`] for
    /// `asynCommonSyncIO`.
    pub fn connect(
        manager: &PortManager,
        port_name: &str,
        addr: i32,
        timeout: Duration,
        iface: InterfaceType,
    ) -> AsynResult<Self> {
        let handle = manager.find_port_handle(port_name)?;
        if let Some(message) = missing_interface(&handle, iface) {
            return Err(refused(message));
        }
        Ok(Self::from_handle(handle, addr, timeout))
    }

    /// C `pasyn<iface>SyncIO->connect` against the global registry; `iface`
    /// is [`InterfaceType::Common`] for `asynCommonSyncIO`. The user exists
    /// even when the connect fails (asynOctetSyncIO.c:117-137), so a driver
    /// that ignores the status keeps a user whose every transfer fails.
    pub fn connect_port(
        port_name: &str,
        addr: i32,
        timeout: Duration,
        iface: InterfaceType,
    ) -> (Self, AsynResult<()>) {
        let (link, status) = match crate::registry::get_port(port_name) {
            Some(entry) => match missing_interface(&entry.handle, iface) {
                None => (Link::Port(entry.handle), Ok(())),
                Some(message) => (
                    Link::NoInterface(entry.handle, message.clone()),
                    Err(refused(message)),
                ),
            },
            None => (
                Link::NoPort,
                Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("asynManager:connectDevice port {port_name} not found"),
                }),
            ),
        };
        (
            Self {
                link,
                addr,
                timeout,
            },
            status,
        )
    }

    /// The port this user transfers through — for a driver that submits its
    /// own requests asynchronously — or the refusal of a user whose connect
    /// failed: C `queueLockPort`'s for one that found no port
    /// (asynManager.c:1843-1847), the connect's own for a missing interface.
    pub fn port(&self) -> AsynResult<&PortHandle> {
        match &self.link {
            Link::Port(handle) => Ok(handle),
            Link::NoInterface(_, message) => Err(refused(message.clone())),
            Link::NoPort => Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "asynManager::queueLockPort not connected".into(),
            }),
        }
    }

    /// The port `connectDevice` found, whether or not it has the interfaces.
    fn device(&self) -> Option<&PortHandle> {
        match &self.link {
            Link::Port(handle) | Link::NoInterface(handle, _) => Some(handle),
            Link::NoPort => None,
        }
    }

    /// C `asynPrint(pasynUser, mask, ...)` on this user: the port's rung for a
    /// connected user, and the global trace for one whose connect found no
    /// port (`findTracePvt`, asynManager.c:548-553). `args` is the line
    /// without its newline.
    pub fn print(&self, reason: i32, mask: TraceMask, args: std::fmt::Arguments<'_>) {
        match self.device() {
            Some(handle) => handle.trace().print(
                Some(handle.port_name()),
                (self.addr >= 0).then_some(self.addr),
                reason,
                mask,
                args,
            ),
            None => PortServices::global()
                .trace()
                .print(None, None, reason, mask, args),
        }
    }

    pub(crate) fn user(&self, reason: usize) -> AsynUser {
        let user = AsynUser::new(reason)
            .with_addr(self.addr)
            .with_timeout(self.timeout);
        match self.device() {
            Some(handle) => user.locking_port(handle),
            None => user,
        }
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
            .port()?
            .submit_blocking(RequestOp::Int32Read, self.user(reason))?;
        result
            .int_val
            .ok_or_else(|| crate::error::AsynError::Status {
                status: crate::error::AsynStatus::Error,
                message: "int32 read returned no value".into(),
            })
    }

    pub fn write_int32(&self, reason: usize, value: i32) -> AsynResult<()> {
        self.port()?
            .submit_blocking(RequestOp::Int32Write { value }, self.user(reason))?;
        Ok(())
    }

    pub fn read_float64(&self, reason: usize) -> AsynResult<f64> {
        let result = self
            .port()?
            .submit_blocking(RequestOp::Float64Read, self.user(reason))?;
        result
            .float_val
            .ok_or_else(|| crate::error::AsynError::Status {
                status: crate::error::AsynStatus::Error,
                message: "float64 read returned no value".into(),
            })
    }

    pub fn write_float64(&self, reason: usize, value: f64) -> AsynResult<()> {
        self.port()?
            .submit_blocking(RequestOp::Float64Write { value }, self.user(reason))?;
        Ok(())
    }

    pub fn read_octet(&self, reason: usize, buf_size: usize) -> crate::request::OctetRead {
        self.octet_read(reason, crate::request::OctetReadOp::Read { buf_size })
    }

    fn octet_read(
        &self,
        reason: usize,
        op: crate::request::OctetReadOp,
    ) -> crate::request::OctetRead {
        match self.port() {
            Ok(port) => port.submit_octet_read_blocking(op, self.user(reason)),
            Err(e) => crate::request::OctetRead::failed(e),
        }
    }

    /// Returns the number of bytes transferred (C `asynOctet::write`'s
    /// `*nbytesTransferred`).
    pub fn write_octet(&self, reason: usize, data: &[u8]) -> AsynResult<usize> {
        let user = self.user(reason);
        let result = self.port()?.submit_blocking(
            RequestOp::OctetWrite {
                data: data.to_vec(),
            },
            user,
        )?;
        Ok(result.nbytes)
    }

    /// Blocking [`PortHandle::write_read`] — C `asynOctetSyncIO.c:231-276`.
    ///
    /// The flush, the write and the read are one actor op, so the port
    /// cannot be served to anyone else between the halves and no stale
    /// reply survives into this exchange. Protocol drivers doing
    /// request/response must call this rather than `write_octet` followed
    /// by `read_octet`.
    pub fn write_read(
        &self,
        reason: usize,
        data: &[u8],
        buf_size: usize,
    ) -> crate::request::OctetRead {
        self.octet_read(
            reason,
            crate::request::OctetReadOp::WriteRead {
                data: data.to_vec(),
                buf_size,
            },
        )
    }

    pub fn read_uint32_digital(&self, reason: usize, mask: u32) -> AsynResult<u32> {
        let user = self.user(reason);
        let result = self
            .port()?
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
        self.port()?
            .submit_blocking(RequestOp::UInt32DigitalWrite { value, mask }, user)?;
        Ok(())
    }

    pub fn read_int64(&self, reason: usize) -> AsynResult<i64> {
        let user = self.user(reason);
        let result = self.port()?.submit_blocking(RequestOp::Int64Read, user)?;
        result
            .int64_val
            .ok_or_else(|| crate::error::AsynError::Status {
                status: crate::error::AsynStatus::Error,
                message: "int64 read returned no value".into(),
            })
    }

    pub fn write_int64(&self, reason: usize, value: i64) -> AsynResult<()> {
        let user = self.user(reason);
        self.port()?
            .submit_blocking(RequestOp::Int64Write { value }, user)?;
        Ok(())
    }

    pub fn read_enum(&self, reason: usize) -> AsynResult<usize> {
        let user = self.user(reason);
        let result = self.port()?.submit_blocking(RequestOp::EnumRead, user)?;
        result
            .enum_index
            .ok_or_else(|| crate::error::AsynError::Status {
                status: crate::error::AsynStatus::Error,
                message: "enum read returned no index".into(),
            })
    }

    pub fn write_enum(&self, reason: usize, index: usize) -> AsynResult<()> {
        let user = self.user(reason);
        self.port()?
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
        // not a failed `create`. A connect that found no port returned before the
        // drvInfo step (:117-137), leaving reason 0 as well.
        let Link::Port(handle) = &self.link else {
            return Ok(0);
        };
        if !handle.has_interface(crate::interfaces::InterfaceType::DrvUser) {
            return Ok(0);
        }
        // Synchronous I/O binds at the port level: no per-record asyn addr (use
        // addr 0, C `getAddr` default) and no record interface behind the bind.
        // Surface only the shared reason.
        handle
            .drv_user_create_blocking(&crate::port::DrvUserRequest::new(drv_info, 0))
            .map(|info| info.reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::param::ParamType;
    use crate::port::InterfaceMask;
    use crate::port::{PortDriver, PortDriverBase, PortFlags};
    use crate::runtime::{RuntimeConfig, create_port_runtime_unclaimed};

    struct TestPort {
        base: PortDriverBase,
    }

    impl TestPort {
        fn new() -> Self {
            Self::named("synctest")
        }

        fn named(name: &str) -> Self {
            let mut base = PortDriverBase::new(
                name,
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            );
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
        let (handle, _jh) =
            create_port_runtime_unclaimed(TestPort::new(), RuntimeConfig::default())
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
        let (data, status) = sio.read_octet(2, 32).into_parts();
        status.unwrap();
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

    /// C `asynOctetSyncIO::writeRead` (`asynOctetSyncIO.c:231-276`) is one
    /// locked exchange that flushes before it writes. A caller that reaches
    /// the port through two separate calls gets neither property, so the
    /// wrapper must be the only thing a request/response protocol needs.
    #[test]
    fn write_read_flushes_then_writes_then_reads_in_one_exchange() {
        use std::sync::{Arc, Mutex};

        struct Sequenced {
            base: PortDriverBase,
            seq: Arc<Mutex<Vec<&'static str>>>,
        }
        impl PortDriver for Sequenced {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
                self.seq.lock().unwrap().push("flush");
                Ok(())
            }
            fn io_write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                self.seq.lock().unwrap().push("write");
                Ok(data.len())
            }
            fn io_read_octet(
                &mut self,
                _user: &AsynUser,
                buf: &mut [u8],
            ) -> crate::interpose::OctetReadResult {
                self.seq.lock().unwrap().push("read");
                let resp = b"RSP";
                buf[..resp.len()].copy_from_slice(resp);
                crate::interpose::OctetReadResult::ok(
                    resp.len(),
                    crate::interpose::EomReason::empty(),
                )
            }
        }

        let seq = Arc::new(Mutex::new(Vec::new()));
        let drv = Sequenced {
            base: PortDriverBase::new(
                "wr_seq",
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            ),
            seq: Arc::clone(&seq),
        };
        let (rt, _jh) = create_port_runtime_unclaimed(drv, RuntimeConfig::default())
            .expect("the port runtime thread must start");
        let sio = SyncIOHandle::from_handle(rt.port_handle().clone(), 0, Duration::from_secs(1));
        let (reply, status) = sio.write_read(0, b"CMD", 16).into_parts();
        status.expect("the exchange must run");
        assert_eq!(reply, b"RSP".to_vec());
        assert_eq!(*seq.lock().unwrap(), vec!["flush", "write", "read"]);
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

        let (rt, _jh) = create_port_runtime_unclaimed(
            Transport(PortDriverBase::new(
                "syncnodrvuser",
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
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
        mgr.register_port_unclaimed(TestPort::new(), &crate::registry::open_owner())
            .unwrap();
        let sio = SyncIOHandle::connect(
            &mgr,
            "synctest",
            0,
            Duration::from_secs(1),
            InterfaceType::Int32,
        )
        .unwrap();
        sio.write_int32(0, 100).unwrap();
        assert_eq!(sio.read_int32(0).unwrap(), 100);
    }

    /// C `pasynOctetSyncIO->connect` to a port that does not exist still hands
    /// back the asynUser (asynOctetSyncIO.c:117-123); every transfer on it then
    /// fails in `queueLockPort` (asynManager.c:1843-1847).
    #[test]
    fn a_user_connected_to_a_missing_port_refuses_every_transfer() {
        let (sio, status) = SyncIOHandle::connect_port(
            "sync_no_such_port",
            0,
            Duration::from_secs(1),
            InterfaceType::Octet,
        );
        let msg = |e: AsynError| match e {
            AsynError::Status {
                status: AsynStatus::Error,
                message,
            } => message,
            other => panic!("not asynError: {other:?}"),
        };
        assert_eq!(
            msg(status.unwrap_err()),
            "asynManager:connectDevice port sync_no_such_port not found"
        );
        let refused = "asynManager::queueLockPort not connected";
        assert_eq!(msg(sio.read_int32(0).unwrap_err()), refused);
        assert_eq!(msg(sio.write_int32(0, 1).unwrap_err()), refused);
        assert_eq!(msg(sio.read_float64(0).unwrap_err()), refused);
        assert_eq!(msg(sio.write_float64(0, 1.0).unwrap_err()), refused);
        assert_eq!(
            msg(sio.read_octet(0, 8).into_parts().1.unwrap_err()),
            refused
        );
        assert_eq!(msg(sio.write_octet(0, b"x").unwrap_err()), refused);
        assert_eq!(
            msg(sio.write_read(0, b"x", 8).into_parts().1.unwrap_err()),
            refused
        );
        assert_eq!(msg(sio.read_uint32_digital(0, 1).unwrap_err()), refused);
        assert_eq!(msg(sio.write_uint32_digital(0, 1, 1).unwrap_err()), refused);
        assert_eq!(msg(sio.read_int64(0).unwrap_err()), refused);
        assert_eq!(msg(sio.write_int64(0, 1).unwrap_err()), refused);
        assert_eq!(msg(sio.read_enum(0).unwrap_err()), refused);
        assert_eq!(msg(sio.write_enum(0, 0).unwrap_err()), refused);
        assert_eq!(sio.drv_user_create("INT_VAL").unwrap(), 0);
        assert_eq!(msg(sio.port().err().unwrap()), refused);

        // Control: the same call on a published port connects and transfers.
        let mgr = PortManager::new();
        mgr.register_port_unclaimed(
            TestPort::named("sync_connect_port_live"),
            &crate::registry::open_owner(),
        )
        .unwrap();
        let (live, status) = SyncIOHandle::connect_port(
            "sync_connect_port_live",
            0,
            Duration::from_secs(1),
            InterfaceType::Octet,
        );
        status.unwrap();
        live.write_int32(0, 7).unwrap();
        assert_eq!(live.read_int32(0).unwrap(), 7);
    }

    /// A published port declaring exactly `caps`.
    fn port_with(name: &str, caps: Vec<crate::interfaces::Capability>) {
        struct Declared(PortDriverBase, Vec<crate::interfaces::Capability>);
        impl PortDriver for Declared {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
                self.1.clone()
            }
        }
        let mut base = PortDriverBase::new(
            name,
            1,
            PortFlags::default(),
            InterfaceMask::UNMIGRATED_ALL,
            InterfaceMask::UNMIGRATED_INTERRUPTS,
        );
        base.create_param("V", ParamType::Int32).unwrap();
        PortManager::new()
            .register_port_unclaimed(Declared(base, caps), &crate::registry::open_owner())
            .unwrap();
    }

    fn refusal(e: AsynError) -> String {
        match e {
            AsynError::Status {
                status: AsynStatus::Error,
                message,
            } => message,
            other => panic!("not asynError: {other:?}"),
        }
    }

    /// asynOctetSyncIO.c:124-130: a port without asynCommon refuses the
    /// connect, and the user it hands back transfers nothing.
    #[test]
    fn an_octet_connect_to_a_port_without_asyn_common_is_refused() {
        use crate::interfaces::Capability::*;
        port_with("sync_no_common", vec![OctetRead, OctetWrite]);
        let (sio, status) = SyncIOHandle::connect_port(
            "sync_no_common",
            0,
            Duration::from_secs(1),
            InterfaceType::Octet,
        );
        let refused = "asynCommon interface not supported";
        assert_eq!(refusal(status.unwrap_err()), refused);
        assert_eq!(refusal(sio.write_octet(0, b"x").unwrap_err()), refused);
        assert_eq!(refusal(sio.port().err().unwrap()), refused);
    }

    /// asynOctetSyncIO.c:131-137: asynCommon present, asynOctet missing.
    #[test]
    fn an_octet_connect_to_a_port_without_asyn_octet_is_refused() {
        use crate::interfaces::Capability::*;
        port_with("sync_no_octet", vec![Int32Read, Int32Write, Connect]);
        let (sio, status) = SyncIOHandle::connect_port(
            "sync_no_octet",
            0,
            Duration::from_secs(1),
            InterfaceType::Octet,
        );
        let refused = "asynOctet interface not supported";
        assert_eq!(refusal(status.unwrap_err()), refused);
        assert_eq!(
            refusal(sio.read_octet(0, 8).into_parts().1.unwrap_err()),
            refused
        );
    }

    /// asynCommonSyncIO.c:76-80 checks asynCommon alone, in its own words.
    #[test]
    fn a_common_connect_needs_only_asyn_common() {
        use crate::interfaces::Capability::*;
        port_with("sync_common_absent", vec![OctetRead, OctetWrite]);
        let (_, status) = SyncIOHandle::connect_port(
            "sync_common_absent",
            0,
            Duration::from_secs(1),
            InterfaceType::Common,
        );
        assert_eq!(
            refusal(status.unwrap_err()),
            "interface asynCommon is not supported by port"
        );
        port_with("sync_common_only", vec![Connect]);
        let (_, status) = SyncIOHandle::connect_port(
            "sync_common_only",
            0,
            Duration::from_secs(1),
            InterfaceType::Common,
        );
        status.unwrap();
    }

    /// Each SyncIO file's refusal text for its own missing interface:
    /// asynInt32SyncIO.c, asynFloat64SyncIO.c, asynUInt32DigitalSyncIO.c.
    #[test]
    fn each_syncio_refuses_its_missing_interface_in_its_own_words() {
        port_with(
            "sync_transport_only",
            crate::interfaces::octet_transport_capabilities(),
        );
        for (iface, refused) in [
            (
                InterfaceType::Int32,
                "port does not implement interface asynInt32",
            ),
            (
                InterfaceType::Float64,
                "interface asynFloat64 is not supported by port",
            ),
            (
                InterfaceType::UInt32Digital,
                "asynUInt32Digital interface not supported",
            ),
        ] {
            let (_, status) =
                SyncIOHandle::connect_port("sync_transport_only", 0, Duration::from_secs(1), iface);
            assert_eq!(refusal(status.unwrap_err()), refused, "{iface}");
        }
        let (_, status) = SyncIOHandle::connect_port(
            "sync_transport_only",
            0,
            Duration::from_secs(1),
            InterfaceType::Octet,
        );
        status.unwrap();
    }

    /// The PortManager connect refuses the same missing interface.
    #[test]
    fn a_manager_connect_refuses_a_missing_interface() {
        let mgr = PortManager::new();
        mgr.register_port_unclaimed(TestPort::new(), &crate::registry::open_owner())
            .unwrap();
        let err = SyncIOHandle::connect(
            &mgr,
            "synctest",
            0,
            Duration::from_secs(1),
            InterfaceType::Int32Array,
        )
        .err()
        .unwrap();
        assert_eq!(
            refusal(err),
            "interface asynInt32Array is not supported by port"
        );
    }

    /// C `asynPrint` on a user whose connect found no port prints on the
    /// global trace (asynManager.c:548-553).
    #[test]
    fn a_user_of_a_missing_port_prints_on_the_global_trace() {
        use crate::trace::TraceFile;

        let path = std::env::temp_dir().join(format!("syncio-global-{}", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        let trace = PortServices::global().trace().clone();
        trace.set_trace_file(None, TraceFile::File(Arc::new(std::sync::Mutex::new(file))));

        let (sio, _) = SyncIOHandle::connect_port(
            "sync_print_no_port",
            0,
            Duration::ZERO,
            InterfaceType::Octet,
        );
        sio.print(0, TraceMask::ERROR, format_args!("unconnected line\n"));
        trace.set_trace_file(None, TraceFile::default());

        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(text.ends_with("unconnected line\n"), "{text:?}");
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

    /// R7-46: C `asynOctetSyncIO::read` hands the caller `*nbytesTransferred`
    /// alongside the failing status — `pasynOctet->read` fills the caller's
    /// buffer before returning `asynTimeout`, so a partial line is not lost to
    /// a SyncIO caller, so `read_octet` hands back the bytes beside the
    /// status.
    #[test]
    fn read_octet_error_carries_the_partial_transfer() {
        use crate::error::{AsynError, AsynResult, AsynStatus};
        use crate::interfaces::InterfaceType;
        use crate::interpose::{EomReason, OctetReadResult};
        use crate::user::AsynUser;

        struct PartialThenTimeout(PortDriverBase);
        impl PortDriver for PartialThenTimeout {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
            fn io_read_octet(&mut self, _user: &AsynUser, buf: &mut [u8]) -> OctetReadResult {
                buf[..3].copy_from_slice(b"abc");
                OctetReadResult {
                    status: Err(AsynError::Status {
                        status: AsynStatus::Timeout,
                        message: "read timeout".into(),
                    }),
                    ..OctetReadResult::ok(3, EomReason::empty())
                }
            }
        }

        let (handle, _jh) = create_port_runtime_unclaimed(
            PartialThenTimeout(PortDriverBase::new(
                "syncpartial",
                1,
                PortFlags::default(),
                InterfaceMask::UNMIGRATED_ALL,
                InterfaceMask::UNMIGRATED_INTERRUPTS,
            )),
            RuntimeConfig::default(),
        )
        .expect("the port runtime thread must start");
        let sio =
            SyncIOHandle::from_handle(handle.port_handle().clone(), 0, Duration::from_secs(1));

        let (data, status) = sio.read_octet(0, 32).into_parts();
        assert_eq!(
            status.unwrap_err().status(),
            AsynStatus::Timeout,
            "the timeout still surfaces"
        );
        assert_eq!(data, b"abc", "the transfer must reach the caller");
    }
}
